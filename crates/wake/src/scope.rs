//! The screensaver's agent readout, drawn the same way in Wake's Agents view.
//!
//! Ported to cairo from Slipstream's `slipstream/src/effects/scope.rs` (and the note format in
//! `slipstream/src/working.rs`) at slipstream-desktop commit 5c12763, which is GPL-3.0-or-later
//! like Wake. Constants, wave maths, the dot grid, the backing, the label with the PID in the top
//! right, helper rows with their small waves, the settle and appear timing and reduced motion
//! (standing waves) are kept exactly; what differs is only where it is drawn: one readout per
//! agent row in a `gtk::DrawingArea` instead of a stack down the wallpaper's right edge, so the
//! readout is a fixed width and its trace always the full height (no squeezing to fit a screen).
//!
//! Task rows (the background tasks of agent-notify's `task<TAB><label>` lines) follow the helper
//! rows in the same parts. Their small waves are in Wake's tertiary colour, its lightness moved
//! just enough to stand out 3:1 from the backing (theme.rs `readable_on`), and keep moving while
//! the task runs, even when the agent is idle and its own trace lies flat.
//!
//! The two colours come from Slipstream's settings exactly as the compositor takes them
//! (`set_colours(settings.borders.selected_tile_rgb(), settings.borders.bullet_time_rgb())`): the
//! focused window's ring colour for a wave at work, bullet time's for a line that has stopped, with
//! scope.rs's defaults (`Borders::default()`) when the file or a value is missing or not a colour.

use std::{cell::RefCell, collections::HashMap, path::PathBuf, rc::Rc};

use gtk4::{self as gtk, cairo, pango, prelude::*};

// Sizes in logical pixels, as scope.rs.
/// A readout's width here: scope.rs gives 19% of the screen, held to 120–300; on a common
/// 1536-point-wide screen that is 291, so Wake's column uses 290.
pub const WIDTH: i32 = 290;
const PAD: i32 = 9;
const LABEL: i32 = 15;
const PID_GAP: i32 = 8;
const TRACE: i32 = 44;
const HELPER: i32 = 14;
const HELPER_WAVE: i32 = 24;
const HELPER_GAP: i32 = 6;
const MOST_ROWS: usize = 4;
const DOT: f64 = 2.0;
const PITCH: f64 = 3.0;

const BACKING: u32 = 0x0b0e13b8;
const BACKING_RADIUS: f64 = 7.0;
const NAME: u32 = 0xaab2c0ff;
const HELPER_NAME: u32 = 0x7f8898ff;

const SETTLE: f32 = 3.0;
const APPEAR: f64 = 0.6;
/// Traces are redrawn 30 times a second when a wave is moving.
pub const STEP: f64 = 1.0 / 30.0;
const SWEEP: f64 = 0.6;
const FLAT: f32 = 0.85;
const DIMMEST: f32 = 0.25;
const HELPER_LIT: f32 = 0.7;

/// scope.rs's defaults: `slipstream_config::SELECTED_TILE` and `BULLET_TIME`.
const SELECTED_TILE: &str = "#42d3ff";
const BULLET_TIME: &str = "#ffb547";

/// `#rrggbb` or `rrggbb` as three channels 0–1, as `slipstream_config::rgb`.
fn rgb(text: &str) -> Option<[f32; 3]> {
    let digits = text.trim().strip_prefix('#').unwrap_or(text.trim());
    if digits.len() != 6 || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let channel = |at: usize| u8::from_str_radix(&digits[at..at + 2], 16).ok().map(|v| v as f32 / 255.0);
    Some([channel(0)?, channel(2)?, channel(4)?])
}

/// What the readouts take from Slipstream's settings.
#[derive(Debug, Clone, PartialEq)]
pub struct Look {
    /// Each channel 0–255, as scope.rs keeps them.
    pub working: [f32; 3],
    pub stopped: [f32; 3],
    pub reduced_motion: bool,
}

fn settings_path() -> PathBuf {
    wake_core::store::Paths::from_env().config_home.join("slipstream").join("settings.toml")
}

impl Look {
    /// Read-only look at `~/.config/slipstream/settings.toml`: `[borders] selected-tile` and
    /// `bullet-time`, and `[motion] reduced`. Only those three keys are read, so a tiny line reader
    /// does instead of a TOML parser.
    pub fn from_settings() -> Self {
        Self::parse(&std::fs::read_to_string(settings_path()).unwrap_or_default())
    }

    pub fn parse(text: &str) -> Self {
        let (mut working, mut stopped, mut reduced) = (None, None, false);
        let mut section = String::new();
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                section = line.trim_matches(|c| c == '[' || c == ']').trim().to_string();
                continue;
            }
            let Some((key, value)) = line.split_once('=') else { continue };
            let value = value.trim();
            let value = match value.strip_prefix('"') {
                Some(rest) => rest.split('"').next().unwrap_or(""),
                None => value.split('#').next().unwrap_or("").trim(),
            };
            match (section.as_str(), key.trim()) {
                ("borders", "selected-tile") => working = rgb(value),
                ("borders", "bullet-time") => stopped = rgb(value),
                ("motion", "reduced") => reduced = value == "true",
                _ => {}
            }
        }
        let channels = |c: [f32; 3]| c.map(|v| v * 255.0);
        Self { working: channels(working.or_else(|| rgb(SELECTED_TILE)).unwrap_or_default()), stopped: channels(stopped.or_else(|| rgb(BULLET_TIME)).unwrap_or_default()), reduced_motion: reduced }
    }
}

/// An agent as its note says, the input to `Model::follow` (scope.rs's `working::Agent`).
#[derive(Debug, Clone, PartialEq)]
pub struct Agent {
    pub pid: u32,
    pub name: String,
    pub active: bool,
    pub helpers: Vec<String>,
    /// Its background tasks, by label.
    pub tasks: Vec<String>,
}

/// One agent's readout state.
#[derive(Debug, Clone)]
struct Trace {
    pid: u32,
    name: String,
    helpers: Vec<String>,
    tasks: Vec<String>,
    alive: bool,
    active: bool,
    colour: f32,
    amp: f32,
}

impl Trace {
    fn shape(&self) -> (f32, f32, f32) {
        let hash = self.pid.wrapping_mul(2_654_435_761);
        (2.0 + (hash % 3) as f32, 5.0 + ((hash >> 8) % 4) as f32, ((hash >> 16) & 0xff) as f32 / 255.0 * std::f32::consts::TAU)
    }

    fn wave(&self, across: f32, t: f32) -> f32 {
        let (slow, quick, phase) = self.shape();
        let turn = std::f32::consts::TAU * across;
        0.6 * (turn * slow + t * 1.4 + phase).sin() + 0.4 * (turn * quick - t * 1.9 + phase * 2.0).sin()
    }

    fn helper_wave(&self, nth: usize, across: f32, t: f32) -> f32 {
        self.wave(0.35 * across + 0.29 * nth as f32, 1.3 * t + 1.7 * nth as f32)
    }
}

/// What a row under a readout is: a helper (subagent) or a task, each with its small wave, or the
/// last row saying how many more there are.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RowKind {
    Helper,
    Task,
    More,
}

/// The rows listed under a readout: the helpers, then the tasks; past MOST_ROWS (both counted,
/// helpers first) the last says how many more.
pub fn rows(helpers: &[String], tasks: &[String]) -> Vec<(String, RowKind)> {
    let all: Vec<(String, RowKind)> = helpers.iter().map(|h| (h.clone(), RowKind::Helper)).chain(tasks.iter().map(|t| (t.clone(), RowKind::Task))).collect();
    if all.len() <= MOST_ROWS {
        return all;
    }
    let mut rows: Vec<_> = all[..MOST_ROWS - 1].to_vec();
    rows.push((format!("+{} more", all.len() - rows.len()), RowKind::More));
    rows
}

/// How tall the readout listing `rows` helper rows is.
pub fn height(rows: usize) -> i32 {
    2 * PAD + LABEL + TRACE + rows as i32 * HELPER
}

/// Every readout's state, shared by the Agents view's drawing areas and its frame clock.
pub struct Model {
    traces: HashMap<u32, Trace>,
    pub look: Look,
    /// How far in the readouts are, 0 to 1: they come in over APPEAR when the view is shown.
    shown: f32,
    last: Option<f64>,
    /// Frame time of the last paint, for the 30-a-second limit.
    pub painted_at: f64,
    /// The time the waves are drawn at.
    now: f64,
}

pub type Shared = Rc<RefCell<Model>>;

impl Model {
    pub fn new(look: Look) -> Shared {
        Rc::new(RefCell::new(Self { traces: HashMap::new(), look, shown: 0.0, last: None, painted_at: f64::NEG_INFINITY, now: 0.0 }))
    }

    /// As scope.rs's `follow`: agents with notes now, plus pids whose note outlived the process.
    /// A new one starts at its target; a known one keeps its name from before going idle. Traces
    /// with no row any more are dropped (Wake has no "clear on touch": the row has gone).
    pub fn follow(&mut self, at_work: &[Agent], gone: &[u32]) {
        for trace in self.traces.values_mut() {
            match at_work.iter().find(|a| a.pid == trace.pid) {
                Some(agent) => {
                    trace.alive = true;
                    trace.active = agent.active;
                    trace.helpers.clone_from(&agent.helpers);
                    trace.tasks.clone_from(&agent.tasks);
                    if agent.active || trace.name.is_empty() {
                        trace.name.clone_from(&agent.name);
                    }
                }
                None => {
                    trace.alive = false;
                    trace.helpers.clear();
                    trace.tasks.clear();
                }
            }
        }
        for agent in at_work {
            self.traces.entry(agent.pid).or_insert_with(|| Trace {
                pid: agent.pid,
                name: agent.name.clone(),
                helpers: agent.helpers.clone(),
                tasks: agent.tasks.clone(),
                alive: true,
                active: agent.active,
                colour: if agent.active { 1.0 } else { 0.0 },
                amp: if agent.active { 1.0 } else { 0.0 },
            });
        }
        for &pid in gone {
            self.traces.entry(pid).or_insert_with(|| Trace { pid, name: String::new(), helpers: Vec::new(), tasks: Vec::new(), alive: false, active: false, colour: 0.0, amp: 0.0 });
        }
        self.traces.retain(|pid, _| at_work.iter().any(|a| a.pid == *pid) || gone.contains(pid));
    }

    /// For a screenshot: the readouts fully in, as they are once the view has been up a moment.
    pub fn appeared(&mut self, now: f64) {
        self.shown = 1.0;
        self.now = now;
        self.last = Some(now);
    }

    /// The view was hidden: next time it shows, the readouts come in again.
    pub fn hidden(&mut self) {
        self.shown = 0.0;
        self.last = None;
    }

    /// Moves everything on to `now` (seconds) with the view visible, as scope.rs's `advance`.
    /// Says whether anything is still moving, so the frame clock can rest when nothing is.
    pub fn advance(&mut self, now: f64) -> bool {
        let dt = self.last.map_or(0.0, |last| (now - last).clamp(0.0, 0.1));
        self.last = Some(now);
        self.now = now;
        let reduced = self.look.reduced_motion;
        self.shown = if reduced { 1.0 } else { (self.shown + (dt / APPEAR) as f32).min(1.0) };
        let mut settling = false;
        for trace in self.traces.values_mut() {
            let target = if trace.alive && trace.active { 1.0 } else { 0.0 };
            if reduced {
                trace.colour = target;
                trace.amp = target;
            } else {
                trace.colour += (target - trace.colour) * (dt as f32 * SETTLE).min(1.0);
                if (target - trace.colour).abs() < 0.002 {
                    trace.colour = target;
                }
                trace.amp += (target - trace.amp) * (dt as f32 * SETTLE).min(1.0);
                if (target - trace.amp).abs() < 0.002 {
                    trace.amp = target;
                }
            }
            settling |= trace.colour != target || trace.amp != target;
        }
        let waving = !reduced && self.traces.values().any(|t| t.amp > 0.0 || !t.tasks.is_empty());
        waving || settling || self.shown < 1.0
    }
}

/// The readout as Wake's Agents shows it: the screensaver's whole readout (backing, name, PID,
/// trace, helper rows) at seven tenths of its size, so it sits in a row.
pub const COMPACT_W: i32 = WIDTH * 7 / 10;
const COMPACT: f64 = 0.7;

/// How tall the smaller readout listing `rows` helper rows is.
pub fn compact_height(rows: usize) -> i32 {
    (f64::from(height(rows)) * COMPACT).ceil() as i32
}

pub fn compact(model: &Shared, pid: u32, rows: usize, label: &str) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::builder().content_width(COMPACT_W).content_height(compact_height(rows)).accessible_role(gtk::AccessibleRole::Img).build();
    area.update_property(&[gtk::accessible::Property::Label(label)]);
    area.set_valign(gtk::Align::Center);
    area.add_css_class("readout");
    let model = model.clone();
    area.set_draw_func(move |area, cr, _, _| {
        let scale = area.native().and_then(|n| n.surface()).map_or(area.scale_factor() as f64, |s| s.scale());
        let Ok(model) = model.try_borrow() else { return };
        if let Some(trace) = model.traces.get(&pid) {
            // The same drawing at its own size, scaled down (and snapped at the scaled pixel).
            cr.scale(COMPACT, COMPACT);
            let _ = draw(cr, area, &model, trace, (WIDTH, height(rows)), scale * COMPACT);
        }
    });
    area
}

/// The colour task rows' waves are drawn in: the tertiary colour, made to stand out 3:1 from the
/// backing as it shows over a row of the theme now showing.
fn task_colour() -> String {
    let backing = format!("#{:06X}", BACKING >> 8);
    let ground = crate::theme::over(&backing, f64::from(BACKING & 0xff) / 255.0, crate::theme::hex("panel"));
    crate::theme::readable_on(&crate::theme::tertiary_hex(), &ground, 3.0)
}

fn set_hex(cr: &cairo::Context, colour: u32) {
    let c = |shift: u32| ((colour >> shift) & 0xff) as f64 / 255.0;
    cr.set_source_rgba(c(24), c(16), c(8), c(0));
}

fn font(px: f64) -> pango::FontDescription {
    let mut d = pango::FontDescription::from_string("JetBrains Mono");
    d.set_absolute_size(px * pango::SCALE as f64);
    d
}

/// Text centred on the line at `centre`, as Slipstream's `Painter::text`: the baseline sits at
/// `centre + (ascent - descent) / 2` of the font at that size. Returns the layout's width.
fn text(cr: &cairo::Context, layout: &pango::Layout, x: f64, centre: f64, colour: u32) {
    let ctx = layout.context();
    let metrics = ctx.metrics(layout.font_description().as_ref(), None);
    let (ascent, descent) = (metrics.ascent() as f64 / pango::SCALE as f64, metrics.descent() as f64 / pango::SCALE as f64);
    let baseline = centre + (ascent - descent) / 2.0;
    cr.move_to(x, baseline - layout.baseline() as f64 / pango::SCALE as f64);
    set_hex(cr, colour);
    pangocairo::functions::show_layout(cr, layout);
}

fn draw(cr: &cairo::Context, area: &gtk::DrawingArea, model: &Model, trace: &Trace, (w, h): (i32, i32), scale: f64) -> Result<(), cairo::Error> {
    let reduced = model.look.reduced_motion;
    let shown = model.shown as f64;
    if shown <= 0.0 {
        return Ok(());
    }
    cr.push_group();
    let (width, height) = (w as f64, h as f64);
    // The backing.
    rounded(cr, 0.0, 0.0, width, height, BACKING_RADIUS);
    set_hex(cr, BACKING);
    cr.fill()?;
    // The name and the PID in the top right.
    let pid_layout = area.create_pango_layout(Some(&format!("PID: {}", trace.pid)));
    pid_layout.set_font_description(Some(&font(10.0)));
    let pid_width = pid_layout.pixel_size().0 as f64;
    let line = PAD as f64 + LABEL as f64 / 2.0 - 1.0;
    text(cr, &pid_layout, width - PAD as f64 - pid_width, line, HELPER_NAME);
    let room = width - 2.0 * PAD as f64 - pid_width - PID_GAP as f64;
    let name = area.create_pango_layout(Some(&trace.name));
    name.set_font_description(Some(&font(11.0)));
    name.set_width((room.max(0.0) * pango::SCALE as f64) as i32);
    name.set_ellipsize(pango::EllipsizeMode::End);
    name.set_single_paragraph_mode(true);
    text(cr, &name, PAD as f64, line, NAME);
    // Helper rows.
    let left = (PAD + HELPER_WAVE + HELPER_GAP) as f64;
    let first = (PAD + LABEL + TRACE) as f64;
    let shown_rows = rows(&trace.helpers, &trace.tasks);
    for (i, (row, _)) in shown_rows.iter().enumerate() {
        let l = area.create_pango_layout(Some(row));
        l.set_font_description(Some(&font(10.0)));
        l.set_width(((width - left - PAD as f64).max(0.0) * pango::SCALE as f64) as i32);
        l.set_ellipsize(pango::EllipsizeMode::End);
        l.set_single_paragraph_mode(true);
        text(cr, &l, left, first + (i as i32 * HELPER) as f64 + HELPER as f64 / 2.0 - 1.0, HELPER_NAME);
    }
    // The dots, in device pixels as scope.rs's `plot`, each replacing what is under it.
    cr.save()?;
    cr.scale(1.0 / scale, 1.0 / scale);
    cr.set_operator(cairo::Operator::Source);
    let px = |logical: i32| (logical as f64 * scale).round() as i32;
    let pitch = (PITCH * scale).round().max(2.0) as i32;
    let dot = (DOT * scale).round().max(1.0) as i32;
    let device_w = (width * scale).round() as i32;
    let (l, r) = (px(PAD), device_w - px(PAD) - dot);
    let t = (!reduced).then_some(model.now);
    let time = t.unwrap_or(0.0);
    let (working, stopped) = (model.look.working, model.look.stopped);
    let rgb: [f32; 3] = std::array::from_fn(|c| stopped[c] + (working[c] - stopped[c]) * trace.colour);
    let spot = (time * SWEEP + trace.pid as f64 * 0.37).fract() as f32;
    let trace_top = PAD + LABEL;
    let pen = Pen { pitch, dot, rgb };
    // Task rows' small waves: the tertiary colour, made to stand out 3:1 from the backing as it
    // shows over the row (WCAG's ask of marks that aren't text), whatever the trace is doing.
    let task_pen = Pen { pitch, dot, rgb: crate::theme::colour_rgb(&task_colour()).map(|c| c as f32 * 255.0) };
    pen.line(cr, (l, r), (px(trace_top) + px(TRACE) / 2, px(TRACE)), |across| {
        let lift = trace.wave(across, time as f32) * trace.amp;
        let lit = match t {
            Some(_) => {
                let behind = (spot - across).rem_euclid(1.0);
                DIMMEST + (1.0 - DIMMEST) * (-behind * 3.5).exp()
            }
            None => 0.85,
        };
        (lift, FLAT + (lit - FLAT) * trace.amp)
    });
    let first = trace_top + TRACE;
    let wave_right = l + px(HELPER_WAVE) - dot;
    for (i, (_, kind)) in shown_rows.iter().enumerate() {
        // A helper's wave lies flat with its agent's trace; a task's moves while the task runs, and
        // is drawn solid so it shows in exactly the colour that was made to stand out.
        let (pen, amp, lit) = match kind {
            RowKind::Helper => (&pen, trace.amp, HELPER_LIT),
            RowKind::Task => (&task_pen, 1.0, 1.0),
            RowKind::More => continue,
        };
        pen.line(cr, (l, wave_right), (px(first + i as i32 * HELPER) + px(HELPER) / 2, px(HELPER)), |across| (trace.helper_wave(i, across, time as f32) * amp, lit));
    }
    cr.restore()?;
    cr.pop_group_to_source()?;
    cr.paint_with_alpha(shown)?;
    Ok(())
}

fn rounded(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    use std::f64::consts::PI;
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -PI / 2.0, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, PI / 2.0);
    cr.arc(x + r, y + h - r, r, PI / 2.0, PI);
    cr.arc(x + r, y + r, r, PI, 1.5 * PI);
    cr.close_path();
}

struct Pen {
    pitch: i32,
    dot: i32,
    rgb: [f32; 3],
}

impl Pen {
    /// scope.rs's `Pen::line`: dots across `from`..`to` about `middle` in a band `band` high, the
    /// climb from the last column filled so a steep wave stays one line.
    fn line(&self, cr: &cairo::Context, (from, to): (i32, i32), (middle, band): (i32, i32), shape: impl Fn(f32) -> (f32, f32)) {
        let Self { pitch, dot, rgb } = *self;
        let span = (to - from).max(1) as f32;
        let swing = (band / 2 - pitch).max(0) as f32;
        let mut before = None;
        let mut x = from;
        while x <= to {
            let (lift, alpha) = shape((x - from) as f32 / span);
            let y = middle + (lift * swing / pitch as f32).round() as i32 * pitch;
            let last = before.unwrap_or(y);
            let mut at = last.min(y);
            while at <= last.max(y) {
                put(cr, (x, at), dot, rgb, alpha);
                at += pitch;
            }
            before = Some(y);
            x += pitch;
        }
    }
}

/// One square dot in place of what was there (the operator is Source), as scope.rs's `put`.
fn put(cr: &cairo::Context, (x, y): (i32, i32), dot: i32, rgb: [f32; 3], alpha: f32) {
    let alpha = alpha.clamp(0.0, 1.0) as f64;
    cr.set_source_rgba(rgb[0] as f64 / 255.0, rgb[1] as f64 / 255.0, rgb[2] as f64 / 255.0, alpha);
    cr.rectangle(x as f64, y as f64, dot as f64, dot as f64);
    let _ = cr.fill();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_come_from_the_borders_section_with_scope_s_defaults() {
        let look = Look::parse("[borders]\nselected-tile = \"#b794ff\"\nbullet-time = \"nope\"\n[motion]\nreduced = true\n");
        assert_eq!(look.working, [0xb7 as f32, 0x94 as f32, 255.0]);
        assert_eq!(look.stopped, rgb(BULLET_TIME).unwrap().map(|c| c * 255.0));
        assert!(look.reduced_motion);
        let none = Look::parse("");
        assert_eq!(none.working, rgb(SELECTED_TILE).unwrap().map(|c| c * 255.0));
        assert!(!none.reduced_motion);
    }

    #[test]
    fn past_the_room_for_rows_the_last_says_how_many_more() {
        let many: Vec<String> = ["a", "b", "c", "d", "e", "f"].iter().map(|s| s.to_string()).collect();
        let r = rows(&many, &[]);
        assert_eq!(r.len(), MOST_ROWS);
        assert_eq!(r[MOST_ROWS - 1], ("+3 more".to_string(), RowKind::More));
    }

    #[test]
    fn task_rows_follow_the_helpers_and_count_toward_the_room() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let kinds = |r: &[(String, RowKind)]| r.iter().map(|x| x.1).collect::<Vec<_>>();
        let r = rows(&s(&["review"]), &s(&["cargo test", "CI run 42"]));
        assert_eq!(r.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), ["review", "cargo test", "CI run 42"]);
        assert_eq!(kinds(&r), [RowKind::Helper, RowKind::Task, RowKind::Task]);
        // Helpers first: two helpers and three tasks fill the room with one task and "+2 more".
        let r = rows(&s(&["a", "b"]), &s(&["t1", "t2", "t3"]));
        assert_eq!(kinds(&r), [RowKind::Helper, RowKind::Helper, RowKind::Task, RowKind::More]);
        assert_eq!(r[3].0, "+2 more");
        assert_eq!(rows(&[], &s(&["t"])), [("t".to_string(), RowKind::Task)]);
    }

    #[test]
    fn tasks_never_change_idle_and_go_with_their_agent() {
        let model = Model::new(Look { reduced_motion: true, ..Look::parse("") });
        let mut m = model.borrow_mut();
        m.follow(&[Agent { pid: 300, name: "kiln".into(), active: false, helpers: vec![], tasks: vec!["cargo build".into()] }], &[]);
        m.advance(0.0);
        assert_eq!((m.traces[&300].amp, m.traces[&300].tasks.len()), (0.0, 1), "idle stays flat with a task running");
        assert!(!m.advance(1.0), "reduced motion: nothing moves, tasks or not");
        drop(m);
        let moving = Model::new(Look::parse(""));
        let mut m = moving.borrow_mut();
        m.follow(&[Agent { pid: 301, name: "kiln".into(), active: false, helpers: vec![], tasks: vec!["cargo build".into()] }], &[]);
        m.appeared(0.0);
        assert!(m.advance(0.05), "a running task's wave keeps moving while its agent is idle");
        assert_eq!(m.traces[&301].amp, 0.0, "its own trace stays flat");
        m.follow(&[Agent { pid: 301, name: "kiln".into(), active: false, helpers: vec![], tasks: vec![] }], &[]);
        assert!(!m.advance(0.1), "with the task done, nothing moves");
        drop(m);
        let mut m = model.borrow_mut();
        m.follow(&[], &[300]);
        assert!(m.traces[&300].tasks.is_empty(), "a gone agent lists no tasks");
    }

    #[test]
    fn idle_settles_flat_and_reduced_motion_snaps() {
        let model = Model::new(Look { reduced_motion: true, ..Look::parse("") });
        let mut m = model.borrow_mut();
        m.follow(&[Agent { pid: 300, name: "kiln".into(), active: true, helpers: vec![], tasks: vec![] }], &[]);
        m.follow(&[Agent { pid: 300, name: "done".into(), active: false, helpers: vec![], tasks: vec![] }], &[]);
        m.advance(0.0);
        assert_eq!(m.traces[&300].amp, 0.0);
        assert_eq!(m.traces[&300].name, "kiln", "the name stays from before idle");
        assert_eq!(m.shown, 1.0);
    }
}
