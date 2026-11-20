//! Capturing a thought asks first which project it is about (added 2026-11-20): `c` (or clicking
//! into the capture box) opens a small picker above the capture bar, the best guess already
//! chosen; type to filter, ↑/↓, Enter picks, Esc means Unsorted. Then the cursor is in the text
//! box with that project shown beside it, and Enter keeps the thought there. Picking the guess is
//! one Enter. Mouse users can still use the project drop-down.
//!
//! The steps are a small state machine (`Flow`), kept free of GTK so it can be tested.

use std::cell::RefCell;

use gtk4::{self as gtk, gdk, glib, prelude::*};

use crate::{
    app::{self, Drawer, Ui, View},
    views, w,
};

/// Where capturing is.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Stage {
    /// Not capturing (the next `c` or click asks for the project).
    #[default]
    Idle,
    /// The picker is open: what is typed in its filter, and which of the matches is chosen.
    Picking { query: String, sel: usize },
    /// The project is chosen (None = Unsorted); the keyboard is in the text box.
    Typing { project: Option<String> },
}

/// One project in the picker. `id` None is Unsorted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub id: Option<String>,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Enter,
    Esc,
    /// The filter's text changed to this.
    Filter(String),
}

#[derive(Debug, Clone, Default)]
pub struct Flow {
    pub stage: Stage,
    pub choices: Vec<Choice>,
}

/// The projects to offer: every tracked project in the rail's order, then Unsorted.
pub fn choices(projects: &[(String, String)]) -> Vec<Choice> {
    let mut v: Vec<Choice> = projects.iter().map(|(id, name)| Choice { id: Some(id.clone()), name: name.clone() }).collect();
    v.push(Choice { id: None, name: "Unsorted".into() });
    v
}

/// The best guess: the project of what is open or selected, else the filtered project, else the
/// last one used. Pure, for tests.
pub fn best_guess(open: Option<String>, selected: Option<String>, filtered: Option<String>, last: Option<String>) -> Option<String> {
    open.or(selected).or(filtered).or(last)
}

impl Flow {
    /// The choices matching the filter, as indexes into `choices`: names containing the text
    /// (any case), those starting with it first.
    pub fn matches(&self) -> Vec<usize> {
        let q = match &self.stage {
            Stage::Picking { query, .. } => query.trim().to_lowercase(),
            _ => String::new(),
        };
        if q.is_empty() {
            return (0..self.choices.len()).collect();
        }
        let mut starts = Vec::new();
        let mut within = Vec::new();
        for (i, c) in self.choices.iter().enumerate() {
            let n = c.name.to_lowercase();
            if n.starts_with(&q) || n.split(|ch: char| !ch.is_alphanumeric()).any(|w| w.starts_with(&q)) {
                starts.push(i);
            } else if n.contains(&q) {
                within.push(i);
            }
        }
        starts.extend(within);
        starts
    }

    /// Opens the picker on `guess` (which is chosen; Unsorted when there is none or it's gone).
    pub fn start(&mut self, choices: Vec<Choice>, guess: Option<&str>) {
        let sel = choices.iter().position(|c| c.id.as_deref() == guess).unwrap_or(choices.len().saturating_sub(1));
        self.choices = choices;
        self.stage = Stage::Picking { query: String::new(), sel };
    }

    /// The choice the picker has on now (an index into `choices`).
    pub fn chosen(&self) -> Option<usize> {
        match &self.stage {
            Stage::Picking { sel, .. } => {
                let m = self.matches();
                m.get(*sel).copied().or_else(|| if m.is_empty() { None } else { m.last().copied() })
            }
            _ => None,
        }
    }

    /// A key in the picker. Returns Some(project) once one is picked (None inside = Unsorted); the
    /// flow is then `Typing`.
    pub fn key(&mut self, k: Key) -> Option<Option<String>> {
        let Stage::Picking { query, sel } = &mut self.stage else { return None };
        let n = {
            let q = query.clone();
            let probe = Flow { stage: Stage::Picking { query: q, sel: 0 }, choices: self.choices.clone() };
            probe.matches().len()
        };
        match k {
            Key::Down => *sel = (*sel + 1).min(n.saturating_sub(1)),
            Key::Up => *sel = sel.saturating_sub(1),
            Key::Filter(t) => {
                *query = t;
                *sel = 0;
            }
            Key::Esc => {
                self.stage = Stage::Typing { project: None };
                return Some(None);
            }
            Key::Enter => {
                // Nothing matches: Enter does nothing (the filter shows "no project matches").
                let at = self.chosen()?;
                let project = self.choices[at].id.clone();
                self.stage = Stage::Typing { project: project.clone() };
                return Some(project);
            }
        }
        None
    }

    /// Capturing ended (the keyboard left the capture bar).
    pub fn leave(&mut self) {
        self.stage = Stage::Idle;
    }
}

// ---------- the widgets ----------

struct Ws {
    pick: gtk::Box,
    filter: gtk::Entry,
    list: gtk::Box,
    scroll: gtk::ScrolledWindow,
}

thread_local! {
    static WS: RefCell<Option<Ws>> = const { RefCell::new(None) };
    static FLOW: RefCell<Flow> = RefCell::new(Flow::default());
    /// The project last kept, for the next guess.
    static LAST: RefCell<Option<String>> = const { RefCell::new(None) };
}

fn with<T>(f: impl FnOnce(&Ws) -> T) -> Option<T> {
    WS.with(|w| w.borrow().as_ref().map(f))
}

pub fn stage() -> Stage {
    FLOW.with(|f| f.borrow().stage.clone())
}

/// Builds the picker (hidden) just above the capture bar.
pub fn build(ui: &Ui) {
    let Some(bar) = ui.w.cap_entry.parent() else { return };
    let Some(main) = bar.parent().and_downcast::<gtk::Box>() else { return };
    let pick = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).accessible_role(gtk::AccessibleRole::Group).build();
    pick.add_css_class("cappick");
    pick.update_property(&[gtk::accessible::Property::Label("Which project is this thought about?")]);
    let head = w::hbox(10, &[]);
    head.append(&w::label("WHICH PROJECT?", &["flabel"]));
    let filter = gtk::Entry::builder().placeholder_text("type to filter").max_length(60).build();
    filter.add_css_class("capfilter");
    filter.update_property(&[gtk::accessible::Property::Label("Filter projects")]);
    head.append(&filter);
    head.append(&w::spacer());
    for (k, what) in [("↑↓", "choose"), ("Enter", "pick"), ("Esc", "Unsorted")] {
        head.append(&w::kbd(k));
        head.append(&w::label(what, &["meta"]));
    }
    pick.append(&head);
    let list = w::vbox(1, &["caplist"]);
    let scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).propagate_natural_height(true).max_content_height(230).child(&list).build();
    pick.append(&scroll);
    pick.set_visible(false);
    main.insert_child_after(&pick, bar.prev_sibling().as_ref());
    {
        let ui = ui.clone();
        filter.connect_changed(move |e| {
            let t = e.text().to_string();
            FLOW.with(|f| f.borrow_mut().key(Key::Filter(t)));
            draw(&ui);
        });
    }
    {
        // Clicking into the text box asks too, unless the drop-down was just used to choose.
        // Only a click: focus arriving by Tab or F6 never opens anything (moving focus must not
        // act); there `c` asks, and the drop-down shows the project it will go to.
        let ui2 = ui.clone();
        let click = gtk::GestureClick::new();
        click.set_propagation_phase(gtk::PropagationPhase::Capture);
        click.connect_pressed(move |_, _, _, _| {
            if stage() == Stage::Idle {
                let ui = ui2.clone();
                // After GTK has finished moving the focus in.
                glib::idle_add_local_once(move || {
                    if stage() == Stage::Idle {
                        begin(&ui);
                    }
                });
            }
        });
        ui.w.cap_entry.add_controller(click);
    }
    WS.with(|w| *w.borrow_mut() = Some(Ws { pick, filter, list, scroll }));
}

/// `c`, or a click into the capture box: ask which project first.
pub fn begin(ui: &Ui) {
    if !WidgetExt::is_visible(&ui.w.cap_entry) {
        return;
    }
    let (projects, guess, manual, chosen_now) = {
        let st = ui.st.borrow();
        // In the rail's order, so each project is where the eye expects it.
        let mut sorted: Vec<&wake_core::ProjectSummary> = st.ov.as_ref().map(|ov| ov.projects.iter().filter(|p| !p.removed).collect()).unwrap_or_default();
        sorted.sort_by(|a, b| views::by_project(a, b));
        let ps: Vec<(String, String)> = sorted.iter().map(|p| (p.id.clone(), p.name.clone())).collect();
        let session_project = |sid: &str| st.ov.as_ref().and_then(|ov| ov.sessions.iter().find(|s| s.id == sid).and_then(|s| s.project.clone()));
        let open = match &st.drawer {
            Some(Drawer::Project(id)) => Some(id.clone()),
            Some(Drawer::Session(id)) => session_project(id),
            None => None,
        };
        let selected = match st.view {
            View::Projects => st.sel.clone(),
            View::Monitor | View::History => st.sel.as_deref().and_then(session_project),
            View::Agents => crate::agents::selected_proc(&st).and_then(|a| a.session_id.clone()).and_then(|s| session_project(&s)),
            View::Ideas => st.sel.as_deref().and_then(|id| st.ideas.iter().flatten().find(|i| i.id == id).and_then(|i| i.project.clone())),
            _ => None,
        };
        let last = LAST.with(|l| l.borrow().clone()).or_else(|| st.ideas.iter().flatten().find_map(|i| i.project.clone()));
        let chosen_now = st.cap_projects.get(ui.w.cap_to.selected() as usize).cloned().flatten();
        (ps, best_guess(open, selected, st.proj_filter.clone(), last), st.cap_manual, chosen_now)
    };
    if manual {
        // The mouse already chose with the drop-down: straight to the text.
        FLOW.with(|f| f.borrow_mut().stage = Stage::Typing { project: chosen_now.clone() });
        typing(ui, chosen_now);
        return;
    }
    FLOW.with(|f| f.borrow_mut().start(choices(&projects), guess.as_deref()));
    with(|ws| {
        ws.filter.set_text("");
        ws.pick.set_visible(true);
        crate::motion::pop(&ws.pick);
        ws.filter.grab_focus();
    });
    // Setting the text above reset the choice to the first match: put the guess back.
    FLOW.with(|f| {
        let mut f = f.borrow_mut();
        let sel = f.choices.iter().position(|c| c.id == guess).unwrap_or(f.choices.len().saturating_sub(1));
        f.stage = Stage::Picking { query: String::new(), sel };
    });
    draw(ui);
}

fn draw(ui: &Ui) {
    let (choices, matches, chosen) = FLOW.with(|f| {
        let f = f.borrow();
        (f.choices.clone(), f.matches(), f.chosen())
    });
    with(|ws| {
        w::clear(&ws.list);
        if matches.is_empty() {
            ws.list.append(&w::label("No project matches. Esc keeps it Unsorted.", &["meta", "capnone"]));
        }
        for i in matches {
            let c = &choices[i];
            let b = gtk::Button::new();
            b.set_focusable(false);
            b.set_focus_on_click(false);
            b.add_css_class("capitem");
            let inner = w::hbox(8, &[]);
            match &c.id {
                Some(id) => {
                    let col = views::colour_by_id(ui, id).unwrap_or_else(|| "slate".into());
                    let d = w::dot("d");
                    d.add_css_class(&format!("pc-{col}"));
                    inner.append(&d);
                }
                None => inner.append(&w::dot("d ring")),
            }
            inner.append(&w::label(&c.name, &[]));
            b.set_child(Some(&inner));
            if Some(i) == chosen {
                b.add_css_class("on");
                b.update_property(&[gtk::accessible::Property::Label(&format!("{}, chosen", c.name))]);
                let (row, scroll) = (b.clone(), ws.scroll.clone());
                crate::motion::after_layout(&b, move || {
                    if let Some(bounds) = row.compute_bounds(&scroll) {
                        let adj = scroll.vadjustment();
                        let (y, h) = (f64::from(bounds.y()) + adj.value(), f64::from(bounds.height()));
                        if y < adj.value() || y + h > adj.value() + adj.page_size() {
                            adj.set_value((y + h - adj.page_size()).max(0.0).min(y));
                        }
                    }
                });
            }
            let (ui2, id) = (ui.clone(), c.id.clone());
            b.connect_clicked(move |_| picked(&ui2, id.clone()));
            ws.list.append(&b);
        }
    });
    // Screen readers hear which project is chosen.
    let chosen_name = chosen.and_then(|i| choices.get(i)).map(|c| c.name.clone());
    if let Some(n) = chosen_name {
        with(|ws| ws.filter.update_property(&[gtk::accessible::Property::Description(&format!("{n} is chosen"))]));
    }
}

fn picked(ui: &Ui, project: Option<String>) {
    FLOW.with(|f| f.borrow_mut().stage = Stage::Typing { project: project.clone() });
    typing(ui, project);
}

/// The project is chosen: show it beside the text box and put the cursor there.
fn typing(ui: &Ui, project: Option<String>) {
    with(|ws| ws.pick.set_visible(false));
    let at = ui.st.borrow().cap_projects.iter().position(|p| *p == project).unwrap_or(0);
    ui.w.cap_to.set_selected(at as u32);
    ui.st.borrow_mut().cap_manual = true;
    let name = project.as_deref().and_then(|id| app::find_project(ui, id)).map(|p| p.name);
    ui.w.cap_entry.set_placeholder_text(Some(&match &name {
        Some(n) => format!("What did you notice about {n}? Enter keeps it."),
        None => "What did you notice? Enter keeps it, Unsorted.".into(),
    }));
    ui.w.cap_entry.grab_focus_without_selecting();
    ui.w.cap_entry.set_position(-1);
}

/// The keyboard left the capture bar and the picker: capturing is over.
pub fn focus_moved(ui: &Ui, focus: Option<&gtk::Widget>) {
    if stage() == Stage::Idle {
        return;
    }
    let inside = focus.is_some_and(|f| ui.w.cap_entry.parent().is_some_and(|bar| f.is_ancestor(&bar)) || with(|ws| f.is_ancestor(&ws.pick)).unwrap_or(false));
    // A drop-down list's popover is outside the bar but still part of it.
    let in_popover = focus.is_some_and(|f| f.ancestor(gtk::Popover::static_type()).is_some());
    if inside || in_popover {
        return;
    }
    // While the window itself has no focus widget (a dialog closing, the window inactive) wait.
    if focus.is_none() {
        return;
    }
    end(ui);
}

/// Back to idle: the picker hidden, the drop-down following the selection again.
pub fn end(ui: &Ui) {
    FLOW.with(|f| f.borrow_mut().leave());
    with(|ws| ws.pick.set_visible(false));
    ui.w.cap_entry.set_placeholder_text(Some(PLACEHOLDER));
    ui.st.borrow_mut().cap_manual = false;
    app::capture_default(ui);
}

pub const PLACEHOLDER: &str = "What did you notice? Enter keeps it, sort it later.";

/// A thought was kept in `project`: the next guess starts there.
pub fn kept(project: Option<String>) {
    if project.is_some() {
        LAST.with(|l| *l.borrow_mut() = project);
    }
}

/// Keys while the picker is open: ↑/↓ choose, Enter picks, Esc is Unsorted. Typing goes to the
/// filter. None when the picker isn't open.
pub fn on_key(ui: &Ui, key: gdk::Key, mods: gdk::ModifierType) -> Option<glib::Propagation> {
    use gdk::Key as K;
    use glib::Propagation::Stop;
    if !matches!(stage(), Stage::Picking { .. }) || !with(|ws| ws.pick.is_visible()).unwrap_or(false) {
        return None;
    }
    if mods.intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK) {
        return None;
    }
    let k = match key {
        K::Up | K::KP_Up => Key::Up,
        K::Down | K::KP_Down => Key::Down,
        K::Return | K::KP_Enter => Key::Enter,
        K::Escape => Key::Esc,
        _ => return None,
    };
    let out = FLOW.with(|f| f.borrow_mut().key(k));
    match out {
        Some(project) => typing(ui, project),
        None => draw(ui),
    }
    Some(Stop)
}

/// Whether `f` is in the picker.
pub fn contains(f: &gtk::Widget) -> bool {
    with(|ws| f.is_ancestor(&ws.pick)).unwrap_or(false)
}

/// The picker shares the capture bar's "keyboard is here" edge.
pub fn mark(on: bool) {
    with(|ws| {
        if on {
            ws.pick.add_css_class("here");
        } else {
            ws.pick.remove_css_class("here");
        }
    });
}

/// For screenshots and the key replay: is the picker showing.
pub fn picker_open() -> bool {
    with(|ws| ws.pick.is_visible()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flow(guess: Option<&str>) -> Flow {
        let mut f = Flow::default();
        let ps = [("wake", "Wake"), ("gradebook", "Gradebook"), ("skyhook", "Skyhook"), ("tideline", "tideline-desktop")];
        f.start(choices(&ps.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect::<Vec<_>>()), guess);
        f
    }

    #[test]
    fn picking_the_guess_is_one_enter() {
        let mut f = flow(Some("gradebook"));
        assert_eq!(f.choices[f.chosen().unwrap()].name, "Gradebook");
        assert_eq!(f.key(Key::Enter), Some(Some("gradebook".into())));
        assert_eq!(f.stage, Stage::Typing { project: Some("gradebook".into()) });
        // Keys after picking are the text box's, not the picker's.
        assert_eq!(f.key(Key::Down), None);
    }

    #[test]
    fn no_guess_starts_on_unsorted_and_esc_is_unsorted() {
        let mut f = flow(None);
        assert_eq!(f.choices[f.chosen().unwrap()].id, None);
        let mut g = flow(Some("wake"));
        assert_eq!(g.key(Key::Esc), Some(None));
        assert_eq!(g.stage, Stage::Typing { project: None });
        f.leave();
        assert_eq!(f.stage, Stage::Idle);
        assert_eq!(flow(Some("gone")).choices[flow(Some("gone")).chosen().unwrap()].name, "Unsorted", "a guess that isn't tracked any more");
    }

    #[test]
    fn arrows_move_within_the_matches_and_stop_at_the_ends() {
        let mut f = flow(Some("wake"));
        f.key(Key::Up);
        assert_eq!(f.chosen(), Some(0), "already at the top");
        f.key(Key::Down);
        f.key(Key::Down);
        assert_eq!(f.choices[f.chosen().unwrap()].id.as_deref(), Some("skyhook"));
        for _ in 0..10 {
            f.key(Key::Down);
        }
        assert_eq!(f.choices[f.chosen().unwrap()].name, "Unsorted");
    }

    #[test]
    fn typing_filters_word_starts_first_and_enter_picks_the_top_match() {
        let mut f = flow(Some("wake"));
        f.key(Key::Filter("desk".into()));
        assert_eq!(f.matches().iter().map(|&i| f.choices[i].name.as_str()).collect::<Vec<_>>(), ["tideline-desktop"]);
        f.key(Key::Filter("s".into()));
        // Skyhook starts with s; tideline-desktop and Unsorted only contain one.
        assert_eq!(f.matches().iter().map(|&i| f.choices[i].name.as_str()).collect::<Vec<_>>(), ["Skyhook", "tideline-desktop", "Unsorted"]);
        assert_eq!(f.key(Key::Enter), Some(Some("skyhook".into())));
        let mut g = flow(None);
        g.key(Key::Filter("zzz".into()));
        assert_eq!(g.key(Key::Enter), None, "nothing matches: Enter waits");
        assert!(matches!(g.stage, Stage::Picking { .. }));
        assert_eq!(g.key(Key::Esc), Some(None));
    }

    #[test]
    fn the_guess_is_what_is_open_then_selected_then_filtered_then_last_used() {
        let s = |x: &str| Some(x.to_string());
        assert_eq!(best_guess(s("a"), s("b"), s("c"), s("d")), s("a"));
        assert_eq!(best_guess(None, s("b"), s("c"), s("d")), s("b"));
        assert_eq!(best_guess(None, None, s("c"), s("d")), s("c"));
        assert_eq!(best_guess(None, None, None, s("d")), s("d"));
        assert_eq!(best_guess(None, None, None, None), None);
    }
}
