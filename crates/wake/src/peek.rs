//! Hold to peek: hold Space or Tab on an agent's row (Monitor, Agents, or the switcher) to look at
//! it while the key is held. Release comes back; Enter while holding stays there ("Go to"); Esc
//! comes back too. A quick tap keeps its usual meaning (Space activates, Tab moves the focus).
//!
//! How Wake shows the agent is the core's choice ([`Source::look_kind`]): on Slipstream or KDE
//! Plasma the desktop flies to the agent's window (in any terminal app) and highlights it, with the
//! keyboard left on Wake; anywhere else, and for agents with no window (a background session with
//! nothing attached, or a terminal with no window such as tmux), Wake floats a live
//! text preview of the agent's terminal ([`Source::screen`]) over its own window. The preview is a
//! popover that never takes the keyboard, so the key release always reaches Wake.
//!
//! The hold-or-tap decision is a pure state machine ([`step`]), tested without a window.

use std::time::Duration;

use gtk4::{self as gtk, gdk, glib, prelude::*};
use wake_core::*;

use crate::{
    app::{self, Ui, View},
    switcher, w,
};

/// A press held this long is a peek; shorter is a tap.
pub const HOLD_MS: i64 = 180;
const SCREEN_POLL: Duration = Duration::from_millis(250);
const LOGS_POLL: Duration = Duration::from_millis(1000);
const SUBS_EVERY: u32 = 8;

// ---------- the hold-or-tap state machine (pure) ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldKey {
    Space,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    #[default]
    Idle,
    /// Pressed at `since` (ms); not yet long enough to be a hold.
    Pending { key: HoldKey, since: i64 },
    /// Peeking while `key` is held.
    Peeking { key: HoldKey },
    /// The peek ended (Enter, Esc, or the desktop ended it) but `key` is still down: its repeats and
    /// release are swallowed so they can't start another peek or reach the list.
    Spent { key: HoldKey },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ev {
    Down(HoldKey, i64),
    Up(HoldKey, i64),
    /// The hold timer fired.
    Timer(i64),
    Enter,
    Escape,
    /// Any other key.
    Other,
    FocusLost,
    /// The desktop ended the look by itself.
    Ended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    /// Not ours: the key goes on to the normal handler.
    Pass,
    /// Ours, and nothing to do.
    Swallow,
    /// Start the hold timer.
    Arm,
    /// It was a tap: do the key's usual thing.
    Tap(HoldKey),
    /// A tap interrupted by another key: do the tap, then let the other key through.
    TapThenPass(HoldKey),
    Start,
    End {
        stay: bool,
    },
}

/// One step of the machine.
pub fn step(p: Phase, ev: Ev) -> (Phase, Act) {
    use Act::*;
    use Phase::*;
    match (p, ev) {
        (Idle, Ev::Down(key, t)) => (Pending { key, since: t }, Arm),
        (Idle, Ev::Ended | Ev::Timer(_) | Ev::FocusLost) => (Idle, Swallow),
        (Idle, _) => (Idle, Pass),

        // Key repeat while still deciding: a repeat past the threshold starts the peek early.
        (Pending { key, since }, Ev::Down(k, t)) if k == key => {
            if t - since >= HOLD_MS {
                (Peeking { key }, Start)
            } else {
                (p, Swallow)
            }
        }
        (Pending { key, .. }, Ev::Down(..)) => (Idle, TapThenPass(key)),
        (Pending { key, since }, Ev::Up(k, t)) if k == key => {
            // Released before the timer came round: a tap if it was short, else nothing at all
            // (the person held it, but no peek was shown, so don't surprise them with a tap).
            (Idle, if t - since < HOLD_MS { Tap(key) } else { Swallow })
        }
        (Pending { .. }, Ev::Up(..)) => (p, Swallow),
        (Pending { key, since }, Ev::Timer(t)) => {
            if t - since >= HOLD_MS {
                (Peeking { key }, Start)
            } else {
                (p, Swallow)
            }
        }
        (Pending { key, .. }, Ev::Enter | Ev::Escape | Ev::Other) => (Idle, TapThenPass(key)),
        (Pending { .. }, Ev::FocusLost) => (Idle, Swallow),
        (Pending { .. }, Ev::Ended) => (p, Swallow),

        (Peeking { key }, Ev::Up(k, _)) if k == key => (Idle, End { stay: false }),
        (Peeking { key }, Ev::Enter) => (Spent { key }, End { stay: true }),
        (Peeking { key }, Ev::Escape) => (Spent { key }, End { stay: false }),
        (Peeking { .. }, Ev::FocusLost) => (Idle, End { stay: false }),
        (Peeking { key }, Ev::Ended) => (Spent { key }, Swallow),
        // Everything else while peeking goes nowhere.
        (Peeking { .. }, _) => (p, Swallow),

        (Spent { key }, Ev::Up(k, _)) if k == key => (Idle, Swallow),
        (Spent { key }, Ev::Down(k, _)) if k == key => (p, Swallow),
        (Spent { .. }, Ev::FocusLost) => (Idle, Swallow),
        (Spent { .. }, Ev::Timer(_) | Ev::Ended | Ev::Up(..)) => (p, Swallow),
        (Spent { .. }, _) => (p, Pass),
    }
}

/// The hold key for a key press, if it is one: Space or Tab with no modifier.
pub fn hold_key(key: gdk::Key, mods: gdk::ModifierType) -> Option<HoldKey> {
    let m = gdk::ModifierType::SHIFT_MASK | gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK | gdk::ModifierType::SUPER_MASK;
    if mods.intersects(m) {
        return None;
    }
    match key {
        gdk::Key::space => Some(HoldKey::Space),
        _ => None,
    }
}

// ---------- the app side ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    None,
    /// Asked the desktop to fly there; waiting for its answer.
    Asking,
    /// The desktop is showing the agent's window.
    Flying,
    Preview,
}

#[derive(Clone)]
struct PreviewW {
    pop: gtk::Popover,
    card: gtk::Box,
    /// The card sits in Wake's own window instead when the popover can't be shown (a desktop
    /// that refuses the popup, or Broadway for screenshots).
    inside: std::rc::Rc<std::cell::Cell<bool>>,
    title: gtk::Label,
    meta: gtk::Label,
    source: gtk::Label,
    body: gtk::Label,
    scroll: gtk::ScrolledWindow,
    note: gtk::Label,
    subs: gtk::Box,
}

#[derive(Default)]
pub struct Peek {
    pub phase: Phase,
    /// How this desktop lets Wake look, once the core has said.
    pub kind: Option<LookKind>,
    target: Option<String>,
    pub mode: Mode,
    /// Bumped whenever a peek starts or ends, so late answers for an old peek are dropped.
    epoch: u64,
    view: Option<PreviewW>,
    text: String,
    /// Screenshots: read the screen only this many more times.
    reads_left: Option<u32>,
    /// Space is down and its press went on to what has the keyboard (not a session row): its
    /// repeats are dropped until it is let go, so holding Space never presses a button again and
    /// again. Typing (a text box) keeps its repeats.
    through: bool,
}

/// Log a peek step to stderr when `WAKE_PEEK_LOG` is set (the hold test reads it).
fn log(what: impl FnOnce() -> String) {
    if std::env::var_os("WAKE_PEEK_LOG").is_some() {
        eprintln!("peek {:>6} ms: {}", now_ms() % 1_000_000, what());
    }
}

pub fn now_ms() -> i64 {
    glib::monotonic_time() / 1000
}

/// Asks the core once how this desktop can show a window (read-only D-Bus checks). When that
/// changes what holding Space does, the rows and the details say so again.
pub fn learn_kind(ui: &Ui) {
    app::bg(
        ui,
        |src| src.look_kind(),
        |ui, k| {
            let before = can_fly(ui);
            ui.peek.borrow_mut().kind = Some(k);
            if can_fly(ui) != before {
                app::render_all(ui);
            } else {
                crate::views::render_status(ui);
            }
        },
    );
}

/// Whether this desktop can fly to a window.
pub fn can_fly(ui: &Ui) -> bool {
    ui.peek.try_borrow().is_ok_and(|p| matches!(p.kind, Some(LookKind::Slipstream | LookKind::Kwin)))
}

/// Whether holding Space on session `s` zooms to its window (else it previews it).
pub fn zooms(ui: &Ui, s: &SessionSummary) -> bool {
    s.location.is_some() && can_fly(ui)
}

/// What holding Space does for `s`, as the Space key and a few words: "zoom to window" when a
/// window shows it, else "preview". `long` gives the details panel's fuller words.
pub fn cue(ui: &Ui, s: &SessionSummary, long: bool) -> gtk::Box {
    let zoom = zooms(ui, s);
    let words = match (zoom, long) {
        (true, false) => "zoom to window",
        (true, true) => "hold to zoom to its window",
        (false, false) => "preview",
        (false, true) => "hold to preview",
    };
    let b = w::hbox(5, &["peek-cue", if zoom { "zoom" } else { "look" }]);
    b.append(&w::kbd("Space"));
    b.append(&w::label(words, &["hint"]));
    b.set_tooltip_text(Some(if zoom {
        "Hold Space to zoom to the window this session is in. Let go to come back to Wake, or press Enter while holding to stay there."
    } else if can_fly(ui) {
        "No window shows this session, so holding Space previews its screen here instead. Let go to close it."
    } else {
        "Hold Space to preview this session's screen without leaving Wake. Let go to close it."
    }));
    b
}

thread_local! {
    /// Every "PEEK AVAILABLE" chip (one in each page's title row), and what they say now: shown or
    /// not, and the tooltip. A page drawn later starts with the same.
    static CHIPS: std::cell::RefCell<(Vec<glib::WeakRef<gtk::Label>>, Option<String>)> = Default::default();
}

/// A "PEEK AVAILABLE" chip for a page's title row, at the end where its actions go. It shows while
/// holding Space would peek at the session the keyboard is on.
pub fn chip() -> gtk::Label {
    let c = w::label("PEEK AVAILABLE", &["pill", "mint", "peekchip"]);
    c.set_valign(gtk::Align::Center);
    CHIPS.with(|cs| {
        let mut cs = cs.borrow_mut();
        cs.0.retain(|x| x.upgrade().is_some());
        cs.0.push(c.downgrade());
        show_chip(&c, cs.1.as_deref());
    });
    c
}

fn show_chip(c: &gtk::Label, tip: Option<&str>) {
    c.set_visible(tip.is_some());
    c.set_tooltip_text(tip);
    if let Some(t) = tip {
        c.update_property(&[gtk::accessible::Property::Description(t)]);
    }
}

/// Shows the chips while holding Space would peek at the session the keyboard is on (the same
/// check a held Space makes), and says in their tooltip what it would do.
pub fn update_chip(ui: &Ui) {
    if ui.st.try_borrow_mut().is_err() || ui.peek.try_borrow_mut().is_err() {
        return;
    }
    // While a peek is on, the chips stay as they were when it began.
    if ui.peek.borrow().phase != Phase::Idle {
        return;
    }
    let session = target(ui, HoldKey::Space).and_then(|id| ui.st.borrow().session(&id).cloned());
    let tip = session.map(|s| {
        if zooms(ui, &s) {
            format!("Hold Space to zoom to the window \"{}\" is in. Let go to come back to Wake, or press Enter while holding to stay there.", s.title)
        } else if can_fly(ui) {
            format!("Hold Space to preview \"{}\" here: no window shows it. Let go to close the preview.", s.title)
        } else {
            format!("Hold Space to preview \"{}\" here. Let go to close the preview.", s.title)
        }
    });
    CHIPS.with(|cs| {
        let mut cs = cs.borrow_mut();
        cs.0.retain(|x| x.upgrade().is_some());
        for c in cs.0.iter().filter_map(|x| x.upgrade()) {
            show_chip(&c, tip.as_deref());
        }
        cs.1 = tip;
    });
}

/// The session a peek would show from where the keyboard is now, if any.
fn target(ui: &Ui, key: HoldKey) -> Option<String> {
    let st = ui.st.borrow();
    if st.modal_open {
        return None;
    }
    if st.sw.open {
        // Space types into the filter once something is typed; Tab is free.
        if key == HoldKey::Space && !st.sw.q.is_empty() {
            return None;
        }
        drop(st);
        return switcher::selected_session(ui);
    }
    let focus = gtk::prelude::GtkWindowExt::focus(&ui.window)?;
    // From inside a session's side panel (Space tapped on a row opens it and the keyboard goes
    // there): the panel's session. A tap is still the focused button's, a hold peeks.
    if let Some(app::Drawer::Session(id)) = &st.drawer
        && focus.is_ancestor(&ui.w.drawer)
        && !(focus.is::<gtk::Text>() || focus.is::<gtk::Entry>() || focus.is::<gtk::TextView>())
    {
        return st.ov.as_ref()?.sessions.iter().any(|s| s.id == *id && app::is_live(s.state)).then(|| id.clone());
    }
    if !matches!(st.view, View::Monitor | View::Agents) {
        return None;
    }
    // Only from the table itself, so Space on a button is still the button's.
    let list = &ui.w.views[&st.view].list;
    if !focus.is::<gtk::ListBoxRow>() || !focus.is_ancestor(list) || focus.widget_name() != st.sel.clone().unwrap_or_default().as_str() {
        return None;
    }
    let sel = st.sel.clone()?;
    let id = match st.view {
        // A subagent's row peeks its session.
        View::Agents => crate::agents::selected_proc(&st)?.session_id.clone()?,
        _ => sel,
    };
    st.ov.as_ref()?.sessions.iter().any(|s| s.id == id).then_some(id)
}

/// The window's key press, before anything else sees it. None: not the peek's, carry on.
pub fn on_press(ui: &Ui, key: gdk::Key, mods: gdk::ModifierType) -> Option<glib::Propagation> {
    let Ok(p) = ui.peek.try_borrow() else { return None };
    let (phase, through) = (p.phase, p.through);
    drop(p);
    let hk = hold_key(key, mods);
    let ev = match (phase, hk) {
        // A repeat of a Space that went on to a button or a row's own action: dropped.
        (Phase::Idle, Some(_)) if through => return Some(glib::Propagation::Stop),
        (Phase::Idle, Some(k)) => {
            let Some(id) = target(ui, k) else {
                ui.peek.borrow_mut().through = !typing(ui);
                return None;
            };
            ui.peek.borrow_mut().target = Some(id);
            Ev::Down(k, now_ms())
        }
        (Phase::Idle, None) => return None,
        (_, Some(k)) => Ev::Down(k, now_ms()),
        _ => match key {
            gdk::Key::Return | gdk::Key::KP_Enter => Ev::Enter,
            gdk::Key::Escape => Ev::Escape,
            _ => Ev::Other,
        },
    };
    feed(ui, ev)
}

pub fn on_release(ui: &Ui, key: gdk::Key) {
    let k = match key {
        gdk::Key::space => HoldKey::Space,
        _ => return,
    };
    let Ok(mut p) = ui.peek.try_borrow_mut() else { return };
    p.through = false;
    let busy = p.phase != Phase::Idle;
    drop(p);
    if busy {
        feed(ui, Ev::Up(k, now_ms()));
    }
}

/// Whether the keyboard is in a text box, where a held Space types spaces.
fn typing(ui: &Ui) -> bool {
    gtk::prelude::GtkWindowExt::focus(&ui.window).is_some_and(|f| f.is::<gtk::Text>() || f.is::<gtk::Entry>() || f.is::<gtk::TextView>())
}

/// Wake's window stopped being the active one: a peek ends, and a Space let go elsewhere is
/// forgotten.
pub fn focus_lost(ui: &Ui) {
    if let Ok(mut p) = ui.peek.try_borrow_mut() {
        p.through = false;
    }
    if ui.peek.borrow().phase != Phase::Idle {
        feed(ui, Ev::FocusLost);
    }
}

fn feed(ui: &Ui, ev: Ev) -> Option<glib::Propagation> {
    use glib::Propagation::Stop;
    // Never panic inside a GLib callback (that aborts the app): a key arriving while the peek
    // state is busy is simply not the peek's.
    let Ok(mut p) = ui.peek.try_borrow_mut() else { return None };
    let before = p.phase;
    let (phase, act) = step(before, ev);
    p.phase = phase;
    drop(p);
    if !matches!((ev, act), (Ev::Down(..), Act::Swallow)) {
        log(|| format!("{ev:?} in {before:?} -> {act:?}, now {phase:?}"));
    } else {
        log(|| format!("{ev:?} (a key repeat) swallowed"));
    }
    match act {
        Act::Pass => return None,
        Act::Swallow => {}
        Act::Arm => {
            let ui = ui.clone();
            glib::timeout_add_local_once(Duration::from_millis(HOLD_MS as u64 + 5), move || {
                feed(&ui, Ev::Timer(now_ms()));
            });
        }
        Act::Tap(k) => tap(ui, k),
        Act::TapThenPass(k) => {
            tap(ui, k);
            return None;
        }
        Act::Start => {
            // Bind first: start() borrows the peek state mutably, and a borrow held across it
            // panics inside a GLib callback, which aborts the whole app.
            let target = ui.peek.borrow().target.clone();
            if let Some(id) = target {
                start(ui, &id, false);
            }
        }
        Act::End { stay } => end(ui, stay),
    }
    Some(Stop)
}

/// A tap's usual meaning: Space activates the row (or, in the switcher, is a space in the filter).
fn tap(ui: &Ui, k: HoldKey) {
    match k {
        HoldKey::Space if ui.st.borrow().sw.open => {
            let e = &ui.w.sw_entry;
            let mut pos = e.position();
            e.insert_text(" ", &mut pos);
            e.set_position(pos);
        }
        HoldKey::Space => {
            if let Some(f) = gtk::prelude::GtkWindowExt::focus(&ui.window) {
                f.activate();
            }
        }
    }
}

/// Start looking at session `id`: fly there if the desktop can, else the preview. `preview_only`
/// (screenshots) never asks the desktop to move.
pub fn start(ui: &Ui, id: &str, preview_only: bool) {
    let epoch = {
        let mut p = ui.peek.borrow_mut();
        p.epoch += 1;
        p.target = Some(id.to_string());
        p.epoch
    };
    let kind = ui.peek.borrow().kind;
    let can_fly = matches!(kind, Some(LookKind::Slipstream | LookKind::Kwin));
    // Always the agent's own window first, whatever terminal it is in and however busy it is;
    // the core says when there is none (a background session nothing is attached to), and then
    // it's the preview.
    if preview_only || !can_fly {
        log(|| format!("preview for {id} (can fly: {can_fly})"));
        open_preview(ui, id, epoch, None);
        return;
    }
    ui.peek.borrow_mut().mode = Mode::Asking;
    let sid = id.to_string();
    let hook = crate::hooks::look();
    log(|| format!("asking the desktop to fly to {}", hook.as_ref().map_or(format!("session {id}'s window"), |(_, p)| format!("the window holding pid {p}"))));
    app::bg(
        ui,
        move |src| match hook {
            Some((l, pid)) => {
                let loc = Location { pid, app: None };
                l.start(Some(&loc), false)
            }
            None => src.look_start(&sid, false),
        },
        {
            let id = id.to_string();
            move |ui: &Ui, r: ActionResult| {
                let current = ui.peek.borrow().epoch == epoch;
                log(|| format!("the desktop answered: ok {} '{}'{}", r.ok, r.message, if current { "" } else { " (already released)" }));
                if !current {
                    // Released before the desktop answered: put it back at once.
                    if r.ok {
                        app::bg(ui, |src| look_end(src, false), |_, _| {});
                    }
                    return;
                }
                if r.ok {
                    ui.peek.borrow_mut().mode = Mode::Flying;
                    watch_ended(ui, epoch);
                } else {
                    // Refused or not there any more: the preview, and ask the core again next time.
                    learn_kind(ui);
                    open_preview(ui, &id, epoch, Some(r.message));
                }
            }
        },
    );
}

/// While flying: notice the desktop ending the look by itself.
fn watch_ended(ui: &Ui, epoch: u64) {
    let ui = ui.clone();
    glib::timeout_add_local(Duration::from_millis(300), move || {
        if ui.peek.borrow().epoch != epoch || ui.peek.borrow().mode != Mode::Flying {
            return glib::ControlFlow::Break;
        }
        let hook = crate::hooks::look().map(|(l, _)| l);
        app::bg(
            &ui,
            move |src| hook.map_or_else(|| src.look_ended(), |l| l.ended()),
            move |ui, ended| {
                if ended.is_some() && ui.peek.borrow().epoch == epoch {
                    log(|| format!("the desktop ended the look by itself (stayed: {ended:?})"));
                    ui.peek.borrow_mut().mode = Mode::None;
                    feed(ui, Ev::Ended);
                }
            },
        );
        glib::ControlFlow::Continue
    });
}

/// The desktop's End, through the test hook's link when it is in use.
fn look_end(src: &dyn Source, stay: bool) -> ActionResult {
    match crate::hooks::look() {
        Some((l, _)) => l.end(stay, false),
        None => src.look_end(stay, false),
    }
}

/// End the peek: back to Wake as it was, or (`stay`) over to the agent.
pub fn end(ui: &Ui, stay: bool) {
    let (mode, id) = {
        let mut p = ui.peek.borrow_mut();
        p.epoch += 1;
        p.reads_left = None;
        (std::mem::take(&mut p.mode), p.target.take())
    };
    match mode {
        Mode::Flying => app::bg(
            ui,
            move |src| look_end(src, stay),
            move |ui, r| {
                log(|| format!("End(stay={stay}) answered ok {}", r.ok));
                if !r.ok {
                    app::toast(ui, "The desktop didn't answer; switch back yourself if it didn't.", true);
                }
            },
        ),
        Mode::Preview => {
            if let Some(v) = preview(ui) {
                if v.inside.get() {
                    v.card.set_visible(false);
                } else {
                    v.pop.popdown();
                }
            }
            if stay && let Some(id) = &id {
                app::go_to(ui, id);
            }
        }
        // Asked but not answered: the answer's handler puts the desktop back.
        Mode::Asking | Mode::None => {}
    }
}

fn build_preview(ui: &Ui) -> PreviewW {
    let card = w::vbox(0, &["peek"]);
    let head = w::hbox(12, &["peek-head"]);
    let names = w::vbox(2, &[]);
    names.set_hexpand(true);
    let title = w::clip("", &["peek-title"]);
    let meta = w::clip("", &["peek-meta"]);
    names.append(&title);
    names.append(&meta);
    head.append(&names);
    let source = w::label("", &["peek-source"]);
    source.set_valign(gtk::Align::Start);
    head.append(&source);
    card.append(&head);
    let body = w::label("", &["peek-term"]);
    body.set_valign(gtk::Align::End);
    body.set_yalign(1.0);
    body.set_selectable(false);
    let scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&body).build();
    scroll.add_css_class("peek-scroll");
    // Always the newest lines, as a terminal shows them.
    scroll.vadjustment().connect_changed(|a| a.set_value(a.upper() - a.page_size()));
    card.append(&scroll);
    let note = w::wrap("", &["peek-note"]);
    note.set_visible(false);
    card.append(&note);
    let subs = w::vbox(3, &["peek-subs"]);
    subs.set_visible(false);
    card.append(&subs);
    let foot = w::hbox(14, &["peek-foot"]);
    for (k, word) in [("release", "back to Wake"), ("Enter", "go to it"), ("Esc", "close")] {
        let b = w::hbox(5, &[]);
        b.append(&w::kbd(k));
        b.append(&w::label(word, &[]));
        foot.append(&b);
    }
    card.append(&foot);
    let pop = gtk::Popover::new();
    pop.add_css_class("peek-pop");
    pop.set_autohide(false);
    pop.set_has_arrow(false);
    pop.set_can_focus(false);
    pop.set_position(gtk::PositionType::Bottom);
    pop.set_child(Some(&card));
    card.set_can_target(true);
    pop.set_parent(&ui.w.column);
    pop.update_property(&[gtk::accessible::Property::Label("Live preview of the agent's terminal")]);
    PreviewW { pop, card, inside: std::rc::Rc::new(std::cell::Cell::new(false)), title, meta, source, body, scroll, note, subs }
}

/// The preview's widgets, cloned out so no borrow of the peek state is held while GTK runs.
fn preview(ui: &Ui) -> Option<PreviewW> {
    ui.peek.try_borrow().ok()?.view.clone()
}

fn open_preview(ui: &Ui, id: &str, epoch: u64, why: Option<String>) {
    if ui.peek.borrow().view.is_none() {
        let v = build_preview(ui);
        ui.peek.borrow_mut().view = Some(v);
    }
    {
        let mut p = ui.peek.borrow_mut();
        p.mode = Mode::Preview;
        p.text.clear();
    }
    let s = ui.st.borrow().session(id).cloned();
    if let Some(v) = preview(ui) {
        let v = &v;
        v.title.set_text(s.as_ref().map_or("", |s| s.title.as_str()));
        v.meta.set_text(&s.as_ref().map(|s| format!("{} · {}", crate::disk::project_words(ui, s), w::state_word(s.state).1)).unwrap_or_default());
        v.source.set_text("reading…");
        v.body.set_text("");
        v.note.set_visible(why.as_deref().is_some_and(|m| !m.is_empty() && !m.contains("showing a preview")));
        v.note.set_text(why.as_deref().unwrap_or(""));
        v.subs.set_visible(false);
        // Floats over the middle of Wake, a little above centre: 940 by 560 when there is room,
        // else Wake's width less a margin each side.
        let (wd, ht) = (ui.w.column.width(), ui.w.column.height());
        v.card.set_size_request((wd - 48).clamp(320, 940), (ht - 120).clamp(300, 560));
        if !v.inside.get() {
            v.pop.set_pointing_to(Some(&gdk::Rectangle::new(wd / 2, (ht / 2 - 300).max(40), 1, 1)));
            v.pop.popup();
        }
        if v.inside.get() || !v.pop.is_visible() {
            float_inside(ui, v);
        }
        // The preview fades in; holding the key already did what it does.
        crate::motion::pop(&v.card);
    }
    // A desktop can refuse the popup a moment later; then float the card inside Wake instead.
    let ui2 = ui.clone();
    glib::timeout_add_local_once(Duration::from_millis(150), move || {
        let current = ui2.peek.try_borrow().is_ok_and(|p| p.epoch == epoch && p.mode == Mode::Preview);
        if current
            && let Some(v) = preview(&ui2)
            && !v.inside.get()
            && !v.pop.is_visible()
        {
            float_inside(&ui2, &v);
        }
    });
    poll(ui, id.to_string(), epoch, 0);
}

/// Show the preview card in Wake's own window (the popover couldn't be shown).
fn float_inside(ui: &Ui, v: &PreviewW) {
    if !v.inside.get() {
        v.inside.set(true);
        v.pop.set_child(None::<&gtk::Widget>);
        v.card.set_halign(gtk::Align::Center);
        v.card.set_valign(gtk::Align::Start);
        v.card.add_css_class("peek-inside");
        if let Some(o) = ui.window.child().and_downcast::<gtk::Overlay>() {
            o.add_overlay(&v.card);
        }
    }
    v.card.set_margin_top((ui.w.column.height() / 2 - 300).max(40));
    v.card.set_visible(true);
}

/// Read the screen off the UI thread, show it, and come round again while the preview is open.
fn poll(ui: &Ui, id: String, epoch: u64, n: u32) {
    {
        let mut p = ui.peek.borrow_mut();
        if p.epoch != epoch || p.mode != Mode::Preview {
            return;
        }
        if let Some(left) = p.reads_left.as_mut() {
            if *left == 0 {
                return;
            }
            *left -= 1;
        }
    }
    let subs_too = n.is_multiple_of(SUBS_EVERY);
    let sid = id.clone();
    app::bg(
        ui,
        move |src| {
            let screen = src.screen(&sid);
            let subs = if subs_too { Some(src.session(&sid).map(|d| d.subagents.into_iter().filter(|a| a.state == State::Working).collect::<Vec<_>>()).unwrap_or_default()) } else { None };
            (screen, subs)
        },
        move |ui, (screen, subs)| {
            if ui.peek.borrow().epoch != epoch {
                return;
            }
            let next = show(ui, screen.as_ref(), subs);
            let ui2 = ui.clone();
            glib::timeout_add_local_once(next, move || poll(&ui2, id, epoch, n + 1));
        },
    );
}

/// Put a screen into the preview; returns how soon to read again.
fn show(ui: &Ui, screen: Option<&Screen>, subs: Option<Vec<SubagentSummary>>) -> Duration {
    let Some(v) = preview(ui) else { return LOGS_POLL };
    // Only the lines that fit, newest last, as the terminal itself shows them.
    let fit = Some(&v).map_or(40, |v| {
        let line = v.body.create_pango_layout(Some("X")).pixel_size().1.max(10);
        let room = if v.scroll.height() > 0 { v.scroll.height() } else { 380 };
        ((room - 26) / line).max(6) as usize
    });
    let text = screen.map(|s| s.lines[s.lines.len().saturating_sub(fit)..].join("\n")).unwrap_or_default();
    let changed = {
        let mut p = ui.peek.borrow_mut();
        let changed = p.text != text;
        if changed {
            p.text = text.clone();
        }
        changed
    };
    let background = screen.is_some_and(|s| s.source == "claude logs");
    match screen {
        Some(s) => {
            v.source.set_text(&if background { "background · via claude logs".to_string() } else { format!("{} · live", s.source) });
            if let Some(n) = &s.note {
                v.note.set_text(n);
                v.note.set_visible(true);
            } else if v.note.text().as_str() == crate::peek::NO_SCREEN_WORDS {
                v.note.set_visible(false);
            }
        }
        None => {
            // Running, but in a terminal app, whose screen Wake can't read: don't call it stopped.
            let target = ui.peek.borrow().target.clone();
            let live = target.is_some_and(|id| ui.st.borrow().session(&id).is_some_and(|s| app::is_live(s.state)));
            if live {
                v.source.set_text("can't read this terminal");
                v.note.set_text("This agent is running, but Wake can't read its terminal, so there is no screen to show here.");
            } else {
                v.source.set_text("not running");
                v.note.set_text("This session isn't running, so there is no live screen to show. Enter opens it.");
            }
            v.note.set_visible(true);
        }
    }
    if changed {
        v.body.set_text(&text);
        // Newest lines in view, once the new text is laid out.
        let adj = v.scroll.vadjustment();
        glib::idle_add_local_once(move || adj.set_value(adj.upper() - adj.page_size()));
        let adj = v.scroll.vadjustment();
        glib::timeout_add_local_once(Duration::from_millis(60), move || adj.set_value(adj.upper() - adj.page_size()));
    }
    if let Some(subs) = subs {
        w::clear(&v.subs);
        for a in subs.iter().take(4) {
            let row = w::hbox(8, &["peek-sub"]);
            row.append(&w::label("▸", &["peek-sub-mark"]));
            row.append(&w::label(&a.task, &["peek-sub-task"]));
            row.append(&w::clip(a.live.as_deref().unwrap_or("working"), &["peek-sub-live"]));
            v.subs.append(&row);
        }
        if subs.len() > 4 {
            v.subs.append(&w::label(&format!("and {} more", subs.len() - 4), &["peek-sub-live"]));
        }
        v.subs.set_visible(!subs.is_empty());
    }
    if background { LOGS_POLL } else { SCREEN_POLL }
}

const NO_SCREEN_WORDS: &str = wake_core::vt::NO_SCREEN;

/// `--show peek[:session]`: the preview for a screenshot, never asking the desktop to move. On real
/// data only a background session is shown (its screen is the only one Wake can read).
pub fn show_for_shot(ui: &Ui, arg: Option<String>) {
    let pick = {
        let st = ui.st.borrow();
        let ov = st.ov.as_ref();
        let ok = |s: &&SessionSummary| app::is_live(s.state) && (ui.demo || s.job_id.is_some());
        match arg {
            Some(id) => ov.and_then(|o| o.sessions.iter().find(|s| s.id == id || s.id.starts_with(&id))).filter(ok).map(|s| s.id.clone()),
            None => ov.and_then(|o| o.sessions.iter().filter(ok).find(|s| s.state == State::Working).or_else(|| o.sessions.iter().find(ok)).map(|s| s.id.clone())),
        }
    };
    let Some(id) = pick else {
        eprintln!("wake: --show peek: no live session {}to preview", if ui.demo { "" } else { "in the background " });
        return;
    };
    ui.peek.borrow_mut().reads_left = Some(3);
    start(ui, &id, true);
}

/// For screenshots: the open preview (when it is a popover, its own surface) and where it sits.
pub fn shot_layer(ui: &Ui) -> Option<(gtk::Widget, f64, f64)> {
    let p = ui.peek.borrow();
    let v = p.view.as_ref().filter(|v| !v.inside.get() && v.pop.is_visible())?;
    let wd = v.pop.width() as f64;
    let (cw, ch) = (ui.w.column.width() as f64, ui.w.column.height() as f64);
    let y = (ch / 2.0 - 300.0).max(40.0) + (ui.window.height() as f64 - ch);
    Some((v.pop.clone().upcast(), (cw - wd) / 2.0, y))
}

#[cfg(test)]
mod tests {
    use super::*;
    use Act::*;
    use HoldKey::*;

    fn run(evs: &[Ev]) -> (Phase, Vec<Act>) {
        let mut p = Phase::Idle;
        let mut acts = vec![];
        for e in evs {
            let (np, a) = step(p, *e);
            p = np;
            acts.push(a);
        }
        (p, acts)
    }

    #[test]
    fn a_quick_tap_keeps_its_meaning() {
        let (p, a) = run(&[Ev::Down(Space, 0), Ev::Up(Space, 90), Ev::Timer(185)]);
        assert_eq!(a, vec![Arm, Tap(Space), Swallow]);
        assert_eq!(p, Phase::Idle);
    }

    #[test]
    fn holding_peeks_and_release_comes_back() {
        let (p, a) = run(&[Ev::Down(Space, 0), Ev::Down(Space, 30), Ev::Timer(185), Ev::Down(Space, 220), Ev::Up(Space, 900)]);
        assert_eq!(a, vec![Arm, Swallow, Start, Swallow, End { stay: false }]);
        assert_eq!(p, Phase::Idle);
    }

    #[test]
    fn a_key_repeat_past_the_threshold_starts_the_peek_before_the_timer() {
        let (p, a) = run(&[Ev::Down(Space, 0), Ev::Down(Space, 200), Ev::Timer(205)]);
        assert_eq!(a, vec![Arm, Start, Swallow]);
        assert_eq!(p, Phase::Peeking { key: Space });
    }

    #[test]
    fn enter_stays_and_the_rest_of_the_hold_is_swallowed() {
        let (p, a) = run(&[Ev::Down(Space, 0), Ev::Timer(181), Ev::Enter, Ev::Down(Space, 400), Ev::Timer(600), Ev::Up(Space, 700)]);
        assert_eq!(a, vec![Arm, Start, End { stay: true }, Swallow, Swallow, Swallow]);
        assert_eq!(p, Phase::Idle, "a fresh press after this can peek again");
    }

    #[test]
    fn escape_comes_back_and_other_keys_go_nowhere_while_peeking() {
        let (_, a) = run(&[Ev::Down(Space, 0), Ev::Timer(190), Ev::Other, Ev::Other, Ev::Other, Ev::Escape]);
        assert_eq!(a, vec![Arm, Start, Swallow, Swallow, Swallow, End { stay: false }]);
    }

    #[test]
    fn losing_focus_ends_the_peek_and_the_desktop_can_end_it_too() {
        let (p, a) = run(&[Ev::Down(Space, 0), Ev::Timer(190), Ev::FocusLost]);
        assert_eq!((p, a[2]), (Phase::Idle, End { stay: false }));
        let (p, a) = run(&[Ev::Down(Space, 0), Ev::Timer(190), Ev::Ended, Ev::Up(Space, 500), Ev::Down(Space, 600)]);
        assert_eq!(a, vec![Arm, Start, Swallow, Swallow, Arm]);
        assert_eq!(p, Phase::Pending { key: Space, since: 600 });
    }

    #[test]
    fn another_key_during_the_decision_is_a_tap_then_that_key() {
        let (p, a) = run(&[Ev::Down(Space, 0), Ev::Other]);
        assert_eq!((p, a[1]), (Phase::Idle, TapThenPass(Space)));
    }

    #[test]
    fn a_hold_whose_timer_was_late_is_neither_a_peek_nor_a_tap() {
        let (p, a) = run(&[Ev::Down(Space, 0), Ev::Up(Space, 400)]);
        assert_eq!((p, a[1]), (Phase::Idle, Swallow));
    }

    #[test]
    fn idle_keys_are_not_ours_and_tab_is_never_a_hold() {
        assert_eq!(step(Phase::Idle, Ev::Other), (Phase::Idle, Pass));
        assert_eq!(step(Phase::Idle, Ev::Up(Space, 0)), (Phase::Idle, Pass));
        assert_eq!(hold_key(gdk::Key::Tab, gdk::ModifierType::empty()), None, "Tab is always the next control");
        assert_eq!(hold_key(gdk::Key::space, gdk::ModifierType::CONTROL_MASK), None);
        assert_eq!(hold_key(gdk::Key::space, gdk::ModifierType::empty()), Some(Space));
    }
}
