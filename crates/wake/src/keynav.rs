//! Moving round the window with the keyboard.
//!
//! The window has regions, in this order: the rail (views and projects), the filters (the chips
//! above the list), the list (the view itself: its search box, column heads and rows), the side
//! panel, the capture bar, and the top bar. Tab and Shift+Tab move through a region's controls and
//! then on to the next region; the rail and the filters are one stop each (arrows move inside
//! them, as in a toolbar). F6 and Shift+F6 jump region to region. The region the keyboard is in
//! gets a quiet accent edge, but only after keyboard use (GTK's focus-visible). Esc in the rail,
//! the filters or the top bar steps back to the list.
//!
//! `--show keys:<sequence>` replays keys through the same handler, for focus screenshots.

use std::time::Duration;

use gtk4::{self as gtk, gdk, glib, prelude::*};

use crate::app::{self, Ui};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    Rail,
    Filters,
    List,
    Panel,
    Top,
}

pub const ORDER: [Region; 5] = [Region::Rail, Region::Filters, Region::List, Region::Panel, Region::Top];

impl Region {
    /// One Tab stop, arrows inside (a toolbar or a menu of links).
    pub fn composite(self) -> bool {
        matches!(self, Region::Rail | Region::Filters)
    }

    pub fn name(self) -> &'static str {
        match self {
            Region::Rail => "views and projects",
            Region::Filters => "filters",
            Region::List => "list",
            Region::Panel => "details",
            Region::Top => "top bar",
        }
    }
}

/// The region after (or before) `from` among those `shown`, wrapping round. Pure, for tests.
pub fn next_region(shown: &[Region], from: Option<Region>, forward: bool) -> Option<Region> {
    let order: Vec<Region> = ORDER.iter().copied().filter(|r| shown.contains(r)).collect();
    if order.is_empty() {
        return None;
    }
    let Some(from) = from else { return Some(if forward { order[0] } else { order[order.len() - 1] }) };
    // `from` may be hidden now (a panel just closed): step from where it would be.
    let at = ORDER.iter().position(|r| *r == from).unwrap_or(0);
    let n = ORDER.len();
    for k in 1..=n {
        let i = if forward { (at + k) % n } else { (at + n - k) % n };
        if order.contains(&ORDER[i]) {
            return Some(ORDER[i]);
        }
    }
    None
}

fn widget(ui: &Ui, r: Region) -> Option<gtk::Widget> {
    match r {
        // Slim, the keyboard works in the full form that slides out over the strip.
        Region::Rail if ui.w.body.peek_open() => Some(ui.w.peek_rail.clone().upcast()),
        Region::Rail => ui.w.nav.parent(),
        Region::Filters => Some(ui.w.chips.clone().upcast()),
        Region::List => {
            let v = ui.st.borrow().view;
            Some(ui.w.views[&v].root.clone().upcast())
        }
        Region::Panel => Some(ui.w.drawer.clone().upcast()),
        Region::Top => {
            // The top bar (the behind banner may sit above it in the column).
            let mut c = ui.w.column.first_child();
            while let Some(x) = c {
                if x.has_css_class("top") {
                    return Some(x);
                }
                // It is the window's title bar, inside the handle that lets it drag the window.
                if let Some(inner) = x.first_child().filter(|i| x.is::<gtk::WindowHandle>() && i.has_css_class("top")) {
                    return Some(inner);
                }
                c = x.next_sibling();
            }
            None
        }
    }
}

fn focusable(w: &gtk::Widget) -> bool {
    // An entry and a drop-down take the focus through a child (their text, their button).
    let takes = w.is_focusable() || w.is::<gtk::Entry>() || w.is::<gtk::DropDown>();
    w.is_visible() && w.is_child_visible() && w.is_sensitive() && takes && w.can_focus() && !w.is::<gtk::Label>()
}

/// The focusable controls inside `w`, in order.
fn controls(w: &gtk::Widget) -> Vec<gtk::Widget> {
    fn walk(w: &gtk::Widget, out: &mut Vec<gtk::Widget>) {
        if !w.is_visible() || !w.is_sensitive() {
            return;
        }
        if focusable(w)
            && (w.is::<gtk::Button>() || w.is::<gtk::CheckButton>() || w.is::<gtk::Entry>() || w.is::<gtk::DropDown>() || w.is::<gtk::ListBoxRow>() || w.is::<gtk::Switch>() || w.is::<gtk::TextView>())
        {
            out.push(w.clone());
            // A drop-down's own button is part of it.
            if w.is::<gtk::DropDown>() {
                return;
            }
        }
        let mut c = w.first_child();
        while let Some(x) = c {
            walk(&x, out);
            c = x.next_sibling();
        }
    }
    let mut out = Vec::new();
    walk(w, &mut out);
    out
}

/// A region's Tab stops: its controls, where a list counts as one stop (its current row, then
/// that row's own controls) and the other rows' controls are skipped.
fn tab_stops(w: &gtk::Widget, focus: &gtk::Widget) -> Vec<gtk::Widget> {
    let row_of = |c: &gtk::Widget| -> Option<gtk::ListBoxRow> { c.downcast_ref::<gtk::ListBoxRow>().cloned().or_else(|| c.ancestor(gtk::ListBoxRow::static_type()).and_downcast()) };
    let cs = controls(w);
    // Per list: the row holding the focus, else the selected one, else the first.
    let current = |list: &gtk::ListBox| -> Option<gtk::ListBoxRow> {
        let rows: Vec<gtk::ListBoxRow> = cs.iter().filter_map(|c| c.downcast_ref::<gtk::ListBoxRow>().cloned()).filter(|r| r.parent().as_ref() == Some(list.upcast_ref())).collect();
        rows.iter().find(|r| focus == r.upcast_ref::<gtk::Widget>() || focus.is_ancestor(*r)).cloned().or_else(|| list.selected_row().filter(|s| rows.contains(s))).or_else(|| rows.first().cloned())
    };
    cs.iter()
        .filter(|c| match row_of(c) {
            None => true,
            Some(row) => match row.parent().and_downcast::<gtk::ListBox>() {
                Some(list) => current(&list).as_ref() == Some(&row),
                None => true,
            },
        })
        .cloned()
        .collect()
}

fn shown(ui: &Ui, r: Region) -> bool {
    // In a narrow window the rail is behind ☰: still a stop, it opens as a drawer.
    if r == Region::Rail && ui.st.borrow().lay.rail == crate::layout::Rail::Hidden {
        return true;
    }
    let Some(w) = widget(ui, r) else { return false };
    if !w.is_visible() || !w.is_mapped() {
        return false;
    }
    match r {
        // Chips with nothing to press (History's) are not a stop.
        Region::Filters => !controls(&w).is_empty(),
        Region::Panel => ui.st.borrow().drawer.is_some(),
        _ => true,
    }
}

/// The region holding `f`.
pub fn region_of(ui: &Ui, f: &gtk::Widget) -> Option<Region> {
    if f.is_ancestor(&ui.w.peek_rail) {
        return Some(Region::Rail);
    }
    // Inner regions first (the list's root sits inside the main column with the chips).
    for r in [Region::Panel, Region::Filters, Region::List, Region::Rail, Region::Top] {
        if let Some(w) = widget(ui, r)
            && (f == &w || f.is_ancestor(&w))
        {
            return Some(r);
        }
    }
    None
}

fn focus(ui: &Ui) -> Option<gtk::Widget> {
    gtk::prelude::GtkWindowExt::focus(&ui.window)
}

fn first(ws: &[gtk::Widget], on: impl Fn(&gtk::Widget) -> bool) -> Option<gtk::Widget> {
    ws.iter().find(|w| on(w)).or(ws.first()).cloned()
}

/// Puts the keyboard on a region's natural place: the current view in the rail, the chosen chip,
/// the selected row, the panel's main button, the capture box (which asks for the project).
pub fn home(ui: &Ui, r: Region) {
    if r == Region::Rail && ui.st.borrow().lay.rail == crate::layout::Rail::Hidden && !ui.w.body.rail_open() {
        // The drawer opens with the keyboard on the current view.
        app::set_rail_open(ui, true);
        return;
    }
    if r == Region::Rail && ui.st.borrow().lay.slim() {
        // Slim: the full form slides out at once, with the keyboard on the same item it would
        // be on in the strip.
        ui.w.body.slide(|s| s.focus(true));
    }
    let Some(w) = widget(ui, r) else { return };
    match r {
        Region::Rail | Region::Filters => {
            if let Some(b) = first(&controls(&w), |b| b.has_css_class("on")) {
                b.grab_focus();
            }
        }
        Region::List => {
            app::focus_list(ui);
            if !focus(ui).is_some_and(|f| f.is_ancestor(&w)) {
                w.child_focus(gtk::DirectionType::TabForward);
            }
        }
        Region::Panel => {
            crate::panel::focus_primary(ui);
            if !focus(ui).is_some_and(|f| f.is_ancestor(&w)) {
                w.child_focus(gtk::DirectionType::TabForward);
            }
        }
        Region::Top => {
            if let Some(b) = controls(&w).first() {
                b.grab_focus();
            }
        }
    }
}

fn regions_shown(ui: &Ui) -> Vec<Region> {
    ORDER.iter().copied().filter(|r| shown(ui, *r)).collect()
}

/// To the next (or previous) region.
pub fn cycle(ui: &Ui, forward: bool) {
    let cur = focus(ui).and_then(|f| region_of(ui, &f));
    if let Some(r) = next_region(&regions_shown(ui), cur, forward) {
        home(ui, r);
    }
}

/// Tab: through the region's controls, then on to the next region.
pub fn tab(ui: &Ui, forward: bool) {
    let st = ui.st.borrow();
    if st.modal_open || st.sw.open {
        drop(st);
        ui.window.child_focus(if forward { gtk::DirectionType::TabForward } else { gtk::DirectionType::TabBackward });
        return;
    }
    drop(st);
    let Some(f) = focus(ui) else {
        cycle(ui, forward);
        return;
    };
    let Some(r) = region_of(ui, &f) else {
        cycle(ui, forward);
        return;
    };
    if !r.composite()
        && let Some(w) = widget(ui, r)
    {
        let cs = tab_stops(&w, &f);
        let at = cs.iter().position(|c| *c == f || f.is_ancestor(c));
        if std::env::var_os("WAKE_SHOT_DEBUG").is_some() {
            eprintln!("tab: {} stops in {:?}, focus at {at:?}: {:?}", cs.len(), r, cs.iter().map(|c| c.type_().name()).collect::<Vec<_>>());
        }
        let next = match at {
            Some(i) if forward => cs.get(i + 1),
            Some(i) => i.checked_sub(1).and_then(|j| cs.get(j)),
            None => None,
        };
        if let Some(n) = next {
            n.grab_focus();
            return;
        }
    }
    cycle(ui, forward);
}

/// Arrows inside the rail (up and down) and the filters (left and right).
fn step_within(ui: &Ui, r: Region, f: &gtk::Widget, d: i32) -> bool {
    let Some(w) = widget(ui, r) else { return false };
    let cs = controls(&w);
    let Some(i) = cs.iter().position(|c| c == f || f.is_ancestor(c)) else { return false };
    let j = (i as i32 + d).clamp(0, cs.len() as i32 - 1) as usize;
    cs[j].grab_focus();
    true
}

/// Tab, Shift+Tab, F6, arrows in the rail and filters, Esc out of them. None for every other key.
pub fn on_key(ui: &Ui, key: gdk::Key, mods: gdk::ModifierType) -> Option<glib::Propagation> {
    use gdk::Key as K;
    use glib::Propagation::Stop;
    let shift = mods.contains(gdk::ModifierType::SHIFT_MASK);
    if mods.intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK) {
        return None;
    }
    match key {
        K::F6 => {
            if !ui.st.borrow().modal_open && !ui.st.borrow().sw.open {
                cycle(ui, !shift);
                return Some(Stop);
            }
            return None;
        }
        K::Tab | K::KP_Tab | K::ISO_Left_Tab => {
            tab(ui, !(shift || key == K::ISO_Left_Tab));
            return Some(Stop);
        }
        _ => {}
    }
    {
        let st = ui.st.borrow();
        if st.modal_open || st.sw.open {
            return None;
        }
    }
    let f = focus(ui)?;
    let r = region_of(ui, &f)?;
    let d = match (r, key.to_lower()) {
        (Region::Rail, K::Down | K::j) => 1,
        (Region::Rail, K::Up | K::k) => -1,
        (Region::Filters, K::Right | K::l) => 1,
        (Region::Filters, K::Left | K::h) => -1,
        (Region::Rail | Region::Filters | Region::Top, K::Escape) => {
            app::set_rail_open(ui, false);
            app::focus_list(ui);
            return Some(Stop);
        }
        _ => return None,
    };
    step_within(ui, r, &f, d);
    Some(Stop)
}

/// Marks the region the keyboard is in (class `here`), only while focus is visible.
fn mark(ui: &Ui) {
    let f = focus(ui);
    let here = f.as_ref().filter(|_| ui.window.gets_focus_visible()).and_then(|f| region_of(ui, f));
    // The slim strip and its full form: only the one in use carries the mark.
    ui.w.peek_rail.remove_css_class("here");
    if let Some(r) = ui.w.nav.parent() {
        r.remove_css_class("here");
    }
    for r in ORDER {
        let Some(w) = widget(ui, r) else { continue };
        if Some(r) == here {
            w.add_css_class("here");
        } else {
            w.remove_css_class("here");
        }
    }
    // The view roots that aren't showing lose it too.
    let v = ui.st.borrow().view;
    for (k, vw) in &ui.w.views {
        if *k != v {
            vw.root.remove_css_class("here");
        }
    }
}

pub fn build(ui: &Ui) {
    {
        let ui2 = ui.clone();
        ui.window.connect_focus_widget_notify(move |win| {
            let _ = gtk::prelude::GtkWindowExt::focus(win);
            mark(&ui2);
        });
    }
    {
        let ui2 = ui.clone();
        ui.window.connect_focus_visible_notify(move |_| mark(&ui2));
    }
}

// ---------- replaying keys for screenshots ----------

/// One step of `--show keys:<sequence>`: comma-separated keys by GDK name (`Tab`, `Shift+Tab`,
/// `F6`, `Down`, `Return`, `Escape`, `j`, `c`...), `type:<text>` to type text, or `wait`.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Key(gdk::Key, gdk::ModifierType),
    Type(String),
    /// `wait`: a beat with no key.
    Wait,
}

pub fn parse(seq: &str) -> Result<Vec<Step>, String> {
    let mut out = Vec::new();
    for tok in seq.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        if tok == "wait" {
            out.push(Step::Wait);
            continue;
        }
        if let Some(t) = tok.strip_prefix("type:") {
            out.push(Step::Type(t.to_string()));
            continue;
        }
        let mut mods = gdk::ModifierType::empty();
        let mut name = tok;
        loop {
            if let Some(rest) = name.strip_prefix("Shift+") {
                mods |= gdk::ModifierType::SHIFT_MASK;
                name = rest;
            } else if let Some(rest) = name.strip_prefix("Ctrl+") {
                mods |= gdk::ModifierType::CONTROL_MASK;
                name = rest;
            } else {
                break;
            }
        }
        let key = match name {
            "Enter" => gdk::Key::Return,
            "Esc" => gdk::Key::Escape,
            "Space" => gdk::Key::space,
            n => gdk::Key::from_name(n).ok_or_else(|| format!("no key called {n}"))?,
        };
        let key = if key == gdk::Key::Tab && mods.contains(gdk::ModifierType::SHIFT_MASK) { gdk::Key::ISO_Left_Tab } else { key };
        out.push(Step::Key(key, mods));
    }
    Ok(out)
}

/// What GTK itself would do with a key Wake's handler let through (a replay has no real event).
fn default_action(ui: &Ui, key: gdk::Key, _mods: gdk::ModifierType) {
    use gdk::Key as K;
    let Some(f) = focus(ui) else { return };
    match key {
        K::Return | K::KP_Enter | K::space => {
            if let Some(e) = f.ancestor(gtk::Entry::static_type()).and_downcast::<gtk::Entry>() {
                if key == K::space {
                    type_text(ui, " ");
                } else {
                    e.emit_activate();
                }
            } else if let Some(row) = f.downcast_ref::<gtk::ListBoxRow>() {
                if let Some(list) = row.parent().and_downcast::<gtk::ListBox>() {
                    list.emit_by_name::<()>("row-activated", &[row]);
                }
            } else {
                f.activate();
            }
        }
        K::Left | K::Right | K::Up | K::Down => {
            let d = match key {
                K::Left => gtk::DirectionType::Left,
                K::Right => gtk::DirectionType::Right,
                K::Up => gtk::DirectionType::Up,
                _ => gtk::DirectionType::Down,
            };
            ui.window.child_focus(d);
        }
        _ => {
            if let Some(c) = key.to_unicode().filter(|c| !c.is_control()) {
                type_text(ui, &c.to_string());
            }
        }
    }
}

fn type_text(ui: &Ui, t: &str) {
    let Some(f) = focus(ui) else { return };
    if let Some(e) = f.ancestor(gtk::Entry::static_type()).and_downcast::<gtk::Entry>() {
        let mut pos = e.text().chars().count() as i32;
        e.insert_text(t, &mut pos);
        e.set_position(pos);
    } else if let Some(tv) = f.downcast_ref::<gtk::TextView>() {
        tv.buffer().insert_at_cursor(t);
    }
}

/// Replays `seq` a key at a time (150 ms apart), as if typed, with focus drawn as after keyboard use.
pub fn replay(ui: &Ui, seq: &str) {
    let steps = match parse(seq) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("wake: --show keys: {e}");
            return;
        }
    };
    for (i, step) in steps.into_iter().enumerate() {
        let ui = ui.clone();
        glib::timeout_add_local_once(Duration::from_millis(150 * (i as u64 + 1)), move || {
            ui.window.set_focus_visible(true);
            match step {
                Step::Type(t) => type_text(&ui, &t),
                Step::Wait => {}
                Step::Key(k, m) => {
                    if app::on_key(&ui, k, m) == glib::Propagation::Proceed {
                        default_action(&ui, k, m);
                    }
                    // A tap: released at once (a held Space or Tab would peek).
                    crate::peek::on_release(&ui, k);
                }
            }
            if std::env::var_os("WAKE_SHOT_DEBUG").is_some() {
                let f = focus(&ui);
                let v = ui.st.borrow().view;
                eprintln!("keys: step {i}: scroll {:.0}", ui.w.views[&v].scroll.vadjustment().value());
                eprintln!(
                    "keys: after step {i}: focus {:?} in {:?}",
                    f.as_ref().map(|f| (f.type_().name(), f.widget_name().to_string(), f.state_flags(), f.css_classes())),
                    f.as_ref().and_then(|f| region_of(&ui, f)).map(Region::name)
                );
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Region::*;

    #[test]
    fn regions_go_rail_filters_list_panel_capture_top_and_wrap() {
        let all = ORDER.to_vec();
        assert_eq!(next_region(&all, Some(Rail), true), Some(Filters));
        assert_eq!(next_region(&all, Some(List), true), Some(Panel));
        assert_eq!(next_region(&all, Some(Top), true), Some(Rail), "wraps round");
        assert_eq!(next_region(&all, Some(Rail), false), Some(Top));
        assert_eq!(next_region(&all, None, true), Some(Rail));
        assert_eq!(next_region(&all, None, false), Some(Top));
    }

    #[test]
    fn hidden_regions_are_skipped_even_the_one_just_left() {
        let no_panel = [Rail, Filters, List, Top];
        assert_eq!(next_region(&no_panel, Some(List), true), Some(Top));
        assert_eq!(next_region(&no_panel, Some(Top), false), Some(List));
        // The panel closed while the keyboard was in it.
        assert_eq!(next_region(&no_panel, Some(Panel), true), Some(Top));
        assert_eq!(next_region(&no_panel, Some(Panel), false), Some(List));
        assert_eq!(next_region(&[], Some(List), true), None);
    }

    #[test]
    fn only_the_rail_and_the_filters_are_single_stops() {
        assert!(Rail.composite() && Filters.composite());
        assert!(!List.composite() && !Panel.composite() && !Top.composite());
    }

    #[test]
    fn key_sequences_parse_with_modifiers_and_text() {
        let s = parse("Tab, Shift+Tab, F6, j, Enter, Esc, type:hi there").unwrap();
        assert_eq!(s[0], Step::Key(gdk::Key::Tab, gdk::ModifierType::empty()));
        assert_eq!(s[1], Step::Key(gdk::Key::ISO_Left_Tab, gdk::ModifierType::SHIFT_MASK));
        assert_eq!(s[2], Step::Key(gdk::Key::F6, gdk::ModifierType::empty()));
        assert_eq!(s[3], Step::Key(gdk::Key::j, gdk::ModifierType::empty()));
        assert_eq!(s[4], Step::Key(gdk::Key::Return, gdk::ModifierType::empty()));
        assert_eq!(s[5], Step::Key(gdk::Key::Escape, gdk::ModifierType::empty()));
        assert_eq!(s[6], Step::Type("hi there".into()));
        assert!(parse("NoSuchKey").is_err());
    }
}
