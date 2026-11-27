//! Live state of agent sessions.
//!
//! Sources (read only):
//!   ~/.claude/sessions/<pid>.json   pid, sessionId, cwd, status (busy|idle|…), statusUpdatedAt (ms), name, kind, jobId, spare
//!   /proc/<pid>/stat                alive check; start time must match procStart when present
//!   <runtime>/working/<pid>         agent-notify: line 1 name, line 2 "idle" or working, lines 3+ helpers
//!   <runtime>/resume/<pid>          agent-notify: one line, the resume command
//!
//! Never opens ~/.claude/sessions/*.key.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::Path;

use serde_json::Value;

use crate::index::{self, Acc};
use crate::store::{self, Paths, ProjectRec};
use crate::types::{SessionSummary, State, WorkingNote};
use crate::util::{self, ago};

/// Busy, but nothing updated or written for this long -> unknown.
pub const STALE_BUSY: f64 = 30.0 * 60.0;
/// An agent-notify note newer than the status file by this much, and disagreeing.
pub const DISAGREE_GRACE: f64 = 60.0;

#[derive(Debug, Clone, Default)]
pub struct LiveInfo {
    pub pid: u32,
    pub session_id: String,
    pub cwd: Option<String>,
    pub alive: bool,
    pub status: Option<String>,
    /// Epoch seconds.
    pub status_updated_at: Option<f64>,
    pub started_at: Option<f64>,
    pub name: Option<String>,
    pub kind: Option<String>,
    pub job_id: Option<String>,
    /// "idle" | "working", from agent-notify's note.
    pub notify: Option<String>,
    pub notify_at: Option<f64>,
    pub note: Option<WorkingNote>,
    pub resume_command: Option<String>,
    /// The entry says `"spare": true`: a background session Claude started ahead of time that no
    /// job has taken yet. The only sign of a spare Wake trusts (agent-notify's `is_spare`).
    pub spare: bool,
    /// The process start time the entry recorded (`procStart`, clock ticks as text), when it has one.
    /// `alive` already means it matched.
    pub proc_start: Option<String>,
    /// A running Codex process holding this session's rollout file (no session file of its own:
    /// only `pid`, `session_id` and `alive` are known).
    pub codex: bool,
    /// Kept from the last pass because the session file didn't parse this time.
    pub carried: bool,
}

/// sessionId -> live info for every running Codex process that holds a session open.
pub fn read_codex_live(proc_root: &Path, uid: u32) -> HashMap<String, LiveInfo> {
    crate::procs::codex_sessions(proc_root, uid).into_iter().map(|(pid, sid)| (sid.clone(), LiveInfo { pid, session_id: sid, alive: true, codex: true, ..Default::default() })).collect()
}

/// Process start time in clock ticks (field 22 of /proc/<pid>/stat), as text.
pub fn proc_start(proc_root: &Path, pid: u32) -> Option<String> {
    let data = fs::read_to_string(proc_root.join(pid.to_string()).join("stat")).ok()?;
    let end = data.rfind(')')?;
    data.get(end + 2..)?.split_whitespace().nth(19).map(String::from)
}

pub fn pid_alive(proc_root: &Path, pid: u32, expected_start: Option<&str>) -> bool {
    if pid == 0 || !proc_root.join(pid.to_string()).exists() {
        return false;
    }
    match expected_start {
        Some(e) if !e.is_empty() => proc_start(proc_root, pid).as_deref() == Some(e),
        // No start time to check: the pid may have been reused, so it must at least be an agent
        // (a bare `claude` when the executable can't be read, as inside a distrobox).
        _ => {
            let argv = crate::procs::read_cmdline(proc_root, pid);
            match fs::read_link(proc_root.join(pid.to_string()).join("exe")) {
                Ok(exe) => crate::procs::agent_of(&argv, Some(exe.to_string_lossy().trim_end_matches(" (deleted)"))).is_some(),
                Err(_) => crate::procs::agent_of(&argv, None).is_some() || argv.first().is_some_and(|a| a.rsplit('/').next() == Some("claude")),
            }
        }
    }
}

fn read_note_file(path: &Path) -> Option<(Vec<String>, f64)> {
    use std::os::unix::fs::MetadataExt;
    let st = fs::metadata(path).ok()?;
    let mut buf = Vec::new();
    fs::File::open(path).ok()?.take(4096).read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    let lines = text.lines().map(String::from).collect();
    Some((lines, st.mtime() as f64 + st.mtime_nsec() as f64 / 1e9))
}

/// agent-notify's working note for a pid: (note, its mtime).
pub fn read_working_note(paths: &Paths, pid: u32) -> Option<(WorkingNote, f64)> {
    let (lines, at) = read_note_file(&paths.runtime.join("working").join(pid.to_string()))?;
    Some((parse_note(&lines), at))
}

pub fn parse_note(lines: &[String]) -> WorkingNote {
    WorkingNote {
        name: lines.first().map(|l| l.trim().to_string()).unwrap_or_default(),
        idle: lines.get(1).is_some_and(|l| l.trim() == "idle"),
        helpers: lines.iter().skip(2).map(|l| l.trim()).filter(|l| !l.is_empty()).map(String::from).collect(),
    }
}

pub fn read_resume_note(paths: &Paths, pid: u32) -> Option<String> {
    let (lines, _) = read_note_file(&paths.runtime.join("resume").join(pid.to_string()))?;
    lines.first().map(|l| l.trim().to_string()).filter(|l| !l.is_empty())
}

/// sessionId -> live info for every session file in ~/.claude/sessions.
pub fn read_live(paths: &Paths, proc_root: &Path) -> HashMap<String, LiveInfo> {
    read_live_after(paths, proc_root, &HashMap::new())
}

/// [`read_live`], given the last pass's result: a session file caught mid-rewrite (it doesn't
/// parse) keeps that pid's last entry for one pass, so its row doesn't flicker.
pub fn read_live_after(paths: &Paths, proc_root: &Path, prev: &HashMap<String, LiveInfo>) -> HashMap<String, LiveInfo> {
    let d = paths.claude.join("sessions");
    let Ok(rd) = fs::read_dir(&d) else { return HashMap::new() };
    let mut out: HashMap<String, LiveInfo> = HashMap::new();
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        let Some(stem) = n.strip_suffix(".json") else { continue };
        if stem.is_empty() || !stem.bytes().all(|c| c.is_ascii_digit()) || store::is_forbidden(Path::new(&n)) {
            continue;
        }
        let Some(rec) = store::read_json::<Value>(&d.join(&n)) else {
            let pid: u32 = stem.parse().unwrap_or(0);
            if let Some(old) = prev.values().find(|l| l.pid == pid && !l.codex && !l.carried && !out.contains_key(&l.session_id)) {
                let alive = pid_alive(proc_root, pid, old.proc_start.as_deref());
                out.insert(old.session_id.clone(), LiveInfo { alive, carried: true, ..old.clone() });
            }
            continue;
        };
        let Some(sid) = rec.get("sessionId").and_then(Value::as_str).filter(|s| !s.is_empty()) else { continue };
        let pid = rec.get("pid").and_then(Value::as_u64).map(|p| p as u32).or_else(|| stem.parse().ok()).unwrap_or(0);
        let proc_start_s = match rec.get("procStart") {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Number(n)) => Some(n.to_string()),
            _ => None,
        };
        let alive = pid_alive(proc_root, pid, proc_start_s.as_deref());
        let raw = read_note_file(&paths.runtime.join("working").join(pid.to_string()));
        let notify_at = raw.as_ref().map(|r| r.1);
        // Python: a note says idle/working only when it has a second line.
        let notify = raw.as_ref().filter(|r| r.0.len() >= 2).map(|r| if r.0[1].trim() == "idle" { "idle" } else { "working" }.to_string());
        let note = raw.map(|r| parse_note(&r.0));
        let str_of = |k: &str| rec.get(k).and_then(Value::as_str).map(String::from);
        let upd = rec.get("statusUpdatedAt").or_else(|| rec.get("updatedAt")).and_then(Value::as_f64);
        let info = LiveInfo {
            pid,
            session_id: sid.into(),
            cwd: str_of("cwd"),
            alive,
            status: str_of("status"),
            status_updated_at: upd.map(|u| u / 1000.0),
            started_at: rec.get("startedAt").and_then(Value::as_f64).map(|u| u / 1000.0),
            name: str_of("name"),
            kind: str_of("kind"),
            job_id: str_of("jobId"),
            notify,
            notify_at,
            note,
            resume_command: read_resume_note(paths, pid),
            spare: rec.get("spare") == Some(&Value::Bool(true)),
            proc_start: proc_start_s.filter(|s| !s.is_empty()),
            codex: false,
            carried: false,
        };
        let better = match out.get(sid) {
            None => true,
            Some(prev) => (info.alive, info.status_updated_at.unwrap_or(0.0)) > (prev.alive, prev.status_updated_at.unwrap_or(0.0)),
        };
        if better {
            out.insert(sid.into(), info);
        }
    }
    out
}

/// The session's own name, unless it is just its id (an unused session).
pub fn real_name(info: Option<&LiveInfo>) -> Option<String> {
    let info = info?;
    let name = info.name.as_deref().unwrap_or("");
    if name.is_empty() || info.session_id.starts_with(name) || name.chars().count() < 3 {
        return None;
    }
    Some(name.into())
}

/// In a session's evidence: its last turn stopped on a tool call with no closing words after it
/// (interrupted, crashed, or killed while working).
pub const MID_TASK: &str = "stopped mid-task";
/// In a session's evidence: its last turn ended with Claude's own closing words.
pub const FINISHED: &str = "finished its turn";

/// Requests for a decision that don't end in a question mark ("Next, say whether you want me to
/// …", "I need your yes to push main"). Lower case; matched anywhere in the text.
const DECISION: [&str; 22] = [
    "say whether",
    "tell me whether",
    "let me know whether",
    "let me know which",
    "say which",
    "tell me which",
    "need your yes",
    "need your ok",
    "need your go-ahead",
    "need your approval",
    "need your decision",
    "need a decision",
    "need your call",
    "your call on",
    "waiting for your",
    "waiting on your",
    "want me to",
    "should i ",
    "shall i ",
    "would you like",
    "please confirm",
    "which would you",
];

/// When `text` asks the person to decide something, the paragraph that asks (at most 400
/// characters, the boilerplate about recaps left out); None otherwise.
pub fn asks_decision(text: &str) -> Option<String> {
    let text = strip_config_note(text);
    let paras: Vec<&str> = text.split("\n\n").map(str::trim).filter(|p| !p.is_empty()).collect();
    let hit = paras.iter().rev().find(|p| {
        let l = p.to_lowercase();
        DECISION.iter().any(|d| l.contains(d))
    })?;
    let q = util::squash(hit);
    Some(if q.chars().count() <= 400 { q } else { format!("{}...", util::take_chars(&q, 397)) })
}

/// `text` without Claude's "(turn off recaps in /config)"-style aside.
pub fn strip_config_note(text: &str) -> String {
    let mut out = text.to_string();
    while let Some(i) = out.to_ascii_lowercase().find("in /config)") {
        let Some(open) = out[..i].rfind('(') else { break };
        let close = i + "in /config)".len();
        out.replace_range(open..close, "");
    }
    out.trim().to_string()
}

/// Last paragraph of a closing question, at most 400 characters.
pub fn question_text(text: &str) -> String {
    let mut para: Vec<&str> = Vec::new();
    for l in text.trim().split('\n') {
        if l.trim().is_empty() {
            para.clear();
        } else {
            para.push(l);
        }
    }
    let q = util::squash(&para.join("\n"));
    if q.chars().count() <= 400 { q } else { format!("...{}", util::last_chars(&q, 397)) }
}

/// Fill state, evidence, question, resume command (and cwd if missing) on a summary.
pub fn apply_live(summary: &mut SessionSummary, acc: Option<&Acc>, info: Option<&LiveInfo>, now: f64) {
    let last = summary.last_at.as_deref().and_then(util::parse_iso);
    summary.question = None;
    if let Some(cmd) = info.and_then(|i| i.resume_command.clone()) {
        summary.resume_command = Some(cmd);
    }
    if summary.agent == crate::types::Agent::Codex {
        use std::os::unix::fs::MetadataExt;
        if let Some(i) = info.filter(|i| i.alive) {
            // Running: never offered a reopen (a second `codex resume` of the same conversation).
            summary.pid = Some(i.pid);
            summary.state = State::Unknown;
            summary.evidence = "Codex is running it; Wake cannot see Codex live state".into();
            return;
        }
        let mtime = acc.and_then(|a| a.source.as_deref()).and_then(|s| fs::metadata(s).ok()).map(|m| m.mtime() as f64);
        match mtime {
            Some(m) if now - m < 120.0 => {
                summary.state = State::Unknown;
                summary.evidence = format!("Codex wrote to it {} ago; Wake cannot see Codex live state", ago(Some(now - m)));
            }
            _ => {
                summary.state = State::Ended;
                summary.evidence = match last {
                    Some(l) => format!("last active {} ago", ago(Some(now - l))),
                    None => "ended".into(),
                };
            }
        }
        return;
    }
    let Some(info) = info.filter(|i| i.alive) else {
        summary.state = State::Ended;
        // How its last turn ended, when the transcript says: mid-task (a tool call with no closing
        // words after it) or finished.
        let how = match acc.and_then(|a| a.last_kind.as_deref()) {
            Some("tool") => format!(", {MID_TASK}"),
            Some("text") => format!(", {FINISHED}"),
            _ => String::new(),
        };
        summary.evidence = if summary.deleted_by_claude {
            "ended; Claude deleted the transcript, Wake kept this summary".into()
        } else {
            match last {
                Some(l) => format!("ended {} ago{how}", ago(Some(now - l))),
                None => format!("ended{how}"),
            }
        };
        return;
    };
    summary.pid = Some(info.pid);
    summary.job_id = info.job_id.clone();
    if summary.cwd.is_none() {
        summary.cwd = info.cwd.clone();
    }
    let upd = info.status_updated_at;
    let newest = [upd, last].into_iter().flatten().fold(None, |a: Option<f64>, b| Some(a.map_or(b, |a| a.max(b))));
    let notify_newer = |want: &str| info.notify.as_deref() == Some(want) && matches!((info.notify_at, upd), (Some(n), Some(u)) if n - u > DISAGREE_GRACE);
    match info.status.as_deref() {
        Some("busy") => {
            if let Some(n) = newest.filter(|n| now - n > STALE_BUSY) {
                summary.state = State::Unknown;
                summary.evidence = format!("Claude says it's busy, but nothing has moved for {}", ago(Some(now - n)));
            } else if notify_newer("idle") {
                summary.state = State::Unknown;
                summary.evidence = "Claude's notes disagree: one says working, one says idle".into();
            } else {
                summary.state = State::Working;
                summary.evidence = match newest {
                    Some(n) => format!("working, last moved {} ago", ago(Some(now - n))),
                    None => "working".into(),
                };
            }
        }
        Some("idle") => {
            if notify_newer("working") {
                summary.state = State::Unknown;
                summary.evidence = "Claude's notes disagree: one says idle, one says working".into();
                return;
            }
            let text = acc.and_then(|a| a.last_text.as_deref());
            let t_at = acc.and_then(|a| a.last_text_at.as_deref()).unwrap_or("");
            let u_at = acc.and_then(|a| a.last_user_at.as_deref()).unwrap_or("");
            let closing = acc.and_then(|a| a.last_kind.as_deref()) == Some("text") && t_at >= u_at;
            // Claude's own recap, when it came after the person last wrote.
            let recap = acc.filter(|a| a.away_at.as_deref().unwrap_or("") >= u_at).and_then(|a| a.away_summary.as_deref());
            let asked = if closing && index::ends_with_question(text) {
                text.map(|t| (question_text(index::without_trailing_code(t)), "asked a question", t_at))
            } else if let Some(d) = text.filter(|_| closing).and_then(asks_decision) {
                Some((d, "asked you to decide", t_at))
            } else {
                recap.and_then(asks_decision).map(|d| (d, "its recap asks you to decide", acc.and_then(|a| a.away_at.as_deref()).unwrap_or("")))
            };
            if let Some((q, how, at)) = asked {
                summary.state = State::Waiting;
                summary.question = Some(q);
                summary.evidence = match util::parse_iso(at) {
                    Some(a) => format!("{how} {} ago", ago(Some(now - a))),
                    None => how.into(),
                };
            } else {
                summary.state = State::Idle;
                // One idle clock everywhere: the transcript's last activity. When Claude marked it
                // idle much later (a restart, an attach), that is said after it.
                let since = last.or(upd);
                let how = if acc.and_then(|a| a.last_kind.as_deref()) == Some("tool") { MID_TASK } else { FINISHED };
                let marked = match (last, upd) {
                    (Some(l), Some(u)) if u - l > 600.0 => format!("; Claude marked it idle {} ago", ago(Some(now - u))),
                    _ => String::new(),
                };
                summary.evidence = match since {
                    Some(s) => format!("{how}, idle for {}{marked}", ago(Some(now - s))),
                    None => how.into(),
                };
            }
        }
        other => {
            summary.state = State::Unknown;
            summary.evidence = match other {
                None => "Claude is running but doesn't say whether it's working".into(),
                Some(o) => format!("Claude is running but says '{}', which Wake doesn't know", util::take_chars(o, 30)),
            };
        }
    }
}

/// A summary for a live session the indexer has not seen yet.
pub fn live_only_summary(info: &LiveInfo, projects: &[ProjectRec], home: &Path, now: f64) -> SessionSummary {
    let mut acc = index::new_acc(&info.session_id, None, "claude");
    acc.cwd = info.cwd.clone();
    acc.launch_cwd = info.cwd.clone();
    if let Some(c) = info.cwd.as_deref().and_then(|c| store::norm_path(c, home)) {
        acc.cwd_seen.0.push((c, 1));
    }
    let mut s = index::base_summary(&acc, projects, home);
    s.title = info.name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| "New session".into());
    apply_live(&mut s, Some(&acc), Some(info), now);
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Indexer;
    use crate::store::Store;
    use crate::store::testenv::Env;

    struct Fx {
        env: Env,
        ix: Indexer,
        projects: Vec<ProjectRec>,
        pid: u32,
    }

    fn setup() -> Fx {
        let env = Env::new();
        let ix = Indexer::new(env.paths.clone());
        ix.run();
        let projects = Store::new(env.paths.clone()).load_projects();
        Fx { env, ix, projects, pid: std::process::id() }
    }

    impl Fx {
        fn write_session(&self, sid: &str, status: &str, proc_start_v: Option<&str>, updated: Option<f64>) {
            let ps = proc_start_v.map(String::from).or_else(|| proc_start(Path::new("/proc"), self.pid));
            let rec = serde_json::json!({"pid": self.pid, "sessionId": sid,
                "cwd": format!("{}/alpha", self.env.paths.projects.display()), "status": status,
                "statusUpdatedAt": ((updated.unwrap_or_else(util::now)) * 1000.0) as u64, "kind": "interactive",
                "name": "x", "procStart": ps});
            let d = self.env.paths.claude.join("sessions");
            fs::write(d.join(format!("{}.json", self.pid)), rec.to_string()).unwrap();
            // a key file sits next to it and must never be read: make it unreadable garbage
            fs::write(d.join(format!("{}.abc.key", self.pid)), "secret").unwrap();
        }

        fn state(&self, sid: &str) -> SessionSummary {
            let acc = self.ix.get_lite(sid).unwrap();
            let mut s = index::base_summary(&acc, &self.projects, &self.env.paths.home);
            let live = read_live(&self.env.paths, Path::new("/proc"));
            apply_live(&mut s, Some(&acc), live.get(sid), util::now());
            s
        }
    }

    #[test]
    fn idle_with_question_is_waiting() {
        let f = setup();
        f.write_session("sess-aaaa", "idle", None, None);
        let s = f.state("sess-aaaa");
        assert_eq!(s.state, State::Waiting);
        assert_eq!(s.question.as_deref(), Some("Shall I open a pull request for the parser change?"));
        assert!(s.evidence.starts_with("asked a question"));
        assert_eq!(s.pid, Some(f.pid));
    }

    #[test]
    fn idle_without_question() {
        let f = setup();
        f.write_session("sess-bbbb", "idle", None, None);
        let s = f.state("sess-bbbb");
        assert_eq!(s.state, State::Idle);
        assert_eq!(s.question, None);
    }

    #[test]
    fn busy_is_working_and_stale_busy_is_unknown() {
        let f = setup();
        f.write_session("sess-bbbb", "busy", None, None);
        assert_eq!(f.state("sess-bbbb").state, State::Working);
        f.write_session("sess-bbbb", "busy", None, Some(util::now() - 10.0 * 3600.0));
        let s = f.state("sess-bbbb");
        assert_eq!(s.state, State::Unknown);
        assert!(s.evidence.contains("busy"));
    }

    #[test]
    fn dead_or_reused_pid_is_ended() {
        let f = setup();
        f.write_session("sess-aaaa", "idle", Some("1"), None);
        assert_eq!(f.state("sess-aaaa").state, State::Ended);
        assert_eq!(f.state("sess-bbbb").state, State::Ended);
        assert!(f.state("sess-bbbb").evidence.starts_with("ended "));
    }

    #[test]
    fn resume_note_and_disagreeing_notify() {
        let f = setup();
        f.write_session("sess-bbbb", "idle", None, Some(util::now() - 600.0));
        fs::write(f.env.paths.runtime.join("resume").join(f.pid.to_string()), "claude --resume sess-bbbb\n").unwrap();
        fs::write(f.env.paths.runtime.join("working").join(f.pid.to_string()), "claude: alpha\nworking\nReview parser\n").unwrap();
        let s = f.state("sess-bbbb");
        assert_eq!(s.resume_command.as_deref(), Some("claude --resume sess-bbbb"));
        assert_eq!(s.state, State::Unknown);
        assert!(s.evidence.contains("disagree"));
        let live = read_live(&f.env.paths, Path::new("/proc"));
        let note = live["sess-bbbb"].note.clone().unwrap();
        assert_eq!((note.name.as_str(), note.idle, note.helpers.len()), ("claude: alpha", false, 1));
    }

    #[test]
    fn decisions_are_asked_without_a_question_mark() {
        let human = "Shortlisted three venues and drafted the pitch.\n\nNext, say whether you want me to find a shortlist of editors. (turn off recaps in /config)";
        assert_eq!(asks_decision(human).as_deref(), Some("Next, say whether you want me to find a shortlist of editors."));
        assert!(asks_decision("Merged the branch. I need your yes to push main.").is_some());
        assert!(asks_decision("Fixed the parser; all 56 tests pass.").is_none());
        assert!(asks_decision("Done. Next: write the pack.").is_none());
        assert_eq!(strip_config_note("All merged. (disable recaps in /config)"), "All merged.");
        assert_eq!(strip_config_note("Ok (Turn off recaps in /config) then"), "Ok  then");
    }

    #[test]
    fn idle_says_whether_it_finished_or_stopped_mid_task_on_one_clock() {
        let f = setup();
        // Claude marked it idle long after its last words (a restart): the transcript's clock wins.
        f.write_session("sess-bbbb", "idle", None, Some(util::now() - 60.0));
        let acc = f.ix.get_lite("sess-bbbb").unwrap();
        let mut s = index::base_summary(&acc, &f.projects, &f.env.paths.home);
        let live = read_live(&f.env.paths, Path::new("/proc"));
        let mut a = (*acc).clone();
        a.last_kind = Some("tool".into());
        a.last_at = Some("2020-01-01T00:00:00Z".into());
        s.last_at = a.last_at.clone();
        apply_live(&mut s, Some(&a), live.get("sess-bbbb"), util::now());
        assert_eq!(s.state, State::Idle);
        assert!(s.evidence.starts_with(MID_TASK), "{}", s.evidence);
        assert!(s.evidence.contains("days") && s.evidence.contains("Claude marked it idle"), "{}", s.evidence);
        a.last_kind = Some("text".into());
        a.last_text = Some("All done; the tests pass.".into());
        a.last_text_at = Some("2020-01-01T00:00:00Z".into());
        a.last_user_at = Some("2019-12-31T23:00:00Z".into());
        apply_live(&mut s, Some(&a), live.get("sess-bbbb"), util::now());
        assert!(s.evidence.starts_with(FINISHED), "{}", s.evidence);
        // Its last words ask for a yes: waiting, though there is no question mark.
        a.last_text = Some("Committed locally. I need your yes to push main.".into());
        apply_live(&mut s, Some(&a), live.get("sess-bbbb"), util::now());
        assert_eq!(s.state, State::Waiting);
        assert_eq!(s.question.as_deref(), Some("Committed locally. I need your yes to push main."));
        // A recap after the last prompt that asks for a decision counts too.
        a.last_text = Some("Done.".into());
        a.away_summary = Some("Drafted it. Next, say whether you want me to send it.".into());
        a.away_at = Some("2020-01-01T00:05:00Z".into());
        apply_live(&mut s, Some(&a), live.get("sess-bbbb"), util::now());
        assert_eq!(s.state, State::Waiting);
        assert!(s.evidence.starts_with("its recap asks you to decide"));
        // ...but not a recap from before the person last wrote.
        a.last_user_at = Some("2020-01-01T00:10:00Z".into());
        a.last_text_at = Some("2020-01-01T00:11:00Z".into());
        apply_live(&mut s, Some(&a), live.get("sess-bbbb"), util::now());
        assert_eq!(s.state, State::Idle);
    }

    #[test]
    fn without_a_start_time_only_an_agent_process_counts_as_alive() {
        use crate::procs::tests::FakeProc;
        let fp = FakeProc::new();
        let uid = crate::procs::my_uid();
        fp.add(4100, 1, &["/usr/bin/python3", "x.py"], "/usr/bin/python3", 1000, 1, 5, uid); // a reused pid
        fp.add(4101, 1, &["/usr/bin/claude"], "/usr/bin/claude", 1000, 1, 5, uid);
        fp.add(4102, 1, &["claude", "--resume", "x"], "", 1000, 1, 5, uid); // in a distrobox
        assert!(!pid_alive(fp.root(), 4100, None));
        assert!(pid_alive(fp.root(), 4101, None));
        assert!(pid_alive(fp.root(), 4102, Some("")));
        assert!(pid_alive(fp.root(), 4100, Some("5")), "a matching start time is enough");
    }

    #[test]
    fn a_session_file_caught_mid_rewrite_keeps_its_last_entry_for_one_pass() {
        let f = setup();
        f.write_session("sess-bbbb", "idle", None, None);
        let first = read_live(&f.env.paths, Path::new("/proc"));
        assert!(first["sess-bbbb"].alive);
        let file = f.env.paths.claude.join("sessions").join(format!("{}.json", f.pid));
        fs::write(&file, "{\"pid\": ").unwrap();
        let second = read_live_after(&f.env.paths, Path::new("/proc"), &first);
        assert!(second["sess-bbbb"].alive && second["sess-bbbb"].carried);
        assert!(read_live_after(&f.env.paths, Path::new("/proc"), &second).is_empty(), "only for one pass");
        assert!(read_live(&f.env.paths, Path::new("/proc")).is_empty());
    }

    #[test]
    fn real_names_and_questions() {
        let mut i = LiveInfo { session_id: "8b1c4e7a-3f62".into(), name: Some("8b1c4e7a".into()), ..Default::default() };
        assert_eq!(real_name(Some(&i)), None);
        i.name = Some("unfinished-work-check".into());
        assert_eq!(real_name(Some(&i)).as_deref(), Some("unfinished-work-check"));
        assert_eq!(question_text("Done.\n\n  Shall I\n go on?  "), "Shall I go on?");
        assert_eq!(parse_note(&["n".into(), "idle".into(), "".into(), "h1".into()]).helpers, ["h1"]);
    }
}
