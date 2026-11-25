//! The switcher (Ctrl+K or g): filter sessions as you type, Enter goes there, → looks inside at
//! the subagents, → again opens one's steps and what it handed back, ← comes back out. "Back to
//! where you were" heads the list.

use gtk4::{self as gtk, gdk, glib, prelude::*};
use wake_core::*;

use crate::{
    app::{self, Ui},
    fmt, panel, w,
};

pub fn widgets() -> (gtk::Box, gtk::Entry, gtk::Box, gtk::Box, gtk::ScrolledWindow) {
    let scrim = w::vbox(0, &["scrim"]);
    scrim.set_visible(false);
    let pal = gtk::Box::builder().orientation(gtk::Orientation::Vertical).accessible_role(gtk::AccessibleRole::Dialog).build();
    pal.add_css_class("pal");
    pal.update_property(&[gtk::accessible::Property::Label("Go to a session")]);

    let top = w::hbox(12, &["pal-in"]);
    top.append(&w::label("GO TO", &["mark"]));
    let entry = gtk::Entry::builder().placeholder_text("Type a project, a title or a tab…").hexpand(true).build();
    entry.update_property(&[gtk::accessible::Property::Label("Filter sessions")]);
    entry.add_css_class("bare");
    top.append(&entry);
    pal.append(&top);
    let cols = w::hbox(0, &["cols2"]);
    let list = w::vbox(0, &["swlist"]);
    let list_scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).child(&list).min_content_height(420).max_content_height(420).build();
    list_scroll.set_size_request(-1, 420);
    list_scroll.add_css_class("swlist-scroll");
    // 480 px when there is room, narrowing to 240 beside what's inside in a narrow window.
    cols.append(&w::Fit::new(&list_scroll, 240, Some(480)).upcast());
    let inside = w::vbox(0, &["swinside"]);
    let inside_scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).child(&inside).min_content_height(420).hexpand(true).build();
    cols.append(&inside_scroll);
    pal.append(&cols);
    // The keys: in a narrow window the less needed ones drop out.
    let ft = crate::layout::Squeeze::new(16, &["pal-ft"]);
    for (prio, (keys, word)) in [(&["↑", "↓"][..], "choose"), (&["Enter"], "go to"), (&["→"], "look inside"), (&["←"], "back"), (&["Esc"], "close")].into_iter().enumerate() {
        let b = w::hbox(4, &[]);
        for k in keys {
            b.append(&w::kbd(k));
        }
        b.append(&w::label(word, &[]));
        ft.add(&b, [2, 0, 3, 4, 1][prio]);
    }
    pal.append(&ft);
    // 900 px wide when there is room, else the window's width less a margin each side.
    let fit = w::Fit::new(&pal, 0, Some(900)).upcast();
    fit.set_halign(gtk::Align::Center);
    fit.set_valign(gtk::Align::Start);
    fit.set_margin_top(70);
    fit.set_margin_start(16);
    fit.set_margin_end(16);
    scrim.append(&fit);
    (scrim, entry, list, inside, list_scroll)
}

/// The rows: "back" first when nothing is typed, then matching sessions, most urgent first.
#[derive(Clone)]
enum Item {
    Back(SessionSummary),
    Session(SessionSummary),
}

impl Item {
    fn id(&self) -> String {
        match self {
            Item::Back(_) => "back".into(),
            Item::Session(s) => s.id.clone(),
        }
    }
    fn session(&self) -> &SessionSummary {
        match self {
            Item::Back(s) | Item::Session(s) => s,
        }
    }
}

fn items(ui: &Ui) -> Vec<Item> {
    let unsure = app::unsure(ui);
    let st = ui.st.borrow();
    let Some(ov) = &st.ov else { return Vec::new() };
    let terms: Vec<String> = st.sw.q.to_lowercase().split_whitespace().map(String::from).collect();
    let mut out = Vec::new();
    if terms.is_empty()
        && let Some(back) = st.back.as_ref().and_then(|id| ov.sessions.iter().find(|s| &s.id == id))
    {
        out.push(Item::Back(back.clone()));
    }
    let mut list: Vec<&SessionSummary> = ov
        .sessions
        .iter()
        .filter(|s| {
            if terms.is_empty() {
                return true;
            }
            let (loc, ..) = app::loc_of(s, unsure);
            let hay = [
                s.project_name.clone().unwrap_or_default(),
                s.title.clone(),
                format!("{:?}", s.agent),
                loc,
                s.location.as_ref().map(|l| l.tab_title.clone()).unwrap_or_default(),
                s.last_prompt.clone().unwrap_or_default(),
                w::state_word(s.state).1.to_string(),
            ]
            .join(" ")
            .to_lowercase();
            terms.iter().all(|t| hay.contains(t))
        })
        .collect();
    list.sort_by(|a, b| crate::views::by_session(a, b));
    out.extend(list.into_iter().map(|s| Item::Session(s.clone())));
    out
}

fn current(ui: &Ui) -> (Vec<Item>, usize) {
    let it = items(ui);
    let sel = ui.st.borrow().sw.sel_id.clone();
    let i = it.iter().position(|x| Some(x.id()) == sel).unwrap_or(0);
    if let Some(x) = it.get(i) {
        ui.st.borrow_mut().sw.sel_id = Some(x.id());
    }
    (it, i)
}

/// The session the switcher's highlighted row is (for hold-to-peek).
pub fn selected_session(ui: &Ui) -> Option<String> {
    let (it, i) = current(ui);
    it.get(i).map(|x| x.session().id.clone())
}

pub fn open(ui: &Ui) {
    if ui.st.borrow().sw.open {
        ui.w.sw_entry.grab_focus();
        return;
    }
    // Esc puts the keyboard back where it was (the panel, the rail...), not always on the list.
    RETURN_TO.with(|r| *r.borrow_mut() = gtk::prelude::GtkWindowExt::focus(&ui.window));
    ui.st.borrow_mut().sw = app::Sw { open: true, ..Default::default() };
    ui.w.sw_entry.set_text("");
    ui.w.switcher.set_visible(true);
    // It fades and grows in a little; the keyboard is already in it.
    crate::motion::pop(&ui.w.switcher);
    if let Some(pal) = ui.w.switcher.first_child().and_then(|f| f.first_child()) {
        crate::motion::pop(&pal);
    }
    // Nothing behind the switcher can take the keyboard while it is open.
    ui.w.column.set_sensitive(false);
    ui.w.sw_entry.grab_focus();
    // Wired once: typing filters, clicking the scrim closes.
    if !ui.w.sw_entry.has_css_class("wired") {
        ui.w.sw_entry.add_css_class("wired");
        let ui2 = ui.clone();
        ui.w.sw_entry.connect_changed(move |e| {
            {
                // Fired from inside our own set_text too: never panic on a busy state.
                let Ok(mut st) = ui2.st.try_borrow_mut() else { return };
                if !st.sw.open {
                    return;
                }
                st.sw.q = e.text().to_string();
                st.sw.sel_id = None;
                st.sw.in_subs = false;
                st.sw.expanded = None;
            }
            render(&ui2);
        });
        let click = gtk::GestureClick::new();
        let ui2 = ui.clone();
        click.connect_pressed(move |g, _, x, y| {
            let scrim = &ui2.w.switcher;
            if scrim.pick(x, y, gtk::PickFlags::DEFAULT).is_some_and(|p| p == *scrim.upcast_ref::<gtk::Widget>()) {
                g.set_state(gtk::EventSequenceState::Claimed);
                close(&ui2);
            }
        });
        ui.w.switcher.add_controller(click);
    }
    render(ui);
}

pub fn close(ui: &Ui) {
    ui.st.borrow_mut().sw.open = false;
    ui.w.switcher.set_visible(false);
    ui.w.column.set_sensitive(true);
    match RETURN_TO.with(|r| r.borrow_mut().take()) {
        Some(w) if w.is_mapped() && w.is_sensitive() && !w.is_ancestor(&ui.w.switcher) => {
            w.grab_focus();
        }
        _ => app::focus_list(ui),
    }
}

thread_local! {
    static RETURN_TO: std::cell::RefCell<Option<gtk::Widget>> = const { std::cell::RefCell::new(None) };
}

/// For screenshots: select `id` and look inside its first subagent.
pub fn show_subs_of(ui: &Ui, id: Option<String>) {
    let Some(id) = id else { return };
    ui.st.borrow_mut().sw.sel_id = Some(id.clone());
    render(ui);
    let ui = ui.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || {
        let subs = subs(&ui);
        if let Some(a) = subs.iter().find(|a| a.state == State::Ended).or(subs.first()) {
            let n = subs.iter().position(|x| x.agent_id == a.agent_id).unwrap_or(0);
            ui.st.borrow_mut().sw.in_subs = true;
            toggle_sub(&ui, n);
        }
    });
}

/// The rows, every one-line label ready to shorten, so a narrow switcher cuts nothing off.
pub fn render(ui: &Ui) {
    render_rows(ui);
    w::shrinkable(ui.w.sw_list.upcast_ref());
    columns(ui);
}

/// A narrow window shows one column at a time, as a phone does: the sessions, or (after →) what is
/// inside the chosen one; ← comes back.
fn columns(ui: &Ui) {
    let (narrow, inside) = {
        let st = ui.st.borrow();
        (st.lay.narrow, st.sw.in_subs)
    };
    if let Some(list) = ui.w.sw_scroll.parent() {
        list.set_visible(!narrow || !inside);
        list.set_hexpand(narrow);
    }
    if let Some(sc) = ui.w.sw_inside.ancestor(gtk::ScrolledWindow::static_type()) {
        sc.set_visible(!narrow || inside);
    }
}

fn render_rows(ui: &Ui) {
    if !ui.st.borrow().sw.open {
        return;
    }
    let (it, i) = current(ui);
    let list = &ui.w.sw_list;
    w::clear(list);
    if it.is_empty() {
        let has = ui.st.borrow().ov.as_ref().is_some_and(|ov| !ov.sessions.is_empty());
        let q = ui.st.borrow().sw.q.clone();
        list.append(&w::wrap(
            &if has { format!("Nothing matches “{q}”. Try part of a project or tab name.") } else { "No sessions yet. Start Claude in a project and it appears here.".into() },
            &["none", "pad"],
        ));
    }
    let mut last_group = "";
    let mut sel_widget = None;
    for (n, x) in it.iter().enumerate() {
        let s = x.session();
        let group = match x {
            Item::Back(_) => "Just now",
            Item::Session(s) if s.location.is_some() || matches!(s.state, State::Working | State::Waiting | State::Idle) => "Open",
            _ => "Not open · Enter reopens",
        };
        if group != last_group {
            list.append(&w::label(&group.to_uppercase(), &["pgrp"]));
            last_group = group;
        }
        let row = w::vbox(2, &["prow"]);
        if n == i {
            row.add_css_class("sel");
            sel_widget = Some(row.clone());
        }
        let line = w::hbox(10, &[]);
        let (class, word) = w::state_word(s.state);
        let t = w::hbox(0, &[]);
        t.set_hexpand(true);
        let text = match x {
            Item::Back(_) => {
                line.append(&w::dot("back"));
                format!(
                    "<b>Back to where you were</b> <span foreground=\"{}\">· {} · {}</span>",
                    crate::theme::hex("muted"),
                    w::esc(s.project_name.as_deref().unwrap_or("no project")),
                    w::esc(&s.title)
                )
            }
            Item::Session(_) => {
                line.append(&w::dot(class));
                format!("<b>{}</b> <span foreground=\"{}\">· {}</span>", w::esc(s.project_name.as_deref().unwrap_or("no project")), crate::theme::hex("muted"), w::esc(&s.title))
            }
        };
        let l = w::clip("", &["t"]);
        l.set_markup(&text);
        t.append(&l);
        if let Item::Session(_) = x {
            t.append(&w::label(word, &["st", "state", class]));
        }
        line.append(&t);
        let (loc, lclass, ..) = app::loc_for(ui, s);
        line.append(&w::label(&loc, &["loc", if lclass.is_empty() { "plain" } else { lclass }]));
        row.append(&line);
        if let (Item::Session(_), State::Waiting, Some(q)) = (x, s.state, &s.question) {
            let q = w::clip(&format!("asks: {q}"), &["q"]);
            row.append(&q);
        }
        let click = gtk::GestureClick::new();
        let ui2 = ui.clone();
        let id = x.id();
        click.connect_released(move |_, _, _, _| {
            let already = ui2.st.borrow().sw.sel_id.as_deref() == Some(&id);
            if already {
                choose(&ui2);
            } else {
                {
                    let mut st = ui2.st.borrow_mut();
                    st.sw.sel_id = Some(id.clone());
                    st.sw.in_subs = false;
                    st.sw.expanded = None;
                }
                render(&ui2);
            }
            ui2.w.sw_entry.grab_focus();
        });
        row.add_controller(click);
        list.append(&row);
    }
    if let Some(row) = sel_widget {
        let scroll = ui.w.sw_scroll.clone();
        glib::idle_add_local_once(move || {
            if let Some(p) = row.compute_point(&scroll, &gtk::graphene::Point::new(0.0, 0.0)) {
                let adj = scroll.vadjustment();
                let (y, h) = (p.y() as f64 + adj.value(), row.height() as f64);
                if y < adj.value() {
                    adj.set_value(y);
                } else if y + h > adj.value() + adj.page_size() {
                    adj.set_value(y + h - adj.page_size());
                }
            }
        });
    }
    render_inside(ui);
}

fn subs(ui: &Ui) -> Vec<SubagentSummary> {
    let (it, i) = current(ui);
    let Some(Item::Session(s)) = it.get(i) else { return Vec::new() };
    // Running first (so → lands on the live one), then the newest finished.
    ui.st.borrow().sessions.get(&s.id).map(|d| crate::why::ordered_subs(&d.subagents)).unwrap_or_default()
}

pub fn render_inside(ui: &Ui) {
    render_inside_rows(ui);
    w::shrinkable(ui.w.sw_inside.upcast_ref());
    columns(ui);
}

fn render_inside_rows(ui: &Ui) {
    if !ui.st.borrow().sw.open {
        return;
    }
    let el = &ui.w.sw_inside;
    w::clear(el);
    let (it, i) = current(ui);
    let Some(x) = it.get(i) else { return };
    let ih = w::vbox(3, &["ih"]);
    if let Item::Back(_) = x {
        ih.append(&w::label("Back to your last place", &["b"]));
        ih.append(&w::wrap("Enter goes back to the tab you were on before your last go to.", &["s"]));
        el.append(&ih);
        return;
    }
    let s = x.session().clone();
    let (detail, in_subs, sub_sel, expanded) = {
        let st = ui.st.borrow();
        (st.sessions.get(&s.id).cloned(), st.sw.in_subs, st.sw.sub_sel, st.sw.expanded.clone())
    };
    let total = s.subagents.total;
    let run = s.subagents.running;
    ih.append(&w::wrap(&format!("Inside · {}", s.title), &["b"]));
    let _ = run;
    ih.append(&w::label(&if total > 0 { format!("{}{}", crate::why::sub_count(&s.subagents), if in_subs { "" } else { " · → to look inside" }) } else { "no subagents".into() }, &["s"]));
    el.append(&ih);
    if let (State::Waiting, Some(q)) = (s.state, &s.question) {
        let b = w::vbox(3, &["box", "q", "tight"]);
        b.append(&w::label("It is asking you", &["w"]));
        b.append(&w::wrap(q, &[]));
        el.append(&b);
    } else if let Some(p) = &s.last_prompt {
        el.append(&w::wrap(&format!("You last asked: {}", fmt::clip(p, 200)), &["none", "pad4"]));
    }
    if s.deleted_by_claude {
        el.append(&w::wrap("Kept by Wake — Claude deleted its copy.", &["none", "violet", "pad4"]));
    }
    if total == 0 {
        el.append(&w::label("This session did its work itself.", &["none"]));
        return;
    }
    let Some(d) = detail else {
        el.append(&w::label("Looking inside…", &["loading"]));
        panel::load_session(ui, &s.id, false);
        return;
    };
    for (n, a) in crate::why::ordered_subs(&d.subagents).iter().enumerate() {
        let open = expanded.as_deref() == Some(&a.agent_id);
        let ui2 = ui.clone();
        let click: Box<dyn Fn()> = Box::new(move || {
            toggle_sub(&ui2, n);
            ui2.w.sw_entry.grab_focus();
        });
        let wdg = panel::sub_widget(ui, &s.id, a, open, in_subs && n == sub_sel, Some(click));
        wdg.first_child().inspect(|b| b.set_focusable(false));
        el.append(&wdg);
    }
}

fn toggle_sub(ui: &Ui, n: usize) {
    let subs = subs(ui);
    let Some(a) = subs.get(n) else { return };
    let (it, i) = current(ui);
    let Some(Item::Session(s)) = it.get(i) else { return };
    let sid = s.id.clone();
    let open = {
        let mut st = ui.st.borrow_mut();
        st.sw.in_subs = true;
        st.sw.sub_sel = n;
        st.sw.expanded = if st.sw.expanded.as_deref() == Some(&a.agent_id) { None } else { Some(a.agent_id.clone()) };
        st.sw.expanded.is_some()
    };
    render_inside(ui);
    let known = ui.st.borrow().subs.contains_key(&(sid.clone(), a.agent_id.clone()));
    if open && !known {
        panel::load_sub(ui, &sid, &a.agent_id, render_inside);
    }
}

fn choose(ui: &Ui) {
    let (it, i) = current(ui);
    let Some(x) = it.get(i) else { return };
    let id = x.session().id.clone();
    close(ui);
    app::go_to(ui, &id);
}

pub fn on_key(ui: &Ui, key: gdk::Key) -> glib::Propagation {
    use glib::Propagation::{Proceed, Stop};
    let in_subs = ui.st.borrow().sw.in_subs;
    match key {
        gdk::Key::Down | gdk::Key::Up => {
            let d: i32 = if key == gdk::Key::Down { 1 } else { -1 };
            if in_subs {
                let n = subs(ui).len();
                let mut st = ui.st.borrow_mut();
                st.sw.sub_sel = (st.sw.sub_sel as i32 + d).clamp(0, n.saturating_sub(1) as i32) as usize;
                st.sw.expanded = None;
                drop(st);
                render_inside(ui);
            } else {
                let (it, i) = current(ui);
                if it.is_empty() {
                    return Stop;
                }
                let next = (i as i32 + d).clamp(0, it.len() as i32 - 1) as usize;
                {
                    let mut st = ui.st.borrow_mut();
                    st.sw.sel_id = Some(it[next].id());
                    st.sw.expanded = None;
                }
                render(ui);
            }
            Stop
        }
        gdk::Key::Right => {
            let e = &ui.w.sw_entry;
            if !in_subs && e.position() < e.text().chars().count() as i32 {
                return Proceed;
            }
            if !in_subs {
                let (it, i) = current(ui);
                if let Some(Item::Session(s)) = it.get(i) {
                    if s.subagents.total > 0 && ui.st.borrow().sessions.contains_key(&s.id) {
                        let mut st = ui.st.borrow_mut();
                        st.sw.in_subs = true;
                        st.sw.sub_sel = 0;
                        st.sw.expanded = None;
                    } else if s.subagents.total > 0 {
                        panel::load_session(ui, &s.id, false);
                    }
                }
                render_inside(ui);
            } else {
                let n = ui.st.borrow().sw.sub_sel;
                toggle_sub(ui, n);
            }
            Stop
        }
        gdk::Key::Left => {
            if !in_subs {
                return Proceed;
            }
            {
                let mut st = ui.st.borrow_mut();
                if st.sw.expanded.is_some() {
                    st.sw.expanded = None;
                } else {
                    st.sw.in_subs = false;
                }
            }
            render_inside(ui);
            Stop
        }
        gdk::Key::Return | gdk::Key::KP_Enter => {
            choose(ui);
            Stop
        }
        gdk::Key::Escape => {
            if in_subs {
                {
                    let mut st = ui.st.borrow_mut();
                    st.sw.in_subs = false;
                    st.sw.expanded = None;
                }
                render_inside(ui);
            } else {
                close(ui);
            }
            Stop
        }
        gdk::Key::Tab | gdk::Key::ISO_Left_Tab => Stop,
        _ => Proceed,
    }
}
