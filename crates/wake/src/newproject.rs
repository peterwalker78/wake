//! New project (added 2026-11-27): a dialog where the person's own AI (the one Ask uses) asks a
//! few questions about what they want to make, one at a time; then a review of the plan where
//! everything can be changed before anything is made; then `Source::create_project` makes the
//! folder (and a git repository if chosen), writes CLAUDE.md or AGENTS.md, tracks it, and opens a
//! new terminal window with the agent on its first task.
//!
//! The interview runs in the core like a question with no tools (`Source::interview`); this
//! module asks for its state every 200 ms while the AI is writing and draws it with Ask's parts.
//! Closing the dialog stops and deletes the interview.

use std::{
    cell::RefCell,
    rc::Rc,
    sync::Mutex,
    time::{Duration, Instant},
};

use gtk4::{self as gtk, gdk, glib, prelude::*};
use wake_core::{
    ActionResult, Agent, QOverview, QThread, QTurn,
    newproject::{self, Plan},
};

use crate::{
    app::{self, Ui},
    dialogs, w,
};

const POLL: Duration = Duration::from_millis(200);
const WIDTH: i32 = 640;

#[derive(Default)]
struct St {
    thread: Option<String>,
    ov: Option<QOverview>,
    busy: bool,
    polling: bool,
    sig: String,
    /// The AI's message being written: its box and the text it shows.
    streaming: Option<(gtk::Box, String)>,
    /// The plan on the review page (and as edited there, kept for going back to it).
    plan: Option<Plan>,
    /// Which AI message the plan came from: a new plan opens the review once.
    plan_turn: Option<usize>,
    reviewing: bool,
    /// Something was typed or changed: Esc asks first.
    typed: bool,
    esc_once: bool,
    closed: bool,
    /// Keep the conversation scrolled to the end while the reader is there.
    stick: bool,
}

struct Dlg {
    ui: Ui,
    card: gtk::Box,
    talk: gtk::Box,
    ask_part: gtk::Box,
    review_part: gtk::Box,
    input: gtk::TextView,
    err: gtk::Label,
    st: RefCell<St>,
}

type D = Rc<Dlg>;

thread_local! {
    static CUR: RefCell<Option<D>> = const { RefCell::new(None) };
}

fn cur() -> Option<D> {
    CUR.with(|c| c.borrow().clone())
}

/// "+ New project", as Projects shows it beside "+ Track a folder".
pub fn button(ui: &Ui) -> gtk::Button {
    let ui = ui.clone();
    let b = w::button("+ New project", None, &["sm"], move || open(&ui));
    b.set_tooltip_text(Some("Your AI asks a few questions, then Wake makes the folder and opens an agent in it"));
    b
}

/// "+ New project": the dialog, and the interview's first question on its way.
pub fn open(ui: &Ui) {
    let ai = ui.src.interview_ai();
    let card = dialogs::card(WIDTH);
    card.update_property(&[gtk::accessible::Property::Label("New project")]);

    // The interview page.
    let ask_part = w::vbox(12, &[]);
    ask_part.append(&w::wrap("New project", &["h2"]));
    ask_part.append(&w::wrap(&format!("Your AI ({}) asks a few questions about what you'd like to make. You check the plan before anything is made.", ai.label()), &["plain"]));
    let talk = w::vbox(18, &["nptalk"]);
    let talk_scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).min_content_height(260).max_content_height(380).propagate_natural_height(true).child(&talk).build();
    talk_scroll.set_focusable(false);
    ask_part.append(&talk_scroll);
    let input = gtk::TextView::new();
    input.add_css_class("area");
    input.add_css_class("qinput");
    input.set_wrap_mode(gtk::WrapMode::WordChar);
    input.set_accepts_tab(false);
    input.update_property(&[gtk::accessible::Property::Label("Your answer")]);
    let frame = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).propagate_natural_height(true).max_content_height(120).hexpand(true).child(&input).build();
    frame.add_css_class("areaframe");
    ask_part.append(&frame);
    let foot = w::hbox(6, &["qfoot"]);
    let decide = w::button("You decide", None, &["ghost", "sm"], || {
        if let Some(d) = cur() {
            send(&d, newproject::YOU_DECIDE);
        }
    });
    decide.set_tooltip_text(Some("Let your AI choose for you"));
    foot.append(&decide);
    foot.append(&w::spacer());
    for (k, word) in [("Enter", "sends"), ("Shift+Enter", "new line")] {
        for part in k.split('+') {
            foot.append(&w::kbd(part));
        }
        let l = w::label(word, &["hint"]);
        l.set_valign(gtk::Align::Center);
        foot.append(&l);
    }
    ask_part.append(&foot);
    card.append(&ask_part);

    // The review page, filled when the plan comes.
    let review_part = w::vbox(12, &[]);
    review_part.set_visible(false);
    card.append(&review_part);

    let row = w::hbox(8, &["row"]);
    let err = w::wrap("", &["err"]);
    err.set_hexpand(true);
    row.append(&err);
    let ui2 = ui.clone();
    let cancel = w::button("Cancel", Some("Esc"), &["ghost"], move || dialogs::close(&ui2));
    cancel.set_widget_name("np-cancel");
    row.append(&cancel);
    ask_part.append(&row);

    let d: D = Rc::new(Dlg { ui: ui.clone(), card: card.clone(), talk, ask_part, review_part, input: input.clone(), err, st: RefCell::new(St { stick: true, ..St::default() }) });
    CUR.with(|c| *c.borrow_mut() = Some(d.clone()));

    // Enter sends, Shift+Enter is a new line.
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.connect_key_pressed(|_, key, _, mods| {
        if matches!(key, gdk::Key::Return | gdk::Key::KP_Enter) && !mods.intersects(gdk::ModifierType::SHIFT_MASK | gdk::ModifierType::CONTROL_MASK) {
            if let Some(d) = cur() {
                let text = buffer_text(&d.input);
                send(&d, &text);
            }
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    input.add_controller(keys);
    input.buffer().connect_changed(|b| {
        if let Some(d) = cur() {
            let mut s = d.st.borrow_mut();
            s.typed = b.char_count() > 0 || s.reviewing;
            s.esc_once = false;
        }
    });
    {
        // Follow the end of the conversation while the reader is at the end.
        let adj = talk_scroll.vadjustment();
        adj.connect_value_changed(|a| {
            let at_end = a.value() >= a.upper() - a.page_size() - 48.0;
            if let Some(d) = cur()
                && let Ok(mut s) = d.st.try_borrow_mut()
            {
                s.stick = at_end;
            }
        });
        adj.connect_changed(|a| {
            if cur().is_some_and(|d| d.st.try_borrow().is_ok_and(|s| s.stick)) {
                a.set_value(a.upper() - a.page_size());
            }
        });
    }

    let submit: Rc<dyn Fn()> = Rc::new(|| {
        if let Some(d) = cur() {
            if d.st.borrow().reviewing {
                create(&d);
            } else {
                let text = buffer_text(&d.input);
                send(&d, &text);
            }
        }
    });
    dialogs::show(ui, &card, Some(submit));
    {
        let mut st = ui.st.borrow_mut();
        st.modal_esc = Some(Rc::new(on_esc));
        st.modal_closed = Some(Rc::new(closed));
    }
    input.grab_focus();
    render(&d);
    let d2 = d.clone();
    app::bg(
        ui,
        |src| src.interview(None, newproject::OPENER),
        move |_, r| match r {
            Ok(id) => {
                d2.st.borrow_mut().thread = Some(id);
                fetch(&d2);
            }
            Err(e) => d2.err.set_text(&e),
        },
    );
}

fn buffer_text(t: &gtk::TextView) -> String {
    let b = t.buffer();
    b.text(&b.start_iter(), &b.end_iter(), false).trim().to_string()
}

/// Esc: closes, unless something was typed: then the first Esc says so and the second closes.
fn on_esc() -> bool {
    let Some(d) = cur() else { return false };
    let mut s = d.st.borrow_mut();
    if s.typed && !s.esc_once {
        s.esc_once = true;
        d.err.set_text(if s.reviewing { "Press Esc again to close without making the project." } else { "You've started an answer. Press Esc again to close and lose it." });
        return true;
    }
    false
}

/// The dialog has closed: stop and delete the interview.
fn closed() {
    READ.with(|r| *r.borrow_mut() = None);
    let Some(d) = CUR.with(|c| c.borrow_mut().take()) else { return };
    let id = {
        let mut s = d.st.borrow_mut();
        s.closed = true;
        s.thread.take()
    };
    if let Some(id) = id {
        app::bg(
            &d.ui,
            move |src| {
                src.cancel_question(&id);
                let _ = src.delete_thread(&id);
            },
            |_, ()| {},
        );
    }
}

fn answering(ov: &QOverview) -> bool {
    ov.answering.is_some() || !ov.waiting.is_empty()
}

fn fetch(d: &D) {
    let (id, since) = {
        let mut s = d.st.borrow_mut();
        if s.busy || s.closed {
            return;
        }
        let Some(id) = s.thread.clone() else { return };
        s.busy = true;
        (id, s.ov.as_ref().map_or(0, |o| o.seq))
    };
    let (d2, d3) = (d.clone(), d.clone());
    app::bg_or(
        &d.ui,
        move |src| src.interview_state(&id, since),
        move |_, r| {
            d2.st.borrow_mut().busy = false;
            if d2.st.borrow().closed {
                return;
            }
            if let Some(ov) = r {
                d2.st.borrow_mut().ov = Some(ov);
                render(&d2);
            }
            if d2.st.borrow().ov.as_ref().is_some_and(answering) {
                poll(&d2);
            }
        },
        move |_| {
            d3.st.borrow_mut().busy = false;
            d3.err.set_text("Wake hit an error reading the interview. Please try again.");
        },
    );
}

fn poll(d: &D) {
    if std::mem::replace(&mut d.st.borrow_mut().polling, true) {
        return;
    }
    let d = d.clone();
    glib::timeout_add_local(POLL, move || {
        fetch(&d);
        let s = d.st.borrow();
        if s.closed || !(s.busy || s.ov.as_ref().is_some_and(answering)) {
            drop(s);
            d.st.borrow_mut().polling = false;
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    });
}

/// The person's answer (or "You decide."), sent once the AI has finished writing.
fn send(d: &D, text: &str) {
    let text = text.trim().to_string();
    if text.is_empty() {
        return;
    }
    let (id, busy) = {
        let s = d.st.borrow();
        (s.thread.clone(), s.ov.as_ref().is_none_or(answering))
    };
    let Some(id) = id else { return };
    if busy {
        d.err.set_text("Your AI is still writing. Send your answer when it has finished.");
        return;
    }
    d.err.set_text("");
    let d2 = d.clone();
    app::bg(
        &d.ui,
        move |src| src.interview(Some(&id), &text),
        move |_, r| match r {
            Ok(_) => {
                d2.input.buffer().set_text("");
                {
                    let mut s = d2.st.borrow_mut();
                    s.typed = false;
                    s.stick = true;
                }
                fetch(&d2);
            }
            Err(e) => d2.err.set_text(&e),
        },
    );
}

fn retry(d: &D) {
    let Some(id) = d.st.borrow().thread.clone() else { return };
    d.err.set_text("");
    let d2 = d.clone();
    app::bg(
        &d.ui,
        move |src| src.retry_question(&id),
        move |_, r| match r {
            Ok(()) => fetch(&d2),
            Err(e) => d2.err.set_text(&e),
        },
    );
}

/// The plan in the AI's last message, once it has finished writing it.
fn plan_of(ov: &QOverview) -> Option<(usize, Plan)> {
    let t = ov.threads.first()?;
    let n = t.turns.len().checked_sub(1)?;
    let last = &t.turns[n];
    if answering(ov) || last.answered_at.is_none() {
        return None;
    }
    newproject::parse_plan(&last.answer).map(|p| (n, p))
}

/// Draws the conversation (only the message being written, when only that changed), and opens
/// the review when a new plan has come.
fn render(d: &D) {
    let ov = d.st.borrow().ov.clone().unwrap_or_default();
    let t: Option<QThread> = ov.threads.first().cloned();
    let busy = answering(&ov);
    let turns: Vec<QTurn> = t.as_ref().map(|t| t.turns.clone()).unwrap_or_default();
    let n = turns.len();
    let shape: Vec<(&str, bool, Option<&str>, &str)> =
        turns.iter().enumerate().map(|(i, x)| (x.question.as_str(), x.answered_at.is_some(), x.error.as_deref(), if i + 1 == n && busy { "" } else { x.answer.as_str() })).collect();
    let sig = format!("{shape:?}{busy}{:?}", ov.activity);
    let last_answer = turns.last().map(|x| newproject::without_plan(&x.answer)).unwrap_or_default();
    if d.st.borrow().sig == sig && !sig.is_empty() {
        let streaming = d.st.borrow().streaming.clone();
        if let Some((bx, shown)) = streaming
            && shown != last_answer
        {
            crate::questions::fill_answer(&bx, &last_answer);
            d.st.borrow_mut().streaming = Some((bx, last_answer));
        }
        return;
    }
    {
        let mut s = d.st.borrow_mut();
        s.sig = sig;
        s.streaming = None;
    }
    w::clear(&d.talk);
    if turns.is_empty() {
        d.talk.append(&thinking(None));
    }
    let plan = plan_of(&ov);
    for (i, turn) in turns.iter().enumerate() {
        let last = i + 1 == n;
        // The first message is Wake's opener: only the answers are the person's.
        if i > 0 {
            let q = w::wrap(&turn.question, &["qq"]);
            q.set_selectable(true);
            q.set_focusable(false);
            d.talk.append(&q);
        }
        let ab = w::vbox(10, &["qa"]);
        let text = newproject::without_plan(&turn.answer);
        crate::questions::fill_answer(&ab, &text);
        d.talk.append(&ab);
        if last && busy {
            d.st.borrow_mut().streaming = Some((ab.clone(), text));
            d.talk.append(&thinking(ov.activity.as_deref().or(Some(if turn.answer.is_empty() { "thinking" } else { "writing" }))));
        } else if let Some(e) = &turn.error {
            let l = w::hbox(8, &["qerrline"]);
            l.append(&w::wrap(e, &["qerr"]));
            if last {
                let d2 = d.clone();
                l.append(&w::button("Try again", None, &["sm"], move || retry(&d2)));
            }
            d.talk.append(&l);
        } else if last && let Some((_, p)) = &plan {
            // Back from the review: the way to it again.
            let l = w::hbox(8, &["qerrline"]);
            let d2 = d.clone();
            let p = p.clone();
            l.append(&w::button("Check the plan", None, &["sm"], move || {
                let kept = d2.st.borrow().plan.clone();
                review(&d2, &kept.unwrap_or_else(|| p.clone()));
            }));
            d.talk.append(&l);
        }
    }
    if let Some((turn, p)) = plan {
        let new = d.st.borrow().plan_turn != Some(turn);
        if new {
            {
                let mut s = d.st.borrow_mut();
                s.plan_turn = Some(turn);
                s.plan = None;
            }
            review(d, &p);
        }
    }
}

fn thinking(what: Option<&str>) -> gtk::Box {
    let l = w::hbox(8, &["qstate"]);
    l.append(&w::dot("working"));
    l.append(&w::label(&format!("{}…", what.unwrap_or("thinking")), &[]));
    l
}

/// A labelled part of the review.
fn field(label: &str, child: &impl IsA<gtk::Widget>, hint: Option<&gtk::Label>) -> gtk::Box {
    let b = w::vbox(6, &[]);
    b.append(&w::label(label, &["flabel"]));
    b.append(child);
    if let Some(h) = hint {
        b.append(h);
    }
    b
}

/// The review: every part of the plan, editable, then "Create and open".
fn review(d: &D, plan: &Plan) {
    let ui = d.ui.clone();
    let choices: Vec<Agent> = ui.src.agent_choices();
    let rp = &d.review_part;
    w::clear(rp);
    {
        let mut s = d.st.borrow_mut();
        s.reviewing = true;
        s.esc_once = false;
        s.typed = true;
        s.plan = Some(plan.clone());
    }
    d.err.set_text("");
    rp.append(&w::wrap("Check the plan", &["h2"]));
    let mut about = plan.summary.trim().to_string();
    if plan.technical && !plan.tech.is_empty() {
        about.push_str(&format!("{}Tech: {}.", if about.is_empty() { "" } else { " " }, plan.tech.join(", ")));
    }
    if !about.is_empty() {
        rp.append(&w::wrap(&about, &["plain"]));
    }
    rp.append(&w::wrap("Change anything you like. Nothing is made until you press Create and open.", &["hint"]));

    let name = gtk::Entry::builder().text(&plan.name).hexpand(true).build();
    name.update_property(&[gtk::accessible::Property::Label("Name")]);
    let folder = gtk::Entry::builder().text(&plan.folder_name).hexpand(true).max_length(newproject::MAX_SLUG as i32).build();
    folder.add_css_class("mono");
    folder.update_property(&[gtk::accessible::Property::Label("Folder name")]);
    let check = w::wrap("", &["hint"]);
    let names = w::hbox(14, &["agentrow"]);
    names.append(&field("NAME", &name, None));
    let fb = field("FOLDER", &folder, Some(&check));
    fb.set_hexpand(true);
    names.append(&fb);
    names.first_child().inspect(|c| c.set_hexpand(true));
    rp.append(&names);

    // The folder: checked as it is typed ("already exists", or where it will be).
    let check_now = {
        let (ui, check) = (ui.clone(), check.clone());
        move |slug: &str| match ui.src.new_project_folder(slug) {
            Ok(p) => {
                check.set_text(&format!("Wake makes {p}"));
                check.remove_css_class("err");
                check.add_css_class("hint");
            }
            Err(e) => {
                check.set_text(&e);
                check.remove_css_class("hint");
                check.add_css_class("err");
            }
        }
    };
    check_now(&plan.folder_name);
    let folder_typed = Rc::new(std::cell::Cell::new(false));
    {
        let check_now = check_now.clone();
        let ft = folder_typed.clone();
        folder.connect_changed(move |e| {
            if e.has_focus() {
                ft.set(true);
            }
            check_now(e.text().trim());
        });
    }
    {
        // Until the folder is typed in, it follows the name.
        let (folder, ft) = (folder.clone(), folder_typed.clone());
        name.connect_changed(move |e| {
            if !ft.get() {
                let s = newproject::slug(&e.text());
                folder.set_text(if s.is_empty() { "new-project" } else { &s });
            }
        });
    }

    let git_row = w::hbox(12, &[]);
    let git_words = w::vbox(2, &[]);
    git_words.set_hexpand(true);
    git_words.append(&w::label("Track changes with git", &["heading"]));
    git_words.append(&w::wrap("Keeps a history of every change, so you can go back to an earlier version.", &["hint"]));
    git_row.append(&git_words);
    let git = gtk::Switch::new();
    git.set_active(plan.git);
    git.set_valign(gtk::Align::Center);
    git.update_property(&[gtk::accessible::Property::Label("Track changes with git")]);
    git_row.append(&git);
    rp.append(&git_row);

    let agent_dd = gtk::DropDown::from_strings(&choices.iter().map(|c| c.label()).collect::<Vec<_>>());
    agent_dd.add_css_class("mini");
    agent_dd.set_halign(gtk::Align::Start);
    agent_dd.update_property(&[gtk::accessible::Property::Label("Agent")]);
    let agent_hint = w::wrap("", &["hint"]);
    let agent_box = if choices.len() > 1 {
        field("THE AGENT IN THE NEW WINDOW", &agent_dd, Some(&agent_hint))
    } else {
        field("THE AGENT IN THE NEW WINDOW", &w::label(choices.first().map_or("Claude", |c| c.label()), &["plain"]), Some(&agent_hint))
    };
    rp.append(&agent_box);

    let desc = dialogs::text_area(&plan.description, 120);
    desc.add_css_class("code");
    desc.update_property(&[gtk::accessible::Property::Label("What the agent reads first")]);
    let desc_label = w::label("", &["flabel"]);
    let db = w::vbox(6, &[]);
    db.append(&desc_label);
    db.append(&dialogs::framed(&desc, 170));
    rp.append(&db);
    let task = dialogs::text_area(&plan.first_task, 60);
    task.update_property(&[gtk::accessible::Property::Label("Its first task")]);
    rp.append(&field(
        "ITS FIRST TASK",
        &dialogs::framed(&task, 72),
        Some(&w::wrap("Sent as the agent's first message. It sets up the rest, explaining as it goes, and asks before installing anything.", &["hint"])),
    ));
    let show_agent = {
        let (desc_label, agent_hint, choices) = (desc_label.clone(), agent_hint.clone(), choices.clone());
        move |at: u32| {
            let a = choices.get(at as usize).copied().unwrap_or_default();
            let file = newproject::file_name(a);
            desc_label.set_text(&format!("WHAT THE AGENT READS FIRST ({file})"));
            agent_hint.set_text(&format!("A new terminal window opens in the folder with {} on its first task.", a.label()));
        }
    };
    show_agent(0);
    agent_dd.connect_selected_notify(move |dd| show_agent(dd.selected()));

    let row = w::hbox(8, &["row"]);
    row.set_widget_name("np-review-row");
    let d2 = d.clone();
    row.append(&w::button("Back to the questions", None, &["ghost"], move || back(&d2)));
    let d2 = d.clone();
    let go = w::button("Create and open", Some("Ctrl+Enter"), &["primary"], move || create(&d2));
    go.set_widget_name("np-create");
    row.append(&go);
    rp.append(&row);

    // What `create` reads.
    let read: ReadPlan = {
        let (name, folder, git, agent_dd, desc, task, plan) = (name.clone(), folder.clone(), git.clone(), agent_dd.clone(), desc.clone(), task.clone(), plan.clone());
        Rc::new(move || {
            let agent = choices.get(agent_dd.selected() as usize).copied().unwrap_or_default();
            let p = Plan {
                name: name.text().trim().to_string(),
                folder_name: folder.text().trim().to_string(),
                git: git.is_active(),
                description: buffer_text(&desc),
                first_task: buffer_text(&task),
                ..plan.clone()
            };
            (p, agent)
        })
    };
    READ.with(|r| *r.borrow_mut() = Some(read));
    // The err line moves to the review's buttons.
    if let Some(p) = d.err.parent().and_downcast::<gtk::Box>() {
        p.remove(&d.err);
    }
    row.prepend(&d.err);
    d.ask_part.set_visible(false);
    rp.set_visible(true);
    crate::motion::pop(&d.card);
    name.grab_focus();
    name.set_position(-1);
}

/// Reads the review as it stands: the plan as edited, and the agent picked.
type ReadPlan = Rc<dyn Fn() -> (Plan, Agent)>;

thread_local! {
    static READ: RefCell<Option<ReadPlan>> = const { RefCell::new(None) };
}

/// Back to the questions, keeping what was changed on the review for "Check the plan".
fn back(d: &D) {
    if let Some(read) = READ.with(|r| r.borrow().clone()) {
        d.st.borrow_mut().plan = Some(read().0);
    }
    {
        let mut s = d.st.borrow_mut();
        s.reviewing = false;
        s.esc_once = false;
        s.typed = d.input.buffer().char_count() > 0;
        s.sig.clear();
    }
    d.err.set_text("");
    if let Some(p) = d.err.parent().and_downcast::<gtk::Box>() {
        p.remove(&d.err);
    }
    // Under the interview: the err line goes back before Cancel.
    if let Some(cancel) = w::named(&d.ask_part, "np-cancel")
        && let Some(row) = cancel.parent().and_downcast::<gtk::Box>()
    {
        row.prepend(&d.err);
    }
    d.review_part.set_visible(false);
    d.ask_part.set_visible(true);
    render(d);
    d.input.grab_focus();
}

fn create(d: &D) {
    let Some(read) = READ.with(|r| r.borrow().clone()) else { return };
    let (plan, agent) = read();
    if plan.name.is_empty() {
        d.err.set_text("Give the project a name.");
        return;
    }
    if let Err(e) = d.ui.src.new_project_folder(&plan.folder_name) {
        d.err.set_text(&e);
        return;
    }
    if std::mem::replace(&mut d.st.borrow_mut().busy, true) {
        return;
    }
    d.err.set_text("Making the project…");
    let (d2, d3) = (d.clone(), d.clone());
    app::bg_or(
        &d.ui,
        move |src| src.create_project(&plan, agent, false),
        move |ui, r: ActionResult| {
            d2.st.borrow_mut().busy = false;
            if !r.ok {
                d2.err.set_text(&r.message);
                return;
            }
            dialogs::close(ui);
            app::toast(ui, &r.message, false);
            app::refresh(ui);
        },
        move |_| {
            d3.st.borrow_mut().busy = false;
            d3.err.set_text("Wake hit an error making the project. Please try again.");
        },
    );
}

// ---------- screenshots and tests ----------

/// Answers the demo interview gives, in order (`--show newproject`).
const SCRIPTED: [&str; 5] = ["A website with our family's recipes, for my parents and my kids.", "Something technical, I think: a website.", "You decide.", "Yes please.", "Family Recipes"];

/// `--show newproject[:review]`: opens the dialog and answers with the scripted words, stopping
/// mid-conversation (two answers in, the next question on screen) or at the review.
pub fn show(ui: &Ui, review_too: bool) {
    open(ui);
    let stop_at = if review_too { SCRIPTED.len() } else { 2 };
    let sent = std::cell::Cell::new(0usize);
    let quiet = std::cell::Cell::new(0u32);
    glib::timeout_add_local(Duration::from_millis(250), move || {
        let Some(d) = cur() else { return glib::ControlFlow::Break };
        let (ready, reviewing) = {
            let s = d.st.borrow();
            (s.ov.as_ref().is_some_and(|o| !answering(o) && o.threads.first().is_some_and(|t| t.turns.last().is_some_and(|x| x.answered_at.is_some()))), s.reviewing)
        };
        if reviewing || sent.get() >= stop_at {
            return glib::ControlFlow::Break;
        }
        if !ready {
            return glib::ControlFlow::Continue;
        }
        // A beat on each question, as a person would take.
        quiet.set(quiet.get() + 1);
        if quiet.get() < 2 {
            return glib::ControlFlow::Continue;
        }
        quiet.set(0);
        send(&d, SCRIPTED[sent.get()]);
        sent.set(sent.get() + 1);
        glib::ControlFlow::Continue
    });
}

// ---------- demo ----------

/// The AI's side of the demo interview: questions, then the plan. Nothing is sent anywhere.
const DEMO_SAYS: [&str; 6] = [
    "Hello! Let's get your new project started. What would you like to make, and who is it for?",
    "That sounds lovely. Is it a technical project, like an app, a website or a script? Or something else, like writing, planning or research?",
    "Which tech would you like to use? If you're not sure, say \"you decide\" and I'll suggest something simple.",
    "I'll use plain **HTML and CSS**: a web page made of two kinds of text file, nothing to install, and it opens in any browser.\n\nShall I track changes with git? It keeps a history of every change, so you can always go back to an earlier version. I'd suggest yes.",
    "Last one: what should we call it?",
    "Here's the plan: **Family Recipes**, a simple website where your family's recipes live in one place, easy for your parents and kids to read on a phone or a computer. It's plain HTML and CSS, tracked with git.\n\n```json\n{\"name\": \"Family Recipes\", \"folder_name\": \"family-recipes\", \"summary\": \"A simple website with the family's recipes, easy for parents and kids to read on a phone or a computer.\", \"technical\": true, \"tech\": [\"HTML\", \"CSS\"], \"git\": true, \"description\": \"# Family Recipes\\n\\nA simple website with our family's recipes in one place, for my parents and my kids. It should be easy to read on a phone or a computer.\\n\\n## Tech\\n\\n- Plain HTML and CSS, no frameworks and nothing to install.\\n- One page per recipe, and an index page that lists them.\\n\\n## Working with me\\n\\n- I'm new to this: explain things in plain words and keep steps small.\\n- Ask before installing anything, spending money, or doing anything that can't be undone.\", \"first_task\": \"Set up the starting files: an index page, one example recipe page and a simple stylesheet. Explain each file in plain words as you make it, and ask me before installing anything. Then show me how to open it in a browser and suggest what to do next.\"}\n```",
];

/// Characters the demo writes per second.
const DEMO_SPEED: f64 = 900.0;

struct Talk {
    id: String,
    /// Each turn: what the person said, and when the AI started its reply.
    turns: Vec<(String, Instant)>,
    stopped: bool,
}

/// The demo interview: replies from [`DEMO_SAYS`], written out over time like a real one.
#[derive(Default)]
pub struct Demo {
    talk: Mutex<Option<Talk>>,
}

impl Demo {
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Talk>> {
        self.talk.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn interview(&self, thread: Option<&str>, answer: &str) -> Result<String, String> {
        let mut t = self.lock();
        match thread {
            None => {
                let id = format!("qnp{}", glib::monotonic_time());
                *t = Some(Talk { id: id.clone(), turns: vec![(answer.to_string(), Instant::now())], stopped: false });
                Ok(id)
            }
            Some(id) => {
                let talk = t.as_mut().filter(|x| x.id == id).ok_or("That interview is gone.")?;
                talk.turns.push((answer.to_string(), Instant::now()));
                talk.stopped = false;
                Ok(id.to_string())
            }
        }
    }

    pub fn state(&self, thread: &str, since: u64) -> Option<QOverview> {
        let t = self.lock();
        let talk = t.as_ref().filter(|x| x.id == thread)?;
        let n = talk.turns.len();
        let mut turns = Vec::new();
        let mut writing = false;
        let mut seq = n as u64;
        for (i, (said, at)) in talk.turns.iter().enumerate() {
            let full = DEMO_SAYS[i.min(DEMO_SAYS.len() - 1)];
            let total = full.chars().count();
            let shown = if talk.stopped && i + 1 == n { 0 } else { ((at.elapsed().as_secs_f64() * DEMO_SPEED) as usize).min(total) };
            let done = shown == total;
            writing |= !done && !(talk.stopped && i + 1 == n);
            seq = seq.wrapping_mul(7919).wrapping_add(shown as u64);
            turns.push(QTurn {
                question: said.clone(),
                answer: full.chars().take(shown).collect(),
                asked_at: String::new(),
                answered_at: done.then(String::new),
                error: (talk.stopped && i + 1 == n && !done).then(|| "Stopped.".to_string()),
            });
        }
        let seq = seq.max(1);
        if since == seq {
            return None;
        }
        Some(QOverview {
            threads: vec![QThread { id: talk.id.clone(), title: newproject::TITLE.into(), turns, kind: Some(newproject::KIND.into()), ..QThread::default() }],
            answering: writing.then(|| talk.id.clone()),
            seq,
            ..QOverview::default()
        })
    }

    /// True when it was this interview (stopped, or dropped with `delete`).
    pub fn stop(&self, thread: &str, delete: bool) -> bool {
        let mut t = self.lock();
        if !t.as_ref().is_some_and(|x| x.id == thread) {
            return false;
        }
        if delete {
            *t = None;
        } else if let Some(x) = t.as_mut() {
            x.stopped = true;
        }
        true
    }

    pub fn retry(&self, thread: &str) -> bool {
        let mut t = self.lock();
        let Some(x) = t.as_mut().filter(|x| x.id == thread) else { return false };
        x.stopped = false;
        if let Some(last) = x.turns.last_mut() {
            last.1 = Instant::now();
        }
        true
    }
}

/// The demo's "make it": checks as the real one does, makes nothing.
pub fn demo_create(plan: &Plan, agent: Agent, taken: &[String]) -> ActionResult {
    let fail = |m: &str| ActionResult { ok: false, message: m.into(), steps: vec![] };
    if let Err(e) = demo_folder(&plan.folder_name, taken) {
        return fail(&e);
    }
    if !agent.can_brief() {
        return fail("That agent isn't installed here.");
    }
    if plan.description.trim().is_empty() || plan.first_task.trim().is_empty() {
        return fail("The description and the first task can't be empty.");
    }
    ActionResult { ok: true, message: format!("Demo: nothing was made. Wake would make ~/Projects/{} and start {} there in a new terminal window.", plan.folder_name, agent.label()), steps: vec![] }
}

/// The demo's folder check: the made-up projects' folders are taken.
pub fn demo_folder(folder: &str, taken: &[String]) -> Result<String, String> {
    newproject::check_folder(std::path::Path::new("/nonexistent-wake-demo"), folder)?;
    if taken.iter().any(|t| newproject::slug(t) == folder) {
        return Err("A folder with that name already exists. Pick another name.".into());
    }
    Ok(format!("~/Projects/{folder}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_demo_interview_ends_in_a_plan_and_makes_nothing() {
        let d = Demo::default();
        let id = d.interview(None, newproject::OPENER).unwrap();
        for a in SCRIPTED {
            d.interview(Some(&id), a).unwrap();
        }
        // Every reply written out at once (as if time had passed).
        if let Some(t) = d.lock().as_mut() {
            for turn in t.turns.iter_mut() {
                turn.1 = Instant::now() - Duration::from_secs(60);
            }
        }
        let ov = d.state(&id, 0).unwrap();
        assert!(ov.answering.is_none());
        let (_, plan) = plan_of(&ov).unwrap();
        assert_eq!((plan.name.as_str(), plan.folder_name.as_str(), plan.git), ("Family Recipes", "family-recipes", true));
        assert!(newproject::without_plan(&ov.threads[0].turns[5].answer).starts_with("Here's the plan"));
        let r = demo_create(&plan, Agent::Claude, &["Gradebook".into()]);
        assert!(r.ok && r.message.starts_with("Demo: nothing was made."), "{r:?}");
        assert!(!demo_create(&Plan { folder_name: "gradebook".into(), ..plan.clone() }, Agent::Claude, &["Gradebook".into()]).ok);
        assert!(!demo_create(&Plan { folder_name: "Bad".into(), ..plan }, Agent::Claude, &[]).ok);
        assert!(d.stop(&id, true) && d.state(&id, 0).is_none());
    }
}
