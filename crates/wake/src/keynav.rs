//! Moving round the window with the keyboard.
//!
//! The window has regions, in the order they sit on screen: the title bar, the filters (the
//! controls above the list), the list (the view itself), the details, and the view tabs along the
//! bottom of a narrow window. Tab and Shift+Tab go from area to area in that order, one press each
//! (as panes do in a file manager or a mail app), landing where the keyboard last was in that area:
//! the selected item in the list, the main button the first time in the details. The arrows move
//! within the area that has the keyboard and never leave it; in the list they move the selection. F6 and Shift+F6 jump region to region. The region the keyboard is in gets a
//! quiet accent edge, but only after keyboard use (GTK's focus-visible). Esc in the filters or the
//! title bar steps back to the list.
//!
//! Wake owns where the keyboard is. GTK shows focus only after its own handlers see a Tab or an
//! arrow, and Wake takes those keys first, so `note_key` turns focus-visible on itself. And the
//! board redraws under the keyboard all the time (live counts, rows, details): a redraw that moves
//! the focus to another area, or onto nothing, or onto a scroller, without the person pressing or
//! clicking anything, is put back where the keyboard was (`settle`). So Tab always starts from the
//! area the person last saw it in.
//!
//! `--show keys:<sequence>` replays keys through the same handler, for focus screenshots.

use std::time::Duration;

use gtk4::{self as gtk, gdk, glib, prelude::*};

use crate::app::{self, Ui};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
            for x in crate::w::children(&ui.w.column) {
                if x.has_css_class("top") {
                    return Some(x);
                }
                // It is the window's title bar, inside the handle that lets it drag the window.
                if let Some(inner) = x.first_child().filter(|i| x.is::<gtk::WindowHandle>() && i.has_css_class("top")) {
                    return Some(inner);
                }
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
        for x in crate::w::children(&w) {
            walk(&x, out);
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
    // Where the keyboard last was in this area, if it is still there to go back to. Not in the
    // list (its place is the selected item), except in Ask, whose question box is in it too.
    let ask = ui.st.borrow().view == app::View::Questions;
    if (r != Region::List || ask)
        && let Some(last) = LAST.with(|l| l.borrow().get(&r).and_then(|x| x.upgrade()))
        && last.is_ancestor(&w)
        // Another view's tab: the bar's place is the current view's tab.
        && !(last.has_css_class("tab") && !last.has_css_class("on"))
        && last.is_mapped()
        && last.is_sensitive()
        && last.grab_focus()
    {
        return;
    }
    match r {
        Region::List if ask => crate::questions::focus_list(ui),
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
            // The current view's tab.
            if let Some(b) = first(&controls(&w), |b| b.has_css_class("on")) {
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

/// The areas on screen now. Details that take the list's place leave the list and its filters out.
fn regions_shown(ui: &Ui) -> Vec<Region> {
    let covered = {
        let st = ui.st.borrow();
        st.lay.push && st.drawer.is_some()
    };
    ORDER.iter().copied().filter(|r| shown(ui, *r) && !(covered && matches!(r, Region::List | Region::Filters))).collect()
}

thread_local! {
    /// Where the keyboard last was in each area, to go back to it there.
    static LAST: std::cell::RefCell<std::collections::HashMap<Region, glib::WeakRef<gtk::Widget>>> = Default::default();
}

/// To the next (or previous) region. From where the keyboard is, else from the area it was last in
/// (a redraw may have taken the focus away a moment ago). An area that can't take the keyboard
/// now is passed over, so every press lands somewhere new.
pub fn cycle(ui: &Ui, forward: bool) {
    let shown = regions_shown(ui);
    let mut from = focus(ui).filter(|f| f.is_mapped()).and_then(|f| region_of(ui, &f)).or_else(|| CURRENT.with(|c| c.get()));
    for _ in 0..shown.len() {
        let Some(r) = next_region(&shown, from, forward) else { return };
        home(ui, r);
        // The rail's drawer and slim form take the keyboard once they have slid out.
        let opening = r == Region::Rail && (ui.w.body.rail_open() || ui.w.body.peek_open());
        if opening || focus(ui).is_some_and(|f| region_of(ui, &f) == Some(r)) {
            CURRENT.with(|c| c.set(Some(r)));
            return;
        }
        from = Some(r);
    }
}

// ---------- keeping the keyboard where the person put it ----------

thread_local! {
    /// The area the keyboard is in, as the person last put it there.
    static CURRENT: std::cell::Cell<Option<Region>> = const { std::cell::Cell::new(None) };
    /// When the person last pressed a key or clicked (monotonic µs); None before the first time.
    static INPUT_AT: std::cell::Cell<Option<i64>> = const { std::cell::Cell::new(None) };
    /// A settle is queued, or running (its own focus moves are not redraws).
    static SETTLE_QUEUED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static SETTLING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Focus moves this soon after a key or a click are the person's (directly, or a moment later
/// once what they opened has drawn).
const INPUT_GRACE_US: i64 = 1_000_000;

/// The navigation keys: GTK shows where the focus is once one of these is pressed.
pub fn is_nav_key(key: gdk::Key) -> bool {
    use gdk::Key as K;
    matches!(
        key,
        K::Tab
            | K::KP_Tab
            | K::ISO_Left_Tab
            | K::Up
            | K::Down
            | K::Left
            | K::Right
            | K::KP_Up
            | K::KP_Down
            | K::KP_Left
            | K::KP_Right
            | K::Home
            | K::End
            | K::KP_Home
            | K::KP_End
            | K::Page_Up
            | K::Page_Down
            | K::KP_Page_Up
            | K::KP_Page_Down
            | K::F6
    )
}

/// A key was pressed: the person is moving the keyboard. A navigation key shows where it is, as
/// GTK would if it had seen the key (Wake handles them before GTK does).
pub fn note_key(ui: &Ui, key: gdk::Key) {
    note_input();
    if is_nav_key(key) {
        ui.window.set_focus_visible(true);
    }
}

/// A key or a click: focus moves for the next moment are the person's.
pub fn note_input() {
    INPUT_AT.with(|t| t.set(Some(glib::monotonic_time())));
}

/// Whether the person has pressed or clicked anything yet.
pub fn person_moved() -> bool {
    INPUT_AT.with(|t| t.get()).is_some()
}

fn input_recent() -> bool {
    INPUT_AT.with(|t| t.get()).is_some_and(|at| glib::monotonic_time() - at < INPUT_GRACE_US)
}

/// Something the keyboard can rest on: a control, a list's item, or a list with nothing in it.
fn is_resting_place(f: &gtk::Widget) -> bool {
    f.is::<gtk::Button>()
        || f.is::<gtk::CheckButton>()
        || f.is::<gtk::Switch>()
        || f.is::<gtk::ListBoxRow>()
        || f.is::<gtk::FlowBoxChild>()
        || f.is::<gtk::ListBox>()
        || f.is::<gtk::FlowBox>()
        || f.is::<gtk::Text>()
        || f.is::<gtk::TextView>()
        || f.ancestor(gtk::DropDown::static_type()).is_some()
}

/// While a dialog, the Find pop-up or a menu has the keyboard, its focus is its own.
fn others_own_it(ui: &Ui) -> bool {
    let st = ui.st.borrow();
    st.modal_open || st.sw.open || focus(ui).is_some_and(|f| f.ancestor(gtk::Popover::static_type()).is_some())
}

/// The focus moved. The person's moves are remembered; a move made by a redraw is checked once
/// things settle and, if it took the keyboard out of its area, put back.
fn focus_moved(ui: &Ui) {
    if SETTLING.with(|s| s.get()) {
        return;
    }
    let persons = !person_moved() || input_recent();
    // In the middle of an update: decide once it is done.
    if ui.st.try_borrow_mut().is_err() {
        let ui = ui.clone();
        glib::idle_add_local_once(move || {
            if others_own_it(&ui) {
                return;
            }
            if persons {
                remember(&ui);
            } else {
                queue_settle(&ui);
            }
        });
        return;
    }
    if others_own_it(ui) {
        return;
    }
    if persons {
        remember(ui);
    } else {
        queue_settle(ui);
    }
}

/// Remembers where the keyboard is now, as the place to come back to in its area.
fn remember(ui: &Ui) {
    if let Some(f) = focus(ui)
        && f.is_mapped()
        && let Some(r) = region_of(ui, &f)
    {
        CURRENT.with(|c| c.set(Some(r)));
        if is_resting_place(&f) {
            LAST.with(|l| l.borrow_mut().insert(r, f.downgrade()));
        }
    }
}

fn queue_settle(ui: &Ui) {
    if SETTLE_QUEUED.with(|q| q.replace(true)) {
        return;
    }
    let ui = ui.clone();
    glib::idle_add_local_once(move || {
        SETTLE_QUEUED.with(|q| q.set(false));
        settle(&ui);
    });
}

/// After a redraw: the keyboard stays in the area it was in, on something it can rest on. If that
/// area has gone (the details closed), it goes to the list.
pub fn settle(ui: &Ui) {
    if ui.st.try_borrow_mut().is_err() || others_own_it(ui) {
        return;
    }
    let Some(want) = CURRENT.with(|c| c.get()) else { return };
    let f = focus(ui);
    if f.as_ref().is_some_and(|f| f.is_mapped() && is_resting_place(f) && region_of(ui, f) == Some(want)) {
        remember(ui);
        return;
    }
    let shown = regions_shown(ui);
    // Its area isn't on screen (the details now cover the list), but the keyboard already rests
    // in one that is: it stays there, rather than going to the first area (the title bar).
    if !shown.contains(&want) && f.as_ref().is_some_and(|f| f.is_mapped() && is_resting_place(f) && region_of(ui, f).is_some_and(|r| shown.contains(&r))) {
        remember(ui);
        return;
    }
    let to = if shown.contains(&want) {
        want
    } else if shown.contains(&Region::List) {
        Region::List
    } else {
        match shown.first() {
            Some(r) => *r,
            None => return,
        }
    };
    SETTLING.with(|s| s.set(true));
    home(ui, to);
    SETTLING.with(|s| s.set(false));
    remember(ui);
}

/// The view changed: the keyboard goes to the new view's list, or, if it was on the title bar or
/// the view tabs, to the new view's tab there.
pub fn view_changed(ui: &Ui) {
    let was = focus(ui).filter(|f| f.is_mapped()).and_then(|f| region_of(ui, &f)).or_else(|| CURRENT.with(|c| c.get()));
    let to = match was {
        Some(r @ (Region::Top | Region::Tabs | Region::Rail)) => r,
        _ => Region::List,
    };
    CURRENT.with(|c| c.set(Some(to)));
    let ui = ui.clone();
    // Once the new view has drawn.
    glib::idle_add_local_once(move || {
        if others_own_it(&ui) {
            return;
        }
        let f = focus(&ui);
        let fine = f.as_ref().is_some_and(|f| f.is_mapped() && is_resting_place(f) && region_of(&ui, f) == Some(to) && !(f.has_css_class("tab") && !f.has_css_class("on")));
        if !fine {
            SETTLING.with(|s| s.set(true));
            home(&ui, to);
            SETTLING.with(|s| s.set(false));
        }
        remember(&ui);
    });
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
        for x in crate::w::children(&w) {
            if let Some(t) = first_label(&x) {
                return Some(t);
            }
        }
        None
    }
    first_label(w).or_else(|| w.tooltip_text().map(|t| t.to_string())).map(|t| t.chars().take(30).collect())
}

/// For WAKE_KEY_DEBUG: where the keyboard is now.
pub fn log_focus(ui: &Ui, after: &str) {
    let f = focus(ui);
    eprintln!(
        "key: after {after}: {} {:?} in {} visible={}",
        f.as_ref().map_or("nothing".to_string(), |f| f.type_().name().to_string()),
        f.as_ref().and_then(label_of).unwrap_or_default(),
        f.as_ref().and_then(|f| region_of(ui, f)).map_or("?", Region::name),
        ui.window.gets_focus_visible()
    );
}

/// A list's item: a selectable row or a card.
fn is_item(w: &gtk::Widget) -> bool {
    w.is::<gtk::FlowBoxChild>() || w.downcast_ref::<gtk::ListBoxRow>().is_some_and(|r| r.is_selectable())
}

/// A row that only holds things (a heading, a row of column heads, a row of cards): never a stop
/// itself; what it holds may be.
fn is_holder_row(w: &gtk::Widget) -> bool {
    w.downcast_ref::<gtk::ListBoxRow>().is_some_and(|r| !r.is_selectable())
}

/// The list item holding `w` (or `w` itself), if any.
fn item_of(w: &gtk::Widget) -> Option<gtk::Widget> {
    let mut p = Some(w.clone());
    while let Some(x) = p {
        if is_item(&x) {
            return Some(x);
        }
        p = x.parent();
    }
    None
}

/// The list's own controls (a search box, column heads, a group's buttons): not its items.
fn own_controls(list_area: &gtk::Widget) -> Vec<gtk::Widget> {
    controls(list_area).into_iter().filter(|c| item_of(c).is_none() && !is_holder_row(c)).collect()
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
    arrow_among(area, &controls(area), f, a);
}

/// Moves the focus among `cs` (controls inside `area`) by `a`, never out of them.
fn arrow_among(area: &gtk::Widget, cs: &[gtk::Widget], f: &gtk::Widget, a: Arrow) {
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
        // Tab and Shift+Tab: the next or previous area (F6 too). A dialog, the Find pop-up and an
        // open menu keep GTK's.
        K::Tab | K::KP_Tab | K::ISO_Left_Tab => {
            let st = ui.st.borrow();
            if st.modal_open || st.sw.open {
                return None;
            }
            drop(st);
            if focus(ui).is_some_and(|f| f.ancestor(gtk::Popover::static_type()).is_some()) {
                return None;
            }
            cycle(ui, !(shift || key == K::ISO_Left_Tab));
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
    // The list's own controls (column heads, a search box): ← → among them, ↓ into the list.
    if r == Region::List
        && item_of(&f).is_none()
        && let Some(a) = arrow_key(key)
    {
        match a {
            Arrow::Down => app::focus_list(ui),
            _ => {
                if let Some(w) = widget(ui, r) {
                    arrow_among(&w, &own_controls(&w), &f, a);
                }
            }
        }
        return Some(Stop);
    }
    // In the list, on an item or one of its own buttons.
    if r == Region::List
        && let Some(item) = item_of(&f)
        && let Some(w) = widget(ui, r)
    {
        let inner: Vec<gtk::Widget> = controls(&item).into_iter().filter(|c| c != &item).collect();
        let v = ui.st.borrow().view;
        let rows_across = !matches!(v, app::View::Projects | app::View::Ideas | app::View::Agents | app::View::Settings);
        match key {
            // On one of the item's buttons: ← → among the item and its buttons.
            K::Left | K::Right if f != item => {
                let mut all = vec![item.clone()];
                all.extend(inner);
                arrow_among(&w, &all, &f, if key == K::Left { Arrow::Left } else { Arrow::Right });
                return Some(Stop);
            }
            // On a list row (not cards, a tree or settings, whose ← → are their own): → onto
            // its first button.
            K::Right if rows_across => {
                if let Some(b) = inner.first() {
                    b.grab_focus();
                }
                return Some(Stop);
            }
            // ↑ from the first item: up to the nearest of the list's own controls above it (its
            // group's heading, the column heads, a search box), if there is one.
            K::Up if f == item => {
                let before = ui.st.borrow().sel.clone();
                app::move_sel(ui, -1);
                if ui.st.borrow().sel == before {
                    let mut all = vec![item.clone()];
                    all.extend(own_controls(&w));
                    arrow_among(&w, &all, &f, Arrow::Up);
                }
                return Some(Stop);
            }
            _ => {}
        }
    }
    if r != Region::List
        && let Some(a) = arrow_key(key)
    {
        if let Some(w) = widget(ui, r) {
            arrow_within(&w, &f, a);
        }
        return Some(Stop);
    }
    if matches!(r, Region::Rail | Region::Filters | Region::Top) && key == K::Escape {
        app::set_rail_open(ui, false);
        app::focus_list(ui);
        return Some(Stop);
    }
    None
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
        ui.window.connect_focus_widget_notify(move |_| {
            if std::env::var_os("WAKE_KEY_DEBUG").is_some() {
                log_focus(&ui2, "focus change");
            }
            focus_moved(&ui2);
            mark(&ui2);
            crate::peek::update_chip(&ui2);
        });
    }
    {
        let ui2 = ui.clone();
        ui.window.connect_focus_visible_notify(move |_| mark(&ui2));
    }
    {
        // A click is the person's too (a press or a release), wherever it lands.
        let click = gtk::GestureClick::new();
        click.set_button(0);
        click.set_propagation_phase(gtk::PropagationPhase::Capture);
        click.connect_pressed(|_, _, _, _| note_input());
        let ui2 = ui.clone();
        click.connect_released(move |_, _, _, _| {
            note_input();
            // Once the click has done its work (a row picked, Find opened or closed).
            let ui3 = ui2.clone();
            glib::idle_add_local_once(move || crate::peek::update_chip(&ui3));
        });
        ui.window.add_controller(click);
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
    ClickDetails,
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
        if tok == "click-details" {
            out.push(Step::ClickDetails);
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
            match step {
                Step::Type(t) => {
                    note_input();
                    type_text(&ui, &t);
                }
                Step::Wait => {}
                // As when new data arrives: the details and the list are drawn again.
                // A click in the middle of the details (what the click handler does after it).
                Step::ClickDetails => {
                    note_input();
                    let d = &ui.w.drawer;
                    crate::panel::focus_near(&ui, f64::from(d.width()) / 2.0, f64::from(d.height()) / 2.0);
                }
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
                    "keys: after step {i}: focus {:?} in {:?} label {:?} drawer {} visible {}",
                    f.as_ref().map(|f| (f.type_().name(), f.widget_name().to_string(), f.state_flags(), f.css_classes())),
                    f.as_ref().and_then(|f| region_of(&ui, f)).map(Region::name),
                    f.as_ref().and_then(label_of),
                    ui.st.borrow().drawer.is_some(),
                    ui.window.gets_focus_visible()
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
