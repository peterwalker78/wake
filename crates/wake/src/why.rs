//! Reading a session at a glance: why it stopped, which of its subagents matter now, and how long
//! it has been idle. Pure functions over the overview's data, so every surface (Monitor, the side
//! panel, Agents, the switcher) says the same thing, and each is tested without a window.

use wake_core::{SessionSummary, State, SubagentCounts, SubagentSummary};

use crate::fmt;

/// Why a session isn't working, in a few words, and how loud to say it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reason {
    /// "wants a decision", "finished, handed back", "stopped mid-task (last ran 09:52)",
    /// "ended: finished, handed back".
    pub words: String,
    /// CSS class: "amber" (it needs you), "plain" or "dim".
    pub class: &'static str,
    /// The decision or question it is waiting on, when it is.
    pub decision: Option<String>,
}

/// "09:52" today, "yesterday 09:52", "12 Nov 09:52".
fn ran(at: Option<&str>) -> String {
    let w = fmt::when(at);
    w.strip_prefix("today ").map(str::to_string).unwrap_or(w)
}

/// What the core's evidence says about how the last turn ended (see wake-core `live::MID_TASK`).
fn mid_task(s: &SessionSummary) -> bool {
    s.evidence.contains(wake_core::live::MID_TASK)
}

fn finished_turn(s: &SessionSummary) -> bool {
    s.evidence.contains(wake_core::live::FINISHED)
}

/// Waiting on a plain question (it ends in "?"), rather than a decision asked for in words.
pub fn asks_question(s: &SessionSummary) -> bool {
    s.evidence.starts_with("asked a question") || s.question.as_deref().is_some_and(|q| wake_core::index::ends_with_question(Some(q)))
}

/// Why `s` stopped (or why Wake lost track of it), or None while it works.
pub fn stop_reason(s: &SessionSummary) -> Option<Reason> {
    let mid = || format!("stopped mid-task (last ran {})", ran(s.last_at.as_deref()));
    match s.state {
        State::Working => None,
        // The core's own evidence: "says busy, but nothing has changed for 6 h".
        State::Unknown => {
            let words = if s.evidence.is_empty() { "lost track".to_string() } else { format!("lost track: {}", s.evidence) };
            Some(Reason { words, class: "dim", decision: None })
        }
        State::Waiting => {
            let q = s.question.clone();
            let words = if asks_question(s) { "asks you" } else { "wants a decision" };
            Some(Reason { words: words.into(), class: "amber", decision: q })
        }
        State::Idle if mid_task(s) => Some(Reason { words: mid(), class: "plain", decision: None }),
        State::Idle => Some(Reason { words: "finished, handed back".into(), class: "plain", decision: None }),
        State::Ended => {
            let how = if s.deleted_by_claude {
                "Claude deleted the transcript".to_string()
            } else if mid_task(s) {
                mid()
            } else if finished_turn(s) {
                "finished, handed back".into()
            } else {
                format!("last ran {}", ran(s.last_at.as_deref()))
            };
            Some(Reason { words: format!("ended: {how}"), class: "dim", decision: None })
        }
    }
}

/// When Claude's recap (or a session's question) asks the person to decide something, that ask.
pub fn decision_in(text: &str) -> Option<String> {
    wake_core::live::asks_decision(text)
}

// ---------- subagents ----------

/// Running first (as they started), then the finished ones, the most recently finished first.
pub fn split_subs(subs: &[SubagentSummary]) -> (Vec<&SubagentSummary>, Vec<&SubagentSummary>) {
    let mut running: Vec<&SubagentSummary> = subs.iter().filter(|a| a.state == State::Working).collect();
    running.sort_by(|a, b| a.started_at.cmp(&b.started_at).then_with(|| a.agent_id.cmp(&b.agent_id)));
    let mut done: Vec<&SubagentSummary> = subs.iter().filter(|a| a.state != State::Working).collect();
    done.sort_by(|a, b| end_of(b).cmp(&end_of(a)).then_with(|| a.agent_id.cmp(&b.agent_id)));
    (running, done)
}

fn end_of(a: &SubagentSummary) -> Option<&str> {
    a.last_at.as_deref().or(a.started_at.as_deref())
}

/// Every subagent in the order every surface lists them: running first, then newest finished.
pub fn ordered_subs(subs: &[SubagentSummary]) -> Vec<SubagentSummary> {
    let (r, d) = split_subs(subs);
    r.into_iter().chain(d).cloned().collect()
}

/// The one line the finished ones fold into: "28 finished · last one 12 min ago".
pub fn finished_line(done: &[&SubagentSummary]) -> String {
    let last = done.first().and_then(|a| end_of(a)).map(|t| fmt::ago(Some(t))).filter(|a| !a.is_empty());
    match last {
        Some(a) => format!("{} finished · last one {a}", done.len()),
        None => format!("{} finished", done.len()),
    }
}

/// When a finished one finished: "finished 3 min ago".
pub fn finished_ago(a: &SubagentSummary) -> String {
    let at = fmt::ago(end_of(a));
    let word = if a.state == State::Ended { "finished" } else { "last seen" };
    if at.is_empty() { word.to_string() } else { format!("{word} {at}") }
}

/// The one way every surface counts subagents: "2 running · 30 subagents", "30 subagents, none
/// running", "1 subagent · running".
pub fn sub_count(c: &SubagentCounts) -> String {
    let all = fmt::pl(c.total, "subagent", "subagents");
    match c.running {
        0 => format!("{all}, none running"),
        r if c.total == 1 && r == 1 => format!("{all} · running"),
        r => format!("{r} running · {all}"),
    }
}

// ---------- one idle clock ----------

/// How long a session has been idle, on the one clock every surface uses: the transcript's last
/// activity; the process's own idea (when Claude marked it idle) only when there is no transcript.
pub fn idle_secs(last_at: Option<&str>, proc_idle: Option<u64>) -> Option<u64> {
    last_at.and_then(fmt::secs_since).map(|s| s as u64).or(proc_idle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wake_core::Agent;

    fn session(state: State, evidence: &str) -> SessionSummary {
        SessionSummary {
            id: "s".into(),
            agent: Agent::Claude,
            project: None,
            project_name: None,
            title: "t".into(),
            state,
            evidence: evidence.into(),
            started_at: None,
            last_at: Some("2026-11-12T09:52:00Z".into()),
            minutes: 0,
            prompts: 0,
            last_prompt: None,
            away_summary: None,
            question: None,
            subagents: SubagentCounts::default(),
            cwd: None,
            location: None,
            resume_command: None,
            job_id: None,
            pid: None,
            deleted_by_claude: false,
        }
    }

    #[test]
    fn stop_reasons_from_fixtures() {
        let at = ran(Some("2026-11-12T09:52:00Z"));
        assert_eq!(stop_reason(&session(State::Working, "live")), None);
        let mut w = session(State::Waiting, "its recap asks you to decide 4 min ago");
        w.question = Some("Next, say whether you want me to find a shortlist.".into());
        assert_eq!(stop_reason(&w), Some(Reason { words: "wants a decision".into(), class: "amber", decision: w.question.clone() }));
        w.evidence = "asked a question 3 min ago".into();
        assert_eq!(stop_reason(&w).unwrap().words, "asks you");
        w.evidence = "live".into();
        w.question = Some("Send it now?".into());
        assert_eq!(stop_reason(&w).unwrap().words, "asks you");
        let fin = stop_reason(&session(State::Idle, "finished its turn, idle for 2 h")).unwrap();
        assert_eq!((fin.words.as_str(), fin.class), ("finished, handed back", "plain"));
        let mid = stop_reason(&session(State::Idle, "stopped mid-task, idle for 38 h; Claude marked it idle 2 h ago")).unwrap();
        assert_eq!(mid.words, format!("stopped mid-task (last ran {at})"));
        assert_eq!(stop_reason(&session(State::Ended, "ended 3 h ago, stopped mid-task")).unwrap().words, format!("ended: stopped mid-task (last ran {at})"));
        assert_eq!(stop_reason(&session(State::Ended, "ended 3 h ago, finished its turn")).unwrap().words, "ended: finished, handed back");
        assert_eq!(stop_reason(&session(State::Ended, "ended 3 h ago")).unwrap().words, format!("ended: last ran {at}"));
        let mut gone = session(State::Ended, "ended; Claude deleted the transcript, Wake kept this summary");
        gone.deleted_by_claude = true;
        assert_eq!(stop_reason(&gone).unwrap().words, "ended: Claude deleted the transcript");
        assert_eq!(stop_reason(&session(State::Unknown, "status files disagree (busy vs idle)")).unwrap().words, "lost track: status files disagree (busy vs idle)");
    }

    #[test]
    fn decisions_found_in_recaps() {
        assert!(decision_in("Next, I need your yes to push main. (turn off recaps in /config)").is_some_and(|d| !d.contains("/config")));
        assert_eq!(decision_in("Tidied the parser; tests pass."), None);
    }

    fn sub(id: &str, state: State, started: &str, last: &str) -> SubagentSummary {
        SubagentSummary {
            agent_id: id.into(),
            task: id.into(),
            kind: String::new(),
            state,
            started_at: Some(started.into()),
            last_at: Some(last.into()),
            seconds: 60,
            tools: vec![],
            handed_back: None,
            live: None,
        }
    }

    #[test]
    fn running_subagents_come_first_and_finished_fold_newest_first() {
        let subs = vec![
            sub("old", State::Ended, "2026-11-20T08:00:00Z", "2026-11-20T08:05:00Z"),
            sub("run2", State::Working, "2026-11-20T10:00:00Z", "2026-11-20T10:01:00Z"),
            sub("new", State::Ended, "2026-11-20T09:00:00Z", "2026-11-20T09:30:00Z"),
            sub("run1", State::Working, "2026-11-20T09:50:00Z", "2026-11-20T10:02:00Z"),
            sub("lost", State::Unknown, "2026-11-20T07:00:00Z", "2026-11-20T07:01:00Z"),
        ];
        let (run, done) = split_subs(&subs);
        assert_eq!(run.iter().map(|a| a.agent_id.as_str()).collect::<Vec<_>>(), ["run1", "run2"]);
        assert_eq!(done.iter().map(|a| a.agent_id.as_str()).collect::<Vec<_>>(), ["new", "old", "lost"]);
        let ordered: Vec<String> = ordered_subs(&subs).into_iter().map(|a| a.agent_id).collect();
        assert_eq!(ordered, ["run1", "run2", "new", "old", "lost"]);
        let line = finished_line(&done);
        assert!(line.starts_with("3 finished · last one ") && line.ends_with(" ago"), "{line}");
        assert!(finished_ago(done[0]).starts_with("finished ") && finished_ago(done[2]).starts_with("last seen "));
        assert_eq!(finished_line(&[]), "0 finished");
    }

    #[test]
    fn one_count_everywhere() {
        assert_eq!(sub_count(&SubagentCounts { total: 30, running: 2 }), "2 running · 30 subagents");
        assert_eq!(sub_count(&SubagentCounts { total: 30, running: 0 }), "30 subagents, none running");
        assert_eq!(sub_count(&SubagentCounts { total: 1, running: 1 }), "1 subagent · running");
    }

    #[test]
    fn one_idle_clock_the_transcripts() {
        // Claude marked it idle 2 h ago, but nothing happened in the transcript for 38 h: 38 h.
        let last = glib_iso_hours_ago(38);
        let s = idle_secs(Some(&last), Some(2 * 3600)).unwrap();
        assert!((38 * 3600 - 5..=38 * 3600 + 5).contains(&s), "{s}");
        assert_eq!(idle_secs(None, Some(7200)), Some(7200));
        assert_eq!(idle_secs(None, None), None);
    }

    fn glib_iso_hours_ago(h: i64) -> String {
        let t = gtk4::glib::DateTime::now_utc().unwrap().add_hours(-(h as i32)).unwrap();
        t.format_iso8601().unwrap().to_string()
    }
}
