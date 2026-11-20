//! Sorting a table by a column: click a column's name to sort by it, again to reverse, a third
//! time to go back to Wake's own order (what needs you first, rows that keep their place).
//! Each view's choice is kept in Settings (`[sort]` in settings.toml) as "column" or
//! "column reversed".
//!
//! Every column has a natural direction, the one a first click gives: names A to Z, states what
//! needs you first, times newest first, sizes biggest first. Ties keep Wake's order, so rows
//! never swap while you read.

use std::cmp::Ordering;

use wake_core::*;

/// A view's sort: the column's key and whether it is reversed. None is Wake's own order.
pub type Sort = Option<(String, bool)>;

pub fn parse(s: &str) -> Sort {
    let mut it = s.split_whitespace();
    let key = it.next()?.to_string();
    let rev = it.next() == Some("reversed");
    Some((key, rev))
}

pub fn format(key: &str, rev: bool) -> String {
    if rev { format!("{key} reversed") } else { key.to_string() }
}

/// What a click on `key` does: sort by it, reverse it, then back to Wake's order.
pub fn next(cur: &Sort, key: &str) -> Sort {
    match cur {
        Some((k, false)) if k == key => Some((key.to_string(), true)),
        Some((k, true)) if k == key => None,
        _ => Some((key.to_string(), false)),
    }
}

/// The order in words, for the header's tooltip.
pub fn words(key: &str, rev: bool) -> &'static str {
    let (a, b) = match key {
        "project" | "task" | "name" => ("A to Z", "Z to A"),
        "state" => ("what needs you first", "what needs you last"),
        "last" => ("newest first", "oldest first"),
        "pid" => ("lowest first", "highest first"),
        "memory" => ("most memory first", "least memory first"),
        "disk" => ("biggest first", "smallest first"),
        "open" => ("most open first", "fewest open first"),
        "git" => ("most loose ends first", "fewest loose ends first"),
        _ => ("in order", "reversed"),
    };
    if rev { b } else { a }
}

fn needs(s: State) -> u8 {
    match s {
        State::Waiting => 0,
        State::Working => 1,
        State::Idle => 2,
        State::Unknown | State::Ended => 3,
    }
}

fn lower(s: Option<&str>) -> String {
    s.unwrap_or("\u{10FFFF}").to_lowercase()
}

/// Sessions (Monitor, History) by `key` ("project", "state", "last"); `base` is Wake's order,
/// which breaks ties.
pub fn sessions(rows: &mut [SessionSummary], key: &str, rev: bool, base: fn(&SessionSummary, &SessionSummary) -> Ordering) {
    let by = |a: &SessionSummary, b: &SessionSummary| -> Ordering {
        match key {
            "project" => lower(a.project_name.as_deref()).cmp(&lower(b.project_name.as_deref())).then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase())),
            "state" => needs(a.state).cmp(&needs(b.state)),
            "last" => b.last_at.cmp(&a.last_at),
            _ => Ordering::Equal,
        }
    };
    rows.sort_by(|a, b| {
        let o = by(a, b);
        (if rev { o.reverse() } else { o }).then_with(|| base(a, b))
    });
}

/// How much a project asks of you, for "open": waiting first, then working, then idle.
fn open_rank(p: &ProjectSummary) -> (u32, u32, u32) {
    (p.live.waiting, p.live.working, p.live.idle)
}

/// Projects by `key` ("project", "open", "git", "last"). Untracked ones stay at the bottom.
pub fn projects(rows: &mut [ProjectSummary], key: &str, rev: bool, base: fn(&ProjectSummary, &ProjectSummary) -> Ordering) {
    let by = |a: &ProjectSummary, b: &ProjectSummary| -> Ordering {
        match key {
            "project" => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            "open" => open_rank(b).cmp(&open_rank(a)),
            "git" => {
                let n = |p: &ProjectSummary| p.git.ahead_total + p.git.dirty_files + p.git.no_upstream;
                n(b).cmp(&n(a))
            }
            "last" => b.last_at.cmp(&a.last_at),
            _ => Ordering::Equal,
        }
    };
    rows.sort_by(|a, b| {
        let o = by(a, b);
        a.removed.cmp(&b.removed).then_with(|| if rev { o.reverse() } else { o }).then_with(|| base(a, b))
    });
}

/// Agent processes within one group by `key` ("pid", "task", "state", "memory").
pub fn agents(rows: &mut [&AgentProc], key: &str, rev: bool) {
    let title = |a: &AgentProc| lower(a.title.as_deref().or(a.project_name.as_deref()));
    let by = |a: &&AgentProc, b: &&AgentProc| -> Ordering {
        match key {
            "pid" => a.pid.cmp(&b.pid),
            "task" => title(a).cmp(&title(b)),
            // At work first, then the least idle.
            "state" => needs(a.state).cmp(&needs(b.state)).then_with(|| a.idle_seconds.unwrap_or(u64::MAX).cmp(&b.idle_seconds.unwrap_or(u64::MAX))),
            "memory" => b.rss_bytes.cmp(&a.rss_bytes),
            _ => Ordering::Equal,
        }
    };
    // `rows` arrive in Wake's order, and the sort is stable, so ties keep it.
    rows.sort_by(|a, b| {
        let o = by(a, b);
        if rev { o.reverse() } else { o }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(id: &str, project: Option<&str>, state: State, last: &str) -> SessionSummary {
        SessionSummary {
            id: id.into(),
            agent: Agent::Claude,
            project: project.map(String::from),
            project_name: project.map(String::from),
            title: id.into(),
            state,
            evidence: String::new(),
            started_at: Some("2026-11-20T00:00:00Z".into()),
            last_at: Some(last.into()),
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

    fn ids(v: &[SessionSummary]) -> Vec<&str> {
        v.iter().map(|s| s.id.as_str()).collect()
    }

    #[test]
    fn a_click_sorts_then_reverses_then_goes_back_to_wake_s_order() {
        let none: Sort = None;
        let a = next(&none, "last");
        assert_eq!(a, Some(("last".into(), false)));
        let b = next(&a, "last");
        assert_eq!(b, Some(("last".into(), true)));
        assert_eq!(next(&b, "last"), None);
        assert_eq!(next(&b, "project"), Some(("project".into(), false)), "another column starts afresh");
        assert_eq!(parse(&format("last", true)), Some(("last".into(), true)));
        assert_eq!(parse(&format("pid", false)), Some(("pid".into(), false)));
        assert_eq!(parse(""), None);
    }

    #[test]
    fn sessions_sort_by_each_column_both_ways() {
        let base = crate::views::by_session;
        let mut v =
            vec![s("a", Some("Zeta"), State::Idle, "2026-11-20T09:00:00Z"), s("b", Some("alpha"), State::Waiting, "2026-11-19T09:00:00Z"), s("c", None, State::Working, "2026-11-20T10:00:00Z")];
        sessions(&mut v, "project", false, base);
        assert_eq!(ids(&v), ["b", "a", "c"], "A to Z, case aside; no project last");
        sessions(&mut v, "project", true, base);
        assert_eq!(ids(&v), ["c", "a", "b"]);
        sessions(&mut v, "last", false, base);
        assert_eq!(ids(&v), ["c", "a", "b"], "newest first");
        sessions(&mut v, "last", true, base);
        assert_eq!(ids(&v), ["b", "a", "c"]);
        sessions(&mut v, "state", false, base);
        assert_eq!(ids(&v), ["b", "c", "a"], "waiting, working, idle");
    }

    fn p(id: &str, waiting: u32, working: u32, ahead: u32, removed: bool, last: &str) -> ProjectSummary {
        ProjectSummary {
            id: id.into(),
            name: id.into(),
            roots: vec![],
            removed,
            desc: String::new(),
            next: String::new(),
            why: String::new(),
            live: LiveCounts { working, waiting, idle: 0 },
            last_at: Some(last.into()),
            sessions_14d: 0,
            ideas: 0,
            git: GitTotals { ahead_total: ahead, ..Default::default() },
            paused: None,
            last_summary: None,
            git_checked: true,
            color: None,
            remote_problem: None,
        }
    }

    #[test]
    fn projects_sort_and_untracked_ones_stay_last() {
        let base = crate::views::by_project;
        let mut v = vec![p("gone", 0, 0, 9, true, "2026-11-20"), p("beta", 0, 1, 0, false, "2026-11-18"), p("alpha", 1, 0, 2, false, "2026-11-10"), p("delta", 0, 0, 5, false, "2026-11-19")];
        let names = |v: &[ProjectSummary]| v.iter().map(|p| p.id.clone()).collect::<Vec<_>>();
        projects(&mut v, "project", false, base);
        assert_eq!(names(&v), ["alpha", "beta", "delta", "gone"]);
        projects(&mut v, "project", true, base);
        assert_eq!(names(&v), ["delta", "beta", "alpha", "gone"]);
        projects(&mut v, "git", false, base);
        assert_eq!(names(&v), ["delta", "alpha", "beta", "gone"]);
        projects(&mut v, "open", false, base);
        assert_eq!(names(&v), ["alpha", "beta", "delta", "gone"], "waiting first, then working");
        projects(&mut v, "last", false, base);
        assert_eq!(names(&v), ["delta", "beta", "alpha", "gone"]);
    }

    #[test]
    fn agents_sort_within_their_group() {
        let a = |pid: u32, title: &str, rss: u64, state: State, idle: Option<u64>| AgentProc {
            pid,
            ppid: 1,
            kind: ProcKind::Session,
            agent: None,
            command: String::new(),
            session_id: None,
            job_id: None,
            title: Some(title.into()),
            project_name: None,
            state,
            idle_seconds: idle,
            rss_bytes: rss,
            cpu_percent: None,
            started_at: None,
            location: None,
            note: None,
            stale_reason: None,
            children: vec![],
            start_ticks: None,
            helpers: None,
        };
        let (x, y, z) = (a(30, "beta", 500, State::Idle, Some(600)), a(10, "alpha", 900, State::Idle, Some(60)), a(20, "Gamma", 100, State::Working, None));
        let mut v = vec![&x, &y, &z];
        let pids = |v: &[&AgentProc]| v.iter().map(|a| a.pid).collect::<Vec<_>>();
        agents(&mut v, "memory", false);
        assert_eq!(pids(&v), [10, 30, 20]);
        agents(&mut v, "pid", true);
        assert_eq!(pids(&v), [30, 20, 10]);
        agents(&mut v, "task", false);
        assert_eq!(pids(&v), [10, 30, 20]);
        agents(&mut v, "state", false);
        assert_eq!(pids(&v), [20, 10, 30], "working, then the least idle");
    }
}
