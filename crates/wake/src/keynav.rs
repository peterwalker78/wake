//! Moving round the window with the keyboard.
//!
//! The window has regions, in the order they sit on screen: the title bar, the filters (the
//! controls above the list), the list (the view itself), and the details. Tab and Shift+Tab are
//! GTK's own, control by control in that same order, with a list as one stop (as in any GTK app).
//! The arrows move within the region that has the keyboard and never leave it; in the list they
//! move the selection. F6 and Shift+F6 jump region to region. The region the keyboard is in gets a
//! quiet accent edge, but only after keyboard use (GTK's focus-visible). Esc in the filters or the
//! title bar steps back to the list.
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
    /// The view tabs along the bottom of a narrow window.
    Tabs,
}

pub const ORDER: [Region; 6] = [Region::Top, Region::Rail, Region::Filters, Region::List, Region::Panel, Region::Tabs];

impl Region {
    pub fn name(self) -> &'static str {
        match self {
            Region::Rail => "views and projects",
            Region::Filters => "filters",
            Region::List => "list",
            Region::Panel => "details",
            Region::Top => "top bar",
            Region::Tabs => "view tabs",
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
        Region::Tabs => Some(ui.w.tabs_bottom.clone().upcast()),
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
pub fn controls(w: &gtk::Widget) -> Vec<gtk::Widget> {
    fn walk(w: &gtk::Widget, out: &mut Vec<gtk::Widget>) {
        // Only what is on screen: not another page of a stack, nor a folded part.
        if !w.is_visible() || !w.is_child_visible() || !w.is_sensitive() {
            return;
        }
        if focusable(w)
            && (w.is::<gtk::Button>()
                || w.is::<gtk::CheckButton>()
                || w.is::<gtk::Entry>()
                || w.is::<gtk::DropDown>()
                || w.is::<gtk::ListBoxRow>()
                || w.is::<gtk::FlowBoxChild>()
                || w.is::<gtk::Switch>()
                || w.is::<gtk::TextView>())
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

fn shown(ui: &Ui, r: Region) -> bool {
    // A rail tucked away is not a stop: the keyboard never opens what the eye can't see.
    if r == Region::Rail && ui.st.borrow().lay.rail == crate::layout::Rail::Hidden && !ui.w.body.rail_open() {
        return false;
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
    for r in [Region::Panel, Region::Filters, Region::List, Region::Rail, Region::Top, Region::Tabs] {
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
        Region::Tabs => {
            if let Some(b) = first(&controls(&w), |b| b.has_css_class("on")) {
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

/// Arrows inside the rail (up and down) and the filters (left and right).
fn step_within(ui: &Ui, r: Region, f: &gtk::Widget, d: i32) -> bool {
    let Some(w) = widget(ui, r) else { return false };
    let cs = controls(&w);
    let Some(i) = cs.iter().position(|c| c == f || f.is_ancestor(c)) else { return false };
    let j = (i as i32 + d).clamp(0, cs.len() as i32 - 1) as usize;
    cs[j].grab_focus();
    true
}

/// The words on a control (its label, else its tooltip), for the replay's log.
fn label_of(w: &gtk::Widget) -> Option<String> {
    if let Some(b) = w.downcast_ref::<gtk::Button>()
        && let Some(l) = b.label()
    {
        return Some(l.to_string());
    }
    fn first_label(w: &gtk::Widget) -> Option<String> {
        if let Some(l) = w.downcast_ref::<gtk::Label>() {
            return Some(l.text().to_string()).filter(|t| !t.trim().is_empty());
        }
        let mut c = w.first_child();
        while let Some(x) = c {
            if let Some(t) = first_label(&x) {
                return Some(t);
            }
            c = x.next_sibling();
        }
        None
    }
    first_label(w).or_else(|| w.tooltip_text().map(|t| t.to_string())).map(|t| t.chars().take(30).collect())
}

/// The list item (a selectable row or a card) holding `w`, if any.
fn item_of(w: &gtk::Widget) -> Option<gtk::Widget> {
    let mut p = Some(w.clone());
    while let Some(x) = p {
        if x.is::<gtk::FlowBoxChild>() || x.downcast_ref::<gtk::ListBoxRow>().is_some_and(|r| r.is_selectable()) {
            return Some(x);
        }
        p = x.parent();
    }
    None
}

/// The next (or previous) control in the window after `from`, wrapping round, that takes the focus.
fn step_on(ui: &Ui, from: Option<&gtk::Widget>, back: bool) {
    let all = controls(ui.window.upcast_ref());
    if all.is_empty() {
        return;
    }
    let n = all.len();
    let at = from.and_then(|f| all.iter().rposition(|c| c == f || f.is_ancestor(c)));
    for k in 1..=n {
        let i = match at {
            Some(a) if back => (a + n - k) % n,
            Some(a) => (a + k) % n,
            None if back => n - k,
            None => k - 1,
        };
        if Some(&all[i]) != from && all[i].grab_focus() {
            return;
        }
    }
}

/// The list items (rows, cards) under `root` that Tab skips while the keyboard is on `f`: every
/// item but the one holding it, and rows with nothing to press (a heading). Rows that only hold
/// controls of their own (column heads, a group's "Show them") stay stops: they are buttons.
fn items_besides(root: &gtk::Widget, f: &gtk::Widget) -> Vec<gtk::Widget> {
    fn walk(w: &gtk::Widget, f: &gtk::Widget, out: &mut Vec<gtk::Widget>) {
        let heading = || w.downcast_ref::<gtk::ListBoxRow>().is_some_and(|r| !r.is_selectable() && controls(w).iter().all(|c| c == w));
        let item = w.is::<gtk::FlowBoxChild>() || w.downcast_ref::<gtk::ListBoxRow>().is_some_and(|r| r.is_selectable()) || heading();
        let holds = w == f || f.is_ancestor(w);
        if item && !holds && w.is_sensitive() {
            out.push(w.clone());
            return;
        }
        let mut c = w.first_child();
        while let Some(x) = c {
            walk(&x, f, out);
            c = x.next_sibling();
        }
    }
    let mut out = Vec::new();
    walk(root, f, &mut out);
    out
}

/// A move the arrow keys (and Home, End) make among controls.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Arrow {
    Up,
    Down,
    Left,
    Right,
    First,
    Last,
}

fn arrow_key(key: gdk::Key) -> Option<Arrow> {
    use gdk::Key as K;
    Some(match key {
        K::Up | K::KP_Up => Arrow::Up,
        K::Down | K::KP_Down => Arrow::Down,
        K::Left | K::KP_Left => Arrow::Left,
        K::Right | K::KP_Right => Arrow::Right,
        K::Home | K::KP_Home => Arrow::First,
        K::End | K::KP_End => Arrow::Last,
        _ => return None,
    })
}

/// A control's place: x, y, width, height.
type Rect = (f64, f64, f64, f64);

/// Where the arrow `a` goes from control `at` (x, y, width, height) among `others`: up and down to
/// the nearest control above or below (the closest row, then the closest across); left and right to
/// the nearest on the same line, else the previous or next in order. Pure, for tests.
pub fn arrow_target(at: usize, rects: &[(f64, f64, f64, f64)], a: Arrow) -> usize {
    let n = rects.len();
    if n == 0 {
        return at;
    }
    let (x, y, w, h) = rects[at];
    let (cx, cy) = (x + w / 2.0, y + h / 2.0);
    let pick = |keep: &dyn Fn(&Rect) -> bool, cost: &dyn Fn(&Rect) -> (f64, f64)| {
        rects
            .iter()
            .enumerate()
            .filter(|(i, r)| *i != at && keep(r))
            .min_by(|a, b| {
                let (ca, cb) = (cost(a.1), cost(b.1));
                ca.0.total_cmp(&cb.0).then(ca.1.total_cmp(&cb.1))
            })
            .map(|(i, _)| i)
    };
    let same_line = |r: &(f64, f64, f64, f64)| r.1 < y + h - 2.0 && r.1 + r.3 > y + 2.0;
    // Across, for up and down: among the controls this one spans, the left-most (the first of a
    // row of buttons); else the one nearest across.
    let across = |r: &(f64, f64, f64, f64)| if r.0 < x + w && r.0 + r.2 > x { (r.0 - x).abs() } else { 1e6 + (r.0 + r.2 / 2.0 - cx).abs() };
    match a {
        Arrow::First => 0,
        Arrow::Last => n - 1,
        Arrow::Down => pick(&|r| r.1 >= y + h - 2.0, &|r| (r.1 - y, across(r))).unwrap_or(at),
        Arrow::Up => pick(&|r| r.1 + r.3 <= y + 2.0, &|r| (cy - (r.1 + r.3 / 2.0), across(r))).unwrap_or(at),
        Arrow::Right => pick(&|r| same_line(r) && r.0 >= x + w - 2.0, &|r| (r.0 - x, 0.0)).unwrap_or(at),
        Arrow::Left => pick(&|r| same_line(r) && r.0 + r.2 <= x + 2.0, &|r| (x - r.0, 0.0)).unwrap_or(at),
    }
}

/// Moves the focus among the controls of `area` (the details, the title bar, a menu) by `a`.
fn arrow_within(area: &gtk::Widget, f: &gtk::Widget, a: Arrow) {
    let cs = controls(area);
    let Some(at) = cs.iter().position(|c| c == f || f.is_ancestor(c)) else {
        if let Some(c) = cs.first() {
            c.grab_focus();
        }
        return;
    };
    // Just redrawn and not laid out yet: a step in reading order rather than nothing.
    if cs.iter().any(|c| c.width() == 0 || c.height() == 0) {
        let to = match a {
            Arrow::Down | Arrow::Right => (at + 1).min(cs.len() - 1),
            Arrow::Up | Arrow::Left => at.saturating_sub(1),
            Arrow::First => 0,
            Arrow::Last => cs.len() - 1,
        };
        cs[to].grab_focus();
        return;
    }
    let rects: Vec<(f64, f64, f64, f64)> =
        cs.iter().map(|c| c.compute_bounds(area).map_or((0.0, 0.0, 0.0, 0.0), |b| (f64::from(b.x()), f64::from(b.y()), f64::from(b.width()), f64::from(b.height())))).collect();
    let to = arrow_target(at, &rects, a);
    cs[to].grab_focus();
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
        // Tab in a list: GTK's own move, with the list as one stop (as on any desktop): from a
        // row to its own buttons, then out of the list; never on to the next row.
        K::Tab | K::KP_Tab | K::ISO_Left_Tab => {
            let st = ui.st.borrow();
            if st.modal_open || st.sw.open {
                return None;
            }
            let v = st.view;
            drop(st);
            let root: gtk::Widget = ui.w.views[&v].root.clone().upcast();
            let back = shift || key == K::ISO_Left_Tab;
            let off = focus(ui).filter(|f| f.is_ancestor(&root)).map(|f| items_besides(&root, &f)).unwrap_or_default();
            // Out of reach for this one move (and back before anything is drawn): a list and a
            // card grid both pass over items that can't be used.
            for w in &off {
                w.set_sensitive(false);
            }
            let before = focus(ui);
            ui.window.emit_by_name::<()>("move-focus", &[&if back { gtk::DirectionType::TabBackward } else { gtk::DirectionType::TabForward }]);
            // GTK said it moved but didn't (a scrolling area with nothing to focus does that):
            // the next control on screen, so a press is never lost.
            if focus(ui) == before {
                step_on(ui, before.as_ref(), back);
            }
            for w in &off {
                w.set_sensitive(true);
            }
            // Landed on a list's scrolling area or the list itself (no row it could take, as when
            // nothing is selected yet), or came into the list on an item that isn't the selected
            // one (a card grid comes in at its end): the selected item, else the first.
            if let Some(f) = focus(ui)
                && f.is_ancestor(&root)
            {
                let bare = f.is::<gtk::ScrolledWindow>() || f.is::<gtk::Viewport>() || f.is::<gtk::ListBox>() || f.is::<gtk::FlowBox>();
                let came_in = !before.as_ref().is_some_and(|b| b.is_ancestor(&root));
                let item = item_of(&f).map(|i| i.widget_name().to_string());
                let sel = ui.st.borrow().sel.clone();
                if bare || (came_in && item.is_some() && item != sel) {
                    app::focus_list(ui);
                }
            }
            return Some(Stop);
        }
        K::F6 => {
            if !ui.st.borrow().modal_open && !ui.st.borrow().sw.open {
                cycle(ui, !shift);
                return Some(Stop);
            }
            return None;
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
    // Typing: the keys are the text's, except that ↑ and ↓ leave a one-line box (in a menu or
    // the details), as in any form.
    let line = f.ancestor(gtk::Entry::static_type()).is_some();
    if f.is::<gtk::TextView>() || ((f.is::<gtk::Text>() || line) && !(line && matches!(key, K::Up | K::Down | K::KP_Up | K::KP_Down))) {
        return None;
    }
    // An open menu: the arrows move among its items, as in any menu.
    if let Some(pop) = f.ancestor(gtk::Popover::static_type()) {
        return arrow_key(key).map(|a| {
            arrow_within(&pop, &f, a);
            Stop
        });
    }
    let r = region_of(ui, &f)?;
    // The details and the title bar: the arrows move among their own controls and never leave
    // them for the list (the list has them only while it has the focus).
    // ↓ from the title bar goes into the content, as from a toolbar.
    if r == Region::Top && arrow_key(key) == Some(Arrow::Down) {
        app::focus_list(ui);
        return Some(Stop);
    }
    if matches!(r, Region::Panel | Region::Top | Region::Tabs)
        && let Some(a) = arrow_key(key)
    {
        if let Some(w) = widget(ui, r) {
            arrow_within(&w, &f, a);
        }
        return Some(Stop);
    }
    let d = match (r, key) {
        (Region::Rail, K::Down) => 1,
        (Region::Rail, K::Up) => -1,
        (Region::Filters, K::Right) => 1,
        (Region::Filters, K::Left) => -1,
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
    Redraw,
}

pub fn parse(seq: &str) -> Result<Vec<Step>, String> {
    let mut out = Vec::new();
    for tok in seq.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        if tok == "wait" {
            out.push(Step::Wait);
            continue;
        }
        if tok == "redraw" {
            out.push(Step::Redraw);
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
fn default_action(ui: &Ui, key: gdk::Key, mods: gdk::ModifierType) {
    use gdk::Key as K;
    // Tab: GTK's own (the window's move-focus binding, as a real key press runs it).
    if matches!(key, K::Tab | K::KP_Tab | K::ISO_Left_Tab) {
        let back = key == K::ISO_Left_Tab || mods.contains(gdk::ModifierType::SHIFT_MASK);
        let dir = if back { gtk::DirectionType::TabBackward } else { gtk::DirectionType::TabForward };
        ui.window.emit_by_name::<()>("move-focus", &[&dir]);
        return;
    }
    let Some(f) = focus(ui) else { return };
    // Esc in a menu: GTK's popover closes itself (and the keyboard goes back to its button).
    if key == K::Escape
        && let Some(p) = f.ancestor(gtk::Popover::static_type()).and_downcast::<gtk::Popover>()
    {
        p.popdown();
        return;
    }
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
                // As when new data arrives: the details and the list are drawn again.
                Step::Redraw => {
                    ui.st.borrow_mut().panel_drawn = None;
                    crate::panel::render(&ui);
                    crate::views::render_view(&ui);
                }
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
                    "keys: after step {i}: focus {:?} in {:?} label {:?} drawer {}",
                    f.as_ref().map(|f| (f.type_().name(), f.widget_name().to_string(), f.state_flags(), f.css_classes())),
                    f.as_ref().and_then(|f| region_of(&ui, f)).map(Region::name),
                    f.as_ref().and_then(label_of),
                    ui.st.borrow().drawer.is_some()
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
    fn regions_go_in_screen_order_and_wrap() {
        let all = ORDER.to_vec();
        assert_eq!(next_region(&all, Some(Top), true), Some(Rail));
        assert_eq!(next_region(&all, Some(Filters), true), Some(List));
        assert_eq!(next_region(&all, Some(List), true), Some(Panel));
        assert_eq!(next_region(&all, Some(Tabs), true), Some(Top), "wraps round");
        assert_eq!(next_region(&all, Some(Top), false), Some(Tabs));
        assert_eq!(next_region(&all, None, true), Some(Top));
        assert_eq!(next_region(&all, None, false), Some(Tabs));
    }

    #[test]
    fn arrows_go_to_the_nearest_control_that_way() {
        use Arrow::*;
        // A row of three buttons, then two rows of one (a details panel: actions, then sections).
        let r = [(0.0, 0.0, 100.0, 30.0), (110.0, 0.0, 60.0, 30.0), (300.0, 0.0, 30.0, 30.0), (0.0, 50.0, 400.0, 40.0), (0.0, 90.0, 400.0, 40.0)];
        assert_eq!(arrow_target(0, &r, Right), 1);
        assert_eq!(arrow_target(1, &r, Right), 2);
        assert_eq!(arrow_target(2, &r, Left), 1);
        assert_eq!(arrow_target(3, &r, Right), 3, "nothing on its line: it stays");
        assert_eq!(arrow_target(1, &r, Down), 3, "down from a button: the row under it");
        assert_eq!(arrow_target(3, &r, Down), 4);
        assert_eq!(arrow_target(4, &r, Down), 4, "the last stays: the keys never leave the area");
        assert_eq!(arrow_target(3, &r, Up), 0, "up from a wide row: the first button of the row above");
        assert_eq!(arrow_target(4, &r, Up), 3);
        assert_eq!(arrow_target(0, &r, Up), 0);
        assert_eq!((arrow_target(2, &r, First), arrow_target(0, &r, Last)), (0, 4));
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
