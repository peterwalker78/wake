//! The side panel: a session (prompts, Claude's recap, files, subagents to drill into) or a
//! project (next step, last time, since then, repos in plain words, sessions, ideas, pause).

use gtk4::{self as gtk, glib, prelude::*};
use wake_core::*;

use crate::{
    app::{self, Drawer, Ui},
    dialogs, fmt, w,
};

pub fn open_session(ui: &Ui, id: &str, focus: bool) {
    let was_open = ui.st.borrow().drawer.is_some();
    {
        let mut st = ui.st.borrow_mut();
        if st.drawer != Some(Drawer::Session(id.to_string())) {
            st.sub_open.clear();
            st.subs_done_open = false;
            st.panel_page = None;
            // From a History search: open the part that holds the match, as its page.
            let hit = (st.view == app::View::History && !st.search_q.trim().is_empty())
                .then(|| st.search.as_ref()?.iter().find(|(s, _)| s.id == id).map(|(_, m)| m.split_once(": ").map_or("", |(k, _)| k).to_string()))
                .flatten();
            match hit.as_deref() {
                Some("prompt") => st.panel_page = Some("asked"),
                Some("file") => st.panel_page = Some("files"),
                Some("recap") => st.panel_page = Some("recap"),
                _ => {}
            }
        }
        st.drawer = Some(Drawer::Session(id.to_string()));
        if matches!(st.view, app::View::Monitor | app::View::History) {
            st.sel = Some(id.to_string());
        }
    }
    let view = ui.st.borrow().view;
    app::restore_sel(ui, view, false);
    render(ui);
    if focus {
        focus_primary(ui);
    }
    load_session(ui, id, false);
    if !was_open {
        // The table makes room for the panel (its compact columns).
        crate::views::render_view(ui);
    }
}

pub fn open_project(ui: &Ui, id: &str, focus: bool) {
    let was_open = ui.st.borrow().drawer.is_some();
    {
        let mut st = ui.st.borrow_mut();
        if st.drawer != Some(Drawer::Project(id.to_string())) {
            st.edit_next = false;
            st.confirm_remove = false;
            st.panel_page = None;
        }
        st.drawer = Some(Drawer::Project(id.to_string()));
        if st.view == app::View::Projects {
            st.sel = Some(id.to_string());
        }
    }
    let view = ui.st.borrow().view;
    app::restore_sel(ui, view, false);
    render(ui);
    if focus {
        focus_primary(ui);
    }
    load_project(ui, id);
    if !was_open {
        crate::views::render_view(ui);
    }
}

/// Opens a section of the details as a page of its own, with the way back at its top.
pub fn open_page(ui: &Ui, key: &'static str) {
    {
        let mut st = ui.st.borrow_mut();
        st.panel_page = Some(key);
        st.panel_drawn = None;
    }
    render(ui);
    if let Some(sw) = find_scroll(&ui.w.drawer) {
        sw.vadjustment().set_value(0.0);
    }
    if let Some(b) = find_named(ui.w.drawer.upcast_ref(), "page-back") {
        b.grab_focus();
    }
}

/// Back: from a section's page to the summary, else the details close.
pub fn back(ui: &Ui) {
    let page = ui.st.borrow().panel_page;
    let Some(key) = page else {
        close(ui);
        return;
    };
    {
        let mut st = ui.st.borrow_mut();
        st.panel_page = None;
        st.panel_drawn = None;
    }
    render(ui);
    // The keyboard goes back to the row that opened the page.
    if let Some(b) = find_named(ui.w.drawer.upcast_ref(), &format!("fold-{key}")) {
        b.grab_focus();
    }
}

pub fn close(ui: &Ui) {
    {
        let mut st = ui.st.borrow_mut();
        st.drawer = None;
        st.panel_drawn = None;
        st.panel_page = None;
    }
    show(ui, false);
    crate::views::render_view(ui);
    app::focus_list(ui);
}

/// After a click at (`x`, `y`) in the details: the keyboard goes there too, unless it already is,
/// onto the control nearest the click, without scrolling the details.
pub fn focus_near(ui: &Ui, x: f64, y: f64) {
    if ui.st.borrow().drawer.is_none() || gtk::prelude::GtkWindowExt::focus(&ui.window).is_some_and(|f| f.is_ancestor(&ui.w.drawer)) {
        return;
    }
    let root: gtk::Widget = ui.w.drawer.clone().upcast();
    let cs = crate::keynav::controls(&root);
    let near = cs
        .iter()
        .filter_map(|c| {
            let b = c.compute_bounds(&root)?;
            let dx = (f64::from(b.x()) - x).max(x - f64::from(b.x() + b.width())).max(0.0);
            let dy = (f64::from(b.y()) - y).max(y - f64::from(b.y() + b.height())).max(0.0);
            Some((dy * 4.0 + dx, c.clone()))
        })
        .filter(|(_, c)| c.widget_name() != "close")
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, c)| c);
    let viewport = find_scroll(&ui.w.drawer).and_then(|s| s.child()).and_downcast::<gtk::Viewport>();
    if let Some(v) = &viewport {
        v.set_scroll_to_focus(false);
    }
    if !near.is_some_and(|c| c.grab_focus()) {
        focus_primary(ui);
    }
    if let Some(v) = viewport {
        let v2 = v.clone();
        crate::motion::after_layout(&v, move || v2.set_scroll_to_focus(true));
    }
}

pub fn focus_primary(ui: &Ui) {
    let mut child = ui.w.drawer.first_child();
    // The primary button carries the "primary" class; else the close button.
    fn find(w: &gtk::Widget) -> Option<gtk::Widget> {
        if w.is::<gtk::Button>() && w.has_css_class("primary") {
            return Some(w.clone());
        }
        let mut c = w.first_child();
        while let Some(x) = c {
            if let Some(f) = find(&x) {
                return Some(f);
            }
            c = x.next_sibling();
        }
        None
    }
    while let Some(c) = child {
        if let Some(f) = find(&c)
            && f.grab_focus()
        {
            return;
        }
        child = c.next_sibling();
    }
    // No main button (a session Wake can't place, or one that has ended): the first control
    // after the close button, so the keyboard is always in the details once they open.
    let root: gtk::Widget = ui.w.drawer.clone().upcast();
    for c in crate::keynav::controls(&root) {
        if c.widget_name() != "close" && c.grab_focus() {
            return;
        }
    }
    ui.w.drawer.child_focus(gtk::DirectionType::TabForward);
}

/// Loads a session's detail (and, on `refresh`, the open running subagents) off the UI thread.
pub fn load_session(ui: &Ui, id: &str, refresh: bool) {
    let id = id.to_string();
    let open: Vec<String> = if refresh { ui.st.borrow().sub_open.iter().cloned().collect() } else { Vec::new() };
    app::bg(
        ui,
        {
            let id = id.clone();
            move |src| {
                let d = src.session(&id);
                let subs: Vec<SubagentDetail> = d
                    .as_ref()
                    .map(|d| d.subagents.iter().filter(|a| open.contains(&a.agent_id) && a.state == State::Working).filter_map(|a| src.subagent(&id, &a.agent_id)).collect())
                    .unwrap_or_default();
                (d, subs)
            }
        },
        move |ui, (d, subs)| {
            let (live, opened): (Vec<SubagentSummary>, Vec<String>) = {
                let mut st = ui.st.borrow_mut();
                let Some(d) = d else { return };
                let live = d.subagents.clone();
                st.sessions.insert(id.clone(), d);
                let opened = subs.iter().map(|s| s.summary.agent_id.clone()).collect();
                for s in subs {
                    st.subs.insert((id.clone(), s.summary.agent_id.clone()), s);
                }
                (live, opened)
            };
            let still = ui.st.borrow().drawer == Some(Drawer::Session(id.clone()));
            if still {
                // What ticks (a running subagent's line, its time, its open steps) changes in place;
                // anything else redraws, keeping the place.
                let root = ui.w.drawer.clone().upcast::<gtk::Widget>();
                patch_subs(&root, &live);
                for aid in &opened {
                    set_steps(ui, &root, None, &id, aid, true);
                }
                render(ui);
            }
            if ui.st.borrow().sw.open {
                crate::switcher::render_inside(ui);
            }
        },
    );
}

pub fn load_project(ui: &Ui, id: &str) {
    let id = id.to_string();
    app::bg(
        ui,
        {
            let id = id.clone();
            move |src| src.project(&id)
        },
        move |ui, d| {
            if let Some(d) = d {
                ui.st.borrow_mut().projects.insert(id.clone(), d);
            }
            let (still, editing) = {
                let st = ui.st.borrow();
                (st.drawer == Some(Drawer::Project(id.clone())), st.edit_next)
            };
            if still && !editing {
                render(ui);
            }
        },
    );
}

pub fn load_sub(ui: &Ui, sid: &str, aid: &str, then: impl Fn(&Ui) + 'static) {
    let (sid, aid) = (sid.to_string(), aid.to_string());
    app::bg(
        ui,
        {
            let (sid, aid) = (sid.clone(), aid.clone());
            move |src| src.subagent(&sid, &aid)
        },
        move |ui, d| {
            if let Some(d) = d {
                ui.st.borrow_mut().subs.insert((sid, aid), d);
            }
            then(ui);
        },
    );
}

/// A session as the panel shows it, without what ticks by the second (the panel's ages are
/// refreshed once a minute).
fn steady(mut s: SessionSummary) -> SessionSummary {
    s.evidence.clear();
    s.last_at = None;
    s
}

/// What the panel would show: when it hasn't changed, a refresh leaves the panel alone, so its
/// scroll, focus and text selection stay exactly where the reader left them.
fn signature(ui: &Ui) -> String {
    let behind = crate::behind::sig(ui);
    let st = ui.st.borrow();
    // Which sections and subagents are open is not here: opening one happens in place.
    let minute = glib::monotonic_time() / 60_000_000;
    let page = format!("{:?}", st.panel_page);
    let s = match &st.drawer {
        None => String::new(),
        Some(Drawer::Session(id)) => {
            let sum = st.ov.as_ref().and_then(|ov| ov.sessions.iter().find(|s| &s.id == id).cloned()).map(steady);
            // A running subagent's "now:" line and how long it has run are updated in place.
            let det = st.sessions.get(id).cloned().map(|mut d| {
                d.summary = steady(d.summary);
                for a in &mut d.subagents {
                    a.seconds = 0;
                    a.live = None;
                }
                d
            });
            let procs = res_of(&st, id).map(|r| r.pid);
            format!("s{id}{minute}{}{}{procs:?}", serde_json::to_string(&sum).unwrap_or_default(), serde_json::to_string(&det).unwrap_or_default())
        }
        Some(Drawer::Project(id)) => {
            let sum = st.ov.as_ref().and_then(|ov| ov.projects.iter().find(|p| &p.id == id).cloned()).map(|mut p| {
                p.last_at = None;
                p
            });
            let det = st.projects.get(id).cloned().map(|mut d| {
                d.summary.last_at = None;
                d.sessions = d.sessions.into_iter().map(steady).collect();
                d
            });
            format!("p{id}{minute}{}{}{}{}{behind}", serde_json::to_string(&sum).unwrap_or_default(), serde_json::to_string(&det).unwrap_or_default(), st.edit_next, st.confirm_remove)
        }
    };
    if s.is_empty() { s } else { format!("{s}{page}") }
}

/// The panel's scroll position, for the scroll test.
pub fn scroll_adj(ui: &Ui) -> Option<gtk::Adjustment> {
    find_scroll(&ui.w.drawer).map(|s| s.vadjustment())
}

pub fn render(ui: &Ui) {
    let drawer = ui.st.borrow().drawer.clone();
    let Some(drawer) = drawer else {
        show(ui, false);
        ui.st.borrow_mut().panel_drawn = None;
        return;
    };
    let sig = signature(ui);
    if ui.w.drawer.is_visible() && ui.st.borrow().panel_drawn.as_deref() == Some(sig.as_str()) {
        return;
    }
    // The same session or project drawn again (its data changed): the scrolled area stays, only
    // its contents are replaced, so the reader's place is kept exactly, with no timers. Another
    // one starts at the top.
    let same = ui.st.borrow().panel_drawn.as_ref().is_some_and(|d| d.starts_with(&target_key(&drawer))) && ui.w.drawer.is_visible();
    ui.st.borrow_mut().panel_drawn = Some(sig);
    // Where the keyboard is in the details, to put it back on the same control after the redraw:
    // by its name when that is its own, else by its place among the controls.
    let root: gtk::Widget = ui.w.drawer.clone().upcast();
    let had = gtk::prelude::GtkWindowExt::focus(&ui.window).filter(|f| f.is_ancestor(&ui.w.drawer));
    let before = crate::keynav::controls(&root);
    let focus_at = had.as_ref().map(|f| {
        let at = before.iter().position(|c| c == f || f.is_ancestor(c));
        let name = at.map(|i| before[i].widget_name().to_string()).filter(|n| !n.starts_with("Gtk") && !n.is_empty());
        (name, at.unwrap_or(0))
    });
    let sw = find_scroll(&ui.w.drawer);
    let mut c = ui.w.drawer.first_child();
    while let Some(x) = c {
        c = x.next_sibling();
        if Some(&x) != sw.as_ref().map(|s| s.upcast_ref::<gtk::Widget>()) {
            ui.w.drawer.remove(&x);
        }
    }
    if !same && let Some(s) = &sw {
        s.vadjustment().set_value(0.0);
    }
    match &drawer {
        Drawer::Session(id) => session(ui, id),
        Drawer::Project(id) => project(ui, id),
    }
    show(ui, true);
    // Giving focus back must not scroll the focused widget into view (before layout it has no
    // place yet, which used to send the reader to the top): the viewport stops following focus
    // until the new contents are laid out.
    if let Some((name, at)) = focus_at {
        let after = crate::keynav::controls(&root);
        let w = name.and_then(|n| after.iter().find(|c| c.widget_name() == n.as_str()).cloned()).or_else(|| after.get(at.min(after.len().saturating_sub(1))).cloned());
        let viewport = find_scroll(&ui.w.drawer).and_then(|s| s.child()).and_downcast::<gtk::Viewport>();
        if let Some(v) = &viewport {
            v.set_scroll_to_focus(false);
        }
        if !w.is_some_and(|w| w.grab_focus()) {
            focus_primary(ui);
        }
        if let Some(v) = viewport {
            let v2 = v.clone();
            crate::motion::after_layout(&v, move || v2.set_scroll_to_focus(true));
        }
    }
}

/// What names the thing the panel shows: its drawn signature starts with this.
fn target_key(d: &Drawer) -> String {
    match d {
        Drawer::Session(id) => format!("s{id}"),
        Drawer::Project(id) => format!("p{id}"),
    }
}

/// The panel slides in and out beside the table (the table makes room at once; the panel's
/// motion follows). Closing clears it once it has slid away.
fn show(ui: &Ui, on: bool) {
    // Back in the title bar while the details fill the window.
    ui.w.back_btn.set_visible(on && ui.st.borrow().lay.push);
    let rev = &ui.w.drawer_rev;
    if on {
        if !ui.w.drawer.is_visible() {
            ui.w.drawer.set_visible(true);
        }
        if !rev.reveals_child() {
            ui.w.drawer.set_opacity(1.0);
            rev.set_reveal_child(true);
        }
        return;
    }
    if !rev.reveals_child() && !ui.w.drawer.is_visible() {
        return;
    }
    rev.set_reveal_child(false);
    let ui2 = ui.clone();
    let done = move || {
        if ui2.st.borrow().drawer.is_none() && !ui2.w.drawer_rev.reveals_child() {
            ui2.w.drawer.set_visible(false);
            w::clear(&ui2.w.drawer);
        }
    };
    if crate::motion::on() && rev.is_mapped() {
        glib::timeout_add_local_once(std::time::Duration::from_millis(u64::from(crate::motion::DUR) + 40), done);
    } else {
        done();
    }
}

fn find_scroll(w: &gtk::Box) -> Option<gtk::ScrolledWindow> {
    let mut c = w.first_child();
    while let Some(x) = c {
        if let Ok(s) = x.clone().downcast::<gtk::ScrolledWindow>() {
            return Some(s);
        }
        c = x.next_sibling();
    }
    None
}

pub fn find_named(w: &gtk::Widget, name: &str) -> Option<gtk::Widget> {
    if w.widget_name() == name {
        return Some(w.clone());
    }
    let mut c = w.first_child();
    while let Some(x) = c {
        if let Some(f) = find_named(&x, name) {
            return Some(f);
        }
        c = x.next_sibling();
    }
    None
}

fn head(ui: &Ui, title: &str, sub: &[gtk::Widget]) {
    // No back bar here: a phone-narrow window has Back in its title bar.
    let dh = w::hbox(10, &["dh"]);
    let left = w::vbox(4, &[]);
    left.set_hexpand(true);
    let h = w::wrap(title, &["title-2"]);
    h.set_xalign(0.0);
    left.append(&h);
    for s in sub {
        left.append(s);
    }
    dh.append(&left);
    // Filling the window, the details close with Back in the title bar.
    if !ui.st.borrow().lay.push {
        dh.append(&close_button(ui));
    }
    put_top(ui, &dh);
}

/// The standard close button at the details' top right (Esc).
fn close_button(ui: &Ui) -> gtk::Button {
    let ui2 = ui.clone();
    let x = gtk::Button::new();
    x.set_child(Some(&w::icon(w::icons::CLOSE)));
    x.add_css_class("flat");
    x.set_widget_name("close");
    x.set_tooltip_text(Some("Close details (Esc)"));
    x.update_property(&[gtk::accessible::Property::Label("Close")]);
    x.set_valign(gtk::Align::Start);
    x.connect_clicked(move |_| close(&ui2));
    x
}

/// A section's page: Back to the summary, the section's name, and what it belongs to.
fn page_head(ui: &Ui, section: &str, of: &str) {
    let dh = w::hbox(8, &["dh", "pagehead"]);
    let ui2 = ui.clone();
    let b = gtk::Button::new();
    b.set_child(Some(&w::icon(w::icons::BACK)));
    b.add_css_class("flat");
    b.set_widget_name("page-back");
    b.set_tooltip_text(Some("Back (Esc)"));
    b.update_property(&[gtk::accessible::Property::Label("Back")]);
    b.set_valign(gtk::Align::Center);
    b.connect_clicked(move |_| back(&ui2));
    dh.append(&b);
    let left = w::vbox(2, &[]);
    left.set_hexpand(true);
    let t = w::wrap(section, &["title-3"]);
    t.set_xalign(0.0);
    left.append(&t);
    let o = w::clip(of, &["dim", "caption"]);
    o.set_xalign(0.0);
    left.append(&o);
    dh.append(&left);
    dh.append(&close_button(ui));
    put_top(ui, &dh);
}

/// A section's page title, by its key.
fn page_title(key: &str) -> &'static str {
    match key {
        "subs" => "Subagents",
        "asked" => "You asked",
        "recap" => "Claude's recap",
        "files" => "Files it changed",
        "sessions" => "Sessions",
        "ideas" => "Ideas",
        "git" => "Git",
        "disk" => "Disk",
        "colour" => "Dot colour",
        _ => "Details",
    }
}

/// Adds `w` above the panel's scrolled area (the title, the actions, a banner block).
pub fn put_top(ui: &Ui, w: &impl IsA<gtk::Widget>) {
    match find_scroll(&ui.w.drawer) {
        Some(sw) => w.insert_before(&ui.w.drawer, Some(&sw)),
        None => ui.w.drawer.append(w),
    }
}

/// The panel's scrolled area with new, empty contents. The area itself is kept across redraws of
/// the same thing, so its scroll position is too.
fn body(ui: &Ui) -> gtk::Box {
    let b = w::vbox(16, &["dbody"]);
    match find_scroll(&ui.w.drawer) {
        Some(sw) => sw.set_child(Some(&b)),
        None => {
            let s = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&b).build();
            ui.w.drawer.append(&s);
        }
    }
    b
}

fn skeleton(b: &gtk::Box) {
    // Narrower than the narrowest panel (320 px), so a skeleton never widens it.
    for width in [160, 280, 240, 200] {
        let s = w::hbox(0, &["skel"]);
        s.set_size_request(width, 12);
        s.set_halign(gtk::Align::Start);
        b.append(&s);
    }
}

fn section(title: &str, link: Option<gtk::Button>) -> gtk::Box {
    let s = w::sec(title);
    if let Some(l) = link {
        s.append(&w::spacer());
        s.append(&l);
    }
    s
}

fn link(text: &str, name: &str, act: impl Fn() + 'static) -> gtk::Button {
    let b = gtk::Button::with_label(text);
    b.add_css_class("link");
    b.set_widget_name(name);
    b.connect_clicked(move |_| act());
    b
}

fn asked(at: &str, text: &str) -> gtk::Box {
    let b = w::hbox(8, &["asked"]);
    let t = w::label(at, &["at"]);
    t.set_size_request(54, -1);
    t.set_valign(gtk::Align::Start);
    b.append(&t);
    b.append(&w::wrap(text, &[]));
    b
}

fn boxed(class: &str) -> gtk::Box {
    w::vbox(4, &["box", class])
}

/// How long a subagent has run, or when it finished.
fn sub_when(a: &SubagentSummary) -> String {
    if a.state == State::Working {
        let took = if a.seconds < 60 { "under a minute".to_string() } else { fmt::secs(a.seconds as u64) };
        format!("{took} so far")
    } else {
        crate::why::finished_ago(a)
    }
}

/// A subagent: its line, and when open its steps and what it handed back (sliding open in place).
pub fn sub_widget(ui: &Ui, sid: &str, a: &SubagentSummary, open: bool, sel: bool, on_click: Option<Box<dyn Fn()>>) -> gtk::Box {
    let outer = w::vbox(0, &["sub"]);
    outer.set_widget_name(&format!("subw-{}", a.agent_id));
    if sel {
        outer.add_css_class("sel");
    }
    let btn = gtk::Button::new();
    btn.add_css_class("subbtn");
    btn.set_widget_name(&format!("sub-{}", a.agent_id));
    let inner = w::vbox(3, &[]);
    let h = w::hbox(8, &["h"]);
    let dclass = match a.state {
        State::Working => "running",
        State::Ended => "done",
        _ => "unknown",
    };
    h.append(&w::dot(dclass));
    if !a.kind.is_empty() && a.kind != "general-purpose" {
        h.append(&w::label(&a.kind, &["ty"]));
    }
    h.append(&w::clip(&a.task, &["task"]));
    let dur = w::label(&sub_when(a), &["dur"]);
    dur.set_widget_name(&format!("subdur-{}", a.agent_id));
    h.append(&dur);
    inner.append(&h);
    if a.state == State::Working {
        let l = w::clip(&format!("now: {}", a.live.as_deref().unwrap_or("working")), &["one", "live"]);
        l.set_widget_name(&format!("sublive-{}", a.agent_id));
        inner.append(&l);
    } else if let Some(back) = &a.handed_back {
        let l = w::clip(back, &["one"]);
        l.set_widget_name(&format!("subback-{}", a.agent_id));
        l.set_visible(!open);
        inner.append(&l);
    }
    btn.set_child(Some(&inner));
    if let Some(f) = on_click {
        btn.connect_clicked(move |_| f());
    } else {
        btn.set_focusable(false);
        btn.set_can_target(false);
    }
    outer.append(&btn);
    let steps = w::vbox(3, &["steps"]);
    if open {
        fill_steps(ui, sid, &a.agent_id, &steps);
    }
    let rev = crate::motion::revealer(gtk::RevealerTransitionType::SlideDown, &steps, open);
    rev.set_widget_name(&format!("steps-{}", a.agent_id));
    outer.append(&rev);
    outer
}

/// A subagent's steps and what it handed back, from what has been read of its record.
fn fill_steps(ui: &Ui, sid: &str, aid: &str, steps: &gtk::Box) {
    w::clear(steps);
    let step = |left: &str, right: &str, class: &str| {
        let r = w::hbox(8, &[]);
        let l = w::label(left, &["at"]);
        l.set_size_request(70, -1);
        l.set_valign(gtk::Align::Start);
        r.append(&l);
        r.append(&w::wrap(right, &[class]));
        r
    };
    let detail = ui.st.borrow().subs.get(&(sid.to_string(), aid.to_string())).cloned();
    match detail {
        None => steps.append(&w::label("reading its record…", &["loading"])),
        Some(d) => {
            let tools: Vec<String> = d.summary.tools.iter().map(|(k, v)| format!("{k} {v}")).collect();
            if !tools.is_empty() {
                steps.append(&step("tools", &tools.join(" · "), "meta"));
            }
            if d.steps.is_empty() {
                steps.append(&step("", "no steps recorded", "meta"));
            }
            for s in &d.steps {
                steps.append(&step(&s.verb, &s.what, "plain"));
            }
            if d.summary.state == State::Working {
                steps.append(&step("now", d.summary.live.as_deref().unwrap_or("still working"), "ret"));
            } else if let Some(back) = d.handed_back_full.as_ref().or(d.summary.handed_back.as_ref()) {
                steps.append(&step("handed back", back, "ret"));
            }
        }
    }
}

/// Opens or closes a subagent's steps in place, under `root` (the panel, an Agents row, the
/// switcher): no redraw, so nothing above moves and the scroll stays. Opening scrolls it into view
/// within `scroll` when it opens below the fold.
pub fn set_steps(ui: &Ui, root: &gtk::Widget, scroll: Option<&gtk::ScrolledWindow>, sid: &str, aid: &str, open: bool) {
    let Some(rev) = find_named(root, &format!("steps-{aid}")).and_downcast::<gtk::Revealer>() else { return };
    if open && let Some(steps) = rev.child().and_downcast::<gtk::Box>() {
        fill_steps(ui, sid, aid, &steps);
    }
    if let Some(back) = find_named(root, &format!("subback-{aid}")) {
        back.set_visible(!open);
    }
    rev.set_reveal_child(open);
    if open && let (Some(sw), Some(block)) = (scroll, find_named(root, &format!("subw-{aid}"))) {
        // Opened under the subagent's row: show the steps, never move the row off the top.
        crate::motion::reveal_below(sw, &block, &block);
    }
}

/// Updates the running subagents' "now:" lines and times in place (they change every few seconds,
/// and nothing else needs drawing for them).
pub fn patch_subs(root: &gtk::Widget, subs: &[SubagentSummary]) {
    for a in subs {
        if let Some(l) = find_named(root, &format!("sublive-{}", a.agent_id)).and_downcast::<gtk::Label>() {
            let t = format!("now: {}", a.live.as_deref().unwrap_or("working"));
            if l.text() != t {
                l.set_text(&t);
                l.set_tooltip_text(Some(&t));
            }
        }
        if let Some(l) = find_named(root, &format!("subdur-{}", a.agent_id)).and_downcast::<gtk::Label>() {
            let t = sub_when(a);
            if l.text() != t {
                l.set_text(&t);
            }
        }
    }
}

/// A section of the details. On the summary it is a row (its name, a count, ›) that opens it as a
/// page; on its own page, all of its content; on another section's page, nothing.
pub fn fold(ui: &Ui, key: &'static str, title: &str, note: &str, note_class: &str, build: impl FnOnce(&gtk::Box)) -> gtk::Box {
    let page = ui.st.borrow().panel_page;
    let outer = w::vbox(0, &["fold"]);
    match page {
        Some(k) if k == key => {
            outer.add_css_class("onpage");
            let inner = w::vbox(8, &["foldin"]);
            build(&inner);
            outer.append(&inner);
        }
        Some(_) => outer.set_visible(false),
        None => {
            let btn = gtk::Button::new();
            btn.add_css_class("foldbtn");
            btn.set_widget_name(&format!("fold-{key}"));
            let h = w::hbox(10, &[]);
            h.append(&w::label(title, &["ft"]));
            h.append(&w::spacer());
            if !note.is_empty() {
                let n = w::label(note, &["fnote"]);
                // A long note gives way before the panel grows.
                n.set_ellipsize(gtk::pango::EllipsizeMode::End);
                if !note_class.is_empty() {
                    n.add_css_class(note_class);
                }
                h.append(&n);
            }
            h.append(&w::icon(w::icons::NEXT));
            btn.set_child(Some(&h));
            btn.set_tooltip_text(Some(&format!("Open {title}")));
            btn.update_property(&[gtk::accessible::Property::Label(title)]);
            let ui2 = ui.clone();
            btn.connect_clicked(move |_| open_page(&ui2, key));
            outer.append(&btn);
        }
    }
    outer
}

/// Once `rev` has opened, scrolls the panel so `head` and what opened under it are in view.
fn reveal_into_view(ui: &Ui, rev: &gtk::Revealer, head: &gtk::Widget) {
    let Some(sw) = find_scroll(&ui.w.drawer) else { return };
    let block = head.parent().unwrap_or_else(|| head.clone());
    if rev.is_child_revealed() || !crate::motion::on() {
        crate::motion::reveal_below(&sw, head, &block);
        return;
    }
    let head = head.clone();
    let id: std::rc::Rc<std::cell::RefCell<Option<glib::SignalHandlerId>>> = Default::default();
    let id2 = id.clone();
    let h = rev.connect_child_revealed_notify(move |r| {
        if r.is_child_revealed() {
            if let Some(h) = id2.borrow_mut().take() {
                r.disconnect(h);
            }
            crate::motion::reveal_below(&sw, &head, &block);
        }
    });
    *id.borrow_mut() = Some(h);
}

/// The session's line under its title: its status tag, then its project and where it is.
fn session_line(ui: &Ui, s: &SessionSummary) -> gtk::Box {
    let line = w::hbox(8, &["dline"]);
    line.append(&w::state_badge(s.state));
    let (mut loc, ..) = app::loc_for(ui, s);
    // "in <app>": the terminal app whose window shows it.
    if let Some(l) = &s.location {
        loc = format!("in {}", l.app.as_deref().unwrap_or("a terminal window"));
    }
    let wl = w::wrap(&format!("{} · {loc}", crate::disk::project_reason(ui, s)), &["dim"]); // disk.rs hook
    wl.set_xalign(0.0);
    wl.set_hexpand(true);
    line.append(&wl);
    // How Wake knows, and the rest of the small print, only when asked.
    let agent = format!("{:?}", s.agent).to_lowercase();
    line.set_tooltip_text(Some(&format!("{}\n{agent} · {} · {}", s.evidence, fmt::minutes(s.minutes), fmt::pl(s.prompts, "prompt", "prompts"))));
    line
}

/// ⋯ beside the main button: its project, which project it belongs to, and stopping it.
fn session_menu(ui: &Ui, s: &SessionSummary) -> gtk::MenuButton {
    let mb = gtk::MenuButton::new();
    mb.set_child(Some(&w::icon(w::icons::MORE)));
    mb.add_css_class("flat");
    mb.set_widget_name("panel-more");
    mb.set_tooltip_text(Some("More"));
    mb.set_valign(gtk::Align::Center);
    let pop = gtk::Popover::new();
    let items = w::vbox(2, &["moremenu"]);
    let item = |label: &str, danger: bool, f: Box<dyn Fn()>| {
        let b = gtk::Button::with_label(label);
        b.add_css_class("menuitem");
        if danger {
            b.add_css_class("danger");
        }
        let pop = pop.clone();
        b.connect_clicked(move |_| {
            pop.popdown();
            f();
        });
        items.append(&b);
    };
    if let Some(p) = s.project.clone().filter(|p| app::find_project(ui, p).is_some()) {
        let ui2 = ui.clone();
        item("Open its project", false, Box::new(move || open_project(&ui2, &p, true)));
    }
    let (ui2, id) = (ui.clone(), s.id.clone());
    item("Change its project…", false, Box::new(move || crate::disk::set_project(&ui2, &id)));
    // Stopping: the same confirmation as in Agents, and never the main button.
    let pid = res_of(&ui.st.borrow(), &s.id).map(|r| r.pid);
    if let Some(pid) = pid {
        let ui2 = ui.clone();
        item("Show it in Agents", false, Box::new(move || crate::agents::show_row(&ui2, pid)));
    }
    if let Some(pid) = pid.filter(|_| app::is_live(s.state)) {
        let ui2 = ui.clone();
        item("Stop this agent…", true, Box::new(move || crate::agents::stop_one(&ui2, pid)));
    }
    pop.set_child(Some(&items));
    mb.set_popover(Some(&pop));
    mb
}

fn session(ui: &Ui, id: &str) {
    let (detail, summary) = {
        let st = ui.st.borrow();
        let sum = st.ov.as_ref().and_then(|ov| ov.sessions.iter().find(|s| s.id == id).cloned());
        (st.sessions.get(id).cloned(), sum)
    };
    let Some(s) = summary.clone().or_else(|| detail.as_ref().map(|d| d.summary.clone())) else {
        head(ui, "Session", &[]);
        skeleton(&body(ui));
        return;
    };
    let page = ui.st.borrow().panel_page;
    if let Some(k) = page {
        // A section's page: the way back, its name, and whose it is.
        page_head(ui, page_title(k), &s.title);
    } else {
        let mut sub: Vec<gtk::Widget> = vec![session_line(ui, &s).upcast()];
        if let Some(r) = crate::why::stop_reason(&s).filter(|r| r.decision.is_none()) {
            let l = w::wrap(&r.words, &["dim"]);
            l.set_xalign(0.0);
            l.set_tooltip_text(Some(&s.evidence));
            sub.push(l.upcast());
        }
        head(ui, &s.title, &sub);
        // One main action, and the rest under ⋯.
        let act = w::hbox(8, &["dact"]);
        if let Some(b) = go_button(ui, &s, true) {
            act.append(&b);
            if app::is_live(s.state) {
                // Holding Space is only a key, so say what it does here, where you'd go to it.
                act.append(&crate::peek::cue(ui, &s, true));
            }
        } else {
            act.append(&w::clip(if s.deleted_by_claude { "Claude deleted the conversation; it can't be reopened." } else { "Nothing to reopen." }, &["meta"]));
        }
        act.append(&w::spacer());
        act.append(&session_menu(ui, &s));
        put_top(ui, &act);
    }

    let b = body(ui);
    // What it is doing now: the decision or question it is waiting on (with the way there right
    // under it), else Claude's recap, else your last ask.
    let recap = s.away_summary.as_deref().map(fmt::recap).filter(|a| !a.is_empty());
    let mut recap_shown = false;
    let asks = s.question.clone().or_else(|| crate::why::stop_reason(&s).and_then(|r| r.decision)).filter(|_| page.is_none());
    if page.is_some() {
        // A section's page shows only that section.
    } else if let Some(q) = &asks {
        b.append(&decision_card(ui, &s, q));
    } else if let Some(a) = &recap {
        let x = w::vbox(6, &["now"]);
        x.append(&w::sec("Where it got to"));
        x.append(&w::wrap(a, &["lead"]));
        b.append(&x);
        recap_shown = true;
    } else if let Some(p) = &s.last_prompt {
        let x = w::vbox(6, &["now"]);
        x.append(&w::sec("You last asked"));
        x.append(&w::wrap(p, &["lead"]));
        b.append(&x);
    }
    let Some(d) = detail else {
        skeleton(&b);
        return;
    };
    let (sub_open, done_open) = {
        let st = ui.st.borrow();
        (st.sub_open.clone(), st.subs_done_open)
    };
    let folds = w::vbox(0, &["folds"]);
    if page.is_some() {
        folds.add_css_class("paged");
    }
    // Subagents first while one is running: that is what moves.
    let run = d.subagents.iter().filter(|a| a.state == State::Working).count();
    let subs_fold = |folds: &gtk::Box| {
        if d.subagents.is_empty() {
            return;
        }
        let note = crate::why::sub_count(&s.subagents);
        folds.append(&fold(ui, "subs", "Subagents", &note, if run > 0 { "mint" } else { "" }, |x| {
            // Running first; the finished ones folded into one line, newest first, on demand.
            let (running, done) = crate::why::split_subs(&d.subagents);
            let one = |a: &SubagentSummary| {
                let open = sub_open.contains(&a.agent_id);
                let ui2 = ui.clone();
                let (sid, aid) = (s.id.clone(), a.agent_id.clone());
                let click: Box<dyn Fn()> = Box::new(move || {
                    let now_open = {
                        let mut st = ui2.st.borrow_mut();
                        if !st.sub_open.remove(&aid) {
                            st.sub_open.insert(aid.clone());
                            true
                        } else {
                            false
                        }
                    };
                    let root = ui2.w.drawer.clone().upcast::<gtk::Widget>();
                    set_steps(&ui2, &root, find_scroll(&ui2.w.drawer).as_ref(), &sid, &aid, now_open);
                    if now_open {
                        let aid2 = aid.clone();
                        load_sub(&ui2, &sid, &aid, move |ui| {
                            let open = ui.st.borrow().sub_open.contains(&aid2);
                            let sid = match ui.st.borrow().drawer.clone() {
                                Some(Drawer::Session(id)) => id,
                                _ => return,
                            };
                            if open {
                                let root = ui.w.drawer.clone().upcast::<gtk::Widget>();
                                set_steps(ui, &root, find_scroll(&ui.w.drawer).as_ref(), &sid, &aid2, true);
                            }
                        });
                    }
                });
                sub_widget(ui, &s.id, a, open, false, Some(click))
            };
            for a in &running {
                x.append(&one(a));
            }
            if done.is_empty() {
                return;
            }
            let list = w::vbox(0, &[]);
            for a in &done {
                list.append(&one(a));
            }
            if running.is_empty() && done.len() <= 3 {
                // A few, nothing running: no need to fold them.
                x.append(&list);
                return;
            }
            x.append(&done_fold(&crate::why::finished_line(&done), done_open, &list, move |ui, open| ui.st.borrow_mut().subs_done_open = open, ui));
        }));
    };
    if run > 0 {
        subs_fold(&folds);
    }
    if !d.prompts.is_empty() {
        let n = d.prompts.len();
        folds.append(&fold(ui, "asked", "You asked", &n.to_string(), "", |x| {
            // Its own page has the room: every prompt.
            for p in &d.prompts {
                let at = p.at.as_deref().and_then(fmt::parse).and_then(|t| t.to_local().ok()).and_then(|t| t.format("%H:%M").ok()).map(|s| s.to_string()).unwrap_or_default();
                x.append(&asked(&at, &p.text));
            }
        }));
    }
    if let Some(a) = recap.filter(|_| !recap_shown) {
        folds.append(&fold(ui, "recap", "Claude's recap", "", "", |x| x.append(&w::wrap(&a, &["lead"]))));
    }
    if !d.files.is_empty() {
        let n = d.files.len();
        folds.append(&fold(ui, "files", "Files it changed", &n.to_string(), "", |x| {
            // Its own page has the room: every file.
            for f in &d.files {
                let r = w::hbox(9, &["did"]);
                r.append(&w::wrap(&fmt::rel(&f.path, s.cwd.as_deref()), &["p"]));
                r.append(&w::label(&f.edits.to_string(), &["num"]));
                x.append(&r);
            }
        }));
    }
    if run == 0 {
        subs_fold(&folds);
    }
    folds.append(&fold(ui, "details", "Details", "", "", |x| {
        let mut lines = vec![s.evidence.clone()];
        if s.deleted_by_claude {
            lines.push("Kept by Wake: Claude deleted its copy of the conversation.".into());
        }
        let mut facts = Vec::new();
        if d.commands > 0 {
            facts.push(fmt::pl(d.commands, "command", "commands"));
        }
        if d.tests > 0 {
            facts.push(fmt::pl(d.tests, "test run", "test runs"));
        }
        if let Some(c) = d.cost_usd {
            facts.push(format!("${c:.2}"));
        }
        lines.push(facts.join(" · "));
        lines.extend(d.pr_links.iter().map(|u| u.trim_start_matches("https://github.com/").to_string()));
        if let Some(cwd) = &s.cwd {
            lines.push(format!("in {}", fmt::tildify(cwd)));
        }
        if let Some(r) = &s.resume_command {
            lines.push(format!("resume: {r}"));
        }
        if let Some(j) = &s.job_id {
            lines.push(format!("background job {j} · attach: claude attach {j}"));
        }
        if let Some(t) = &d.transcript {
            lines.push(format!("transcript: {t}"));
        }
        if let Some(r) = res_of(&ui.st.borrow(), &s.id) {
            lines.push(format!("{} in memory, with its terminal · PID {}", fmt::bytes(r.rss_bytes), r.pid));
        }
        lines.push(format!("started {} · id {}", fmt::when(s.started_at.as_deref()), s.id));
        let f = w::wrap(&lines.into_iter().filter(|l| !l.is_empty()).collect::<Vec<_>>().join("\n"), &["facts"]);
        f.set_selectable(true);
        x.append(&f);
    }));
    b.append(&folds);
}

/// A live session's processes added up (wake-core's `Overview::resources`).
fn res_of(st: &app::AppState, id: &str) -> Option<SessionResources> {
    st.ov.as_ref()?.resources.as_ref()?.session(id).cloned()
}

/// "735 MB in memory": with its terminal; the PID and the breakdown are in the tooltip.
fn proc_words(r: SessionResources) -> String {
    format!("{} in memory", fmt::bytes(r.rss_bytes))
}

/// The memory on the open panel's process line, kept current in place.
pub fn tick_procs(ui: &Ui) {
    let Some(Drawer::Session(id)) = ui.st.borrow().drawer.clone() else { return };
    let Some(b) = find_named(ui.w.drawer.upcast_ref(), "pmem").and_downcast::<gtk::Button>() else { return };
    let t = {
        let st = ui.st.borrow();
        res_of(&st, &id).map(proc_words)
    };
    // Change the wrapping label inside, never the button's own label (that one doesn't wrap).
    if let Some(t) = t
        && let Some(l) = b.child().and_downcast::<gtk::Label>()
        && l.text().as_str() != t.as_str()
    {
        l.set_text(&t);
    }
}

/// The way to a session, saying where it goes (the same key, o): "Go to tab 3" for one in a tab,
/// "Open in a new window" for one running in the background, "Resume in a new window" for one
/// that has ended and can be reopened.
pub fn go_button(ui: &Ui, s: &SessionSummary, primary: bool) -> Option<gtk::Button> {
    if !(s.location.is_some() || app::can_reopen(s) || app::is_live(s.state)) {
        return None;
    }
    let (loc, _, can_go, tip) = app::loc_for(ui, s);
    let word = go_words(s, &loc, can_go);
    let ui2 = ui.clone();
    let id = s.id.clone();
    let b = w::button(&word, None, if primary { &["primary"] } else { &["sm"] }, move || app::go_to(&ui2, &id));
    if primary {
        b.set_widget_name("primary");
    }
    if !tip.is_empty() {
        b.set_tooltip_text(Some(&tip));
    }
    Some(b)
}

/// What the way-there button says, from `loc_for`'s place: "Go to <its terminal app>",
/// "Go to its window", "Open in a new window", "Resume in a new window".
pub fn go_words(s: &SessionSummary, loc: &str, can_go: bool) -> String {
    if s.location.is_some() {
        format!("Go to {loc}")
    } else if !can_go {
        // Wake is still finding its window.
        "Go to".into()
    } else if app::can_reopen(s) {
        "Resume in a new window".into()
    } else {
        // Live in no window: a background job, attached in a new window.
        "Open in a new window".into()
    }
}

/// The amber card for a session that wants something from you: what it asks, and the way there
/// right under it (Go to, or Resume when it has ended).
fn decision_card(ui: &Ui, s: &SessionSummary, ask: &str) -> gtk::Box {
    let k = boxed("q");
    let title = match (s.state, crate::why::stop_reason(s).map(|r| r.words)) {
        (State::Waiting, Some(w)) if w == "asks you" => "It is asking you",
        (State::Waiting, _) => "It wants a decision",
        (State::Ended, _) => "It ended asking you this",
        _ => "It stopped on this question",
    };
    let t = w::sec(title);
    t.add_css_class("amber");
    k.append(&t);
    k.append(&w::wrap(ask, &["h"]));
    if let Some(b) = go_button(ui, s, false) {
        b.set_halign(gtk::Align::Start);
        b.add_css_class("q-go");
        k.append(&b);
    }
    k
}

/// The finished subagents, folded into one line ("28 finished · last one 12 min ago") that opens
/// them in place.
fn done_fold(line: &str, open: bool, list: &gtk::Box, remember: impl Fn(&Ui, bool) + 'static, ui: &Ui) -> gtk::Box {
    let outer = w::vbox(0, &["donefold"]);
    let btn = gtk::Button::new();
    btn.add_css_class("donebtn");
    btn.set_widget_name("subs-done");
    let h = w::hbox(8, &[]);
    let chev = w::label(if open { "▾" } else { "▸" }, &["chev"]);
    h.append(&chev);
    h.append(&w::clip(line, &["dl"]));
    btn.set_child(Some(&h));
    outer.append(&btn);
    // A long list fades in rather than sliding: sliding would lay it all out on every frame.
    let rev = crate::motion::revealer(gtk::RevealerTransitionType::Crossfade, list, open);
    outer.append(&rev);
    let ui2 = ui.clone();
    btn.connect_clicked(move |b| {
        let open = !rev.reveals_child();
        remember(&ui2, open);
        chev.set_text(if open { "▾" } else { "▸" });
        rev.set_reveal_child(open);
        if open {
            reveal_into_view(&ui2, &rev, b.upcast_ref());
        }
    });
    outer
}

/// A project's most recently active session.
fn last_session(ui: &Ui, project: &str) -> Option<SessionSummary> {
    let st = ui.st.borrow();
    let ov = st.ov.as_ref()?;
    ov.sessions.iter().filter(|s| s.project.as_deref() == Some(project)).max_by(|a, b| a.last_at.cmp(&b.last_at)).cloned()
}

/// What in this project wants a decision from you: a waiting session's ask, else Claude's last
/// recap here when it asks for one (on its most recent session).
fn project_ask(ui: &Ui, p: &ProjectSummary) -> Option<(SessionSummary, String)> {
    let waiting = {
        let st = ui.st.borrow();
        st.ov.as_ref().and_then(|ov| ov.sessions.iter().find(|s| s.project.as_deref() == Some(&p.id) && s.state == State::Waiting && s.question.is_some()).cloned())
    };
    if let Some(s) = waiting {
        let q = s.question.clone()?;
        return Some((s, q));
    }
    let ask = p.last_summary.as_deref().and_then(crate::why::decision_in)?;
    let last = last_session(ui, &p.id)?;
    // A recap that asked, with its session since back at work, has been answered.
    (last.state != State::Working).then_some((last, ask))
}

/// One repo in plain words; `unasked` when Wake couldn't ask the remote, so "in step" isn't claimed.
fn repo(r: &RepoState, unasked: bool) -> gtk::Box {
    let b = w::vbox(3, &["repo"]);
    b.append(&w::wrap(&format!("{} · {}", fmt::base(&r.path), r.branch.as_deref().unwrap_or("no branch")), &["rn"]));
    let mut lines: Vec<(&str, String)> = Vec::new();
    // No remote at all is a fine way to work: said once, quietly, rather than as a loose end.
    let local_only = local_only(r);
    let unpushed = if r.remote_host.as_deref() == Some("GitHub") { "on GitHub" } else { "pushed" };
    if local_only {
        lines.push(("", "Only on this computer (no remote).".into()));
    } else if r.upstream.is_none() {
        lines.push(("w", format!("This branch isn't {unpushed} yet.")));
    } else if let Some(a) = r.ahead.filter(|a| *a > 0) {
        lines.push(("w", format!("{} not {unpushed} yet.", fmt::pl(a, "commit", "commits"))));
    }
    if let Some(b) = r.behind.filter(|b| *b > 0) {
        lines.push(("", format!("{} on the remote you don't have here.", fmt::pl(b, "change", "changes"))));
    }
    if r.dirty_files > 0 {
        lines.push(("w", format!("{} edited but not committed.", fmt::pl(r.dirty_files, "file", "files"))));
    }
    if r.untracked > 0 {
        lines.push(("", format!("{} git isn't tracking.", fmt::pl(r.untracked, "new file", "new files"))));
    }
    // Not when the one such branch is this one, already said above.
    if r.branches_no_upstream > 0 && !local_only && !(r.upstream.is_none() && r.branches_no_upstream == 1) {
        let n = r.branches_no_upstream;
        lines.push(("", format!("{n} of {} {} not {unpushed}.", fmt::pl(r.branches, "branch", "branches"), if n == 1 { "is" } else { "are" })));
    }
    if r.worktrees > 1 {
        lines.push(("", format!("{} (worktrees) open.", fmt::pl(r.worktrees - 1, "side copy", "side copies"))));
    }
    if lines.is_empty() && unasked {
        lines.push(("", "Nothing unsaved. Wake couldn't check the remote just now.".into()));
    } else if lines.is_empty() {
        lines.push(("ok", "In step with the remote. Nothing unsaved.".into()));
    }
    for (c, t) in lines {
        b.append(&w::wrap(&t, &["ex", if c.is_empty() { "plain" } else { c }]));
    }
    if let Some(at) = &r.last_commit_at {
        b.append(&w::wrap(&format!("last commit {}: {}", fmt::ago(Some(at)), r.last_commit_subject.as_deref().unwrap_or("")), &["meta"]));
    }
    b
}

/// Why no repo of a project could be read, from its `could_not_check` lines ("<path>: <reason>"),
/// in plain words; None when there is simply no git repository in its folders.
fn git_unread(failed: &[String]) -> Option<String> {
    let reasons: Vec<&str> = failed.iter().map(|f| f.split_once(": ").map_or(f.as_str(), |(_, r)| r)).filter(|r| *r != "no git repository here").collect();
    if reasons.iter().any(|r| r.contains("(os error 2)")) {
        // Running `git` itself failed: it isn't on PATH.
        return Some("git isn't installed".into());
    }
    let first = reasons.first()?;
    Some(if *first == "folder not found" { "its folder isn't there".into() } else { (*first).to_string() })
}

/// What a pause kept besides the note, in one quiet line: "Saved with main @ 7d05b9e · 2 open
/// sessions"; with several repos each is named ("wake main @ 7d05b9e, site dev @ 1a2b3c4"). None
/// when it kept neither.
fn saved_with(cp: &Checkpoint) -> Option<String> {
    let short = |h: &String| h.chars().take(7).collect::<String>();
    let repos: Vec<String> = cp
        .git
        .iter()
        .map(|(path, branch, head, _)| {
            let at = match (branch, head) {
                (Some(b), Some(h)) => format!("{b} @ {}", short(h)),
                (Some(b), None) => b.clone(),
                (None, Some(h)) => format!("@ {}", short(h)),
                (None, None) => "no branch".into(),
            };
            if cp.git.len() > 1 { format!("{} {at}", fmt::base(path)) } else { at }
        })
        .collect();
    let mut parts = Vec::new();
    if !repos.is_empty() {
        parts.push(repos.join(", "));
    }
    if !cp.sessions.is_empty() {
        parts.push(fmt::pl(cp.sessions.len() as u64, "open session", "open sessions"));
    }
    (!parts.is_empty()).then(|| format!("Saved with {}", parts.join(" · ")))
}

/// `r` has no remote at all: it lives only on this computer.
fn local_only(r: &RepoState) -> bool {
    r.remote_host.is_none()
}

/// A change to a project, run off the UI thread; it hands back the project as it is now.
type ProjectChange = Box<dyn FnOnce(&dyn wake_core::Source) -> Result<ProjectSummary, String> + Send>;

/// ⋯ beside Pause: how much the project matters, and which group it is in.
pub fn project_menu(ui: &Ui, p: &ProjectSummary) -> gtk::MenuButton {
    let mb = gtk::MenuButton::new();
    mb.set_child(Some(&w::icon(w::icons::MORE)));
    mb.add_css_class("flat");
    mb.set_widget_name("project-more");
    mb.set_tooltip_text(Some("Importance and group"));
    mb.set_valign(gtk::Align::Center);
    let pop = gtk::Popover::new();
    let items = w::vbox(2, &["moremenu"]);
    // Says what changed, and draws the project again.
    let apply = |ui: &Ui, id: String, work: ProjectChange, said: String| {
        app::bg(ui, work, move |ui, r| match r {
            Ok(_) => {
                app::toast(ui, &said, false);
                app::refresh(ui);
                load_project(ui, &id);
            }
            Err(e) => app::toast(ui, &e, true),
        });
    };
    items.append(&w::label("Importance", &["caption-heading", "menuhead"]));
    for (val, label) in [(Some("high"), "High"), (None, "Normal"), (Some("low"), "Low")] {
        let on = p.importance.as_deref() == val;
        let b = gtk::Button::with_label(&format!("{} {label}", if on { "✓" } else { "  " }));
        b.add_css_class("menuitem");
        let (ui2, pop2, id, name) = (ui.clone(), pop.clone(), p.id.clone(), p.name.clone());
        b.connect_clicked(move |_| {
            pop2.popdown();
            let id2 = id.clone();
            apply(&ui2, id.clone(), Box::new(move |src| src.set_project_importance(&id2, val)), format!("{name}: {} importance.", label.to_lowercase()));
        });
        items.append(&b);
    }
    items.append(&w::label("Group", &["caption-heading", "menuhead"]));
    // One of the person's groups (new ones come from "+ New group" above the projects), or none.
    let groups: Vec<String> = ui.st.borrow().ov.as_ref().map(|ov| ov.groups.clone()).unwrap_or_default();
    for g in groups.into_iter().map(Some).chain([None]) {
        let on = p.group == g;
        let label = g.clone().unwrap_or_else(|| "No group".into());
        let b = gtk::Button::with_label(&format!("{} {label}", if on { "✓" } else { "  " }));
        b.add_css_class("menuitem");
        let (ui2, pop2, id, name) = (ui.clone(), pop.clone(), p.id.clone(), p.name.clone());
        b.connect_clicked(move |_| {
            pop2.popdown();
            let (id2, g2) = (id.clone(), g.clone());
            let said = match &g {
                Some(g) => format!("{name} is in {g}."),
                None => format!("{name} is in no group."),
            };
            apply(&ui2, id.clone(), Box::new(move |src| src.set_project_group(&id2, g2.as_deref())), said);
        });
        items.append(&b);
    }
    pop.set_child(Some(&items));
    mb.set_popover(Some(&pop));
    mb
}

fn project(ui: &Ui, id: &str) {
    let (detail, summary, edit_next, confirm_remove) = {
        let st = ui.st.borrow();
        (st.projects.get(id).cloned(), st.ov.as_ref().and_then(|ov| ov.projects.iter().find(|p| p.id == id).cloned()), st.edit_next, st.confirm_remove)
    };
    let Some(p) = summary.or_else(|| detail.as_ref().map(|d| d.summary.clone())) else {
        head(ui, "Project", &[]);
        skeleton(&body(ui));
        return;
    };
    // One line: what is going on in it now, and when it was last touched.
    let mut now = Vec::new();
    if p.live.waiting > 0 {
        now.push(format!("{} waiting for you", p.live.waiting));
    }
    if p.live.working > 0 {
        now.push(format!("{} working", p.live.working));
    }
    if now.is_empty() {
        now.push(if p.paused.is_some() { "paused".to_string() } else { "nothing running".to_string() });
    }
    if p.last_at.is_some() {
        now.push(format!("last touched {}", fmt::ago(p.last_at.as_deref())));
    }
    let line = w::clip(&now.join(" · "), &["where"]);
    line.add_css_class("dline");
    let mut sub: Vec<gtk::Widget> = vec![line.upcast()];
    if !p.desc.is_empty() {
        sub.push(w::wrap(&p.desc, &["desc"]).upcast());
    }
    let page = ui.st.borrow().panel_page;
    if let Some(k) = page {
        page_head(ui, page_title(k), &p.name);
    } else {
        head(ui, &p.name, &sub);
    }

    let act = w::hbox(8, &["dact"]);
    if page.is_some() {
        // A section's page: only that section.
    } else if p.removed {
        let ui2 = ui.clone();
        let id2 = p.id.clone();
        let b = w::button("Track again", None, &["primary"], move || crate::views::restore_project(&ui2, &id2));
        b.set_widget_name("primary");
        act.append(&b);
        act.append(&w::clip("Wake is not tracking this project.", &["meta"]));
    } else {
        let ui2 = ui.clone();
        let id2 = p.id.clone();
        let b = w::button(if p.paused.is_some() { "Pause again" } else { "Pause" }, None, &["primary"], move || dialogs::pause(&ui2, &id2));
        b.set_widget_name("primary");
        b.set_tooltip_text(Some("Keep a note, the next step and where git stands, so coming back is quick."));
        act.append(&b);
        let ui2 = ui.clone();
        let id2 = p.id.clone();
        act.append(&w::button("Its sessions", None, &["ghost"], move || {
            {
                let mut st = ui2.st.borrow_mut();
                st.proj_filter = Some(id2.clone());
                st.filter = app::Filter::All;
                st.sel = None;
            }
            close(&ui2);
            app::set_view(&ui2, app::View::Monitor);
            app::render_all(&ui2);
        }));
        // Its most recent session, when it has ended and can be reopened.
        if let Some(last) = last_session(ui, &p.id).filter(|s| app::can_reopen(s) && !app::is_live(s.state)) {
            let ui2 = ui.clone();
            let sid = last.id.clone();
            let b = w::button("Resume last", None, &["ghost"], move || app::go_to(&ui2, &sid));
            b.set_widget_name("resume-last");
            b.set_tooltip_text(Some(&format!("Resume its last session, “{}” ({}): in a new terminal window.", last.title, fmt::ago(last.last_at.as_deref()))));
            act.append(&b);
        }
        act.append(&w::spacer());
        act.append(&project_menu(ui, &p));
    }
    if page.is_none() {
        put_top(ui, &act);
        if let Some(bk) = crate::behind::panel_block(ui, &p.id) {
            put_top(ui, &bk);
        }
    }

    let b = body(ui);
    // A session here that wants a decision: the ask, with the way there right under it.
    if let Some((s, ask)) = project_ask(ui, &p).filter(|_| page.is_none()) {
        b.append(&decision_card(ui, &s, &ask));
    }
    let x = w::vbox(6, &["now"]);
    x.set_visible(page.is_none());
    let edit = (!edit_next && !p.removed).then(|| {
        let ui = ui.clone();
        link("edit", "edit-next", move || {
            ui.st.borrow_mut().edit_next = true;
            render(&ui);
        })
    });
    x.append(&section("Next step", edit));
    if edit_next {
        let nb = boxed("next");
        let entry = gtk::Entry::builder().text(&p.next).max_length(400).build();
        entry.set_widget_name("next-edit");
        let ui2 = ui.clone();
        let id2 = p.id.clone();
        entry.connect_activate(move |e| {
            let next = e.text().trim().to_string();
            let id3 = id2.clone();
            app::bg(
                &ui2,
                move |src| src.set_next(&id3, &next),
                |ui, r| match r {
                    Ok(p) => {
                        ui.st.borrow_mut().edit_next = false;
                        app::toast(ui, "Next step saved.", false);
                        load_project(ui, &p.id);
                        app::refresh(ui);
                    }
                    Err(e) => app::toast(ui, &e, true),
                },
            );
        });
        nb.append(&entry);
        nb.append(&w::wrap("Enter saves · Esc cancels", &["w"]));
        let e2 = entry.clone();
        gtk::glib::idle_add_local_once(move || {
            e2.grab_focus();
        });
        x.append(&nb);
    } else if !p.next.is_empty() {
        x.append(&w::wrap(&p.next, &["lead", "strong"]));
    } else if let Some(sum) = &p.last_summary {
        x.append(&w::wrap(&fmt::recap(sum), &["lead"]));
        x.append(&w::wrap("Claude's last recap here; no next step written yet.", &["meta"]));
    } else {
        x.append(&w::wrap("No next step written yet.", &["lead", "quiet"]));
    }
    if !p.why.is_empty() {
        x.append(&w::wrap(&format!("Why: {}", p.why), &["meta"]));
    }
    b.append(&x);
    let Some(d) = detail else {
        skeleton(&b);
        return;
    };
    if let Some(lp) = d.last_pause.as_ref().filter(|_| page.is_none()) {
        let x = w::vbox(6, &["now"]);
        x.append(&w::sec(&format!("Where you left it · {}", fmt::when(Some(&lp.at)))));
        x.append(&w::wrap(if lp.note.is_empty() { "(no note)" } else { &lp.note }, &["lead"]));
        if let Some(saved) = saved_with(lp) {
            x.append(&w::wrap(&saved, &["meta"]));
        }
        for s in d.since_pause.iter().take(3) {
            x.append(&w::wrap(&format!("Since then: {s}"), &["meta"]));
        }
        b.append(&x);
    }
    let folds = w::vbox(0, &["folds"]);
    if page.is_some() {
        folds.add_css_class("paged");
    }
    let mut ss = d.sessions.clone();
    ss.sort_by(crate::views::by_session);
    if !ss.is_empty() {
        let waiting = ss.iter().filter(|s| s.state == State::Waiting).count();
        let note = if waiting > 0 { format!("{waiting} waiting") } else { ss.len().to_string() };
        folds.append(&fold(ui, "sessions", "Sessions", &note, if waiting > 0 { "amber" } else { "" }, |x| {
            for s in ss.iter().take(20) {
                let btn = gtk::Button::new();
                btn.add_css_class("srow");
                let g = w::vbox(3, &[]);
                let top = w::hbox(8, &[]);
                let (class, word) = w::state_word(s.state);
                top.append(&w::dot(class));
                top.append(&w::clip(&s.title, &[]));
                g.append(&top);
                let (loc, ..) = app::loc_for(ui, s);
                let sub = w::clip(&format!("{word} · {loc}"), &["sub-l"]);
                sub.set_tooltip_text(Some(&s.evidence));
                g.append(&sub);
                btn.set_child(Some(&g));
                let ui2 = ui.clone();
                let sid = s.id.clone();
                btn.connect_clicked(move |_| open_session(&ui2, &sid, true));
                x.append(&btn);
            }
        }));
    }
    let to_sort: Vec<&Idea> = d.ideas.iter().filter(|i| i.state == "inbox").collect();
    let sent = d.ideas.iter().filter(|i| i.state == "sent").count();
    if !to_sort.is_empty() || sent > 0 {
        let note = if to_sort.is_empty() { format!("{sent} sent") } else { to_sort.len().to_string() };
        folds.append(&fold(ui, "ideas", "Ideas", &note, "", |x| {
            for i in &to_sort {
                let r = w::hbox(9, &["did"]);
                r.append(&w::wrap(&i.text, &[]));
                r.append(&w::label(&fmt::ago(Some(&i.at)), &["num"]));
                x.append(&r);
            }
            if !to_sort.is_empty() {
                let ids: Vec<String> = to_sort.iter().map(|i| i.id.clone()).collect();
                let ui2 = ui.clone();
                let label = format!("Start an agent on {}", if ids.len() == 1 { "it".to_string() } else { format!("these {}", ids.len()) });
                let b = w::button(&label, None, &["sm", "violet"], move || crate::start::open(&ui2, Some(ids.clone())));
                b.set_widget_name("start-agent");
                b.set_halign(gtk::Align::Start);
                b.set_tooltip_text(Some("Starts an agent in a new terminal window in this project's folder, with a brief of these ideas"));
                x.append(&b);
            }
            if sent > 0 {
                x.append(&w::label(&format!("{} sent to an agent", fmt::pl(sent as u64, "idea", "ideas")), &["meta"]));
            }
        }));
    }
    if !d.repos.is_empty() {
        // The one git fact that asks for something, else that all is well. Repos only on this
        // computer are a way of working, not a loose end: said, but not in amber.
        let g = &p.git;
        let all_local = d.repos.iter().all(local_only);
        let (note, warn) = if crate::behind::of(ui, &p.id).is_some() {
            ("behind its remote", true)
        } else if g.ahead_total > 0 || g.no_upstream > 0 {
            (if g.github { "not on GitHub" } else { "not pushed" }, true)
        } else if g.dirty_files > 0 {
            ("uncommitted edits", true)
        } else if all_local {
            ("only on this computer (no remote)", false)
        } else if let Some(why) = &p.remote_problem {
            // Wake couldn't ask the remote, so it doesn't claim "in step".
            (why.as_str(), false)
        } else {
            ("in step", false)
        };
        folds.append(&fold(ui, "git", "Git", note, if warn { "amber" } else { "" }, |x| {
            for r in &d.repos {
                x.append(&repo(r, p.remote_problem.is_some()));
            }
            x.append(&w::wrap("Read only: Wake changes a repo only when you press Pull or Remove worktree.", &["meta"]));
        }));
    } else if let Some(why) = git_unread(&d.could_not_check).filter(|_| !p.roots.is_empty()) {
        // Rather than no Git section at all: say why Wake can't read it.
        folds.append(&fold(ui, "git", "Git", "can't read", "amber", |x| {
            x.append(&w::wrap(&format!("Wake can't read git here: {why}. Run wake doctor in a terminal to check."), &["ex", "w"]));
        }));
    }
    // The project's dot colour: a row of swatches (Tab or the arrows move, Enter picks).
    let colour = crate::setview::colour_of(&p);
    crate::disk::panel_fold(ui, &p, &folds); // disk.rs hook: the Disk section
    folds.append(&fold(ui, "colour", "Dot colour", &colour, "", |x| {
        // Fourteen in two rows of seven, so they fit the narrowest panel (320 px) with room to spare.
        let rows = w::vbox(7, &["swatches"]);
        let mut row = w::hbox(7, &[]);
        for (n, c) in wake_core::store::COLOUR_NAMES.into_iter().enumerate() {
            if n > 0 && n % 7 == 0 {
                rows.append(&row);
                row = w::hbox(7, &[]);
            }
            let b = gtk::Button::new();
            b.add_css_class("swatch");
            b.add_css_class(&format!("pc-{c}"));
            b.set_widget_name(&format!("colour-{c}"));
            if c == colour {
                b.add_css_class("on");
            }
            b.set_tooltip_text(Some(c));
            b.update_property(&[gtk::accessible::Property::Label(&format!("{c}{}", if c == colour { ", chosen" } else { "" }))]);
            let (ui2, id2) = (ui.clone(), p.id.clone());
            b.connect_clicked(move |_| crate::setview::set_colour(&ui2, &id2, c));
            row.append(&b);
        }
        rows.append(&row);
        x.append(&rows);
        x.append(&w::wrap("Its dot in the side list and on its rows: filled while something is open in it, a ring when nothing is.", &["meta"]));
    }));
    let pid = p.id.clone();
    let pname = p.name.clone();
    folds.append(&fold(ui, "pdetails", "Details", "", "", |x| {
        let f = w::wrap(&p.roots.iter().map(|r| fmt::tildify(r)).collect::<Vec<_>>().join("\n"), &["facts"]);
        f.set_selectable(true);
        x.append(&f);
        for c in &d.could_not_check {
            x.append(&w::wrap(&format!("Couldn't check: {c}"), &["meta"]));
        }
        if p.removed {
            return;
        }
        let row = w::hbox(8, &[]);
        if confirm_remove {
            x.append(&w::wrap(&format!("Wake stops showing {pname}. Nothing on disk changes, and you can track it again from Projects."), &["meta"]));
            let ui2 = ui.clone();
            let id2 = pid.clone();
            let b = w::button("Stop tracking", None, &["danger", "sm"], move || {
                ui2.st.borrow_mut().confirm_remove = false;
                let id3 = id2.clone();
                app::bg(
                    &ui2,
                    move |src| src.remove_project(&id3, true),
                    |ui, r| match r {
                        Ok(p) => {
                            app::toast(ui, &format!("Stopped tracking {}. Track it again from Projects any time.", p.name), false);
                            app::refresh(ui);
                            close(ui);
                        }
                        Err(e) => app::toast(ui, &e, true),
                    },
                );
            });
            b.set_widget_name("confirm");
            row.append(&b);
            let ui2 = ui.clone();
            row.append(&w::button("Keep", None, &["ghost", "sm"], move || {
                ui2.st.borrow_mut().confirm_remove = false;
                render(&ui2);
            }));
        } else {
            let ui2 = ui.clone();
            let b = w::button("Stop tracking…", None, &["ghost", "sm"], move || {
                ui2.st.borrow_mut().confirm_remove = true;
                render(&ui2);
            });
            b.set_widget_name("stop-tracking");
            b.set_tooltip_text(Some("Wake stops showing it. Nothing on disk changes."));
            row.append(&b);
        }
        x.append(&row);
    }));
    b.append(&folds);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pause_says_what_it_saved() {
        let mut cp = Checkpoint { project: "p".into(), at: String::new(), note: String::new(), next: String::new(), git: Vec::new(), sessions: Vec::new() };
        assert_eq!(saved_with(&cp), None);
        cp.git.push(("/home/sam/Projects/wake".into(), Some("main".into()), Some("7d05b9e1f2a3".into()), Some(0)));
        cp.sessions = vec!["a".into(), "b".into()];
        assert_eq!(saved_with(&cp).as_deref(), Some("Saved with main @ 7d05b9e · 2 open sessions"));
        cp.git.push(("/home/sam/Projects/site".into(), None, None, None));
        cp.sessions.clear();
        assert_eq!(saved_with(&cp).as_deref(), Some("Saved with wake main @ 7d05b9e, site no branch"));
    }

    #[test]
    fn unreadable_git_says_why_and_a_plain_folder_says_nothing() {
        assert_eq!(git_unread(&["/home/sam/x: No such file or directory (os error 2)".into()]).as_deref(), Some("git isn't installed"));
        assert_eq!(git_unread(&["/home/sam/x: no git repository here".into()]), None);
        assert_eq!(git_unread(&["/home/sam/x: folder not found".into()]).as_deref(), Some("its folder isn't there"));
        assert_eq!(git_unread(&["/home/sam/x: fatal: detected dubious ownership".into()]).as_deref(), Some("fatal: detected dubious ownership"));
        assert_eq!(git_unread(&[]), None);
    }

    #[test]
    fn a_repo_with_no_remote_is_local_only() {
        let r = RepoState {
            path: "/x".into(),
            branch: Some("main".into()),
            head: None,
            upstream: None,
            ahead: None,
            behind: None,
            dirty_files: 0,
            untracked: 0,
            branches_no_upstream: 2,
            branches: 2,
            worktrees: 1,
            last_commit_at: None,
            last_commit_subject: None,
            remote_host: None,
        };
        assert!(local_only(&r));
        // A remote with nothing pushed to it yet is a loose end, not "only on this computer".
        assert!(!local_only(&RepoState { remote_host: Some("GitHub".into()), ..r }));
    }
}
