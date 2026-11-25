//! The Questions view (`6`, or `a` from anywhere): ask your own claude a quick question and keep
//! every thread, like a chat page, but calm and keyboard-first like the rest of Wake.
//!
//! Left, the threads (newest asked first; a dot while answering, waiting or unread). Right, the
//! selected thread at a readable width, answers drawn from Markdown, and the input under it:
//! Enter asks, Shift+Enter starts a new line. Follow-ups continue the selected thread; `n` starts a
//! new one. Next to the input, the context: "Just a question" (Claude may search the web) or a
//! project (it may also read that project's files). Nothing ever changes files.
//!
//! Wake's core runs the questions (`Source::ask` and friends, wake-core's questions.rs); this
//! module only asks for the state, every 200 ms while something is being answered, and draws it.
//! Its state lives here, beside the app's, so the rest of the window doesn't need to know about it.

use std::{cell::RefCell, rc::Rc, time::Duration};

use gtk4::{self as gtk, gdk, glib, prelude::*};
use wake_core::*;

use crate::{
    app::{self, Ui, View},
    dialogs, fmt, views, w,
};

const POLL: Duration = Duration::from_millis(200);
/// The thread's column: about 72 characters of text.
const COLUMN: i32 = 700;

struct W {
    filter: gtk::Entry,
    list: gtk::ListBox,
    thread: gtk::Box,
    input: gtk::TextView,
    ctx: gtk::DropDown,
    ctx_note: gtk::Label,
    /// The thread list's column, the thread's, and the way back to the list in a narrow window.
    side: gtk::Widget,
    main: gtk::Box,
    back: gtk::Button,
}

#[derive(Default)]
struct St {
    ov: Option<QOverview>,
    /// The selected thread; None while writing a new one.
    sel: Option<String>,
    filter: String,
    busy: bool,
    polling: bool,
    /// The context dropdown's entries (None = just a question), and whether the person picked one.
    ctx_ids: Vec<Option<String>>,
    ctx_names: Vec<String>,
    ctx_manual: bool,
    list_sig: String,
    thread_sig: String,
    /// The answer being written: its box and the text it shows, redrawn alone as text arrives.
    streaming: Option<(gtk::Box, String)>,
    /// Keep the thread scrolled to the end while the reader is there.
    stick: bool,
    /// The thread drawn last.
    shown: Option<String>,
    /// A narrow window shows one column at a time, as a phone does: the list, or the thread.
    narrow: bool,
    list_shown: bool,
}

thread_local! {
    static WS: RefCell<Option<Rc<W>>> = const { RefCell::new(None) };
    static ST: RefCell<St> = RefCell::new(St::default());
}

fn ws() -> Option<Rc<W>> {
    WS.with(|w| w.borrow().clone())
}

/// The state, briefly: never call GTK (whose signals may come back here) inside `f`.
fn st<R>(f: impl FnOnce(&mut St) -> R) -> R {
    ST.with(|s| f(&mut s.borrow_mut()))
}

fn thread_of(ov: &QOverview, id: &str) -> Option<QThread> {
    ov.threads.iter().find(|t| t.id == id).cloned()
}

// ---------- building ----------

/// Puts the view's widgets into its page of the stack. Called once from `app::build`.
pub fn build(ui: &Ui) {
    let vw = &ui.w.views[&View::Questions];
    vw.head.set_visible(false);
    vw.scroll.set_visible(false);
    let pane = w::hbox(0, &["qpane"]);
    pane.set_vexpand(true);

    // Left: the threads.
    let side = w::vbox(0, &["qside"]);
    let top = w::hbox(8, &["qfilter"]);
    let filter = gtk::Entry::builder().placeholder_text("Filter questions").hexpand(true).build();
    filter.add_css_class("search");
    top.append(&filter);

    side.append(&top);
    let ui2 = ui.clone();
    let new = w::button("New question", None, &["ghost", "sm", "qnew"], move || new_thread(&ui2, true));
    new.set_halign(gtk::Align::Start);
    side.append(&new);
    let list = gtk::ListBox::new();
    list.add_css_class("qlist");
    list.set_selection_mode(gtk::SelectionMode::Single);
    list.set_activate_on_single_click(true);
    let list_scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&list).build();
    side.append(&list_scroll);
    // 300 px when there is room, narrowing to 180 in a narrow window.
    let side_fit = w::Fit::new(&side, 180, Some(300)).upcast();
    side_fit.set_hexpand(false);
    pane.append(&side_fit);

    // Right: the thread and the input.
    let main = w::vbox(0, &["qmain"]);
    main.set_hexpand(true);
    let back = gtk::Button::with_label("‹ Questions");
    back.add_css_class("back");
    back.set_halign(gtk::Align::Start);
    back.set_tooltip_text(Some("All your questions"));
    back.set_visible(false);
    let backbar = w::hbox(0, &["backbar"]);
    backbar.append(&back);
    backbar.set_visible(false);
    main.append(&backbar);
    let thread = w::vbox(22, &["qthread"]);
    let col = w::Fit::new(&thread, 280, Some(COLUMN)).upcast();
    col.set_halign(gtk::Align::Center);
    let holder = w::vbox(0, &[]);
    holder.append(&col);
    let thread_scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&holder).build();
    // Not a Tab stop of its own: the thread is for reading (its buttons still are stops).
    thread_scroll.set_focusable(false);
    main.append(&thread_scroll);

    // A chat composer: the text box the full width, and one line under it with what the question
    // is about (and what that lets Claude do) on the left and the keys on the right.
    let bar = w::vbox(8, &["qbar"]);
    let ctx = gtk::DropDown::from_strings(&["Just a question"]);
    ctx.add_css_class("mini");
    ctx.set_valign(gtk::Align::Center);
    let input = gtk::TextView::new();
    input.add_css_class("area");
    input.add_css_class("qinput");
    input.set_wrap_mode(gtk::WrapMode::WordChar);
    input.set_accepts_tab(false);
    input.update_property(&[gtk::accessible::Property::Label("Your question")]);
    let frame = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).propagate_natural_height(true).max_content_height(160).hexpand(true).child(&input).build();
    frame.add_css_class("areaframe");
    bar.append(&frame);
    let foot = w::hbox(6, &["qfoot"]);
    foot.append(&ctx);
    let note = w::label("", &["hint"]);
    note.set_hexpand(true);
    note.set_xalign(0.0);
    note.set_margin_start(4);
    note.set_valign(gtk::Align::Center);
    note.set_ellipsize(gtk::pango::EllipsizeMode::End);
    foot.append(&note);
    for (k, word) in [("Enter", "asks"), ("Shift+Enter", "new line")] {
        for part in k.split('+') {
            let kb = w::kbd(part);
            kb.set_valign(gtk::Align::Center);
            foot.append(&kb);
        }
        let l = w::label(word, &["hint"]);
        l.set_valign(gtk::Align::Center);
        foot.append(&l);
    }
    bar.append(&foot);
    let bar_fit = w::Fit::new(&bar, 280, Some(COLUMN)).upcast();
    bar_fit.set_halign(gtk::Align::Center);
    let bar_holder = w::vbox(0, &["qbarwrap"]);
    bar_holder.append(&bar_fit);
    main.append(&bar_holder);
    pane.append(&main);
    vw.root.append(&pane);

    let ws_ = Rc::new(W { filter: filter.clone(), list: list.clone(), thread, input: input.clone(), ctx: ctx.clone(), ctx_note: note, side: side_fit, main: main.clone(), back: back.clone() });
    WS.with(|w| *w.borrow_mut() = Some(ws_));

    // Signals.
    {
        let ui = ui.clone();
        list.connect_row_selected(move |_, row| {
            let Some(row) = row else { return };
            let id = row.widget_name().to_string();
            if id.is_empty() || st(|s| s.sel.as_deref() == Some(id.as_str())) {
                return;
            }
            select(&ui, Some(id));
        });
    }
    {
        let ui = ui.clone();
        list.connect_row_activated(move |_, _| {
            show_list(false);
            focus_input(&ui);
        });
    }
    {
        let ui = ui.clone();
        back.connect_clicked(move |_| {
            show_list(true);
            if let Some(wd) = ws() {
                wd.list.selected_row().map_or_else(|| wd.filter.grab_focus(), |r| r.grab_focus());
            }
            let _ = &ui;
        });
    }
    {
        let ui = ui.clone();
        filter.connect_changed(move |e| {
            let t = e.text().to_lowercase();
            if st(|s| std::mem::replace(&mut s.filter, t.clone()) != t) {
                render_list(&ui);
            }
        });
    }
    {
        let ui = ui.clone();
        ctx.connect_selected_notify(move |_| {
            if let Ok(true) = ST.with(|s| {
                s.try_borrow_mut().map(|mut s| {
                    s.ctx_manual = true;
                    true
                })
            }) {
                ctx_note(&ui);
            }
        });
    }
    {
        // Follow the end of the answer while the reader is at the end.
        let adj = thread_scroll.vadjustment();
        adj.connect_value_changed(|a| {
            let at_end = a.value() >= a.upper() - a.page_size() - 48.0;
            let _ = ST.with(|s| s.try_borrow_mut().map(|mut s| s.stick = at_end));
        });
        adj.connect_changed(|a| {
            if ST.with(|s| s.try_borrow().map(|s| s.stick).unwrap_or(false)) {
                a.set_value(a.upper() - a.page_size());
            }
        });
    }
    st(|s| s.stick = true);
    fetch(ui);
}

// ---------- data ----------

/// Asks the source what changed; draws it; keeps polling while anything is being answered.
pub fn fetch(ui: &Ui) {
    let since = st(|s| {
        if s.busy {
            return None;
        }
        s.busy = true;
        Some(s.ov.as_ref().map_or(0, |o| o.seq))
    });
    let Some(since) = since else { return };
    app::bg(
        ui,
        move |src| src.questions(since),
        |ui, r| {
            st(|s| s.busy = false);
            if let Some(ov) = r {
                st(|s| s.ov = Some(ov));
                changed(ui);
            }
            let active = st(|s| s.ov.as_ref().is_some_and(|o| o.answering.is_some() || !o.waiting.is_empty()));
            if active {
                poll(ui);
            }
        },
    );
}

fn poll(ui: &Ui) {
    if st(|s| std::mem::replace(&mut s.polling, true)) {
        return;
    }
    let ui = ui.clone();
    glib::timeout_add_local(POLL, move || {
        fetch(&ui);
        let active = st(|s| s.busy || s.ov.as_ref().is_some_and(|o| o.answering.is_some() || !o.waiting.is_empty()));
        if !active {
            st(|s| s.polling = false);
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    });
}

/// Draws the view again from scratch (the theme or the AI changed).
pub fn redraw(ui: &Ui) {
    st(|s| {
        s.list_sig.clear();
        s.thread_sig.clear();
    });
    if ui.st.borrow().view == View::Questions {
        render(ui);
    }
}

fn changed(ui: &Ui) {
    views::render_rail(ui);
    if ui.st.borrow().view == View::Questions {
        render(ui);
        mark_seen(ui);
    }
}

/// The selected thread's answer has been seen once it is on screen.
fn mark_seen(ui: &Ui) {
    let id = st(|s| s.sel.clone().filter(|id| s.ov.as_ref().is_some_and(|o| o.unread.contains(id))));
    if let Some(id) = id
        && ui.window.is_active()
        && ui.st.borrow().view == View::Questions
    {
        app::bg(ui, move |src| src.mark_read(&id), |ui, ()| fetch(ui));
    }
}

/// For the rail: (unread answers, answering now), or None when there is nothing to show.
pub fn rail_badge() -> Option<(u32, bool)> {
    st(|s| {
        let o = s.ov.as_ref()?;
        let answering = o.answering.is_some() || !o.waiting.is_empty();
        (answering || !o.unread.is_empty()).then_some((o.unread.len() as u32, answering))
    })
}

// ---------- drawing ----------

/// Draws the view from the last state (cheap when nothing changed). The app calls this on every
/// refresh while the view is showing.
pub fn render(ui: &Ui) {
    if ws().is_none() {
        return;
    }
    sync_ctx(ui);
    render_list(ui);
    render_thread(ui);
}

fn matches(t: &QThread, q: &str) -> bool {
    q.is_empty() || t.title.to_lowercase().contains(q) || t.turns.iter().any(|x| x.question.to_lowercase().contains(q) || x.answer.to_lowercase().contains(q))
}

fn project_name(ui: &Ui, id: &str) -> String {
    app::find_project(ui, id).map_or_else(|| id.to_string(), |p| p.name)
}

/// (id, title, project name, when, status dot)
type ListRow = (String, String, Option<(String, Option<String>)>, String, Option<&'static str>);
/// (question, answered, error, answer unless it is being written)
type TurnShape<'a> = (&'a str, bool, Option<&'a str>, &'a str);

fn render_list(ui: &Ui) {
    let Some(wd) = ws() else { return };
    let (threads, ov, sel, q) = st(|s| {
        let ov = s.ov.clone().unwrap_or_default();
        let threads: Vec<QThread> = ov.threads.iter().filter(|t| matches(t, &s.filter)).cloned().collect();
        (threads, ov, s.sel.clone(), s.filter.clone())
    });
    let status = |id: &str| -> Option<&'static str> {
        if ov.answering.as_deref() == Some(id) {
            Some("working")
        } else if ov.waiting.iter().any(|x| x == id) {
            Some("idle")
        } else if ov.unread.iter().any(|x| x == id) {
            Some("back")
        } else {
            None
        }
    };
    let rows: Vec<ListRow> = threads
        .iter()
        .map(|t| {
            let last = t.turns.last().map(|x| x.asked_at.as_str()).unwrap_or(t.created.as_str());
            let when = fmt::short_when(Some(last));
            // Claude's threads say nothing extra; another AI's name its AI.
            let ai = wake_core::questions::thread_ai(t);
            let when = if ai == wake_core::questions::Ai::Claude { when } else { format!("{} · {when}", ai.name()) };
            let project = t.project.as_deref().map(|p| (project_name(ui, p), crate::views::colour_by_id(ui, p)));
            (t.id.clone(), t.title.clone(), project, when, status(&t.id))
        })
        .collect();
    let sig = format!("{rows:?}{q}{}", ov.threads.is_empty());
    if st(|s| s.list_sig == sig) {
        restore_list_sel(&wd, sel.as_deref());
        return;
    }
    st(|s| s.list_sig = sig);
    let had_focus = gtk::prelude::GtkWindowExt::focus(&ui.window).is_some_and(|f| f.is_ancestor(&wd.list));
    w::clear(&wd.list);
    if rows.is_empty() {
        let row = gtk::ListBoxRow::new();
        row.set_selectable(false);
        row.set_activatable(false);
        row.set_child(Some(&w::wrap(if q.is_empty() { "Your questions will be kept here." } else { "No question matches." }, &["qnone"])));
        wd.list.append(&row);
    }
    for (id, title, project, when, dot) in rows {
        let b = w::vbox(3, &["qrow"]);
        let line = w::hbox(8, &[]);
        let t = w::clip(&title, &["qt"]);
        line.append(&t);
        if let Some(d) = dot {
            let d = w::dot(d);
            d.set_tooltip_text(Some(match dot {
                Some("working") => "Being answered",
                Some("idle") => "Waiting its turn",
                _ => "New answer",
            }));
            line.append(&d);
        }
        b.append(&line);
        let meta = w::hbox(6, &["qm"]);
        if let Some((p, colour)) = &project {
            if let Some(c) = colour {
                meta.append(&w::dot(&format!("pc-{c}")));
            }
            meta.append(&w::label(p, &["qtag"]));
        }
        meta.append(&w::label(&when, &[]));
        b.append(&meta);
        let row = views::list_row(&id, &b);
        wd.list.append(&row);
    }
    restore_list_sel(&wd, sel.as_deref());
    if had_focus && let Some(r) = wd.list.selected_row() {
        r.grab_focus();
    }
}

fn restore_list_sel(wd: &W, sel: Option<&str>) {
    let mut c = wd.list.first_child();
    while let Some(x) = c {
        if let Ok(row) = x.clone().downcast::<gtk::ListBoxRow>()
            && sel.is_some_and(|s| row.widget_name() == s)
        {
            if !row.is_selected() {
                wd.list.select_row(Some(&row));
            }
            return;
        }
        c = x.next_sibling();
    }
    wd.list.unselect_all();
}

fn render_thread(ui: &Ui) {
    let Some(wd) = ws() else { return };
    let (t, ov) = st(|s| {
        let ov = s.ov.clone().unwrap_or_default();
        (s.sel.as_deref().and_then(|id| thread_of(&ov, id)), ov)
    });
    let answering = t.as_ref().is_some_and(|t| ov.answering.as_deref() == Some(t.id.as_str()));
    let waiting = t.as_ref().is_some_and(|t| ov.waiting.contains(&t.id));
    // Everything but the text of an answer being written: when only that changed, redraw it alone.
    let shape: Option<Vec<TurnShape>> = t.as_ref().map(|t| {
        let n = t.turns.len();
        t.turns
            .iter()
            .enumerate()
            .map(|(i, x)| {
                let streaming = i + 1 == n && (answering || waiting);
                (x.question.as_str(), x.answered_at.is_some(), x.error.as_deref(), if streaming { "" } else { x.answer.as_str() })
            })
            .collect()
    });
    let sig = format!("{:?}{:?}{answering}{waiting}{:?}{:?}", t.as_ref().map(|t| (&t.id, &t.project)), shape, ov.activity, ov.problem);
    let last_answer = t.as_ref().and_then(|t| t.turns.last()).map(|x| x.answer.clone()).unwrap_or_default();
    if st(|s| s.thread_sig == sig) {
        let streaming = st(|s| s.streaming.clone());
        if let Some((bx, shown)) = streaming
            && shown != last_answer
        {
            fill_answer(&bx, &last_answer);
            st(|s| s.streaming = Some((bx, last_answer)));
        }
        return;
    }
    let id = t.as_ref().map(|t| t.id.clone());
    st(|s| {
        s.thread_sig = sig;
        s.streaming = None;
        if s.shown != id {
            s.shown = id;
            s.stick = true;
        }
    });
    w::clear(&wd.thread);
    if let Some(p) = &ov.problem {
        wd.thread.append(&w::wrap(p, &["qerr"]));
    }
    let Some(t) = t else {
        let e = w::empty(
            "Ask anything",
            &format!(
                "Wake asks your own {ai} and keeps every thread here. Just a question: {Ai} can search the web. About a project: it can also read that project's files. It never changes anything. Settings (,) picks which AI answers.",
                ai = crate::setview::current_ai().id(),
                Ai = crate::setview::current_ai().name()
            ),
        );
        e.set_margin_top(60);
        wd.thread.append(&e);
        return;
    };
    let n = t.turns.len();
    for (i, turn) in t.turns.iter().enumerate() {
        let last = i + 1 == n;
        let tb = w::vbox(10, &["qturn"]);
        let q = w::wrap(&turn.question, &["qq"]);
        q.set_selectable(true);
        q.set_focusable(false);
        tb.append(&q);
        let mut meta = fmt::when(Some(&turn.asked_at));
        if i == 0 {
            meta.push_str(" · ");
            meta.push_str(&t.project.as_deref().map_or_else(|| "just a question".to_string(), |p| format!("about {}", project_name(ui, p))));
            // Which AI answers this thread (follow-ups stay with it).
            meta.push_str(&format!(" · {}", wake_core::questions::thread_ai(&t).name()));
        }
        tb.append(&w::label(&meta, &["qmeta"]));
        let ab = w::vbox(10, &["qa"]);
        fill_answer(&ab, &turn.answer);
        tb.append(&ab);
        if last && answering {
            st(|s| s.streaming = Some((ab.clone(), turn.answer.clone())));
            let l = w::hbox(8, &["qstate"]);
            l.append(&w::dot("working"));
            l.append(&w::label(&format!("{}…", ov.activity.as_deref().unwrap_or(if turn.answer.is_empty() { "thinking" } else { "writing" })), &[]));
            l.append(&w::spacer());
            l.append(&w::kbd("Esc"));
            l.append(&w::label("stops", &["hint"]));
            tb.append(&l);
        } else if last && waiting {
            let l = w::hbox(8, &["qstate"]);
            l.append(&w::dot("idle"));
            l.append(&w::label("waiting: one question is answered at a time", &[]));
            l.append(&w::spacer());
            l.append(&w::kbd("Esc"));
            l.append(&w::label("drops it", &["hint"]));
            tb.append(&l);
        } else if let Some(e) = &turn.error {
            let l = w::hbox(8, &["qerrline"]);
            let m = w::wrap(e, &["qerr"]);
            l.append(&m);
            if last {
                let ui2 = ui.clone();
                l.append(&w::button("Ask again", None, &["sm"], move || retry(&ui2)));
            }
            tb.append(&l);
        } else if last && !turn.answer.trim().is_empty() {
            let l = w::hbox(8, &[]);
            let ui2 = ui.clone();
            l.append(&w::button("Copy the answer", None, &["sm", "ghost"], move || copy(&ui2)));
            tb.append(&l);
        }
        wd.thread.append(&tb);
    }
}

/// Draws an answer's Markdown into `bx`: text blocks as Pango markup, code in monospace boxes.
fn fill_answer(bx: &gtk::Box, text: &str) {
    w::clear(bx);
    let mut list: Option<gtk::Box> = None;
    for b in md::blocks(text) {
        if let md::Block::Item(level, mark, m) = &b {
            let l = list.get_or_insert_with(|| {
                let l = w::vbox(5, &["qlistblock"]);
                bx.append(&l);
                l
            });
            let row = w::hbox(8, &[]);
            row.set_margin_start(*level as i32 * 20);
            let mk = w::label(mark, &["qtext", "qmark"]);
            mk.set_valign(gtk::Align::Start);
            mk.set_size_request(16, -1);
            row.append(&mk);
            let t = w::wrap("", &["qtext"]);
            t.set_markup(m);
            t.set_selectable(true);
            t.set_focusable(false);
            row.append(&t);
            l.append(&row);
            continue;
        }
        list = None;
        match b {
            md::Block::Item(..) => {}
            md::Block::Text(m) => {
                let l = w::wrap("", &["qtext"]);
                l.set_markup(&m);
                l.set_selectable(true);
                l.set_focusable(false);
                bx.append(&l);
            }
            md::Block::Table(head, body) => bx.append(&table(&head, &body)),
            md::Block::Code(c) => {
                let l = w::wrap(&c, &["qcode"]);
                l.set_wrap_mode(gtk::pango::WrapMode::Char);
                l.set_selectable(true);
                l.set_focusable(false);
                bx.append(&l);
            }
        }
    }
}

/// A Markdown table as a grid: wrapped cells, each column given a share of the reading width by
/// how much it holds, a quiet header and a hairline under each row.
fn table(head: &[String], body: &[Vec<String>]) -> gtk::Widget {
    let n = head.len().max(body.iter().map(Vec::len).max().unwrap_or(0)).max(1);
    let plain = |m: &str| gtk::pango::parse_markup(m, '\0').map(|(_, s, _)| s.chars().count()).unwrap_or(m.len());
    let mut weight = vec![1usize; n];
    for row in std::iter::once(head).chain(body.iter().map(Vec::as_slice)) {
        for (i, c) in row.iter().enumerate().take(n) {
            weight[i] = weight[i].max(plain(c).min(160));
        }
    }
    let total: usize = weight.iter().sum();
    const CHARS: usize = 86;
    let width: Vec<i32> = weight.iter().map(|w| ((w * CHARS / total.max(1)).max(10)) as i32).collect();
    let grid = gtk::Grid::builder().column_spacing(0).row_spacing(0).build();
    grid.add_css_class("qtable");
    let cell = |m: &str, r: i32, c: usize, classes: &[&str]| {
        let l = w::wrap("", classes);
        l.set_markup(m);
        l.set_max_width_chars(width[c]);
        l.set_width_chars(width[c].min(14));
        // Fill the row's height (text at the top) so every cell's line under it sits level.
        l.set_valign(gtk::Align::Fill);
        l.set_yalign(0.0);
        l.set_selectable(true);
        l.set_focusable(false);
        grid.attach(&l, c as i32, r, 1, 1);
    };
    let mut r = 0;
    if !head.is_empty() {
        for c in 0..n {
            cell(head.get(c).map_or("", String::as_str), r, c, &["qtext", "qth"]);
        }
        r += 1;
    }
    for (i, row) in body.iter().enumerate() {
        let last = i + 1 == body.len();
        for c in 0..n {
            cell(row.get(c).map_or("", String::as_str), r, c, if last { &["qtext", "qtd", "qlast"] } else { &["qtext", "qtd"] });
        }
        r += 1;
    }
    grid.upcast()
}

/// The context dropdown: "Just a question" and every tracked project. A thread keeps its own.
fn sync_ctx(ui: &Ui) {
    let Some(wd) = ws() else { return };
    let (mut ids, mut names) = (vec![None], vec!["Just a question".to_string()]);
    if let Some(ov) = ui.st.borrow().ov.as_ref() {
        for p in ov.projects.iter().filter(|p| !p.removed) {
            ids.push(Some(p.id.clone()));
            names.push(p.name.clone());
        }
    }
    let (sel_thread, changed) = st(|s| {
        let t = s.sel.as_deref().and_then(|id| s.ov.as_ref().and_then(|o| thread_of(o, id)));
        let changed = s.ctx_ids != ids || s.ctx_names != names;
        (t, changed)
    });
    if changed {
        let current = st(|s| s.ctx_ids.get(wd.ctx.selected() as usize).cloned().flatten());
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let manual = st(|s| {
            s.ctx_ids = ids.clone();
            s.ctx_names = names.clone();
            s.ctx_manual
        });
        wd.ctx.set_model(Some(&gtk::StringList::new(&refs)));
        wd.ctx.set_selected(ids.iter().position(|i| *i == current).unwrap_or(0) as u32);
        st(|s| s.ctx_manual = manual);
    }
    let want = match &sel_thread {
        Some(t) => Some(t.project.clone()),
        None if !st(|s| s.ctx_manual) => Some(ui.st.borrow().proj_filter.clone()),
        None => None,
    };
    if let Some(want) = want {
        let at = ids.iter().position(|i| *i == want).unwrap_or(0) as u32;
        if wd.ctx.selected() != at {
            let manual = st(|s| s.ctx_manual);
            wd.ctx.set_selected(at);
            st(|s| s.ctx_manual = manual);
        }
    }
    wd.ctx.set_sensitive(sel_thread.is_none());
    wd.ctx.set_tooltip_text(Some(if sel_thread.is_some() {
        "A follow-up stays in its thread's context. n starts a new question."
    } else {
        "What the AI may look at: the web only, or also a project's files (never changing them)"
    }));
    ctx_note(ui);
}

fn ctx_note(ui: &Ui) {
    let Some(wd) = ws() else { return };
    let (project, follow) = st(|s| (s.ctx_ids.get(wd.ctx.selected() as usize).cloned().flatten(), s.sel.is_some()));
    let text = match (&project, follow) {
        (_, true) => {
            let ai = st(|s| s.sel.as_deref().and_then(|id| s.ov.as_ref().and_then(|o| thread_of(o, id)))).map(|t| wake_core::questions::thread_ai(&t)).unwrap_or_default();
            format!("A follow-up in this thread, answered by {}", ai.name())
        }
        (None, false) => format!("{} can search the web", crate::setview::current_ai().name()),
        (Some(p), false) => format!("{} can read {}'s files and search the web; it changes nothing", crate::setview::current_ai().name(), project_name(ui, p)),
    };
    if wd.ctx_note.text() != text {
        wd.ctx_note.set_text(&text);
    }
}

// ---------- actions ----------

fn select(ui: &Ui, id: Option<String>) {
    st(|s| {
        s.sel = id;
        s.stick = true;
    });
    render(ui);
    mark_seen(ui);
    views::render_status(ui);
}

/// The view became visible.
pub fn shown(ui: &Ui) {
    let Some(wd) = ws() else { return };
    render(ui);
    mark_seen(ui);
    let empty = st(|s| s.ov.as_ref().is_none_or(|o| o.threads.is_empty()));
    if empty || st(|s| s.sel.is_none()) {
        wd.input.grab_focus();
    } else if let Some(r) = wd.list.selected_row() {
        r.grab_focus();
    } else {
        wd.list.grab_focus();
    }
}

pub fn focus_input(ui: &Ui) {
    let _ = ui;
    if let Some(wd) = ws() {
        wd.input.grab_focus();
    }
}

/// The window's width crossed the line where the thread list and the thread stop fitting side by
/// side (layout.rs): one at a time, the thread first.
pub fn relayout(narrow: bool) {
    if st(|s| std::mem::replace(&mut s.narrow, narrow) != narrow) {
        st(|s| s.list_shown = false);
        apply_columns();
    }
}

fn show_list(on: bool) {
    st(|s| s.list_shown = on);
    apply_columns();
}

fn apply_columns() {
    let Some(wd) = ws() else { return };
    let (narrow, list) = st(|s| (s.narrow, s.list_shown));
    wd.side.set_visible(!narrow || list);
    wd.side.set_hexpand(narrow);
    wd.main.set_visible(!narrow || !list);
    wd.back.set_visible(narrow);
    if let Some(bar) = wd.back.parent() {
        bar.set_visible(narrow);
    }
}

fn new_thread(ui: &Ui, focus: bool) {
    show_list(false);
    st(|s| s.ctx_manual = false);
    select(ui, None);
    if focus {
        focus_input(ui);
    }
}

fn input_text(wd: &W) -> String {
    let b = wd.input.buffer();
    b.text(&b.start_iter(), &b.end_iter(), false).trim().to_string()
}

fn send(ui: &Ui) {
    let Some(wd) = ws() else { return };
    let text = input_text(&wd);
    if text.is_empty() {
        return;
    }
    let (thread, project) = st(|s| (s.sel.clone(), s.ctx_ids.get(wd.ctx.selected() as usize).cloned().flatten()));
    if ui.demo {
        app::toast(ui, "Demo data: Wake doesn't send questions to Claude here.", false);
        return;
    }
    wd.input.set_editable(false);
    app::bg(
        ui,
        move |src| src.ask(thread.as_deref(), &text, project.as_deref()),
        move |ui, r| {
            let Some(wd) = ws() else { return };
            wd.input.set_editable(true);
            match r {
                Ok(id) => {
                    wd.input.buffer().set_text("");
                    st(|s| {
                        s.sel = Some(id);
                        s.stick = true;
                        s.ctx_manual = false;
                    });
                    fetch(ui);
                    poll(ui);
                }
                Err(e) => app::toast(ui, &e, true),
            }
        },
    );
}

fn selected(ui: &Ui) -> Option<QThread> {
    let _ = ui;
    st(|s| s.sel.as_deref().and_then(|id| s.ov.as_ref().and_then(|o| thread_of(o, id))))
}

/// Esc / Ctrl+C: stop the selected thread's answer (or drop its waiting question). False if there
/// was nothing to stop.
fn stop(ui: &Ui) -> bool {
    let busy = st(|s| {
        let id = s.sel.clone()?;
        let o = s.ov.as_ref()?;
        (o.answering.as_deref() == Some(id.as_str()) || o.waiting.contains(&id)).then_some(id)
    });
    let Some(id) = busy else { return false };
    app::bg(
        ui,
        move |src| src.cancel_question(&id),
        |ui, stopped| {
            if stopped {
                app::toast(ui, "Stopped. What came so far is kept.", false);
            }
            fetch(ui);
        },
    );
    true
}

fn retry(ui: &Ui) {
    let Some(t) = selected(ui) else { return };
    if t.turns.last().is_none_or(|x| x.error.is_none()) {
        return;
    }
    if ui.demo {
        app::toast(ui, "Demo data: Wake doesn't send questions to Claude here.", false);
        return;
    }
    let id = t.id.clone();
    app::bg(
        ui,
        move |src| src.retry_question(&id),
        |ui, r| match r {
            Ok(()) => {
                st(|s| s.stick = true);
                fetch(ui);
                poll(ui);
            }
            Err(e) => app::toast(ui, &e, true),
        },
    );
}

fn copy(ui: &Ui) {
    let Some(t) = selected(ui) else { return };
    let Some(a) = t.turns.iter().rev().map(|x| x.answer.trim()).find(|a| !a.is_empty()) else {
        app::toast(ui, "No answer to copy yet.", false);
        return;
    };
    ui.window.clipboard().set_text(a);
    app::toast(ui, "Copied the answer.", false);
}

fn delete(ui: &Ui) {
    let Some(t) = selected(ui) else { return };
    let id = t.id.clone();
    let ai = wake_core::questions::thread_ai(&t);
    dialogs::confirm(
        ui,
        &format!("Delete \u{201c}{}\u{201d}?", t.title),
        &[format!("{} goes from Wake. {}'s own copy of the conversation stays in ~/.{}.", fmt::pl(t.turns.len() as u64, "question and its answer", "questions and their answers"), ai.name(), ai.id())],
        &[],
        "Delete",
        move |ui| {
            let id = id.clone();
            app::bg(
                ui,
                move |src| src.delete_thread(&id),
                |ui, r| match r {
                    Ok(()) => {
                        st(|s| s.sel = None);
                        fetch(ui);
                        app::toast(ui, "Deleted.", false);
                    }
                    Err(e) => app::toast(ui, &e, true),
                },
            );
        },
    );
}

fn move_sel(ui: &Ui, d: i32) {
    let Some(wd) = ws() else { return };
    let mut rows = Vec::new();
    let mut c = wd.list.first_child();
    while let Some(x) = c {
        if let Ok(r) = x.clone().downcast::<gtk::ListBoxRow>()
            && r.is_selectable()
        {
            rows.push(r);
        }
        c = x.next_sibling();
    }
    if rows.is_empty() {
        return;
    }
    let sel = st(|s| s.sel.clone());
    let i = rows.iter().position(|r| Some(r.widget_name().as_str()) == sel.as_deref());
    let i = match i {
        None if d > 0 => 0,
        None => rows.len() - 1,
        Some(i) => (i as i32 + d).clamp(0, rows.len() as i32 - 1) as usize,
    };
    let in_filter = gtk::prelude::GtkWindowExt::focus(&ui.window).is_some_and(|f| f.is_ancestor(&wd.filter));
    select(ui, Some(rows[i].widget_name().to_string()));
    if in_filter {
        wd.filter.grab_focus_without_selecting();
    } else {
        rows[i].grab_focus();
    }
}

// ---------- keys ----------

/// The view's keys, before the window's own. None: not ours, carry on.
pub fn on_key(ui: &Ui, key: gdk::Key, mods: gdk::ModifierType) -> Option<glib::Propagation> {
    use gdk::Key as K;
    use glib::Propagation::Stop;
    let wd = ws()?;
    let ctrl = mods.contains(gdk::ModifierType::CONTROL_MASK);
    let alt = mods.contains(gdk::ModifierType::ALT_MASK);
    let shift = mods.contains(gdk::ModifierType::SHIFT_MASK);
    let focus = gtk::prelude::GtkWindowExt::focus(&ui.window);
    let in_input = focus.as_ref().is_some_and(|f| f.is_ancestor(&wd.input) || f == wd.input.upcast_ref::<gtk::Widget>());
    let in_filter = focus.as_ref().is_some_and(|f| f.is_ancestor(&wd.filter));
    if in_input {
        return match key {
            K::Return | K::KP_Enter if !shift && !ctrl && !alt => {
                send(ui);
                Some(Stop)
            }
            K::Escape => {
                if !stop(ui) {
                    focus_list(ui);
                }
                Some(Stop)
            }
            k if ctrl && k.to_lower() == K::c && !wd.input.buffer().has_selection() => stop(ui).then_some(Stop),
            _ => None,
        };
    }
    if in_filter {
        return match key {
            K::Escape => {
                if wd.filter.text().is_empty() {
                    focus_list(ui);
                } else {
                    wd.filter.set_text("");
                }
                Some(Stop)
            }
            K::Down | K::Up => {
                move_sel(ui, if key == K::Down { 1 } else { -1 });
                Some(Stop)
            }
            K::Return | K::KP_Enter => {
                focus_list(ui);
                Some(Stop)
            }
            _ => None,
        };
    }
    // Another text field (the capture bar): its keys are its own.
    if focus.as_ref().is_some_and(|f| f.is::<gtk::Text>() || f.is::<gtk::TextView>()) {
        return None;
    }
    if ctrl && !alt && key.to_lower() == K::c {
        return stop(ui).then_some(Stop);
    }
    if ctrl || alt {
        return None;
    }
    let on_control = focus.as_ref().is_some_and(|f| f.is::<gtk::Button>() || f.is::<gtk::DropDown>() || f.ancestor(gtk::DropDown::static_type()).is_some());
    match key.to_lower() {
        K::Down | K::KP_Down => move_sel(ui, 1),
        K::Up | K::KP_Up => move_sel(ui, -1),
        K::Return | K::KP_Enter if on_control => return None,
        K::Return | K::KP_Enter => focus_input(ui),
        K::Delete | K::KP_Delete => delete(ui),
        K::Escape => return stop(ui).then_some(Stop),
        _ => return None,
    }
    Some(Stop)
}

fn focus_list(ui: &Ui) {
    let _ = ui;
    let Some(wd) = ws() else { return };
    if let Some(r) = wd.list.selected_row() {
        r.grab_focus();
    } else {
        wd.list.grab_focus();
    }
}

/// `--show questions[:thread]` (or `questions:new`): open the view on that thread.
pub fn show(ui: &Ui, arg: Option<String>) {
    app::set_view(ui, View::Questions);
    let ui2 = ui.clone();
    let pick = move |ui: &Ui| {
        let id = match arg.as_deref() {
            Some("new") => None,
            Some(id) => Some(id.to_string()),
            None => st(|s| s.ov.as_ref().and_then(|o| o.threads.first().map(|t| t.id.clone()))),
        };
        select(ui, id);
    };
    // Pick once the threads have loaded (or after ten seconds, whatever there is).
    let tries = std::cell::Cell::new(0);
    glib::timeout_add_local(Duration::from_millis(100), move || {
        tries.set(tries.get() + 1);
        if st(|s| s.ov.as_ref().is_some_and(|o| !o.threads.is_empty())) || tries.get() > 100 {
            pick(&ui2);
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    });
}

// ---------- demo ----------

/// Made-up threads for `wake --demo`: answered (Markdown and code), one being written, one waiting,
/// one unread, one failed. Nothing is ever sent to Claude.
pub struct Demo {
    ov: std::sync::Mutex<QOverview>,
}

impl Default for Demo {
    fn default() -> Self {
        Self::new()
    }
}

impl Demo {
    pub fn new() -> Demo {
        let ago = |s: i64| glib::DateTime::now_utc().and_then(|d| d.add_seconds(-s as f64)).ok().and_then(|d| d.format_iso8601().ok()).map(|s| s.to_string()).unwrap_or_default();
        let turn = |q: &str, a: &str, at: i64, done: bool, err: Option<&str>| QTurn {
            question: q.into(),
            answer: a.into(),
            asked_at: ago(at),
            answered_at: (done && err.is_none()).then(|| ago(at - 9)),
            error: err.map(String::from),
        };
        let thread = |id: &str, project: Option<&str>, turns: Vec<QTurn>| QThread {
            id: id.into(),
            title: wake_core::questions::title_of(&turns[0].question),
            project: project.map(String::from),
            claude_session: Some(format!("{id}-session")),
            created: turns[0].asked_at.clone(),
            turns,
            // One made-up thread was answered by Codex, to show how that reads.
            ai: (id == "qdemo-git").then(|| "codex".to_string()),
        };
        let threads = vec![
            thread("qdemo-cache", None, vec![turn("What's the best way to cache a Rust build inside a distrobox?", "", 20, false, None)]),
            thread(
                "qdemo-mem",
                None,
                vec![turn(
                    "Is half a gigabyte per claude process normal?",
                    "Yes, roughly. Each `claude` process is a Node runtime that keeps the whole conversation, its tools and their caches in memory, so **300 to 600 MB** is typical.\n\nWhat makes it grow:\n\n- long conversations (the context stays in memory)\n- subagents, which are",
                    40,
                    false,
                    None,
                )],
            ),
            thread(
                "qdemo-git",
                None,
                vec![
                    turn(
                        "How do I undo the last git commit but keep the changes?",
                        "Use a **soft reset**:\n\n```sh\ngit reset --soft HEAD~1\n```\n\nThe commit is gone and your changes stay *staged*, ready to commit again. To keep them but unstaged, use a mixed reset: `git reset HEAD~1`.\n\n- **Already pushed?** Prefer `git revert HEAD`: it adds a new commit that undoes the last one, so nobody else's history changes.\n- Lost something? `git reflog` lists where `HEAD` has been, for about 90 days.",
                        3 * 3600,
                        true,
                        None,
                    ),
                    turn(
                        "And if I only want one file back as it was?",
                        "Restore just that file from the commit before:\n\n```sh\ngit restore --source=HEAD~1 -- path/to/file.rs\n```\n\nOnly the working copy changes; nothing is committed until you say so.",
                        3 * 3600 - 300,
                        true,
                        None,
                    ),
                ],
            ),
            thread(
                "qdemo-sky",
                None,
                vec![turn(
                    "Difference between Rayleigh and Mie scattering, in two sentences?",
                    "**Rayleigh** scattering comes from particles much smaller than the wavelength (air molecules) and favours short wavelengths, which is why the sky is blue. **Mie** scattering comes from particles about the wavelength's size or larger (water droplets, dust) and treats all colours alike, which is why clouds are white.",
                    26 * 3600,
                    true,
                    None,
                )],
            ),
            thread(
                "qdemo-index",
                Some("Gradebook"),
                vec![turn(
                    "Where does the invite email get its subject line?",
                    "From the mailer's locale file: `config/locales/en.yml`, key `invite_mailer.invite.subject`. The mailer (`app/mailers/invite_mailer.rb`) calls `default_i18n_subject` with the school's name, so the subject reads *\"You're invited to join <school> on Gradebook\"*.",
                    2 * 86_400,
                    true,
                    None,
                )],
            ),
            thread("qdemo-deploy", None, vec![turn("Summarise what changed in GTK 4.18", "", 4 * 86_400, false, Some("Claude isn't logged in. Run claude once in a terminal to log in, then retry."))]),
        ];
        Demo {
            ov: std::sync::Mutex::new(QOverview {
                threads,
                answering: Some("qdemo-mem".into()),
                activity: None,
                waiting: vec!["qdemo-cache".into()],
                unread: vec!["qdemo-sky".into()],
                seq: 1,
                problem: None,
            }),
        }
    }

    pub fn questions(&self, since: u64) -> Option<QOverview> {
        let ov = self.ov.lock().unwrap_or_else(|e| e.into_inner());
        (since != ov.seq).then(|| ov.clone())
    }

    pub fn cancel(&self, thread: &str) -> bool {
        let mut ov = self.ov.lock().unwrap_or_else(|e| e.into_inner());
        let mut any = false;
        if ov.answering.as_deref() == Some(thread) {
            ov.answering = None;
            any = true;
        }
        if ov.waiting.iter().any(|w| w == thread) {
            ov.waiting.retain(|w| w != thread);
            any = true;
        }
        if any {
            if let Some(t) = ov.threads.iter_mut().find(|t| t.id == thread)
                && let Some(x) = t.turns.last_mut()
            {
                x.error = Some(if x.answer.is_empty() { "Stopped before Claude answered.".into() } else { "Stopped.".into() });
            }
            ov.seq += 1;
        }
        any
    }

    pub fn delete(&self, thread: &str) -> Result<(), String> {
        let mut ov = self.ov.lock().unwrap_or_else(|e| e.into_inner());
        ov.threads.retain(|t| t.id != thread);
        ov.unread.retain(|t| t != thread);
        ov.waiting.retain(|t| t != thread);
        if ov.answering.as_deref() == Some(thread) {
            ov.answering = None;
        }
        ov.seq += 1;
        Ok(())
    }

    pub fn mark_read(&self, thread: &str) {
        let mut ov = self.ov.lock().unwrap_or_else(|e| e.into_inner());
        if ov.unread.iter().any(|t| t == thread) {
            ov.unread.retain(|t| t != thread);
            ov.seq += 1;
        }
    }
}

// ---------- Markdown to Pango ----------

/// Just enough Markdown for answers: paragraphs, headings, bullets and numbers, quotes, bold,
/// italic, inline code, links (their text), and fenced code blocks (and tables) in monospace.
/// Everything else is escaped: an answer can never inject markup.
pub mod md {
    #[derive(Debug, Clone, PartialEq)]
    pub enum Block {
        /// Pango markup.
        Text(String),
        /// Plain text, shown in monospace.
        Code(String),
        /// A list item: nesting level, its mark ("•", "2."), and its text as Pango markup.
        Item(usize, String, String),
        /// A table: the header row, then the body rows, every cell as Pango markup.
        Table(Vec<String>, Vec<Vec<String>>),
    }

    /// A table row's cells: split on `|` outside `code`, with `\|` kept as a bar.
    fn cells(line: &str) -> Vec<String> {
        let s = line.trim();
        let s = s.strip_prefix('|').unwrap_or(s);
        let s = s.strip_suffix('|').unwrap_or(s);
        let (mut out, mut cur, mut code, mut esc_next) = (Vec::new(), String::new(), false, false);
        for c in s.chars() {
            if esc_next {
                cur.push(c);
                esc_next = false;
                continue;
            }
            match c {
                '\\' => esc_next = true,
                '`' => {
                    code = !code;
                    cur.push(c);
                }
                '|' if !code => out.push(std::mem::take(&mut cur)),
                c => cur.push(c),
            }
        }
        out.push(cur);
        out.iter().map(|c| inline(c.trim())).collect()
    }

    fn divider(line: &str) -> bool {
        line.contains('-') && line.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
    }

    pub fn esc(s: &str) -> String {
        let mut o = String::with_capacity(s.len());
        for c in s.chars() {
            match c {
                '&' => o.push_str("&amp;"),
                '<' => o.push_str("&lt;"),
                '>' => o.push_str("&gt;"),
                '"' => o.push_str("&quot;"),
                '\'' => o.push_str("&#39;"),
                c if c.is_control() && c != '\n' && c != '\t' => {}
                c => o.push(c),
            }
        }
        o
    }

    /// Inline code and links in the theme now showing (Pango can't use the stylesheet's names).
    fn code_span() -> String {
        format!("<span font_family=\"JetBrains Mono, monospace\" size=\"92%\" foreground=\"{}\" background=\"{}\">", crate::theme::hex("soft"), crate::theme::hex("codespan"))
    }

    fn link_span() -> String {
        format!("<span foreground=\"{}\">", crate::theme::accent_hex())
    }

    fn alnum(c: Option<char>) -> bool {
        c.is_some_and(|c| c.is_alphanumeric())
    }

    /// One line's inline Markdown as markup.
    pub fn inline(s: &str) -> String {
        let mut out = String::new();
        let mut i = 0;
        'next: while i < s.len() {
            let rest = &s[i..];
            let prev = s[..i].chars().next_back();
            // `code`, ``code with ` inside``
            if rest.starts_with('`') {
                let n = rest.bytes().take_while(|&c| c == b'`').count();
                if let Some(end) = rest[n..].find(&rest[..n]) {
                    let code = &rest[n..n + end];
                    out.push_str(&code_span());
                    out.push_str(&esc(code.strip_prefix(' ').and_then(|c| c.strip_suffix(' ')).unwrap_or(code)));
                    out.push_str("</span>");
                    i += n + end + n;
                    continue 'next;
                }
            }
            // **bold**, __bold__, then *italic*, _italic_ (never inside a word for _)
            for (d, tag) in [("**", "b"), ("__", "b"), ("*", "i"), ("_", "i")] {
                if !rest.starts_with(d) || (tag == "i" && rest[d.len()..].starts_with(d)) || (d.starts_with('_') && alnum(prev)) {
                    continue;
                }
                let body = &rest[d.len()..];
                if body.starts_with(char::is_whitespace) {
                    continue;
                }
                let Some(end) = body.find(d) else { continue };
                let inner = &body[..end];
                let next = body[end + d.len()..].chars().next();
                if inner.is_empty() || inner.ends_with(char::is_whitespace) || (d.starts_with('_') && alnum(next)) {
                    continue;
                }
                out.push_str(&format!("<{tag}>{}</{tag}>", inline(inner)));
                i += d.len() + end + d.len();
                continue 'next;
            }
            // [text](url): the text, in the link colour (never clickable: no surprises)
            if rest.starts_with('[')
                && let Some(close) = rest.find("](")
                && let Some(end) = rest[close + 2..].find(')')
                && !rest[1..close].contains('[')
            {
                out.push_str(&link_span());
                out.push_str(&inline(&rest[1..close]));
                out.push_str("</span>");
                i += close + 2 + end + 1;
                continue 'next;
            }
            let c = rest.chars().next().unwrap_or(' ');
            out.push_str(&esc(c.encode_utf8(&mut [0; 4])));
            i += c.len_utf8();
        }
        out
    }

    fn list_item(line: &str) -> Option<(usize, String, &str)> {
        let indent = line.len() - line.trim_start().len();
        let t = line.trim_start();
        for m in ["- ", "* ", "+ "] {
            if let Some(r) = t.strip_prefix(m) {
                return Some((indent, "•".into(), r));
            }
        }
        let digits = t.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 && digits < 4 {
            let after = &t[digits..];
            if let Some(r) = after.strip_prefix(". ").or_else(|| after.strip_prefix(") ")) {
                return Some((indent, format!("{}.", &t[..digits]), r));
            }
        }
        None
    }

    pub fn blocks(md: &str) -> Vec<Block> {
        let mut out = Vec::new();
        let mut text: Vec<String> = Vec::new();
        let mut para = false;
        let mut code: Option<(String, Vec<&str>)> = None;
        let mut table: Vec<&str> = Vec::new();
        let flush_text = |text: &mut Vec<String>, out: &mut Vec<Block>| {
            while text.last().is_some_and(|l| l.is_empty()) {
                text.pop();
            }
            if !text.is_empty() {
                out.push(Block::Text(text.join("\n")));
            }
            text.clear();
        };
        let flush_table = |table: &mut Vec<&str>, out: &mut Vec<Block>| {
            if !table.is_empty() {
                // A header row needs the |---| divider straight after it; without one every row is body.
                let has_head = table.len() > 1 && divider(table[1]);
                let rows: Vec<Vec<String>> = table.iter().copied().filter(|l| !divider(l)).map(cells).collect();
                let (head, body) = if has_head { (rows[0].clone(), rows[1..].to_vec()) } else { (Vec::new(), rows) };
                out.push(Block::Table(head, body));
                table.clear();
            }
        };
        for line in md.lines() {
            let t = line.trim_start();
            if let Some((fence, lines)) = code.as_mut() {
                if t.trim_end().starts_with(fence.as_str()) && t.trim_end().chars().all(|c| fence.starts_with(c)) {
                    out.push(Block::Code(lines.join("\n")));
                    code = None;
                } else {
                    lines.push(line);
                }
                continue;
            }
            if t.starts_with("```") || t.starts_with("~~~") {
                flush_table(&mut table, &mut out);
                flush_text(&mut text, &mut out);
                para = false;
                let ch = t.chars().next().unwrap_or('`');
                let n = t.chars().take_while(|&c| c == ch).count();
                code = Some((ch.to_string().repeat(n), Vec::new()));
                continue;
            }
            if t.starts_with('|') {
                flush_text(&mut text, &mut out);
                para = false;
                table.push(line.trim());
                continue;
            }
            flush_table(&mut table, &mut out);
            if t.trim().is_empty() {
                if text.last().is_some_and(|l| !l.is_empty()) {
                    text.push(String::new());
                }
                para = false;
                continue;
            }
            if t.trim_end().len() >= 3 && t.trim_end().chars().all(|c| c == '-' || c == '*' || c == '_') {
                para = false;
                continue;
            }
            let hashes = t.bytes().take_while(|&c| c == b'#').count();
            if (1..=6).contains(&hashes) && t[hashes..].starts_with(' ') {
                if text.last().is_some_and(|l| !l.is_empty()) {
                    text.push(String::new());
                }
                text.push(format!("<b>{}</b>", inline(t[hashes..].trim())));
                para = false;
                continue;
            }
            if let Some((indent, mark, rest)) = list_item(line) {
                flush_text(&mut text, &mut out);
                out.push(Block::Item(indent / 2, mark, inline(rest.trim())));
                para = true;
                continue;
            }
            if let Some(q) = t.strip_prefix('>') {
                text.push(format!("<span foreground=\"{}\">│ {}</span>", crate::theme::hex("muted"), inline(q.trim())));
                para = false;
                continue;
            }
            let hard = line.ends_with("  ");
            // a line straight after a list item carries on that item
            if para
                && text.is_empty()
                && let Some(Block::Item(_, _, m)) = out.last_mut()
            {
                m.push(' ');
                m.push_str(&inline(t.trim_end()));
                continue;
            }
            if para && let Some(last) = text.last_mut() {
                last.push(' ');
                last.push_str(&inline(t.trim_end()));
            } else {
                text.push(inline(t.trim_end()));
            }
            para = !hard;
        }
        flush_table(&mut table, &mut out);
        if let Some((_, lines)) = code {
            // still being written: show what has come so far
            out.push(Block::Code(lines.join("\n")));
        }
        // keep code blocks and text in their order: text before a fence was flushed there
        flush_text(&mut text, &mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::md::{Block, blocks, inline};

    fn valid(m: &str) -> bool {
        gtk4::pango::parse_markup(m, '\0').is_ok()
    }

    #[test]
    fn inline_markdown_becomes_markup() {
        assert_eq!(inline("plain"), "plain");
        assert_eq!(inline("a **bold** move"), "a <b>bold</b> move");
        assert_eq!(inline("an *aside* and _this_"), "an <i>aside</i> and <i>this</i>");
        assert!(inline("run `git status` now").contains(">git status</span>"));
        assert_eq!(inline("snake_case_name stays"), "snake_case_name stays");
        assert_eq!(inline("2 * 3 * 4"), "2 * 3 * 4");
        assert_eq!(inline("see [the docs](https://example.com)"), "see <span foreground=\"#38CEFF\">the docs</span>");
        assert_eq!(inline("**unclosed"), "**unclosed");
    }

    #[test]
    fn nothing_in_an_answer_can_inject_markup() {
        for nasty in [
            "<b>bold?</b> & <span foreground=\"red\">x</span>",
            "**<i>x</i>**",
            "`</span><span size=\"99999\">`",
            "*a<b*",
            "[<u>x</u>](http://a)",
            "# <big>",
            "- <tt>",
            "> </span>",
            "a & b < c > d \" e ' f",
            "\u{1}\u{7} control",
        ] {
            for b in blocks(nasty) {
                if let Block::Text(m) | Block::Item(_, _, m) = b {
                    assert!(valid(&m), "{nasty:?} gave invalid markup {m:?}");
                    assert!(!m.contains("<b>bold?") && !m.contains("<i>x") && !m.contains("<u>") && !m.contains("<big>") && !m.contains("<tt>"), "{m}");
                }
            }
        }
        assert_eq!(inline("<b>"), "&lt;b&gt;");
    }

    #[test]
    fn blocks_keep_paragraphs_lists_and_code_in_order() {
        let md = "Use a **soft reset**:\n\n```sh\ngit reset --soft HEAD~1\n```\n\nThen:\n- one\n- two *x*\n  continued\n  - nested\n\n1. first\n## Heading\nlast line";
        let b = blocks(md);
        assert_eq!(
            b,
            vec![
                Block::Text("Use a <b>soft reset</b>:".into()),
                Block::Code("git reset --soft HEAD~1".into()),
                Block::Text("Then:".into()),
                Block::Item(0, "•".into(), "one".into()),
                Block::Item(0, "•".into(), "two <i>x</i> continued".into()),
                Block::Item(1, "•".into(), "nested".into()),
                Block::Item(0, "1.".into(), "first".into()),
                Block::Text("<b>Heading</b>\nlast line".into()),
            ]
        );
        for x in &b {
            if let Block::Text(m) | Block::Item(_, _, m) = x {
                assert!(valid(m));
            }
        }
        // an unfinished fence (still streaming) shows as code so far
        assert_eq!(blocks("Try:\n```\nls -la"), vec![Block::Text("Try:".into()), Block::Code("ls -la".into())]);
        // code is plain text, never markup
        assert_eq!(blocks("```\n<b>&</b>\n```"), vec![Block::Code("<b>&</b>".into())]);
        // tables become cells: header, body, inline Markdown inside, the divider dropped
        assert_eq!(blocks("| a | b |\n|---|---|\n| 1 | 2 |"), vec![Block::Table(vec!["a".into(), "b".into()], vec![vec!["1".into(), "2".into()]])]);
    }

    #[test]
    fn tables_keep_markup_safe_and_bars_inside_code() {
        let b = blocks("| App | Pick |\n|:--|--:|\n| **CLI** | `a|b` and a\\|bar |\n| <x> & | *Rust* |");
        let Block::Table(head, body) = &b[0] else { panic!("{b:?}") };
        assert_eq!(head, &vec!["App".to_string(), "Pick".to_string()]);
        assert_eq!(body.len(), 2);
        assert_eq!(body[0][0], "<b>CLI</b>");
        assert!(body[0][1].contains("a|b") && body[0][1].ends_with("and a|bar"), "{:?}", body[0][1]);
        assert_eq!(body[1][0], "&lt;x&gt; &amp;");
        for m in head.iter().chain(body.iter().flatten()) {
            assert!(valid(m), "{m}");
        }
        // no divider: every row is body
        assert!(matches!(&blocks("| a | b |\n| 1 | 2 |")[0], Block::Table(h, rows) if h.is_empty() && rows.len() == 2));
    }
}
