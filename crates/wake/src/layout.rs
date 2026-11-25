//! How the window fits whatever width it is given (added 2026-11-20).
//!
//! Slipstream tiles windows, so Wake gets the width the layout hands it, often 900 to 1100 px and
//! sometimes much less. Nothing may be cut off at any width, and the window's minimum width is what
//! its narrowest layout truly needs, so the compositor never gives it less. The layout adapts the
//! way phone and tablet apps do rather than squeezing:
//!
//! - **Sidebar → slim rail → drawer** (libadwaita's OverlaySplitView, Material's navigation rail):
//!   wide, the rail has names; medium, initials and dots; narrow, it hides behind ☰ in the top bar
//!   and opens over everything as a drawer (Esc or a click outside closes it).
//! - **Split view → navigation stack** (iPad split view, libadwaita's NavigationSplitView): while
//!   the list keeps a readable width beside it, the details panel shares the row; below that,
//!   opening a row pushes the details over the whole content area with "‹ Back" at its top, and
//!   closing pops back to the list with the same row selected and the scroll kept.
//! - **Content-first rows**: a narrow list shows each row as a two-line cell instead of columns cut
//!   to a few letters (views.rs), with sorting in a "Sort ▾" menu.
//! - **Priority+** ([`Squeeze`]): toolbars show what fits and put the rest under "More ▾" (the
//!   filter chips) or drop the least important (the top bar's counts, the status bar's keys).
//!
//! [`Shell`] decides all this from its own width, with a little hysteresis so a width on a line
//! can't flicker, in the layout pass itself: the panel and drawer never jump. The rail's, rows' and
//! bars' forms are redrawn just after (they are different widgets, not just a size).
//! The slim rail slides out ([`Slide`]): hovering it (or the keyboard entering it) lays the full
//! rail over the content from the left edge, without moving anything under it.
//! [`clips`] walks the window after layout and lists anything drawn past its parent or the window's
//! right edge: the `--show clips` test hook prints it per view and width.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gtk4::{self as gtk, glib, graphene, prelude::*, subclass::prelude::*};

/// The rail's width, full and slim.
pub const RAIL_W: i32 = 216;
pub const SLIM_W: i32 = 64;
/// The body's minimum width: the narrowest layout (no rail, a list of two-line rows, the details
/// pushed over it). `--show clips` checks that it really fits.
pub const BODY_MIN: i32 = 460;
/// Below this body width the rail goes slim, and below the next it hides behind ☰.
const SLIM_BELOW: i32 = 1180;
const HIDE_BELOW: i32 = 760;
/// Below this list width (no panel) rows become two-line cells.
const NARROW_BELOW: i32 = 720;
/// Below this list width the Agents table drops its Where and Why stale columns (all of them at
/// their natural widths need about 1130 px).
const TIGHT_BELOW: i32 = 1140;
/// The details share the row only while the list keeps this much beside them; else they push.
#[cfg(test)]
const LIST_MIN: i32 = 620;
/// Below this the details are a page of their own; below `OVERLAY_BELOW` they slide over the list.
const PUSH_BELOW: i32 = 760;
const OVERLAY_BELOW: i32 = 1180;
/// Each breakpoint holds this far past its line once crossed, so resizing across it can't flicker.
const HYST: i32 = 12;
/// The soft edge drawn left of the details while they slide over the list.
const EDGE_W: i32 = 22;
/// The soft shadow right of the slim rail's full form, slid out over the content.
const PEEK_EDGE_W: i32 = 18;

/// The rail's form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Rail {
    #[default]
    Full,
    Slim,
    /// Behind ☰: a drawer over everything when open.
    Hidden,
}

/// The window's layout at its current width.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Lay {
    pub rail: Rail,
    /// The list is narrow even without the panel: rows are two-line cells, the top bar and the
    /// capture bar their short forms.
    pub narrow: bool,
    /// The details, when open, are a page of their own (a phone-narrow window), with Back.
    pub push: bool,
    /// The details, when open, slide in over the list from the right (a medium window); the list
    /// keeps its full width underneath.
    pub overlay: bool,
    /// Beside the details (when they share the row), the list is narrow: two-line rows there too.
    pub narrow_beside: bool,
    /// The list is under the width the Agents table needs for all its columns.
    pub tight: bool,
}

impl Lay {
    /// The rail is drawn with initials and dots.
    pub fn slim(self) -> bool {
        self.rail == Rail::Slim
    }
}

/// Is `v` below the line `t`, keeping the current answer `cur` within `HYST` of it.
fn below(cur: Option<bool>, v: i32, t: i32) -> bool {
    match cur {
        None => v < t,
        Some(true) => v < t + HYST,
        Some(false) => v < t - HYST,
    }
}

/// Whether the rail takes a place in the row. It doesn't: the views are tabs in the top bar, and
/// the rail is only the drawer behind ☰ in a phone-narrow window.
const RAIL_IN_ROW: bool = false;

pub fn rail_w(rail: Rail) -> i32 {
    if !RAIL_IN_ROW {
        return 0;
    }
    match rail {
        Rail::Full => RAIL_W,
        Rail::Slim => SLIM_W,
        Rail::Hidden => 0,
    }
}

/// The details' width beside the list, from the width left of the rail.
pub fn side_w(avail: i32) -> i32 {
    (avail * 36 / 100).clamp(340, 440)
}

/// The layout for a body `width` wide, given the one before (for the hysteresis). Pure, for tests.
pub fn decide(prev: Option<Lay>, width: i32) -> Lay {
    let hidden = below(prev.map(|p| p.rail == Rail::Hidden), width, HIDE_BELOW);
    let slim = below(prev.map(|p| p.rail != Rail::Full), width, SLIM_BELOW);
    let rail = if hidden {
        Rail::Hidden
    } else if slim {
        Rail::Slim
    } else {
        Rail::Full
    };
    let avail = width - rail_w(rail);
    let narrow = below(prev.map(|p| p.narrow), avail, NARROW_BELOW);
    // As GNOME, Material and Fluent do it: beside the list when there is room, over it when there
    // isn't, a page of its own when the window is phone-narrow.
    let push = below(prev.map(|p| p.push), avail, PUSH_BELOW);
    let overlay = !push && below(prev.map(|p| p.overlay), avail, OVERLAY_BELOW);
    let narrow_beside = if push || overlay { narrow } else { below(prev.map(|p| p.narrow_beside), avail - side_w(avail), NARROW_BELOW) };
    let tight = below(prev.map(|p| p.tight), avail, TIGHT_BELOW);
    Lay { rail, narrow, push, overlay, narrow_beside, tight }
}

// ---------- the slim rail sliding out ----------

/// Hovering the slim rail this long opens the full rail over it (a pointer just crossing it doesn't).
pub const OPEN_AFTER: u32 = 150;
/// The pointer gone from both the slim rail and the full one this long closes it.
pub const CLOSE_AFTER: u32 = 300;

/// What the slide-out should do after an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlideDo {
    Nothing,
    Open,
    Close,
    /// Call [`Slide::fire`] with the token after this many milliseconds.
    Wait(u32, u32),
}

/// When the slim rail's full form lies over the content: after a short hover on the strip, at once
/// when the keyboard goes in, until the pointer has been gone from both a moment and the keyboard
/// has left. Pure (timers are tokens to come back with), for tests.
#[derive(Debug, Default, Clone)]
pub struct Slide {
    pub open: bool,
    /// The pointer is over the strip, and over the full rail.
    strip: bool,
    over: bool,
    /// The keyboard is in the full rail.
    focus: bool,
    /// Just closed by a pick (a click, Enter, Esc) with the pointer still on the strip: it stays
    /// closed until the pointer leaves the strip.
    held: bool,
    token: u32,
}

impl Slide {
    fn hovered(&self) -> bool {
        (self.strip && !self.held) || self.over
    }

    /// Waits towards what the state now wants (a newer event drops an older wait).
    fn settle(&mut self) -> SlideDo {
        self.token += 1;
        let want = self.focus || self.hovered();
        if want == self.open { SlideDo::Nothing } else { SlideDo::Wait(if want { OPEN_AFTER } else { CLOSE_AFTER }, self.token) }
    }

    /// The pointer went onto (or off) the strip.
    pub fn strip(&mut self, inside: bool) -> SlideDo {
        self.strip = inside;
        if !inside {
            self.held = false;
        }
        self.settle()
    }

    /// The pointer went onto (or off) the full rail.
    pub fn overlay(&mut self, inside: bool) -> SlideDo {
        self.over = inside;
        self.settle()
    }

    /// The keyboard went into the full rail (it opens at once) or left it (it closes at once
    /// unless the pointer is on it).
    pub fn focus(&mut self, inside: bool) -> SlideDo {
        if self.focus == inside {
            return SlideDo::Nothing;
        }
        self.focus = inside;
        self.token += 1;
        match (inside, self.open) {
            (true, false) => {
                self.open = true;
                SlideDo::Open
            }
            (false, true) if !self.hovered() => {
                self.open = false;
                SlideDo::Close
            }
            _ => SlideDo::Nothing,
        }
    }

    /// An item was picked, or Esc: closed now, and held closed while the pointer stays on the strip.
    pub fn dismiss(&mut self, pointer_on_strip: bool) -> SlideDo {
        let was = self.open;
        *self = Slide { strip: pointer_on_strip, held: pointer_on_strip, token: self.token + 1, ..Slide::default() };
        if was { SlideDo::Close } else { SlideDo::Nothing }
    }

    /// A wait ended: opens or closes if that is still what the state wants.
    pub fn fire(&mut self, token: u32) -> SlideDo {
        if token != self.token {
            return SlideDo::Nothing;
        }
        let want = self.focus || self.hovered();
        if want == self.open {
            return SlideDo::Nothing;
        }
        self.open = want;
        if want { SlideDo::Open } else { SlideDo::Close }
    }
}

// ---------- the shell: rail, main, details ----------

glib::wrapper! {
    pub struct Shell(ObjectSubclass<shell::Imp>) @extends gtk::Widget, @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Shell {
    /// `rail`, `main` and the details' revealer, in the row they make; `peek`, the full rail that
    /// slides out over the content while the rail is slim.
    pub fn new(rail: &impl IsA<gtk::Widget>, peek: &impl IsA<gtk::Widget>, main: &impl IsA<gtk::Widget>, panel: &impl IsA<gtk::Widget>) -> Shell {
        let s: Shell = glib::Object::new();
        s.add_css_class("body");
        let edge = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        edge.add_css_class("paneledge");
        edge.set_can_target(false);
        // Behind the rail's drawer: dims the rest; a click on it closes the drawer.
        let scrim = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        scrim.add_css_class("railscrim");
        let click = gtk::GestureClick::new();
        let weak = s.downgrade();
        click.connect_released(move |_, _, _, _| {
            if let Some(s) = weak.upgrade() {
                s.set_rail_open(false);
            }
        });
        scrim.add_controller(click);
        // Drawn in this order: the rail last, so as a drawer it lies over everything (and the full
        // rail over the slim one).
        main.set_parent(&s);
        edge.set_parent(&s);
        panel.set_parent(&s);
        scrim.set_parent(&s);
        rail.set_parent(&s);
        // The full rail's soft shadow on the content, right of it.
        let peek_edge = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        peek_edge.add_css_class("peekedge");
        peek_edge.set_can_target(false);
        peek_edge.set_parent(&s);
        peek_edge.set_child_visible(false);
        peek.set_parent(&s);
        peek.set_child_visible(false);
        peek.set_can_target(false);
        // The pointer on the strip, or on the full rail (which counts as the strip).
        for (w, strip) in [(rail.clone().upcast::<gtk::Widget>(), true), (peek.clone().upcast(), false)] {
            let m = gtk::EventControllerMotion::new();
            let weak = s.downgrade();
            m.connect_enter(move |_, x, _| {
                if let Some(s) = weak.upgrade() {
                    s.imp().pointer_x.set(x);
                    s.slide(|sl| if strip { sl.strip(true) } else { sl.overlay(true) });
                }
            });
            let weak = s.downgrade();
            m.connect_motion(move |_, x, _| {
                if let Some(s) = weak.upgrade() {
                    s.imp().pointer_x.set(x);
                }
            });
            let weak = s.downgrade();
            m.connect_leave(move |_| {
                if let Some(s) = weak.upgrade() {
                    s.slide(|sl| if strip { sl.strip(false) } else { sl.overlay(false) });
                }
            });
            w.add_controller(m);
        }
        *s.imp().parts.borrow_mut() = Some(shell::Parts {
            rail: rail.clone().upcast(),
            peek: peek.clone().upcast(),
            peek_edge: peek_edge.upcast(),
            main: main.clone().upcast(),
            edge: edge.upcast(),
            panel: panel.clone().upcast(),
            scrim: scrim.upcast(),
        });
        s.set_overflow(gtk::Overflow::Hidden);
        s
    }

    /// The layout as of the last allocation (None before the first).
    pub fn lay(&self) -> Option<Lay> {
        self.imp().lay.get()
    }

    /// Called (once per change, just after the layout pass) when the layout changes.
    pub fn connect_changed(&self, f: impl Fn(Lay) + 'static) {
        *self.imp().on_change.borrow_mut() = Some(Rc::new(f));
    }

    /// Called when the rail's drawer opens or closes.
    pub fn connect_rail(&self, f: impl Fn(bool) + 'static) {
        *self.imp().on_rail.borrow_mut() = Some(Rc::new(f));
    }

    /// Opens or closes the rail's drawer (only shown while the rail is hidden).
    pub fn set_rail_open(&self, open: bool) {
        if self.imp().rail_open.replace(open) != open {
            self.queue_allocate();
            let f = self.imp().on_rail.borrow().clone();
            if let Some(f) = f {
                f(open);
            }
        }
    }

    pub fn rail_open(&self) -> bool {
        self.imp().rail_open.get()
    }

    /// The details cover the whole content area (pushed, and all the way in).
    pub fn covered(&self) -> bool {
        self.imp().covered.get()
    }

    /// The slim rail's full form is out over the content (or on its way out).
    pub fn peek_open(&self) -> bool {
        self.imp().peek_on.get()
    }

    /// Called when the full rail slides out or back.
    pub fn connect_peek(&self, f: impl Fn(bool) + 'static) {
        *self.imp().on_peek.borrow_mut() = Some(Rc::new(f));
    }

    /// Feeds the slide-out an event and does what it says. Only while the rail is slim.
    pub fn slide(&self, f: impl FnOnce(&mut Slide) -> SlideDo) {
        if self.lay().map(|l| l.rail) != Some(Rail::Slim) {
            return;
        }
        let d = f(&mut self.imp().slide.borrow_mut());
        self.slide_do(d);
    }

    fn slide_do(&self, d: SlideDo) {
        match d {
            SlideDo::Nothing => {}
            SlideDo::Open => self.set_peek(true),
            SlideDo::Close => self.set_peek(false),
            SlideDo::Wait(ms, token) => {
                let weak = self.downgrade();
                glib::timeout_add_local_once(Duration::from_millis(u64::from(ms)), move || {
                    if let Some(s) = weak.upgrade()
                        && s.lay().map(|l| l.rail) == Some(Rail::Slim)
                    {
                        let d = s.imp().slide.borrow_mut().fire(token);
                        s.slide_do(d);
                    }
                });
            }
        }
    }

    /// An item was picked or Esc pressed: the full rail goes back now.
    pub fn dismiss_peek(&self) {
        let imp = self.imp();
        let on_strip = { imp.slide.borrow().over } && imp.pointer_x.get() < f64::from(SLIM_W);
        let on_strip = on_strip || imp.slide.borrow().strip;
        self.slide(|sl| sl.dismiss(on_strip));
    }

    /// The rail isn't slim any more: no slide-out, at once.
    pub fn end_peek(&self) {
        let imp = self.imp();
        *imp.slide.borrow_mut() = Slide::default();
        if let Some(a) = imp.peek_anim.take() {
            a.set(true);
        }
        imp.peek_t.set(0.0);
        if imp.peek_on.replace(false) {
            self.peek_changed(false);
        }
        self.queue_allocate();
    }

    fn peek_changed(&self, open: bool) {
        if let Some(p) = self.imp().parts.borrow().as_ref() {
            p.peek.set_can_target(open);
            if open {
                p.peek.set_child_visible(true);
            }
        }
        let f = self.imp().on_peek.borrow().clone();
        if let Some(f) = f {
            f(open);
        }
    }

    /// Slides the full rail out (fading in) or back, from wherever it is now.
    fn set_peek(&self, open: bool) {
        let imp = self.imp();
        if imp.peek_on.replace(open) == open {
            return;
        }
        self.peek_changed(open);
        self.queue_allocate();
        if let Some(a) = imp.peek_anim.take() {
            a.set(true);
        }
        let from = imp.peek_t.get();
        let to = if open { 1.0 } else { 0.0 };
        let weak = self.downgrade();
        let stop = crate::motion::animate(self, crate::motion::DUR, move |p| {
            let Some(s) = weak.upgrade() else { return };
            s.imp().peek_t.set(from + (to - from) * p);
            s.queue_draw();
            if p >= 1.0 && to == 0.0 {
                // All the way back: out of sight.
                s.queue_allocate();
            }
        });
        imp.peek_anim.replace(Some(stop));
    }
}

mod shell {
    use super::*;

    pub type OnLay = Rc<dyn Fn(Lay)>;
    pub type OnRail = Rc<dyn Fn(bool)>;

    pub struct Parts {
        pub rail: gtk::Widget,
        pub peek: gtk::Widget,
        pub peek_edge: gtk::Widget,
        pub main: gtk::Widget,
        pub edge: gtk::Widget,
        pub panel: gtk::Widget,
        pub scrim: gtk::Widget,
    }

    #[derive(Default)]
    pub struct Imp {
        pub parts: RefCell<Option<Parts>>,
        pub lay: Cell<Option<Lay>>,
        pub pending: Cell<bool>,
        pub on_change: RefCell<Option<OnLay>>,
        pub on_rail: RefCell<Option<OnRail>>,
        pub rail_open: Cell<bool>,
        pub covered: Cell<bool>,
        /// The slide-out: its state, whether it is out, how far (0 to 1, as drawn), its animation,
        /// and the pointer's last x over the rails.
        pub slide: RefCell<Slide>,
        pub peek_on: Cell<bool>,
        pub peek_t: Cell<f64>,
        pub peek_anim: RefCell<Option<Rc<Cell<bool>>>>,
        pub pointer_x: Cell<f64>,
        pub on_peek: RefCell<Option<OnRail>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Imp {
        const NAME: &'static str = "WakeShell";
        type Type = Shell;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for Imp {
        fn dispose(&self) {
            self.parts.borrow_mut().take();
            while let Some(c) = self.obj().first_child() {
                c.unparent();
            }
        }
    }

    /// Moves the focus on through `order` (the children of `obj`, first to last; reversed for
    /// Shift+Tab): further inside the child that has it, else into the next that takes it.
    fn focus_in_order(obj: &gtk::Widget, mut order: Vec<gtk::Widget>, dir: gtk::DirectionType) -> bool {
        if dir == gtk::DirectionType::TabBackward {
            order.reverse();
        }
        let current = obj.focus_child();
        let from = match current.as_ref().and_then(|c| order.iter().position(|w| w == c)) {
            Some(i) => {
                if order[i].child_focus(dir) {
                    return true;
                }
                i + 1
            }
            None => 0,
        };
        order[from..].iter().any(|w| w.child_focus(dir))
    }

    fn show(w: &gtk::Widget, on: bool) {
        if w.is_child_visible() != on {
            w.set_child_visible(on);
        }
    }

    impl WidgetImpl for Imp {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::HeightForWidth
        }

        /// Tab and Shift+Tab in a fixed order, the order things read in: the rail (when it is
        /// out), the list, then the details. Not by where they happen to be drawn, which changes
        /// while the details slide. Details that take the list's place (a narrow window) leave
        /// the list out of reach, as it is out of sight.
        fn focus(&self, dir: gtk::DirectionType) -> bool {
            use gtk::DirectionType as D;
            if !matches!(dir, D::TabForward | D::TabBackward) {
                return self.parent_focus(dir);
            }
            let order = {
                let parts = self.parts.borrow();
                let Some(p) = parts.as_ref() else { return false };
                let details = p.panel.is_visible();
                let pushed = details && self.lay.get().is_some_and(|l| l.push);
                let mut order: Vec<gtk::Widget> = Vec::new();
                for w in [&p.peek, &p.rail] {
                    if w.is_visible() && w.is_child_visible() {
                        order.push(w.clone());
                    }
                }
                if !pushed && p.main.is_child_visible() {
                    order.push(p.main.clone());
                }
                if details {
                    order.push(p.panel.clone());
                }
                order
            };
            focus_in_order(self.obj().upcast_ref(), order, dir)
        }

        fn measure(&self, o: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let parts = self.parts.borrow();
            let Some(p) = parts.as_ref() else { return (0, 0, -1, -1) };
            if o == gtk::Orientation::Horizontal {
                // Whatever the current layout needs, the narrowest one fits in BODY_MIN.
                let nat = p.rail.measure(o, -1).1 + p.main.measure(o, -1).1 + if p.panel.is_visible() { p.panel.measure(o, -1).1 } else { 0 };
                return (BODY_MIN, nat.max(BODY_MIN), -1, -1);
            }
            let (mut min, mut nat) = (0, 0);
            for c in [&p.rail, &p.main, &p.panel] {
                if c.is_visible() {
                    let (a, b, _, _) = c.measure(o, -1);
                    min = min.max(a);
                    nat = nat.max(b);
                }
            }
            (min, nat, -1, -1)
        }

        fn size_allocate(&self, width: i32, height: i32, _baseline: i32) {
            let parts = self.parts.borrow();
            let Some(p) = parts.as_ref() else { return };
            let prev = self.lay.get();
            let lay = decide(prev, width);
            if prev != Some(lay) {
                self.lay.set(Some(lay));
                // Redrawing the rail, rows and bars in their new forms changes their sizes, which
                // can't be done in the middle of a layout pass: just after it.
                if !self.pending.get() {
                    self.pending.set(true);
                    let obj = self.obj().downgrade();
                    glib::idle_add_local_once(move || {
                        let Some(obj) = obj.upgrade() else { return };
                        obj.imp().pending.set(false);
                        let f = obj.imp().on_change.borrow().clone();
                        if let (Some(f), Some(l)) = (f, obj.imp().lay.get()) {
                            f(l);
                        }
                    });
                }
            }
            let at = |w: &gtk::Widget, x: i32, wd: i32| w.size_allocate(&gtk::Allocation::new(x, 0, wd.max(0), height), -1);
            let rail_min = p.rail.measure(gtk::Orientation::Horizontal, -1).0;
            // The rail: in the row, or a drawer over everything, or out of sight.
            let drawer = !RAIL_IN_ROW || lay.rail == Rail::Hidden;
            let rail_open = drawer && self.rail_open.get();
            show(&p.rail, !drawer || rail_open);
            show(&p.scrim, rail_open);
            let rail_w = if drawer { 0 } else { rail_min };
            let avail = width - rail_w;
            // The details' width, times how far they have slid in (their revealer reports that).
            let mut shown = 0;
            let mut frac = 0.0;
            if p.panel.is_visible() {
                let target = if lay.push { avail } else { side_w(avail) };
                let full = p.panel.first_child().map_or(0, |c| c.measure(gtk::Orientation::Horizontal, -1).1);
                let (pmin, now, _, _) = p.panel.measure(gtk::Orientation::Horizontal, -1);
                frac = if full > 0 { (f64::from(now) / f64::from(full)).clamp(0.0, 1.0) } else { 1.0 };
                shown = ((f64::from(target) * frac).round() as i32).max(pmin).min(avail);
            }
            // Pushed all the way in, the list is out of sight (and out of the keyboard's reach).
            let covered = lay.push && shown > 0 && frac >= 1.0;
            self.covered.set(covered);
            show(&p.main, !covered);
            if !covered {
                let main_w = if lay.push || lay.overlay { avail } else { avail - shown };
                let main_min = p.main.measure(gtk::Orientation::Horizontal, -1).0;
                at(&p.main, rail_w, main_w.max(main_min));
            }
            // A soft shadow on the list's side of the details when they lie over it.
            let edge_w = if (lay.push || lay.overlay) && shown > 0 && !covered { EDGE_W } else { 0 };
            at(&p.edge, width - shown - edge_w, edge_w);
            if p.panel.is_visible() {
                at(&p.panel, width - shown, shown);
            }
            if rail_open {
                at(&p.scrim, 0, width);
            }
            if !drawer || rail_open {
                at(&p.rail, 0, rail_min);
            }
            // The slim rail's full form, over the content from the left edge (nothing moves under it).
            let peek = RAIL_IN_ROW && lay.rail == Rail::Slim && (self.peek_on.get() || self.peek_t.get() > 0.0);
            show(&p.peek, peek);
            show(&p.peek_edge, peek);
            if peek {
                let pw = p.peek.measure(gtk::Orientation::Horizontal, -1).0.max(RAIL_W).min(width);
                at(&p.peek, 0, pw);
                at(&p.peek_edge, pw, PEEK_EDGE_W.min(width - pw));
            }
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let obj = self.obj();
            let (peek, edge, pw) = match self.parts.borrow().as_ref() {
                Some(p) => (Some(p.peek.clone()), Some(p.peek_edge.clone()), p.peek.width()),
                None => (None, None, 0),
            };
            let mut c = obj.first_child();
            while let Some(x) = c {
                c = x.next_sibling();
                let t = self.peek_t.get();
                if (Some(&x) == peek.as_ref() || Some(&x) == edge.as_ref()) && t < 1.0 {
                    // Sliding out from under the slim rail as it fades in, its shadow with it.
                    let shift = -(1.0 - t) * f64::from((pw - SLIM_W).max(0));
                    snapshot.save();
                    snapshot.translate(&graphene::Point::new(shift as f32, 0.0));
                    snapshot.push_opacity(t);
                    obj.snapshot_child(&x, snapshot);
                    snapshot.pop();
                    snapshot.restore();
                } else {
                    obj.snapshot_child(&x, snapshot);
                }
            }
        }
    }
}

// ---------- a row that drops what doesn't fit ----------

glib::wrapper! {
    pub struct Squeeze(ObjectSubclass<squeeze::Imp>) @extends gtk::Widget, @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Squeeze {
    /// A horizontal row `spacing` apart. Children added with priority 0 always show; the others
    /// show while the row has room for them at their natural width, the lowest numbers first.
    pub fn new(spacing: i32, classes: &[&str]) -> Squeeze {
        Self::with_role(spacing, classes, gtk::AccessibleRole::Generic)
    }

    pub fn with_role(spacing: i32, classes: &[&str], role: gtk::AccessibleRole) -> Squeeze {
        let s: Squeeze = glib::Object::builder().property("accessible-role", role).build();
        s.imp().spacing.set(spacing);
        for c in classes {
            s.add_css_class(c);
        }
        s
    }

    pub fn add(&self, child: &impl IsA<gtk::Widget>, prio: i32) {
        match self.imp().overflow.borrow().as_ref() {
            Some(o) => child.insert_before(self, Some(o)),
            None => child.set_parent(self),
        }
        self.imp().prio.borrow_mut().push((child.clone().upcast(), prio));
    }

    /// Adds `child` after the others: always shown when it is marked "on" (the chosen filter), else
    /// dropped before anything added earlier.
    pub fn add_next(&self, child: &impl IsA<gtk::Widget>) {
        let n = self.imp().next.get() + 1;
        self.imp().next.set(n);
        self.add(child, if child.has_css_class("on") || child.has_css_class("ttl") { 0 } else { n });
    }

    pub fn clear(&self) {
        self.imp().prio.borrow_mut().clear();
        self.imp().next.set(0);
        let keep = self.imp().overflow.borrow().clone();
        let mut c = self.first_child();
        while let Some(x) = c {
            c = x.next_sibling();
            if Some(&x) != keep.as_ref() {
                x.unparent();
            }
        }
    }

    /// Priority+: a "More ▾" button at the end, shown when something didn't fit; its menu holds
    /// the buttons that were dropped, each doing what it does in the row.
    pub fn with_more(&self) {
        let mb = gtk::MenuButton::new();
        mb.set_label("More");
        mb.add_css_class("more");
        mb.set_tooltip_text(Some("What doesn't fit in the row"));
        let weak = self.downgrade();
        mb.set_create_popup_func(move |mb| {
            let Some(s) = weak.upgrade() else { return };
            let pop = gtk::Popover::new();
            let menu = gtk::Box::new(gtk::Orientation::Vertical, 2);
            menu.add_css_class("moremenu");
            for b in s.dropped() {
                let Ok(btn) = b.downcast::<gtk::Button>() else { continue };
                let item = gtk::Button::with_label(&label_text(btn.upcast_ref()));
                item.add_css_class("menuitem");
                if btn.has_css_class("on") {
                    item.add_css_class("on");
                }
                let p = pop.clone();
                item.connect_clicked(move |_| {
                    p.popdown();
                    btn.emit_clicked();
                });
                menu.append(&item);
            }
            pop.set_child(Some(&menu));
            mb.set_popover(Some(&pop));
        });
        mb.set_parent(self);
        *self.imp().overflow.borrow_mut() = Some(mb.upcast());
    }

    /// The children that didn't fit, in order.
    pub fn dropped(&self) -> Vec<gtk::Widget> {
        let keep = self.imp().overflow.borrow().clone();
        let mut out = Vec::new();
        let mut c = self.first_child();
        while let Some(x) = c {
            if x.is_visible() && !x.is_child_visible() && Some(&x) != keep.as_ref() {
                out.push(x.clone());
            }
            c = x.next_sibling();
        }
        out
    }
}

/// A widget's label text, its labels joined (a chip's "Waiting for you 2").
fn label_text(w: &gtk::Widget) -> String {
    fn walk(w: &gtk::Widget, out: &mut Vec<String>) {
        if let Some(l) = w.downcast_ref::<gtk::Label>() {
            if !w.has_css_class("kbd") {
                out.push(l.text().to_string());
            }
            return;
        }
        let mut c = w.first_child();
        while let Some(x) = c {
            if x.is_visible() {
                walk(&x, out);
            }
            c = x.next_sibling();
        }
    }
    let mut out = Vec::new();
    walk(w, &mut out);
    out.join(" ")
}

mod squeeze {
    use super::*;

    #[derive(Default)]
    pub struct Imp {
        pub spacing: Cell<i32>,
        pub prio: RefCell<Vec<(gtk::Widget, i32)>>,
        pub next: Cell<i32>,
        /// "More ▾", when the row has one.
        pub overflow: RefCell<Option<gtk::Widget>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Imp {
        const NAME: &'static str = "WakeSqueeze";
        type Type = Squeeze;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for Imp {
        fn dispose(&self) {
            self.prio.borrow_mut().clear();
            while let Some(c) = self.obj().first_child() {
                c.unparent();
            }
        }
    }

    impl Imp {
        /// The visible children with their priority and horizontal sizes, in order.
        fn kids(&self) -> Vec<(gtk::Widget, i32, i32, i32)> {
            let prio = self.prio.borrow();
            let more = self.overflow.borrow().clone();
            let mut out = Vec::new();
            let mut c = self.obj().first_child();
            while let Some(w) = c {
                if w.is_visible() && Some(&w) != more.as_ref() {
                    let p = prio.iter().find(|(x, _)| x == &w).map_or(0, |x| x.1);
                    let (min, nat, _, _) = w.measure(gtk::Orientation::Horizontal, -1);
                    out.push((w.clone(), p, min, nat));
                }
                c = w.next_sibling();
            }
            out
        }

        /// Which children fit in `width` (and whether "More ▾" shows): all the priority-0 ones, then
        /// the others in priority order while they fit at their natural width. With "More ▾", room
        /// is kept for it once anything has to go.
        fn pick(&self, width: i32) -> (Vec<bool>, bool) {
            let kids = self.kids();
            let all = self.pick_in(&kids, width);
            let more = self.overflow.borrow().clone();
            match more {
                // Only buttons go in the menu: a dropped hint or note needs no "More".
                Some(m) if kids.iter().zip(&all).any(|(k, on)| !on && k.0.is::<gtk::Button>()) => {
                    let mw = m.measure(gtk::Orientation::Horizontal, -1).1 + self.spacing.get();
                    (self.pick_in(&kids, width - mw), true)
                }
                _ => (all, false),
            }
        }

        fn pick_in(&self, kids: &[(gtk::Widget, i32, i32, i32)], width: i32) -> Vec<bool> {
            let sp = self.spacing.get();
            let mut on: Vec<bool> = kids.iter().map(|k| k.1 == 0).collect();
            let mut used: i32 = kids.iter().filter(|k| k.1 == 0).map(|k| k.2).sum::<i32>() + sp * (on.iter().filter(|x| **x).count() as i32 - 1).max(0);
            let mut order: Vec<usize> = (0..kids.len()).filter(|i| kids[*i].1 != 0).collect();
            order.sort_by_key(|i| (kids[*i].1, *i));
            for i in order {
                // A gap before it only when something is already shown.
                let need = kids[i].3 + if on.iter().any(|x| *x) { sp } else { 0 };
                if used + need <= width {
                    used += need;
                    on[i] = true;
                } else {
                    // Lower priorities stay out too, so the row never shows a later key without an earlier one.
                    break;
                }
            }
            on
        }
    }

    impl WidgetImpl for Imp {
        fn measure(&self, o: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let kids = self.kids();
            let more = self.overflow.borrow().clone().map_or(0, |m| m.measure(o, -1).0 + self.spacing.get());
            if o == gtk::Orientation::Horizontal {
                let sp = self.spacing.get();
                let req: Vec<_> = kids.iter().filter(|k| k.1 == 0).collect();
                let min = req.iter().map(|k| k.2).sum::<i32>() + sp * (req.len() as i32 - 1).max(0) + more;
                let nat = kids.iter().map(|k| k.3).sum::<i32>() + sp * (kids.len() as i32 - 1).max(0);
                return (min, nat.max(min), -1, -1);
            }
            let (mut min, mut nat) = (0, 0);
            let ws = kids.iter().map(|k| k.0.clone()).chain(self.overflow.borrow().clone());
            for w in ws {
                let (a, b, _, _) = w.measure(o, -1);
                min = min.max(a);
                nat = nat.max(b);
            }
            (min, nat, -1, -1)
        }

        fn size_allocate(&self, width: i32, height: i32, _baseline: i32) {
            let kids = self.kids();
            let (on, more) = self.pick(width);
            let over = self.overflow.borrow().clone();
            let more_w = match &over {
                Some(m) => {
                    if m.is_child_visible() != more {
                        m.set_child_visible(more);
                    }
                    if more { m.measure(gtk::Orientation::Horizontal, -1).1 + self.spacing.get() } else { 0 }
                }
                None => 0,
            };
            let width = width - more_w;
            for (k, show) in kids.iter().zip(&on) {
                if k.0.is_child_visible() != *show {
                    k.0.set_child_visible(*show);
                }
            }
            let shown: Vec<_> = kids.iter().zip(&on).filter(|(_, s)| **s).map(|(k, _)| k).collect();
            let sp = self.spacing.get();
            let mins: i32 = shown.iter().map(|k| k.2).sum::<i32>() + sp * (shown.len() as i32 - 1).max(0);
            let gap: i32 = shown.iter().map(|k| k.3 - k.2).sum();
            let mut extra = (width - mins).max(0);
            // Up to natural widths first, in proportion; what is left goes to the expanding ones.
            let grow = extra.min(gap);
            extra -= grow;
            let expanders = shown.iter().filter(|k| k.0.hexpands()).count() as i32;
            let mut x = 0;
            let mut given = 0;
            let mut ex_given = 0;
            let mut ex_seen = 0;
            for (i, k) in shown.iter().enumerate() {
                let mut w = k.2;
                if gap > 0 {
                    let share = if i + 1 == shown.len() { grow - given } else { (i64::from(grow) * i64::from(k.3 - k.2) / i64::from(gap)) as i32 };
                    w += share;
                    given += share;
                }
                if expanders > 0 && k.0.hexpands() {
                    ex_seen += 1;
                    let share = if ex_seen == expanders { extra - ex_given } else { extra / expanders };
                    w += share;
                    ex_given += share;
                }
                k.0.size_allocate(&gtk::Allocation::new(x, 0, w, height), -1);
                x += w + sp;
            }
            if let Some(m) = over.filter(|_| more) {
                let mw = more_w - sp;
                let mh = m.measure(gtk::Orientation::Vertical, mw).1.min(height);
                m.size_allocate(&gtk::Allocation::new(width + sp, (height - mh) / 2, mw, mh), -1);
            }
        }
    }
}

// ---------- the clip check ----------

/// A widget and its labels' text, for the debugging output.
pub fn describe(w: &gtk::Widget) -> String {
    fn texts(w: &gtk::Widget, out: &mut Vec<String>) {
        if let Some(l) = w.downcast_ref::<gtk::Label>() {
            out.push(format!("{:?}({})", l.text().chars().take(24).collect::<String>(), l.measure(gtk::Orientation::Horizontal, -1).0));
        }
        let mut c = w.first_child();
        while let Some(x) = c {
            if x.is_visible() {
                texts(&x, out);
            }
            c = x.next_sibling();
        }
    }
    let mut t = Vec::new();
    texts(w, &mut t);
    format!("{}.{} {}", w.type_().name(), w.css_classes().iter().map(|c| c.to_string()).collect::<Vec<_>>().join("."), t.join(" "))
}

/// Everything drawn past its parent's right edge or past the window's: (what, how far over).
/// Children of a sideways-scrolling area are allowed past it (that is what it scrolls), and a
/// widget already reported hides its own children from the list.
pub fn clips(root: &gtk::Widget) -> Vec<(String, f32)> {
    fn name(w: &gtk::Widget) -> String {
        let cls = w.css_classes().iter().map(|c| c.to_string()).collect::<Vec<_>>().join(".");
        let text = w.downcast_ref::<gtk::Label>().map(|l| format!(" \"{}\"", l.text().chars().take(30).collect::<String>())).unwrap_or_default();
        format!("{}{}{}{}", w.type_().name(), if cls.is_empty() { "" } else { "." }, cls, text)
    }
    fn scrolls_sideways(w: &gtk::Widget) -> bool {
        w.downcast_ref::<gtk::ScrolledWindow>().is_some_and(|s| s.hscrollbar_policy() != gtk::PolicyType::Never)
    }
    fn walk(w: &gtk::Widget, root: &gtk::Widget, rw: f32, path: &str, sideways: bool, out: &mut Vec<(String, f32)>) {
        let mut c = w.first_child();
        while let Some(x) = c {
            c = x.next_sibling();
            if !x.is_visible() || !x.is_child_visible() || !x.is_mapped() || x.is::<gtk::Popover>() {
                continue;
            }
            let here = format!("{path} > {}", name(&x));
            let side = sideways || scrolls_sideways(w);
            // Both edges in the parent's own coordinates (its border box, padding included). A
            // parent not laid out yet (just rebuilt, before the next frame) has nothing to say.
            let pb = w.compute_bounds(w);
            let laid_out = pb.as_ref().is_some_and(|b| b.width() > 0.0 && b.width() as i32 + 1 >= w.measure(gtk::Orientation::Horizontal, -1).0);
            if !side && laid_out {
                let pr = pb.map_or(0.0, |b| b.x() + b.width());
                let over_parent = x.compute_bounds(w).map_or(0.0, |b| b.x() + b.width() - pr);
                let over_window = x.compute_bounds(root).map_or(0.0, |b| b.x() + b.width() - rw);
                let over = over_parent.max(over_window);
                if over > 1.0 {
                    let here = if std::env::var_os("WAKE_CLIPS_DEBUG").is_some() {
                        let b = x.compute_bounds(w);
                        format!("{here} [in parent {:?} of {:?}; window {rw}]", b.map(|b| (b.x(), b.width())), pb.map(|b| (b.x(), b.width())))
                    } else {
                        here
                    };
                    out.push((here, over));
                    continue;
                }
            }
            walk(&x, root, rw, &name(&x), side, out);
        }
    }
    let mut out = Vec::new();
    let rw = root.compute_bounds(root).map_or(root.width() as f32, |b| b.x() + b.width());
    walk(root, root, rw, &name(root), false, &mut out);
    out
}

/// Test hook `--show clips`: every view at this width with the side panel closed and open, the
/// switcher, a dialog and the Keys sheet, printing what is cut off (or out of sight) (`ok` when nothing is), the window's content
/// minimum and the layout; then quits. `wake-clips.sh` runs it across widths and asserts.
pub fn hook(ui: &crate::app::Ui) {
    use crate::app::{self, View};
    use std::time::Duration;
    let mut steps: Vec<(View, &'static str)> = Vec::new();
    for v in View::ALL {
        steps.push((v, "closed"));
        if app::opens_panel(v) {
            steps.push((v, "panel"));
        }
    }
    steps.push((View::Monitor, "switcher"));
    steps.push((View::Monitor, "dialog"));
    steps.push((View::Monitor, "keys"));
    let ui = ui.clone();
    let step = Rc::new(Cell::new(0usize));
    let failed = Rc::new(Cell::new(0usize));
    let waited = Rc::new(Cell::new(0u32));
    // Each step: set it up, give it a moment to lay out (and the rail and tables to redraw), check.
    glib::timeout_add_local(Duration::from_millis(900), move || {
        let i = step.get();
        let report = |ui: &crate::app::Ui, (v, what): (View, &str)| {
            let root = ui.window.child().expect("window content");
            let (min, ..) = root.measure(gtk::Orientation::Horizontal, -1);
            let mut found = clips(&root);
            // A dialog must be in sight: a card laid out below the window draws nothing at all.
            if matches!(what, "dialog" | "keys") {
                let card = find_class(ui.w.modal.upcast_ref(), "modal");
                let b = card.as_ref().and_then(|c| c.compute_bounds(&root));
                if b.is_none_or(|b| b.y() < 0.0 || b.y() > root.height() as f32 - 40.0) {
                    found.push((format!("the {what} card is out of sight ({b:?})"), 0.0));
                }
            }
            let lay = ui.w.body.lay().unwrap_or_default();
            let form = format!("{:?}{}{}", lay.rail, if lay.narrow { ",narrow" } else { "" }, if lay.push { ",push" } else { "" }).to_lowercase();
            let fits = min <= root.width();
            let main_min = ui.w.body.first_child().map_or(0, |m| m.measure(gtk::Orientation::Horizontal, -1).0);
            let form = format!("{form}; main min {main_min}");
            if found.is_empty() && fits {
                println!("clips width={} view={} {what}: ok (content min {min}, {form})", root.width(), v.id());
            } else {
                failed.set(failed.get() + 1);
                println!("clips width={} view={} {what}: CLIPPED {} (content min {min}, {form})", root.width(), v.id(), found.len());
                for (w, by) in found.iter().take(6) {
                    println!("    {by:.0} px over: {w}");
                }
            }
        };
        // Check the step before, then undo it.
        if i > 0 {
            let prev = steps[i - 1];
            // A dialog just put up may not be laid out yet (Broadway's frame clock is lazy): give it
            // a few more ticks before judging where it is.
            if matches!(prev.1, "dialog" | "keys") && find_class(ui.w.modal.upcast_ref(), "modal").is_some_and(|c| c.width() < 100) && waited.get() < 5 {
                waited.set(waited.get() + 1);
                return glib::ControlFlow::Continue;
            }
            waited.set(0);
            report(&ui, prev);
            match prev.1 {
                "panel" => crate::panel::close(&ui),
                "switcher" => crate::switcher::close(&ui),
                "dialog" | "keys" => crate::dialogs::close(&ui),
                _ => {}
            }
        }
        let Some(&(v, what)) = steps.get(i) else {
            println!("clips done: {} of {} cut off", failed.get(), steps.len());
            if let Some(a) = ui.window.application() {
                a.quit();
            }
            return glib::ControlFlow::Break;
        };
        step.set(i + 1);
        app::set_view(&ui, v);
        match what {
            "panel" => {
                app::move_sel(&ui, 1);
                app::open_sel(&ui);
            }
            "switcher" => crate::switcher::open(&ui),
            "keys" => crate::dialogs::keys(&ui),
            "dialog" => {
                let id = ui.st.borrow().ov.as_ref().and_then(|ov| ov.projects.first().map(|p| p.id.clone()));
                if let Some(id) = id {
                    crate::dialogs::pause(&ui, &id);
                }
            }
            _ => {}
        }
        glib::ControlFlow::Continue
    });
}

/// The first widget under `w` (or `w`) with CSS class `class`.
fn find_class(w: &gtk::Widget, class: &str) -> Option<gtk::Widget> {
    if w.has_css_class(class) {
        return Some(w.clone());
    }
    let mut c = w.first_child();
    while let Some(x) = c {
        if let Some(f) = find_class(&x, class) {
            return Some(f);
        }
        c = x.next_sibling();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_layout_steps_down_as_the_window_narrows() {
        // Wide: names in the rail, columns, details beside the list.
        assert_eq!(decide(None, 1600), Lay { rail: Rail::Full, narrow: false, push: false, overlay: false, narrow_beside: false, tight: false });
        // The usual tile: a slim rail and the details still beside the list.
        assert_eq!(decide(None, 1075), Lay { rail: Rail::Slim, narrow: false, push: false, overlay: true, narrow_beside: false, tight: true });
        // Narrower: the details push over the list.
        assert_eq!(decide(None, 900), Lay { rail: Rail::Slim, narrow: false, push: false, overlay: true, narrow_beside: false, tight: true });
        // Narrow: no rail, two-line rows.
        assert_eq!(decide(None, 700), Lay { rail: Rail::Hidden, narrow: true, push: true, overlay: false, narrow_beside: true, tight: true });
    }

    #[test]
    fn breakpoints_hold_past_their_line_once_crossed() {
        let wide = decide(None, 1400);
        assert_eq!(decide(Some(wide), SLIM_BELOW - 5).rail, Rail::Full);
        assert_eq!(decide(Some(wide), SLIM_BELOW - HYST - 1).rail, Rail::Slim);
        let slim = decide(None, 1000);
        assert_eq!(decide(Some(slim), SLIM_BELOW + 5).rail, Rail::Slim);
        assert_eq!(decide(Some(slim), SLIM_BELOW + HYST).rail, Rail::Full);
        assert_eq!(decide(Some(slim), HIDE_BELOW - 5).rail, Rail::Slim);
        assert_eq!(decide(Some(slim), HIDE_BELOW - HYST - 1).rail, Rail::Hidden);
    }

    #[test]
    fn the_slim_rail_opens_after_a_short_hover_and_not_on_a_crossing() {
        let mut s = Slide::default();
        let SlideDo::Wait(ms, t) = s.strip(true) else { panic!() };
        assert_eq!(ms, OPEN_AFTER);
        // Gone again before the wait ends: the wait does nothing.
        assert_eq!(s.strip(false), SlideDo::Nothing);
        assert_eq!(s.fire(t), SlideDo::Nothing);
        assert!(!s.open);
        // Staying: it opens.
        let SlideDo::Wait(_, t) = s.strip(true) else { panic!() };
        assert_eq!(s.fire(t), SlideDo::Open);
        assert!(s.open);
    }

    #[test]
    fn the_full_rail_counts_as_the_strip_and_closes_a_moment_after_the_pointer_leaves_both() {
        let mut s = Slide::default();
        let SlideDo::Wait(_, t) = s.strip(true) else { panic!() };
        s.fire(t);
        // The full rail comes up under the pointer: the strip loses it, the full rail gets it, in
        // either order, and nothing closes.
        let SlideDo::Wait(ms, t1) = s.strip(false) else { panic!() };
        assert_eq!(ms, CLOSE_AFTER);
        assert_eq!(s.overlay(true), SlideDo::Nothing);
        assert_eq!(s.fire(t1), SlideDo::Nothing);
        assert!(s.open);
        // Off it, and back within the moment: still open.
        let SlideDo::Wait(_, t2) = s.overlay(false) else { panic!() };
        assert_eq!(s.overlay(true), SlideDo::Nothing);
        assert_eq!(s.fire(t2), SlideDo::Nothing);
        // Off for good: closed.
        let SlideDo::Wait(_, t3) = s.overlay(false) else { panic!() };
        assert_eq!(s.fire(t3), SlideDo::Close);
        assert!(!s.open);
    }

    #[test]
    fn the_keyboard_opens_it_at_once_and_closes_it_on_leaving() {
        let mut s = Slide::default();
        assert_eq!(s.focus(true), SlideDo::Open);
        assert_eq!(s.focus(true), SlideDo::Nothing, "an arrow inside changes nothing");
        // The pointer passing over and away doesn't close it while the keyboard is in.
        assert_eq!(s.overlay(true), SlideDo::Nothing);
        assert_eq!(s.overlay(false), SlideDo::Nothing);
        assert!(s.open);
        assert_eq!(s.focus(false), SlideDo::Close);
        // Leaving while the pointer rests on it: open until the pointer goes.
        s.focus(true);
        s.overlay(true);
        assert_eq!(s.focus(false), SlideDo::Nothing);
        assert!(s.open);
    }

    #[test]
    fn a_pick_closes_it_and_it_stays_closed_until_the_pointer_leaves_the_strip() {
        let mut s = Slide::default();
        let SlideDo::Wait(_, t) = s.strip(true) else { panic!() };
        s.fire(t);
        s.strip(false);
        s.overlay(true);
        assert_eq!(s.dismiss(true), SlideDo::Close);
        // The pointer is on the strip again as the full rail goes: no reopening.
        assert_eq!(s.strip(true), SlideDo::Nothing);
        assert!(!s.open);
        // Out and back in: it opens as usual.
        s.strip(false);
        let SlideDo::Wait(_, t) = s.strip(true) else { panic!() };
        assert_eq!(s.fire(t), SlideDo::Open);
        // Picked with the pointer over the content side of the full rail: the strip opens it again.
        assert_eq!(s.dismiss(false), SlideDo::Close);
        assert!(matches!(s.strip(true), SlideDo::Wait(OPEN_AFTER, _)));
    }

    #[test]
    fn beside_the_details_the_list_keeps_its_width() {
        for width in (BODY_MIN..=2400).step_by(10) {
            let l = decide(None, width);
            let avail = width - rail_w(l.rail);
            // Beside the details (neither over the list nor a page of its own), the list keeps room.
            if !l.push && !l.overlay {
                assert!(avail - side_w(avail) >= LIST_MIN, "{width}");
            }
        }
    }
}
