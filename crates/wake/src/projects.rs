//! The Projects view: a card or row per project, in groups you can name, order and drag
//! projects between, with each project's menu (pause, importance, group, stop tracking).

use gtk4::{self as gtk, gdk, glib, prelude::*};
use wake_core::*;

use crate::{
    app::{self, Ui, View},
    fmt, prefs,
    views::{amber, begin, by_project, card_grid, dim, finish, grid_row, list_row, live_text, render_chips, render_view, section_row},
    w,
};

/// A project's git loose ends in plain words (Pango markup). "not on GitHub" only when its remote
/// is GitHub; a repo with no remote at all is a way of working, said once and quietly; and "all
/// pushed" isn't claimed when Wake couldn't ask the remote.
pub fn git_words(p: &ProjectSummary) -> String {
    let g = &p.git;
    let unpushed = if g.github { "not on GitHub" } else { "not pushed" };
    let mut out = Vec::new();
    if g.ahead_total > 0 {
        out.push(amber(&format!("{} {unpushed}", fmt::pl(g.ahead_total, "commit", "commits"))));
    }
    if g.dirty_files > 0 {
        out.push(amber(&format!("{} edited, not committed", fmt::pl(g.dirty_files, "file", "files"))));
    }
    if g.no_upstream > 0 {
        out.push(format!("{} {unpushed}", fmt::pl(g.no_upstream, "branch", "branches")));
    }
    if g.no_remote > 0 {
        out.push(dim("only on this computer (no remote)"));
    }
    match (out.is_empty(), &p.remote_problem) {
        // On two lines, like the other loose ends, so the narrow column shows all of it.
        (true, Some(why)) => dim(&glib::markup_escape_text(&why.replacen(" (", "\n(", 1))),
        (true, None) => dim("all pushed"),
        _ => out.join("\n"),
    }
}

pub fn render(ui: &Ui) {
    let (list, adding, compact) = {
        let st = ui.st.borrow();
        let Some(ov) = &st.ov else { return };
        let mut list: Vec<ProjectSummary> = ov.projects.iter().filter(|p| st.show_removed || !p.removed).cloned().collect();
        list.sort_by(by_project);
        if let Some((k, rev)) = prefs::sort_of(View::Projects) {
            crate::disk::sort_projects(&mut list, &k, rev, by_project, ov); // disk.rs hook: disk and memory sort
        }
        (list, st.adding, app::compact(&st) || st.lay.tight)
    };
    // Never rebuild under someone typing a folder into "Track a folder", nor under a project being
    // dragged (it would drop the drag); it draws again when the drag ends.
    let vw = &ui.w.views[&View::Projects];
    if (adding && (app::typing_in(ui, &vw.top) || app::typing_in(ui, &vw.list))) || ui.st.borrow().dragging {
        return;
    }
    let mut sig_list = list.clone();
    for p in &mut sig_list {
        p.last_at = None;
    }
    let groups = ui.st.borrow().ov.as_ref().map(|ov| ov.groups.clone()).unwrap_or_default();
    let (layout, grouping) = (prefs::projects_layout(), prefs::projects_group());
    let sig = format!("{adding}{compact}{}{}{:?}{layout}{grouping}{groups:?}", serde_json::to_string(&sig_list).unwrap_or_default(), crate::behind::sig(ui), prefs::sort_of(View::Projects));
    let Some(had_focus) = begin(ui, View::Projects, &sig) else { return };
    let vw = &ui.w.views[&View::Projects];
    w::clear(&vw.top);
    if adding {
        let row = w::hbox(8, &["addrow"]);
        row.append(&w::label("Folder", &["meta"]));
        let entry = gtk::Entry::builder().placeholder_text("~/Projects/something").hexpand(true).build();
        row.append(&entry);
        // Or pick it in the desktop's folder chooser: it fills the box, and Track it adds it.
        let (ui2, entry2) = (ui.clone(), entry.clone());
        row.append(&w::button("Choose…", None, &["ghost"], move || {
            let d = gtk::FileDialog::builder().title("Choose a folder to track").modal(true).build();
            let entry = entry2.clone();
            d.select_folder(Some(&ui2.window), gtk::gio::Cancellable::NONE, move |r| {
                if let Some(path) = r.ok().and_then(|f| f.path()) {
                    entry.set_text(&path.to_string_lossy());
                    entry.grab_focus();
                }
            });
        }));
        let err = w::label("", &["err"]);
        let add = {
            let ui = ui.clone();
            let entry = entry.clone();
            let err = err.clone();
            move || {
                let path = entry.text().trim().to_string();
                if path.is_empty() {
                    return;
                }
                let err = err.clone();
                app::bg(
                    &ui,
                    move |src| src.add_project(&path),
                    move |ui, r| match r {
                        Ok(p) => {
                            app::toast(ui, &format!("Tracking {}.", p.name), false);
                            ui.st.borrow_mut().adding = false;
                            app::refresh(ui);
                            ui.st.borrow_mut().drawn.remove(&View::Projects);
                            render(ui);
                        }
                        Err(e) => err.set_text(&e),
                    },
                );
            }
        };
        let add2 = add.clone();
        entry.connect_activate(move |_| add2());
        row.append(&w::button("Track it", None, &["primary"], add));
        let ui2 = ui.clone();
        row.append(&w::button("Cancel", None, &["ghost"], move || {
            ui2.st.borrow_mut().adding = false;
            ui2.st.borrow_mut().drawn.remove(&View::Projects);
            render(&ui2);
        }));
        row.append(&err);
        vw.top.append(&row);
        entry.grab_focus();
    }
    if list.is_empty() {
        let e = w::empty("No projects yet.", "Wake finds git folders under ~/Projects and the folders your sessions ran in. Track any other folder with the button above, or start a new project.");
        let b = crate::newproject::button(ui);
        b.set_halign(gtk::Align::Center);
        b.set_margin_top(10);
        e.append(&b);
        vw.empty.append(&e);
        return;
    }
    // A grid of cards, one per project, in its colour: the next step first, then what is open and
    // what is loose in git. Arrows, Enter, p and the panel work on it as on any list.
    // In sections (the person's groups, or by activity), each a grid of cards or a boxed list.
    // In the person's groups, a project can be dragged from one group to another (or out of
    // them), and a group with nothing in it yet is a place to drop one.
    let sections = project_sections(&list, &grouping, &groups);
    let headed = sections.len() > 1 || sections.first().is_some_and(|s| !s.title.is_empty());
    for sec in &sections {
        let mut zone: Vec<gtk::Widget> = Vec::new();
        if headed {
            let h = group_heading(ui, sec);
            vw.list.append(&h);
            zone.push(h.upcast());
        }
        if sec.items.is_empty() {
            let r = drop_hint_row(matches!(sec.target, Some(None)));
            vw.list.append(&r);
            zone.push(r.upcast());
        } else if layout == "list" {
            let n = sec.items.len();
            for (i, p) in sec.items.iter().enumerate() {
                let r = project_item(ui, p, (i == 0, i + 1 == n));
                if sec.target.is_some() {
                    drag_project(ui, &r, &p.id);
                }
                vw.list.append(&r);
                zone.push(r.upcast());
            }
        } else {
            let grid = card_grid(ui, "pgrid", 4);
            if headed {
                grid.add_css_class("ingroup");
            }
            for p in &sec.items {
                let c = project_card(ui, p);
                if sec.target.is_some() {
                    drag_project(ui, &c, &p.id);
                }
                grid.insert(&c, -1);
            }
            let r = grid_row(&grid);
            vw.list.append(&r);
            zone.push(r.upcast());
        }
        if let Some(target) = &sec.target {
            drop_into(ui, &zone, target.as_deref());
        }
    }
    finish(ui, View::Projects, had_focus);
}

/// A section of the Projects view: its heading, its projects, and, in the person's groups, the
/// group a project dropped on it goes into (`Some(None)`: no group).
struct Section<'a> {
    title: String,
    items: Vec<&'a ProjectSummary>,
    target: Option<Option<String>>,
}

/// What a dragged project carries: its id, marked as Wake's so text dragged in from elsewhere is
/// never taken for a project. A dragged group carries its name.
const DRAG_PREFIX: &str = "wake-project:";
const GROUP_PREFIX: &str = "wake-group:";

/// Lets a project's row or card be dragged to another group.
fn drag_project(ui: &Ui, w: &impl IsA<gtk::Widget>, id: &str) {
    drag_payload(ui, w, format!("{DRAG_PREFIX}{id}"));
}

/// Lets a group's heading be dragged onto another group, to put it in that group's place.
fn drag_group(ui: &Ui, w: &impl IsA<gtk::Widget>, name: &str) {
    w.set_cursor_from_name(Some("grab"));
    w.set_tooltip_text(Some("Drag to put the group in another place"));
    drag_payload(ui, w, format!("{GROUP_PREFIX}{name}"));
}

fn drag_payload(ui: &Ui, w: &impl IsA<gtk::Widget>, payload: String) {
    let src = gtk::DragSource::new();
    src.set_actions(gdk::DragAction::MOVE);
    let at = std::rc::Rc::new(std::cell::Cell::new((0.0, 0.0)));
    {
        let at = at.clone();
        src.connect_prepare(move |_, x, y| {
            at.set((x, y));
            Some(gdk::ContentProvider::for_value(&payload.to_value()))
        });
    }
    {
        let w = w.clone().upcast::<gtk::Widget>();
        let ui = ui.clone();
        src.connect_drag_begin(move |s, _| {
            ui.st.borrow_mut().dragging = true;
            let (x, y) = at.get();
            s.set_icon(Some(&gtk::WidgetPaintable::new(Some(&w))), x as i32, y as i32);
        });
    }
    {
        let ui = ui.clone();
        src.connect_drag_end(move |_, _, _| {
            ui.st.borrow_mut().dragging = false;
            // Whatever came in while it was held.
            let ui = ui.clone();
            glib::idle_add_local_once(move || render_view(&ui));
        });
    }
    w.add_controller(src);
}

/// Makes `zone` (a section's heading and its rows, or its grid) a place to drop a project: it goes
/// into `target` (None: no group). The whole section lights up while one is held over it.
fn drop_into(ui: &Ui, zone: &[gtk::Widget], target: Option<&str>) {
    for z in zone {
        let t = gtk::DropTarget::new(glib::Type::STRING, gdk::DragAction::MOVE);
        let (ui2, target2) = (ui.clone(), target.map(String::from));
        t.connect_drop(move |_, v, _, _| {
            let Ok(s) = v.get::<String>() else { return false };
            if let Some(id) = s.strip_prefix(DRAG_PREFIX) {
                move_to_group(&ui2, id, target2.clone());
            } else if let Some(name) = s.strip_prefix(GROUP_PREFIX) {
                // Into that group's place; onto "Not in a group", last.
                let order = group_order(&ui2);
                let to = match &target2 {
                    Some(g) => order.iter().position(|x| x == g),
                    None => order.len().checked_sub(1),
                };
                if let (Some(from), Some(to)) = (order.iter().position(|x| x == name), to) {
                    move_group(&ui2, from, to);
                }
            } else {
                return false;
            }
            true
        });
        let lit = zone.to_vec();
        t.connect_enter(move |_, _, _| {
            for w in &lit {
                w.add_css_class("dropping");
            }
            gdk::DragAction::MOVE
        });
        let lit = zone.to_vec();
        t.connect_leave(move |_| {
            for w in &lit {
                w.remove_css_class("dropping");
            }
        });
        z.add_controller(t);
    }
}

/// Puts a project in a group (None: in none), saying so; nothing if it is there already.
fn move_to_group(ui: &Ui, id: &str, group: Option<String>) {
    let Some(p) = ui.st.borrow().ov.as_ref().and_then(|ov| ov.projects.iter().find(|p| p.id == id).cloned()) else { return };
    if p.group == group {
        return;
    }
    let said = match &group {
        Some(g) => format!("{} is in {g}.", p.name),
        None => format!("{} is in no group.", p.name),
    };
    let id = id.to_string();
    app::bg(
        ui,
        move |src| src.set_project_group(&id, group.as_deref()),
        move |ui, r| match r {
            Ok(_) => {
                app::toast(ui, &said, false);
                app::refresh(ui);
            }
            Err(e) => app::toast(ui, &e, true),
        },
    );
}

/// The person's groups, in their order.
fn group_order(ui: &Ui) -> Vec<String> {
    ui.st.borrow().ov.as_ref().map(|ov| ov.groups.clone()).unwrap_or_default()
}

/// Moves the group at `from` to `to` in the person's order (it takes that place; the others make
/// room).
fn move_group(ui: &Ui, from: usize, to: usize) {
    let mut order = group_order(ui);
    if from == to || from >= order.len() || to >= order.len() {
        return;
    }
    let g = order.remove(from);
    order.insert(to, g);
    app::bg(
        ui,
        move |src| src.order_groups(&order),
        |ui, r| match r {
            Ok(()) => app::refresh(ui),
            Err(e) => app::toast(ui, &e, true),
        },
    );
}

/// A section's heading; a group's can be dragged to another place, and has a ⋯ menu to move or
/// remove the group.
fn group_heading(ui: &Ui, sec: &Section) -> gtk::ListBoxRow {
    let r = section_row(&sec.title, sec.items.len());
    if let Some(Some(name)) = &sec.target
        && let Some(b) = r.child().and_downcast::<gtk::Box>()
    {
        b.append(&w::spacer());
        b.append(&group_menu(ui, name, sec.items.len()));
        drag_group(ui, &r, name);
    }
    r
}

/// A group's ⋯: Move up, Move down, Remove group (its projects stay, in no group).
fn group_menu(ui: &Ui, name: &str, n: usize) -> gtk::MenuButton {
    let mb = gtk::MenuButton::new();
    mb.set_child(Some(&w::icon(w::icons::MORE)));
    mb.add_css_class("flat");
    mb.set_widget_name("group-more");
    mb.set_tooltip_text(Some(&format!("{name}: move or remove the group")));
    mb.update_property(&[gtk::accessible::Property::Label(&format!("{name} group options"))]);
    mb.set_valign(gtk::Align::Center);
    mb.set_focus_on_click(false);
    let pop = gtk::Popover::new();
    let items = w::vbox(2, &["moremenu"]);
    // Up and down in the person's order (or drag the heading), for the keyboard too.
    let order = group_order(ui);
    if let Some(at) = order.iter().position(|g| g == name) {
        for (label, to) in [("Move up", at.checked_sub(1)), ("Move down", Some(at + 1).filter(|t| *t < order.len()))] {
            let Some(to) = to else { continue };
            let b = gtk::Button::with_label(label);
            b.add_css_class("menuitem");
            let (ui, pop) = (ui.clone(), pop.clone());
            b.connect_clicked(move |_| {
                pop.popdown();
                move_group(&ui, at, to);
            });
            items.append(&b);
        }
    }
    let b = gtk::Button::with_label("Remove group");
    b.add_css_class("menuitem");
    b.set_tooltip_text(Some(if n == 0 { "It has no projects in it." } else { "Its projects stay, in no group." }));
    let (ui, pop2, name) = (ui.clone(), pop.clone(), name.to_string());
    b.connect_clicked(move |_| {
        pop2.popdown();
        let name2 = name.clone();
        let name3 = name.clone();
        app::bg(
            &ui,
            move |src| src.remove_group(&name2),
            move |ui, r| match r {
                Ok(0) => {
                    app::toast(ui, &format!("Removed the group {name3}."), false);
                    app::refresh(ui);
                }
                Ok(n) => {
                    let rest = if n == 1 { "Its project is now in no group.".to_string() } else { format!("Its {n} projects are now in no group.") };
                    app::toast(ui, &format!("Removed the group {name3}. {rest}"), false);
                    app::refresh(ui);
                }
                Err(e) => app::toast(ui, &e, true),
            },
        );
    });
    items.append(&b);
    pop.set_child(Some(&items));
    mb.set_popover(Some(&pop));
    mb
}

/// A group with no projects in it yet (or, `ungrouped`, every project in a group): a place to drop
/// one.
fn drop_hint_row(ungrouped: bool) -> gtk::ListBoxRow {
    let r = gtk::ListBoxRow::new();
    r.set_selectable(false);
    r.set_activatable(false);
    r.set_focusable(false);
    r.add_css_class("grouprow");
    r.add_css_class("droprow");
    let words =
        if ungrouped { "Every project is in a group. Drag one here to take it out of its group." } else { "No projects in this group yet. Drag one here, or pick the group in a project's ⋯ menu." };
    let l = w::label(words, &["dim", "drophint"]);
    l.set_wrap(true);
    l.set_xalign(0.0);
    r.set_child(Some(&l));
    r
}

/// "+ New group": a group of the person's own, empty until projects are put in it.
pub fn new_group_button(ui: &Ui) -> gtk::MenuButton {
    let mb = gtk::MenuButton::new();
    mb.set_child(Some(&gtk::Label::new(Some("+ New group"))));
    mb.add_css_class("organise");
    mb.set_tooltip_text(Some("Add a group to put projects in"));
    let pop = gtk::Popover::new();
    let items = w::vbox(6, &["moremenu", "newgroup"]);
    items.append(&w::label("New group", &["caption-heading", "menuhead"]));
    let entry = gtk::Entry::builder().placeholder_text("Its name, then Enter").max_length(40).build();
    entry.set_widget_name("new-group-entry");
    let err = w::label("", &["err"]);
    err.set_visible(false);
    {
        let (ui, pop, err) = (ui.clone(), pop.clone(), err.clone());
        entry.connect_activate(move |e| {
            let name = e.text().trim().to_string();
            if name.is_empty() {
                return;
            }
            let (pop, err, e) = (pop.clone(), err.clone(), e.clone());
            app::bg(
                &ui,
                move |src| src.add_group(&name),
                move |ui, r| match r {
                    Ok(name) => {
                        pop.popdown();
                        e.set_text("");
                        // Groups show only when the projects are grouped by them.
                        if prefs::projects_group() != "groups" {
                            prefs::change(ui, |s| s.projects_group = "groups".into());
                            ui.st.borrow_mut().chips_drawn.clear();
                            render_chips(ui);
                        }
                        app::toast(ui, &format!("Added the group {name}. Drag projects into it."), false);
                        app::refresh(ui);
                    }
                    Err(m) => {
                        err.set_text(&m);
                        err.set_visible(true);
                    }
                },
            );
        });
    }
    items.append(&entry);
    items.append(&err);
    pop.set_child(Some(&items));
    {
        let entry = entry.clone();
        pop.connect_show(move |_| {
            entry.grab_focus();
        });
    }
    mb.set_popover(Some(&pop));
    mb
}

/// The projects in sections: by the person's own groups (then those in none), by activity
/// (worked out: needs you, at work, paused, this week, older), or one section with no heading.
fn project_sections<'a>(list: &'a [ProjectSummary], grouping: &str, groups: &[String]) -> Vec<Section<'a>> {
    let plain = |v: Vec<(String, Vec<&'a ProjectSummary>)>| v.into_iter().map(|(title, items)| Section { title, items, target: None }).collect();
    match grouping {
        // Every group, even one with nothing in it yet, then those in none (a place to drag a
        // project out of its group, so it shows even when empty).
        "groups" if !groups.is_empty() || list.iter().any(|p| p.group.is_some()) => {
            // In the person's order (the core keeps it); any other a project names, after.
            let mut names: Vec<String> = groups.to_vec();
            for g in list.iter().filter_map(|p| p.group.as_ref()) {
                if !names.contains(g) {
                    names.push(g.clone());
                }
            }
            let mut out: Vec<Section> =
                names.into_iter().map(|n| Section { items: list.iter().filter(|p| p.group.as_deref() == Some(n.as_str())).collect(), title: n.clone(), target: Some(Some(n)) }).collect();
            out.push(Section { title: "Not in a group".into(), items: list.iter().filter(|p| p.group.is_none()).collect(), target: Some(None) });
            out
        }
        "activity" => plain({
            let week = glib::DateTime::now_local().ok().and_then(|n| n.add_days(-7).ok());
            let recent = |p: &ProjectSummary| p.last_at.as_deref().and_then(fmt::parse).and_then(|t| t.to_local().ok()).zip(week.clone()).is_some_and(|(t, w)| t > w);
            let kind = |p: &ProjectSummary| {
                if p.live.waiting > 0 {
                    0
                } else if p.live.working > 0 {
                    1
                } else if p.paused.is_some() {
                    2
                } else if recent(p) {
                    3
                } else {
                    4
                }
            };
            ["Needs you", "At work", "Paused", "This week", "Older"]
                .into_iter()
                .enumerate()
                .map(|(k, t)| (t.to_string(), list.iter().filter(|p| kind(p) == k).collect::<Vec<_>>()))
                .filter(|(_, v)| !v.is_empty())
                .collect()
        }),
        _ => plain(vec![(String::new(), list.iter().collect())]),
    }
}

/// A project's importance as three joined choices (High, Normal, Low): one click changes it.
fn importance_switch(ui: &Ui, p: &ProjectSummary) -> gtk::Box {
    let b = w::hbox(0, &["segs", "imp"]);
    b.set_valign(gtk::Align::Center);
    b.set_halign(gtk::Align::Start);
    b.update_property(&[gtk::accessible::Property::Label(&format!("{} importance", p.name))]);
    for (val, label) in [(Some("high"), "High"), (None, "Normal"), (Some("low"), "Low")] {
        let btn = gtk::Button::with_label(label);
        btn.add_css_class("seg");
        let on = p.importance.as_deref() == val;
        if on {
            btn.add_css_class("on");
        }
        btn.set_tooltip_text(Some(&format!("Importance: {}", label.to_lowercase())));
        // A click here leaves the keyboard where it was (Menu or ⋯ has importance for the keys).
        btn.set_focus_on_click(false);
        let (ui, id) = (ui.clone(), p.id.clone());
        btn.connect_clicked(move |_| {
            if on {
                return;
            }
            let id = id.clone();
            app::bg(
                &ui,
                move |src| src.set_project_importance(&id, val),
                |ui, r| match r {
                    Ok(_) => app::refresh(ui),
                    Err(e) => app::toast(ui, &e, true),
                },
            );
        });
        b.append(&btn);
    }
    b
}

/// A project's ⋯ on its card or row: importance and group, where you see the project.
fn card_menu(ui: &Ui, p: &ProjectSummary) -> gtk::MenuButton {
    let m = crate::panel::project_menu(ui, p);
    m.set_widget_name("card-more");
    m.set_focus_on_click(false);
    m
}

/// A right-click on `w` opens `menu`, as a context menu does anywhere.
fn right_click_opens(w: &impl IsA<gtk::Widget>, menu: &gtk::MenuButton) {
    let click = gtk::GestureClick::new();
    click.set_button(3);
    let menu = menu.clone();
    click.connect_pressed(move |g, _, _, _| {
        g.set_state(gtk::EventSequenceState::Claimed);
        menu.popup();
    });
    w.add_controller(click);
}

/// One project as a row of a boxed list: its colour and name, its importance, the next step (or
/// where it was left), and at the end its open sessions and when it was last touched.
fn project_item(ui: &Ui, p: &ProjectSummary, edge: (bool, bool)) -> gtk::ListBoxRow {
    let row = w::hbox(14, &["item"]);
    let mid = w::vbox(4, &[]);
    mid.set_hexpand(true);
    let top = w::hbox(8, &[]);
    let dot = w::dot(&format!("pc-{}", crate::setview::colour_of(p)));
    dot.add_css_class("tag");
    top.append(&dot);
    top.append(&w::label(&p.name, &["pname"]));
    if let Some(g) = p.group.as_deref().filter(|_| prefs::projects_group() != "groups") {
        let t = w::state_tag(g, "group");
        t.set_valign(gtk::Align::Center);
        top.append(&t);
    }
    if let Some(pause) = &p.paused {
        let pill = w::label(&format!("paused {}", fmt::short_when(Some(&pause.at))), &["pill", "paused"]);
        pill.set_tooltip_text(Some(&pause.note));
        top.append(&pill);
    }
    mid.append(&top);
    let second = if !p.next.is_empty() {
        p.next.clone()
    } else if let Some(pause) = p.paused.as_ref().filter(|x| !x.note.is_empty()) {
        format!("paused with a note: {}", pause.note)
    } else {
        p.last_summary.as_deref().map(fmt::recap).filter(|s| !s.is_empty()).map_or_else(|| "No next step written yet.".into(), |s| format!("last time: {s}"))
    };
    let l = w::clip(&second, &["dim"]);
    l.set_xalign(0.0);
    mid.append(&l);
    row.append(&mid);
    let end = w::hbox(6, &[]);
    end.set_valign(gtk::Align::Center);
    if p.live.waiting > 0 {
        end.append(&w::state_tag(&format!("{} waiting", p.live.waiting), "waiting"));
    }
    if p.live.working > 0 {
        end.append(&w::state_tag(&format!("{} working", p.live.working), "working"));
    }
    if let Some(b) = crate::behind::row_line(ui, &p.id) {
        let t = w::state_tag(&b.text(), "behind");
        t.set_tooltip_text(b.tooltip_text().as_deref());
        end.append(&t);
    }
    if !p.removed {
        end.append(&importance_switch(ui, p));
    }
    let last = w::label(&fmt::ago(p.last_at.as_deref()), &["num", "dim"]);
    // One width for every row, so the importance choices line up down the list.
    last.set_width_chars(11);
    last.set_xalign(1.0);
    let id = p.id.clone();
    live_text(ui, View::Projects, &last, move |st| st.ov.as_ref()?.projects.iter().find(|x| x.id == id).map(|p| fmt::ago(p.last_at.as_deref())));
    end.append(&last);
    let menu = card_menu(ui, p);
    end.append(&menu);
    row.append(&end);
    let r = list_row(&p.id, &row);
    right_click_opens(&r, &menu);
    r.add_css_class("itemrow");
    if edge.0 {
        r.add_css_class("first");
    }
    if edge.1 {
        r.add_css_class("last");
    }
    if p.removed {
        r.add_css_class("gone");
    }
    r
}

/// Grid or list, as two joined buttons (the choice is remembered).
pub fn layout_switch(ui: &Ui) -> gtk::Box {
    let now = prefs::projects_layout();
    let b = w::hbox(0, &["pair", "layoutswitch"]);
    for (id, icon, tip) in [("grid", &["view-grid-symbolic"][..], "Show as cards"), ("list", &["view-list-symbolic"][..], "Show as a list")] {
        let btn = gtk::Button::new();
        btn.set_child(Some(&w::icon(icon)));
        btn.add_css_class("pairbtn");
        if now == id {
            btn.add_css_class("on");
        }
        btn.set_tooltip_text(Some(tip));
        btn.update_property(&[gtk::accessible::Property::Label(tip)]);
        let ui = ui.clone();
        btn.connect_clicked(move |_| {
            prefs::change(&ui, |s| s.projects_layout = id.into());
            ui.st.borrow_mut().chips_drawn.clear();
            render_chips(&ui);
            render_view(&ui);
        });
        b.append(&btn);
    }
    b
}

/// Organise: how the projects are grouped, and in what order.
pub fn organise_menu(ui: &Ui) -> gtk::MenuButton {
    let mb = gtk::MenuButton::new();
    mb.set_label("Organise");
    mb.add_css_class("organise");
    mb.set_tooltip_text(Some("Group and sort the projects"));
    let pop = gtk::Popover::new();
    let items = w::vbox(2, &["moremenu"]);
    let grouping = prefs::projects_group();
    let sort = prefs::sort_of(View::Projects);
    items.append(&w::label("Group by", &["caption-heading", "menuhead"]));
    for (id, label) in [("groups", "Your groups"), ("activity", "Activity (automatic)"), ("none", "No groups")] {
        let b = gtk::Button::with_label(&format!("{} {label}", if grouping == id { "✓" } else { "  " }));
        b.add_css_class("menuitem");
        let (ui, pop) = (ui.clone(), pop.clone());
        b.connect_clicked(move |_| {
            pop.popdown();
            prefs::change(&ui, |s| s.projects_group = id.into());
            ui.st.borrow_mut().chips_drawn.clear();
            render_chips(&ui);
            render_view(&ui);
        });
        items.append(&b);
    }
    items.append(&w::label("Sort by", &["caption-heading", "menuhead"]));
    for (key, label) in [(None, "What needs you"), (Some("importance"), "Importance"), (Some("last"), "Last touched"), (Some("project"), "Name")] {
        let on = sort.as_ref().map(|(k, _)| k.as_str()) == key;
        let b = gtk::Button::with_label(&format!("{} {label}", if on { "✓" } else { "  " }));
        b.add_css_class("menuitem");
        let (ui, pop) = (ui.clone(), pop.clone());
        b.connect_clicked(move |_| {
            pop.popdown();
            prefs::set_sort(&ui, View::Projects, &key.map(|k| (k.to_string(), false)));
            ui.st.borrow_mut().drawn.remove(&View::Projects);
            ui.st.borrow_mut().chips_drawn.clear();
            render_chips(&ui);
            render_view(&ui);
        });
        items.append(&b);
    }
    pop.set_child(Some(&items));
    mb.set_popover(Some(&pop));
    mb
}

/// One project as a card: its colour along the top, its name, the next step in large type (or
/// where it was left), its open sessions, and its loose ends in git.
fn project_card(ui: &Ui, p: &ProjectSummary) -> gtk::FlowBoxChild {
    let colour = crate::setview::colour_of(p);
    let card = w::vbox(0, &["card", "pcard"]);
    let body = w::vbox(8, &["pbody"]);
    let head = w::hbox(8, &[]);
    let dot = w::dot(&format!("pc-{colour}"));
    dot.add_css_class("tag");
    head.append(&dot);
    let name = w::clip(&p.name, &["title-3"]);
    head.append(&name);
    if let Some(pause) = &p.paused {
        let pill = w::label(&format!("paused {}", fmt::short_when(Some(&pause.at))), &["pill", "paused"]);
        pill.set_tooltip_text(Some(&pause.note));
        pill.set_valign(gtk::Align::Center);
        head.append(&pill);
    }
    let last = w::label(&fmt::ago(p.last_at.as_deref()), &["num", "dim"]);
    last.set_valign(gtk::Align::Center);
    let id = p.id.clone();
    live_text(ui, View::Projects, &last, move |st| st.ov.as_ref()?.projects.iter().find(|x| x.id == id).map(|p| fmt::ago(p.last_at.as_deref())));
    head.append(&last);
    let menu = card_menu(ui, p);
    head.append(&menu);
    body.append(&head);
    if let Some(g) = p.group.as_deref().filter(|_| prefs::projects_group() != "groups") {
        let t = w::state_tag(g, "group");
        t.set_halign(gtk::Align::Start);
        body.append(&t);
    }
    let sub = if p.desc.is_empty() { p.roots.first().map(|r| fmt::folder_hint(r, &p.name)).unwrap_or_default() } else { p.desc.clone() };
    if !sub.is_empty() {
        body.append(&w::clip(&sub, &["dim"]));
    }
    // The next step, or where it was left.
    let (lead, text, class) = if !p.next.is_empty() {
        ("Next step", p.next.clone(), "body-large")
    } else if let Some(pause) = &p.paused {
        ("Paused with a note", pause.note.clone(), "body-large")
    } else if let Some(sum) = p.last_summary.as_deref().map(fmt::recap).filter(|s| !s.is_empty()) {
        ("Last time", sum, "dim")
    } else {
        ("Next step", "None written yet. Open it (Enter) to write one.".to_string(), "dim")
    };
    let l = w::label(lead, &["caption-heading"]);
    l.set_xalign(0.0);
    l.set_margin_top(6);
    body.append(&l);
    let t = w::wrap(&fmt::clip(&text, 400), &[class]);
    t.set_xalign(0.0);
    t.set_lines(3);
    t.set_ellipsize(gtk::pango::EllipsizeMode::End);
    t.set_tooltip_text(Some(&text));
    t.set_vexpand(true);
    t.set_valign(gtk::Align::Start);
    body.append(&t);
    // What is open, and what has come in.
    let chips = w::hbox(6, &[]);
    chips.set_margin_top(6);
    let chip = |n: u32, word: &str, class: &str| w::state_tag(&format!("{n} {word}"), class);
    if p.live.waiting > 0 {
        chips.append(&chip(p.live.waiting, "waiting", "waiting"));
    }
    if p.live.working > 0 {
        chips.append(&chip(p.live.working, "working", "working"));
    }
    if p.live.idle > 0 {
        chips.append(&chip(p.live.idle, "idle", "idle"));
    }
    if let Some(l) = crate::behind::row_line(ui, &p.id) {
        // As a tag like the others, in its whole words.
        let t = w::state_tag(&l.text(), "behind");
        t.set_tooltip_text(l.tooltip_text().as_deref());
        chips.append(&t);
    }
    if chips.first_child().is_some() {
        body.append(&chips);
    }
    if p.removed {
        let ui = ui.clone();
        let id = p.id.clone();
        body.append(&w::button("Track again", None, &["sm"], move || restore_project(&ui, &id)));
    } else {
        let words = if p.git_checked { git_words(p) } else { dim("checking git…") };
        let g = w::markup(&words.replace("\n(", " (").replace('\n', " · "), &["caption"]);
        g.set_wrap(true);
        g.set_xalign(0.0);
        g.set_max_width_chars(1);
        g.set_hexpand(true);
        body.append(&g);
        let imp = importance_switch(ui, p);
        imp.set_margin_top(4);
        body.append(&imp);
    }
    card.append(&body);
    let c = gtk::FlowBoxChild::new();
    c.set_widget_name(&p.id);
    c.set_child(Some(&card));
    c.set_tooltip_text(Some("Double-click for details · Enter · right-click or Menu key for importance and group"));
    right_click_opens(&c, &menu);
    if p.removed {
        c.add_css_class("gone");
    }
    c
}

pub fn restore_project(ui: &Ui, id: &str) {
    let id = id.to_string();
    app::bg(
        ui,
        move |src| src.remove_project(&id, false),
        |ui, r| match r {
            Ok(p) => {
                app::toast(ui, &format!("Tracking {} again.", p.name), false);
                app::refresh(ui);
            }
            Err(e) => app::toast(ui, &e, true),
        },
    );
}
