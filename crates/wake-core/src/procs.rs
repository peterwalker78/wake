//! The agent process explorer behind the Agents view: which PID is which task, how much memory
//! each holds, and which look stale. Reads `/proc` directly (an injectable root, so tests use a
//! fake tree); stopping goes through an injectable [`Actor`], so tests never signal anything.
//!
//! How Claude's processes look (Claude 2.1.32x):
//! - the daemon: `<versions>/2.1.328 daemon run …`; `comm` is the version string, so processes are
//!   recognised by `/proc/<pid>/exe` and argv, never by `comm`;
//! - per background session a pty host (`claude bg-pty-host --bg-pty-host <sock> …`) whose child is
//!   the session (`claude bg-spare --bg-spare <sock>` once claimed, or `<versions>/x --resume …`);
//! - spares: a session entry with `"spare": true`, or a `bg-spare` process with no entry whose
//!   claim socket is still there (unclaimed);
//! - subagents: an agent process with another agent session above it in the tree, folded into
//!   that session (kind `Subagent`), named by the session's job `fan` entry;
//! - attach clients: `claude attach <jobId>`, usually under agent-notify in a Konsole tab.
//!
//! Rows come out as a tree in order: daemons, then each session followed by the rows folded into it
//! (its pty host and attach clients, kind `PtyHost`), then spares (each followed by its pty host),
//! then strays. A session's `rss_bytes` already includes its folded rows, listed in `children`;
//! `ProcOverview::total_rss_bytes` counts each process once.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use crate::live::LiveInfo;
use crate::types::{ActionResult, Agent, AgentProc, Location, ProcKind, ProcOverview, ProjectResources, Resources, SessionResources, State, StopHow, WorkingNote};
use crate::util;

/// Idle this long with no Konsole tab and no attach client counts as stale.
pub const STALE_IDLE: f64 = 6.0 * 3600.0;
/// A spare nobody has claimed for this long counts as stale.
pub const STALE_SPARE: f64 = 3600.0;

pub fn clock_ticks() -> f64 {
    let t = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if t > 0 { t as f64 } else { 100.0 }
}

pub fn my_uid() -> u32 {
    unsafe { libc::getuid() }
}

/// One process as /proc shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    pub uid: u32,
    pub comm: String,
    pub argv: Vec<String>,
    pub exe: Option<String>,
    pub rss_bytes: u64,
    /// utime + stime, clock ticks.
    pub cpu_ticks: u64,
    /// Start time after boot, clock ticks.
    pub start_ticks: u64,
}

pub fn read_cmdline(root: &Path, pid: u32) -> Vec<String> {
    let mut raw = Vec::new();
    if let Ok(f) = fs::File::open(root.join(pid.to_string()).join("cmdline")) {
        let _ = f.take(8192).read_to_end(&mut raw);
    }
    raw.split(|&c| c == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8_lossy(a).into_owned()).collect()
}

fn parse_stat(stat: &str) -> Option<(String, u32, u64, u64)> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let comm = stat.get(open + 1..close)?.to_string();
    let f: Vec<&str> = stat.get(close + 1..)?.split_whitespace().collect();
    let ppid = f.get(1)?.parse().ok()?;
    let ut: u64 = f.get(11)?.parse().ok()?;
    let st: u64 = f.get(12)?.parse().ok()?;
    let start = f.get(19)?.parse().ok()?;
    Some((comm, ppid, ut + st, start))
}

fn rss_from_status(status: &str) -> u64 {
    status.lines().find_map(|l| l.strip_prefix("VmRSS:")).and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok()).map(|kb| kb * 1024).unwrap_or(0)
}

fn uid_from_status(status: &str) -> Option<u32> {
    status.lines().find_map(|l| l.strip_prefix("Uid:")).and_then(|v| v.split_whitespace().next()?.parse().ok())
}

/// Read one process; None if it is gone.
pub fn read_proc(root: &Path, pid: u32) -> Option<ProcInfo> {
    let dir = root.join(pid.to_string());
    let stat = fs::read_to_string(dir.join("stat")).ok()?;
    let (comm, ppid, cpu_ticks, start_ticks) = parse_stat(&stat)?;
    let status = fs::read_to_string(dir.join("status")).unwrap_or_default();
    let uid = uid_from_status(&status).or_else(|| fs::metadata(&dir).ok().map(|m| m.uid())).unwrap_or(u32::MAX);
    Some(ProcInfo {
        pid,
        ppid,
        uid,
        comm,
        argv: read_cmdline(root, pid),
        exe: fs::read_link(dir.join("exe")).ok().map(|p| p.to_string_lossy().trim_end_matches(" (deleted)").to_string()),
        rss_bytes: rss_from_status(&status),
        cpu_ticks,
        start_ticks,
    })
}

fn base(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

/// The npm packages of the three agent CLIs. A path counts only when it lies inside one of them,
/// matched on whole path components, so `~/Projects/gemini-chat/server.js` is not Gemini.
const AGENT_PACKAGES: &[(&str, Agent)] =
    &[("/node_modules/@anthropic-ai/claude-code/", Agent::Claude), ("/node_modules/@openai/codex/", Agent::Codex), ("/node_modules/@google/gemini-cli/", Agent::Gemini)];

fn package_of(path: &str) -> Option<Agent> {
    AGENT_PACKAGES.iter().find(|(p, _)| path.contains(p)).map(|(_, a)| *a)
}

/// A native agent executable: Claude's own install (`…/claude/versions/<version>`), a binary named
/// exactly `claude` or `codex`, Codex's platform build (`codex-x86_64-unknown-linux-musl`), or a
/// binary shipped inside one of the agents' npm packages.
fn native_agent(exe: &str) -> Option<Agent> {
    if !exe.starts_with('/') {
        return None;
    }
    let p = Path::new(exe);
    let name = p.file_name()?.to_string_lossy();
    let parent = p.parent().and_then(Path::file_name).map(|n| n.to_string_lossy());
    let grand = p.parent().and_then(Path::parent).and_then(Path::file_name).map(|n| n.to_string_lossy());
    if parent.as_deref() == Some("versions") && grand.as_deref() == Some("claude") {
        return Some(Agent::Claude);
    }
    if name == "claude" {
        return Some(Agent::Claude);
    }
    if name == "codex" || (name.starts_with("codex-") && name.contains("-linux")) {
        return Some(Agent::Codex);
    }
    package_of(exe)
}

fn is_js_runner(exe: &str) -> bool {
    let b = base(exe);
    b == "node" || b == "nodejs" || b == "bun" || b == "deno" || b.strip_prefix("node").is_some_and(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_digit() || c == '.'))
}

/// Which agent a process is, identified positively from its executable and argv (never from `comm`,
/// never from a word appearing somewhere in a path):
/// - the executable is an agent binary ([`native_agent`]); or
/// - the executable is node/bun/deno and the script it runs (argv's first non-option argument after
///   the runner, with symlinks such as npm's `bin/gemini` followed) lies inside one of the agents'
///   npm packages.
///
/// `argv[0]` stands in for the executable only when `/proc/<pid>/exe` could not be read.
pub fn agent_of(argv: &[String], exe: Option<&str>) -> Option<Agent> {
    let exe = exe.filter(|e| !e.is_empty()).or_else(|| argv.first().map(String::as_str).filter(|a| a.starts_with('/')))?;
    if let Some(a) = native_agent(exe) {
        return Some(a);
    }
    if !is_js_runner(exe) {
        return None;
    }
    let script = argv.iter().skip(1).find(|a| !a.starts_with('-'))?;
    if !script.starts_with('/') {
        return None;
    }
    let resolved = fs::canonicalize(script).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| script.clone());
    package_of(&resolved).or_else(|| package_of(script))
}

/// [`agent_of`] for a process as read from /proc, and one more case: inside a toolbox or distrobox
/// `/proc/<pid>/exe` can't be read and argv[0] is often a bare `claude`, so a process with no
/// readable executable is taken as Claude when a live `~/.claude/sessions/<pid>.json` names that pid
/// with the very same start time (a reused pid has another start time and is not taken).
pub fn identify(p: &ProcInfo, live: &HashMap<String, LiveInfo>) -> Option<Agent> {
    agent_of(&p.argv, p.exe.as_deref()).or_else(|| {
        let start = p.start_ticks.to_string();
        (p.exe.is_none() && live.values().any(|l| l.alive && l.pid == p.pid && l.proc_start.as_deref() == Some(start.as_str()))).then_some(Agent::Claude)
    })
}

/// Run once and exit (`claude -p`), or serving another program (`claude mcp serve`): such a process
/// belongs to whatever started it, so Wake never calls it stale.
pub fn is_headless(argv: &[String]) -> bool {
    argv.get(1).is_some_and(|a| a == "mcp") || argv.iter().skip(1).any(|a| a == "-p" || a == "--print" || a.starts_with("--print="))
}

/// What role a Claude process plays, from argv.
#[derive(Debug, Clone, PartialEq)]
pub enum Role {
    Daemon,
    PtyHost,
    Attach(String),
    SpareProc,
    Main,
}

pub fn role_of(argv: &[String]) -> Role {
    let has = |w: &str| argv.iter().any(|a| a == w);
    if argv.get(1).map(String::as_str) == Some("daemon") {
        return Role::Daemon;
    }
    if has("--bg-pty-host") || argv.get(1).map(String::as_str) == Some("bg-pty-host") {
        return Role::PtyHost;
    }
    if argv.get(1).map(String::as_str) == Some("attach")
        && let Some(job) = argv.get(2)
    {
        return Role::Attach(job.clone());
    }
    if has("--bg-spare") {
        return Role::SpareProc;
    }
    Role::Main
}

const SECRET_WORDS: &[&str] = &["key", "token", "secret", "password", "passwd", "auth", "cookie", "credential"];

fn secretish(s: &str) -> bool {
    let l = s.to_lowercase();
    SECRET_WORDS.iter().any(|w| l.contains(w))
}

/// A short command line (≤ 120 chars) with values of secret-looking options hidden.
pub fn short_command(argv: &[String]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut hide_next = false;
    for (i, a) in argv.iter().enumerate() {
        if hide_next {
            out.push("…".into());
            hide_next = false;
            continue;
        }
        let a = if i == 0 { base(a).to_string() } else { a.clone() };
        if a.starts_with('-') && secretish(&a) {
            match a.split_once('=') {
                Some((k, _)) => out.push(format!("{k}=…")),
                None => {
                    out.push(a);
                    hide_next = true;
                }
            }
        } else if let Some((k, _)) = a.split_once('=').filter(|(k, _)| secretish(k) && !k.contains(' ')) {
            out.push(format!("{k}=…"));
        } else {
            out.push(a);
        }
    }
    let s = out.join(" ");
    if s.chars().count() > 120 { format!("{}…", util::take_chars(&s, 119)) } else { s }
}

/// What the rest of Wake knows about a session, for a row.
#[derive(Debug, Clone, Default)]
pub struct SessionView {
    pub title: Option<String>,
    pub project_name: Option<String>,
    pub state: Option<State>,
    /// Epoch seconds since when it has been idle (None while working).
    pub idle_since: Option<f64>,
    /// False for a session never used: no prompt and no name of its own.
    pub used: bool,
}

/// Everything `processes` needs from outside /proc.
pub struct Inputs<'a> {
    /// sessionId -> live info, from ~/.claude/sessions.
    pub live: &'a HashMap<String, LiveInfo>,
    pub view: &'a dyn Fn(&LiveInfo) -> SessionView,
    /// (pid, job id) -> Konsole tab.
    pub locate: &'a dyn Fn(u32, Option<&str>) -> Option<Location>,
    /// pids with an agent-notify note (alive or not) and their notes.
    pub notes: &'a HashMap<u32, WorkingNote>,
    pub now: f64,
}

/// (start ticks, cpu ticks, when, last percent)
type Sample = (u64, u64, Instant, Option<f32>);

/// Walks /proc; remembers CPU samples between calls.
pub struct Explorer {
    pub root: PathBuf,
    pub uid: u32,
    pub own_pid: u32,
    hz: f64,
    samples: Mutex<HashMap<u32, Sample>>,
    /// Claude's background job records, `~/.claude/jobs` (read only): their `fan` names the
    /// subagents a background session has running.
    pub jobs_dir: Option<PathBuf>,
}

fn boot_time(root: &Path, now: f64) -> f64 {
    if let Ok(s) = fs::read_to_string(root.join("stat"))
        && let Some(b) = s.lines().find_map(|l| l.strip_prefix("btime "))
        && let Ok(b) = b.trim().parse::<f64>()
    {
        return b;
    }
    if let Ok(u) = fs::read_to_string(root.join("uptime"))
        && let Some(up) = u.split_whitespace().next().and_then(|x| x.parse::<f64>().ok())
    {
        return now - up;
    }
    now
}

fn gb(b: u64) -> String {
    if b >= 1 << 30 { format!("{:.1} GB", b as f64 / (1u64 << 30) as f64) } else { format!("{} MB", b >> 20) }
}

impl Explorer {
    pub fn new(root: &Path) -> Explorer {
        let jobs_dir = Some(crate::store::Paths::from_env().claude.join("jobs"));
        Explorer { root: root.to_path_buf(), uid: my_uid(), own_pid: std::process::id(), hz: clock_ticks(), samples: Mutex::new(HashMap::new()), jobs_dir }
    }

    /// This user's processes, all of them (cheap: stat, status and cmdline per pid).
    pub fn scan(&self) -> Vec<ProcInfo> {
        let Ok(rd) = fs::read_dir(&self.root) else { return Vec::new() };
        let mut out = Vec::new();
        for e in rd.flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
            if e.metadata().map(|m| m.uid() != self.uid).unwrap_or(true) {
                continue;
            }
            if let Some(p) = read_proc(&self.root, pid)
                && p.uid == self.uid
            {
                out.push(p);
            }
        }
        out.sort_by_key(|p| p.pid);
        out
    }

    fn cpu(&self, p: &ProcInfo, now: Instant) -> Option<f32> {
        let mut s = self.samples.lock().unwrap_or_else(|e| e.into_inner());
        let prev = s.get(&p.pid).copied();
        let (val, keep) = match prev {
            Some((start, ticks, at, last)) if start == p.start_ticks => {
                let dt = now.duration_since(at).as_secs_f64();
                if dt >= 0.5 {
                    let pct = ((p.cpu_ticks.saturating_sub(ticks)) as f64 / self.hz / dt * 100.0) as f32;
                    (Some(pct), (p.start_ticks, p.cpu_ticks, now, Some(pct)))
                } else {
                    (last, (start, ticks, at, last))
                }
            }
            _ => (None, (p.start_ticks, p.cpu_ticks, now, None)),
        };
        s.insert(p.pid, keep);
        val
    }

    /// The job record of a background job, `<jobs>/<id>/state.json`, if it can be read (at most
    /// 1 MB). A job id that isn't plain letters, digits, `-` and `_` names nothing.
    pub fn job_record(&self, id: &str) -> Option<serde_json::Value> {
        let jobs = self.jobs_dir.as_ref()?;
        if id.is_empty() || id.len() > 64 || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return None;
        }
        let mut raw = Vec::new();
        fs::File::open(jobs.join(id).join("state.json")).ok()?.take(1 << 20).read_to_end(&mut raw).ok()?;
        serde_json::from_slice(&raw).ok()
    }

    /// Agent processes, mapped to sessions, with stale reasons.
    ///
    /// Spares and subagents follow agent-notify (the screensaver's source), not guesses:
    /// - a spare is a session entry with `"spare": true`, or a `bg-spare` process with no entry
    ///   whose claim socket is still there (Claude's daemon removes it when a job claims the spare);
    ///   never "no prompts yet", since a claimed process running a subagent keeps its transcript in
    ///   the parent's subagents folder and looks unused;
    /// - a Claude/Codex/Gemini process with another agent session above it in its process tree is a
    ///   subagent of that session (agent-notify's `agent_above`). Claude's daemon, pty hosts and
    ///   attach clients don't count as sessions there (every background session is under them), and
    ///   one of the same agent straight above another is the same session restarting itself.
    pub fn processes(&self, inp: &Inputs) -> ProcOverview {
        let all = self.scan();
        let now_i = Instant::now();
        let boot = boot_time(&self.root, inp.now);
        let by_pid: HashMap<u32, &ProcInfo> = all.iter().map(|p| (p.pid, p)).collect();
        let live_by_pid: HashMap<u32, &LiveInfo> = inp.live.values().filter(|l| l.alive).map(|l| (l.pid, l)).collect();
        let started = |p: &ProcInfo| boot + p.start_ticks as f64 / self.hz;

        struct Cand<'a> {
            p: &'a ProcInfo,
            agent: Agent,
            role: Role,
        }
        let cands: Vec<Cand> = all
            .iter()
            .filter(|p| p.pid != self.own_pid)
            .filter_map(|p| {
                let agent = identify(p, inp.live)?;
                let role = if agent == Agent::Claude { role_of(&p.argv) } else { Role::Main };
                Some(Cand { p, agent, role })
            })
            .collect();
        let mut seen: Vec<u32> = cands.iter().map(|c| c.p.pid).collect();
        seen.sort();
        self.samples.lock().unwrap_or_else(|e| e.into_inner()).retain(|pid, _| seen.binary_search(pid).is_ok());
        let cand_of: HashMap<u32, &Cand> = cands.iter().map(|c| (c.p.pid, c)).collect();
        let is_session_role = |r: &Role| matches!(r, Role::Main | Role::SpareProc);

        // Subagents: pid -> the agent session above it (agent-notify's `agent_above`).
        let mut sub_parent: HashMap<u32, u32> = HashMap::new();
        for c in cands.iter().filter(|c| is_session_role(&c.role)) {
            let mut pid = c.p.ppid;
            let mut adjacent = true;
            let mut steps = 0;
            while pid > 1 && steps < 256 {
                steps += 1;
                match cand_of.get(&pid) {
                    Some(k) if is_session_role(&k.role) => {
                        if !(adjacent && k.agent == c.agent) {
                            sub_parent.insert(c.p.pid, pid);
                            break;
                        }
                    }
                    _ => adjacent = false,
                }
                match by_pid.get(&pid) {
                    Some(p) => pid = p.ppid,
                    None => break,
                }
            }
        }
        // The top session a subagent (or a subagent's subagent) belongs to.
        let root_of = |mut pid: u32| {
            for _ in 0..64 {
                match sub_parent.get(&pid) {
                    Some(&up) => pid = up,
                    None => break,
                }
            }
            pid
        };

        let mk = |c: &Cand, kind: ProcKind| AgentProc {
            pid: c.p.pid,
            ppid: c.p.ppid,
            kind,
            agent: Some(c.agent),
            command: short_command(&c.p.argv),
            session_id: None,
            job_id: None,
            title: None,
            project_name: None,
            state: State::Unknown,
            idle_seconds: None,
            rss_bytes: c.p.rss_bytes,
            cpu_percent: self.cpu(c.p, now_i),
            started_at: Some(util::iso_from_epoch(started(c.p))),
            location: None,
            note: inp.notes.get(&c.p.pid).cloned(),
            stale_reason: None,
            children: Vec::new(),
            start_ticks: Some(c.p.start_ticks),
            helpers: None,
        };
        let age = |c: &Cand| inp.now - started(c.p);
        // Only a session Claude's daemon hosts, or one left behind with init (or the user's systemd)
        // as its parent, can be stale. Anything another program started (an editor, a script, a
        // terminal other than Konsole) belongs to that program, and so do headless runs.
        let can_be_stale = |c: &Cand| {
            if is_headless(&c.p.argv) {
                return false;
            }
            if c.p.ppid <= 1 {
                return true;
            }
            if let Some(parent) = cand_of.get(&c.p.ppid) {
                return matches!(parent.role, Role::PtyHost | Role::Daemon);
            }
            by_pid.get(&c.p.ppid).is_some_and(|p| p.comm == "systemd")
        };

        // pty hosts by the child they host; attach clients by job
        let mut hosts: HashMap<u32, Vec<&Cand>> = HashMap::new(); // hosted pid -> hosts
        let mut orphan_hosts: Vec<&Cand> = Vec::new();
        let mut attaches: HashMap<String, Vec<&Cand>> = HashMap::new();
        for c in &cands {
            match &c.role {
                Role::PtyHost => {
                    let kid = cands.iter().find(|k| k.p.ppid == c.p.pid && !matches!(k.role, Role::PtyHost));
                    match kid {
                        Some(k) => hosts.entry(k.p.pid).or_default().push(c),
                        None => orphan_hosts.push(c),
                    }
                }
                Role::Attach(job) => attaches.entry(job.clone()).or_default().push(c),
                _ => {}
            }
        }

        let mut daemons = Vec::new();
        let mut sessions: Vec<Vec<AgentProc>> = Vec::new();
        let mut spares: Vec<Vec<AgentProc>> = Vec::new();
        let mut strays: Vec<Vec<AgentProc>> = Vec::new();
        let mut used_attach: Vec<u32> = Vec::new();

        let fold = |row: &mut AgentProc, extra: &[&Cand], group: &mut Vec<AgentProc>| {
            for h in extra {
                let mut r = mk(h, ProcKind::PtyHost);
                r.session_id = row.session_id.clone();
                r.job_id = row.job_id.clone();
                r.title = row.title.clone();
                r.project_name = row.project_name.clone();
                r.state = row.state;
                row.rss_bytes += h.p.rss_bytes;
                row.children.push(h.p.pid);
                group.push(r);
            }
        };
        let hosts_of = |pid: u32| hosts.get(&pid).cloned().unwrap_or_default();

        for c in &cands {
            match &c.role {
                Role::Daemon => {
                    let mut r = mk(c, ProcKind::Daemon);
                    r.title = Some("Claude's background daemon".into());
                    r.state = State::Working;
                    daemons.push(r);
                }
                Role::PtyHost | Role::Attach(_) => {}
                Role::SpareProc | Role::Main => {
                    if sub_parent.contains_key(&c.p.pid) {
                        continue; // folded under its session below
                    }
                    let info = live_by_pid.get(&c.p.pid).copied();
                    let unclaimed = info.is_none() && c.role == Role::SpareProc && claim_socket_waiting(&c.p.argv);
                    if info.is_some_and(|i| i.spare) || unclaimed {
                        let mut r = mk(c, ProcKind::Spare);
                        r.title = Some("Spare session, not used yet".into());
                        r.state = State::Idle;
                        r.idle_seconds = Some(age(c).max(0.0) as u64);
                        if age(c) > STALE_SPARE {
                            r.stale_reason = Some(format!("a spare started {} ago and never used", util::ago(Some(age(c)))));
                        }
                        let mut group = Vec::new();
                        fold(&mut r, &hosts_of(c.p.pid), &mut group);
                        group.insert(0, r);
                        spares.push(group);
                        continue;
                    }
                    let codex_sid = if c.agent == Agent::Codex { codex_session_of(&self.root, c.p.pid) } else { None };
                    let location = (inp.locate)(c.p.pid, info.and_then(|i| i.job_id.as_deref()));
                    if info.is_none() && codex_sid.is_none() && location.is_none() {
                        // Unknown is not the same as stale: a stray is listed, never counted as stale.
                        let mut r = mk(c, ProcKind::Stray);
                        r.state = State::Unknown;
                        r.title = Some(if is_headless(&c.p.argv) { "Started by another program (not a session)".into() } else { "Not tied to any session, Konsole tab or status file".into() });
                        let mut group = vec![];
                        fold(&mut r, &hosts_of(c.p.pid), &mut group);
                        group.insert(0, r);
                        strays.push(group);
                        continue;
                    }
                    let mut r = mk(c, ProcKind::Session);
                    r.location = location.clone();
                    if let Some(info) = info {
                        let v = (inp.view)(info);
                        r.session_id = Some(info.session_id.clone());
                        r.job_id = info.job_id.clone();
                        r.title = v.title.clone().or_else(|| info.note.as_ref().map(|n| n.name.clone()).filter(|n| !n.is_empty()));
                        r.project_name = v.project_name.clone();
                        r.state = v.state.unwrap_or(State::Unknown);
                        r.idle_seconds = v.idle_since.map(|s| (inp.now - s).max(0.0) as u64);
                        if r.note.is_none() {
                            r.note = info.note.clone();
                        }
                        r.helpers = info.job_id.as_deref().and_then(|j| self.job_record(j)).map(|rec| helpers(&rec));
                        let mut group = Vec::new();
                        let mut extra: Vec<&Cand> = hosts_of(c.p.pid);
                        let att: Vec<&Cand> = info.job_id.as_ref().and_then(|j| attaches.get(j)).cloned().unwrap_or_default();
                        used_attach.extend(att.iter().map(|a| a.p.pid));
                        let attached = !att.is_empty();
                        extra.extend(att);
                        fold(&mut r, &extra, &mut group);
                        let idle = r.idle_seconds.unwrap_or(0) as f64;
                        if idle > STALE_IDLE && location.is_none() && !attached && can_be_stale(c) {
                            r.stale_reason = Some(format!("idle for {}, with no Konsole tab and nothing attached", util::ago(Some(idle))));
                        }
                        group.insert(0, r);
                        sessions.push(group);
                    } else {
                        r.session_id = codex_sid;
                        r.title = location.as_ref().map(|l| l.tab_title.clone());
                        r.state = State::Unknown;
                        let mut group = Vec::new();
                        fold(&mut r, &hosts_of(c.p.pid), &mut group);
                        group.insert(0, r);
                        sessions.push(group);
                    }
                }
            }
        }

        // Subagents, each folded into the session at the top of its tree: counted into its memory,
        // listed after it, named by the parent job's `fan` entry when its start time matches one.
        let mut subs: Vec<&Cand> = cands.iter().filter(|c| sub_parent.contains_key(&c.p.pid)).collect();
        subs.sort_by_key(|c| (c.p.start_ticks, c.p.pid));
        let mut fans: HashMap<u32, Vec<FanAgent>> = HashMap::new();
        let mut loose: Vec<Vec<AgentProc>> = Vec::new();
        for c in subs {
            let root = root_of(c.p.pid);
            let group = sessions.iter_mut().chain(spares.iter_mut()).chain(strays.iter_mut()).find(|g| g[0].pid == root);
            let info = live_by_pid.get(&c.p.pid).copied();
            let v = info.map(|i| (inp.view)(i));
            let mut r = mk(c, ProcKind::Subagent);
            r.session_id = info.map(|i| i.session_id.clone());
            r.project_name = v.as_ref().and_then(|v| v.project_name.clone());
            r.state = v.as_ref().and_then(|v| v.state).unwrap_or(State::Unknown);
            r.idle_seconds = v.as_ref().and_then(|v| v.idle_since).map(|s| (inp.now - s).max(0.0) as u64);
            let own_title = v.as_ref().and_then(|v| v.title.clone()).or_else(|| crate::live::real_name(info)).or_else(|| r.note.as_ref().map(|n| n.name.clone()).filter(|n| !n.is_empty()));
            let mut fan_label = None;
            if let Some(g) = &group {
                let top = &g[0];
                if r.project_name.is_none() {
                    r.project_name = top.project_name.clone();
                }
                if let Some(job) = top.job_id.as_deref() {
                    let fan = fans.entry(root).or_insert_with(|| self.job_record(job).map(|rec| fan_agents(&rec)).unwrap_or_default());
                    let start = started(c.p);
                    let best = fan
                        .iter()
                        .enumerate()
                        .filter(|(_, f)| !f.taken && !f.label.is_empty())
                        .filter_map(|(i, f)| f.started.map(|s| (i, (s - start).abs())))
                        .filter(|(_, d)| *d <= FAN_MATCH)
                        .min_by(|a, b| a.1.total_cmp(&b.1))
                        .map(|(i, _)| i);
                    if let Some(i) = best {
                        fan[i].taken = true;
                        fan_label = Some(fan[i].label.clone());
                    }
                }
            }
            if fan_label.is_some() && r.state == State::Unknown {
                r.state = State::Working; // its parent's job lists it as running
            }
            r.title = fan_label.or(own_title);
            match group {
                Some(g) => {
                    let extra = hosts_of(c.p.pid);
                    let mut rows = Vec::new();
                    fold(&mut r, &extra, &mut rows);
                    g[0].rss_bytes += r.rss_bytes;
                    g[0].children.push(r.pid);
                    g[0].children.extend(extra.iter().map(|h| h.p.pid));
                    g.push(r);
                    g.extend(rows);
                }
                None => loose.push(vec![r]),
            }
        }
        for c in orphan_hosts {
            let mut r = mk(c, ProcKind::Stray);
            r.title = Some("Terminal host whose session has ended".into());
            strays.push(vec![r]);
        }
        for (job, list) in &attaches {
            for c in list {
                if used_attach.contains(&c.p.pid) {
                    continue;
                }
                let mut r = mk(c, ProcKind::Stray);
                r.job_id = Some(job.clone());
                r.title = Some(format!("Attached to background job {job}, which is no longer running"));
                r.location = (inp.locate)(c.p.pid, None);
                strays.push(vec![r]);
            }
        }
        sessions.sort_by(|a, b| b[0].rss_bytes.cmp(&a[0].rss_bytes));
        spares.sort_by_key(|g| g[0].pid);
        strays.sort_by_key(|g| g[0].pid);

        let mut procs = daemons;
        procs.extend(sessions.into_iter().flatten());
        procs.extend(spares.into_iter().flatten());
        procs.extend(strays.into_iter().flatten());
        procs.extend(loose.into_iter().flatten());

        let top: Vec<&AgentProc> = procs.iter().filter(|r| !is_folded(r, &procs)).collect();
        let total: u64 = top.iter().map(|r| r.rss_bytes).sum();
        let stale: Vec<&&AgentProc> = top.iter().filter(|r| r.stale_reason.is_some() && r.kind != ProcKind::Stray).collect();
        let mut dead_notes: Vec<u32> = inp.notes.keys().copied().filter(|p| !by_pid.contains_key(p) && !self.root.join(p.to_string()).exists()).collect();
        dead_notes.sort();
        ProcOverview {
            generated_at: util::iso_from_epoch(inp.now),
            total_rss_bytes: total,
            stale_rss_bytes: stale.iter().map(|r| r.rss_bytes).sum(),
            stale_count: stale.len() as u32,
            dead_notes,
            procs,
        }
    }
}

/// Memory and CPU by session and by project (added 2026-11-20): each session's row already holds
/// its pty host, attach clients and helper agents in `rss_bytes`; CPU is added up here from their
/// rows. `project_of` names a session's project (None: no project).
pub fn resources(ov: &ProcOverview, project_of: &dyn Fn(&str) -> Option<String>) -> Resources {
    let by_pid: HashMap<u32, &AgentProc> = ov.procs.iter().map(|r| (r.pid, r)).collect();
    let mut sessions = Vec::new();
    let mut projects: Vec<ProjectResources> = Vec::new();
    for r in ov.procs.iter().filter(|r| r.kind == ProcKind::Session) {
        let Some(sid) = r.session_id.clone() else { continue };
        let kids: Vec<&AgentProc> = r.children.iter().filter_map(|c| by_pid.get(c).copied()).collect();
        // A helper's own row already holds its pty host: count each process once.
        let nested: HashSet<u32> = kids.iter().flat_map(|k| k.children.iter().copied()).collect();
        let kids_rss: u64 = kids.iter().filter(|k| !nested.contains(&k.pid)).map(|k| k.rss_bytes).sum();
        let cpu = std::iter::once(r.cpu_percent).chain(kids.iter().map(|k| k.cpu_percent)).try_fold(0.0f32, |a, c| c.map(|c| a + c));
        let mut pids = vec![r.pid];
        pids.extend(r.children.iter().copied());
        let project = project_of(&sid);
        match projects.iter_mut().find(|p| p.project == project) {
            Some(p) => {
                p.rss_bytes += r.rss_bytes;
                p.cpu_percent = match (p.cpu_percent, cpu) {
                    (Some(a), Some(b)) => Some(a + b),
                    (a, b) => a.or(b),
                };
                p.sessions += 1;
                p.processes += pids.len() as u32;
            }
            None => projects.push(ProjectResources { project, rss_bytes: r.rss_bytes, cpu_percent: cpu, sessions: 1, processes: pids.len() as u32 }),
        }
        sessions.push(SessionResources { session: sid, pid: r.pid, start_ticks: r.start_ticks, rss_bytes: r.rss_bytes, own_rss_bytes: r.rss_bytes.saturating_sub(kids_rss), cpu_percent: cpu, pids });
    }
    projects.sort_by_key(|p| std::cmp::Reverse(p.rss_bytes));
    Resources { sessions, projects, total_rss_bytes: ov.total_rss_bytes }
}

/// A subagent process counts as a parent job's `fan` entry when they started this close together.
const FAN_MATCH: f64 = 30.0;

/// One running subagent in a job record's `fan`.
#[derive(Debug, Clone, PartialEq)]
struct FanAgent {
    label: String,
    /// Epoch seconds (`startedAt` is in milliseconds).
    started: Option<f64>,
    taken: bool,
}

/// The subagents a job record has running, in the order it lists them, as agent-notify reads them
/// (`agents/claude.rs`, `helpers`): `fan` is an array or an object of entries; an entry of kind
/// `agent` without `doneAt` is running; its `label` is its task, on one line, at most 60
/// characters. A malformed entry ends the list there.
fn fan_agents(record: &serde_json::Value) -> Vec<FanAgent> {
    use serde_json::Value;
    let entries: Vec<&Value> = match record.get("fan") {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(Value::Object(items)) => items.values().collect(),
        _ => Vec::new(),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = match entry {
            Value::Object(entry) => entry,
            Value::Null => continue,
            _ => break,
        };
        let running = entry.get("kind").and_then(Value::as_str) == Some("agent") && entry.get("doneAt").is_none_or(Value::is_null);
        if !running {
            continue;
        }
        let label = match entry.get("label") {
            None | Some(Value::Null | Value::Bool(false)) => String::new(),
            Some(Value::String(label)) => label.split_whitespace().collect::<Vec<_>>().join(" "),
            Some(_) => break,
        };
        let label: String = label.chars().filter(|c| !c.is_control()).collect();
        let label = util::take_chars(label.trim(), 60);
        out.push(FanAgent { label, started: entry.get("startedAt").and_then(Value::as_f64).filter(|s| *s > 0.0).map(|s| s / 1000.0), taken: false });
        if out.len() == 64 {
            break;
        }
    }
    out
}

/// The running subagents' tasks, up to eight, as the screensaver's helper lines show them.
pub fn helpers(record: &serde_json::Value) -> Vec<String> {
    fan_agents(record).into_iter().map(|f| f.label).filter(|l| !l.is_empty()).take(8).collect()
}

/// A `bg-spare` process no job has claimed yet: its claim socket (`--bg-spare <sock>`) is still
/// there. Claude's daemon removes it once a job takes the spare.
fn claim_socket_waiting(argv: &[String]) -> bool {
    let Some(i) = argv.iter().position(|a| a == "--bg-spare") else { return false };
    argv.get(i + 1).is_some_and(|s| s.starts_with('/') && fs::symlink_metadata(s).is_ok())
}

fn is_folded(r: &AgentProc, all: &[AgentProc]) -> bool {
    all.iter().any(|o| o.children.contains(&r.pid))
}

/// The top line: "Agents are using 3.6 GB in 27 processes; 10 look stale and hold 2.6 GB."
pub fn headline(o: &ProcOverview) -> String {
    let n = o.procs.len();
    let mut s = format!("Agents are using {} in {} process{}", gb(o.total_rss_bytes), n, if n == 1 { "" } else { "es" });
    if o.stale_count > 0 {
        s.push_str(&format!("; {} look stale and hold {}", o.stale_count, gb(o.stale_rss_bytes)));
    } else {
        s.push_str("; none look stale");
    }
    s.push('.');
    s
}

/// A Codex process's session id, from the rollout file it holds open.
pub fn codex_session_of(root: &Path, pid: u32) -> Option<String> {
    let rd = fs::read_dir(root.join(pid.to_string()).join("fd")).ok()?;
    for e in rd.flatten().take(256) {
        let Ok(t) = fs::read_link(e.path()) else { continue };
        let t = t.to_string_lossy();
        let name = base(&t);
        if t.contains("/sessions/")
            && name.starts_with("rollout-")
            && let Some(id) = name.strip_suffix(".jsonl").and_then(util::uuid_tail)
        {
            return Some(id.to_string());
        }
    }
    None
}

// ------------------------------------------------------------------ stopping

/// Does the real work of stopping; tests use a fake.
pub trait Actor: Send + Sync {
    /// Send `sig` to `pid` only if it is still the process that started at `start_ticks`.
    fn signal(&self, pid: u32, start_ticks: u64, sig: i32) -> Result<(), String>;
    /// Run a command (no shell): (exit code, output).
    fn run(&self, argv: &[String]) -> (i32, String);
}

/// Signals through a pidfd after re-checking the start time, so a reused PID is never hit.
pub struct RealActor {
    pub root: PathBuf,
}

impl Actor for RealActor {
    fn signal(&self, pid: u32, start_ticks: u64, sig: i32) -> Result<(), String> {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::c_int, 0 as libc::c_uint) };
        if raw < 0 {
            let e = std::io::Error::last_os_error();
            return Err(if e.raw_os_error() == Some(libc::ESRCH) { "the process has already gone".into() } else { format!("Wake couldn't safely reach the process ({e}), so it did nothing") });
        }
        // SAFETY: pidfd_open returned a new descriptor that nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(raw as i32) };
        // The pidfd is open, so from here the pid cannot be reused; check it is still the same process.
        let now = read_proc(&self.root, pid).ok_or("the process has already gone")?;
        if now.start_ticks != start_ticks {
            return Err("that process ID now belongs to a different process".to_string());
        }
        let r = unsafe { libc::syscall(libc::SYS_pidfd_send_signal, fd.as_raw_fd(), sig, std::ptr::null::<libc::siginfo_t>(), 0 as libc::c_uint) };
        if r < 0 { Err(std::io::Error::last_os_error().to_string()) } else { Ok(()) }
    }

    fn run(&self, argv: &[String]) -> (i32, String) {
        (crate::desktop::default_runner())(argv, std::time::Duration::from_secs(20))
    }
}

/// "asked 'x' (PID n) to exit", "force-stopped 'x' (PID n)", "asked Claude to stop 'x' (PID n)".
fn said(what: &str, name: &str) -> String {
    if what == "asked" { format!("asked {name} to exit") } else { format!("{what} {name}") }
}

/// The sentence for a pid that no longer names the process the person confirmed.
pub const ALREADY_ENDED: &str = "That process has already ended; nothing was stopped.";

/// Stop one agent process. Everything about the target is found fresh from /proc and the session
/// files. `expected_start` (the `start_ticks` of the row the person confirmed) binds the action to
/// that very process: if the pid now names a different one, nothing is done.
#[allow(clippy::too_many_arguments)]
pub fn stop(
    explorer: &Explorer,
    actor: &dyn Actor,
    live: &HashMap<String, LiveInfo>,
    title_of: &dyn Fn(&LiveInfo) -> Option<String>,
    pid: u32,
    expected_start: Option<u64>,
    how: StopHow,
    dry_run: bool,
) -> ActionResult {
    let refuse = |m: String| ActionResult { ok: false, message: m, steps: vec![] };
    if pid <= 1 {
        return refuse("Wake will not touch process 1, the system's own.".into());
    }
    if pid == explorer.own_pid {
        return refuse("That is Wake itself.".into());
    }
    let Some(p) = read_proc(&explorer.root, pid) else {
        return refuse(if expected_start.is_some() { ALREADY_ENDED.into() } else { format!("Process {pid} is no longer running.") });
    };
    if expected_start.is_some_and(|t| t != p.start_ticks) {
        return refuse(ALREADY_ENDED.into());
    }
    if p.uid != explorer.uid {
        return refuse(format!("Process {pid} belongs to another user, so Wake will not touch it."));
    }
    let Some(agent) = identify(&p, live) else {
        return refuse(format!("Process {pid} ({}) is not an agent process, so Wake will not stop it.", util::take_chars(&p.comm, 40)));
    };
    let role = if agent == Agent::Claude { role_of(&p.argv) } else { Role::Main };
    if role == Role::Daemon {
        return refuse("That is Claude's background daemon. Stopping it would stop every background session, so Wake leaves it alone.".into());
    }
    let info = live.values().find(|l| l.alive && l.pid == pid);
    let name = info.and_then(title_of).map(|t| format!("'{t}' (PID {pid})")).unwrap_or_else(|| format!("PID {pid}"));
    let (steps, what) = match how {
        StopHow::AgentStop => {
            let Some(job) = info.and_then(|i| i.job_id.clone()) else {
                return refuse(format!("{name} isn't a background session, so 'claude stop' can't stop it. Ask it to exit instead."));
            };
            if job.is_empty() || job.len() > 64 || !job.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                return refuse(format!("The job id of {name} looks wrong, so Wake will not pass it to 'claude stop'."));
            }
            (vec!["claude".to_string(), "stop".into(), job], "asked Claude to stop")
        }
        StopHow::Terminate => (vec!["kill".into(), "-TERM".into(), pid.to_string()], "asked"),
        StopHow::Kill => (vec!["kill".into(), "-KILL".into(), pid.to_string()], "force-stopped"),
    };
    let step = util::shell_join(&steps);
    if dry_run {
        return ActionResult { ok: true, message: format!("Dry run. Wake would have {}.", said(what, &name)), steps: vec![step] };
    }
    let outcome = match how {
        StopHow::AgentStop => {
            let (code, out) = actor.run(&steps);
            if code == 0 { Ok(()) } else { Err(util::take_chars(out.trim(), 200)) }
        }
        StopHow::Terminate => actor.signal(pid, p.start_ticks, libc::SIGTERM),
        StopHow::Kill => actor.signal(pid, p.start_ticks, libc::SIGKILL),
    };
    match outcome {
        Ok(()) => {
            let tail = if how == StopHow::Kill { "" } else { " If it is still running in 5 seconds, Force is offered." };
            ActionResult { ok: true, message: format!("Wake {}.{tail}", said(what, &name)), steps: vec![step] }
        }
        Err(e) => ActionResult { ok: false, message: format!("Could not stop {name}: {e}."), steps: vec![step] },
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::desktop::tests::TmpDir;

    /// A fake /proc with this user's uid.
    pub struct FakeProc {
        pub dir: TmpDir,
    }

    impl FakeProc {
        pub fn new() -> FakeProc {
            let dir = TmpDir::new("fakeproc");
            fs::write(dir.0.join("stat"), "cpu 1 2 3\nbtime 1795000000\n").unwrap();
            FakeProc { dir }
        }
        pub fn root(&self) -> &Path {
            &self.dir.0
        }
        #[allow(clippy::too_many_arguments)]
        pub fn add(&self, pid: u32, ppid: u32, argv: &[&str], exe: &str, rss_kb: u64, ticks: u64, start: u64, uid: u32) {
            let d = self.dir.0.join(pid.to_string());
            fs::create_dir_all(&d).unwrap();
            let comm = if exe.contains("/versions/") { base(exe).to_string() } else { base(argv[0]).to_string() };
            fs::write(d.join("stat"), format!("{pid} ({comm}) S {ppid} {pid} {pid} 0 -1 4194304 1 1 1 1 {ticks} 0 0 0 20 0 1 0 {start} 1000 100\n")).unwrap();
            fs::write(d.join("status"), format!("Name:\t{comm}\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\nVmRSS:\t  {rss_kb} kB\n")).unwrap();
            let mut raw = argv.join("\0").into_bytes();
            raw.push(0);
            fs::write(d.join("cmdline"), raw).unwrap();
            if !exe.is_empty() {
                std::os::unix::fs::symlink(exe, d.join("exe")).unwrap();
            }
        }
    }

    pub struct FakeActor {
        pub calls: Mutex<Vec<String>>,
    }
    impl Actor for FakeActor {
        fn signal(&self, pid: u32, start: u64, sig: i32) -> Result<(), String> {
            self.calls.lock().unwrap().push(format!("signal {pid} {start} {sig}"));
            Ok(())
        }
        fn run(&self, argv: &[String]) -> (i32, String) {
            self.calls.lock().unwrap().push(argv.join(" "));
            (0, String::new())
        }
    }

    const V: &str = "/home/u/.local/share/claude/versions/2.1.328";
    const HZ: u64 = 100;

    /// A machine like this one: a daemon, two background sessions (one used and attached in a tab,
    /// one never used for a day), an old and a fresh spare, an interactive session, a stray, an
    /// orphan attach client, codex, and things that merely mention claude.
    pub fn machine(fp: &FakeProc, now: f64) -> HashMap<String, LiveInfo> {
        let uid = my_uid();
        let boot = 1795000000.0;
        let st = |ago: f64| ((now - ago - boot) * HZ as f64) as u64;
        // unclaimed spares still have their claim socket; claimed ones don't
        let claim = |n: &str| {
            let p = fp.root().join(n);
            fs::write(&p, "").unwrap();
            p.to_string_lossy().into_owned()
        };
        let (c_claim, d_claim) = (claim("c.claim.sock"), claim("d.claim.sock"));
        fp.add(100, 1, &[V, "daemon", "run", "--origin", "transient"], V, 100_000, 50, st(86400.0), uid);
        // session A: used, idle 10 h, attached from a Konsole tab
        fp.add(200, 100, &["claude", "bg-pty-host", "--bg-pty-host", "/tmp/s.sock", "--", V, "--bg-spare"], V, 40_000, 5, st(36000.0), uid);
        fp.add(201, 200, &["claude", "bg-spare", "--bg-spare", "/tmp/a.claim.sock"], V, 300_000, 900, st(36000.0), uid);
        fp.add(300, 299, &["claude", "attach", "jobA"], "/home/u/.local/share/claude/versions/2.1.327", 70_000, 10, st(36000.0), uid);
        // session B: never used, idle a day, nothing attached
        fp.add(210, 100, &["claude", "bg-pty-host", "--bg-pty-host", "/tmp/b.sock"], V, 40_000, 5, st(86000.0), uid);
        fp.add(211, 210, &["claude", "bg-spare", "--bg-spare", "/tmp/b.claim.sock"], V, 200_000, 50, st(86000.0), uid);
        // spares: one 3 h old, one 10 min old
        fp.add(220, 100, &["claude", "bg-pty-host", "--bg-pty-host", "/tmp/c.sock"], V, 40_000, 5, st(10800.0), uid);
        fp.add(221, 220, &["claude", "bg-spare", "--bg-spare", &c_claim], V, 180_000, 5, st(10800.0), uid);
        fp.add(230, 100, &["claude", "bg-pty-host", "--bg-pty-host", "/tmp/d.sock"], V, 40_000, 5, st(600.0), uid);
        fp.add(231, 230, &["claude", "bg-spare", "--bg-spare", &d_claim], V, 180_000, 5, st(600.0), uid);
        // interactive session in a tab, working
        fp.add(400, 399, &["claude", "--model", "opus", "--api-key", "sk-SECRET"], V, 500_000, 9000, st(3600.0), uid);
        // stray claude with no session file, old (an MCP server another app started); orphan attach client
        fp.add(500, 1, &["claude", "mcp", "serve"], V, 90_000, 1, st(7200.0), uid);
        fp.add(510, 1, &["claude", "attach", "gone99"], V, 60_000, 1, st(7200.0), uid);
        // node-based codex in a tab; gemini; things that are not agents
        fp.add(600, 399, &["node", "/usr/lib/node_modules/@openai/codex/bin/codex.js"], "/usr/bin/node", 120_000, 10, st(100.0), uid);
        fp.add(700, 1, &["/home/u/.local/bin/agent-notify", "keep", "claude", "attach", "jobA"], "/home/u/.local/bin/agent-notify", 2_000, 1, st(36000.0), uid);
        fp.add(299, 1, &["zsh", "-c", "claude attach jobA"], "/usr/bin/zsh", 3_000, 1, st(36000.0), uid);
        fp.add(800, 1, &["/usr/bin/python3", "x.py"], "/usr/bin/python3", 10_000, 1, st(100.0), uid);
        // look-alikes: a dev server whose path says gemini, a clone of claude-code built from source
        fp.add(810, 399, &["node", "/home/u/Projects/gemini-chat/server.js"], "/usr/bin/node", 50_000, 1, st(7200.0), uid);
        fp.add(820, 1, &["/home/u/src/claude-code/target/server", "--port", "3000"], "/home/u/src/claude-code/target/server", 9_000, 1, st(7200.0), uid);
        fp.add(900, 1, &["claude", "--resume", "x"], V, 77_000, 1, st(100.0), uid + 1); // other user
        let mut live = HashMap::new();
        let mk = |pid: u32, sid: &str, job: Option<&str>, name: &str, status: &str, upd: f64| LiveInfo {
            pid,
            session_id: sid.into(),
            alive: true,
            status: Some(status.into()),
            status_updated_at: Some(upd),
            name: Some(name.into()),
            kind: Some(if job.is_some() { "bg" } else { "interactive" }.into()),
            job_id: job.map(Into::into),
            ..Default::default()
        };
        live.insert("sess-A".into(), mk(201, "sess-A", Some("jobA"), "parser-work", "idle", now - 36000.0));
        live.insert("sess-B".into(), mk(211, "sess-B", Some("jobB"), "sess-B", "idle", now - 86000.0));
        live.insert("sess-I".into(), mk(400, "sess-I", None, "interactive-thing", "busy", now - 5.0));
        live.insert("sess-dead".into(), LiveInfo { alive: false, ..mk(999, "sess-dead", Some("jobD"), "x", "idle", now) });
        live
    }

    fn run(fp: &FakeProc, now: f64) -> ProcOverview {
        let live = machine(fp, now);
        let ex = Explorer::new(fp.root());
        let view = |i: &LiveInfo| SessionView {
            title: if i.session_id == "sess-B" { None } else { Some(format!("Title {}", i.session_id)) },
            project_name: Some("alpha".into()),
            state: Some(if i.status.as_deref() == Some("busy") { State::Working } else { State::Idle }),
            idle_since: if i.status.as_deref() == Some("idle") { i.status_updated_at } else { None },
            used: i.session_id != "sess-B",
        };
        let locate = |pid: u32, job: Option<&str>| -> Option<Location> {
            if pid == 400 || pid == 600 || job == Some("jobA") {
                Some(Location { konsole_pid: 3264, service: "org.kde.konsole-3264".into(), window: 1, session: pid, tab_index: 0, tab_title: format!("tab {pid}"), label: None })
            } else {
                None
            }
        };
        let mut notes = HashMap::new();
        notes.insert(201, WorkingNote { name: "claude: alpha".into(), idle: true, helpers: vec![] });
        notes.insert(4242, WorkingNote { name: "claude: gone".into(), idle: true, helpers: vec![] });
        ex.processes(&Inputs { live: &live, view: &view, locate: &locate, notes: &notes, now })
    }

    #[test]
    fn maps_a_machine() {
        let fp = FakeProc::new();
        let now = util::now();
        let o = run(&fp, now);
        let row = |pid: u32| o.procs.iter().find(|p| p.pid == pid).unwrap_or_else(|| panic!("no row {pid}"));
        let pids: Vec<u32> = o.procs.iter().map(|p| p.pid).collect();
        // not agents, other users and Wake itself never appear
        for p in [700, 299, 800, 900] {
            assert!(!pids.contains(&p), "{p} listed");
        }
        assert_eq!(pids[0], 100);
        assert_eq!(row(100).kind, ProcKind::Daemon);
        assert_eq!(row(100).stale_reason, None);
        // session A: pty host and attach client folded in, located through the job, not stale
        let a = row(201);
        assert_eq!(a.kind, ProcKind::Session);
        assert_eq!(a.session_id.as_deref(), Some("sess-A"));
        assert_eq!(a.job_id.as_deref(), Some("jobA"));
        assert_eq!(a.title.as_deref(), Some("Title sess-A"));
        let mut kids = a.children.clone();
        kids.sort();
        assert_eq!(kids, [200, 300]);
        assert_eq!(a.rss_bytes, (300_000 + 40_000 + 70_000) * 1024);
        assert!(a.location.is_some());
        assert_eq!(a.stale_reason, None);
        assert_eq!(a.note.as_ref().unwrap().name, "claude: alpha");
        assert_eq!(row(200).kind, ProcKind::PtyHost);
        assert_eq!(row(300).kind, ProcKind::PtyHost);
        let ia = pids.iter().position(|p| *p == 201).unwrap();
        assert!(pids[ia + 1..ia + 3].contains(&200) && pids[ia + 1..ia + 3].contains(&300));
        // session B: never used, idle a day, nothing attached -> stale (idle, not "never used")
        let b = row(211);
        assert_eq!(b.kind, ProcKind::Session);
        assert!(b.stale_reason.as_deref().unwrap().starts_with("idle for 23 h"), "{:?}", b.stale_reason);
        assert!(b.idle_seconds.unwrap() >= 85000);
        assert_eq!(b.children, [210]);
        // spares
        assert_eq!(row(221).kind, ProcKind::Spare);
        assert!(row(221).stale_reason.as_deref().unwrap().starts_with("a spare started 3 h ago"));
        assert_eq!(row(231).kind, ProcKind::Spare);
        assert_eq!(row(231).stale_reason, None);
        // interactive, working, in a tab; secrets hidden
        let i = row(400);
        assert_eq!((i.kind, i.state, i.stale_reason.is_none()), (ProcKind::Session, State::Working, true));
        assert!(!i.command.contains("SECRET"), "{}", i.command);
        assert!(i.command.starts_with("claude --model opus --api-key …"));
        // codex in a tab counts as a session; strays are listed, never stale
        assert_eq!((row(600).kind, row(600).agent), (ProcKind::Session, Some(Agent::Codex)));
        assert_eq!(row(500).kind, ProcKind::Stray);
        assert_eq!(row(500).stale_reason, None);
        assert_eq!(row(510).kind, ProcKind::Stray);
        assert_eq!(row(510).stale_reason, None);
        assert!(row(510).title.as_deref().unwrap().contains("gone99"));
        // look-alikes are not agents
        assert!(!pids.contains(&810) && !pids.contains(&820));
        // every row carries its start time, for stop
        for p in &o.procs {
            let stat = fs::read_to_string(fp.root().join(p.pid.to_string()).join("stat")).unwrap();
            assert_eq!(p.start_ticks, parse_stat(&stat).map(|s| s.3), "{}", p.pid);
        }
        // totals count each process once
        let all_agent_kb: u64 = [100, 200, 201, 300, 210, 211, 220, 221, 230, 231, 400, 500, 510, 600]
            .iter()
            .map(|p| fs::read_to_string(fp.root().join(p.to_string()).join("status")).map(|s| rss_from_status(&s)).unwrap())
            .sum();
        assert_eq!(o.total_rss_bytes, all_agent_kb);
        assert_eq!(o.stale_count, 2); // B and the old spare; never the strays
        let stale_sum = row(211).rss_bytes + row(221).rss_bytes;
        assert_eq!(o.stale_rss_bytes, stale_sum);
        assert_eq!(o.dead_notes, [4242]);
        assert!(headline(&o).contains("2 look stale"));
        // CPU needs a second sample
        assert!(o.procs.iter().all(|p| p.cpu_percent.is_none()));
    }

    #[test]
    fn cpu_from_two_samples() {
        let fp = FakeProc::new();
        let uid = my_uid();
        fp.add(42, 1, &["codex"], "/usr/bin/codex", 1000, 100, 5, uid);
        let ex = Explorer::new(fp.root());
        let live = HashMap::new();
        let view = |_: &LiveInfo| SessionView::default();
        let locate = |_: u32, _: Option<&str>| -> Option<Location> { None };
        let notes = HashMap::new();
        let inp = Inputs { live: &live, view: &view, locate: &locate, notes: &notes, now: util::now() };
        assert_eq!(ex.processes(&inp).procs[0].cpu_percent, None);
        {
            let mut s = ex.samples.lock().unwrap();
            let e = s.get_mut(&42).unwrap();
            e.2 -= std::time::Duration::from_secs(2);
        }
        fs::write(fp.root().join("42/stat"), "42 (codex) S 1 42 42 0 -1 4194304 1 1 1 1 200 100 0 0 20 0 1 0 5 1000 100\n").unwrap();
        let cpu = ex.processes(&inp).procs[0].cpu_percent.unwrap();
        assert!((cpu - 100.0 * (300.0 - 100.0) / clock_ticks() as f32 / 2.0).abs() < 5.0, "{cpu}");
    }

    #[test]
    fn stop_refuses_and_dry_runs() {
        let fp = FakeProc::new();
        let now = util::now();
        let live = machine(&fp, now);
        let ex = Explorer::new(fp.root());
        let actor = FakeActor { calls: Mutex::new(vec![]) };
        let title = |i: &LiveInfo| i.name.clone();
        let go = |pid: u32, how: StopHow, dry: bool| stop(&ex, &actor, &live, &title, pid, None, how, dry);
        assert!(go(1, StopHow::Terminate, true).message.contains("process 1"));
        assert!(go(std::process::id(), StopHow::Kill, true).message.contains("Wake itself"));
        assert!(go(100, StopHow::Terminate, true).message.contains("daemon"));
        assert!(go(800, StopHow::Terminate, true).message.contains("not an agent"));
        assert!(go(700, StopHow::Kill, true).message.contains("not an agent"));
        assert!(go(900, StopHow::Kill, true).message.contains("another user"));
        assert!(go(4242, StopHow::Kill, true).message.contains("no longer running"));
        assert!(!go(400, StopHow::AgentStop, true).ok); // interactive: no job
        let r = go(201, StopHow::AgentStop, true);
        assert!(r.ok);
        assert_eq!(r.steps, ["claude stop jobA"]);
        assert!(r.message.starts_with("Dry run.") && r.message.contains("'parser-work' (PID 201)"));
        let t = go(221, StopHow::Terminate, true);
        assert_eq!(t.steps, ["kill -TERM 221"]);
        assert!(t.message.starts_with("Dry run. Wake would have asked ") && t.message.ends_with(" to exit."), "{}", t.message);
        assert!(!t.message.contains("SIG"), "{}", t.message);
        assert!(!go(500, StopHow::Kill, true).message.contains("SIG"));
        assert!(!go(400, StopHow::AgentStop, true).message.contains("End"));
        assert_eq!(go(500, StopHow::Kill, true).steps, ["kill -KILL 500"]);
        assert!(actor.calls.lock().unwrap().is_empty(), "dry run acted");
        // a real run goes through the actor only (a fake here), with the start time checked
        assert!(go(201, StopHow::AgentStop, false).ok);
        assert!(go(500, StopHow::Terminate, false).ok);
        let calls = actor.calls.lock().unwrap().clone();
        assert_eq!(calls[0], "claude stop jobA");
        assert!(calls[1].starts_with("signal 500 ") && calls[1].ends_with(&format!(" {}", libc::SIGTERM)));
    }

    #[test]
    fn recognises_agents() {
        let a = |argv: &[&str], exe: Option<&str>| agent_of(&argv.iter().map(|s| s.to_string()).collect::<Vec<_>>(), exe);
        assert_eq!(a(&["claude", "attach", "x"], Some(V)), Some(Agent::Claude));
        assert_eq!(a(&[V, "daemon", "run"], None), Some(Agent::Claude));
        assert_eq!(a(&["node", "/usr/lib/node_modules/@anthropic-ai/claude-code/cli.js"], Some("/usr/bin/node")), Some(Agent::Claude));
        assert_eq!(a(&["node", "/usr/lib/node_modules/@google/gemini-cli/dist/index.js"], Some("/usr/bin/node")), Some(Agent::Gemini));
        assert_eq!(a(&["/usr/bin/node", "--no-warnings", "/u/.npm/lib/node_modules/@openai/codex/bin/codex.js"], Some("/usr/bin/node")), Some(Agent::Codex));
        assert_eq!(a(&["codex"], Some("/u/node_modules/@openai/codex/vendor/x86_64-unknown-linux-musl/codex/codex")), Some(Agent::Codex));
        assert_eq!(a(&["codex-x86_64-unknown-linux-musl"], Some("/u/vendor/codex-x86_64-unknown-linux-musl")), Some(Agent::Codex));
        assert_eq!(a(&["codex", "resume", "y"], Some("/home/u/.cargo/bin/codex")), Some(Agent::Codex));
        assert_eq!(a(&["konsole", "-e", "zsh", "-c", "claude attach x"], Some("/usr/bin/konsole")), None);
        assert_eq!(a(&["node", "server.js"], Some("/usr/bin/node")), None);
        assert_eq!(a(&["agent-notify", "keep", "claude"], Some("/x/agent-notify")), None);
        // look-alikes: a word in a path is not enough
        assert_eq!(a(&["node", "/x/bin/gemini"], Some("/usr/bin/node")), None); // not inside the package
        assert_eq!(a(&["node", "/home/u/Projects/gemini-chat/server.js"], Some("/usr/bin/node")), None);
        assert_eq!(a(&["node", "/home/u/Projects/my-gemini/node_modules/.bin/vite"], Some("/usr/bin/node")), None);
        assert_eq!(a(&["node", "/u/claude-code-clone/cli.js"], Some("/usr/bin/node")), None);
        assert_eq!(a(&["node", "/u/node_modules/@google/gemini-cli-core/x.js"], Some("/usr/bin/node")), None);
        assert_eq!(a(&["node", "/u/node_modules/@anthropic-ai/claude-code-router/cli.js"], Some("/usr/bin/node")), None);
        assert_eq!(a(&["server"], Some("/home/u/src/claude-code/target/server")), None);
        assert_eq!(a(&["gemini"], Some("/usr/bin/gemini")), None); // e.g. a Gemini-protocol browser
        assert_eq!(a(&["codex-sync"], Some("/usr/local/bin/codex-sync")), None);
        assert_eq!(a(&["claude", "--resume", "x"], Some("/usr/bin/python3")), None); // argv[0] can say anything
        assert_eq!(a(&["node", "relative/cli.js"], Some("/usr/bin/node")), None);
        // argv[0] stands in only when the executable can't be read, and must be a full path
        assert_eq!(a(&[V, "daemon"], None), Some(Agent::Claude));
        assert_eq!(a(&["claude", "daemon"], None), None);
        let r = |argv: &[&str]| role_of(&argv.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(r(&["claude", "attach", "b5a90d33"]), Role::Attach("b5a90d33".into()));
        assert_eq!(r(&["claude", "bg-pty-host", "--bg-pty-host", "s", "--", V, "--bg-spare"]), Role::PtyHost);
        assert_eq!(r(&[V, "--resume", "/x.jsonl"]), Role::Main);
        assert_eq!(short_command(&["x".into(), "TOKEN=abc".into(), "--password=p".into()]), "x TOKEN=… --password=…");
    }

    #[test]
    fn npm_bin_symlinks_are_followed() {
        let tmp = TmpDir::new("npmbin");
        let pkg = tmp.0.join("lib/node_modules/@google/gemini-cli/dist");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(pkg.join("index.js"), "").unwrap();
        fs::create_dir_all(tmp.0.join("bin")).unwrap();
        std::os::unix::fs::symlink(pkg.join("index.js"), tmp.0.join("bin/gemini")).unwrap();
        let fake = tmp.0.join("Projects/gemini/bin");
        fs::create_dir_all(&fake).unwrap();
        fs::write(fake.join("gemini"), "").unwrap();
        let a = |script: &Path| agent_of(&["node".to_string(), script.to_string_lossy().into_owned()], Some("/usr/bin/node"));
        assert_eq!(a(&tmp.0.join("bin/gemini")), Some(Agent::Gemini));
        assert_eq!(a(&fake.join("gemini")), None);
    }

    /// Sessions idle for 7 hours with no tab and nothing attached, started different ways.
    #[test]
    fn only_daemon_hosted_or_orphaned_sessions_go_stale() {
        let fp = FakeProc::new();
        let now = util::now();
        let uid = my_uid();
        let boot = 1795000000.0;
        let st = |ago: f64| ((now - ago - boot) * HZ as f64) as u64;
        let seven_h = 7.0 * 3600.0;
        fp.add(100, 1, &[V, "daemon", "run"], V, 100_000, 1, st(86400.0), uid);
        fp.add(110, 100, &["claude", "bg-pty-host", "--bg-pty-host", "/tmp/x.sock"], V, 40_000, 1, st(seven_h), uid);
        fp.add(111, 110, &["claude", "bg-spare", "--bg-spare", "/tmp/x.claim.sock"], V, 200_000, 1, st(seven_h), uid); // daemon-hosted
        fp.add(120, 1, &["claude"], V, 200_000, 1, st(seven_h), uid); // terminal closed, left with init
        fp.add(130, 1, &["claude", "-p", "summarise this"], V, 200_000, 1, st(seven_h), uid); // headless
        fp.add(140, 1, &["claude", "mcp", "serve"], V, 200_000, 1, st(seven_h), uid); // serving another app
        fp.add(149, 1, &["/usr/share/code/code"], "/usr/share/code/code", 300_000, 1, st(seven_h), uid);
        fp.add(150, 149, &["claude"], V, 200_000, 1, st(seven_h), uid); // under an editor
        fp.add(160, 1, &["claude", "--model", "opus"], V, 100_000, 1, st(seven_h), uid); // no session file: a stray
        let mut live = HashMap::new();
        for (pid, sid) in [(111u32, "s-bg"), (120, "s-orphan"), (130, "s-print"), (140, "s-mcp"), (150, "s-editor")] {
            live.insert(sid.to_string(), LiveInfo { pid, session_id: sid.into(), alive: true, status: Some("idle".into()), status_updated_at: Some(now - seven_h), ..Default::default() });
        }
        let ex = Explorer::new(fp.root());
        let view = |i: &LiveInfo| SessionView { title: Some(i.session_id.clone()), state: Some(State::Idle), idle_since: i.status_updated_at, used: true, ..Default::default() };
        let locate = |_: u32, _: Option<&str>| -> Option<Location> { None };
        let notes = HashMap::new();
        let o = ex.processes(&Inputs { live: &live, view: &view, locate: &locate, notes: &notes, now });
        let stale: Vec<u32> = o.procs.iter().filter(|p| p.stale_reason.is_some()).map(|p| p.pid).collect();
        assert_eq!(stale, [111, 120]);
        assert_eq!(o.stale_count, 2);
        let stray = o.procs.iter().find(|p| p.pid == 160).unwrap();
        assert_eq!((stray.kind, stray.stale_reason.as_deref()), (ProcKind::Stray, None));
    }

    /// Inside a toolbox /proc/<pid>/exe can't be read and argv[0] is a bare `claude`: such a process
    /// counts only when a live session file names it with the same start time.
    #[test]
    fn unreadable_exe_counts_only_when_a_live_session_file_vouches_for_it() {
        let fp = FakeProc::new();
        let uid = my_uid();
        fp.add(610, 1, &["claude", "--resume", "x"], "", 90_000, 1, 5_000, uid);
        fp.add(620, 1, &["claude"], "", 90_000, 1, 6_000, uid);
        fp.add(630, 1, &["claude"], "", 90_000, 1, 7_000, uid);
        let ex = Explorer::new(fp.root());
        let mut live = HashMap::new();
        let entry =
            |pid: u32, start: Option<&str>| LiveInfo { pid, session_id: format!("s{pid}"), alive: true, name: Some(format!("t{pid}")), proc_start: start.map(Into::into), ..Default::default() };
        live.insert("s610".into(), entry(610, Some("5000")));
        live.insert("s620".into(), entry(620, Some("1234"))); // the pid was reused
        live.insert("s630".into(), entry(630, None)); // no start time to compare
        let p = |pid| read_proc(fp.root(), pid).unwrap();
        assert_eq!(p(610).exe, None);
        assert_eq!(identify(&p(610), &live), Some(Agent::Claude));
        assert_eq!(identify(&p(620), &live), None);
        assert_eq!(identify(&p(630), &live), None);
        assert_eq!(identify(&p(610), &HashMap::new()), None);
        let view = |i: &LiveInfo| SessionView { title: i.name.clone(), state: Some(State::Idle), used: true, ..Default::default() };
        let locate = |_: u32, _: Option<&str>| -> Option<Location> { None };
        let notes = HashMap::new();
        let o = ex.processes(&Inputs { live: &live, view: &view, locate: &locate, notes: &notes, now: util::now() });
        assert_eq!(o.procs.iter().map(|r| r.pid).collect::<Vec<_>>(), [610]);
        // Stopping keeps its checks: the vouched-for process can be stopped, the others can't.
        let actor = FakeActor { calls: Mutex::new(vec![]) };
        let title = |i: &LiveInfo| i.name.clone();
        let r = stop(&ex, &actor, &live, &title, 610, Some(5_000), StopHow::Terminate, false);
        assert!(r.ok, "{r:?}");
        assert_eq!(r.message, "Wake asked 't610' (PID 610) to exit. If it is still running in 5 seconds, Force is offered.");
        assert_eq!(stop(&ex, &actor, &live, &title, 610, Some(5_001), StopHow::Terminate, false).message, ALREADY_ENDED);
        assert!(stop(&ex, &actor, &live, &title, 620, None, StopHow::Terminate, false).message.contains("not an agent"));
        assert!(stop(&ex, &actor, &live, &title, 630, None, StopHow::Kill, false).message.contains("not an agent"));
        assert_eq!(actor.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn stop_refuses_a_different_process_on_the_same_pid() {
        let fp = FakeProc::new();
        let now = util::now();
        let live = machine(&fp, now);
        let ex = Explorer::new(fp.root());
        let actor = FakeActor { calls: Mutex::new(vec![]) };
        let title = |i: &LiveInfo| i.name.clone();
        let shown = read_proc(fp.root(), 500).unwrap().start_ticks;
        for how in [StopHow::Terminate, StopHow::Kill] {
            for dry in [true, false] {
                let r = stop(&ex, &actor, &live, &title, 500, Some(shown + 1), how, dry);
                assert!(!r.ok);
                assert_eq!(r.message, ALREADY_ENDED);
                assert!(r.steps.is_empty());
            }
        }
        let r = stop(&ex, &actor, &live, &title, 201, Some(read_proc(fp.root(), 201).unwrap().start_ticks + 7), StopHow::AgentStop, false);
        assert_eq!(r.message, ALREADY_ENDED);
        // a pid that is gone, with the start time given, reads the same
        assert_eq!(stop(&ex, &actor, &live, &title, 4242, Some(1), StopHow::Terminate, false).message, ALREADY_ENDED);
        assert!(actor.calls.lock().unwrap().is_empty(), "a mismatched stop reached the actor");
        // the very process that was shown is stopped, and the actor gets that start time
        let r = stop(&ex, &actor, &live, &title, 500, Some(shown), StopHow::Terminate, false);
        assert!(r.ok, "{r:?}");
        assert_eq!(actor.calls.lock().unwrap().clone(), [format!("signal 500 {shown} {}", libc::SIGTERM)]);
    }

    /// The real actor re-checks the start time after opening a pidfd. Pointed at a fake /proc that
    /// gives this test process a different start time, it must refuse before sending anything. (The
    /// signal number is 0, the null signal, so even a bug here would deliver nothing.)
    #[test]
    fn real_actor_refuses_on_start_time_mismatch() {
        let fp = FakeProc::new();
        let me = std::process::id();
        let real_start = read_proc(Path::new("/proc"), me).unwrap().start_ticks;
        fp.add(me, 1, &["wake-test"], "/usr/bin/true", 1, 1, real_start + 12345, my_uid());
        let actor = RealActor { root: fp.root().to_path_buf() };
        let e = actor.signal(me, real_start, 0).unwrap_err();
        assert!(e.contains("different process"), "{e}");
    }

    /// A small machine for the spare and subagent rules: the jobs folder, the live entries and
    /// the views are made up per test; /proc is a fake.
    struct Rig {
        fp: FakeProc,
        jobs: TmpDir,
        live: HashMap<String, LiveInfo>,
        now: f64,
    }

    impl Rig {
        fn new() -> Rig {
            Rig { fp: FakeProc::new(), jobs: TmpDir::new("jobs"), live: HashMap::new(), now: util::now() }
        }
        fn st(&self, ago: f64) -> u64 {
            ((self.now - ago - 1795000000.0) * HZ as f64) as u64
        }
        fn add(&self, pid: u32, ppid: u32, argv: &[&str], exe: &str, rss_kb: u64, ago: f64) {
            self.fp.add(pid, ppid, argv, exe, rss_kb, 1, self.st(ago), my_uid());
        }
        fn entry(&mut self, pid: u32, sid: &str, job: Option<&str>, spare: bool, idle_for: f64) {
            self.live.insert(
                sid.into(),
                LiveInfo {
                    pid,
                    session_id: sid.into(),
                    alive: true,
                    status: Some("idle".into()),
                    status_updated_at: Some(self.now - idle_for),
                    name: Some(sid.into()),
                    kind: Some(if job.is_some() { "bg" } else { "interactive" }.into()),
                    job_id: job.map(Into::into),
                    spare,
                    ..Default::default()
                },
            );
        }
        fn job(&self, id: &str, record: &str) {
            fs::create_dir_all(self.jobs.0.join(id)).unwrap();
            fs::write(self.jobs.0.join(id).join("state.json"), record).unwrap();
        }
        /// Sessions whose transcript has no prompts (title None, `used` false), idle as their entry says.
        fn run(&self) -> ProcOverview {
            let mut ex = Explorer::new(self.fp.root());
            ex.jobs_dir = Some(self.jobs.0.clone());
            let view = |i: &LiveInfo| SessionView { title: None, project_name: Some("wake".into()), state: Some(State::Idle), idle_since: i.status_updated_at, used: false };
            let locate = |_: u32, _: Option<&str>| -> Option<Location> { None };
            let notes = HashMap::new();
            ex.processes(&Inputs { live: &self.live, view: &view, locate: &locate, notes: &notes, now: self.now })
        }
    }

    fn row(o: &ProcOverview, pid: u32) -> &AgentProc {
        o.procs.iter().find(|p| p.pid == pid).unwrap_or_else(|| panic!("no row {pid}"))
    }

    /// A spare a job claimed, whose transcript looks unused because the work is a subagent's (its
    /// transcript sits in the parent's subagents folder), is a session, not a spare; its subagent
    /// processes are folded into it, named by the job's `fan`, and never stale.
    #[test]
    fn a_claimed_spare_running_a_subagent_is_a_session_and_its_subagents_fold_in() {
        let mut r = Rig::new();
        let h = 3600.0;
        r.add(100, 1, &[V, "daemon", "run"], V, 100_000, 30.0 * h);
        r.add(110, 100, &["claude", "bg-pty-host", "--bg-pty-host", "/gone/x.pty.sock"], V, 40_000, 8.0 * h);
        r.add(111, 110, &["claude", "bg-spare", "--bg-spare", "/gone/x.claim.sock"], V, 300_000, 8.0 * h);
        r.add(112, 111, &["/bin/bash", "-c", "claude -p 'review this'"], "/bin/bash", 3_000, 60.0);
        r.add(113, 112, &["claude", "-p", "review this"], V, 200_000, 60.0);
        r.add(114, 111, &["/bin/bash", "-c", "codex exec go"], "/bin/bash", 3_000, 600.0);
        r.add(115, 114, &["codex", "exec", "go"], "/usr/bin/codex", 50_000, 600.0);
        r.add(116, 115, &["/bin/sh", "-c", "claude"], "/bin/sh", 1_000, 300.0);
        r.add(117, 116, &["claude"], V, 90_000, 300.0); // a subagent's subagent
        r.entry(111, "sess-claimed", Some("jobC"), false, 7.0 * h);
        r.entry(113, "sess-print", None, false, 7.0 * h);
        let t = |ago: f64| (r.now - ago) * 1000.0;
        r.job("jobC", &format!(
            r#"{{"name":"x","fan":[{{"id":"a1","kind":"agent","label":"Review the  parser","startedAt":{}}},{{"id":"s1","kind":"shell","label":"cargo test","startedAt":{}}},{{"id":"a2","kind":"agent","label":"Port codex","startedAt":{}}},{{"id":"a0","kind":"agent","label":"Finished one","startedAt":1,"doneAt":2}}]}}"#,
            t(61.0), t(60.0), t(598.0)
        ));
        let o = r.run();
        let s = row(&o, 111);
        assert_eq!(s.kind, ProcKind::Session, "a claimed spare is a session");
        assert_eq!(s.helpers.as_deref(), Some(&["Review the parser".to_string(), "Port codex".to_string()][..]));
        let mut kids = s.children.clone();
        kids.sort();
        assert_eq!(kids, [110, 113, 115, 117]);
        assert_eq!(s.rss_bytes, (300_000 + 40_000 + 200_000 + 50_000 + 90_000) * 1024);
        // idle 7 h with nothing attached: the session itself may be stale, and says so plainly
        assert!(s.stale_reason.as_deref().unwrap().starts_with("idle for 7 h"), "{:?}", s.stale_reason);
        for (pid, title) in [(113, Some("Review the parser")), (115, Some("Port codex")), (117, None)] {
            let x = row(&o, pid);
            assert_eq!(x.kind, ProcKind::Subagent, "{pid}");
            assert_eq!(x.stale_reason, None, "{pid}");
            assert_eq!(x.title.as_deref(), title, "{pid}");
        }
        assert_eq!(row(&o, 113).state, State::Idle, "its own entry says idle");
        assert_eq!(row(&o, 115).state, State::Working, "the job lists it as running");
        assert_eq!(row(&o, 113).session_id.as_deref(), Some("sess-print"));
        // listed after their session, never as spares, strays or sessions of their own
        let pids: Vec<u32> = o.procs.iter().map(|p| p.pid).collect();
        let at = |p: u32| pids.iter().position(|x| *x == p).unwrap();
        assert!(at(111) < at(113) && at(111) < at(115) && at(111) < at(117));
        assert!(o.procs.iter().all(|p| !matches!(p.kind, ProcKind::Spare | ProcKind::Stray)));
        // memory counted once; only the session itself is stale
        let kb: u64 = [100u64, 110, 111, 113, 115, 117].iter().map(|p| read_proc(r.fp.root(), *p as u32).unwrap().rss_bytes).sum();
        assert_eq!(o.total_rss_bytes, kb);
        assert_eq!((o.stale_count, o.stale_rss_bytes), (1, s.rss_bytes));
    }

    #[test]
    fn a_spare_is_what_its_session_entry_says() {
        let mut r = Rig::new();
        let h = 3600.0;
        let sock = r.fp.root().join("w.claim.sock");
        fs::write(&sock, "").unwrap();
        let sock = sock.to_string_lossy().into_owned();
        r.add(100, 1, &[V, "daemon", "run"], V, 100_000, 30.0 * h);
        r.add(120, 100, &["claude", "bg-spare", "--bg-spare", "/gone/a.claim.sock"], V, 180_000, 2.0 * h);
        r.add(121, 100, &["claude", "bg-spare", "--bg-spare", &sock], V, 180_000, 2.0 * h);
        r.add(122, 100, &["claude", "bg-spare", "--bg-spare", "/gone/b.claim.sock"], V, 180_000, 2.0 * h);
        r.add(123, 100, &["claude", "bg-spare", "--bg-spare", "/gone/c.claim.sock"], V, 180_000, 2.0 * h);
        r.entry(120, "s-spare", None, true, 2.0 * h); // says spare: true
        r.entry(123, "s-used", Some("jobU"), false, 60.0); // claimed, nothing in its transcript
        let o = r.run();
        assert_eq!(row(&o, 120).kind, ProcKind::Spare);
        assert!(row(&o, 120).stale_reason.as_deref().unwrap().starts_with("a spare started 2 h ago"));
        assert_eq!(row(&o, 121).kind, ProcKind::Spare, "no entry yet, claim socket waiting: unclaimed");
        assert_eq!(row(&o, 122).kind, ProcKind::Stray, "no entry and claimed: not a spare");
        assert_eq!(row(&o, 123).kind, ProcKind::Session, "an entry without spare: true is no spare, however unused");
        assert_eq!(row(&o, 123).helpers.as_deref(), None, "no job record");
    }

    /// The host above a background session, `claude attach` clients and Claude's daemon are not
    /// sessions, so what runs under them is no subagent; nor is the same agent restarting itself.
    #[test]
    fn hosts_attach_clients_and_restarts_do_not_make_subagents() {
        let mut r = Rig::new();
        r.add(50, 1, &["agent-notify", "keep", "claude", "attach", "jobA"], "/u/bin/agent-notify", 2_000, 900.0);
        r.add(51, 50, &["claude", "attach", "jobA"], V, 100_000, 900.0);
        r.add(52, 51, &[V, "daemon", "run"], V, 100_000, 900.0);
        r.add(53, 52, &["claude", "bg-pty-host", "--bg-pty-host", "/gone/s"], V, 40_000, 900.0);
        r.add(54, 53, &["claude", "bg-spare", "--bg-spare", "/gone/a.claim.sock"], V, 300_000, 900.0);
        r.add(60, 1, &["node", "/u/lib/node_modules/@openai/codex/bin/codex.js"], "/usr/bin/node", 50_000, 100.0);
        r.add(61, 60, &["codex"], "/u/lib/node_modules/@openai/codex/vendor/x86_64-unknown-linux-musl/codex/codex", 50_000, 100.0);
        r.entry(54, "s-a", Some("jobA"), false, 60.0);
        let o = r.run();
        assert!(o.procs.iter().all(|p| p.kind != ProcKind::Subagent), "{:?}", o.procs.iter().map(|p| (p.pid, p.kind)).collect::<Vec<_>>());
        assert_eq!(row(&o, 54).kind, ProcKind::Session);
        let mut kids = row(&o, 54).children.clone();
        kids.sort();
        assert_eq!(kids, [51, 53]);
    }

    #[test]
    fn helpers_come_from_the_job_fan_in_either_form() {
        let v = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap();
        let arr = v(
            r#"{"fan":[{"kind":"agent","label":"Build\n the  view"},{"kind":"shell","label":"ls"},{"kind":"todo","label":"t"},{"kind":"agent","label":"Done","doneAt":5},{"kind":"agent","label":null},null,{"kind":"agent","label":"Second"}]}"#,
        );
        assert_eq!(helpers(&arr), ["Build the view", "Second"]);
        let obj = v(r#"{"fan":{"a1":{"kind":"agent","label":"Look at window"},"a2":{"kind":"agent","label":"Learn subagents","doneAt":null}}}"#);
        assert_eq!(helpers(&obj), ["Look at window", "Learn subagents"]);
        // a malformed entry ends the list there; a long label is cut to 60 characters
        let bad = v(&format!(r#"{{"fan":[{{"kind":"agent","label":"{}"}},7,{{"kind":"agent","label":"never"}}]}}"#, "x".repeat(80)));
        assert_eq!(helpers(&bad), ["x".repeat(60)]);
        assert!(helpers(&v(r#"{"fan":null}"#)).is_empty());
        let many: Vec<String> = (0..12).map(|i| format!(r#"{{"kind":"agent","label":"h{i}"}}"#)).collect();
        assert_eq!(helpers(&v(&format!(r#"{{"fan":[{}]}}"#, many.join(",")))).len(), 8);
    }

    /// `claude -p` under an interactive session that has been idle 7 hours, with no tab: the
    /// session may be stale, the print run under it never is, and it is no stray.
    #[test]
    fn a_print_run_under_a_session_is_not_stale() {
        let mut r = Rig::new();
        let h = 3600.0;
        r.add(70, 1, &["claude"], V, 200_000, 8.0 * h);
        r.add(71, 70, &["/bin/bash", "-c", "claude -p x"], "/bin/bash", 2_000, 7.5 * h);
        r.add(72, 71, &["claude", "-p", "x"], V, 150_000, 7.5 * h);
        r.entry(70, "s-top", None, false, 7.0 * h);
        r.entry(72, "s-p", None, false, 7.0 * h);
        let o = r.run();
        assert_eq!((row(&o, 72).kind, row(&o, 72).stale_reason.as_deref()), (ProcKind::Subagent, None));
        assert_eq!(row(&o, 70).children, [72]);
        assert_eq!(o.stale_count, 1);
        assert_eq!(o.stale_rss_bytes, (200_000 + 150_000) * 1024);
    }

    #[test]
    fn resources_add_up_each_sessions_processes_by_project() {
        let fp = FakeProc::new();
        let o = run(&fp, util::now());
        let project_of = |sid: &str| (sid == "sess-A" || sid == "sess-B").then(|| "alpha".to_string());
        let r = resources(&o, &project_of);
        let a = r.session("sess-A").unwrap();
        assert_eq!(a.pid, 201);
        assert_eq!(a.rss_bytes, (300_000 + 40_000 + 70_000) * 1024);
        assert_eq!(a.own_rss_bytes, 300_000 * 1024);
        assert_eq!(a.pids[0], 201);
        assert_eq!(a.pids.len(), 3);
        assert_eq!(a.cpu_percent, None); // one sample only
        let p = r.project("alpha").unwrap();
        assert_eq!(p.sessions, 2);
        assert_eq!(p.rss_bytes, a.rss_bytes + r.session("sess-B").unwrap().rss_bytes);
        assert!(r.projects.iter().any(|p| p.project.is_none()));
        assert_eq!(r.total_rss_bytes, o.total_rss_bytes);
    }
}
