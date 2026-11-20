//! Motion: quick and quiet (150–220 ms, easing out), and never in the way. A key changes the state
//! at once; the movement is painted after, so nothing waits on an animation.
//!
//! One switch for all of it: Settings' "Motion" (Full or Reduced) and the desktop's own
//! `gtk-enable-animations`. Reduced (or the desktop asking for less) turns GTK's own animations off
//! for this window (revealers, CSS animations) and makes the ones drawn here jump to their end.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::{self as gtk, glib, prelude::*};

use crate::app::Ui;

/// How long a transition takes, in milliseconds.
pub const DUR: u32 = 180;

thread_local! {
    /// What the desktop said at start: motion is never more than this.
    static SYSTEM: Cell<bool> = const { Cell::new(true) };
    static ON: Cell<bool> = const { Cell::new(true) };
}

/// Reads the desktop's preference once, before anything is drawn, then applies Wake's.
pub fn init() {
    let sys = gtk::Settings::default().is_none_or(|s| s.is_gtk_enable_animations());
    SYSTEM.with(|c| c.set(sys));
    set(wanted(&crate::prefs::get().motion));
}

/// Full motion only when both Wake's setting and the desktop allow it.
pub fn wanted(setting: &str) -> bool {
    setting != "reduced" && SYSTEM.with(Cell::get)
}

fn set(on: bool) {
    ON.with(|c| c.set(on));
    if let Some(s) = gtk::Settings::default()
        && s.is_gtk_enable_animations() != on
    {
        s.set_gtk_enable_animations(on);
    }
}

/// After Settings changed "Motion".
pub fn apply(ui: &Ui) {
    set(wanted(&crate::prefs::get().motion));
    ui.scope.borrow_mut().look.reduced_motion = !on();
    for r in ui.readouts.borrow().iter() {
        r.queue_draw();
    }
}

pub fn on() -> bool {
    ON.with(Cell::get)
}

/// Ease-out (cubic): fast at first, settling gently.
pub fn ease_out(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// A revealer with Wake's timing.
pub fn revealer(kind: gtk::RevealerTransitionType, child: &impl IsA<gtk::Widget>, open: bool) -> gtk::Revealer {
    let r = gtk::Revealer::builder().transition_type(kind).transition_duration(DUR).reveal_child(open).build();
    r.set_child(Some(child));
    if matches!(kind, gtk::RevealerTransitionType::Crossfade | gtk::RevealerTransitionType::None) {
        // A fading revealer keeps its child's full height even when closed (only the slides
        // shrink), which leaves a blank gap. Hide the child once it has faded out, and show it
        // again before it fades in, so a closed one takes no room.
        let child = child.clone().upcast::<gtk::Widget>();
        child.set_visible(open);
        let c = child.clone();
        r.connect_reveal_child_notify(move |r| {
            if r.reveals_child() {
                c.set_visible(true);
            }
        });
        r.connect_child_revealed_notify(move |r| {
            if !r.is_child_revealed() && !r.reveals_child() {
                child.set_visible(false);
            }
        });
    }
    r
}

/// Calls `f` with the progress (0 to 1, eased) on each frame for `ms`, ending on exactly 1. With
/// motion off, `f(1.0)` at once. Returns a handle that stops it early.
pub fn animate(w: &impl IsA<gtk::Widget>, ms: u32, f: impl Fn(f64) + 'static) -> Rc<Cell<bool>> {
    let stop = Rc::new(Cell::new(false));
    if !on() || !w.is_mapped() {
        f(1.0);
        return stop;
    }
    let start: Rc<Cell<Option<i64>>> = Rc::new(Cell::new(None));
    let stop2 = stop.clone();
    w.add_tick_callback(move |_, clock| {
        if stop2.get() {
            return glib::ControlFlow::Break;
        }
        let now = clock.frame_time();
        let t0 = start.get().unwrap_or_else(|| {
            start.set(Some(now));
            now
        });
        let t = (now - t0) as f64 / (f64::from(ms) * 1000.0);
        f(ease_out(t));
        if t >= 1.0 { glib::ControlFlow::Break } else { glib::ControlFlow::Continue }
    });
    stop
}

/// Runs `f` once the widget has been laid out (two frames on), when sizes and places are real.
pub fn after_layout(w: &impl IsA<gtk::Widget>, f: impl FnOnce() + 'static) {
    let f = RefCell::new(Some(f));
    if !w.is_mapped() {
        // Not on screen (a screenshot run before the window is up): do it on the next idle.
        glib::idle_add_local_once(move || {
            if let Some(f) = f.take() {
                f();
            }
        });
        return;
    }
    let n = Cell::new(0);
    w.add_tick_callback(move |_, _| {
        n.set(n.get() + 1);
        if n.get() < 2 {
            return glib::ControlFlow::Continue;
        }
        if let Some(f) = f.take() {
            f();
        }
        glib::ControlFlow::Break
    });
}

/// Fades a widget in (rows that arrive, a preview that opens).
pub fn fade_in(w: &impl IsA<gtk::Widget>) {
    if !on() {
        return;
    }
    let wd = w.clone().upcast::<gtk::Widget>();
    wd.set_opacity(0.0);
    let w2 = wd.clone();
    // Not mapped yet: start once it is.
    let go = move || {
        let w3 = w2.clone();
        animate(&w2, DUR, move |t| w3.set_opacity(t));
    };
    if wd.is_mapped() {
        go();
    } else {
        let id: Rc<RefCell<Option<glib::SignalHandlerId>>> = Rc::new(RefCell::new(None));
        let id2 = id.clone();
        let h = wd.connect_map(move |w| {
            if let Some(h) = id2.borrow_mut().take() {
                w.disconnect(h);
            }
            go();
        });
        *id.borrow_mut() = Some(h);
    }
}

/// Fades a widget out, then calls `done` (which removes it). With motion off, `done` at once.
pub fn fade_out(w: &impl IsA<gtk::Widget>, done: impl FnOnce() + 'static) {
    let done = RefCell::new(Some(done));
    let wd = w.clone().upcast::<gtk::Widget>();
    if !on() || !wd.is_mapped() {
        if let Some(d) = done.take() {
            d();
        }
        return;
    }
    wd.set_can_target(false);
    let w2 = wd.clone();
    animate(&wd, DUR, move |t| {
        w2.set_opacity(1.0 - t);
        if t >= 1.0
            && let Some(d) = done.take()
        {
            d();
        }
    });
}

/// Fades (and, for cards, grows) a widget in as it appears, with a CSS animation (style.css:
/// `.pa` and `.pb`). The two classes run the same animation under two names, so swapping one for
/// the other restarts it at once: the widget never shows at full strength before fading in.
pub fn pop(w: &impl IsA<gtk::Widget>) {
    if !on() {
        return;
    }
    if w.has_css_class("pa") {
        w.remove_css_class("pa");
        w.add_css_class("pb");
    } else {
        w.remove_css_class("pb");
        w.add_css_class("pa");
    }
}

/// Glides a scrolled view to `value` (clamped to its range).
pub fn scroll_to(scroll: &gtk::ScrolledWindow, value: f64) {
    let adj = scroll.vadjustment();
    let max = (adj.upper() - adj.page_size()).max(0.0);
    let to = value.clamp(0.0, max);
    let from = adj.value();
    if (to - from).abs() < 1.0 {
        return;
    }
    animate(scroll, DUR, move |t| adj.set_value(from + (to - from) * t));
}

/// Once laid out: scrolls so all of `w` is in view if it fits (else its top), moving as little as
/// possible and not at all when it already shows. For something just opened below the fold.
pub fn scroll_into_view(scroll: &gtk::ScrolledWindow, w: &impl IsA<gtk::Widget>) {
    let (scroll, w) = (scroll.clone(), w.clone().upcast::<gtk::Widget>());
    after_layout(&scroll.clone(), move || {
        let Some(child) = scroll.child() else { return };
        let Some(b) = w.compute_bounds(&child) else { return };
        let adj = scroll.vadjustment();
        let (top, bottom) = (b.y() as f64, (b.y() + b.height()) as f64);
        let (v, page) = (adj.value(), adj.page_size());
        let want = if top < v {
            top - 8.0
        } else if bottom > v + page {
            (bottom + 8.0 - page).min(top - 8.0)
        } else {
            return;
        };
        scroll_to(&scroll, want);
    });
}

/// Shows `w`, just opened under `anchor` (the row it belongs to), without moving `anchor`: nothing
/// scrolls while `w` already fits on screen; otherwise only as far as needed to show `w`'s end, and
/// never so far that `anchor` leaves the top. The row you opened stays where your eyes are.
pub fn reveal_below(scroll: &gtk::ScrolledWindow, anchor: &impl IsA<gtk::Widget>, w: &impl IsA<gtk::Widget>) {
    let (scroll, anchor, w) = (scroll.clone(), anchor.clone().upcast::<gtk::Widget>(), w.clone().upcast::<gtk::Widget>());
    after_layout(&scroll.clone(), move || {
        let Some(child) = scroll.child() else { return };
        let (Some(a), Some(b)) = (anchor.compute_bounds(&child), w.compute_bounds(&child)) else { return };
        let adj = scroll.vadjustment();
        let (v, page) = (adj.value(), adj.page_size());
        if let Some(want) = reveal_target(v, page, a.y() as f64, (b.y() + b.height()) as f64) {
            scroll_to(&scroll, want);
        }
    });
}

/// Where to scroll so the content ending at `bottom` shows while the anchor row starting at
/// `anchor_top` stays on screen; None when nothing needs to move.
fn reveal_target(v: f64, page: f64, anchor_top: f64, bottom: f64) -> Option<f64> {
    if bottom <= v + page {
        return None;
    }
    let want = (bottom + 8.0 - page).min(anchor_top - 8.0).max(0.0);
    (want > v).then_some(want)
}

#[cfg(test)]
mod tests {
    #[test]
    fn opening_under_a_row_never_moves_the_row_out_of_view() {
        use super::reveal_target;
        // Fits already: nothing moves.
        assert_eq!(reveal_target(0.0, 600.0, 200.0, 500.0), None);
        // Runs past the bottom a little: scroll just enough.
        assert_eq!(reveal_target(0.0, 600.0, 400.0, 700.0), Some(108.0));
        // Taller than the screen: the row goes to the top, not past it.
        assert_eq!(reveal_target(0.0, 600.0, 400.0, 2000.0), Some(392.0));
        // Never scrolls up to reveal something below.
        assert_eq!(reveal_target(500.0, 600.0, 450.0, 1200.0), None);
    }

    use super::*;

    #[test]
    fn eases_out_and_stays_in_range() {
        assert_eq!(ease_out(0.0), 0.0);
        assert_eq!(ease_out(1.0), 1.0);
        assert_eq!(ease_out(2.0), 1.0);
        assert!(ease_out(0.5) > 0.5, "ease-out is past half-way at half time");
    }

    #[test]
    fn the_desktop_wins() {
        SYSTEM.with(|c| c.set(false));
        assert!(!wanted("full"));
        SYSTEM.with(|c| c.set(true));
        assert!(wanted("full"));
        assert!(!wanted("reduced"));
    }
}
