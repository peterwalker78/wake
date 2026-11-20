//! Looking at an agent's window from Wake (hold-to-peek), and reading its screen.
//!
//! Three ways to look, picked at run time by [`detect`]:
//! - **Slipstream** (a Wayland desktop, github.com/peterwalker78/slipstream-desktop) serves its Look
//!   request: it flies to the window's workspace with its own animation and rings the window, while
//!   the keyboard stays with Wake.
//! - **KDE Plasma (KWin)**: a small generated KWin script switches to the window's virtual desktop
//!   (KWin's normal animated switch) and highlights it with the HighlightWindow effect, keeping Wake
//!   active so the key release still reaches it. UNTESTED ON A REAL PLASMA SESSION: written from the
//!   KWin 6 scripting API and the effect's source (see [`kwin_start_script`]), checked only with fakes.
//! - **Preview** everywhere else and for agents with no window (background sessions): the app shows
//!   the agent's terminal text itself, from [`Screen`]s read here.
//!
//! Every D-Bus call goes through the injectable [`Runner`], so tests never touch the desktop, and
//! every call that changes the desktop takes `dry_run` (the steps are returned, nothing is run).

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::desktop::{GDBUS, QDBUS, Runner, TIMEOUT};
use crate::types::{ActionResult, Location, LookKind, Screen};
use crate::util::{self, shell_join};

pub const SLIPSTREAM_NAME: &str = "io.github.peterwalker78.Slipstream";
pub const SLIPSTREAM_PATH: &str = "/io/github/peterwalker78/Slipstream";
pub const SLIPSTREAM_LOOK: &str = "io.github.peterwalker78.Slipstream.Look";
pub const KWIN_NAME: &str = "org.kde.KWin";

/// Reading a Konsole tab's text gives up after this long; `claude logs` after the longer one.
pub const KONSOLE_SCREEN_TIMEOUT: Duration = Duration::from_millis(700);
pub const LOGS_TIMEOUT: Duration = Duration::from_millis(2500);
pub const MAX_LINES: usize = 400;
/// Two polls this close together share one read.
pub const SCREEN_CACHE: Duration = Duration::from_millis(250);
pub const NO_SCREEN: &str = "Couldn't read the screen right now.";

// ---------- which link ----------

fn name_has_owner(run: &Runner, name: &str) -> bool {
    let argv: Vec<String> = [GDBUS, "call", "--session", "--dest", "org.freedesktop.DBus", "--object-path", "/org/freedesktop/DBus", "--method", "org.freedesktop.DBus.NameHasOwner", name]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let (code, out) = run(&argv, TIMEOUT);
    code == 0 && out.trim() == "(true,)"
}

/// Which way of looking this desktop offers. `current_desktop` is `$XDG_CURRENT_DESKTOP`.
pub fn detect(run: &Runner, current_desktop: &str) -> LookKind {
    if name_has_owner(run, SLIPSTREAM_NAME) {
        return LookKind::Slipstream;
    }
    let kde = current_desktop.split(':').any(|d| d.eq_ignore_ascii_case("KDE"));
    if kde && name_has_owner(run, KWIN_NAME) {
        return LookKind::Kwin;
    }
    LookKind::Preview
}

/// The exact `setCurrentSession` call that selects an agent's Konsole tab before looking at it.
fn select_tab_argv(loc: &Location) -> Vec<String> {
    vec![QDBUS.into(), loc.service.clone(), format!("/Windows/{}", loc.window), "setCurrentSession".into(), loc.session.to_string()]
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// A look in progress, as each link needs to end it.
#[derive(Default)]
struct Looking {
    /// Slipstream: the `gdbus monitor` listening for Ended.
    monitor: Option<Child>,
    /// KWin: the virtual desktop that was current at the start (to go back to).
    desktop: Option<String>,
    /// KWin: the pid whose window is being looked at (activated on "stay").
    target: Option<u32>,
}

/// Starts and ends looks for one desktop. Cheap to share.
pub struct Look {
    run: Runner,
    kind: Mutex<Option<LookKind>>,
    current_desktop: String,
    /// Where generated KWin scripts are written (Wake's own data folder).
    script_dir: PathBuf,
    /// Wake's own pid: KWin keeps this window active during a look.
    wake_pid: u32,
    /// Listen for Slipstream's Ended signal with a `gdbus monitor` child (off in tests).
    watch: bool,
    looking: Mutex<Looking>,
    /// Set by the monitor thread when Slipstream says the look ended: Some(stayed).
    ended: Arc<Mutex<Option<bool>>>,
    /// Bumped on each start, so an old monitor's late line can't end a newer look.
    epoch: Arc<AtomicU64>,
}

impl Drop for Look {
    fn drop(&mut self) {
        if let Some(mut c) = self.looking.lock().unwrap_or_else(|e| e.into_inner()).monitor.take() {
            let _ = c.kill(); // our own gdbus monitor, never an agent
            let _ = c.wait();
        }
    }
}

impl Look {
    pub fn new(run: Runner, current_desktop: &str, script_dir: PathBuf, wake_pid: u32, watch: bool) -> Look {
        Look {
            run,
            kind: Mutex::new(None),
            current_desktop: current_desktop.into(),
            script_dir,
            wake_pid,
            watch,
            looking: Mutex::new(Looking::default()),
            ended: Arc::new(Mutex::new(None)),
            epoch: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The way of looking in use, found once (and again after a failure, see `recheck`).
    pub fn kind(&self) -> LookKind {
        let mut k = self.kind.lock().unwrap_or_else(|e| e.into_inner());
        *k.get_or_insert_with(|| detect(&self.run, &self.current_desktop))
    }

    pub fn recheck(&self) {
        *self.kind.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Fly to the window showing `loc`. ok=false (with a sentence) means "use the preview instead".
    pub fn start(&self, loc: Option<&Location>, dry_run: bool) -> ActionResult {
        let fail = |m: &str, steps: Vec<String>| ActionResult { ok: false, message: m.into(), steps };
        let kind = self.kind();
        if kind == LookKind::Preview {
            return fail("This desktop can't fly to a window; showing a preview.", vec![]);
        }
        let Some(loc) = loc else {
            return fail("This agent has no window; showing a preview.", vec![]);
        };
        self.end_monitor();
        *self.ended.lock().unwrap_or_else(|e| e.into_inner()) = None;
        // A window with no Konsole tab to pick first (an empty service) goes straight to the look.
        let select = (!loc.service.is_empty()).then(|| select_tab_argv(loc));
        let mut steps: Vec<String> = select.iter().map(|s| shell_join(s)).collect();
        match kind {
            LookKind::Slipstream => {
                let mut start = argv(&[GDBUS, "call", "--session", "--dest", SLIPSTREAM_NAME, "--object-path", SLIPSTREAM_PATH, "--method"]);
                start.push(format!("{SLIPSTREAM_LOOK}.Start"));
                start.push(loc.konsole_pid.to_string());
                steps.push(shell_join(&start));
                if dry_run {
                    return ActionResult { ok: true, message: "Dry run.".into(), steps };
                }
                // The tab first, so the window Slipstream flies to shows the agent.
                if let Some(select) = &select {
                    let _ = (self.run)(select, TIMEOUT);
                }
                let (code, out) = (self.run)(&start, TIMEOUT);
                let (ok, message) = parse_start_reply(&out);
                if code != 0 || !ok {
                    self.recheck_if_gone(code);
                    return fail(if message.is_empty() { "Slipstream didn't answer." } else { &message }, steps);
                }
                if self.watch {
                    self.start_monitor();
                }
                ActionResult { ok: true, message, steps }
            }
            LookKind::Kwin => {
                let current = argv(&[QDBUS, KWIN_NAME, "/VirtualDesktopManager", "org.kde.KWin.VirtualDesktopManager.current"]);
                steps.push(shell_join(&current));
                let script = self.script_dir.join("look-start.js");
                let load = kwin_load_argv(&script, "wake-look-start");
                steps.push(format!("write {}", script.display()));
                steps.extend(load.iter().map(|a| shell_join(a)));
                if dry_run {
                    return ActionResult { ok: true, message: "Dry run.".into(), steps };
                }
                if let Some(select) = &select {
                    let _ = (self.run)(select, TIMEOUT);
                }
                let (code, desk) = (self.run)(&current, TIMEOUT);
                let desk = desk.trim().to_string();
                if code != 0 || !util_id_ok(&desk) {
                    self.recheck_if_gone(code);
                    return fail("KWin didn't say which desktop is current.", steps);
                }
                if let Err(e) = self.write_script(&script, &kwin_start_script(loc.konsole_pid, self.wake_pid)) {
                    return fail(&e, steps);
                }
                if !self.run_kwin_script(&load) {
                    return fail("KWin didn't run Wake's script.", steps);
                }
                let mut l = self.looking.lock().unwrap_or_else(|e| e.into_inner());
                l.desktop = Some(desk);
                l.target = Some(loc.konsole_pid);
                ActionResult { ok: true, message: String::new(), steps }
            }
            LookKind::Preview => unreachable!(),
        }
    }

    /// Go straight to the window holding `konsole_pid` (the tab already selected): Slipstream's
    /// Look then End(stay), or a KWin script that activates it. ok=false on a desktop with no link
    /// (the caller then just says where the tab is). Never a notification.
    pub fn bring(&self, konsole_pid: u32, dry_run: bool) -> ActionResult {
        let none = |m: &str| ActionResult { ok: false, message: m.into(), steps: vec![] };
        match self.kind() {
            LookKind::Preview => none(""),
            LookKind::Slipstream => {
                let call = |method: &str, arg: String| {
                    let mut a = argv(&[GDBUS, "call", "--session", "--dest", SLIPSTREAM_NAME, "--object-path", SLIPSTREAM_PATH, "--method"]);
                    a.push(format!("{SLIPSTREAM_LOOK}.{method}"));
                    a.push(arg);
                    a
                };
                let start = call("Start", konsole_pid.to_string());
                let end = call("End", "true".into());
                let steps = vec![shell_join(&start), shell_join(&end)];
                if dry_run {
                    return ActionResult { ok: true, message: "Dry run.".into(), steps };
                }
                let (code, out) = (self.run)(&start, TIMEOUT);
                let (ok, message) = parse_start_reply(&out);
                if code != 0 || !ok {
                    self.recheck_if_gone(code);
                    return ActionResult { ok: false, message, steps };
                }
                let (code, out) = (self.run)(&end, TIMEOUT);
                ActionResult { ok: code == 0 && out.contains("true"), message: String::new(), steps }
            }
            LookKind::Kwin => {
                let script = self.script_dir.join("bring.js");
                let load = kwin_load_argv(&script, "wake-bring");
                let mut steps = vec![format!("write {}", script.display())];
                steps.extend(load.iter().map(|a| shell_join(a)));
                if dry_run {
                    return ActionResult { ok: true, message: "Dry run.".into(), steps };
                }
                let ok = self.write_script(&script, &kwin_bring_script(konsole_pid)).is_ok() && self.run_kwin_script(&load);
                ActionResult { ok, message: String::new(), steps }
            }
        }
    }

    /// End a look: back to Wake, or (`stay`) over to the agent's window.
    pub fn end(&self, stay: bool, dry_run: bool) -> ActionResult {
        let kind = self.kind();
        let epoch_before = self.epoch.load(Ordering::SeqCst);
        let r = match kind {
            LookKind::Preview => ActionResult { ok: true, message: String::new(), steps: vec![] },
            LookKind::Slipstream => {
                let mut end = argv(&[GDBUS, "call", "--session", "--dest", SLIPSTREAM_NAME, "--object-path", SLIPSTREAM_PATH, "--method"]);
                end.push(format!("{SLIPSTREAM_LOOK}.End"));
                end.push(stay.to_string());
                let steps = vec![shell_join(&end)];
                if dry_run {
                    return ActionResult { ok: true, message: "Dry run.".into(), steps };
                }
                let (code, out) = (self.run)(&end, TIMEOUT);
                ActionResult { ok: code == 0 && out.contains("true"), message: String::new(), steps }
            }
            LookKind::Kwin => {
                let (desk, target) = {
                    let mut l = self.looking.lock().unwrap_or_else(|e| e.into_inner());
                    (l.desktop.take(), l.target.take())
                };
                let script = self.script_dir.join("look-end.js");
                let load = kwin_load_argv(&script, "wake-look-end");
                let mut steps = vec![format!("write {}", script.display())];
                steps.extend(load.iter().map(|a| shell_join(a)));
                if dry_run {
                    return ActionResult { ok: true, message: "Dry run.".into(), steps };
                }
                let text = kwin_end_script(desk.as_deref(), target, self.wake_pid, stay);
                let ok = self.write_script(&script, &text).is_ok() && self.run_kwin_script(&load);
                ActionResult { ok, message: String::new(), steps }
            }
        };
        if self.epoch.load(Ordering::SeqCst) == epoch_before {
            self.end_monitor();
        }
        r
    }

    /// Some(stayed) once Slipstream has ended the look by itself; taken (cleared) by reading.
    pub fn ended(&self) -> Option<bool> {
        self.ended.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    fn recheck_if_gone(&self, code: i32) {
        // Not installed, not on the bus or not answering: look again next time.
        if code != 0 {
            self.recheck();
        }
    }

    fn write_script(&self, path: &std::path::Path, text: &str) -> Result<(), String> {
        std::fs::create_dir_all(&self.script_dir).map_err(|_| "Wake couldn't write its KWin script.".to_string())?;
        std::fs::write(path, text).map_err(|_| "Wake couldn't write its KWin script.".to_string())
    }

    /// Unload any earlier copy, load the script, run it. Each step through the runner.
    fn run_kwin_script(&self, calls: &[Vec<String>]) -> bool {
        let [unload, load, run_tpl] = calls else { return false };
        let _ = (self.run)(unload, TIMEOUT);
        let (code, out) = (self.run)(load, TIMEOUT);
        let Some(id) = (code == 0).then(|| out.trim().parse::<i64>().ok()).flatten().filter(|id| *id >= 0) else { return false };
        let mut run = run_tpl.clone();
        run[2] = format!("/Scripting/Script{id}");
        (self.run)(&run, TIMEOUT).0 == 0
    }

    fn start_monitor(&self) {
        let epoch = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        let mut cmd = Command::new(GDBUS);
        cmd.args(["monitor", "--session", "--dest", SLIPSTREAM_NAME, "--object-path", SLIPSTREAM_PATH]);
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
        let Ok(mut child) = cmd.spawn() else { return };
        let out = child.stdout.take();
        let ended = self.ended.clone();
        let epochs = self.epoch.clone();
        let _ = std::thread::Builder::new().name("wake-look-watch".into()).spawn(move || {
            let Some(out) = out else { return };
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if let Some(stayed) = parse_ended(&line) {
                    if epochs.load(Ordering::SeqCst) == epoch {
                        *ended.lock().unwrap_or_else(|e| e.into_inner()) = Some(stayed);
                    }
                    return;
                }
            }
        });
        self.looking.lock().unwrap_or_else(|e| e.into_inner()).monitor = Some(child);
    }

    fn end_monitor(&self) {
        if let Some(mut c) = self.looking.lock().unwrap_or_else(|e| e.into_inner()).monitor.take() {
            let _ = c.kill(); // our own gdbus monitor
            let _ = c.wait();
        }
    }
}

/// KWin's desktop ids are UUID-like; anything else is not passed into a script.
fn util_id_ok(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || "-{}_".contains(c))
}

/// `(true, 'Looking at Konsole')` from gdbus → (true, "Looking at Konsole").
pub fn parse_start_reply(out: &str) -> (bool, String) {
    let t = out.trim().trim_start_matches('(').trim_end_matches(')').trim_end_matches(',');
    let (ok, rest) = t.split_once(',').unwrap_or((t, ""));
    let msg = rest.trim().trim_matches(|c| c == '\'' || c == '"').replace("\\'", "'");
    (ok.trim() == "true", msg)
}

/// A `gdbus monitor` line announcing Slipstream's Ended signal → Some(stayed).
pub fn parse_ended(line: &str) -> Option<bool> {
    let at = line.find(&format!("{SLIPSTREAM_LOOK}.Ended"))?;
    let args = &line[at..];
    Some(args.contains("(true"))
}

/// unloadScript, loadScript (returns the id), and the run call (its path filled in with the id).
/// org.kde.kwin.Scripting at /Scripting on org.kde.KWin; each loaded script is /Scripting/Script<id>
/// with org.kde.kwin.Script.run (KWin 6, src/scripting/scripting.cpp).
fn kwin_load_argv(script: &std::path::Path, plugin: &str) -> Vec<Vec<String>> {
    vec![
        argv(&[QDBUS, KWIN_NAME, "/Scripting", "org.kde.kwin.Scripting.unloadScript", plugin]),
        vec![QDBUS.into(), KWIN_NAME.into(), "/Scripting".into(), "org.kde.kwin.Scripting.loadScript".into(), script.display().to_string(), plugin.into()],
        argv(&[QDBUS, KWIN_NAME, "/Scripting/Script<id>", "org.kde.kwin.Script.run"]),
    ]
}

/// The KWin script that flies to the window holding `target_pid` and highlights it.
///
/// Written against the KWin 6 scripting API (https://develop.kde.org/docs/plasma/kwin/api/):
/// `workspace.windowList()`, `workspace.currentDesktop` (read-write VirtualDesktop),
/// `workspace.activeWindow` (read-write), `workspace.raiseWindow(window)`, and Window's `pid`,
/// `internalId` (QUuid), `desktops`, `onAllDesktops`, `minimized`, `normalWindow`; and the global
/// `callDBus(service, path, interface, method, args...)`. The highlight is the HighlightWindow effect
/// Plasma's task manager uses: service org.kde.KWin.HighlightWindow, object
/// /org/kde/KWin/HighlightWindow, method highlightWindows(QStringList of window UUIDs)
/// (kwin src/plugins/highlightwindow/highlightwindow.cpp). Wake is pinned to every desktop for the
/// look, so it stays the active window and the key release reaches it.
/// UNTESTED ON A REAL PLASMA SESSION.
pub fn kwin_start_script(target_pid: u32, wake_pid: u32) -> String {
    format!(
        r#"// Written by Wake (hold-to-peek): fly to the window holding process {target_pid}. Read-only otherwise.
(function () {{
    const all = workspace.windowList ? workspace.windowList() : workspace.stackingOrder;
    const target = all.find(w => w.pid === {target_pid} && w.normalWindow);
    const wake = all.find(w => w.pid === {wake_pid} && w.normalWindow);
    if (!target) return;
    if (wake && !wake.onAllDesktops) wake.onAllDesktops = true;
    if (target.minimized) target.minimized = false;
    if (!target.onAllDesktops && target.desktops.length && target.desktops.indexOf(workspace.currentDesktop) < 0) {{
        workspace.currentDesktop = target.desktops[0];
    }}
    workspace.raiseWindow(target);
    if (wake) workspace.activeWindow = wake;
    callDBus("org.kde.KWin.HighlightWindow", "/org/kde/KWin/HighlightWindow", "org.kde.KWin.HighlightWindow",
             "highlightWindows", [target.internalId.toString()]);
}})();
"#
    )
}

/// The KWin script that ends a look: back to `desktop` with Wake active, or (`stay`) activate the
/// agent's window. Unpins Wake from every desktop. UNTESTED ON A REAL PLASMA SESSION.
pub fn kwin_end_script(desktop: Option<&str>, target_pid: Option<u32>, wake_pid: u32, stay: bool) -> String {
    let desk = desktop.filter(|d| util_id_ok(d)).unwrap_or("");
    let target_pid = target_pid.map_or(-1, i64::from);
    format!(
        r#"// Written by Wake (hold-to-peek): end the look{}.
(function () {{
    callDBus("org.kde.KWin.HighlightWindow", "/org/kde/KWin/HighlightWindow", "org.kde.KWin.HighlightWindow",
             "highlightWindows", []);
    const all = workspace.windowList ? workspace.windowList() : workspace.stackingOrder;
    const wake = all.find(w => w.pid === {wake_pid} && w.normalWindow);
    const stay = {stay};
    if (stay) {{
        const target = all.find(w => w.pid === {target_pid} && w.normalWindow);
        if (target) workspace.activeWindow = target;
        if (wake) wake.onAllDesktops = false;
        return;
    }}
    const back = workspace.desktops.find(d => d.id === "{desk}");
    if (back) workspace.currentDesktop = back;
    if (wake) {{
        wake.onAllDesktops = false;
        if (back) wake.desktops = [back];
        workspace.activeWindow = wake;
    }}
}})();
"#,
        if stay { ", staying on the agent's window" } else { ", back to Wake" }
    )
}

/// The KWin script for "Go to": switch to the desktop of the window holding `target_pid` (KWin's
/// animated switch), unminimise it and make it the active window. Same API notes as
/// [`kwin_start_script`]. UNTESTED ON A REAL PLASMA SESSION.
pub fn kwin_bring_script(target_pid: u32) -> String {
    format!(
        r#"// Written by Wake ("Go to"): bring forward the window holding process {target_pid}.
(function () {{
    const all = workspace.windowList ? workspace.windowList() : workspace.stackingOrder;
    const target = all.find(w => w.pid === {target_pid} && w.normalWindow);
    if (!target) return;
    if (target.minimized) target.minimized = false;
    if (!target.onAllDesktops && target.desktops.length && target.desktops.indexOf(workspace.currentDesktop) < 0) {{
        workspace.currentDesktop = target.desktops[0];
    }}
    workspace.activeWindow = target;
}})();
"#
    )
}

// ---------- reading the screen ----------

/// Text without terminal escape sequences: CSI (`ESC [ … final`), OSC (`ESC ] … BEL` or `ESC \`),
/// two-byte escapes, carriage-return overwrites (the text after the last `\r` of a line wins) and
/// other control characters (tabs kept).
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\u{1b}' => match it.next() {
                Some('[') => {
                    for d in it.by_ref() {
                        if ('@'..='~').contains(&d) {
                            break;
                        }
                    }
                }
                Some(']') | Some('P') | Some('_') | Some('^') => {
                    while let Some(d) = it.next() {
                        if d == '\u{7}' {
                            break;
                        }
                        if d == '\u{1b}' && it.peek() == Some(&'\\') {
                            it.next();
                            break;
                        }
                    }
                }
                Some('(') | Some(')') => {
                    it.next();
                }
                _ => {}
            },
            '\u{9b}' => {
                for d in it.by_ref() {
                    if ('@'..='~').contains(&d) {
                        break;
                    }
                }
            }
            '\r' if it.peek() == Some(&'\n') => {}
            '\r' => {
                // Overwrite: drop what this line had so far.
                let start = out.rfind('\n').map_or(0, |i| i + 1);
                out.truncate(start);
            }
            '\n' | '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// The last `max` lines, right-trimmed, with blank lines at the end dropped.
pub fn last_lines(text: &str, max: usize) -> Vec<String> {
    let mut lines: Vec<String> = text.lines().map(|l| l.trim_end().to_string()).collect();
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    let skip = lines.len().saturating_sub(max);
    lines.split_off(skip)
}

/// What a Konsole tab shows: `getAllDisplayedText` (read-only) on its session.
pub fn read_konsole(run: &Runner, loc: &Location) -> Screen {
    let source = loc.label.clone().unwrap_or_else(|| format!("Konsole {} · tab {}", loc.window, loc.tab_index + 1));
    let argv = vec![QDBUS.into(), loc.service.clone(), format!("/Sessions/{}", loc.session), "getAllDisplayedText".into(), "true".into()];
    let (code, out) = run(&argv, KONSOLE_SCREEN_TIMEOUT);
    screen_from(code, &out, source)
}

/// What a background session's terminal showed lately: `claude logs <jobId>` (read-only).
pub fn read_logs(run: &Runner, job: &str) -> Screen {
    if job.is_empty() || job.len() > 64 || !job.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Screen { source: "claude logs".into(), at: util::now_iso(), note: Some(NO_SCREEN.into()), ..Default::default() };
    }
    let argv = vec!["claude".to_string(), "logs".into(), job.into()];
    let (code, out) = run(&argv, LOGS_TIMEOUT);
    screen_from(code, &out, "claude logs".into())
}

fn screen_from(code: i32, out: &str, source: String) -> Screen {
    let at = util::now_iso();
    if code != 0 {
        return Screen { lines: vec![], source, at, note: Some(NO_SCREEN.into()) };
    }
    Screen { lines: last_lines(&strip_ansi(out), MAX_LINES), source, at, note: None }
}

/// Screens read in the last 250 ms, by session, so overlapping polls don't run a command twice.
#[derive(Default)]
pub struct ScreenCache {
    map: Mutex<HashMap<String, (Instant, Screen)>>,
}

impl ScreenCache {
    pub fn get_or(&self, session: &str, read: impl FnOnce() -> Screen) -> Screen {
        {
            let m = self.map.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((at, s)) = m.get(session)
                && at.elapsed() < SCREEN_CACHE
            {
                return s.clone();
            }
        }
        let s = read();
        let mut m = self.map.lock().unwrap_or_else(|e| e.into_inner());
        m.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(10));
        m.insert(session.into(), (Instant::now(), s.clone()));
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Calls = Arc<Mutex<Vec<Vec<String>>>>;

    /// A runner answering from `answer`, recording every call; never runs anything.
    fn fake(answer: impl Fn(&[String]) -> (i32, String) + Send + Sync + 'static) -> (Runner, Calls) {
        let calls: Calls = Arc::new(Mutex::new(vec![]));
        let c = calls.clone();
        (
            Arc::new(move |argv: &[String], _t: Duration| {
                c.lock().unwrap().push(argv.to_vec());
                answer(argv)
            }),
            calls,
        )
    }

    fn loc() -> Location {
        Location { konsole_pid: 3264, service: "org.kde.konsole-3264".into(), window: 2, session: 7, tab_index: 0, tab_title: "claude".into(), label: Some("Konsole 2 · tab 1".into()) }
    }

    fn has_owner(names: &'static [&'static str]) -> impl Fn(&[String]) -> (i32, String) + Send + Sync {
        move |a: &[String]| {
            if a.iter().any(|x| x == "org.freedesktop.DBus.NameHasOwner") {
                let n = a.last().unwrap();
                return (0, format!("({},)\n", names.contains(&n.as_str())));
            }
            (1, String::new())
        }
    }

    #[test]
    fn detect_prefers_slipstream_then_kwin_on_kde_then_preview() {
        let (r, _) = fake(has_owner(&[SLIPSTREAM_NAME, KWIN_NAME]));
        assert_eq!(detect(&r, "KDE"), LookKind::Slipstream);
        let (r, _) = fake(has_owner(&[KWIN_NAME]));
        assert_eq!(detect(&r, "KDE"), LookKind::Kwin);
        assert_eq!(detect(&r, "GNOME"), LookKind::Preview, "KWin on the bus but not a KDE session");
        let (r, _) = fake(has_owner(&[]));
        assert_eq!(detect(&r, "KDE"), LookKind::Preview);
        let (r, _) = fake(|_| (127, String::new()));
        assert_eq!(detect(&r, "KDE"), LookKind::Preview, "no gdbus at all");
    }

    #[test]
    fn slipstream_look_selects_the_tab_then_starts_and_ends() {
        let (r, calls) = fake(|a| {
            if a.iter().any(|x| x == "org.freedesktop.DBus.NameHasOwner") {
                return (0, "(true,)\n".into());
            }
            if a.iter().any(|x| x.ends_with(".Look.Start")) {
                return (0, "(true, 'Looking')\n".into());
            }
            if a.iter().any(|x| x.ends_with(".Look.End")) {
                return (0, "(true,)\n".into());
            }
            (0, String::new())
        });
        let look = Look::new(r, "Slipstream", std::env::temp_dir(), 99, false);
        let dry = look.start(Some(&loc()), true);
        assert!(dry.ok);
        assert_eq!(dry.steps[0], "qdbus-qt6 org.kde.konsole-3264 /Windows/2 setCurrentSession 7");
        assert!(dry.steps[1].ends_with("--method io.github.peterwalker78.Slipstream.Look.Start 3264"), "{}", dry.steps[1]);
        assert_eq!(calls.lock().unwrap().len(), 1, "a dry run only asks who is on the bus");
        let r = look.start(Some(&loc()), false);
        assert!(r.ok, "{}", r.message);
        let end = look.end(true, false);
        assert!(end.ok);
        let calls = calls.lock().unwrap();
        let methods: Vec<&str> = calls.iter().map(|c| c.iter().rev().nth(1).map(String::as_str).unwrap_or("")).collect();
        assert!(calls[1].contains(&"setCurrentSession".to_string()));
        assert!(calls[2].iter().any(|x| x.ends_with(".Look.Start")));
        assert!(calls[3].iter().any(|x| x.ends_with(".Look.End")) && calls[3].last().unwrap() == "true", "{methods:?}");
    }

    #[test]
    fn a_refused_look_or_no_window_falls_back_to_the_preview() {
        let (r, _) = fake(|a| {
            if a.iter().any(|x| x == "org.freedesktop.DBus.NameHasOwner") {
                return (0, "(true,)\n".into());
            }
            if a.iter().any(|x| x.ends_with(".Look.Start")) {
                return (0, "(false, 'The screen is locked.')\n".into());
            }
            (0, String::new())
        });
        let look = Look::new(r, "", std::env::temp_dir(), 99, false);
        let x = look.start(None, false);
        assert!(!x.ok && x.message.contains("no window"));
        let x = look.start(Some(&loc()), false);
        assert!(!x.ok);
        assert_eq!(x.message, "The screen is locked.");
        let (r, _) = fake(has_owner(&[]));
        let look = Look::new(r, "", std::env::temp_dir(), 99, false);
        assert_eq!(look.kind(), LookKind::Preview);
        assert!(!look.start(Some(&loc()), false).ok);
    }

    #[test]
    fn kwin_look_writes_and_runs_its_scripts() {
        let dir = std::env::temp_dir().join(format!("wake-look-{}", std::process::id()));
        let (r, calls) = fake(|a| {
            if a.iter().any(|x| x == "org.freedesktop.DBus.NameHasOwner") {
                return (0, if a.last().unwrap() == KWIN_NAME { "(true,)\n" } else { "(false,)\n" }.into());
            }
            if a.iter().any(|x| x.ends_with("VirtualDesktopManager.current")) {
                return (0, "a1b2-c3\n".into());
            }
            if a.iter().any(|x| x.ends_with("loadScript")) && !a.iter().any(|x| x.ends_with("unloadScript")) {
                return (0, "5\n".into());
            }
            (0, String::new())
        });
        let look = Look::new(r, "KDE", dir.clone(), 4242, false);
        assert_eq!(look.kind(), LookKind::Kwin);
        assert!(look.start(Some(&loc()), false).ok);
        let start = std::fs::read_to_string(dir.join("look-start.js")).unwrap();
        assert!(start.contains("w.pid === 3264") && start.contains("w.pid === 4242") && start.contains("highlightWindows"));
        assert!(look.end(false, false).ok);
        let end = std::fs::read_to_string(dir.join("look-end.js")).unwrap();
        assert!(end.contains(r#"d.id === "a1b2-c3""#), "goes back to the desktop it came from");
        let calls = calls.lock().unwrap();
        assert!(calls.iter().any(|c| c[2] == "/Scripting/Script5" && c[3] == "org.kde.kwin.Script.run"));
        assert!(!calls.iter().any(|c| c.iter().any(|x| x == "sendText" || x == "newSession")));
        let _ = std::fs::remove_dir_all(&dir);
        // Odd desktop ids never reach a script.
        assert!(kwin_end_script(Some("x\"; evil()"), None, 1, false).contains(r#"d.id === """#));
    }

    #[test]
    fn bring_goes_straight_to_the_window_without_a_notification() {
        let (r, calls) = fake(|a| {
            if a.iter().any(|x| x == "org.freedesktop.DBus.NameHasOwner") {
                return (0, "(true,)\n".into());
            }
            (0, if a.iter().any(|x| x.ends_with(".Look.Start")) { "(true, '')\n" } else { "(true,)\n" }.into())
        });
        let look = Look::new(r, "", std::env::temp_dir(), 1, false);
        assert!(look.bring(3264, false).ok);
        let calls = calls.lock().unwrap();
        assert!(calls[1].iter().any(|x| x.ends_with(".Look.Start")) && calls[1].last().unwrap() == "3264");
        assert!(calls[2].iter().any(|x| x.ends_with(".Look.End")) && calls[2].last().unwrap() == "true");
        assert!(!calls.iter().any(|c| c.iter().any(|x| x.contains("Notifications"))));
        let (r, _) = fake(has_owner(&[]));
        assert!(!Look::new(r, "", std::env::temp_dir(), 1, false).bring(3264, false).ok, "no link: the caller says where the tab is");
        assert!(kwin_bring_script(77).contains("workspace.activeWindow = target"));
    }

    #[test]
    fn replies_and_signals_parse() {
        assert_eq!(parse_start_reply("(true, 'Looking at Konsole')\n"), (true, "Looking at Konsole".into()));
        assert_eq!(parse_start_reply("(false, 'No window holds that process.')"), (false, "No window holds that process.".into()));
        let line = "/io/github/peterwalker78/Slipstream: io.github.peterwalker78.Slipstream.Look.Ended (true,)";
        assert_eq!(parse_ended(line), Some(true));
        assert_eq!(parse_ended(&line.replace("true", "false")), Some(false));
        assert_eq!(parse_ended("something else"), None);
    }

    #[test]
    fn ansi_is_stripped_and_text_kept() {
        let raw = "\u{1b}[1;32m✓ tests\u{1b}[0m passed\r\n\u{1b}]0;title\u{7}next \u{1b}[2Kline\rover\n\tend\u{1b}(B";
        assert_eq!(strip_ansi(raw), "✓ tests passed\nover\n\tend");
        assert_eq!(last_lines("a\nb  \nc\n\n\n", 2), vec!["b", "c"]);
    }

    #[test]
    fn konsole_screen_is_read_with_the_read_only_call() {
        let (r, calls) = fake(|_| (0, "$ cargo test\n   Compiling wake   \n\n".into()));
        let s = read_konsole(&r, &loc());
        assert_eq!(s.lines, vec!["$ cargo test", "   Compiling wake"]);
        assert_eq!(s.source, "Konsole 2 · tab 1");
        assert_eq!(s.note, None);
        assert_eq!(calls.lock().unwrap()[0], vec!["qdbus-qt6", "org.kde.konsole-3264", "/Sessions/7", "getAllDisplayedText", "true"]);
    }

    #[test]
    fn logs_screen_strips_colour_caps_lines_and_reports_a_timeout() {
        let long: String = (0..600).map(|i| format!("\u{1b}[36mline {i}\u{1b}[0m\n")).collect();
        let (r, calls) = fake(move |_| (0, long.clone()));
        let s = read_logs(&r, "job-1");
        assert_eq!(s.lines.len(), MAX_LINES);
        assert_eq!(s.lines.last().unwrap(), "line 599");
        assert_eq!(s.source, "claude logs");
        assert_eq!(calls.lock().unwrap()[0], vec!["claude", "logs", "job-1"]);
        let (r, _) = fake(|_| (124, String::new()));
        let s = read_logs(&r, "job-1");
        assert_eq!(s.note.as_deref(), Some(NO_SCREEN));
        let (r, calls) = fake(|_| (0, String::new()));
        assert_eq!(read_logs(&r, "bad;id").note.as_deref(), Some(NO_SCREEN));
        assert!(calls.lock().unwrap().is_empty(), "an odd job id is never passed to claude");
    }

    #[test]
    fn overlapping_reads_share_one_command() {
        let cache = ScreenCache::default();
        let n = std::cell::Cell::new(0);
        let read = || {
            n.set(n.get() + 1);
            Screen { source: "x".into(), ..Default::default() }
        };
        cache.get_or("s", read);
        cache.get_or("s", read);
        assert_eq!(n.get(), 1);
        cache.get_or("t", read);
        assert_eq!(n.get(), 2);
    }
}
