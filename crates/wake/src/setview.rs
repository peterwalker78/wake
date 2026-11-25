//! The Settings view (`,`, or the last item under the views in the rail): a centred column of
//! sections, each a rounded card of one-line settings with its control on the right (joined
//! choices, colour swatches, a switch or a drop-down). Changes apply at once and are kept in
//! ~/.config/wake/settings.toml (wake-core's `settings`).
//!
//! Keyboard: `j`/`k` (or ↑/↓) move between settings, `←`/`→` (or `h`/`l`) change the selected
//! one, `Space`/`Enter` flips a switch or opens a drop-down.

use std::cell::RefCell;

use gtk4::{self as gtk, glib, prelude::*};
use wake_core::{questions::AiChoice, settings::Settings, *};

use crate::{
    app::{self, Ui, View},
    prefs, theme, views, w,
};

thread_local! {
    /// Which AIs Questions could use here (asked of the source when the view opens).
    static AIS: RefCell<Option<Vec<AiChoice>>> = const { RefCell::new(None) };
    /// Set while a switch is flipped from the keyboard.
    static KEYED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    pub id: String,
    pub label: String,
    pub enabled: bool,
    pub tip: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    /// One joined group of words, one of them chosen.
    Segments,
    /// Accent colours: round swatches, the chosen one ringed, names in tooltips.
    Swatches,
    /// On or off (choices "on" and "off").
    Switch,
    /// A drop-down list, for more choices than fit in a row.
    Dropdown,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub key: String,
    pub name: String,
    /// One quiet line under the name, only where it adds something (empty otherwise).
    pub hint: String,
    pub kind: Kind,
    pub choices: Vec<Choice>,
    pub current: String,
}

fn ch(id: &str, label: &str) -> Choice {
    Choice { id: id.into(), label: label.into(), enabled: true, tip: None }
}

fn row(key: &str, name: &str, hint: &str, kind: Kind, choices: Vec<Choice>, current: &str) -> Row {
    Row { key: key.into(), name: name.into(), hint: hint.into(), kind, choices, current: current.into() }
}

fn switch(key: &str, name: &str, hint: &str, on: bool) -> Row {
    row(key, name, hint, Kind::Switch, vec![ch("on", "On"), ch("off", "Off")], if on { "on" } else { "off" })
}

/// The views Wake can open on, as Settings offers them.
const START_VIEWS: [View; 6] = [View::Monitor, View::Agents, View::Projects, View::Ideas, View::History, View::Questions];

/// Why an AI can't be chosen, in a few words.
fn short_reason(c: &AiChoice) -> String {
    if c.hint.contains("isn't installed") {
        "not installed".into()
    } else if c.hint.contains("policy") {
        "a system policy would let it write files".into()
    } else {
        c.hint.trim_end_matches('.').to_string()
    }
}

/// Every setting, by section, as data (so it can be tested without a window). Sorting and project
/// colours are not here: a third click on a column's name puts a table back in Wake's order, and a
/// project's colour is changed in its panel. Their keys in settings.toml are still read and kept.
pub fn sections(s: &Settings, ais: &[AiChoice]) -> Vec<(&'static str, Vec<Row>)> {
    let ai_rows = {
        let choices: Vec<Choice> = ais.iter().map(|c| Choice { id: c.ai.id().into(), label: c.ai.name().into(), enabled: c.usable, tip: Some(c.hint.clone()) }).collect();
        let current = ais.iter().find(|c| c.ai.id() == s.ai && c.usable).map_or("claude", |c| c.ai.id());
        let off: Vec<String> = ais.iter().filter(|c| !c.usable).map(|c| format!("{}: {}", c.ai.name(), short_reason(c))).collect();
        let hint = if off.is_empty() { "A thread keeps the AI that started it.".to_string() } else { format!("{}.", off.join(" · ")) };
        vec![row("ai", "Answered by", &hint, Kind::Segments, choices, current)]
    };
    let look = vec![
        row("theme", "Theme", "", Kind::Segments, vec![ch("dark", "Dark"), ch("light", "Light"), ch("system", "System")], &s.theme),
        row("accent", "Accent", "", Kind::Swatches, theme::ACCENTS.iter().map(|(n, ..)| ch(n, &cap(n))).collect(), &s.accent),
        switch("motion", "Motion", "Panels slide and rows fade, briefly. Off keeps everything still.", s.motion != "reduced"),
    ];
    let open = vec![row("start", "Start on", "", Kind::Dropdown, START_VIEWS.iter().map(|v| ch(v.id(), v.name())).collect(), &s.start_view)];
    let git = vec![
        row(
            "remote",
            "Check remotes",
            "Asks each remote what's new. Never fetches.",
            Kind::Segments,
            wake_core::settings::REMOTE_MINUTES.iter().map(|m| ch(&m.to_string(), &if *m == 0 { "Off".to_string() } else { format!("{m} min") })).collect(),
            &s.remote_minutes.to_string(),
        ),
        switch("notify", "Notify when building while behind", "The banner in Wake shows either way.", s.notify_behind),
    ];
    vec![("Ask", ai_rows), ("Appearance", look), ("When Wake opens", open), ("Git", git)]
}

fn cap(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// A project's colour name: the one picked, else the one from its name.
pub fn colour_of(p: &ProjectSummary) -> String {
    p.color.as_deref().and_then(store::colour_name).unwrap_or_else(|| store::default_colour(&p.name)).to_string()
}

/// What ← (`d` < 0) or → (`d` > 0) makes the setting: the previous or next enabled choice
/// (wrapping), or for a switch off and on.
pub fn step_choice(r: &Row, d: i32) -> Option<String> {
    if r.kind == Kind::Switch {
        return Some(if d > 0 { "on" } else { "off" }.into());
    }
    let enabled: Vec<&Choice> = r.choices.iter().filter(|c| c.enabled).collect();
    if enabled.is_empty() {
        return None;
    }
    let at = enabled.iter().position(|c| c.id == r.current).unwrap_or(0) as i32;
    let n = enabled.len() as i32;
    Some(enabled[((at + d).rem_euclid(n)) as usize].id.clone())
}

/// The AI that answers new questions: the one chosen, if Wake can use it here, else Claude.
pub fn current_ai() -> wake_core::questions::Ai {
    let want = wake_core::questions::Ai::from_id(&prefs::get().ai).unwrap_or_default();
    let usable = AIS.with(|a| a.borrow().as_ref().is_some_and(|l| l.iter().any(|c| c.ai == want && c.usable)));
    if usable { want } else { wake_core::questions::Ai::Claude }
}

fn ais() -> Vec<AiChoice> {
    AIS.with(|a| a.borrow().clone()).unwrap_or_else(|| vec![wake_core::questions::choice(wake_core::questions::Ai::Claude, None, false)])
}

fn all_rows() -> Vec<(&'static str, Vec<Row>)> {
    sections(&prefs::get(), &ais())
}

fn find_row(key: &str) -> Option<Row> {
    all_rows().into_iter().flat_map(|(_, rows)| rows).find(|r| r.key == key)
}

/// The view came up (or Wake started): ask which AIs are usable here (cheap, but off the UI
/// thread all the same).
pub fn shown(ui: &Ui) {
    app::bg(
        ui,
        |src| src.question_ais(),
        |ui, list| {
            AIS.with(|a| *a.borrow_mut() = Some(list));
            let view = ui.st.borrow().view;
            match view {
                View::Settings => render(ui),
                View::Questions => crate::questions::redraw(ui),
                _ => {}
            }
        },
    );
}

/// A row that only shows something (title, note, section heading): never selected or focused.
fn still_row(child: &impl IsA<gtk::Widget>, class: &str) -> gtk::ListBoxRow {
    let h = gtk::ListBoxRow::new();
    h.set_selectable(false);
    h.set_activatable(false);
    h.set_focusable(false);
    h.add_css_class("grouprow");
    h.add_css_class(class);
    h.set_child(Some(child));
    h
}

pub fn render(ui: &Ui) {
    let secs = all_rows();
    let note = prefs::note();
    let sig = format!("{secs:?}{note:?}{}", theme::is_light());
    let Some(had_focus) = views::begin(ui, View::Settings, sig) else { return };
    let vw = &ui.w.views[&View::Settings];
    w::clear(&vw.top);
    let head = w::vbox(3, &[]);
    head.append(&w::label("Settings", &["setttl"]));
    let where_ = if prefs::saves() { "Changes apply at once and are kept in ~/.config/wake/settings.toml." } else { "Changes apply at once. A demo or a screenshot doesn't keep them." };
    head.append(&w::wrap(where_, &["setwhere"]));
    vw.list.append(&still_row(&head, "sethead"));
    if let Some(n) = note {
        vw.list.append(&still_row(&w::wrap(&n, &["setnote"]), "setnoterow"));
    }
    for (title, rows) in secs {
        vw.list.append(&still_row(&w::label(&title.to_uppercase(), &["setsec"]), "setsecrow"));
        let n = rows.len();
        for (i, r) in rows.iter().enumerate() {
            let lr = row_widget(ui, r);
            if i == 0 {
                lr.add_css_class("first");
            }
            if i + 1 == n {
                lr.add_css_class("last");
            }
            vw.list.append(&lr);
        }
    }
    app::restore_sel(ui, View::Settings, had_focus);
}

/// Whether the keyboard is moving through the settings: then the selected row is lit (GTK's own
/// focus state needs an active window, so this doesn't rely on it). A click turns it off, so a
/// mouse user never sees a row stay lit.
pub fn keyboard(ui: &Ui, on: bool) {
    let list = &ui.w.views[&View::Settings].list;
    if on {
        list.add_css_class("keyed");
    } else {
        list.remove_css_class("keyed");
    }
}

/// A click on a control: the mouse's change.
fn clicked(ui: &Ui, key: &str, id: &str) {
    keyboard(ui, false);
    apply(ui, key, id);
}

/// A group of joined buttons, one chosen.
fn segments(ui: &Ui, r: &Row) -> gtk::Box {
    let b = w::hbox(0, &["segs"]);
    for c in &r.choices {
        let s = gtk::Button::with_label(&c.label);
        s.add_css_class("seg");
        s.set_focusable(false);
        s.set_sensitive(c.enabled);
        if c.id == r.current {
            s.add_css_class("on");
        }
        if let Some(t) = &c.tip {
            s.set_tooltip_text(Some(t));
        }
        let (ui2, key, id) = (ui.clone(), r.key.clone(), c.id.clone());
        s.connect_clicked(move |_| clicked(&ui2, &key, &id));
        b.append(&s);
    }
    b
}

fn swatches(ui: &Ui, r: &Row) -> gtk::Box {
    let b = w::hbox(12, &["setswatches"]);
    for c in &r.choices {
        let s = gtk::Button::new();
        s.add_css_class("swatch");
        s.add_css_class(&format!("acc-{}", c.id));
        s.set_focusable(false);
        s.set_valign(gtk::Align::Center);
        s.set_tooltip_text(Some(&c.label));
        let on = c.id == r.current;
        if on {
            s.add_css_class("on");
        }
        s.update_property(&[gtk::accessible::Property::Label(&format!("{}{}", c.label, if on { ", chosen" } else { "" }))]);
        let (ui2, key, id) = (ui.clone(), r.key.clone(), c.id.clone());
        s.connect_clicked(move |_| clicked(&ui2, &key, &id));
        b.append(&s);
    }
    b
}

fn switch_widget(ui: &Ui, r: &Row) -> gtk::Switch {
    let s = gtk::Switch::new();
    s.set_focusable(false);
    s.set_active(r.current == "on");
    s.update_property(&[gtk::accessible::Property::Label(&r.name)]);
    let (ui2, key) = (ui.clone(), r.key.clone());
    s.connect_state_set(move |_, on| {
        // After this handler: apply redraws the row, and with it this switch.
        let (ui3, key) = (ui2.clone(), key.clone());
        glib::idle_add_local_once(move || apply(&ui3, &key, if on { "on" } else { "off" }));
        if !KEYED.with(std::cell::Cell::get) {
            keyboard(&ui2, false);
        }
        glib::Propagation::Proceed
    });
    s
}

fn dropdown(ui: &Ui, r: &Row) -> gtk::DropDown {
    let names: Vec<&str> = r.choices.iter().map(|c| c.label.as_str()).collect();
    let d = gtk::DropDown::from_strings(&names);
    d.add_css_class("setdrop");
    d.set_focusable(false);
    // Its button never takes the keyboard: the row has it, and Enter or Space opens the list.
    let mut c = d.first_child();
    while let Some(x) = c {
        if x.is::<gtk::Button>() {
            x.set_focusable(false);
        }
        c = x.next_sibling();
    }
    if let Some(i) = r.choices.iter().position(|c| c.id == r.current) {
        d.set_selected(i as u32);
    }
    d.update_property(&[gtk::accessible::Property::Label(&r.name)]);
    let (ui2, key, ids) = (ui.clone(), r.key.clone(), r.choices.iter().map(|c| c.id.clone()).collect::<Vec<_>>());
    d.connect_selected_notify(move |d| {
        let Some(id) = ids.get(d.selected() as usize).cloned() else { return };
        let (ui3, key) = (ui2.clone(), key.clone());
        // Not from inside the drop-down's own signal: apply redraws it.
        glib::idle_add_local_once(move || apply(&ui3, &key, &id));
    });
    d
}

fn row_widget(ui: &Ui, r: &Row) -> gtk::ListBoxRow {
    let line = w::hbox(20, &["cell"]);
    let left = w::vbox(2, &[]);
    left.set_hexpand(true);
    left.set_valign(gtk::Align::Center);
    // The name wraps in a narrow window rather than pushing the control out.
    left.append(&w::wrap(&r.name, &["setname"]));
    if !r.hint.is_empty() {
        left.append(&w::wrap(&r.hint, &["sethint"]));
    }
    line.append(&left);
    let ctl: gtk::Widget = match r.kind {
        Kind::Segments => segments(ui, r).upcast(),
        Kind::Swatches => swatches(ui, r).upcast(),
        Kind::Switch => switch_widget(ui, r).upcast(),
        Kind::Dropdown => dropdown(ui, r).upcast(),
    };
    ctl.set_valign(gtk::Align::Center);
    ctl.set_halign(gtk::Align::End);
    line.append(&ctl);
    let lr = views::list_row(&r.key, &line);
    lr.add_css_class("setrow");
    lr
}

/// Enter or Space on a setting (or a click on its row): a switch flips, a drop-down opens. Rows
/// of choices change with ← and →, or a click on the choice.
pub fn activate(ui: &Ui) {
    let Some(key) = ui.st.borrow().sel.clone() else { return };
    let Some(r) = find_row(&key) else { return };
    match r.kind {
        Kind::Switch => {
            KEYED.with(|k| k.set(true));
            apply(ui, &key, if r.current == "on" { "off" } else { "on" });
            KEYED.with(|k| k.set(false));
        }
        Kind::Dropdown => {
            let Some(row) = ui.w.views[&View::Settings].list.selected_row() else { return };
            if let Some(d) = find_widget::<gtk::DropDown>(row.upcast_ref()) {
                d.emit_activate();
            }
        }
        Kind::Segments | Kind::Swatches => {}
    }
}

fn find_widget<T: IsA<gtk::Widget>>(w: &gtk::Widget) -> Option<T> {
    if let Some(t) = w.downcast_ref::<T>() {
        return Some(t.clone());
    }
    let mut c = w.first_child();
    while let Some(x) = c {
        if let Some(f) = find_widget::<T>(&x) {
            return Some(f);
        }
        c = x.next_sibling();
    }
    None
}

/// ← / →: the selected setting's previous or next choice.
pub fn step(ui: &Ui, d: i32) -> bool {
    keyboard(ui, true);
    let Some(key) = ui.st.borrow().sel.clone() else { return false };
    let Some(r) = find_row(&key) else { return false };
    match step_choice(&r, d) {
        Some(id) if id != r.current => {
            apply(ui, &key, &id);
            true
        }
        Some(_) => true,
        None => false,
    }
}

/// Make the change `key` = `id`, at once, and keep it.
pub fn apply(ui: &Ui, key: &str, id: &str) {
    if find_row(key).is_some_and(|r| r.current == id) {
        return;
    }
    let had_focus = gtk::prelude::GtkWindowExt::focus(&ui.window).is_some_and(|f| f.is_ancestor(&ui.w.views[&View::Settings].list));
    ui.st.borrow_mut().sel = Some(key.to_string());
    let id = id.to_string();
    match key {
        "ai" => {
            prefs::change(ui, |s| s.ai = id);
            ui.src.set_options(&prefs::get().core());
            crate::questions::redraw(ui);
        }
        "theme" | "accent" => {
            prefs::change(ui, |s| if key == "theme" { s.theme = id } else { s.accent = id });
            let p = prefs::get();
            theme::apply(&p.theme, &p.accent);
            app::redraw_all(ui);
        }
        "motion" => {
            prefs::change(ui, |s| s.motion = if id == "on" { "full" } else { "reduced" }.into());
            crate::motion::apply(ui);
        }
        "start" => prefs::change(ui, |s| s.start_view = id),
        "remote" => {
            prefs::change(ui, |s| s.remote_minutes = id.parse().unwrap_or(10));
            ui.src.set_options(&prefs::get().core());
        }
        "notify" => {
            prefs::change(ui, |s| s.notify_behind = id == "on");
            ui.src.set_options(&prefs::get().core());
        }
        _ => {}
    }
    render(ui);
    // The keyboard stays on the row it was on (the redraw made new rows, and a drop-down's list
    // closing leaves the focus nowhere).
    let lost = gtk::prelude::GtkWindowExt::focus(&ui.window).is_none_or(|f| !f.is_ancestor(&ui.w.views[&View::Settings].list));
    if ui.st.borrow().view == View::Settings && (had_focus || lost) {
        app::restore_sel(ui, View::Settings, true);
    }
}

/// A project's dot colour, here and in the project panel: shown at once, then kept.
pub fn set_colour(ui: &Ui, project: &str, colour: &str) {
    {
        let mut st = ui.st.borrow_mut();
        if let Some(p) = st.ov.as_mut().and_then(|o| o.projects.iter_mut().find(|p| p.id == project)) {
            p.color = Some(colour.to_string());
        }
        if let Some(d) = st.projects.get_mut(project) {
            d.summary.color = Some(colour.to_string());
        }
        st.drawn.clear();
        st.rail_drawn.clear();
        st.panel_drawn = None;
    }
    app::render_all(ui);
    let (p, c) = (project.to_string(), colour.to_string());
    app::bg(
        ui,
        move |src| src.set_project_color(&p, Some(&c)),
        |ui, r| {
            if let Err(e) = r {
                app::toast(ui, &e, true);
            }
            app::refresh(ui);
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use wake_core::questions::{Ai, choice};

    #[test]
    fn only_usable_ais_can_be_chosen_and_arrows_skip_the_rest() {
        let ais = vec![choice(Ai::Claude, None, false), choice(Ai::Codex, Some(std::path::Path::new("/x")), false), choice(Ai::Gemini, None, false)];
        let mut s = Settings::default();
        s.ai = "gemini".into();
        let secs = sections(&s, &ais);
        let ai = &secs[0].1[0];
        assert_eq!(ai.current, "claude", "an AI that isn't installed is never the chosen one");
        assert!(!ai.choices[2].enabled);
        assert_eq!(ai.hint, "Gemini: not installed.", "a short reason under the row");
        assert_eq!(step_choice(ai, 1).as_deref(), Some("codex"));
        let mut r = ai.clone();
        r.current = "codex".into();
        assert_eq!(step_choice(&r, 1).as_deref(), Some("claude"), "wraps, skipping Gemini");
    }

    #[test]
    fn the_sections_and_their_controls() {
        let secs = sections(&Settings::default(), &[choice(Ai::Claude, None, false)]);
        let names: Vec<&str> = secs.iter().map(|s| s.0).collect();
        assert_eq!(names, ["Ask", "Appearance", "When Wake opens", "Git"], "no sorting, no project colours");
        let kinds: Vec<(&str, Kind)> = secs.iter().flat_map(|s| s.1.iter().map(|r| (r.key.as_str(), r.kind))).collect();
        assert_eq!(
            kinds,
            [("ai", Kind::Segments), ("theme", Kind::Segments), ("accent", Kind::Swatches), ("motion", Kind::Switch), ("start", Kind::Dropdown), ("remote", Kind::Segments), ("notify", Kind::Switch)]
        );
        for (_, rows) in &secs {
            for r in rows {
                assert!(r.choices.iter().any(|c| c.id == r.current), "{} shows its value", r.key);
            }
        }
        let motion = &secs[1].1[2];
        assert_eq!(motion.current, "on", "full motion is on");
        assert_eq!(step_choice(motion, -1).as_deref(), Some("off"));
        let mut s = Settings::default();
        s.motion = "reduced".into();
        s.notify_behind = false;
        let secs = sections(&s, &[]);
        assert_eq!((secs[1].1[2].current.as_str(), secs[3].1[1].current.as_str()), ("off", "off"));
        assert!(secs[1].1[0].choices.iter().map(|c| c.label.as_str()).eq(["Dark", "Light", "System"]));
        assert!(secs[3].1[0].choices.iter().map(|c| c.label.as_str()).eq(["Off", "5 min", "10 min", "30 min"]));
    }

    #[test]
    fn old_colour_names_still_work() {
        assert_eq!(store::colour_name("violet"), Some("purple"));
        assert_eq!(store::colour_name("mint"), Some("mint"), "mint is a colour of its own again");
        assert_eq!(store::colour_name("orange"), Some("orange"));
        assert_eq!(store::colour_name("magenta"), Some("magenta"));
        assert_eq!(store::colour_name("amber"), None);
        let mut p = ProjectSummary {
            id: "x".into(),
            name: "x".into(),
            roots: vec![],
            removed: false,
            desc: String::new(),
            next: String::new(),
            why: String::new(),
            live: LiveCounts::default(),
            last_at: None,
            sessions_14d: 0,
            ideas: 0,
            git: GitTotals::default(),
            paused: None,
            last_summary: None,
            git_checked: true,
            color: Some("violet".into()),
            remote_problem: None,
        };
        assert_eq!(colour_of(&p), "purple");
        p.color = Some("nonsense".into());
        assert_eq!(colour_of(&p), store::default_colour("x"));
    }

    #[test]
    fn default_colours_are_stable_and_spread() {
        assert_eq!(store::default_colour("Wake"), store::default_colour("wake"));
        assert_eq!(store::default_colour("Gradebook"), store::default_colour("Gradebook"));
        let names = ["Wake", "Gradebook", "Skyhook", "tideline-desktop", "dotfiles", "notes", "alpha", "beta"];
        let distinct: std::collections::HashSet<&str> = names.iter().map(|n| store::default_colour(n)).collect();
        assert!(distinct.len() >= 4, "{distinct:?}");
        // Fixed values: a change to the hash would recolour every project, so it is pinned.
        assert_eq!((store::default_colour("wake"), store::default_colour("fernlight"), store::default_colour("Gradebook")), ("green", "orange", "purple"));
    }
}
