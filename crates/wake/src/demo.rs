//! A made-up machine for `wake --demo` and the offscreen screenshots: five projects, sessions in
//! every state, background sessions with job ids, subagents with steps, and agent processes
//! including stale ones. Nothing here touches the real machine; actions answer with the steps they
//! would take and change only this pretend state.

use std::sync::Mutex;

use gtk4::glib;
use wake_core::*;

/// ISO time `secs` seconds ago.
fn ago(secs: i64) -> String {
    glib::DateTime::now_utc().and_then(|t| t.add_seconds(-secs as f64)).and_then(|t| t.format("%Y-%m-%dT%H:%M:%SZ")).map(|s| s.to_string()).unwrap_or_default()
}

const MIN: i64 = 60;
const HOUR: i64 = 3600;
const DAY: i64 = 86_400;
const HOME: &str = "/home/sam/Projects";

struct Pretend {
    projects: Vec<ProjectSummary>,
    sessions: Vec<SessionSummary>,
    ideas: Vec<Idea>,
    procs: Vec<AgentProc>,
    pauses: Vec<Checkpoint>,
    next_idea: u32,
    /// Behind the remote: projects someone pushed to (made up; nothing is asked or pulled).
    behind: Vec<Behind>,
}

pub struct DemoSource {
    state: Mutex<Pretend>,
    /// Made-up question threads (questions.rs); nothing is ever sent to Claude.
    questions: crate::questions::Demo,
}

impl Default for DemoSource {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(clippy::too_many_arguments)]
fn project(id: &str, desc: &str, next: &str, why: &str, last: i64, ahead: u32, dirty: u32, noup: u32) -> ProjectSummary {
    ProjectSummary {
        id: id.into(),
        name: id.into(),
        roots: vec![format!("{HOME}/{id}")],
        removed: false,
        desc: desc.into(),
        next: next.into(),
        why: why.into(),
        live: LiveCounts::default(),
        last_at: Some(ago(last)),
        sessions_14d: 0,
        ideas: 0,
        // Skyhook lives only on this computer; waypoint's remote is on GitLab; the rest are on GitHub.
        git: GitTotals { ahead_total: ahead, no_upstream: noup, dirty_files: dirty, no_remote: u32::from(id == "Skyhook"), github: !matches!(id, "Skyhook" | "waypoint") },
        paused: None,
        last_summary: None,
        git_checked: true,
        color: Some(wake_core::store::default_colour(id).to_string()),
        remote_problem: None,
    }
}

struct S<'a> {
    id: &'a str,
    project: &'a str,
    title: &'a str,
    state: State,
    evidence: &'a str,
    started: i64,
    last: i64,
    minutes: u32,
    prompts: u32,
    last_prompt: &'a str,
}

fn session(s: S) -> SessionSummary {
    SessionSummary {
        id: s.id.into(),
        agent: Agent::Claude,
        project: (!s.project.is_empty()).then(|| s.project.into()),
        project_name: (!s.project.is_empty()).then(|| s.project.into()),
        title: s.title.into(),
        state: s.state,
        evidence: s.evidence.into(),
        started_at: Some(ago(s.started)),
        last_at: Some(ago(s.last)),
        minutes: s.minutes,
        prompts: s.prompts,
        last_prompt: (!s.last_prompt.is_empty()).then(|| s.last_prompt.into()),
        away_summary: None,
        question: None,
        subagents: SubagentCounts::default(),
        cwd: Some(if s.project.is_empty() { "/home/sam".into() } else { format!("{HOME}/{}", s.project) }),
        location: None,
        resume_command: Some(format!("claude --resume {}", s.id)),
        job_id: None,
        pid: None,
        deleted_by_claude: false,
    }
}

fn tab(window: u32, tab_index: u32, title: &str) -> Option<Location> {
    Some(Location {
        konsole_pid: 3120,
        service: "org.kde.konsole-3120".into(),
        window,
        session: tab_index + 1,
        tab_index,
        tab_title: title.into(),
        label: Some(format!("Konsole {window} · tab {}", tab_index + 1)),
    })
}

/// Tasks for the demo's long tail of finished subagents.
const EARLIER: [&str; 6] = [
    "Survey the readout code paths",
    "Check every caller of draw_rows",
    "Measure label widths at 120 px",
    "Compare with the screensaver build",
    "List the scope tests",
    "Look for other overlapping labels",
];

fn sub(id: &str, task: &str, kind: &str, state: State, secs: u32, back: Option<&str>, live: Option<&str>) -> SubagentSummary {
    SubagentSummary {
        agent_id: id.into(),
        task: task.into(),
        kind: kind.into(),
        state,
        started_at: Some(ago(secs as i64 + 120)),
        last_at: Some(ago(if state == State::Working { 4 } else { 120 })),
        seconds: secs,
        tools: vec![("Read".into(), 14), ("Grep".into(), 9), ("Bash".into(), 3)],
        handed_back: back.map(Into::into),
        live: live.map(Into::into),
    }
}

#[allow(clippy::too_many_arguments)]
fn proc(
    pid: u32,
    ppid: u32,
    kind: ProcKind,
    session: Option<&SessionSummary>,
    state: State,
    idle: Option<u64>,
    mb: u64,
    cpu: Option<f32>,
    note: Option<WorkingNote>,
    stale: Option<&str>,
) -> AgentProc {
    AgentProc {
        pid,
        ppid,
        kind,
        agent: Some(Agent::Claude),
        command: match kind {
            ProcKind::Daemon => "claude daemon --supervise".into(),
            ProcKind::PtyHost => "claude pty-host".into(),
            ProcKind::Spare => "claude --spare".into(),
            _ => "node /usr/lib/node_modules/@anthropic-ai/claude-code/cli.js".into(),
        },
        session_id: session.map(|s| s.id.clone()),
        job_id: session.and_then(|s| s.job_id.clone()),
        title: session.map(|s| s.title.clone()),
        project_name: session.and_then(|s| s.project_name.clone()),
        state,
        idle_seconds: idle,
        rss_bytes: mb * 1_048_576,
        cpu_percent: cpu,
        started_at: Some(ago(idle.unwrap_or(0) as i64 + 40 * MIN)),
        location: session.and_then(|s| s.location.clone()),
        note,
        stale_reason: stale.map(Into::into),
        children: Vec::new(),
        start_ticks: Some(u64::from(pid) * 100),
        helpers: None,
    }
}

fn note(name: &str, idle: bool, helpers: &[&str]) -> Option<WorkingNote> {
    Some(WorkingNote { name: name.into(), idle, helpers: helpers.iter().map(|h| h.to_string()).collect() })
}

impl DemoSource {
    pub fn new() -> Self {
        let mut projects = vec![
            project("Gradebook", "Rails app: AI tutor for schools", "Pick the font for the printed term reports", "Priya wants a sample before parents' evening", 11 * HOUR, 2, 3, 1),
            project("tideline-desktop", "A Wayland compositor and its settings app", "Land the scope readout helpers row fix, then tag 0.9", "", 6 * HOUR, 0, 1, 0),
            project("Skyhook", "Puzzle game, itch.io build", "", "", 2 * DAY, 0, 0, 0),
            project("fernlight", "Short story site, static", "Proofread chapter 4 before it goes up", "", 3 * DAY, 0, 0, 0),
            project("waypoint", "Trip planner, Python", "", "", 5 * DAY, 1, 0, 0),
        ];
        projects[2].paused = Some(PauseNote { at: ago(2 * DAY), note: "Level editor half done; saving works, loading doesn't".into() });
        projects[3].last_summary = Some("Chapters 1–3 are live. Chapter 4 is drafted and needs a proofread.".into());

        let mut sessions = Vec::new();
        let mut s = session(S {
            id: "7c1e9a40-gradebook-reports",
            project: "Gradebook",
            title: "term-report-pdf-export",
            state: State::Waiting,
            evidence: "live, updated 40 s ago",
            started: 3 * HOUR,
            last: 40,
            minutes: 76,
            prompts: 8,
            last_prompt: "okay, send a brief write up of this stuff to priya for her awareness too",
        });
        s.question = Some("Should I send the write-up to Priya now, or keep it as a draft for you to check first?".into());
        s.location = tab(1, 2, "Gradebook: reports");
        s.pid = Some(41022);
        s.away_summary = Some("The PDF export is built and a sample report is drafted, both merged to develop, and Priya has the sample. Next, you pick the font for the printed reports.".into());
        s.subagents = SubagentCounts { total: 3, running: 0 };
        sessions.push(s);

        let mut s = session(S {
            id: "2b8f0d11-slip-scope",
            project: "tideline-desktop",
            title: "scope-helper-rows",
            state: State::Working,
            evidence: "live, updated 3 s ago",
            started: 50 * MIN,
            last: 3,
            minutes: 50,
            prompts: 5,
            last_prompt: "the helper rows overlap the PID when there are more than four, fix that",
        });
        s.location = tab(1, 0, "tideline: scope");
        s.pid = Some(43310);
        s.subagents = SubagentCounts { total: 30, running: 2 };
        sessions.push(s);

        let mut s = session(S {
            id: "91aa4e7c-skyhook-bg",
            project: "Skyhook",
            title: "level-editor-load-bug",
            state: State::Working,
            evidence: "background job, updated 12 s ago",
            started: 25 * MIN,
            last: 12,
            minutes: 25,
            prompts: 2,
            last_prompt: "find out why loading a saved level drops the moving platforms",
        });
        s.job_id = Some("bg-5f2c".into());
        s.pid = Some(44871);
        s.subagents = SubagentCounts { total: 1, running: 1 };
        sessions.push(s);

        let mut s = session(S {
            id: "c4d2771e-waypoint-idle",
            project: "waypoint",
            title: "rail-times-import",
            state: State::Idle,
            evidence: "finished its turn, idle for 31 h",
            started: 33 * HOUR,
            last: 31 * HOUR,
            minutes: 118,
            prompts: 11,
            last_prompt: "ok leave it there for now",
        });
        s.job_id = Some("bg-19ad".into());
        s.pid = Some(30655);
        s.away_summary = Some("The rail timetable import works for GTFS feeds; the Italian feed still fails on a date format.".into());
        sessions.push(s);

        let mut s = session(S {
            id: "e0b3f5aa-fern-proof",
            project: "fernlight",
            title: "chapter-4-proofread",
            state: State::Idle,
            evidence: "stopped mid-task, idle for 9 h",
            started: 10 * HOUR,
            last: 9 * HOUR,
            minutes: 34,
            prompts: 3,
            last_prompt: "go through chapter 4 for typos only, don't touch the voice",
        });
        s.pid = Some(38410);
        sessions.push(s);

        let mut s = session(S {
            id: "5a7d0c3b-home-notes",
            project: "",
            title: "terminal-style-project-dashboard",
            state: State::Waiting,
            evidence: "its recap asks you to decide 2 h ago",
            started: 4 * HOUR,
            last: 2 * HOUR,
            minutes: 41,
            prompts: 6,
            last_prompt: "",
        });
        s.away_summary = Some("Sketched the board in three layouts. Next, say whether you want me to build the dense one or the two-pane one. (turn off recaps in /config)".into());
        s.question = Some("Next, say whether you want me to build the dense one or the two-pane one.".into());
        s.location = tab(2, 0, "~: dashboard");
        s.pid = Some(39977);
        sessions.push(s);

        let mut s = session(S {
            id: "0f9e8d7c-slip-unknown",
            project: "tideline-desktop",
            title: "unfinished-work-check",
            state: State::Unknown,
            evidence: "says busy, but nothing has changed for 6 h",
            started: 8 * HOUR,
            last: 6 * HOUR,
            minutes: 22,
            prompts: 4,
            last_prompt: "list what's half finished across the branches",
        });
        s.subagents = SubagentCounts { total: 2, running: 0 };
        sessions.push(s);

        let s = session(S {
            id: "3c3c1b2a-gradebook-ended",
            project: "Gradebook",
            title: "report-layout-review",
            state: State::Ended,
            evidence: "ended 1 d ago, stopped mid-task",
            started: 30 * HOUR,
            last: 26 * HOUR,
            minutes: 95,
            prompts: 14,
            last_prompt: "can you just have an independent agent review the layout first",
        });
        sessions.push(s);

        let mut s = session(S {
            id: "8e8e7f6d-phase-deleted",
            project: "Skyhook",
            title: "itch-page-assets",
            state: State::Ended,
            evidence: "ended 4 d ago",
            started: 4 * DAY + 2 * HOUR,
            last: 4 * DAY,
            minutes: 63,
            prompts: 9,
            last_prompt: "regenerate the itch banner at 630x500",
        });
        s.deleted_by_claude = true;
        s.resume_command = None;
        s.away_summary = Some("The itch page has a new banner, three screenshots and the cover; the generator writes them to assets/itch.".into());
        sessions.push(s);

        let s = session(S {
            id: "6d5c4b3a-waypoint-ended",
            project: "waypoint",
            title: "map-tiles-offline",
            state: State::Ended,
            evidence: "ended 5 d ago",
            started: 5 * DAY + HOUR,
            last: 5 * DAY,
            minutes: 48,
            prompts: 7,
            last_prompt: "cache the map tiles for offline use",
        });
        sessions.push(s);

        for p in &mut projects {
            for s in sessions.iter().filter(|s| s.project.as_deref() == Some(p.id.as_str())) {
                p.sessions_14d += 1;
                match s.state {
                    State::Working => p.live.working += 1,
                    State::Waiting => p.live.waiting += 1,
                    State::Idle => p.live.idle += 1,
                    _ => {}
                }
            }
        }

        let ideas = vec![
            idea("i1", 20 * MIN, "Wake could show which tab each agent's helper is reading", Some("tideline-desktop"), "typed", "inbox"),
            idea("i2", 3 * HOUR, "Ask Priya whether teachers want the reports as CSV too", Some("Gradebook"), "prompt", "inbox"),
            idea("i3", 26 * HOUR, "Moving platforms need their path saved with the level", Some("Skyhook"), "shell", "inbox"),
            idea("i4", 2 * DAY, "A night mode for the reading pages", None, "typed", "inbox"),
            idea("i6", 4 * HOUR, "Teacher invites: say when the link expires, in the email itself", Some("Gradebook"), "typed", "inbox"),
            idea("i7", 3 * DAY, "Konsole tab titles could carry the project's name", Some("tideline-desktop"), "typed", "sent"),
            idea("i5", 6 * DAY, "Try the Italian feed with a fixed date parser", Some("waypoint"), "typed", "done"),
        ];
        for p in &mut projects {
            p.ideas = ideas.iter().filter(|i| i.project.as_deref() == Some(p.id.as_str()) && i.state == "inbox").count() as u32;
        }

        let find = |id: &str| sessions.iter().find(|s| s.id.starts_with(id)).unwrap();
        let mut procs = vec![
            proc(2101, 1, ProcKind::Daemon, None, State::Unknown, None, 96, Some(0.1), None, None),
            proc(
                43310,
                3120,
                ProcKind::Session,
                Some(find("2b8f")),
                State::Working,
                Some(3),
                512,
                Some(38.0),
                note("tideline: scope helper rows", false, &["reading effects/scope.rs", "running cargo test -p tideline scope"]),
                None,
            ),
            proc(41022, 3120, ProcKind::Session, Some(find("7c1e")), State::Waiting, Some(40), 438, Some(0.4), note("Gradebook: report write-up", true, &[]), None),
            proc(44871, 2101, ProcKind::Session, Some(find("91aa")), State::Working, Some(12), 655, Some(21.5), note("Skyhook: level loader", false, &["tracing the platform save path"]), None),
            proc(44872, 44871, ProcKind::PtyHost, Some(find("91aa")), State::Working, None, 38, Some(0.2), None, None),
            proc(
                30655,
                2101,
                ProcKind::Session,
                Some(find("c4d2")),
                State::Idle,
                Some(31 * HOUR as u64),
                702,
                Some(0.0),
                note("waypoint: rail times", true, &[]),
                Some("idle for 31 h, background, nobody attached"),
            ),
            proc(30656, 30655, ProcKind::PtyHost, Some(find("c4d2")), State::Idle, None, 41, Some(0.0), None, None),
            proc(38410, 3120, ProcKind::Session, Some(find("e0b3")), State::Idle, Some(9 * HOUR as u64), 466, Some(0.0), note("fernlight: chapter 4", true, &[]), None),
            proc(39977, 3120, ProcKind::Session, Some(find("5a7d")), State::Idle, Some(2 * HOUR as u64), 390, Some(0.1), note("", true, &[]), None),
            proc(29001, 2101, ProcKind::Spare, None, State::Idle, Some(3 * HOUR as u64), 214, Some(0.0), None, Some("a spare session nobody has used for 3 h")),
            proc(
                27740,
                1,
                ProcKind::Stray,
                None,
                State::Unknown,
                Some(2 * DAY as u64),
                588,
                Some(0.0),
                note("tideline: unfinished work", true, &[]),
                Some("not tied to any session; its terminal has gone"),
            ),
        ];
        procs[4].command = "claude attach bg-5f2c --pty".into();
        let (a, b) = (procs[3].rss_bytes, procs[4].rss_bytes);
        procs[3].children = vec![44872];
        procs[3].rss_bytes = a + b;
        let (a, b) = (procs[5].rss_bytes, procs[6].rss_bytes);
        procs[5].children = vec![30656];
        procs[5].rss_bytes = a + b;
        procs[10].agent = Some(Agent::Claude);

        Self { state: Mutex::new(Pretend { behind: demo_behind(&projects), projects, sessions, ideas, procs, pauses: Vec::new(), next_idea: 8 }), questions: crate::questions::Demo::new() }
    }
}

fn idea(id: &str, secs: i64, text: &str, project: Option<&str>, source: &str, state: &str) -> Idea {
    Idea {
        id: id.into(),
        at: ago(secs),
        text: text.into(),
        project: project.map(Into::into),
        project_name: project.map(Into::into),
        source: source.into(),
        state: state.into(),
        sent_at: (state == "sent").then(|| ago(secs.saturating_sub(600))),
        brief: (state == "sent").then(|| "b3f9a1c07d2e4".into()),
    }
}

fn steps(list: &[(&str, &str)]) -> Vec<Step> {
    list.iter().enumerate().map(|(i, (verb, what))| Step { at: Some(ago(600 - 40 * i as i64)), verb: verb.to_string(), what: what.to_string() }).collect()
}

impl Pretend {
    fn subagents(&self, session: &str) -> Vec<SubagentSummary> {
        match session.get(..4).unwrap_or("") {
            "7c1e" => vec![
                sub(
                    "a1",
                    "Check the report layout against the school's template",
                    "general-purpose",
                    State::Ended,
                    312,
                    Some("Two gaps: retention period for chat logs, and the lawful basis for teacher analytics."),
                    None,
                ),
                sub("a2", "Find every place grades are rounded", "Explore", State::Ended, 95, Some("Grades are rounded in 4 places; all go through Grade.round_for_report."), None),
                sub("a3", "Draft the email to Priya", "general-purpose", State::Ended, 140, Some("Draft saved in docs/emails/priya-sample.md."), None),
            ],
            "2b8f" => vec![
                sub("b1", "Read scope.rs and list the row layout constants", "Explore", State::Ended, 64, Some("Rows are HELPER = 14 px under a 44 px trace; MOST_ROWS = 4 with a +N more row."), None),
                sub(
                    "b2",
                    "Reproduce the overlap with five helpers",
                    "general-purpose",
                    State::Ended,
                    210,
                    Some("Reproduced: the fifth helper's name runs under the PID label at widths below 160 px."),
                    None,
                ),
                sub("b3", "Run the scope tests", "general-purpose", State::Working, 48, None, Some("Bash: cargo test -p tideline scope")),
                sub("b4", "Check the Settings preview uses the same constants", "Explore", State::Working, 22, None, Some("Grep: HELPER_WAVE in tideline-settings")),
            ]
            .into_iter()
            // A long session: 26 more that finished earlier, oldest first, each a few minutes apart.
            .chain((0..26).map(|i| {
                let mut a = sub(
                    &format!("b{}", i + 5),
                    EARLIER[i % EARLIER.len()],
                    if i % 3 == 0 { "Explore" } else { "general-purpose" },
                    State::Ended,
                    60 + 17 * i as u32,
                    Some("Done; notes in the session."),
                    None,
                );
                a.started_at = Some(ago((40 - i as i64) * 7 * MIN));
                a.last_at = Some(ago((40 - i as i64) * 7 * MIN - 3 * MIN));
                a
            }))
            .collect(),
            "91aa" => vec![sub("c1", "Trace how moving platforms are saved", "Explore", State::Working, 130, None, Some("Read: src/level/serialize.rs"))],
            "0f9e" => vec![
                sub("d1", "List branches with commits not on main", "Explore", State::Ended, 40, Some("7 branches; 3 have work not merged anywhere."), None),
                sub("d2", "Summarise each unmerged branch", "general-purpose", State::Unknown, 180, None, None),
            ],
            _ => Vec::new(),
        }
    }

    fn overview(&self) -> Overview {
        let mut counts = LiveCounts::default();
        for s in &self.sessions {
            match s.state {
                State::Working => counts.working += 1,
                State::Waiting => counts.waiting += 1,
                State::Idle => counts.idle += 1,
                _ => {}
            }
        }
        let mut projects = self.projects.clone();
        for p in &mut projects {
            p.live = LiveCounts::default();
            for s in self.sessions.iter().filter(|s| s.project.as_deref() == Some(p.id.as_str())) {
                match s.state {
                    State::Working => p.live.working += 1,
                    State::Waiting => p.live.waiting += 1,
                    State::Idle => p.live.idle += 1,
                    _ => {}
                }
            }
        }
        Overview {
            generated_at: ago(0),
            indexing: Progress { done: 53, total: 53 },
            counts,
            ideas: self.ideas.iter().filter(|i| i.state == "inbox").count() as u32,
            projects,
            sessions: self.sessions.clone(),
            index_error: None,
            locations_unsure: None,
            behind: Some(self.behind.clone()),
            // Disk, resources, attribution (added 2026-11-20): see crate::disk's demo data.
            resources: Some(crate::disk::demo_resources(&self.procs, &self.sessions)),
            disk: Some(crate::disk::demo_report()),
            attribution: Some(crate::disk::demo_attribution(&self.sessions)),
        }
    }
}

impl Source for DemoSource {
    fn overview(&self) -> Overview {
        self.state.lock().unwrap().overview()
    }

    fn session(&self, id: &str) -> Option<SessionDetail> {
        let st = self.state.lock().unwrap();
        let summary = st.sessions.iter().find(|s| s.id == id)?.clone();
        let n = summary.prompts.max(1);
        let asks = [
            "Can you send what's left to Priya from my email so she can action it later today",
            "the page breaks.. how much of that can we sort out here, now?",
            "can you just have an independent agent review the layout first",
            "fine, go with the school's template",
            "make the retention period 12 months and say why",
            "how do we round a grade that's exactly half way?",
            "add that to the pack",
            "okay, send a brief write up of this stuff to priya for her awareness too",
        ];
        let prompts = (0..n)
            .map(|i| Prompt { at: Some(ago(((n - i) * 9 * 60) as i64)), text: if i + 1 == n { summary.last_prompt.clone().unwrap_or_default() } else { asks[i as usize % asks.len()].into() } })
            .collect();
        let cwd = summary.cwd.clone().unwrap_or_default();
        let files = [("app/models/report_card.rb", 6), ("app/views/reports/term.pdf.erb", 4), ("app/controllers/reports_controller.rb", 2), ("config/locales/en.yml", 1)]
            .iter()
            .take(if summary.prompts > 3 { 4 } else { 1 })
            .map(|(p, e)| FileTouch { path: format!("{cwd}/{p}"), edits: *e })
            .collect();
        Some(SessionDetail {
            subagents: st.subagents(id),
            transcript: (!summary.deleted_by_claude && !cwd.is_empty()).then(|| format!("~/.claude/projects/{}/{id}.jsonl", cwd.replace('/', "-"))),
            prompts,
            files,
            commands: 23,
            tests: 4,
            pr_links: if id.starts_with("7c1e") { vec!["https://github.com/example/gradebook/pull/212".into()] } else { Vec::new() },
            cost_usd: Some(3.42),
            summary,
        })
    }

    fn subagent(&self, session: &str, agent_id: &str) -> Option<SubagentDetail> {
        let st = self.state.lock().unwrap();
        let summary = st.subagents(session).into_iter().find(|a| a.agent_id == agent_id)?;
        let list = match agent_id {
            "b1" => steps(&[("read", "tideline/src/effects/scope.rs"), ("searched", "MOST_ROWS in tideline/src"), ("read", "tideline/src/working.rs")]),
            "b2" => steps(&[
                ("read", "tideline/src/effects/scope.rs"),
                ("edited", "tideline/src/effects/scope.rs (a test with five helpers)"),
                ("ran", "cargo test -p tideline scope -- five_helpers"),
                ("read", "the failing test's output"),
            ]),
            "b3" => steps(&[("ran", "cargo build -p tideline"), ("ran", "cargo test -p tideline scope")]),
            "a1" => {
                steps(&[("fetched", "the school's report template"), ("read", "app/views/reports/term.pdf.erb"), ("read", "docs/report-notes.md"), ("other", "compared the two section by section")])
            }
            _ => steps(&[("searched", "the codebase for the task's names"), ("read", "the three files that matched")]),
        };
        Some(SubagentDetail { handed_back_full: summary.handed_back.clone(), summary, steps: list })
    }

    fn project(&self, id: &str) -> Option<ProjectDetail> {
        let st = self.state.lock().unwrap();
        let summary = st.projects.iter().find(|p| p.id == id)?.clone();
        let root = summary.roots[0].clone();
        let repos = vec![RepoState {
            path: root.clone(),
            branch: Some(if id == "Gradebook" { "develop".into() } else { "main".into() }),
            head: Some("5c12763".into()),
            upstream: (summary.git.no_upstream == 0 && summary.git.no_remote == 0).then(|| "origin/main".into()),
            ahead: Some(summary.git.ahead_total),
            behind: Some(0),
            dirty_files: summary.git.dirty_files,
            untracked: if id == "Gradebook" { 2 } else { 0 },
            branches_no_upstream: if summary.git.no_remote > 0 { 4 } else { summary.git.no_upstream },
            branches: 4,
            worktrees: if id == "tideline-desktop" { 2 } else { 1 },
            last_commit_at: summary.last_at.clone(),
            last_commit_subject: Some("Tests: one scratch folder helper, removed when each test ends".into()),
            remote_host: match id {
                "Skyhook" => None,
                "waypoint" => Some("GitLab".into()),
                _ => Some("GitHub".into()),
            },
        }];
        // What the pause kept: where git stood and which sessions were open.
        let git = repos.iter().map(|r| (r.path.clone(), r.branch.clone(), Some("7d05b9e1c4a2".to_string()), r.ahead)).collect();
        let sessions = st.sessions.iter().filter(|s| s.project.as_deref() == Some(id)).take(2).map(|s| s.id.clone()).collect();
        let last_pause = st
            .pauses
            .iter()
            .rev()
            .find(|c| c.project == id)
            .cloned()
            .or_else(|| summary.paused.as_ref().map(|p| Checkpoint { project: id.into(), at: p.at.clone(), note: p.note.clone(), next: summary.next.clone(), git, sessions }));
        Some(ProjectDetail {
            since_pause: if last_pause.is_some() { vec!["2 commits on main".into(), "1 session: itch-page-assets".into()] } else { Vec::new() },
            could_not_check: Vec::new(),
            last_pause,
            repos,
            sessions: st.sessions.iter().filter(|s| s.project.as_deref() == Some(id)).cloned().collect(),
            ideas: st.ideas.iter().filter(|i| i.project.as_deref() == Some(id)).cloned().collect(),
            summary,
        })
    }

    fn search(&self, query: &str) -> Vec<(SessionSummary, String)> {
        let st = self.state.lock().unwrap();
        let q = query.to_lowercase();
        st.sessions
            .iter()
            .filter_map(|s| {
                let hay = [Some(&s.title), s.last_prompt.as_ref(), s.away_summary.as_ref(), s.project_name.as_ref()];
                hay.into_iter().flatten().find(|t| t.to_lowercase().contains(&q)).map(|t| (s.clone(), t.clone()))
            })
            .collect()
    }

    fn ideas(&self) -> Vec<Idea> {
        self.state.lock().unwrap().ideas.clone()
    }

    fn add_idea(&self, text: &str, project: Option<&str>) -> Result<Idea, String> {
        let mut st = self.state.lock().unwrap();
        let id = format!("i{}", st.next_idea);
        st.next_idea += 1;
        let i = idea(&id, 0, text, project, "typed", "inbox");
        st.ideas.insert(0, i.clone());
        Ok(i)
    }

    fn set_idea(&self, id: &str, state: Option<&str>, project: Option<Option<&str>>) -> Result<Idea, String> {
        let mut st = self.state.lock().unwrap();
        let i = st.ideas.iter_mut().find(|i| i.id == id).ok_or("No such idea.")?;
        if let Some(s) = state {
            i.state = s.into();
            i.sent_at = None;
        }
        if let Some(p) = project {
            i.project = p.map(Into::into);
            i.project_name = p.map(Into::into);
        }
        Ok(i.clone())
    }

    fn pause(&self, project: &str, note: &str, next: &str) -> Result<Checkpoint, String> {
        let mut st = self.state.lock().unwrap();
        let at = ago(0);
        let p = st.projects.iter_mut().find(|p| p.id == project).ok_or("No such project.")?;
        p.paused = Some(PauseNote { at: at.clone(), note: note.into() });
        p.next = next.into();
        let git = vec![(p.roots[0].clone(), Some("main".to_string()), Some("7d05b9e1c4a2".to_string()), Some(p.git.ahead_total))];
        let sessions = st.sessions.iter().filter(|s| s.project.as_deref() == Some(project) && crate::app::is_live(s.state)).map(|s| s.id.clone()).collect();
        let c = Checkpoint { project: project.into(), at, note: note.into(), next: next.into(), git, sessions };
        st.pauses.push(c.clone());
        Ok(c)
    }

    fn add_project(&self, path: &str) -> Result<ProjectSummary, String> {
        let mut st = self.state.lock().unwrap();
        let name = crate::fmt::base(path);
        let p = project(&name, "", "", "", 0, 0, 0, 0);
        st.projects.push(p.clone());
        Ok(p)
    }

    fn remove_project(&self, id: &str, removed: bool) -> Result<ProjectSummary, String> {
        let mut st = self.state.lock().unwrap();
        let p = st.projects.iter_mut().find(|p| p.id == id).ok_or("No such project.")?;
        p.removed = removed;
        Ok(p.clone())
    }

    fn agent_choices(&self) -> Vec<wake_core::brief::Agent> {
        wake_core::brief::Agent::ALL.to_vec()
    }

    fn brief_command(&self, agent: wake_core::brief::Agent, id: &str) -> Option<String> {
        wake_core::brief::command(agent, &wake_core::brief::path(std::path::Path::new("/home/you/.local/share/wake"), id))
    }

    /// Nothing is written or typed: the ideas are only marked sent in the made-up data.
    fn start_agent(&self, req: &wake_core::brief::StartAgent, _dry_run: bool) -> ActionResult {
        let mut st = self.state.lock().unwrap();
        if req.ideas.iter().any(|id| !st.ideas.iter().any(|i| &i.id == id && i.project.as_deref() == Some(req.project.as_str()))) {
            return ActionResult { ok: false, message: format!("Those ideas aren't all for {}. Start one agent per project.", req.project), steps: vec![] };
        }
        let at = ago(0);
        for i in st.ideas.iter_mut().filter(|i| req.ideas.contains(&i.id)) {
            i.state = "sent".into();
            i.sent_at = Some(at.clone());
            i.brief = Some(req.id.clone());
        }
        let agent = wake_core::brief::Agent::from_id(&req.agent).map_or("Claude", |a| a.label());
        ActionResult { ok: true, message: format!("Demo: {agent} would start in a new Konsole window in ~/Projects/{}.", req.project), steps: vec![] }
    }

    fn go_to(&self, session: &str, _dry_run: bool) -> ActionResult {
        let st = self.state.lock().unwrap();
        let Some(s) = st.sessions.iter().find(|s| s.id == session) else {
            return ActionResult { ok: false, message: "Wake can't find that session.".into(), steps: Vec::new() };
        };
        let (message, steps) = if let Some(l) = &s.location {
            (format!("Demo: would switch to Konsole {} · tab {}.", l.window, l.tab_index + 1), vec![format!("qdbus-qt6 {} /Windows/{} setCurrentSession {}", l.service, l.window, l.session)])
        } else if let Some(job) = &s.job_id {
            (format!("Demo: would open a new Konsole window running claude attach {job}."), vec![format!("konsole -e claude attach {job}")])
        } else if let Some(cmd) = &s.resume_command {
            (format!("Demo: would open a new Konsole window running {cmd}."), vec![format!("konsole -e {cmd}")])
        } else {
            return ActionResult { ok: false, message: "Claude deleted this conversation, so it can't be reopened.".into(), steps: Vec::new() };
        };
        ActionResult { ok: true, message, steps }
    }

    fn processes(&self) -> ProcOverview {
        let st = self.state.lock().unwrap();
        let mut procs = st.procs.clone();
        // A little life in the CPU column, as two samples would give.
        let wobble = (glib::monotonic_time() / 1_000_000 % 7) as f32;
        for p in &mut procs {
            if p.state == State::Working {
                p.cpu_percent = p.cpu_percent.map(|c| c + wobble);
            }
        }
        let top: Vec<&AgentProc> = procs.iter().filter(|p| !procs.iter().any(|q| q.children.contains(&p.pid))).collect();
        let total_rss_bytes = top.iter().map(|p| p.rss_bytes).sum();
        let stale: Vec<&&AgentProc> = top.iter().filter(|p| p.stale_reason.is_some()).collect();
        ProcOverview { generated_at: ago(0), total_rss_bytes, stale_rss_bytes: stale.iter().map(|p| p.rss_bytes).sum(), stale_count: stale.len() as u32, dead_notes: vec![48213], procs }
    }

    /// Made-up terminals: a working agent in a tab, a background one, and a quiet prompt for the
    /// other live sessions. The working ones grow a line every second so the preview looks live.
    fn screen(&self, session: &str) -> Option<Screen> {
        let s = self.state.lock().unwrap().sessions.iter().find(|s| s.id == session).cloned()?;
        if !matches!(s.state, State::Working | State::Waiting | State::Idle) {
            return None;
        }
        let tick = (glib::real_time() / 1_000_000) as usize;
        let (source, mut lines): (String, Vec<String>) = match session {
            "2b8f0d11-slip-scope" => ("Konsole 1 · tab 1".into(), DEMO_SCOPE.lines().map(String::from).collect()),
            "91aa4e7c-skyhook-bg" => ("claude logs".into(), DEMO_SKYHOOK.lines().map(String::from).collect()),
            _ => (
                s.location.as_ref().and_then(|l| l.label.clone()).unwrap_or_else(|| "claude logs".into()),
                vec![
                    format!("> {}", s.last_prompt.clone().unwrap_or_default()),
                    String::new(),
                    "● Done. Waiting for your next message.".into(),
                    String::new(),
                    "╭──────────────────────────────────────────────╮".into(),
                    "│ >                                            │".into(),
                    "╰──────────────────────────────────────────────╯".into(),
                    "  ? for shortcuts".into(),
                ],
            ),
        };
        if s.state == State::Working {
            let n = lines.len();
            let shown = n.saturating_sub(6) + tick % 7;
            lines.truncate(shown.min(n));
            lines.push(format!("  {} Working… ({} s · esc to interrupt)", ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴"][tick % 6], 40 + tick % 60));
        }
        Some(Screen { lines, source, at: glib::DateTime::now_utc().ok().and_then(|d| d.format_iso8601().ok()).map(|s| s.to_string()).unwrap_or_default(), note: None })
    }

    fn stop(&self, pid: u32, _start_ticks: Option<u64>, how: StopHow, _dry_run: bool) -> ActionResult {
        let mut st = self.state.lock().unwrap();
        let Some(p) = st.procs.iter().find(|p| p.pid == pid).cloned() else {
            return ActionResult { ok: false, message: format!("PID {pid} has already gone."), steps: Vec::new() };
        };
        if p.kind == ProcKind::Daemon {
            return ActionResult { ok: false, message: "Wake won't stop Claude's daemon: every background session depends on it.".into(), steps: Vec::new() };
        }
        let step = match how {
            StopHow::AgentStop => format!("claude stop {}", p.job_id.clone().unwrap_or_default()),
            StopHow::Terminate => format!("kill -TERM {pid}"),
            StopHow::Kill => format!("kill -KILL {pid}"),
        };
        // The stray pretends to ignore a polite signal, so the Force step can be seen.
        if pid == 27740 && how == StopHow::Terminate {
            return ActionResult { ok: true, message: format!("Demo: asked PID {pid} to stop."), steps: vec![step] };
        }
        let gone: Vec<u32> = std::iter::once(pid).chain(p.children.iter().copied()).collect();
        st.procs.retain(|q| !gone.contains(&q.pid));
        if let Some(id) = &p.session_id
            && let Some(s) = st.sessions.iter_mut().find(|s| &s.id == id)
        {
            s.state = State::Ended;
            s.evidence = "ended just now".into();
            s.pid = None;
        }
        ActionResult { ok: true, message: format!("Demo: stopped PID {pid}. Nothing real was signalled."), steps: vec![step] }
    }

    fn questions(&self, since: u64) -> Option<QOverview> {
        self.questions.questions(since)
    }

    fn ask(&self, _thread: Option<&str>, _question: &str, _project: Option<&str>) -> Result<String, String> {
        Err("Demo data: Wake doesn't send questions to Claude here.".into())
    }

    fn cancel_question(&self, thread: &str) -> bool {
        self.questions.cancel(thread)
    }

    fn delete_thread(&self, thread: &str) -> Result<(), String> {
        self.questions.delete(thread)
    }

    fn mark_read(&self, thread: &str) {
        self.questions.mark_read(thread)
    }

    fn question_ais(&self) -> Vec<wake_core::questions::AiChoice> {
        use wake_core::questions::{Ai, choice};
        // Made up: all three installed, as on the machine this was written on.
        Ai::ALL.into_iter().map(|ai| choice(ai, Some(std::path::Path::new("/demo")), false)).collect()
    }

    /// Set project (disk.rs): the demo's sessions change in memory only.
    fn set_session_project(&self, session: &str, project: Option<Option<&str>>) -> Result<(), String> {
        let mut st = self.state.lock().unwrap();
        let s = st.sessions.iter_mut().find(|s| s.id == session).ok_or("No session with that id.")?;
        if let Some(p) = project {
            s.project = p.map(String::from);
            s.project_name = p.map(String::from);
        }
        Ok(())
    }

    fn set_project_color(&self, id: &str, color: Option<&str>) -> Result<ProjectSummary, String> {
        let mut st = self.state.lock().unwrap();
        let p = st.projects.iter_mut().find(|p| p.id == id).ok_or("No project with that id.")?;
        p.color = Some(color.map_or_else(|| wake_core::store::default_colour(&p.name).to_string(), String::from));
        Ok(p.clone())
    }

    fn set_next(&self, id: &str, next: &str) -> Result<ProjectSummary, String> {
        let mut st = self.state.lock().unwrap();
        let p = st.projects.iter_mut().find(|p| p.id == id).ok_or("No project with that id.")?;
        p.next = next.into();
        Ok(p.clone())
    }

    fn ack_behind(&self, project: &str) {
        for b in self.state.lock().unwrap().behind.iter_mut().filter(|b| b.project == project) {
            b.alarm = None;
        }
    }

    fn pull(&self, project: &str, dry_run: bool) -> ActionResult {
        let mut st = self.state.lock().unwrap();
        let Some(b) = st.behind.iter().find(|b| b.project == project).cloned() else {
            return ActionResult { ok: false, message: "That project isn't behind; nothing to pull.".into(), steps: vec![] };
        };
        let steps = vec![format!("git -C {} pull --ff-only", b.repo)];
        if !b.can_pull {
            return ActionResult { ok: false, message: "Commit or stash first; Wake only pulls a clean tree.".into(), steps };
        }
        if !dry_run {
            st.behind.retain(|x| x.project != project);
        }
        ActionResult { ok: true, message: "Pulled. Fast-forward (demo: nothing was run).".into(), steps }
    }
}

/// Two made-up projects behind their remote: one fetched (3 commits, a file edited), one with
/// new commits not fetched yet (clean, so "Pull now" is offered).
fn demo_behind(projects: &[ProjectSummary]) -> Vec<Behind> {
    use wake_core::remote::{Outcome, RepoCheck, behind_for};
    let mk = |name: &str, count: Option<u32>, dirty: u32| -> Option<Behind> {
        let p = projects.iter().find(|p| p.name == name)?;
        let c = RepoCheck {
            path: p.roots.first()?.clone(),
            upstream: "origin/main".into(),
            host: "GitHub".into(),
            outcome: Outcome::Behind { count, ahead: 0 },
            remote_sha: None,
            problem: None,
            asked_at: Some(ago(4 * MIN)),
            at: std::time::Instant::now(),
        };
        behind_for(&p.id, &p.name, &p.roots, &[&c], &|_| dirty)
    };
    [mk("tideline-desktop", Some(3), 1), mk("fernlight", None, 0)].into_iter().flatten().collect()
}

const DEMO_SCOPE: &str = "\
> the helper rows overlap the PID when there are more than four, fix that

● I'll look at how the helper rows are laid out in the readout.

● Read(tideline/src/effects/scope.rs)
  ⎿  Read 412 lines

● Search(pattern: \"helper_rows|HELPER_PITCH\", path: \"tideline/src\")
  ⎿  Found 3 files

● The rows are placed at a fixed pitch from the top, so with five or more
  helpers the last ones run into the PID in the top right corner. I'll cap
  the visible rows at four and fold the rest into a \"+N more\" line.

● Update(tideline/src/effects/scope.rs)
  ⎿  Updated tideline/src/effects/scope.rs with 14 additions and 3 removals
       212 -    for (i, h) in helpers.iter().enumerate() {
       212 +    let shown = helpers.len().min(MAX_HELPER_ROWS);
       213 +    for (i, h) in helpers.iter().take(shown).enumerate() {
       ...
       229 +    if helpers.len() > shown {
       230 +        draw_more(cr, helpers.len() - shown, y);
       231 +    }

● Bash(cargo test -p tideline scope -- --quiet)
  ⎿  running 9 tests
     .........
     test result: ok. 9 passed; 0 failed

● Now a screenshot of the readout with six helpers to check the spacing.";

const DEMO_SKYHOOK: &str = "\
> find out why loading a saved level drops the moving platforms

● Read(src/editor/save.rs)
  ⎿  Read 188 lines

● Read(src/editor/load.rs)
  ⎿  Read 141 lines

● Saving writes moving platforms under \"movers\", but loading only reads
  \"platforms\" and \"hazards\". The movers are never read back.

● Agent(Check older saves for the movers key)
  ⎿  Done (6 tool uses · 40 s)

● Update(src/editor/load.rs)
  ⎿  Updated src/editor/load.rs with 9 additions

● Bash(cargo test editor::load)
  ⎿  test result: ok. 12 passed; 0 failed

● Loading now restores moving platforms, and old saves without the key
  still load. Writing a test level with three movers to check by hand.";
