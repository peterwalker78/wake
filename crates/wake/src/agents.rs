//! The Agents view: which PID is which task, what its subagents are doing, and stopping the stale
//! ones.
//!
//! One row per agent process, grouped by what to do about it (stale first, then sessions by state,
//! then Claude's background processes, then what Wake can't place), each group a box. A working
//! agent's row carries the screensaver's whole readout (`scope.rs`) at seven tenths of its size:
//! its name, PID, trace and helper rows. A row with subagents opens (→, or its count) to list
//! them, as the switcher does, and each opens again for its steps. Rows keep their order within a
//! group (as they started), and memory, CPU and idle time change in place, so nothing moves while
//! you read.
//!
//! Every stop goes through `Source::stop` with the process's start time, after a confirmation
//! naming the task, where Cancel has the keyboard. Force only after a polite stop has had five
//! seconds, and only with a second confirmation.

use std::time::Duration;

use gtk4::{self as gtk, glib, prelude::*};
use wake_core::*;

use crate::{
    app::{self, Ui, View},
    dialogs, fmt, panel, prefs, scope,
    sort::{self, Sort},
    views::{self, live_text},
    w,
};

type Col = (&'static str, i32, bool, bool);

/// The columns: the readout, PID, task, state, memory, where, why stale, stop.
/// Which columns make way: with the side panel open, the wave, Where and Why stale; and Where or
/// Why stale whenever no row has anything to say in it.
fn hidden_cols(compact: bool, tight: bool, any_where: bool, any_stale: bool) -> u8 {
    let mut m = 0u8;
    if compact {
        m |= 1 | 1 << 5 | 1 << 6;
    }
    // A list short of the full table's width keeps the waves and lets Where and Why stale go (the
    // stale rows still show it: their Stop is red, and the reason is in the details).
    if tight {
        m |= 1 << 5 | 1 << 6;
    }
    // Narrower still with its readouts, the PID goes too (it is in the details, and the tooltip).
    if tight && !compact {
        m |= 1 << 1;
    }
    if !any_where {
        m |= 1 << 5;
    }
    if !any_stale {
        m |= 1 << 6;
    }
    m
}

/// Which columns sort, lined up with `cols`: PID, task, state, memory.
fn sort_keys(hide: u8) -> Vec<Option<&'static str>> {
    let all = [None, Some("pid"), Some("task"), Some("state"), Some("memory"), None, None, None];
    all.into_iter().enumerate().filter(|(i, _)| hide & (1 << i) == 0).map(|(_, k)| k).collect()
}

fn cols(hide: u8) -> Vec<Col> {
    // Compact (the waves gone), the titles are one word each, so they fit their narrower columns.
    let short = hide & 1 != 0;
    let all: [Col; 8] = [
        ("", scope::COMPACT_W | w::FIXED, false, false),
        ("PID", 64, false, false),
        (if short { "Task" } else { "Task · project" }, 180, true, false),
        (if short { "State" } else { "State · idle" }, 126, false, false),
        (if short { "Mem" } else { "Memory" }, 84, false, false),
        ("Where", 110, false, false),
        ("Why stale", 140, false, false),
        ("", 56, false, false),
    ];
    all.into_iter().enumerate().filter(|(i, _)| hide & (1 << i) == 0).map(|(_, c)| c).collect()
}

fn kind_words(k: ProcKind) -> &'static str {
    match k {
        ProcKind::Session => "session",
        ProcKind::Daemon => "Claude's background daemon",
        ProcKind::PtyHost => "terminal host for a background session",
        ProcKind::Spare => "spare session, not used yet",
        ProcKind::Stray => "agent process with no session",
        ProcKind::Subagent => "subagent of the session above",
    }
}

/// Stale, and something Wake would offer to stop with the others: never the daemon, never a stray
/// (a stray is only ever stopped on its own, with its command shown).
pub fn is_stale(a: &AgentProc) -> bool {
    a.stale_reason.is_some() && !matches!(a.kind, ProcKind::Daemon | ProcKind::Stray | ProcKind::Subagent)
}

fn folded(p: &ProcOverview) -> Vec<u32> {
    p.procs.iter().flat_map(|a| a.children.iter().copied()).collect()
}

/// While Wake is still finding which window each session is in, an agent in a window could look
/// stale (idle, no window): nothing is called stale, or offered to stop as such, until it knows.
pub fn hold_stale(p: &mut ProcOverview, unsure: bool) {
    if unsure {
        p.procs.iter_mut().for_each(|a| a.stale_reason = None);
    }
}

/// The stale rows, as "Looks stale" lists them and "Stop all stale" stops them.
pub fn stale_rows(p: &ProcOverview) -> Vec<&AgentProc> {
    let folded = folded(p);
    p.procs.iter().filter(|a| is_stale(a) && !folded.contains(&a.pid)).collect()
}

/// How many look stale and the memory they hold (their folded children included).
pub fn stale_summary(p: &ProcOverview) -> (u32, u64) {
    let rows = stale_rows(p);
    (rows.len() as u32, rows.iter().map(|a| a.rss_bytes).sum())
}

/// How many rows the view shows: the chips' counts are exactly these.
pub fn row_count(p: &ProcOverview, stale_only: bool) -> usize {
    ordered(p, stale_only).len() + if stale_only { 0 } else { p.dead_notes.len() }
}

/// Points the waves at the agents that have a note now (Monitor draws them too).
pub fn follow_notes(ui: &Ui) {
    let st = ui.st.borrow();
    if let Some(p) = &st.procs {
        ui.scope.borrow_mut().follow(&followers(p), &p.dead_notes);
    }
}

/// The agents that have a note, as the readouts follow them.
fn followers(p: &ProcOverview) -> Vec<scope::Agent> {
    p.procs
        .iter()
        .filter_map(|a| {
            let n = a.note.as_ref()?;
            let name = if n.name.is_empty() { a.project_name.clone().or(a.title.clone()).unwrap_or_default() } else { n.name.clone() };
            Some(scope::Agent { pid: a.pid, name, active: !n.idle, helpers: n.helpers.clone() })
        })
        .collect()
}

/// Rows in tree order, top-level only (folded children are left out, except subagent processes,
/// which follow their session in its group, as they started): (group, proc). Within a group: in
/// use first, then stale, each by when it started. Nothing that changes by the second (memory,
/// CPU) decides the order, so rows never swap under the pointer.
pub fn ordered(p: &ProcOverview, stale_only: bool) -> Vec<(&'static str, &AgentProc)> {
    ordered_by(p, stale_only, &None)
}

/// As `ordered`, with each group sorted by a column (the groups and the subagents under their
/// session stay as they are).
pub fn ordered_by<'a>(p: &'a ProcOverview, stale_only: bool, by: &Sort) -> Vec<(&'static str, &'a AgentProc)> {
    let folded = folded(p);
    let top: Vec<&AgentProc> = p.procs.iter().filter(|a| !folded.contains(&a.pid)).filter(|a| !stale_only || is_stale(a)).collect();
    let mut out = Vec::new();
    // What to act on first: what looks stale, then sessions by what they are doing, then Claude's
    // own background processes, then what Wake can't place. Within a group, as they started (memory
    // never reorders them, so nothing moves under the pointer).
    for group in GROUPS {
        let mut these: Vec<&AgentProc> = top.iter().copied().filter(|a| group_of(a) == group).collect();
        these.sort_by_key(|a| (a.start_ticks.unwrap_or(u64::MAX), a.pid));
        if let Some((k, rev)) = by {
            sort::agents(&mut these, k, *rev);
        }
        for a in these {
            out.push((group, a));
            if stale_only {
                continue; // a subagent is never stale on its own
            }
            let mut subs: Vec<&AgentProc> = p.procs.iter().filter(|s| s.kind == ProcKind::Subagent && a.children.contains(&s.pid)).collect();
            subs.sort_by_key(|s| (s.start_ticks.unwrap_or(u64::MAX), s.pid));
            out.extend(subs.into_iter().map(|s| (group, s)));
        }
    }
    out
}

/// The groups, in the order they are shown.
const GROUPS: [&str; 6] = ["Look stale", "Working", "Waiting for you", "Idle", "In the background", "Not matched"];

/// Which group an agent process is in (subagents go under their session, never in a group).
fn group_of(a: &AgentProc) -> &'static str {
    if is_stale(a) {
        return "Look stale";
    }
    match (a.kind, a.state) {
        (ProcKind::Session, State::Working) => "Working",
        (ProcKind::Session, State::Waiting) => "Waiting for you",
        (ProcKind::Session, _) => "Idle",
        (ProcKind::Stray, _) => "Not matched",
        _ => "In the background",
    }
}

/// The session behind a row, from the overview: its subagent counts.
fn session_of<'a>(st: &'a app::AppState, a: &AgentProc) -> Option<&'a SessionSummary> {
    let id = a.session_id.as_deref()?;
    st.ov.as_ref()?.sessions.iter().find(|s| s.id == id)
}

/// What, when it changes, is worth drawing the rows again for: never memory, CPU or idle time.
fn signature(st: &app::AppState, p: &ProcOverview) -> String {
    let rows: Vec<String> = p
        .procs
        .iter()
        .map(|a| {
            let subs = session_of(st, a).map(|s| (s.subagents.total, s.subagents.running));
            let key = (a.pid, a.ppid, a.kind, a.state, &a.title, &a.project_name, &a.location, &a.job_id, &a.stale_reason, &a.children, &a.note, &a.session_id, a.state == State::Working, subs);
            serde_json::to_string(&key).unwrap_or_default()
        })
        .collect();
    // Which rows are open, and their subagents, are not here: they open and update in place.
    let low = st.mem_available.map(|m| m < 3 << 30);
    // Sorted by memory, the order follows memory: then memory (to the nearest 100 MB) redraws too.
    let by = prefs::sort_of(View::Agents);
    let mem: Vec<u64> = if by.as_ref().is_some_and(|(k, _)| k == "memory") { p.procs.iter().map(|a| a.rss_bytes / (100 << 20)).collect() } else { vec![] };
    format!("{}{}{low:?}{:?}{}{rows:?}{by:?}{mem:?}{}", st.stale_only, app::compact(st), p.dead_notes, st.lay.tight, st.show_background)
}

pub fn render(ui: &Ui) {
    let (procs, sig, stale_only, compact) = {
        let st = ui.st.borrow();
        let Some(p) = st.procs.clone() else {
            drop(st);
            let vw = &ui.w.views[&View::Agents];
            w::clear(&vw.empty);
            vw.empty.append(&w::label("Looking at this machine's agent processes…", &["loading"]));
            return;
        };
        let sig = signature(&st, &p);
        // Every agent shows: the stale ones are the first group (the old "Looks stale" filter is gone).
        // Phone-narrow, the readouts make way as they do beside the details.
        (p, sig, false, app::compact(&st) || st.lay.push)
    };
    let p = procs;
    if ui.st.borrow().agents_folding {
        return;
    }
    // Stopped agents whose process has now gone fold away where they are, and the list is redrawn
    // once they have, so nothing jumps.
    let gone: Vec<u32> = ui.st.borrow().agent_leaving.iter().copied().filter(|pid| !p.procs.iter().any(|a| a.pid == *pid)).collect();
    if !gone.is_empty() && fold_away(ui, &gone) {
        return;
    }
    ui.st.borrow_mut().agent_leaving.retain(|pid| !gone.contains(pid));
    ui.scope.borrow_mut().follow(&followers(&p), &p.dead_notes);
    let Some(had_focus) = views::begin(ui, View::Agents, sig) else {
        app::tick_live(ui);
        ensure_ticking(ui);
        return;
    };
    let vw = &ui.w.views[&View::Agents];
    ui.readouts.borrow_mut().clear();
    // The top line: what agents hold, and how much of it looks stale (kept current in place).
    w::clear(&vw.top);
    // The page title, as every screen's: what agents hold, then how much looks stale and what is free.
    let line = w::vbox(4, &["pagetitle", "agentstitle"]);
    let total = w::wrap(&format!("Agents are using {}", fmt::bytes(p.total_rss_bytes)), &["title-1"]);
    total.set_xalign(0.0);
    live_text(ui, View::Agents, &total, |st| st.procs.as_ref().map(|p| format!("Agents are using {}", fmt::bytes(p.total_rss_bytes))));
    line.append(&total);
    let sub_words = |st: &app::AppState| {
        let p = st.procs.as_ref()?;
        let (n, b) = stale_summary(p);
        let stale =
            if n == 0 { "Nothing looks stale".to_string() } else { format!("{n} {} stale and {} {}", if n == 1 { "looks" } else { "look" }, if n == 1 { "holds" } else { "hold" }, fmt::bytes(b)) };
        Some(match st.mem_available {
            Some(m) => format!("{stale} · {} of memory free on this machine", fmt::bytes(m)),
            None => stale,
        })
    };
    let sub = w::wrap(&sub_words(&ui.st.borrow()).unwrap_or_default(), &["dim"]);
    sub.set_xalign(0.0);
    if stale_summary(&p).0 > 0 || ui.st.borrow().mem_available.is_some_and(|m| m < 3 << 30) {
        sub.add_css_class("warnline");
    }
    sub.set_tooltip_text(Some("Stale: idle for hours with no window, or a spare nobody used. Stopping frees the memory; the conversation is kept."));
    live_text(ui, View::Agents, &sub, sub_words);
    line.append(&sub);
    // The title line: the title, and at its end the page's two actions.
    let head = w::hbox(8, &[]);
    line.set_hexpand(true);
    head.append(&line);
    let acts = w::hbox(8, &["agentacts"]);
    acts.set_valign(gtk::Align::Center);
    acts.append(&crate::peek::chip());
    let n_stale = stale_summary(&p).0;
    if n_stale > 0 {
        let ui2 = ui.clone();
        acts.append(&w::button(&format!("Stop all stale ({n_stale})"), None, &["danger"], move || stop_all_stale(&ui2)));
    }
    head.append(&acts);
    vw.top.append(&head);
    let hide = hidden_cols(compact, ui.st.borrow().lay.tight, p.procs.iter().any(|a| where_words(a).is_some()), p.procs.iter().any(|a| a.stale_reason.is_some()) || !p.dead_notes.is_empty());
    let cols = cols(hide);
    vw.head.append(&views::sort_head(ui, View::Agents, &cols, &sort_keys(hide)));

    let rows = ordered_by(&p, stale_only, &prefs::sort_of(View::Agents));
    if rows.is_empty() && (stale_only || p.dead_notes.is_empty()) {
        vw.empty.append(&w::empty(
            if stale_only { "Nothing looks stale." } else { "No agent processes are running." },
            if stale_only { "Every agent is in use or recently used." } else { "Start Claude, Codex or Gemini and it appears here." },
        ));
    }
    let daemon = p.procs.iter().find(|a| a.kind == ProcKind::Daemon).map(|a| a.pid);
    // Each group's count, of its top-level rows (subagents sit under their session).
    let rows_in: Vec<&str> = rows.iter().filter(|(_, a)| a.kind != ProcKind::Subagent).map(|(g, _)| *g).collect();
    // Claude's own background processes are hidden until asked for: their heading says how many.
    let show_bg = ui.st.borrow().show_background;
    let mut last_group = "";
    // Whether a group's box is open (a folded group has none to close).
    let mut boxed = false;
    for (group, a) in rows {
        let hidden = group == "In the background" && !show_bg;
        if hidden && group == last_group {
            continue;
        }
        if group != last_group {
            // Each group is a box: its heading above, its rows inside, rounded top and bottom.
            if boxed {
                let bottom = box_edge(false);
                bottom.add_css_class(&group_class(last_group));
                vw.list.append(&bottom);
                boxed = false;
            }
            let n = rows_in.iter().filter(|g| **g == group).count();
            let head = views::group_row(group, n, group_words(group).filter(|_| !hidden));
            head.add_css_class(&group_class(group));
            if group == "In the background" {
                head.add_css_class("bghead");
                if let Some(line) = head.child().and_then(|b| b.first_child()).and_downcast::<gtk::Box>() {
                    line.append(&w::spacer());
                    let ui2 = ui.clone();
                    let b = w::button(if hidden { "Show them" } else { "Hide them" }, None, &["sm"], move || {
                        let on = !ui2.st.borrow().show_background;
                        ui2.st.borrow_mut().show_background = on;
                        render(&ui2);
                    });
                    b.set_tooltip_text(Some("Claude's daemon, terminal hosts and unused spare sessions: they run on their own, and Wake never stops the daemon."));
                    line.append(&b);
                }
            }
            vw.list.append(&head);
            last_group = group;
            if hidden {
                // No box: the group is folded away.
                continue;
            }
            let top = box_edge(true);
            top.add_css_class(&group_class(group));
            vw.list.append(&top);
            boxed = true;
        }
        let row = proc_row(ui, a, daemon, &p, &cols, hide);
        row.add_css_class(&group_class(group));
        vw.list.append(&row);
        // Its open subagents, each a row of its own under it.
        for want in wanted(ui, a) {
            vw.list.append(&build(ui, a, &want, &cols, hide));
        }
    }
    if boxed {
        let bottom = box_edge(false);
        bottom.add_css_class(&group_class(last_group));
        vw.list.append(&bottom);
    }
    if !stale_only && !p.dead_notes.is_empty() {
        vw.list.append(&views::group_row("Notes whose process has gone", p.dead_notes.len(), None));
        vw.list.append(&box_edge(true));
        for &pid in &p.dead_notes {
            vw.list.append(&dead_row(ui, pid, &cols, hide));
        }
        vw.list.append(&box_edge(false));
    }
    views::finish(ui, View::Agents, had_focus);
    // A selected subagent whose row has gone with the redraw: its session instead.
    let sel = ui.st.borrow().sel.clone();
    if let Some(sel) = sel
        && !matches!(row_key(&sel), RowKey::Session(_) | RowKey::Other)
        && find_named_row(ui, &sel).is_none()
        && let Some((row, _)) = session_pid(&sel).and_then(|p| find_row(ui, p))
    {
        ui.st.borrow_mut().sel = Some(row.widget_name().to_string());
        vw.list.select_row(Some(&row));
        if had_focus {
            row.grab_focus();
        }
    }
    app::tick_live(ui);
    ensure_ticking(ui);
}

/// The rounded top or bottom of a group's box: a thin row that is never selected.
fn box_edge(top: bool) -> gtk::ListBoxRow {
    let r = gtk::ListBoxRow::new();
    r.set_selectable(false);
    r.set_activatable(false);
    r.set_focusable(false);
    r.add_css_class("grouprow");
    r.add_css_class("boxedge");
    r.add_css_class(if top { "edgetop" } else { "edgebottom" });
    r.set_child(Some(&w::hbox(0, &[])));
    r
}

/// What a group of agent processes is, under its heading, for someone who hasn't met it before.
fn group_words(group: &str) -> Option<&'static str> {
    match group {
        "Look stale" => Some("Idle for hours with nothing attached, or a spare nobody used. Stopping one frees its memory; the conversation is kept."),
        "In the background" => Some("Claude's daemon (it runs your background sessions; Wake never stops it), terminal hosts and spare sessions."),
        "Not matched" => Some("Agent processes Wake can't match to a session or window. Check before stopping."),
        _ => None,
    }
}

fn wave_cell(ui: &Ui, a: &AgentProc) -> gtk::Widget {
    // A readout only while it works: an idle agent's flat line says nothing its state doesn't.
    match a.note.as_ref().filter(|_| a.state == State::Working) {
        Some(n) => {
            let what = format!("PID {}, {}", a.pid, if n.idle { "idle" } else { "working" });
            let area = scope::compact(&ui.scope, a.pid, scope::rows(&n.helpers).len(), &what);
            ui.readouts.borrow_mut().push(area.clone());
            area.upcast()
        }
        // No note, no wave: the cell stays empty rather than explaining why.
        None => w::label("", &[]).upcast(),
    }
}

/// Builds a row from the cells for every column, leaving out the ones `cols` dropped.
fn cells(all: Vec<gtk::Widget>, full_cols: &[Col], hide: u8) -> Vec<(gtk::Widget, i32, bool)> {
    let kept: Vec<gtk::Widget> = all.into_iter().enumerate().filter(|(i, _)| hide & (1 << i) == 0).map(|(_, w)| w).collect();
    kept.into_iter().zip(full_cols).map(|(w, c)| (w, c.1, c.2)).collect()
}

fn where_words(a: &AgentProc) -> Option<String> {
    a.location.as_ref().map(app::tab_label)
}

fn proc_row(ui: &Ui, a: &AgentProc, daemon: Option<u32>, p: &ProcOverview, cols: &[Col], hide: u8) -> gtk::ListBoxRow {
    let pid = w::vbox(2, &[]);
    pid.append(&w::clip(&a.pid.to_string(), &["pid"]));
    if a.kind == ProcKind::Subagent {
        pid.append(&w::clip("└ subagent", &["tree"]));
    } else if Some(a.ppid) == daemon && a.kind != ProcKind::Daemon {
        pid.append(&w::clip("└ daemon", &["tree"]));
    }
    // Task: the title, then the project and what it is doing right now (its helpers, in words).
    let task = w::vbox(2, &["pj"]);
    let top = w::hbox(8, &[]);
    let title = a.title.clone().unwrap_or_else(|| kind_words(a.kind).to_string());
    let tl = w::clip(&title, &["title"]);
    // The title keeps some room whatever sits beside it (as much as the column's narrowest allows).
    tl.set_width_chars(9);
    top.append(&tl);
    let subs = session_of(&ui.st.borrow(), a).map(|s| s.subagents.clone()).filter(|s| s.total > 0);
    if let Some(sc) = &subs {
        let open = ui.st.borrow().agent_open.contains(&a.pid);
        // Short: what they are doing is on the line below; how many run is in the tooltip.
        let words = format!("{} {}", if open { "▾" } else { "▸" }, crate::why::sub_count(sc));
        let b = gtk::Button::with_label(&words);
        b.add_css_class("subcount");
        b.set_widget_name(&format!("subcount-{}", a.pid));
        if let Some(l) = b.child().and_downcast::<gtk::Label>() {
            // In a narrow window the count gives way before the title does.
            l.set_ellipsize(gtk::pango::EllipsizeMode::End);
        }
        if sc.running > 0 {
            b.add_css_class("running");
        }
        b.set_tooltip_text(Some(if open { "Hide them (←)" } else { "Show them, running first (→)" }));
        b.set_valign(gtk::Align::Center);
        let ui2 = ui.clone();
        let pid = a.pid;
        b.connect_clicked(move |_| {
            let open = ui2.st.borrow().agent_open.contains(&pid);
            set_open(&ui2, pid, !open);
        });
        top.append(&b);
    }
    task.append(&top);
    // Never the raw command line (that is in the tooltip): the project, else where it started and when.
    let sub = match &a.project_name {
        Some(p) if !p.is_empty() => p.clone(),
        _ if a.kind == ProcKind::Session => {
            let when = a.started_at.as_deref().map(|t| format!(" · started {}", fmt::short_when(Some(t)))).unwrap_or_default();
            format!("{}{when}", views::project_label(None))
        }
        _ => kind_words(a.kind).to_string(),
    };
    let helpers = live_helpers(a);
    let sl = w::clip("", &["desc"]);
    if helpers.is_empty() {
        sl.set_text(&sub);
    } else {
        sl.set_markup(&format!("{} <span foreground=\"{}\">· now: {}</span>", w::esc(&sub), crate::theme::hex("mint"), w::esc(&helpers.join(" · "))));
    }
    sl.set_tooltip_text(Some(&if helpers.is_empty() { a.command.clone() } else { format!("{}\nnow: {}", a.command, helpers.join("\n     ")) }));
    task.append(&sl);
    // State and idle time; the daemon just says what it is for.
    let state = w::vbox(3, &[]);
    if a.kind == ProcKind::Daemon {
        state.append(&w::wrap("keeps background sessions alive", &["ev"]));
    } else {
        state.append(&w::state_badge(a.state));
        let idle = idle_words(&ui.st.borrow(), a);
        let l = w::clip(&idle, &["ev"]);
        let id = a.pid;
        live_text(ui, View::Agents, &l, move |st| st.procs.as_ref()?.procs.iter().find(|x| x.pid == id).map(|a| idle_words(st, a)));
        state.append(&l);
    }
    let mem = w::vbox(3, &[]);
    let rss = w::clip(&fmt::bytes(a.rss_bytes), &["num", "ink"]);
    let id = a.pid;
    live_text(ui, View::Agents, &rss, move |st| st.procs.as_ref()?.procs.iter().find(|x| x.pid == id).map(|a| fmt::bytes(a.rss_bytes)));
    if !a.children.is_empty() {
        let kids: Vec<String> = a
            .children
            .iter()
            .map(|c| {
                let what = p.procs.iter().find(|x| x.pid == *c).map_or("helper".to_string(), |x| {
                    if x.kind == ProcKind::Subagent {
                        "subagent".into()
                    } else if x.command.contains("attach") {
                        "attach client".into()
                    } else {
                        "pty host".into()
                    }
                });
                format!("{what} {c}")
            })
            .collect();
        rss.set_tooltip_text(Some(&format!("Includes {}", kids.join(", "))));
    }
    mem.append(&rss);
    let cpu = w::clip(&cpu_words(a), &["ev"]);
    live_text(ui, View::Agents, &cpu, move |st| st.procs.as_ref()?.procs.iter().find(|x| x.pid == id).map(cpu_words));
    mem.append(&cpu);
    let where_ = match (where_words(a), &a.job_id) {
        (Some(t), _) => w::label(&t, &["loc", "plain"]),
        (None, Some(_)) => w::label("background", &["loc", "re"]),
        _ => w::label("", &[]),
    };
    if let Some(j) = &a.job_id {
        where_.set_tooltip_text(Some(&format!("background job {j}")));
    }
    where_.set_halign(gtk::Align::Start);
    where_.set_ellipsize(gtk::pango::EllipsizeMode::End);
    // Only stale rows say why; the rest is blank.
    let why: gtk::Widget = match &a.stale_reason {
        Some(r) => w::wrap(r, &[if is_stale(a) { "why" } else { "ev" }]).upcast(),
        None => w::label("", &[]).upcast(),
    };
    let act: gtk::Widget = if a.kind == ProcKind::Daemon {
        let l = w::label("", &[]);
        l.set_tooltip_text(Some("Wake won't stop Claude's daemon: every background session depends on it."));
        l.upcast()
    } else {
        let ui2 = ui.clone();
        let pid = a.pid;
        // One verb. Quiet until the row is hovered or selected; red only when the row looks stale.
        let b = w::button("Stop", None, &["sm", "stop", if is_stale(a) { "danger" } else { "ghost" }], move || stop_one(&ui2, pid));
        b.set_tooltip_text(Some("Stop it (s). Wake asks first."));
        b.update_property(&[gtk::accessible::Property::Label(&format!("Stop {title}, PID {pid}"))]);
        b.set_halign(gtk::Align::Start);
        b.upcast()
    };
    let r = w::row(cells(vec![wave_cell(ui, a), pid.upcast(), task.upcast(), state.upcast(), mem.upcast(), where_.upcast(), why, act], cols, hide), &["tr", "agentrow"]);
    if is_stale(a) {
        r.add_css_class("stale");
    }
    let row = gtk::ListBoxRow::new();
    // A row inside its group's box (style.css: boxed lists).
    row.add_css_class("boxed");
    row.set_widget_name(&a.pid.to_string());
    if ui.st.borrow().agent_leaving.contains(&a.pid) {
        row.add_css_class("leaving");
        row.set_sensitive(false);
    }
    row.set_child(Some(&r));
    if a.session_id.is_some() {
        row.set_tooltip_text(Some(views::ROW_HINT));
    }
    row
}

/// What its subagents are doing now, by task: the job record's live list when there is one, else
/// the helper lines of agent-notify's note.
fn live_helpers(a: &AgentProc) -> Vec<String> {
    a.helpers.clone().or_else(|| a.note.as_ref().map(|n| n.helpers.clone())).unwrap_or_default()
}

/// "busy now", "idle 38 h": on the one idle clock (the transcript's last activity, as Monitor's
/// Last column), the process's own only when there is no transcript.
fn idle_words(st: &app::AppState, a: &AgentProc) -> String {
    let last = session_of(st, a).and_then(|s| s.last_at.clone());
    match (a.state, crate::why::idle_secs(last.as_deref(), a.idle_seconds)) {
        (State::Working, _) => "busy now".to_string(),
        (_, Some(s)) => format!("idle {}", fmt::idle_for(s)),
        _ => String::new(),
    }
}

fn cpu_words(a: &AgentProc) -> String {
    a.cpu_percent.map_or("CPU —".into(), |c| format!("CPU {c:.0}%"))
}

fn dead_row(ui: &Ui, pid: u32, cols: &[Col], hide: u8) -> gtk::ListBoxRow {
    let wave: gtk::Widget = {
        let area = scope::compact(&ui.scope, pid, 0, &format!("PID {pid}, gone"));
        ui.readouts.borrow_mut().push(area.clone());
        area.upcast()
    };
    let all: Vec<gtk::Widget> = vec![
        wave,
        w::label(&pid.to_string(), &["pid"]).upcast(),
        w::clip("a note with no process", &["title"]).upcast(),
        w::state_badge(State::Ended).upcast(),
        w::label("", &[]).upcast(),
        w::label("", &[]).upcast(),
        w::wrap("its process has gone; Slipstream clears the note at your next key or pointer input", &["ev"]).upcast(),
        w::label("", &[]).upcast(),
    ];
    let r = w::row(cells(all, cols, hide), &["tr", "agentrow"]);
    let row = gtk::ListBoxRow::new();
    // A row inside its group's box (style.css: boxed lists).
    row.add_css_class("boxed");
    row.set_widget_name(&format!("note-{pid}"));
    row.set_child(Some(&r));
    row
}

// ---------- subagents under a row ----------

/// What a row of the Agents list is, by its name: a session (named by its PID), one of its
/// subagents (`sa-<pid>-<agent id>`), the fold over its finished ones (`sfold-<pid>`), or anything
/// else (a note whose process has gone, a line saying the tree is loading).
#[derive(Debug, Clone, PartialEq)]
pub enum RowKey {
    Session(u32),
    Sub(u32, String),
    Fold(u32),
    Other,
}

pub fn row_key(name: &str) -> RowKey {
    if let Some(rest) = name.strip_prefix("sa-")
        && let Some((pid, aid)) = rest.split_once('-')
        && let Ok(pid) = pid.parse()
        && !aid.is_empty()
    {
        return RowKey::Sub(pid, aid.to_string());
    }
    if let Some(pid) = name.strip_prefix("sfold-").and_then(|p| p.parse().ok()) {
        return RowKey::Fold(pid);
    }
    match name.parse() {
        Ok(pid) => RowKey::Session(pid),
        Err(_) => RowKey::Other,
    }
}

/// The session a row belongs to: its own PID for a session row, its session's for a tree row.
pub fn session_pid(name: &str) -> Option<u32> {
    match row_key(name) {
        RowKey::Session(p) | RowKey::Sub(p, _) | RowKey::Fold(p) => Some(p),
        RowKey::Other => None,
    }
}

/// The session process behind the selected row (a subagent's or the fold's is its session's).
pub fn selected_proc(st: &app::AppState) -> Option<&AgentProc> {
    let pid = session_pid(st.sel.as_deref()?)?;
    st.procs.as_ref()?.procs.iter().find(|a| a.pid == pid)
}

fn sub_name(pid: u32, aid: &str) -> String {
    format!("sa-{pid}-{aid}")
}

fn fold_name(pid: u32) -> String {
    format!("sfold-{pid}")
}

/// The keys that walk the tree.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TreeKey {
    Enter,
    Right,
    Left,
}

#[derive(Debug, Clone, PartialEq)]
enum Act {
    Panel,
    Subs(bool),
    Steps(bool),
    Fold(bool),
    ToSession,
    Nothing,
}

/// What a key does on a row; `open` says whether what that row opens (a session's subagents, a
/// subagent's steps, the finished list) is open now.
fn act(row: &RowKey, k: TreeKey, open: bool) -> Act {
    use TreeKey::*;
    match (row, k) {
        (RowKey::Session(_), Enter) => Act::Panel,
        (RowKey::Session(_), Right) => Act::Subs(true),
        (RowKey::Session(_), Left) => Act::Subs(false),
        (RowKey::Sub(..), Enter) => Act::Steps(!open),
        (RowKey::Fold(_), Enter) => Act::Fold(!open),
        (RowKey::Sub(..) | RowKey::Fold(_), Right) if open => Act::Nothing,
        (RowKey::Sub(..), Right) => Act::Steps(true),
        (RowKey::Fold(_), Right) => Act::Fold(true),
        (RowKey::Sub(..), Left) if open => Act::Steps(false),
        (RowKey::Fold(_), Left) if open => Act::Fold(false),
        (RowKey::Sub(..) | RowKey::Fold(_), Left) => Act::ToSession,
        (RowKey::Other, _) => Act::Nothing,
    }
}

/// Enter, → or ← on the selected row. False when the row is nothing of the tree's.
pub fn key(ui: &Ui, k: TreeKey) -> bool {
    let Some(sel) = ui.st.borrow().sel.clone() else { return false };
    let row = row_key(&sel);
    let open = {
        let st = ui.st.borrow();
        match &row {
            RowKey::Session(p) => st.agent_open.contains(p),
            RowKey::Sub(_, aid) => st.agent_sub_open.contains(aid),
            RowKey::Fold(p) => st.agent_done_open.contains(p),
            RowKey::Other => false,
        }
    };
    match (act(&row, k, open), &row) {
        (Act::Panel, _) => {
            // The row's own session, never the one the panel already shows.
            let id = selected_proc(&ui.st.borrow()).and_then(|p| p.session_id.clone());
            match id {
                Some(id) => panel::open_session(ui, &id, true),
                None => app::toast(ui, "Wake couldn't match this agent to a session, so there are no details to show.", false),
            }
        }
        (Act::Subs(o), RowKey::Session(p)) => set_open(ui, *p, o),
        (Act::Steps(o), RowKey::Sub(p, aid)) => set_steps_open(ui, *p, aid, o),
        (Act::Fold(o), RowKey::Fold(p)) => set_fold(ui, *p, o),
        (Act::ToSession, _) => {
            if let Some((r, _)) = session_pid(&sel).and_then(|p| find_row(ui, p)) {
                app::select_row(ui, &r);
                crate::motion::scroll_into_view(&ui.w.views[&View::Agents].scroll, &r);
            }
        }
        (Act::Nothing, RowKey::Other) => return false,
        _ => {}
    }
    true
}

/// The class naming every tree row of a session, so they can be found again.
fn tree_class(pid: u32) -> String {
    format!("subs-{pid}")
}

thread_local! {
    /// What each tree row shows (apart from what ticks), by row name, so a refresh rebuilds only
    /// the rows that changed.
    static ROWSIG: std::cell::RefCell<std::collections::HashMap<String, String>> = Default::default();
    /// Sessions just opened whose subagents are still being read: once they arrive, they are
    /// scrolled into view as an opening is.
    static REVEAL: std::cell::RefCell<std::collections::HashSet<u32>> = Default::default();
}

/// The columns and hidden-column mask the list is drawn with now (the tree must match them).
fn layout(ui: &Ui) -> (Vec<Col>, u8) {
    let st = ui.st.borrow();
    let compact = app::compact(&st) || st.lay.push;
    let p = st.procs.clone().unwrap_or_default();
    let hide = hidden_cols(compact, st.lay.tight, p.procs.iter().any(|a| where_words(a).is_some()), p.procs.iter().any(|a| a.stale_reason.is_some()) || !p.dead_notes.is_empty());
    (cols(hide), hide)
}

/// How a tree row joins the line: on the trunk (more below), the trunk's end, or (under the
/// finished fold) on a second, inner line.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Link {
    Mid,
    Last,
    /// The fold with its finished list open: the trunk ends at its branch, and the inner line
    /// drops from that branch to the row's bottom, into the finished rows.
    FoldOpen,
    InnerMid,
    InnerLast,
    /// No branch of its own: the trunk passes by (a row's opened steps).
    Pass,
    /// The same on the inner line (the steps of a finished row with more below).
    InnerPass,
    /// Nothing (the steps under the last row).
    None,
}

/// The rows under an open session, in order: running first, then the fold, then (when it is open)
/// the finished ones; each with how it joins the line.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Item {
    Run(usize),
    Fold,
    Done(usize),
}

fn plan(n_run: usize, n_done: usize, done_open: bool) -> Vec<(Item, Link)> {
    let mut out = Vec::new();
    for i in 0..n_run {
        out.push((Item::Run(i), if i + 1 == n_run && n_done == 0 { Link::Last } else { Link::Mid }));
    }
    if n_done > 0 {
        out.push((Item::Fold, if done_open { Link::FoldOpen } else { Link::Last }));
        if done_open {
            for i in 0..n_done {
                out.push((Item::Done(i), if i + 1 == n_done { Link::InnerLast } else { Link::InnerMid }));
            }
        }
    }
    out
}

/// Where the selection goes when its row has gone (a subagent finished into the closed fold, the
/// fold closed): the nearest row after it that is still there, else the nearest before, else the
/// session.
fn fallback(old: &[String], new: &[String], sel: &str, session: &str) -> String {
    let Some(i) = old.iter().position(|n| n == sel) else { return session.to_string() };
    old[i + 1..].iter().find(|n| new.contains(n)).or_else(|| old[..i].iter().rev().find(|n| new.contains(n))).cloned().unwrap_or_else(|| session.to_string())
}

/// The vertical hairline position of the trunk, and of the inner line.
fn trunk_x(w: f64) -> (f64, f64) {
    let trunk = (w - 26.0).max(6.0).floor() + 0.5;
    (trunk, trunk + 12.0)
}

/// The connector, drawn in the wave column: a vertical hairline with a short branch into the row
/// at its title line. Drawn, not typed, so it joins up from row to row.
fn connector(link: Link, tick_at: Option<f64>) -> gtk::DrawingArea {
    let da = gtk::DrawingArea::new();
    da.add_css_class("connector");
    da.set_hexpand(true);
    draw_link(&da, link, tick_at);
    da
}

fn draw_link(da: &gtk::DrawingArea, link: Link, tick_at: Option<f64>) {
    da.set_draw_func(move |_, cr, w, h| {
        let (r, g, b) = crate::theme::rgb(crate::theme::hex("line3"));
        cr.set_source_rgb(r, g, b);
        cr.set_line_width(1.0);
        let (w, h) = (f64::from(w), f64::from(h));
        let (trunk, inner) = trunk_x(w);
        let y = tick_at.unwrap_or(h / 2.0).floor() + 0.5;
        let end = w - 6.0;
        let (x, full) = match link {
            Link::Mid => (trunk, true),
            Link::Last => (trunk, false),
            Link::FoldOpen => {
                cr.move_to(inner, y);
                cr.line_to(inner, h);
                (trunk, false)
            }
            Link::InnerMid => (inner, true),
            Link::InnerLast => (inner, false),
            Link::Pass | Link::InnerPass => {
                let x = if link == Link::Pass { trunk } else { inner };
                cr.move_to(x, 0.0);
                cr.line_to(x, h);
                let _ = cr.stroke();
                return;
            }
            Link::None => return,
        };
        cr.move_to(x, 0.0);
        cr.line_to(x, if full { h } else { y });
        cr.move_to(x, y);
        cr.line_to(end, y);
        let _ = cr.stroke();
    });
}

/// A tree row in the table's columns: `conn` in the wave column, then the PID, task and state
/// cells, the rest blank. With the wave column hidden (a narrow window) the connector takes the
/// PID column's place.
fn tree_row(conn: gtk::DrawingArea, pid: gtk::Widget, task: gtk::Widget, state: gtk::Widget, cols: &[Col], hide: u8) -> gtk::Box {
    let blank = || w::label("", &[]).upcast::<gtk::Widget>();
    let wave_hidden = hide & 1 != 0;
    let all: Vec<gtk::Widget> = if wave_hidden {
        vec![blank(), conn.clone().upcast(), task, state, blank(), blank(), blank(), blank()]
    } else {
        vec![conn.clone().upcast(), pid, task, state, blank(), blank(), blank(), blank()]
    };
    let row = w::row(cells(all, cols, hide), &["tr", "agentrow", "subtr"]);
    // The connector fills its cell top to bottom, so it meets the rows above and below.
    conn.set_valign(gtk::Align::Fill);
    if let Some(cell) = conn.parent() {
        cell.add_css_class("conncell");
    }
    row
}

/// A row spanning from the task column to the end (messages, a subagent's steps).
fn tree_span(conn: gtk::DrawingArea, child: &impl IsA<gtk::Widget>, cols: &[Col], hide: u8) -> gtk::Box {
    let line = w::hbox(0, &["subtr", "span"]);
    let n_lead = if hide & 1 == 0 { 2 } else { 1 };
    for (i, c) in cols.iter().take(n_lead).enumerate() {
        if i == 0 {
            line.append(&w::col(&conn, c.1, false));
            conn.set_valign(gtk::Align::Fill);
            if let Some(cell) = conn.parent() {
                cell.add_css_class("conncell");
            }
        } else {
            line.append(&w::col(&w::hbox(0, &[]), c.1, false));
        }
    }
    let rest: i32 = cols.iter().skip(n_lead).map(|c| w::col_width(c.1)).sum();
    line.append(&w::col(child, rest, true));
    line
}

/// One row the tree wants: its name, what it shows (apart from what ticks), and what it is.
struct Want {
    name: String,
    sig: String,
    what: What,
}

enum What {
    Note(&'static str),
    Sub(SubagentSummary, Link),
    Fold(String, Link),
}

/// The rows an open session's tree should have now, in order (none when it is closed).
fn wanted(ui: &Ui, a: &AgentProc) -> Vec<Want> {
    let pid = a.pid;
    let (open, done_open, detail) = {
        let st = ui.st.borrow();
        (st.agent_open.contains(&pid), st.agent_done_open.contains(&pid), a.session_id.as_ref().and_then(|s| st.sessions.get(s).cloned()))
    };
    if !open || a.session_id.is_none() {
        return vec![];
    }
    let note = |t: &'static str| vec![Want { name: format!("snote-{pid}"), sig: t.to_string(), what: What::Note(t) }];
    let Some(d) = detail else { return note("reading its subagents…") };
    if d.subagents.is_empty() {
        return note("No subagents. This session did its work itself.");
    }
    let helpers = live_helpers(a);
    let (running, done) = crate::why::split_subs(&d.subagents);
    plan(running.len(), done.len(), done_open)
        .into_iter()
        .map(|(item, link)| match item {
            Item::Run(i) => {
                let mut s = running[i].clone();
                // The screensaver's helper line says what a running one is doing when its record doesn't.
                if s.live.is_none() {
                    s.live = helpers.iter().find(|h| h.contains(&s.task) || s.task.contains(h.as_str())).cloned();
                }
                // Its "now:" line is patched in place, so it is not part of what it shows.
                Want { name: sub_name(pid, &s.agent_id), sig: format!("run{link:?}{}{}", s.task, s.kind), what: What::Sub(s, link) }
            }
            Item::Done(i) => {
                let s = done[i].clone();
                // How long ago it finished ticks, and is patched in place.
                let sig = format!("done{link:?}{:?}{}{}{:?}{:?}", s.state, s.task, s.kind, s.handed_back, s.last_at);
                Want { name: sub_name(pid, &s.agent_id), sig, what: What::Sub(s, link) }
            }
            // Open or closed is changed in place, so it is not part of what it shows.
            Item::Fold => {
                let line = crate::why::finished_line(&done);
                Want { name: fold_name(pid), sig: format!("fold{}", done.len()), what: What::Fold(line, if link == Link::FoldOpen { Link::Last } else { link }) }
            }
        })
        .collect()
}

/// Builds one tree row: a list row of its own, selected and walked like a session's row.
fn build(ui: &Ui, a: &AgentProc, want: &Want, cols: &[Col], hide: u8) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();
    // A row inside its group's box (style.css: boxed lists).
    row.add_css_class("boxed");
    row.set_widget_name(&want.name);
    row.add_css_class("treerow");
    row.add_css_class(&tree_class(a.pid));
    let sid = a.session_id.clone().unwrap_or_default();
    match &want.what {
        What::Note(t) => {
            row.set_selectable(false);
            row.set_activatable(false);
            row.set_focusable(false);
            let class = if t.starts_with("reading") { "loading" } else { "desc" };
            row.set_child(Some(&tree_span(connector(Link::Last, None), &w::label(t, &[class]), cols, hide)));
        }
        What::Sub(s, link) => {
            let open = ui.st.borrow().agent_sub_open.contains(&s.agent_id);
            row.set_child(Some(&tree_line(ui, &sid, s, *link, open, cols, hide)));
            let kind = if s.kind.is_empty() || s.kind == "general-purpose" { String::new() } else { format!(" ({})", s.kind) };
            row.set_tooltip_text(Some(&format!("{}{kind}\nClick for its steps · Enter", s.task)));
            on_click(ui, &row);
        }
        What::Fold(line, link) => {
            let open = ui.st.borrow().agent_done_open.contains(&a.pid);
            row.set_child(Some(&fold_line(a.pid, line, if open { Link::FoldOpen } else { *link }, open, cols, hide)));
            row.set_tooltip_text(Some(fold_tip(open)));
            on_click(ui, &row);
        }
    }
    ROWSIG.with(|m| m.borrow_mut().insert(want.name.clone(), want.sig.clone()));
    row
}

fn fold_tip(open: bool) -> &'static str {
    if open { "Hide the finished ones · ← or click" } else { "Show the finished ones, newest first · → or click" }
}

/// A tree row opens on a single click, as the buttons it replaced did (a double click opens it
/// once: the list's own activation skips tree rows).
fn on_click(ui: &Ui, row: &gtk::ListBoxRow) {
    let g = gtk::GestureClick::new();
    g.set_button(gtk::gdk::BUTTON_PRIMARY);
    let ui2 = ui.clone();
    g.connect_released(move |g, n, _, _| {
        let Some(row) = g.widget().and_downcast::<gtk::ListBoxRow>() else { return };
        if n != 1 || row.widget_name().is_empty() {
            return;
        }
        ui2.st.borrow_mut().sel = Some(row.widget_name().to_string());
        key(&ui2, TreeKey::Enter);
    });
    row.add_controller(g);
}

/// The fold's row: "▸ 28 finished" in the task column, when the last one finished in the state
/// column.
fn fold_line(pid: u32, line: &str, link: Link, open: bool, cols: &[Col], hide: u8) -> gtk::Box {
    let task = w::hbox(8, &["pj"]);
    let chev = w::label(if open { "▾" } else { "▸" }, &["chev"]);
    chev.set_widget_name(&format!("subchev-{pid}"));
    task.append(&chev);
    let (count, when) = line.split_once(" · ").unwrap_or((line, ""));
    task.append(&w::clip(count, &["desc"]));
    let state = w::clip(when, &["ev"]);
    state.set_widget_name(&format!("subwhen-{pid}"));
    let conn = connector(link, None);
    conn.set_widget_name(&format!("subconn-{pid}"));
    tree_row(conn, w::label("", &[]).upcast(), task.upcast(), state.upcast(), cols, hide)
}

/// One subagent as a table row: its task where a session's title goes, what it is doing now (or
/// what it handed back) under it, and running or when it finished in the state column. Its steps
/// open under it, inside the same row.
fn tree_line(ui: &Ui, sid: &str, s: &SubagentSummary, link: Link, open: bool, cols: &[Col], hide: u8) -> gtk::Box {
    let outer = w::vbox(0, &["tline"]);
    outer.set_widget_name(&format!("subw-{}", s.agent_id));
    let running = s.state == State::Working;
    let task = w::vbox(2, &["pj"]);
    let title = w::clip(&s.task, &["title"]);
    title.set_width_chars(9);
    task.append(&title);
    let second = if running {
        let l = w::clip(&format!("now: {}", s.live.as_deref().unwrap_or("working")), &["desc", "nowline"]);
        l.set_widget_name(&format!("sublive-{}", s.agent_id));
        l
    } else {
        let back = s.handed_back.as_deref().map(|h| h.lines().next().unwrap_or("").trim().to_string()).filter(|h| !h.is_empty());
        w::clip(&back.unwrap_or_else(|| if s.kind.is_empty() || s.kind == "general-purpose" { "subagent".into() } else { s.kind.clone() }), &["desc"])
    };
    task.append(&second);
    let state = w::vbox(3, &[]);
    if running {
        state.append(&w::state_badge(State::Working));
        state.append(&w::clip("busy now", &["ev"]));
    } else {
        let b = w::hbox(7, &["state", "quiet"]);
        b.append(&w::dot("done"));
        b.append(&w::clip("finished", &[]));
        state.append(&b);
        let ago = w::clip(&fmt::ago(s.last_at.as_deref()), &["ev"]);
        ago.set_widget_name(&format!("subago-{}", s.agent_id));
        state.append(&ago);
    }
    let pid_cell = w::clip("subagent", &["tree"]);
    let line = tree_row(connector(link, Some(18.0)), pid_cell.upcast(), task.upcast(), state.upcast(), cols, hide);
    line.set_widget_name(&format!("sub-{}", s.agent_id));
    if matches!(link, Link::InnerMid | Link::InnerLast) {
        line.add_css_class("inner");
    }
    outer.append(&line);
    let steps = w::vbox(3, &["steps", "tsteps"]);
    let rev = crate::motion::revealer(gtk::RevealerTransitionType::SlideDown, &steps, false);
    rev.set_widget_name(&format!("steps-{}", s.agent_id));
    // Its steps sit in the task column; the line carries on past them when more rows follow.
    let pass = match link {
        Link::Mid => Link::Pass,
        Link::InnerMid => Link::InnerPass,
        _ => Link::None,
    };
    let span = tree_span(connector(pass, None), &rev, cols, hide);
    // Closed, the steps take no room at all (a hidden revealer's line would still be a line high).
    span.set_visible(open);
    {
        let s2 = span.clone();
        rev.connect_reveal_child_notify(move |r| {
            if r.reveals_child() {
                s2.set_visible(true);
            }
        });
        let s3 = span.clone();
        rev.connect_child_revealed_notify(move |r| {
            if !r.is_child_revealed() && !r.reveals_child() {
                s3.set_visible(false);
            }
        });
    }
    outer.append(&span);
    if open {
        let (ui2, aid, sid2) = (ui.clone(), s.agent_id.clone(), sid.to_string());
        let o = outer.clone();
        glib::idle_add_local_once(move || panel::set_steps(&ui2, o.upcast_ref(), None, &sid2, &aid, true));
    }
    outer
}

/// The row named `name` in the list (rows on their way out have no name).
fn find_named_row(ui: &Ui, name: &str) -> Option<gtk::ListBoxRow> {
    let list = &ui.w.views[&View::Agents].list;
    let mut c = list.first_child();
    while let Some(x) = c {
        if x.widget_name() == name {
            return x.downcast().ok();
        }
        c = x.next_sibling();
    }
    None
}

/// The row for `pid` in the list, and where it is.
fn find_row(ui: &Ui, pid: u32) -> Option<(gtk::ListBoxRow, i32)> {
    find_named_row(ui, &pid.to_string()).map(|r| {
        let i = r.index();
        (r, i)
    })
}

/// A session's tree rows in the list, in order.
fn tree_rows(ui: &Ui, pid: u32) -> Vec<gtk::ListBoxRow> {
    let list = &ui.w.views[&View::Agents].list;
    let class = tree_class(pid);
    let mut out = Vec::new();
    let mut c = list.first_child();
    while let Some(x) = c {
        c = x.next_sibling();
        if x.has_css_class(&class)
            && !x.widget_name().is_empty()
            && let Ok(r) = x.downcast::<gtk::ListBoxRow>()
        {
            out.push(r);
        }
    }
    out
}

/// The nearest row above `r` that is not on its way out.
fn prev_live(r: &gtk::ListBoxRow) -> Option<gtk::Widget> {
    let mut c = r.prev_sibling();
    while let Some(x) = c {
        if !x.widget_name().is_empty() {
            return Some(x);
        }
        c = x.prev_sibling();
    }
    None
}

/// A tree row that is no longer wanted: unselectable and nameless at once (the keyboard skips it),
/// then faded out and gone.
fn retire(list: &gtk::ListBox, r: &gtk::ListBoxRow, pid: u32) {
    r.set_widget_name("");
    r.remove_css_class(&tree_class(pid));
    r.set_selectable(false);
    r.set_activatable(false);
    r.set_focusable(false);
    let (l2, r2) = (list.clone(), r.clone());
    crate::motion::fade_out(r, move || {
        if r2.parent().is_some() {
            l2.remove(&r2);
        }
    });
}

/// Brings a session's tree rows in line with what it should show now, in place: rows that didn't
/// change stay (with their selection, focus and open steps), changed ones are rebuilt where they
/// are, new ones are inserted, gone ones fade out. The selection stays put, or moves to a
/// neighbour when its row has gone. Returns the new rows.
fn sync_tree(ui: &Ui, pid: u32) -> Vec<gtk::ListBoxRow> {
    let vw = &ui.w.views[&View::Agents];
    let list = vw.list.clone();
    let Some((session_row, _)) = find_row(ui, pid) else { return vec![] };
    let Some(a) = find(ui, pid) else { return vec![] };
    let want = wanted(ui, &a);
    let existing = tree_rows(ui, pid);
    let old_names: Vec<String> = existing.iter().map(|r| r.widget_name().to_string()).collect();
    let had_focus = gtk::prelude::GtkWindowExt::focus(&ui.window).is_some_and(|f| f.is_ancestor(&list));
    let (cols, hide) = layout(ui);
    let mut used: Vec<gtk::ListBoxRow> = Vec::new();
    let mut fresh = Vec::new();
    let mut prev: gtk::Widget = session_row.clone().upcast();
    for w in &want {
        let sig = Some(w.sig.clone());
        let keep = existing.iter().find(|r| r.widget_name() == w.name.as_str() && !used.contains(r) && ROWSIG.with(|m| m.borrow().get(&w.name).cloned()) == sig).cloned();
        let row = match keep {
            Some(r) => {
                if prev_live(&r).as_ref() != Some(&prev) {
                    list.remove(&r);
                    list.insert(&r, prev.downcast_ref::<gtk::ListBoxRow>().map_or(0, |p| p.index()) + 1);
                }
                r
            }
            None => {
                let r = build(ui, &a, w, &cols, hide);
                list.insert(&r, prev.downcast_ref::<gtk::ListBoxRow>().map_or(0, |p| p.index()) + 1);
                fresh.push(r.clone());
                r
            }
        };
        used.push(row.clone());
        prev = row.upcast();
    }
    let want_names: Vec<String> = want.iter().map(|w| w.name.clone()).collect();
    for r in existing.iter().filter(|r| !used.contains(r)) {
        let name = r.widget_name().to_string();
        if !want_names.contains(&name) {
            ROWSIG.with(|m| m.borrow_mut().remove(&name));
        }
        retire(&list, r, pid);
    }
    // The selection: still on its row (rebuilt or moved rows are selected again), else on the
    // nearest row that is still there.
    let sel = ui.st.borrow().sel.clone();
    if let Some(sel) = sel
        && session_pid(&sel) == Some(pid)
    {
        let target = if find_named_row(ui, &sel).is_some() {
            sel.clone()
        } else {
            let walk = |names: &[String]| std::iter::once(pid.to_string()).chain(names.iter().filter(|n| !n.starts_with("snote-")).cloned()).collect::<Vec<_>>();
            fallback(&walk(&old_names), &walk(&want_names), &sel, &pid.to_string())
        };
        if let Some(row) = find_named_row(ui, &target)
            && list.selected_row().as_ref() != Some(&row)
        {
            ui.st.borrow_mut().sel = Some(target);
            list.select_row(Some(&row));
            if had_focus {
                let adj = vw.scroll.vadjustment();
                let v = adj.value();
                row.grab_focus();
                w::keep_scroll(&adj, v);
            }
        }
    }
    fresh
}

/// New rows fade in under `anchor`, scrolled into view only as far as they need, never pushing
/// `anchor` off the top.
fn reveal(ui: &Ui, anchor: &gtk::ListBoxRow, fresh: &[gtk::ListBoxRow]) {
    for r in fresh {
        crate::motion::fade_in(r);
    }
    if let Some(last) = fresh.last() {
        crate::motion::reveal_below(&ui.w.views[&View::Agents].scroll, anchor, last);
    }
}

/// After new data: each open session's tree brought up to date in place, and the running
/// subagents' live lines patched.
fn patch_trees(ui: &Ui) {
    let open: Vec<(u32, Option<String>)> = {
        let st = ui.st.borrow();
        st.procs.as_ref().map(|p| p.procs.iter().filter(|a| st.agent_open.contains(&a.pid)).map(|a| (a.pid, a.session_id.clone())).collect()).unwrap_or_default()
    };
    for (pid, sid) in open {
        let fresh = sync_tree(ui, pid);
        let first_fill = !fresh.is_empty() && !fresh.iter().any(|r| r.widget_name().starts_with("snote-")) && REVEAL.with(|r| r.borrow_mut().remove(&pid));
        match find_row(ui, pid) {
            Some((row, _)) if first_fill => reveal(ui, &row, &fresh),
            _ => fresh.iter().for_each(crate::motion::fade_in),
        }
        let subs = sid.and_then(|sid| ui.st.borrow().sessions.get(&sid).map(|d| d.subagents.clone())).unwrap_or_default();
        let set = |root: &gtk::ListBoxRow, name: &str, t: &str| {
            if let Some(l) = panel::find_named(root.upcast_ref(), name).and_downcast::<gtk::Label>()
                && l.text() != t
            {
                l.set_text(t);
            }
        };
        for r in tree_rows(ui, pid) {
            match row_key(&r.widget_name()) {
                RowKey::Sub(_, aid) => {
                    if let Some(s) = subs.iter().find(|s| s.agent_id == aid) {
                        panel::patch_subs(r.upcast_ref(), std::slice::from_ref(s));
                        set(&r, &format!("subago-{aid}"), &fmt::ago(s.last_at.as_deref()));
                    }
                }
                RowKey::Fold(_) => {
                    let (_, done) = crate::why::split_subs(&subs);
                    let line = crate::why::finished_line(&done);
                    set(&r, &format!("subwhen-{pid}"), line.split_once(" · ").map_or("", |(_, w)| w));
                }
                _ => {}
            }
        }
    }
}

/// Opens or closes a row's subagents (→ and ←) in place: its tree rows slide in under the row,
/// nothing else is redrawn. Opening fetches them in the background.
pub fn set_open(ui: &Ui, pid: u32, open: bool) {
    let Some(a) = find(ui, pid) else { return };
    let sid = a.session_id.clone();
    let has = sid.as_deref().is_some_and(|id| ui.st.borrow().ov.as_ref().is_some_and(|ov| ov.sessions.iter().any(|s| s.id == id && s.subagents.total > 0)));
    if open && !has {
        return;
    }
    {
        let mut st = ui.st.borrow_mut();
        if open {
            st.agent_open.insert(pid);
        } else {
            st.agent_open.remove(&pid);
        }
    }
    if let Some(b) = panel::find_named(ui.w.views[&View::Agents].list.upcast_ref(), &format!("subcount-{pid}")).and_downcast::<gtk::Button>()
        && let Some(label) = b.label()
    {
        let rest = label.trim_start_matches(['▸', '▾', ' ']).to_string();
        b.set_label(&format!("{} {rest}", if open { "▾" } else { "▸" }));
        b.set_tooltip_text(Some(if open { "Hide them (←)" } else { "Show them, running first (→)" }));
        if let Some(l) = b.child().and_downcast::<gtk::Label>() {
            l.set_ellipsize(gtk::pango::EllipsizeMode::End);
        }
    }
    let fresh = sync_tree(ui, pid);
    if open && let Some((row, _)) = find_row(ui, pid) {
        if fresh.iter().any(|r| r.widget_name().starts_with("snote-")) {
            REVEAL.with(|r| r.borrow_mut().insert(pid));
        }
        reveal(ui, &row, &fresh);
    }
    if open && let Some(sid) = sid {
        load(ui, vec![sid], false);
    }
}

/// Opens or closes the finished list under the fold (Enter, →, ←, or a click on it).
fn set_fold(ui: &Ui, pid: u32, open: bool) {
    {
        let mut st = ui.st.borrow_mut();
        if open {
            st.agent_done_open.insert(pid);
        } else {
            st.agent_done_open.remove(&pid);
        }
    }
    let Some(fold) = find_named_row(ui, &fold_name(pid)) else { return };
    if let Some(c) = panel::find_named(fold.upcast_ref(), &format!("subchev-{pid}")).and_downcast::<gtk::Label>() {
        c.set_text(if open { "▾" } else { "▸" });
    }
    // The inner line drops from the fold's branch only while the finished list is open.
    if let Some(da) = panel::find_named(fold.upcast_ref(), &format!("subconn-{pid}")).and_downcast::<gtk::DrawingArea>() {
        draw_link(&da, if open { Link::FoldOpen } else { Link::Last }, None);
        da.queue_draw();
    }
    fold.set_tooltip_text(Some(fold_tip(open)));
    let fresh = sync_tree(ui, pid);
    reveal(ui, &fold, &fresh);
}

/// Opens or closes a subagent's steps under its row.
fn set_steps_open(ui: &Ui, pid: u32, aid: &str, open: bool) {
    {
        let mut st = ui.st.borrow_mut();
        if open {
            st.agent_sub_open.insert(aid.to_string());
        } else {
            st.agent_sub_open.remove(aid);
        }
    }
    let name = sub_name(pid, aid);
    let Some(row) = find_named_row(ui, &name) else { return };
    let Some(sid) = find(ui, pid).and_then(|a| a.session_id) else { return };
    let sw = ui.w.views[&View::Agents].scroll.clone();
    panel::set_steps(ui, row.upcast_ref(), Some(&sw), &sid, aid, open);
    if open {
        let (aid2, sid2) = (aid.to_string(), sid.clone());
        panel::load_sub(ui, &sid, aid, move |ui| {
            if ui.st.borrow().agent_sub_open.contains(&aid2)
                && let Some(row) = find_named_row(ui, &name)
            {
                panel::set_steps(ui, row.upcast_ref(), None, &sid2, &aid2, true);
            }
        });
    }
}

/// From the session panel: Agents, with this process's row selected and in view.
pub fn show_row(ui: &Ui, pid: u32) {
    app::set_view(ui, View::Agents);
    ui.st.borrow_mut().sel = Some(pid.to_string());
    render(ui);
    app::restore_sel(ui, View::Agents, false);
    if let Some((row, _)) = find_row(ui, pid) {
        ui.w.views[&View::Agents].list.select_row(Some(&row));
        crate::motion::scroll_into_view(&ui.w.views[&View::Agents].scroll, &row);
    }
}

/// After each process refresh: the open rows' subagents again, so their live lines stay current.
pub fn refresh_open(ui: &Ui) {
    let sids: Vec<String> = {
        let st = ui.st.borrow();
        if st.busy_agents_open {
            return;
        }
        st.procs.as_ref().map(|p| p.procs.iter().filter(|a| st.agent_open.contains(&a.pid)).filter_map(|a| a.session_id.clone()).collect()).unwrap_or_default()
    };
    if !sids.is_empty() {
        ui.st.borrow_mut().busy_agents_open = true;
        load(ui, sids, true);
    }
}

/// Reads these sessions (and their opened, running subagents' steps) off the UI thread. A
/// `periodic` read clears the busy flag `refresh_open` set.
fn load(ui: &Ui, sids: Vec<String>, periodic: bool) {
    let open: Vec<String> = ui.st.borrow().agent_sub_open.iter().cloned().collect();
    app::bg_or(
        ui,
        move |src| {
            sids.iter()
                .filter_map(|id| {
                    let d = src.session(id)?;
                    let subs: Vec<SubagentDetail> = d.subagents.iter().filter(|a| open.contains(&a.agent_id) && a.state == State::Working).filter_map(|a| src.subagent(id, &a.agent_id)).collect();
                    Some((d, subs))
                })
                .collect::<Vec<_>>()
        },
        move |ui, got| {
            {
                let mut st = ui.st.borrow_mut();
                if periodic {
                    st.busy_agents_open = false;
                }
                for (d, subs) in got {
                    let id = d.summary.id.clone();
                    for s in subs {
                        st.subs.insert((id.clone(), s.summary.agent_id.clone()), s);
                    }
                    st.sessions.insert(id, d);
                }
            }
            if ui.st.borrow().view == View::Agents {
                render(ui);
                patch_trees(ui);
            }
        },
        move |ui| {
            if periodic {
                ui.st.borrow_mut().busy_agents_open = false;
            }
            app::toast(ui, "Wake hit an error reading that session; it will keep trying.", true);
        },
    );
}

// ---------- animation: only while the view is visible ----------

pub fn shown(ui: &Ui) {
    ui.scope.borrow_mut().hidden();
    ensure_ticking(ui);
}

pub fn hidden(ui: &Ui) {
    if let Some(id) = ui.tick.borrow_mut().take() {
        id.remove();
    }
    ui.scope.borrow_mut().hidden();
}

/// Keeps the waves moving while a view that shows them (Agents, Monitor) is on screen.
pub fn ensure_ticking(ui: &Ui) {
    let v = ui.st.borrow().view;
    if ui.tick.borrow().is_some() || !matches!(v, View::Agents | View::Monitor) {
        return;
    }
    let root = ui.w.views[&v].root.clone();
    let ui2 = ui.clone();
    let id = root.add_tick_callback(move |_, clock| {
        let now = clock.frame_time() as f64 / 1_000_000.0;
        let moving = ui2.scope.borrow_mut().advance(now);
        let due = {
            let m = ui2.scope.borrow();
            now - m.painted_at >= scope::STEP
        };
        if due || !moving {
            ui2.scope.borrow_mut().painted_at = now;
            for r in ui2.readouts.borrow().iter() {
                r.queue_draw();
            }
        }
        if moving {
            glib::ControlFlow::Continue
        } else {
            // At rest: the clock stops until the readouts change again.
            ui2.tick.borrow_mut().take();
            glib::ControlFlow::Break
        }
    });
    *ui.tick.borrow_mut() = Some(id);
}

// ---------- stopping ----------

fn find(ui: &Ui, pid: u32) -> Option<AgentProc> {
    ui.st.borrow().procs.as_ref().and_then(|p| p.procs.iter().find(|a| a.pid == pid).cloned())
}

fn task_of(a: &AgentProc) -> String {
    a.title.clone().unwrap_or_else(|| kind_words(a.kind).to_string())
}

fn how_for(a: &AgentProc) -> StopHow {
    if a.job_id.is_some() { StopHow::AgentStop } else { StopHow::Terminate }
}

/// What a session last said, for the stop dialog: the question it is waiting on, else its recap.
fn last_words(s: &SessionSummary) -> Option<String> {
    if let Some(q) = &s.question {
        return Some(format!("It is waiting on you: “{}”", fmt::clip(q, 220)));
    }
    let r = s.away_summary.as_deref().map(fmt::recap).filter(|r| !r.is_empty())?;
    Some(format!("It last said: “{}”", fmt::clip(&r, 220)))
}

/// One line of a stop list, in the person's terms: task · project · memory · idle, PID on the right.
pub fn stop_line(a: &AgentProc) -> (String, String) {
    stop_line_at(a, None)
}

/// As `stop_line`, idle on the one clock when the session's last activity is known.
pub fn stop_line_at(a: &AgentProc, last_at: Option<&str>) -> (String, String) {
    let mut parts = vec![task_of(a)];
    if let Some(p) = &a.project_name {
        parts.push(p.clone());
    }
    parts.push(fmt::bytes(a.rss_bytes));
    if a.state != State::Working
        && let Some(s) = crate::why::idle_secs(last_at, a.idle_seconds)
    {
        parts.push(format!("idle {}", fmt::idle_for(s)));
    }
    (parts.join(" · "), format!("PID {}", a.pid))
}

/// What stopping this one means, in plain words (no signal names).
pub fn stop_words(a: &AgentProc) -> Vec<String> {
    let later = "If it is still running after 5 seconds, Wake offers to force it.".to_string();
    match (a.kind, &a.job_id, where_words(a)) {
        (ProcKind::Stray, ..) => vec![
            "Wake can't tie this process to a session. Check its command below before you stop it.".into(),
            "If it's a Claude you started yourself in a terminal you still use, choose Cancel. Its conversation is kept either way.".into(),
            later,
        ],
        (ProcKind::Spare, ..) => vec!["It is a spare Claude nobody has used yet; stopping it loses nothing.".into()],
        (ProcKind::Subagent, ..) => vec!["It is a helper its session started. Stopping it ends that piece of work; the session carries on.".into(), later],
        (_, Some(_), _) => vec!["Claude stops this background session cleanly. Its conversation is kept, and you can resume it later.".into()],
        (_, None, Some(_)) => vec![
            format!(
                "The Claude in {} will exit; you can resume it later.",
                a.location.as_ref().and_then(|l| l.app.as_deref()).map_or("its terminal window".to_string(), |app| format!("its {app} window"))
            ),
            later,
        ],
        _ => vec!["It will exit. Its conversation stays on disk, and you can resume it later.".into(), later],
    }
}

pub fn stop_one(ui: &Ui, pid: u32) {
    let Some(a) = find(ui, pid) else {
        app::toast(ui, &format!("PID {pid} isn't among the agent processes any more."), true);
        return;
    };
    if a.kind == ProcKind::Daemon {
        app::toast(ui, "Wake won't stop Claude's daemon: every background session depends on it.", true);
        return;
    }
    let how = how_for(&a);
    let ticks = a.start_ticks;
    let task = task_of(&a);
    let (last, said, dirty) = {
        let st = ui.st.borrow();
        let s = session_of(&st, &a);
        let dirty = s.and_then(|s| s.project.as_deref()).and_then(|p| st.ov.as_ref()?.projects.iter().find(|x| x.id == p)).map(|p| (p.name.clone(), p.git.dirty_files));
        (s.and_then(|s| s.last_at.clone()), s.and_then(last_words), dirty)
    };
    let mut list = vec![stop_line_at(&a, last.as_deref())];
    if a.kind == ProcKind::Stray {
        list.push((a.command.clone(), String::new()));
    }
    if let Some(r) = &a.stale_reason {
        list.push((format!("looks stale: {r}"), String::new()));
    }
    let mut words = stop_words(&a);
    // What it last said (was it asking you something?), and what is unsaved in its project.
    if let Some(said) = said {
        words.push(said);
    }
    if let Some((name, n)) = dirty.filter(|(_, n)| *n > 0) {
        words.push(format!("{name} has {} not committed. Stopping doesn't touch them.", fmt::pl(n, "file", "files")));
    }
    dialogs::confirm(ui, &format!("Stop “{}”?", task_of(&a)), &words, &list, "Stop", move |ui| {
        let task = task.clone();
        mark_leaving(ui, &[pid]);
        app::bg(
            ui,
            move |src| src.stop(pid, ticks, how, false),
            move |ui, r| {
                if r.ok {
                    app::toast(ui, &stopped_words(&task, pid, how), false);
                    after_five(ui, vec![(pid, ticks)]);
                } else {
                    unmark_leaving(ui, &[pid]);
                    say(ui, &r);
                }
                app::refresh_procs(ui);
                app::refresh(ui);
            },
        );
    });
}

pub fn stop_all_stale(ui: &Ui) {
    let stale: Vec<AgentProc> = {
        let st = ui.st.borrow();
        let Some(p) = &st.procs else { return };
        stale_rows(p).into_iter().cloned().collect()
    };
    if stale.is_empty() {
        app::toast(ui, "Nothing looks stale.", false);
        return;
    }
    let total: u64 = stale.iter().map(|a| a.rss_bytes).sum();
    let list: Vec<(String, String)> = {
        let st = ui.st.borrow();
        stale.iter().map(|a| stop_line_at(a, session_of(&st, a).and_then(|s| s.last_at.as_deref()))).collect()
    };
    let title = if stale.len() == 1 { "Stop the stale agent?".to_string() } else { format!("Stop all {} stale agents?", stale.len()) };
    let body = vec![
        format!("This gives back about {}.", fmt::bytes(total)),
        "Background sessions get Claude's own clean stop; the others are asked to exit. Their conversations stay on disk, and you can resume them later.".into(),
        "Anything still running after 5 seconds can then be forced.".into(),
    ];
    let plan: Vec<(u32, Option<u64>, StopHow)> = stale.iter().map(|a| (a.pid, a.start_ticks, how_for(a))).collect();
    let names: Vec<(u32, String)> = stale.iter().map(|a| (a.pid, task_of(a))).collect();
    dialogs::confirm(ui, &title, &body, &list, &format!("Stop {}", stale.len()), move |ui| {
        let (plan, names) = (plan.clone(), names.clone());
        mark_leaving(ui, &plan.iter().map(|p| p.0).collect::<Vec<_>>());
        let which: Vec<(u32, Option<u64>)> = plan.iter().map(|(pid, ticks, _)| (*pid, *ticks)).collect();
        app::bg(
            ui,
            move |src| plan.iter().map(|(pid, ticks, how)| (*pid, src.stop(*pid, *ticks, *how, false))).collect::<Vec<_>>(),
            move |ui, results| {
                let ok: Vec<(u32, Option<u64>)> = which.into_iter().filter(|(pid, _)| results.iter().any(|(p, r)| p == pid && r.ok)).collect();
                unmark_leaving(ui, &results.iter().filter(|(_, r)| !r.ok).map(|(p, _)| *p).collect::<Vec<_>>());
                app::toast(ui, &stopped_all_words(&names, &results), results.iter().any(|(_, r)| !r.ok && !r.message.contains("already ended")));
                after_five(ui, ok);
                app::refresh_procs(ui);
                app::refresh(ui);
            },
        );
    });
}

/// Agents asked to stop: their rows stay where they are, dimmed and out of reach, until the
/// process has gone (then `fold_away`).
fn mark_leaving(ui: &Ui, pids: &[u32]) {
    ui.st.borrow_mut().agent_leaving.extend(pids.iter().copied());
    for &pid in pids {
        if let Some((row, _)) = find_row(ui, pid) {
            row.add_css_class("leaving");
            row.set_sensitive(false);
        }
    }
}

/// A stop that didn't go through: the row comes back.
fn unmark_leaving(ui: &Ui, pids: &[u32]) {
    if pids.is_empty() {
        return;
    }
    ui.st.borrow_mut().agent_leaving.retain(|p| !pids.contains(p));
    for &pid in pids {
        if let Some((row, _)) = find_row(ui, pid) {
            row.remove_css_class("leaving");
            row.set_sensitive(true);
        }
    }
}

/// The class naming every row of a group (its heading, box edges and agents).
fn group_class(group: &str) -> String {
    format!("grp-{}", group.to_lowercase().replace(' ', "-"))
}

/// Folds away the rows of agents whose process has gone (and their subagents; and their group's
/// heading and box, when nothing else is left in it), then redraws the list. False when there is
/// nothing on screen to fold (or motion is off): the list is simply redrawn.
fn fold_away(ui: &Ui, gone: &[u32]) -> bool {
    if !crate::motion::on() {
        return false;
    }
    let list = ui.w.views[&View::Agents].list.clone();
    let mut rows: Vec<gtk::ListBoxRow> = Vec::new();
    for &pid in gone {
        if let Some((row, _)) = find_row(ui, pid) {
            // Its group goes too when every agent in it is going.
            let grp = row.css_classes().into_iter().map(|c| c.to_string()).find(|c| c.starts_with("grp-"));
            rows.push(row);
            rows.extend(tree_rows(ui, pid));
            if let Some(grp) = grp {
                let mut members = Vec::new();
                let mut c = list.first_child();
                while let Some(x) = c {
                    c = x.next_sibling();
                    if x.has_css_class(&grp) {
                        members.push(x);
                    }
                }
                let staying = members.iter().any(|x| x.has_css_class("boxed") && !gone.iter().any(|p| x.widget_name() == p.to_string()));
                if !staying {
                    rows.extend(members.into_iter().filter_map(|x| x.downcast::<gtk::ListBoxRow>().ok()));
                }
            }
        }
    }
    rows.sort_by_key(|r| r.index());
    rows.dedup_by_key(|r| r.index());
    if rows.is_empty() {
        return false;
    }
    ui.st.borrow_mut().agents_folding = true;
    let left = std::rc::Rc::new(std::cell::Cell::new(rows.len()));
    for row in rows {
        // The row's contents in a revealer that slides shut, fading as it goes.
        let Some(child) = row.child() else {
            left.set(left.get() - 1);
            continue;
        };
        row.set_child(None::<&gtk::Widget>);
        let rev = gtk::Revealer::builder().transition_type(gtk::RevealerTransitionType::SlideUp).transition_duration(crate::motion::DUR + 80).reveal_child(true).child(&child).build();
        row.set_child(Some(&rev));
        crate::motion::fade_out(&child, || {});
        let (ui2, left2, gone2) = (ui.clone(), left.clone(), gone.to_vec());
        rev.connect_child_revealed_notify(move |r| {
            if r.is_child_revealed() {
                return;
            }
            left2.set(left2.get().saturating_sub(1));
            if left2.get() == 0 {
                {
                    let mut st = ui2.st.borrow_mut();
                    st.agents_folding = false;
                    st.agent_leaving.retain(|p| !gone2.contains(p));
                    st.drawn.remove(&View::Agents);
                }
                render(&ui2);
            }
        });
        rev.set_reveal_child(false);
    }
    if left.get() == 0 {
        ui.st.borrow_mut().agents_folding = false;
        return false;
    }
    true
}

/// What a stop that went through did, in plain words (core's own message names the signal).
fn stopped_words(task: &str, pid: u32, how: StopHow) -> String {
    let task = fmt::clip(task, 40);
    match how {
        StopHow::AgentStop => format!("Asked Claude to stop “{task}” (PID {pid}). Its conversation is kept."),
        StopHow::Terminate => format!("Asked “{task}” (PID {pid}) to stop. If it is still running in 5 seconds, Wake offers to force it."),
        StopHow::Kill => format!("Forced “{task}” (PID {pid}) to end."),
    }
}

/// Stop all stale: which ones were asked to stop (the first few by name), then each that wasn't,
/// on a line of its own.
fn stopped_all_words(names: &[(u32, String)], results: &[(u32, ActionResult)]) -> String {
    let name = |pid: u32| names.iter().find(|(p, _)| *p == pid).map_or_else(|| format!("PID {pid}"), |(_, t)| format!("“{}” (PID {pid})", fmt::clip(t, 28)));
    let ok: Vec<u32> = results.iter().filter(|(_, r)| r.ok).map(|(p, _)| *p).collect();
    let mut lines = Vec::new();
    if !ok.is_empty() {
        let mut shown: Vec<String> = ok.iter().take(3).map(|&p| name(p)).collect();
        if ok.len() > 3 {
            shown.push(format!("{} more", ok.len() - 3));
        }
        lines.push(format!("Asked {} to stop: {}.", fmt::pl(ok.len() as u64, "agent", "agents"), shown.join(", ")));
    }
    for (pid, r) in results.iter().filter(|(_, r)| !r.ok) {
        lines.push(if r.message.is_empty() { format!("{} didn't stop.", name(*pid)) } else { r.message.clone() });
    }
    lines.join("\n")
}

fn say(ui: &Ui, r: &ActionResult) {
    let mut msg = r.message.clone();
    if msg.is_empty() {
        msg = if r.ok { "Done.".into() } else { "That didn't work.".into() };
    }
    // A process that ended on its own before the stop is news, not a failure.
    let gone = r.message.contains("already ended");
    app::toast(ui, &msg, !r.ok && !gone);
}

/// Five seconds after a polite stop: anything still running can be forced, with a second
/// confirmation naming each.
/// What was stopped and is still running: the same process, by its PID and its start time, so a
/// new process that was given a reused PID is never offered for Force.
fn still_running(p: &ProcOverview, stopped: &[(u32, Option<u64>)]) -> Vec<AgentProc> {
    p.procs.iter().filter(|a| stopped.contains(&(a.pid, a.start_ticks))).cloned().collect()
}

fn after_five(ui: &Ui, stopped: Vec<(u32, Option<u64>)>) {
    if stopped.is_empty() {
        return;
    }
    let ui = ui.clone();
    glib::timeout_add_local_once(Duration::from_secs(5), move || {
        app::bg(
            &ui,
            |src| src.processes(),
            move |ui, mut p| {
                let alive = still_running(&p, &stopped);
                hold_stale(&mut p, app::unsure(ui));
                ui.st.borrow_mut().procs = Some(p);
                if ui.st.borrow().view == View::Agents {
                    render(ui);
                }
                if alive.is_empty() {
                    return;
                }
                let list: Vec<(String, String)> = alive.iter().map(stop_line).collect();
                let title = if alive.len() == 1 { format!("“{}” is still running", task_of(&alive[0])) } else { format!("{} agents are still running", alive.len()) };
                let body = vec!["It didn't exit after 5 seconds. Force ends it at once: anything it hadn't saved is lost. Its conversation on disk stays.".to_string()];
                let force: Vec<(u32, Option<u64>)> = alive.iter().map(|a| (a.pid, a.start_ticks)).collect();
                dialogs::confirm(ui, &title, &body, &list, "Force", move |ui| {
                    let force = force.clone();
                    app::bg(
                        ui,
                        move |src| force.iter().map(|(pid, ticks)| src.stop(*pid, *ticks, StopHow::Kill, false)).collect::<Vec<_>>(),
                        |ui, results| {
                            let bad: Vec<&ActionResult> = results.iter().filter(|r| !r.ok).collect();
                            match bad.first() {
                                Some(r) => say(ui, r),
                                None => app::toast(ui, &format!("Forced {} to end.", fmt::pl(results.len() as u64, "agent", "agents")), false),
                            }
                            app::refresh_procs(ui);
                            app::refresh(ui);
                        },
                    );
                });
            },
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_toasts_name_what_stopped_and_list_each_refusal() {
        let res = |ok: bool, m: &str| ActionResult { ok, message: m.into(), steps: vec![] };
        let names = vec![(1, "fix the login page".to_string()), (2, "spare".to_string()), (3, "old".to_string())];
        let results =
            vec![(1, res(true, "Wake sent the polite stop signal (SIGTERM) to PID 1.")), (2, res(false, "Could not stop PID 2: not permitted.")), (3, res(false, "PID 3 had already ended."))];
        let msg = stopped_all_words(&names, &results);
        assert_eq!(msg, "Asked 1 agent to stop: “fix the login page” (PID 1).\nCould not stop PID 2: not permitted.\nPID 3 had already ended.");
        for how in [StopHow::AgentStop, StopHow::Terminate, StopHow::Kill] {
            assert!(!stopped_words("x", 9, how).contains("SIG"));
        }
        let many: Vec<(u32, ActionResult)> = (1..=5).map(|p| (p, res(true, ""))).collect();
        assert!(stopped_all_words(&[], &many).ends_with("PID 3, 2 more."));
    }

    fn a(pid: u32, kind: ProcKind, rss_mb: u64, start: u64, stale: Option<&str>) -> AgentProc {
        AgentProc {
            pid,
            ppid: 1,
            kind,
            agent: Some(Agent::Claude),
            command: "claude".into(),
            session_id: None,
            job_id: None,
            title: Some(format!("task-{pid}")),
            project_name: Some("wake".into()),
            state: State::Idle,
            idle_seconds: Some(31 * 3600),
            rss_bytes: rss_mb << 20,
            cpu_percent: None,
            started_at: None,
            location: None,
            note: None,
            stale_reason: stale.map(String::from),
            children: vec![],
            start_ticks: Some(start),
            helpers: None,
        }
    }

    #[test]
    fn force_is_offered_only_for_the_same_process() {
        let p = ProcOverview { procs: vec![a(10, ProcKind::Session, 100, 300, None), a(20, ProcKind::Session, 100, 999, None)], ..Default::default() };
        // 10 is the one that was stopped; 20's PID was reused by a new process (a later start).
        let alive: Vec<u32> = still_running(&p, &[(10, Some(300)), (20, Some(200)), (30, Some(1))]).iter().map(|a| a.pid).collect();
        assert_eq!(alive, vec![10]);
    }

    #[test]
    fn nothing_looks_stale_while_windows_are_being_found() {
        let mut p = ProcOverview { procs: vec![a(10, ProcKind::Session, 100, 300, Some("idle 31 h, no window"))], ..Default::default() };
        hold_stale(&mut p, false);
        assert_eq!(stale_summary(&p).0, 1);
        hold_stale(&mut p, true);
        assert_eq!(stale_summary(&p).0, 0, "its window may not have been found yet");
        assert!(p.procs[0].stale_reason.is_none());
    }

    fn pids(p: &ProcOverview, stale_only: bool) -> Vec<u32> {
        ordered(p, stale_only).iter().map(|(_, a)| a.pid).collect()
    }

    #[test]
    fn rows_keep_their_order_when_memory_changes() {
        let mut p = ProcOverview { procs: vec![a(30, ProcKind::Session, 100, 300, None), a(20, ProcKind::Session, 900, 200, None), a(10, ProcKind::Daemon, 50, 100, None)], ..Default::default() };
        // Sessions by state, as they started; Claude's daemon in the background group after them.
        assert_eq!(pids(&p, false), [20, 30, 10]);
        p.procs[0].rss_bytes = 5000 << 20;
        assert_eq!(pids(&p, false), [20, 30, 10], "memory never reorders");
    }

    #[test]
    fn strays_and_the_daemon_are_never_counted_or_stopped_as_stale() {
        let p = ProcOverview {
            procs: vec![
                a(1, ProcKind::Daemon, 50, 1, Some("old")),
                a(2, ProcKind::Stray, 500, 2, Some("unknown")),
                a(3, ProcKind::Session, 700, 3, Some("idle 31 h")),
                a(4, ProcKind::Session, 100, 4, None),
            ],
            dead_notes: vec![99],
            ..Default::default()
        };
        assert_eq!(stale_summary(&p), (1, 700 << 20));
        assert_eq!(pids(&p, true), [3]);
        // The chips count the rows they show.
        assert_eq!(row_count(&p, true), 1);
        assert_eq!(row_count(&p, false), 5);
    }

    #[test]
    fn stop_dialogs_say_what_happens_in_plain_words() {
        let mut x = a(5, ProcKind::Session, 743, 5, None);
        x.title = Some("rail-times-import".into());
        x.project_name = Some("waypoint".into());
        assert_eq!(stop_line(&x), ("rail-times-import · waypoint · 743 MB · idle 31 h".to_string(), "PID 5".to_string()));
        x.location = Some(Location { pid: 1, app: Some("Term".into()) });
        assert_eq!(stop_words(&x)[0], "The Claude in its Term window will exit; you can resume it later.");
        x.location = Some(Location { pid: 1, app: None });
        assert_eq!(stop_words(&x)[0], "The Claude in its terminal window will exit; you can resume it later.");
        x.location = Some(Location { pid: 1, app: Some("Term".into()) });
        let words = stop_words(&x).join(" ");
        assert!(!words.contains("SIG") && !words.contains("claude stop"));
        x.job_id = Some("bg-1".into());
        x.location = None;
        assert!(stop_words(&x)[0].contains("background session cleanly"));
    }

    #[test]
    fn tree_rows_are_named_apart_from_sessions() {
        assert_eq!(row_key("43310"), RowKey::Session(43310));
        assert_eq!(row_key("sa-43310-b3"), RowKey::Sub(43310, "b3".into()));
        assert_eq!(row_key("sa-43310-a1b2-c3"), RowKey::Sub(43310, "a1b2-c3".into()), "agent ids may hold dashes");
        assert_eq!(row_key(&sub_name(7, "x")), RowKey::Sub(7, "x".into()));
        assert_eq!(row_key(&fold_name(7)), RowKey::Fold(7));
        assert_eq!(row_key("note-99"), RowKey::Other);
        assert_eq!(row_key("snote-7"), RowKey::Other);
        assert_eq!(row_key("sa-x-b3"), RowKey::Other);
        assert_eq!(row_key(""), RowKey::Other);
        // Never a PID by accident: a subagent's row stops nothing, peeks and goes to its session.
        assert!("sa-7-x".parse::<u32>().is_err() && "sfold-7".parse::<u32>().is_err());
        assert_eq!(session_pid("sa-7-x"), Some(7));
        assert_eq!(session_pid("sfold-7"), Some(7));
        assert_eq!(session_pid("7"), Some(7));
        assert_eq!(session_pid("note-7"), None);
    }

    #[test]
    fn running_first_then_the_fold_then_the_finished_on_their_own_line() {
        use Item::*;
        use Link::*;
        assert_eq!(plan(2, 0, false), [(Run(0), Mid), (Run(1), Last)]);
        assert_eq!(plan(2, 3, false), [(Run(0), Mid), (Run(1), Mid), (Fold, Last)], "closed: no finished rows at all");
        assert_eq!(plan(1, 2, true), [(Run(0), Mid), (Fold, FoldOpen), (Done(0), InnerMid), (Done(1), InnerLast)]);
        assert_eq!(plan(0, 1, true), [(Fold, FoldOpen), (Done(0), InnerLast)]);
        assert!(plan(0, 0, true).is_empty());
    }

    #[test]
    fn keys_walk_into_and_out_of_the_tree() {
        use TreeKey::*;
        let (s, sub, fold) = (RowKey::Session(1), RowKey::Sub(1, "a".into()), RowKey::Fold(1));
        assert_eq!(act(&s, Enter, false), Act::Panel);
        assert_eq!(act(&s, Right, false), Act::Subs(true));
        assert_eq!(act(&s, Left, true), Act::Subs(false), "← on a session closes its subagents");
        assert_eq!(act(&sub, Enter, false), Act::Steps(true));
        assert_eq!(act(&sub, Enter, true), Act::Steps(false));
        assert_eq!(act(&sub, Right, false), Act::Steps(true));
        assert_eq!(act(&sub, Right, true), Act::Nothing);
        assert_eq!(act(&sub, Left, true), Act::Steps(false));
        assert_eq!(act(&sub, Left, false), Act::ToSession);
        assert_eq!(act(&fold, Enter, false), Act::Fold(true));
        assert_eq!(act(&fold, Right, false), Act::Fold(true));
        assert_eq!(act(&fold, Left, true), Act::Fold(false));
        assert_eq!(act(&fold, Left, false), Act::ToSession);
        assert_eq!(act(&RowKey::Other, Enter, false), Act::Nothing);
    }

    #[test]
    fn a_selected_row_that_goes_leaves_the_selection_on_a_neighbour() {
        let v = |x: &[&str]| x.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let old = v(&["1", "sa-1-a", "sa-1-b", "sfold-1"]);
        // "a" finished into the closed fold: the next running one.
        assert_eq!(fallback(&old, &v(&["1", "sa-1-b", "sfold-1"]), "sa-1-a", "1"), "sa-1-b");
        // The last running one finished: the fold it went into.
        assert_eq!(fallback(&old, &v(&["1", "sa-1-a", "sfold-1"]), "sa-1-b", "1"), "sfold-1");
        // The fold closed under a finished row: back to the fold.
        let open = v(&["1", "sfold-1", "sa-1-x", "sa-1-y"]);
        assert_eq!(fallback(&open, &v(&["1", "sfold-1"]), "sa-1-y", "1"), "sfold-1");
        // Everything closed: the session.
        assert_eq!(fallback(&old, &v(&["1"]), "sa-1-a", "1"), "1");
        assert_eq!(fallback(&old, &v(&["1"]), "unknown", "1"), "1");
    }

    #[test]
    fn subagents_follow_their_session_and_are_never_stale() {
        let mut parent = a(10, ProcKind::Session, 700, 10, Some("idle 31 h"));
        parent.children = vec![11, 12, 13];
        let mut host = a(11, ProcKind::PtyHost, 40, 11, None);
        host.ppid = 10;
        let mut s1 = a(12, ProcKind::Subagent, 200, 40, None);
        s1.ppid = 10;
        let s2 = a(13, ProcKind::Subagent, 100, 30, None);
        let p = ProcOverview { procs: vec![a(20, ProcKind::Session, 100, 5, None), parent, host, s1, s2], ..Default::default() };
        assert_eq!(pids(&p, false), [10, 13, 12, 20], "the stale session first, its subagents right after it as they started");
        assert_eq!(pids(&p, true), [10]);
        assert_eq!(stale_summary(&p), (1, 700 << 20));
        let mut x = a(14, ProcKind::Subagent, 1, 1, Some("x"));
        assert!(!is_stale(&x));
        x.helpers = Some(vec!["live".into()]);
        assert_eq!(live_helpers(&x), ["live"]);
    }
}
