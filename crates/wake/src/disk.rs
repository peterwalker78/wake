//! Disk, memory by project, and which project a session belongs to (added 2026-11-20).
//!
//! - Projects: a DISK column ("19 GB · 18 GB rebuildable") and a MEMORY column (its live sessions
//!   with their terminals and helpers), both sortable; above the table, one line for the machine
//!   ("86 GB free of 244 GB · 25 GB could be rebuilt") whose "Disk details" opens the machine's list.
//!   The machine's list is a dialog rather than a view of its own: it is looked at rarely, and a
//!   line above Projects plus a dialog adds no key, no rail entry and no new place to learn.
//! - The project panel: a "Disk" section, each item in plain words with its size, when it was last
//!   used, how safe it is to delete and what deleting costs, and the one thing Wake offers for it.
//! - Sessions: "probably night-shift" where Wake guessed the project (the panel says why), and Set
//!   project (`m`, or the panel's "change" link).
//!
//! The numbers come from the core's `Overview::{disk, resources, attribution}`; the cells that
//! move (memory, CPU, disk while measuring) update in place, so no table is rebuilt for them.
//! Hooks in other files are marked `disk.rs hook`.

use std::cell::Cell;
use std::cmp::Ordering;

use gtk4::{self as gtk, gdk, glib, prelude::*};
use wake_core::*;

use crate::app::{self, AppState, Drawer, Ui, View};
use crate::{dialogs, fmt, w};

const CSS: &str = "
.disk-item { padding: 8px 0 9px 0; border-bottom: 1px solid @line; }
.disk-item.inner { margin-left: 16px; }
.disk-item:last-child { border-bottom: none; }
.disk-name { font-size: 12.5px; color: @ink; }
.disk-size { font-family: \"JetBrains Mono\", monospace; font-size: 12px; color: @soft; }
.disk-cost { font-size: 11.5px; color: @muted; }
.disk-safe { color: @mint; } .disk-check { color: @amber; } .disk-keep { color: @dim; }
.disk-refused { font-size: 11px; color: @amber; }
.disk-cmd { font-family: \"JetBrains Mono\", monospace; font-size: 11px; color: @soft; background-color: @codebg; border-radius: 5px; padding: 3px 7px; }
.disk-scroll { min-height: 120px; }
.disk-proj { padding: 3px 0; }
";

thread_local! {
    static CSS_ON: Cell<bool> = const { Cell::new(false) };
}

/// This module's few styles, added once beside the app's stylesheet.
fn ensure_css() {
    if CSS_ON.with(|c| c.replace(true)) {
        return;
    }
    let Some(display) = gdk::Display::default() else { return };
    let p = gtk::CssProvider::new();
    p.load_from_string(CSS);
    gtk::style_context_add_provider_for_display(&display, &p, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1);
}

/// "19 GB", "601 MB": the same units as memory everywhere else.
pub fn size(b: u64) -> String {
    fmt::bytes(b)
}

fn report(st: &AppState) -> Option<&DiskReport> {
    st.ov.as_ref()?.disk.as_ref()
}

// ---------------------------------------------------------------- Projects: the Disk button

fn mem_tip(st: &AppState, id: &str) -> Option<String> {
    let r = st.ov.as_ref()?.resources.as_ref()?.project(id)?;
    Some(format!(
        "{} in {} with their terminals and helpers ({} processes).{}",
        fmt::bytes(r.rss_bytes),
        fmt::pl(r.sessions, "live session", "live sessions"),
        r.processes,
        r.cpu_percent.map(|c| format!("\nCPU: {:.0}% of one core.", c)).unwrap_or_default()
    ))
}

/// Projects sorted by `key`: "disk" and "memory" here, the rest by `sort::projects`.
pub fn sort_projects(list: &mut [ProjectSummary], key: &str, rev: bool, base: fn(&ProjectSummary, &ProjectSummary) -> Ordering, ov: &Overview) {
    let n = |p: &ProjectSummary| -> u64 {
        match key {
            "disk" => ov.disk.as_ref().and_then(|d| d.project(&p.id)).map_or(0, |d| d.bytes),
            _ => ov.resources.as_ref().and_then(|r| r.project(&p.id)).map_or(0, |r| r.rss_bytes),
        }
    };
    if key != "disk" && key != "memory" {
        crate::sort::projects(list, key, rev, base);
        return;
    }
    list.sort_by(|a, b| {
        let o = n(b).cmp(&n(a));
        a.removed.cmp(&b.removed).then_with(|| if rev { o.reverse() } else { o }).then_with(|| base(a, b))
    });
}

fn machine_words(st: &AppState) -> String {
    let Some(r) = report(st) else { return "Disk: Wake is measuring it in the background, at low priority.".into() };
    let Some(m) = &r.machine else {
        return r.measuring.as_deref().map_or_else(|| "Disk: not measured yet.".into(), |x| format!("Disk: {x}…"));
    };
    let mut s = format!("Disk: {} free of {} on {}", size(m.home_free), size(m.home_total), m.home_mount);
    if r.rebuildable > 0 {
        s.push_str(&format!(" · {} could be rebuilt", size(r.rebuildable)));
    }
    match &r.measuring {
        Some(x) => s.push_str(&format!(" · {x}…")),
        None => s.push_str(&format!(" · measured {}", fmt::ago(r.measured_at.as_deref()))),
    }
    s
}

/// Projects' "Disk" button: it opens the machine's disk details, and its tooltip says how full
/// the disk is (kept current). Disk is its own job, so Projects gives it a button, not a column.
pub fn details_button(ui: &Ui) -> gtk::Button {
    ensure_css();
    let ui2 = ui.clone();
    let b = w::button("Disk", None, &["ghost", "sm"], move || machine(&ui2));
    b.set_widget_name("disk-details");
    let tip = |st: &AppState| format!("{}\nWhat takes the space, and what could be cleared and made again.", machine_words(st));
    b.set_tooltip_text(Some(&tip(&ui.st.borrow())));
    crate::views::live_tip(ui, View::Projects, &b, move |st| Some(tip(st)));
    b
}

// ---------------------------------------------------------------- items

fn safety_words(it: &DiskItem) -> (&'static str, &'static str) {
    match (it.safety, it.kind) {
        (DiskSafety::Safe, DiskKind::Worktree) => ("safe to remove", "disk-safe"),
        (DiskSafety::Safe, _) => ("safe to clear", "disk-safe"),
        (DiskSafety::Check, _) => ("look first", "disk-check"),
        (DiskSafety::Keep, _) => ("keep", "disk-keep"),
    }
}

fn copy(ui: &Ui, text: &str) {
    ui.window.clipboard().set_text(text);
    app::toast(ui, &format!("Copied: {text}"), false);
}

fn open_folder(ui: &Ui, path: &str) {
    if ui.demo {
        app::toast(ui, &format!("Demo: this would open {} in the file manager.", fmt::tildify(path)), false);
        return;
    }
    let uri = gtk::gio::File::for_path(path).uri();
    if let Err(e) = gtk::gio::AppInfo::launch_default_for_uri(&uri, None::<&gtk::gio::AppLaunchContext>) {
        app::toast(ui, &format!("Couldn't open the folder: {e}"), true);
    }
}

/// The live "an agent here is working" check, on top of what the measurement said.
fn refusal(ui: &Ui, it: &DiskItem, project: Option<&str>) -> Option<String> {
    if it.action == Some(DiskAction::Clear)
        && let Some(p) = project
    {
        let st = ui.st.borrow();
        let busy = st.ov.as_ref().is_some_and(|o| o.sessions.iter().any(|s| s.state == State::Working && s.project.as_deref() == Some(p)));
        if busy {
            return Some("An agent in this project is working; it may be building into this. Try when it's idle.".into());
        }
    }
    it.refused.clone()
}

/// One item: name and size, when last used and how safe, what deleting costs, then the one thing
/// Wake offers (or the command a person would use).
fn item_widget(ui: &Ui, it: &DiskItem, project: Option<&str>, project_name: &str) -> gtk::Box {
    let b = w::vbox(3, &["disk-item"]);
    if it.within.is_some() {
        b.add_css_class("inner");
    }
    let top = w::hbox(10, &[]);
    top.append(&w::wrap(&it.name, &["disk-name"]));
    let sz = w::label(&format!("{}{}", if it.partial { "≥ " } else { "" }, size(it.bytes)), &["disk-size"]);
    sz.set_valign(gtk::Align::Start);
    top.append(&sz);
    b.append(&top);
    let (sw, sc) = safety_words(it);
    let mut meta = Vec::new();
    if it.last_used.is_some() {
        meta.push(format!("last used {}", fmt::ago(it.last_used.as_deref())));
    }
    // Pango can't take a CSS class: the safety word gets the palette's colour in a span.
    let colour = match sc {
        "disk-safe" => crate::theme::hex("mint"),
        "disk-check" => crate::theme::hex("amber"),
        _ => crate::theme::hex("dim"),
    };
    let sep = if meta.is_empty() { "" } else { " · " };
    b.append(&w::markup(&format!("{}{sep}<span foreground=\"{colour}\">{}</span>", w::esc(&meta.join(" · ")), w::esc(sw)), &["meta"]));
    if !it.cost.is_empty() {
        b.append(&w::wrap(&it.cost, &["disk-cost"]));
    }
    if let Some(wt) = &it.worktree {
        let mut facts = Vec::new();
        if let Some(t) = &wt.made_by_title {
            facts.push(format!("made by the session \"{t}\""));
        }
        if let Some(u) = &wt.in_use {
            facts.push(format!("in use now: {u}"));
        }
        if !facts.is_empty() {
            let row = w::hbox(8, &[]);
            row.append(&w::wrap(&facts.join(" · "), &["disk-cost"]));
            if let Some(sid) = wt.made_by.clone() {
                let ui2 = ui.clone();
                let l = w::button("Its session", None, &["ghost", "sm"], move || crate::panel::open_session(&ui2, &sid, true));
                l.set_valign(gtk::Align::Start);
                row.append(&l);
            }
            b.append(&row);
        }
    }
    let refused = refusal(ui, it, project);
    let acts = w::hbox(8, &[]);
    let mut any = false;
    match it.action {
        Some(DiskAction::Clear) => {
            let (ui2, it2, p2, n2) = (ui.clone(), it.clone(), project.map(String::from), project_name.to_string());
            let btn = w::button("Clear…", None, &["sm"], move || confirm_clear(&ui2, &it2, p2.as_deref(), &n2));
            btn.set_tooltip_text(Some("Move it to the Trash, after you confirm."));
            btn.set_sensitive(refused.is_none());
            acts.append(&btn);
            any = true;
        }
        Some(DiskAction::RemoveWorktree) => {
            let (ui2, it2) = (ui.clone(), it.clone());
            let btn = w::button("Remove worktree…", None, &["sm"], move || confirm_remove(&ui2, &it2));
            btn.set_sensitive(refused.is_none());
            acts.append(&btn);
            any = true;
        }
        _ => {}
    }
    if matches!(it.action, Some(DiskAction::OpenFolder) | Some(DiskAction::RemoveWorktree)) || it.kind == DiskKind::Folder {
        let (ui2, p) = (ui.clone(), it.path.clone());
        acts.append(&w::button("Open folder", None, &["ghost", "sm"], move || open_folder(&ui2, &p)));
        any = true;
    }
    if any {
        b.append(&acts);
    }
    if let Some(r) = refused.filter(|_| matches!(it.action, Some(DiskAction::Clear) | Some(DiskAction::RemoveWorktree))) {
        b.append(&w::wrap(&r, &["disk-refused"]));
    }
    if let Some(cmd) = it.command.as_deref().filter(|_| it.kind != DiskKind::Worktree || it.action != Some(DiskAction::RemoveWorktree)) {
        let row = w::hbox(8, &[]);
        let c = w::wrap(cmd, &["disk-cmd"]);
        c.set_selectable(true);
        row.append(&c);
        let (ui2, cmd2) = (ui.clone(), cmd.to_string());
        let cb = w::button("Copy", None, &["ghost", "sm"], move || copy(&ui2, &cmd2));
        cb.set_valign(gtk::Align::Start);
        row.append(&cb);
        b.append(&row);
        if let Some(n) = &it.command_note {
            b.append(&w::wrap(&format!("It {n}. Wake never runs it."), &["meta"]));
        }
    }
    b
}

// ---------------------------------------------------------------- the project panel's section

/// The project panel's "Disk" section. disk.rs hook target.
pub fn panel_fold(ui: &Ui, p: &ProjectSummary, folds: &gtk::Box) {
    ensure_css();
    let pd = report(&ui.st.borrow()).and_then(|r| r.project(&p.id)).cloned();
    let measuring = report(&ui.st.borrow()).and_then(|r| r.measuring.clone());
    let note = match &pd {
        Some(d) if d.rebuildable >= 1 << 27 => format!("{} · {} rebuildable", size(d.bytes), size(d.rebuildable)),
        Some(d) => size(d.bytes),
        None if measuring.is_some() => "measuring…".into(),
        None => "not measured".into(),
    };
    let (id, name) = (p.id.clone(), p.name.clone());
    folds.append(&crate::panel::fold(ui, "disk", "Disk", &note, "", |x| {
        match &pd {
            Some(d) if !d.items.is_empty() => {
                for it in &d.items {
                    x.append(&item_widget(ui, it, Some(&id), &name));
                }
            }
            Some(_) => x.append(&w::wrap("Nothing big here: no build output, worktrees or large folders.", &["meta"])),
            None => x.append(&w::wrap("Wake hasn't measured this project yet. It measures in the background at low priority.", &["meta"])),
        }
        let row = w::hbox(8, &[]);
        let when = match (&measuring, &pd) {
            (Some(m), _) => format!("{m}…"),
            (None, Some(d)) => format!("Measured {}; again every hour.", fmt::ago(d.measured_at.as_deref())),
            _ => String::new(),
        };
        row.append(&w::wrap(&when, &["meta"]));
        let (ui2, id2, n2) = (ui.clone(), id.clone(), name.clone());
        row.append(&w::button("Measure again", None, &["ghost", "sm"], move || {
            let id3 = id2.clone();
            app::bg(&ui2, move |src| src.measure_disk(Some(&id3)), |_, _| {});
            app::toast(&ui2, &format!("Measuring {n2} again, in the background."), false);
        }));
        x.append(&row);
    }));
}

// ---------------------------------------------------------------- the two actions

fn confirm_clear(ui: &Ui, it: &DiskItem, project: Option<&str>, project_name: &str) {
    if let Some(r) = refusal(ui, it, project) {
        app::toast(ui, &r, true);
        return;
    }
    let whose = if project.is_some() { format!(", in {project_name}") } else { String::new() };
    let body = [
        format!("{}{whose}: {}.", it.name, size(it.bytes)),
        format!("What it costs: {}", it.cost),
        "Wake moves only this folder to the Trash; nothing else is touched. The space comes back when you empty the Trash (gio trash --empty).".to_string(),
    ];
    let path = it.path.clone();
    let project = project.map(String::from);
    // Said with whose folder it was: the Trash may hold several called `target`.
    let done = match &project {
        Some(_) => format!("Moved {project_name}'s {} folder ({}) to the Trash. The space comes back when the Trash is emptied.", fmt::base(&it.path), size(it.bytes)),
        None => format!("Moved {} ({}) to the Trash. The space comes back when the Trash is emptied.", fmt::tildify(&it.path), size(it.bytes)),
    };
    dialogs::confirm(ui, &format!("Move {} to the Trash?", it.name), &body, &[(fmt::tildify(&it.path), size(it.bytes))], "Move to Trash", move |ui| {
        let p = path.clone();
        let project = project.clone();
        let done = done.clone();
        app::bg(
            ui,
            move |src| src.clear_disk(&p, false),
            move |ui, r| {
                app::toast(ui, if r.ok { &done } else { &r.message }, !r.ok);
                after_action(ui, project.as_deref());
            },
        );
    });
}

fn confirm_remove(ui: &Ui, it: &DiskItem) {
    if let Some(r) = &it.refused {
        app::toast(ui, r, true);
        return;
    }
    let Some(wt) = &it.worktree else { return };
    let branch = wt.branch.clone().unwrap_or_else(|| "its branch".into());
    let made = match &wt.made_by_title {
        Some(t) => format!("Made by the session \"{t}\"."),
        None => "Wake doesn't know which session made it.".to_string(),
    };
    let body = [
        made,
        format!(
            "Its branch {branch} is merged into {}. git worktree remove deletes this copy and everything in it ({}, its build output included) for good: it doesn't go to the Trash. The branch itself stays, and git refuses if anything in the copy isn't committed.",
            wt.default_branch.as_deref().unwrap_or("the default branch"),
            size(it.bytes)
        ),
    ];
    let cmd = it.command.clone().unwrap_or_default();
    let path = it.path.clone();
    let project = report(&ui.st.borrow()).and_then(|r| r.projects.iter().find(|p| p.items.iter().any(|i| i.path == it.path)).map(|p| p.project.clone()));
    let whose = project.as_deref().and_then(|id| app::find_project(ui, id)).map(|p| format!("{}'s", p.name)).unwrap_or_else(|| "the".into());
    let done = format!("Removed {whose} worktree for {branch}, at {}. The branch is still there.", fmt::tildify(&it.path));
    dialogs::confirm(ui, &format!("Remove the worktree for {branch}?"), &body, &[(cmd, size(it.bytes))], "Remove worktree", move |ui| {
        let p = path.clone();
        let project = project.clone();
        let done = done.clone();
        app::bg(
            ui,
            move |src| src.remove_worktree(&p, false),
            move |ui, r| {
                app::toast(ui, if r.ok { &done } else { &r.message }, !r.ok);
                after_action(ui, project.as_deref());
            },
        );
    });
}

/// After a Clear or a Remove: fetch the overview and redraw the panel even if its own signature
/// hasn't changed (the disk numbers aren't part of it).
fn after_action(ui: &Ui, project: Option<&str>) {
    app::refresh(ui);
    let ui = ui.clone();
    let project = project.map(String::from);
    glib::timeout_add_local_once(std::time::Duration::from_millis(900), move || {
        let open = ui.st.borrow().drawer.clone();
        if project.is_some() && open == project.clone().map(Drawer::Project) {
            ui.st.borrow_mut().panel_drawn = None;
            crate::panel::render(&ui);
        }
    });
}

// ---------------------------------------------------------------- the machine's list

/// "Disk on this machine": free space, the projects by size, and everything outside them.
pub fn machine(ui: &Ui) {
    ensure_css();
    let r = report(&ui.st.borrow()).cloned();
    let card = dialogs::card(720);
    card.append(&w::label("Disk on this machine", &["h2"]));
    let Some(r) = r else {
        card.append(&w::wrap("Wake hasn't finished measuring yet. It measures in the background at low priority; this fills in when it's done.", &["plain"]));
        close_row(ui, &card);
        dialogs::show(ui, &card, None);
        return;
    };
    if let Some(m) = &r.machine {
        card.append(&w::wrap(
            &format!("{} free of {} on {}. {} could be rebuilt: build output and caches, which come back by themselves.", size(m.home_free), size(m.home_total), m.home_mount, size(r.rebuildable)),
            &["plain"],
        ));
        if let Some(n) = &m.root_note {
            card.append(&w::wrap(n, &["meta"]));
        }
    }
    let body = w::vbox(4, &[]);
    // Projects, biggest first: one line each, Enter opens its Disk section.
    let mut projects: Vec<&ProjectDisk> = r.projects.iter().filter(|p| p.bytes > 0).collect();
    projects.sort_by_key(|p| std::cmp::Reverse(p.bytes));
    if !projects.is_empty() {
        body.append(&w::sec("Biggest projects"));
        for pd in projects.iter().take(5) {
            let name = app::find_project(ui, &pd.project).map_or(pd.project.clone(), |p| p.name);
            let btn = gtk::Button::new();
            btn.add_css_class("srow");
            let row = w::hbox(10, &["disk-proj"]);
            row.append(&w::clip(&name, &[]));
            let words = if pd.rebuildable >= 1 << 27 { format!("{} · {} rebuildable", size(pd.bytes), size(pd.rebuildable)) } else { size(pd.bytes) };
            row.append(&w::label(&words, &["disk-size"]));
            btn.set_child(Some(&row));
            btn.set_tooltip_text(Some("Open its Disk section"));
            let (ui2, id) = (ui.clone(), pd.project.clone());
            btn.connect_clicked(move |_| {
                dialogs::close(&ui2);
                open_project_disk(&ui2, &id);
            });
            body.append(&btn);
        }
        if projects.len() > 5 {
            body.append(&w::wrap(&format!("{} more: sort Projects by DISK to see them all.", projects.len() - 5), &["meta"]));
        }
    }
    if let Some(m) = &r.machine
        && !m.items.is_empty()
    {
        body.append(&w::sec("Outside projects"));
        for it in &m.items {
            body.append(&item_widget(ui, it, None, ""));
        }
    }
    let scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).min_content_height(200).max_content_height(470).propagate_natural_height(true).child(&body).build();
    scroll.add_css_class("disk-scroll");
    card.append(&scroll);
    let when = match &r.measuring {
        Some(m) => format!("{m}…"),
        None => format!(
            "Measured {}, at low priority; again every hour. Wake never deletes conversations, containers, apps or toolchains: for those it shows the command.",
            fmt::ago(r.measured_at.as_deref())
        ),
    };
    card.append(&w::wrap(&when, &["meta"]));
    let row = w::hbox(8, &["row"]);
    row.append(&w::spacer());
    let ui2 = ui.clone();
    row.append(&w::button("Measure again", None, &["ghost"], move || {
        app::bg(&ui2, |src| src.measure_disk(None), |_, _| {});
        app::toast(&ui2, "Measuring again, in the background.", false);
    }));
    let ui2 = ui.clone();
    let close = w::button("Close", Some("Esc"), &["ghost"], move || dialogs::close(&ui2));
    row.append(&close);
    card.append(&row);
    dialogs::show(ui, &card, None);
    close.grab_focus();
}

fn close_row(ui: &Ui, card: &gtk::Box) {
    let row = w::hbox(8, &["row"]);
    row.append(&w::spacer());
    let ui2 = ui.clone();
    row.append(&w::button("Close", Some("Esc"), &["ghost"], move || dialogs::close(&ui2)));
    card.append(&row);
}

/// Opens a project's panel with its Disk section open.
pub fn open_project_disk(ui: &Ui, id: &str) {
    if ui.st.borrow().view != View::Projects {
        app::set_view(ui, View::Projects);
    }
    crate::panel::open_project(ui, id, false);
    ui.st.borrow_mut().panel_open.insert("disk");
    ui.st.borrow_mut().panel_drawn = None;
    crate::panel::render(ui);
}

// ---------------------------------------------------------------- attribution

fn attribution<'a>(st: &'a AppState, session: &str) -> Option<&'a Attribution> {
    st.ov.as_ref()?.attribution.as_ref()?.iter().find(|a| a.session == session)
}

/// The project as a row or the panel should say it: "probably night-shift" for a guess.
/// disk.rs hook target (in place of `views::project_label`).
pub fn project_words(ui: &Ui, s: &SessionSummary) -> String {
    let st = ui.st.borrow();
    match attribution(&st, &s.id) {
        Some(a) if a.guessed() => format!("probably {}", s.project_name.as_deref().unwrap_or("?")),
        Some(a) if a.how == "none" => a.words.clone(),
        _ => crate::views::project_label(s.project_name.as_deref()),
    }
}

/// The project as the session panel says it, with how Wake knows when it isn't plain: "probably
/// Skyhook (it changed files there)", "Skyhook (you set it)". disk.rs hook target.
pub fn project_reason(ui: &Ui, s: &SessionSummary) -> String {
    match attribution(&ui.st.borrow(), &s.id) {
        Some(a) => a.words.clone(),
        None => crate::views::project_label(s.project_name.as_deref()),
    }
}

/// The session `m` sets the project of: the open panel's, else the selected row's (an agent's
/// row in Agents too).
fn session_for_key(ui: &Ui) -> Option<String> {
    if let Some(id) = app::selected_session(ui) {
        return Some(id);
    }
    let st = ui.st.borrow();
    if st.view == View::Agents {
        return crate::agents::selected_proc(&st)?.session_id.clone();
    }
    None
}

/// `m`: Set project. disk.rs hook target (app.rs `on_key`, beside behind's).
pub fn on_key(ui: &Ui, key: gdk::Key, mods: gdk::ModifierType) -> Option<glib::Propagation> {
    if key.to_lower() != gdk::Key::m || mods.intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK) {
        return None;
    }
    let view = ui.st.borrow().view;
    let panel_session = matches!(ui.st.borrow().drawer, Some(Drawer::Session(_)));
    if !(panel_session || matches!(view, View::Monitor | View::History | View::Agents)) {
        return None;
    }
    let id = session_for_key(ui)?;
    set_project(ui, &id);
    Some(glib::Propagation::Stop)
}

/// "Which project is this session?": every tracked project, "No project", and (when the person
/// set one) "Let Wake decide".
pub fn set_project(ui: &Ui, session: &str) {
    let Some(s) = app::find_session(ui, session) else { return };
    let (projects, words, set) = {
        let st = ui.st.borrow();
        let ps: Vec<ProjectSummary> = st.ov.as_ref().map(|o| o.projects.iter().filter(|p| !p.removed).cloned().collect()).unwrap_or_default();
        let a = attribution(&st, session);
        (ps, a.map(|a| a.words.clone()), a.is_some_and(|a| a.how == "set"))
    };
    let card = dialogs::card(460);
    card.append(&w::wrap(&format!("Which project is \"{}\"?", s.title), &["h2"]));
    let now = words.unwrap_or_else(|| crate::views::project_label(s.project_name.as_deref()));
    card.append(&w::wrap(&format!("Wake has it as {now}. Your answer is kept in Wake's own store; nothing else changes."), &["plain"]));
    let list = w::vbox(2, &[]);
    let mut first: Option<gtk::Button> = None;
    let pick = |label: &str, choice: Option<Option<String>>, dot: Option<&ProjectSummary>, current: bool| {
        let btn = gtk::Button::new();
        btn.add_css_class("srow");
        let row = w::hbox(8, &[]);
        if let Some(p) = dot {
            row.append(&crate::views::project_dot(p));
        }
        row.append(&w::clip(label, &[]));
        if current {
            row.append(&w::label("now", &["meta"]));
        }
        btn.set_child(Some(&row));
        let (ui2, sid, label2) = (ui.clone(), session.to_string(), label.to_string());
        btn.connect_clicked(move |_| {
            dialogs::close(&ui2);
            let (sid2, c) = (sid.clone(), choice.clone());
            let label3 = label2.clone();
            app::bg(
                &ui2,
                move |src| src.set_session_project(&sid2, c.as_ref().map(|o| o.as_deref())),
                move |ui, r| match r {
                    Ok(()) => {
                        app::toast(ui, &format!("Now: {label3}."), false);
                        app::refresh(ui);
                    }
                    Err(e) => app::toast(ui, &e, true),
                },
            );
        });
        btn
    };
    for p in &projects {
        let b = pick(&p.name, Some(Some(p.id.clone())), Some(p), s.project.as_deref() == Some(p.id.as_str()));
        if first.is_none() || s.project.as_deref() == Some(p.id.as_str()) {
            first = Some(b.clone());
        }
        list.append(&b);
    }
    list.append(&pick("No project", Some(None), None, s.project.is_none()));
    if set {
        list.append(&pick("Let Wake decide again", None, None, false));
    }
    let scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).max_content_height(380).propagate_natural_height(true).child(&list).build();
    card.append(&scroll);
    close_row(ui, &card);
    dialogs::show(ui, &card, None);
    if let Some(b) = first {
        b.grab_focus();
    }
}

/// A rail project's tooltip on hover: memory now and size on disk. disk.rs hook target.
pub fn rail_tip(ui: &Ui, wd: &impl IsA<gtk::Widget>, id: &str) {
    let weak = std::rc::Rc::downgrade(ui);
    let id = id.to_string();
    let base = wd.tooltip_text().map(|t| t.to_string());
    wd.set_has_tooltip(true);
    wd.connect_query_tooltip(move |_, _, _, _, tip| {
        let Some(ui) = weak.upgrade() else { return false };
        let st = ui.st.borrow();
        let mut lines: Vec<String> = base.iter().cloned().collect();
        if let Some(m) = mem_tip(&st, &id) {
            lines.push(m);
        }
        if let Some(d) = report(&st).and_then(|r| r.project(&id)) {
            lines.push(format!("{} on disk{}", size(d.bytes), if d.rebuildable >= 1 << 27 { format!(", {} rebuildable", size(d.rebuildable)) } else { String::new() }));
        }
        if lines.is_empty() {
            return false;
        }
        tip.set_text(Some(&lines.join("\n")));
        true
    });
}

// ---------------------------------------------------------------- --show, for screenshots

/// `--show disk[:project]` (its Disk section), `disk-machine`, `disk-clear[:project]`,
/// `disk-remove[:project]`, `set-project[:session]`. disk.rs hook target (app.rs `apply_show`).
pub fn show(ui: &Ui, what: &str, arg: Option<String>) {
    let biggest = || -> Option<String> {
        let st = ui.st.borrow();
        let r = report(&st)?;
        let mut v: Vec<&ProjectDisk> = r.projects.iter().collect();
        v.sort_by_key(|p| std::cmp::Reverse(p.bytes));
        v.first().map(|p| p.project.clone())
    };
    // An id or a name, any case.
    let project = arg
        .clone()
        .map(|a| ui.st.borrow().ov.as_ref().and_then(|o| o.projects.iter().find(|p| p.id.eq_ignore_ascii_case(&a) || p.name.eq_ignore_ascii_case(&a)).map(|p| p.id.clone())).unwrap_or(a))
        .or_else(biggest);
    match what {
        "disk-machine" => {
            app::set_view(ui, View::Projects);
            machine(ui);
        }
        "set-project" => {
            let id = arg.or_else(|| ui.st.borrow().ov.as_ref().and_then(|o| o.sessions.iter().find(|s| s.project.is_none()).map(|s| s.id.clone())));
            if let Some(id) = id {
                set_project(ui, &id);
            }
        }
        _ => {
            let Some(p) = project else { return };
            open_project_disk(ui, &p);
            if what == "disk" {
                return;
            }
            let item = {
                let st = ui.st.borrow();
                report(&st).and_then(|r| r.project(&p)).and_then(|d| {
                    d.items.iter().find(|i| if what == "disk-remove" { i.action == Some(DiskAction::RemoveWorktree) } else { i.action == Some(DiskAction::Clear) && i.refused.is_none() }).cloned()
                })
            };
            let name = app::find_project(ui, &p).map_or(p.clone(), |x| x.name);
            match (what, item) {
                ("disk-remove", Some(it)) => confirm_remove(ui, &it),
                (_, Some(it)) => confirm_clear(ui, &it, Some(&p), &name),
                _ => {}
            }
        }
    }
}

// ---------------------------------------------------------------- demo data

const H: f64 = 3600.0;
const GB: u64 = 1 << 30;
const MB: u64 = 1 << 20;

fn ago_iso(secs: f64) -> Option<String> {
    Some(wake_core::util::iso_from_epoch(wake_core::util::now() - secs))
}

#[allow(clippy::too_many_arguments)]
fn item(path: &str, kind: DiskKind, name: &str, bytes: u64, used: f64, safety: DiskSafety, cost: &str, action: Option<DiskAction>) -> DiskItem {
    DiskItem {
        path: path.into(),
        kind,
        name: name.into(),
        bytes,
        partial: false,
        last_used: ago_iso(used),
        safety,
        cost: cost.into(),
        action,
        command: None,
        command_note: None,
        worktree: None,
        within: None,
        refused: None,
    }
}

fn cmd(mut i: DiskItem, c: &str, note: &str) -> DiskItem {
    i.command = Some(c.into());
    i.command_note = Some(note.into());
    i
}

const P: &str = "/home/sam/Projects";
const CONV: &str = "Deleting one loses that session's history and resume; Wake keeps its summary. Claude deletes conversations older than its cleanup period (30 days unless cleanupPeriodDays in ~/.claude/settings.json says otherwise) by itself.";
const REBUILD: &str = "Rebuilds on the next build (a few minutes).";

/// Made-up measurements, shaped like a developer laptop with a few busy projects.
pub fn demo_report() -> DiskReport {
    use DiskAction::*;
    use DiskKind::*;
    use DiskSafety::*;
    let mut slip = vec![
        item(
            &format!("{P}/tideline-desktop/target"),
            BuildOutput,
            "Rust build output",
            15 * GB + 300 * MB,
            2.0 * H,
            Safe,
            "Rebuilds on the next build: a long one on this computer (tens of minutes, memory-heavy).",
            Some(Clear),
        ),
        {
            let mut i = item(
                &format!("{P}/tideline-desktop/.claude/worktrees/agent-ace0489"),
                Worktree,
                "Agent worktree for branch worktree-agent-ace0489 (not merged)",
                3 * GB + 600 * MB,
                3.0 * H,
                Check,
                "Its branch worktree-agent-ace0489 has commits main doesn't: removing it could lose work. 3.6 GB of it is build output, listed below.",
                Some(OpenFolder),
            );
            i.worktree = Some(WorktreeInfo {
                repo: format!("{P}/tideline-desktop"),
                branch: Some("worktree-agent-ace0489".into()),
                default_branch: Some("main".into()),
                merged: Some(false),
                dirty_files: Some(0),
                in_use: None,
                made_by: Some("2b8f0d11-slip-scope".into()),
                made_by_title: Some("scope-helper-rows".into()),
            });
            i.refused = Some("Its branch isn't merged.".into());
            i
        },
        {
            let mut i = item(
                &format!("{P}/tideline-desktop/.claude/worktrees/agent-ace0489/target"),
                BuildOutput,
                "Rust build output in worktree agent-ace0489",
                3 * GB + 580 * MB,
                3.0 * H,
                Safe,
                REBUILD,
                Some(Clear),
            );
            i.within = Some(format!("{P}/tideline-desktop/.claude/worktrees/agent-ace0489"));
            i
        },
        {
            let mut i = item(
                &format!("{P}/tideline-desktop/.claude/worktrees/focus-card-tidy"),
                Worktree,
                "Agent worktree for branch focus-card-tidy (merged)",
                412 * MB,
                26.0 * H,
                Safe,
                "Its branch focus-card-tidy is in main; removing it deletes only this copy (the branch stays). 380 MB of it is build output, listed below.",
                Some(RemoveWorktree),
            );
            i.command = Some("git -C ~/Projects/tideline-desktop worktree remove .claude/worktrees/focus-card-tidy".into());
            i.worktree = Some(WorktreeInfo {
                repo: format!("{P}/tideline-desktop"),
                branch: Some("focus-card-tidy".into()),
                default_branch: Some("main".into()),
                merged: Some(true),
                dirty_files: Some(0),
                in_use: None,
                made_by: Some("0f9e8d7c-slip-unknown".into()),
                made_by_title: Some("unfinished-work-check".into()),
            });
            i
        },
        {
            let mut i = item(
                &format!("{P}/tideline-desktop/.claude/worktrees/focus-card-tidy/target"),
                BuildOutput,
                "Rust build output in worktree focus-card-tidy",
                380 * MB,
                26.0 * H,
                Safe,
                REBUILD,
                Some(Clear),
            );
            i.within = Some(format!("{P}/tideline-desktop/.claude/worktrees/focus-card-tidy"));
            i
        },
        item(
            "/home/sam/.cache/tideline-smoke",
            Cache,
            "Tideline's smoke-test clips and screens (~/.cache/tideline-smoke)",
            954 * MB,
            5.0 * H,
            Safe,
            "Test or tool output: the next run makes it again.",
            Some(Clear),
        ),
        item("/home/sam/.claude/projects", Transcripts, "Claude conversations here (31 sessions)", 420 * MB, 60.0, Keep, CONV, None),
        item(&format!("{P}/tideline-desktop/.git"), Git, "Git history (.git)", 310 * MB, 2.0 * H, Keep, "The project's history. Never delete it.", None),
    ];
    // The demo's scope-helper-rows session is working here.
    for i in slip.iter_mut().filter(|i| i.action == Some(Clear) && !i.path.contains(".cache")) {
        i.refused = Some("An agent in this project is working; it may be building into this. Try when it's idle.".into());
    }
    let phase = vec![
        item(
            &format!("{P}/Skyhook/archive"),
            Archive,
            "Archive or exported builds (archive)",
            3 * GB + 100 * MB,
            40.0 * 24.0 * H,
            Keep,
            "May be the only copy of an old version or a shipped build. Wake won't delete it.",
            Some(OpenFolder),
        ),
        item(
            "/home/sam/.cache/skyhook-probe",
            Cache,
            "Skyhook's probe recordings (~/.cache/skyhook-probe)",
            2 * GB + 700 * MB,
            2.0 * 24.0 * H,
            Safe,
            "Test or tool output: the next run makes it again.",
            Some(Clear),
        ),
        item(
            &format!("{P}/Skyhook/Builds"),
            Archive,
            "Archive or exported builds (Builds)",
            2 * GB + 100 * MB,
            9.0 * 24.0 * H,
            Keep,
            "May be the only copy of an old version or a shipped build. Wake won't delete it.",
            Some(OpenFolder),
        ),
        item(
            &format!("{P}/Skyhook/game/.godot"),
            BuildOutput,
            "Godot import cache in game",
            640 * MB,
            2.0 * 24.0 * H,
            Safe,
            "Godot imports the assets again next time the project opens.",
            Some(Clear),
        ),
        item("/home/sam/.claude/projects", Transcripts, "Claude conversations here (12 sessions)", 180 * MB, 4.0 * 24.0 * H, Keep, CONV, None),
    ];
    let school = vec![
        item("/home/sam/.claude/projects", Transcripts, "Claude conversations here (22 sessions)", 610 * MB, 40.0, Keep, CONV, None),
        item(
            &format!("{P}/Gradebook/node_modules"),
            BuildOutput,
            "JavaScript packages (node_modules)",
            420 * MB,
            6.0 * 24.0 * H,
            Safe,
            "Comes back with npm install (needs the network).",
            Some(Clear),
        ),
        item(&format!("{P}/Gradebook/tmp"), BuildOutput, "Rails temporary files (tmp)", 181 * MB, 11.0 * H, Safe, "Rails makes it again; restart a running dev server afterwards.", Some(Clear)),
        cmd(
            item(
                "/home/sam/.claude/jobs",
                JobFiles,
                "Background job folders here (4)",
                96 * MB,
                11.0 * H,
                Check,
                "Each job's tmp folder holds its work files, and hand-back files (briefs, reports) live there too: look before deleting.",
                None,
            ),
            "du -sh ~/.claude/jobs/{7c1e9a40,3c3c1b2a,5e0d9c61,a83f27d0}",
            "shows what's in them, biggest first; changes nothing",
        ),
    ];
    let west = vec![item(
        &format!("{P}/waypoint/.venv"),
        BuildOutput,
        "Python environment (.venv)",
        380 * MB,
        5.0 * 24.0 * H,
        Safe,
        "Comes back by installing the packages again (needs the network).",
        Some(Clear),
    )];
    let glim = vec![item(
        &format!("{P}/fernlight/node_modules"),
        BuildOutput,
        "JavaScript packages (node_modules)",
        210 * MB,
        3.0 * 24.0 * H,
        Safe,
        "Comes back with npm install (needs the network).",
        Some(Clear),
    )];
    let pd = |id: &str, extra: u64, items: Vec<DiskItem>| {
        let top: u64 = items.iter().filter(|i| i.within.is_none()).map(|i| i.bytes).sum();
        let rebuildable = items.iter().filter(|i| matches!(i.kind, BuildOutput | Cache) && i.safety == Safe).map(|i| i.bytes).sum();
        ProjectDisk { project: id.into(), bytes: top + extra, rebuildable, items, measured_at: ago_iso(12.0 * 60.0), partial: false }
    };
    let projects = vec![pd("tideline-desktop", 210 * MB, slip), pd("Skyhook", 380 * MB, phase), pd("Gradebook", 260 * MB, school), pd("waypoint", 40 * MB, west), pd("fernlight", 18 * MB, glim)];
    let home = "/home/sam";
    let machine_items = vec![
        cmd(
            item("/var/lib/flatpak", System, "Flatpak apps and runtimes (system)", 20 * GB, 9.0 * 24.0 * H, Keep, "Installed apps and what they run on.", None),
            "flatpak uninstall --unused",
            "removes runtimes no installed app uses; asks first",
        ),
        cmd(
            item(&format!("{home}/.local/share/flatpak"), System, "Flatpak apps and runtimes (yours)", 11 * GB, 3.0 * 24.0 * H, Keep, "Installed apps and what they run on.", None),
            "flatpak uninstall --unused",
            "removes runtimes no installed app uses; asks first",
        ),
        cmd(
            item(
                &format!("{home}/.local/share/containers"),
                System,
                "Containers: devbox (running), gradebook-postgres (running), docs-preview (stopped 6 days), old-toolbox (stopped 2 days)",
                6 * GB + 800 * MB,
                0.0,
                Keep,
                "3.7 GB is in stopped containers. Wake never removes a container.",
                None,
            ),
            "podman ps -a",
            "lists them; a stopped distrobox still holds what you installed in it, so remove one only if you're sure (distrobox rm <name>)",
        ),
        cmd(
            item(&format!("{home}/.local/share/containers"), System, "Container images", 6 * GB + 600 * MB, 0.0, Keep, "1.5 GB of it nothing uses.", None),
            "podman image prune",
            "removes images nothing uses and nothing names (dangling); asks first",
        ),
        cmd(
            item(&format!("{home}/.local/share/containers"), System, "Container volumes", 2 * GB + 300 * MB, 0.0, Keep, "10 MB nothing uses.", None),
            "podman volume ls",
            "lists them; volumes hold data such as databases",
        ),
        item(&format!("{home}/.claude/projects"), Transcripts, "Claude conversations with no project (96 sessions)", 2 * GB + 100 * MB, 2.0 * H, Keep, CONV, None),
        cmd(
            item(&format!("{home}/.rustup"), System, "Rust toolchains (rustup)", GB + 600 * MB, 6.0 * 24.0 * H, Keep, "Every Rust project builds with these.", None),
            "rustup toolchain list",
            "lists them; rustup toolchain uninstall <name> removes one you no longer use",
        ),
        item(
            &format!("{home}/.cargo/registry"),
            Cache,
            "Downloaded Rust crates (cargo registry)",
            GB + 300 * MB,
            3.0 * H,
            Safe,
            "Cargo downloads them again on the next build (needs the network).",
            Some(Clear),
        ),
        cmd(
            item(
                &format!("{home}/.local/share/Trash"),
                System,
                "The Trash",
                GB + 200 * MB,
                20.0 * 24.0 * H,
                Check,
                "What you (or Wake's Clear) moved there: the space comes back only when it is emptied.",
                None,
            ),
            "gio trash --empty",
            "empties the Trash for good",
        ),
        item(&format!("{home}/.codex"), Transcripts, "Codex conversations (~/.codex)", 606 * MB, 8.0 * 24.0 * H, Keep, "Deleting loses those sessions' history.", None),
        cmd(
            item(
                &format!("{home}/.claude/jobs"),
                JobFiles,
                "Background job folders with no project (38)",
                512 * MB,
                2.0 * H,
                Check,
                "Each job's tmp folder holds its work files, and hand-back files (briefs, reports) live there too: look before deleting.",
                None,
            ),
            "du -sh ~/.claude/jobs/{4be1a7d2,9c03f5e8,d17a6b40,3f8e2c95,60b4d1aa,e2975c3f}",
            "shows what's in them, biggest first; changes nothing",
        ),
        item(
            &format!("{home}/.cache/supertuxkart"),
            Cache,
            "supertuxkart cache (~/.cache/supertuxkart)",
            345 * MB,
            30.0 * 24.0 * H,
            Safe,
            "A cache: the app fills it again when it needs it.",
            Some(Clear),
        ),
        item(&format!("{home}/.cache/Homebrew"), Cache, "Homebrew's downloads (~/.cache/Homebrew)", 289 * MB, 12.0 * 24.0 * H, Safe, "A cache: the app fills it again when it needs it.", Some(Clear)),
    ];
    let mrebuild: u64 = machine_items.iter().filter(|i| matches!(i.kind, BuildOutput | Cache)).map(|i| i.bytes).sum();
    let rebuildable = projects.iter().map(|p| p.rebuildable).sum::<u64>() + mrebuild;
    DiskReport {
        measured_at: ago_iso(12.0 * 60.0),
        measuring: None,
        projects,
        machine: Some(MachineDisk {
            home_mount: "/home".into(),
            home_total: 244 * GB,
            home_free: 86 * GB,
            root_note: Some("/ shows as full: on an atomic system such as Bazzite, that is the read-only system image, always full by design. Nothing there needs cleaning.".into()),
            items: machine_items,
            rebuildable: mrebuild,
        }),
        rebuildable,
    }
}

/// The demo's processes added up the way the real ones are.
pub fn demo_resources(procs: &[AgentProc], sessions: &[SessionSummary]) -> Resources {
    let top: u64 = procs.iter().filter(|p| !procs.iter().any(|q| q.children.contains(&p.pid))).map(|p| p.rss_bytes).sum();
    let ov = ProcOverview { procs: procs.to_vec(), total_rss_bytes: top, ..Default::default() };
    let project_of = |sid: &str| sessions.iter().find(|s| s.id == sid).and_then(|s| s.project.clone());
    wake_core::procs::resources(&ov, &project_of)
}

/// The demo's guesses: its background job started in ~ that changed files in Skyhook, and the
/// home session with no project.
pub fn demo_attribution(sessions: &[SessionSummary]) -> Vec<Attribution> {
    let mut out = Vec::new();
    for s in sessions {
        if s.id.starts_with("91aa4e7c") && s.project.as_deref() == Some("Skyhook") {
            out.push(Attribution { session: s.id.clone(), project: s.project.clone(), how: "files".into(), words: "probably Skyhook (it changed files there)".into(), started_at_home: true });
        } else if s.project.is_none() {
            out.push(Attribution { session: s.id.clone(), project: None, how: "none".into(), words: "no project".into(), started_at_home: true });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_totals_add_up_and_nothing_risky_offers_a_delete() {
        let r = demo_report();
        for p in &r.projects {
            for i in &p.items {
                if matches!(i.kind, DiskKind::Archive | DiskKind::Folder | DiskKind::Git | DiskKind::Transcripts | DiskKind::JobFiles | DiskKind::System) {
                    assert!(!matches!(i.action, Some(DiskAction::Clear) | Some(DiskAction::RemoveWorktree)), "{}", i.name);
                }
            }
            assert!(p.bytes >= p.rebuildable, "{}", p.project);
        }
        let slip = r.project("tideline-desktop").unwrap();
        assert_eq!(size(slip.bytes), "21 GB");
        assert!(r.machine.as_ref().unwrap().items.iter().all(|i| i.kind != DiskKind::System || i.command.is_some()));
    }
}
