//! A made-up machine for `wake --demo` and the offscreen screenshots: five projects, sessions in
//! every state, background sessions with job ids, subagents with steps, and agent processes
//! including stale ones. Nothing here touches the real machine; actions answer with the steps they
//! would take and change only this pretend state.

use std::sync::Mutex;

use gtk4::glib;
use wake_core::*;

/// ISO time `secs` seconds ago.
fn ago(secs: i64) -> String {
    util::iso_from_epoch(util::now() - secs as f64)
}

const MIN: i64 = 60;
const HOUR: i64 = 3600;
const DAY: i64 = 86_400;
/// The made-up person's home, and their projects folder.
pub const HOME_DIR: &str = "/home/sam";
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
    /// Groups kept on their own, with or without projects in them.
    groups: Vec<String>,
}

pub struct DemoSource {
    state: Mutex<Pretend>,
    /// Made-up question threads (questions.rs); nothing is ever sent to Claude.
    questions: crate::questions::Demo,
    /// The New project interview, scripted (newproject.rs); nothing is ever made.
    newproject: crate::newproject::Demo,
}

impl Default for DemoSource {
    fn default() -> Self {
        Self::new()
    }
}

#[expect(clippy::too_many_arguments, reason = "made-up data, one argument per column")]
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
        // waypoint's remote couldn't be asked, so it isn't called in step.
        remote_problem: (id == "waypoint").then(|| "Wake couldn't ask GitLab (offline).".into()),
        // Two made-up groups, and one project that matters most and one that can wait.
        group: match id {
            "Gradebook" | "fernlight" => Some("Work".into()),
            "Skyhook" | "tideline-desktop" => Some("Side projects".into()),
            _ => None,
        },
        importance: match id {
            "Gradebook" => Some("high".into()),
            "waypoint" => Some("low".into()),
            _ => None,
        },
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

fn session(s: &S) -> SessionSummary {
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

/// A made-up terminal window (`n` tells them apart).
fn window(n: u32) -> Location {
    Location { pid: 3120 + n, app: Some("Terminal".into()) }
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

#[expect(clippy::too_many_arguments, reason = "made-up data, one argument per column")]
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
        tasks: Vec::new(),
        others: Vec::new(),
        others_rss_bytes: 0,
    }
}

fn note(name: &str, idle: bool, helpers: &[&str]) -> WorkingNote {
    WorkingNote { name: name.into(), idle, helpers: helpers.iter().map(|h| h.to_string()).collect(), tasks: Vec::new() }
}

impl DemoSource {
    /// The made-up state; a panic elsewhere never locks the demo out of it.
    fn st(&self) -> std::sync::MutexGuard<'_, Pretend> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn new() -> Self {
        let mut projects = vec![
            project("Gradebook", "Rails app: AI tutor for schools", "Pick the font for the printed term reports", "Priya wants a sample before parents' evening", 11 * HOUR, 2, 3, 1),
            project("tideline-desktop", "A Wayland compositor and its settings app", "Land the scope readout helpers row fix, then tag 0.9", "", 6 * HOUR, 0, 1, 0),
            project("Skyhook", "Puzzle game, itch.io build", "", "", 2 * DAY, 0, 0, 0),
            project("fernlight", "Short story site, static", "Proofread chapter 4 before it goes up", "", 3 * DAY, 0, 0, 0),
            project("waypoint", "Trip planner, Python", "", "", 5 * DAY, 0, 0, 0),
        ];
        projects[2].paused = Some(PauseNote { at: ago(2 * DAY), note: "Level editor half done; saving works, loading doesn't".into() });
        projects[3].last_summary = Some("Chapters 1–3 are live. Chapter 4 is drafted and needs a proofread.".into());

        let mut sessions = Vec::new();
        let mut s = session(&S {
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
        s.location = Some(window(1));
        s.pid = Some(41022);
        s.away_summary = Some("The PDF export is built and a sample report is drafted, both merged to develop, and Priya has the sample. Next, you pick the font for the printed reports.".into());
        s.subagents = SubagentCounts { total: 3, running: 0 };
        sessions.push(s);

        let mut s = session(&S {
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
        s.location = Some(window(1));
        s.pid = Some(43310);
        s.subagents = SubagentCounts { total: 30, running: 2 };
        sessions.push(s);

        let mut s = session(&S {
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

        let mut s = session(&S {
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

        let mut s = session(&S {
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

        let mut s = session(&S {
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
        s.location = Some(window(2));
        s.pid = Some(39977);
        sessions.push(s);

        let mut s = session(&S {
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

        let s = session(&S {
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

        let mut s = session(&S {
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

        let s = session(&S {
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
            idea("i1", 20 * MIN, "Wake could show which window each agent's helper is reading", Some("tideline-desktop"), "typed", IdeaState::Inbox),
            idea("i2", 3 * HOUR, "Ask Priya whether teachers want the reports as CSV too", Some("Gradebook"), "prompt", IdeaState::Inbox),
            idea("i3", 26 * HOUR, "Moving platforms need their path saved with the level", Some("Skyhook"), "shell", IdeaState::Inbox),
            idea("i4", 2 * DAY, "A night mode for the reading pages", None, "typed", IdeaState::Inbox),
            idea("i6", 4 * HOUR, "Teacher invites: say when the link expires, in the email itself", Some("Gradebook"), "typed", IdeaState::Inbox),
            idea("i7", 3 * DAY, "Terminal window titles could carry the project's name", Some("tideline-desktop"), "typed", IdeaState::Sent),
            idea("i5", 6 * DAY, "Try the Italian feed with a fixed date parser", Some("waypoint"), "typed", IdeaState::Done),
        ];
        for p in &mut projects {
            p.ideas = ideas.iter().filter(|i| i.project.as_deref() == Some(p.id.as_str()) && i.state == IdeaState::Inbox).count() as u32;
        }

        #[expect(clippy::unwrap_used, reason = "the ids are the made-up sessions' own, a few lines up")]
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
                Some(WorkingNote { tasks: vec!["cargo test -p tideline scope".into()], ..note("tideline: scope helper rows", false, &["reading effects/scope.rs"]) }),
                None,
            ),
            proc(
                41022,
                3120,
                ProcKind::Session,
                Some(find("7c1e")),
                State::Waiting,
                Some(40),
                438,
                Some(0.4),
                Some(WorkingNote { tasks: vec!["npm run dev".into()], ..note("Gradebook: report write-up", true, &[]) }),
                None,
            ),
            proc(
                44871,
                2101,
                ProcKind::Session,
                Some(find("91aa")),
                State::Working,
                Some(12),
                655,
                Some(21.5),
                Some(WorkingNote { tasks: vec!["cargo build --release".into(), "the level loader's test log".into()], ..note("Skyhook: level loader", false, &["tracing the platform save path"]) }),
                None,
            ),
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
                Some(note("waypoint: rail times", true, &[])),
                Some("idle for 31 h, with no terminal window and nothing attached"),
            ),
            proc(30656, 30655, ProcKind::PtyHost, Some(find("c4d2")), State::Idle, None, 41, Some(0.0), None, None),
            proc(38410, 3120, ProcKind::Session, Some(find("e0b3")), State::Idle, Some(9 * HOUR as u64), 466, Some(0.0), Some(note("fernlight: chapter 4", true, &[])), None),
            proc(39977, 3120, ProcKind::Session, Some(find("5a7d")), State::Idle, Some(2 * HOUR as u64), 390, Some(0.1), Some(note("", true, &[])), None),
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
                Some(note("tideline: unfinished work", true, &[])),
                Some("not tied to any session; its terminal has gone"),
            ),
        ];
        // Claude's agent list, open in a terminal window.
        let mut list = proc(45120, 3120, ProcKind::Dashboard, None, State::Unknown, None, 152, Some(0.3), None, None);
        list.title = Some("Claude's agent list".into());
        list.command = "claude agents".into();
        list.location = Some(Location { pid: 3120, app: Some("Terminal".into()) });
        procs.push(list);
        procs[4].command = "claude attach bg-5f2c --pty".into();
        let (a, b) = (procs[3].rss_bytes, procs[4].rss_bytes);
        procs[3].children = vec![44872];
        procs[3].rss_bytes = a + b;
        let (a, b) = (procs[5].rss_bytes, procs[6].rss_bytes);
        procs[5].children = vec![30656];
        procs[5].rss_bytes = a + b;
        procs[10].agent = Some(Agent::Claude);
        // An agent the tideline session started as a process of its own (a `claude -p` review).
        let mut review = proc(43318, 43310, ProcKind::Subagent, None, State::Unknown, None, 61, Some(4.0), None, None);
        review.title = Some("Review the scope diff".into());
        review.command = "claude -p review the scope diff".into();
        procs[1].children.push(43318);
        procs[1].rss_bytes += review.rss_bytes;
        procs.push(review);
        // Background tasks: a command in each of two terminal sessions (one idle, its task still
        // running), and a background job's command and monitor (its job record lists those).
        let task = |kind: &str, label: &str, secs: i64, pid: Option<u32>| TaskSummary { kind: kind.into(), label: label.into(), started_at: Some(ago(secs)), pid };
        procs[1].tasks = vec![task("shell", "cargo test -p tideline scope", 3 * MIN, Some(43402))];
        procs[1].others = vec![43402, 43403];
        procs[1].others_rss_bytes = 1_204 * 1_048_576;
        procs[1].rss_bytes += procs[1].others_rss_bytes;
        procs[2].tasks = vec![task("shell", "npm run dev", 25 * MIN, Some(41090))];
        procs[3].tasks = vec![task("shell", "cargo build --release", 6 * MIN, None), task("monitor", "the level loader's test log", 12 * MIN, None)];

        Self {
            state: Mutex::new(Pretend { behind: demo_behind(&projects), projects, sessions, ideas, procs, pauses: Vec::new(), next_idea: 8, groups: Vec::new() }),
            questions: crate::questions::Demo::new(),
            newproject: crate::newproject::Demo::default(),
        }
    }
}

fn idea(id: &str, secs: i64, text: &str, project: Option<&str>, source: &str, state: IdeaState) -> Idea {
    Idea {
        id: id.into(),
        at: ago(secs),
        text: text.into(),
        project: project.map(Into::into),
        project_name: project.map(Into::into),
        source: source.into(),
        state,
        sent_at: (state == IdeaState::Sent).then(|| ago(secs.saturating_sub(600))),
        brief: (state == IdeaState::Sent).then(|| "b3f9a1c07d2e4".into()),
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
            ideas: self.ideas.iter().filter(|i| i.state == IdeaState::Inbox).count() as u32,
            projects,
            sessions: self.sessions.clone(),
            index_error: None,
            locations_unsure: None,
            behind: Some(self.behind.clone()),
            // Disk, resources, attribution (added 2026-11-20): see crate::disk's demo data.
            resources: Some(resources_of(&self.procs, &self.sessions)),
            disk: Some(disk_report()),
            attribution: Some(attribution_of(&self.sessions)),
            groups: self.all_groups(),
        }
    }

    /// The groups kept on their own, in their order, then those only a project names, A to Z.
    fn all_groups(&self) -> Vec<String> {
        let mut extra: Vec<String> = Vec::new();
        for g in self.projects.iter().filter_map(|p| p.group.clone()) {
            if !self.groups.iter().chain(extra.iter()).any(|x| x.eq_ignore_ascii_case(&g)) {
                extra.push(g);
            }
        }
        extra.sort_by_key(|g| g.to_lowercase());
        let mut out = self.groups.clone();
        out.extend(extra);
        out
    }
}

impl Source for DemoSource {
    fn overview(&self) -> Overview {
        self.st().overview()
    }

    fn session(&self, id: &str) -> Option<SessionDetail> {
        let st = self.st();
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
        let st = self.st();
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
        let st = self.st();
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
        let st = self.st();
        let q = query.to_lowercase();
        st.sessions
            .iter()
            .filter_map(|s| {
                let hay = [("title", Some(&s.title)), ("prompt", s.last_prompt.as_ref()), ("recap", s.away_summary.as_ref()), ("project", s.project_name.as_ref())];
                hay.into_iter().find_map(|(k, t)| t.filter(|t| t.to_lowercase().contains(&q)).map(|t| (s.clone(), format!("{k}: {t}"))))
            })
            .collect()
    }

    fn ideas(&self) -> Vec<Idea> {
        self.st().ideas.clone()
    }

    fn add_idea(&self, text: &str, project: Option<&str>) -> Result<Idea, String> {
        let mut st = self.st();
        let id = format!("i{}", st.next_idea);
        st.next_idea += 1;
        let i = idea(&id, 0, text, project, "typed", IdeaState::Inbox);
        st.ideas.insert(0, i.clone());
        Ok(i)
    }

    fn set_idea(&self, id: &str, state: Option<IdeaState>, project: Option<Option<&str>>) -> Result<Idea, String> {
        let mut st = self.st();
        let i = st.ideas.iter_mut().find(|i| i.id == id).ok_or("No such idea.")?;
        if let Some(s) = state {
            i.state = s;
            i.sent_at = None;
        }
        if let Some(p) = project {
            i.project = p.map(Into::into);
            i.project_name = p.map(Into::into);
        }
        Ok(i.clone())
    }

    fn pause(&self, project: &str, note: &str, next: &str) -> Result<Checkpoint, String> {
        let mut st = self.st();
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
        let mut st = self.st();
        let name = crate::fmt::base(path);
        let p = project(&name, "", "", "", 0, 0, 0, 0);
        st.projects.push(p.clone());
        Ok(p)
    }

    fn remove_project(&self, id: &str, removed: bool) -> Result<ProjectSummary, String> {
        let mut st = self.st();
        let p = st.projects.iter_mut().find(|p| p.id == id).ok_or("No such project.")?;
        p.removed = removed;
        Ok(p.clone())
    }

    fn agent_choices(&self) -> Vec<Agent> {
        Agent::ALL.into_iter().filter(|a| a.can_brief()).collect()
    }

    fn brief_command(&self, agent: Agent, id: &str) -> Option<String> {
        wake_core::brief::command(agent, &wake_core::brief::path(std::path::Path::new("/home/you/.local/share/wake"), id))
    }

    /// Nothing is written or typed: the ideas are only marked sent in the made-up data.
    fn start_agent(&self, req: &wake_core::brief::StartAgent, _dry_run: bool) -> ActionResult {
        let mut st = self.st();
        if req.ideas.iter().any(|id| !st.ideas.iter().any(|i| &i.id == id && i.project.as_deref() == Some(req.project.as_str()))) {
            return ActionResult { ok: false, message: format!("Those ideas aren't all for {}. Start one agent per project.", req.project), steps: vec![] };
        }
        let at = ago(0);
        for i in st.ideas.iter_mut().filter(|i| req.ideas.contains(&i.id)) {
            i.state = IdeaState::Sent;
            i.sent_at = Some(at.clone());
            i.brief = Some(req.id.clone());
        }
        let agent = req.agent.label();
        ActionResult { ok: true, message: format!("Demo: {agent} would start in a new terminal window in ~/Projects/{}.", req.project), steps: vec![] }
    }

    fn go_to(&self, session: &str, _dry_run: bool) -> ActionResult {
        let st = self.st();
        let Some(s) = st.sessions.iter().find(|s| s.id == session) else {
            return ActionResult { ok: false, message: "Wake can't find that session.".into(), steps: Vec::new() };
        };
        let (message, steps) = if let Some(l) = &s.location {
            (format!("Demo: would bring the window of process {} forward.", l.pid), vec![format!("Look.Start {}", l.pid)])
        } else if let Some(job) = &s.job_id {
            (format!("Demo: would open a new terminal window running claude attach {job}."), vec![format!("xdg-terminal-exec claude attach {job}")])
        } else if let Some(cmd) = &s.resume_command {
            (format!("Demo: would open a new terminal window running {cmd}."), vec![format!("xdg-terminal-exec {cmd}")])
        } else {
            return ActionResult { ok: false, message: "Claude deleted this conversation, so it can't be reopened.".into(), steps: Vec::new() };
        };
        ActionResult { ok: true, message, steps }
    }

    fn go_to_window(&self, pid: u32, _dry_run: bool) -> ActionResult {
        let st = self.st();
        match st.procs.iter().find(|p| p.pid == pid && p.kind == ProcKind::Dashboard).and_then(|p| p.location.clone()) {
            Some(l) => ActionResult { ok: true, message: format!("Demo: would bring the window of process {} forward.", l.pid), steps: vec![format!("Look.Start {}", l.pid)] },
            None => ActionResult { ok: false, message: "That agent list has closed.".into(), steps: Vec::new() },
        }
    }

    fn processes(&self) -> ProcOverview {
        let st = self.st();
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

    /// Made-up terminals: a working agent in a window, a background one, and a quiet prompt for the
    /// other live sessions. The working ones grow a line every second so the preview looks live.
    fn screen(&self, session: &str) -> Option<Screen> {
        let s = self.st().sessions.iter().find(|s| s.id == session).cloned()?;
        if !matches!(s.state, State::Working | State::Waiting | State::Idle) {
            return None;
        }
        let tick = (glib::real_time() / 1_000_000) as usize;
        let (source, mut lines): (String, Vec<String>) = match session {
            "2b8f0d11-slip-scope" => ("Terminal".into(), DEMO_SCOPE.lines().map(String::from).collect()),
            "91aa4e7c-skyhook-bg" => ("claude logs".into(), DEMO_SKYHOOK.lines().map(String::from).collect()),
            _ => (
                s.location.as_ref().and_then(|l| l.app.clone()).unwrap_or_else(|| "claude logs".into()),
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
        let mut st = self.st();
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
        self.newproject.stop(thread, false) || self.questions.cancel(thread)
    }

    fn delete_thread(&self, thread: &str) -> Result<(), String> {
        if !self.newproject.stop(thread, true) {
            self.questions.delete(thread);
        }
        Ok(())
    }

    fn retry_question(&self, thread: &str) -> Result<(), String> {
        if self.newproject.retry(thread) { Ok(()) } else { Err("Demo data: Wake doesn't send questions to Claude here.".into()) }
    }

    fn interview_ai(&self) -> Agent {
        Agent::Claude
    }

    fn interview(&self, thread: Option<&str>, answer: &str) -> Result<String, String> {
        self.newproject.interview(thread, answer)
    }

    fn interview_state(&self, thread: &str, since: u64) -> Option<QOverview> {
        self.newproject.state(thread, since)
    }

    fn new_project_folder(&self, folder: &str) -> Result<String, String> {
        let taken: Vec<String> = self.st().projects.iter().map(|p| p.name.clone()).collect();
        crate::newproject::demo_folder(folder, &taken)
    }

    fn create_project(&self, plan: &wake_core::newproject::Plan, agent: Agent, _dry_run: bool) -> ActionResult {
        let taken: Vec<String> = self.st().projects.iter().map(|p| p.name.clone()).collect();
        crate::newproject::demo_create(plan, agent, &taken)
    }

    fn mark_read(&self, thread: &str) {
        self.questions.mark_read(thread)
    }

    fn question_ais(&self) -> Vec<wake_core::questions::AiChoice> {
        use wake_core::Agent;
        use wake_core::questions::choice;
        // Made up: all three installed, as on the machine this was written on.
        Agent::ALL.into_iter().map(|ai| choice(ai, Some(std::path::Path::new("/demo")), false)).collect()
    }

    /// Set project (disk.rs): the demo's sessions change in memory only.
    fn set_session_project(&self, session: &str, project: Option<Option<&str>>) -> Result<(), String> {
        let mut st = self.st();
        let s = st.sessions.iter_mut().find(|s| s.id == session).ok_or("No session with that id.")?;
        if let Some(p) = project {
            s.project = p.map(String::from);
            s.project_name = p.map(String::from);
        }
        Ok(())
    }

    fn set_project_color(&self, id: &str, color: Option<&str>) -> Result<ProjectSummary, String> {
        let mut st = self.st();
        let p = st.projects.iter_mut().find(|p| p.id == id).ok_or("No project with that id.")?;
        p.color = Some(color.map_or_else(|| wake_core::store::default_colour(&p.name).to_string(), String::from));
        Ok(p.clone())
    }

    fn set_next(&self, id: &str, next: &str) -> Result<ProjectSummary, String> {
        let mut st = self.st();
        let p = st.projects.iter_mut().find(|p| p.id == id).ok_or("No project with that id.")?;
        p.next = next.into();
        Ok(p.clone())
    }

    fn set_project_group(&self, id: &str, group: Option<&str>) -> Result<ProjectSummary, String> {
        let mut st = self.st();
        let group = group.map(str::trim).filter(|g| !g.is_empty()).map(String::from);
        let p = st.projects.iter_mut().find(|p| p.id == id).ok_or("No project with that id.")?;
        p.group.clone_from(&group);
        let out = p.clone();
        if let Some(g) = group
            && !st.groups.contains(&g)
        {
            st.groups.push(g);
        }
        Ok(out)
    }

    fn add_group(&self, name: &str) -> Result<String, String> {
        let name: String = name.trim().chars().take(40).collect();
        if name.is_empty() {
            return Err("A group needs a name.".into());
        }
        let mut st = self.st();
        if let Some(same) = st.all_groups().into_iter().find(|g| g.eq_ignore_ascii_case(&name)) {
            if !st.groups.contains(&same) {
                st.groups.push(same.clone());
            }
            return Ok(same);
        }
        st.groups.push(name.clone());
        Ok(name)
    }

    fn order_groups(&self, order: &[String]) -> Result<(), String> {
        let mut st = self.st();
        let known = st.all_groups();
        let mut out: Vec<String> = Vec::new();
        for g in order.iter().chain(known.iter()) {
            if known.contains(g) && !out.contains(g) {
                out.push(g.clone());
            }
        }
        st.groups = out;
        Ok(())
    }

    fn remove_group(&self, name: &str) -> Result<usize, String> {
        let mut st = self.st();
        st.groups.retain(|g| g != name);
        let mut moved = 0;
        for p in st.projects.iter_mut().filter(|p| p.group.as_deref() == Some(name)) {
            p.group = None;
            moved += 1;
        }
        Ok(moved)
    }

    fn set_project_importance(&self, id: &str, importance: Option<&str>) -> Result<ProjectSummary, String> {
        let mut st = self.st();
        let p = st.projects.iter_mut().find(|p| p.id == id).ok_or("No project with that id.")?;
        p.importance = importance.map(String::from);
        Ok(p.clone())
    }

    fn ack_behind(&self, project: &str) {
        for b in self.st().behind.iter_mut().filter(|b| b.project == project) {
            b.alarm = None;
        }
    }

    fn pull(&self, project: &str, dry_run: bool) -> ActionResult {
        let mut st = self.st();
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

// ---------- the disk report

const H: f64 = 3600.0;
const GB: u64 = 1 << 30;
const MB: u64 = 1 << 20;

#[expect(clippy::too_many_arguments, reason = "made-up data, one argument per column")]
fn item(path: &str, kind: DiskKind, name: &str, bytes: u64, used: f64, safety: DiskSafety, cost: &str, action: Option<DiskAction>) -> DiskItem {
    DiskItem {
        path: path.into(),
        kind,
        name: name.into(),
        bytes,
        partial: false,
        last_used: Some(ago(used as i64)),
        safety,
        cost: cost.into(),
        action,
        command: None,
        command_note: None,
        worktree: None,
        within: None,
        refused: None,
    }
}

fn cmd(mut i: DiskItem, c: &str, note: &str) -> DiskItem {
    i.command = Some(c.into());
    i.command_note = Some(note.into());
    i
}

const P: &str = "/home/sam/Projects";
const CONV: &str = "Deleting one loses that session's history and resume; Wake keeps its summary. Claude deletes conversations older than its cleanup period (30 days unless cleanupPeriodDays in ~/.claude/settings.json says otherwise) by itself.";
const REBUILD: &str = "Rebuilds on the next build (a few minutes).";

/// Made-up measurements, shaped like a developer laptop with a few busy projects.
fn disk_report() -> DiskReport {
    use DiskAction::*;
    use DiskKind::*;
    use DiskSafety::*;
    let mut slip = vec![
        item(
            &format!("{P}/tideline-desktop/target"),
            BuildOutput,
            "Rust build output",
            15 * GB + 300 * MB,
            2.0 * H,
            Safe,
            "Rebuilds on the next build: a long one on this computer (tens of minutes, memory-heavy).",
            Some(Clear),
        ),
        {
            let mut i = item(
                &format!("{P}/tideline-desktop/.claude/worktrees/agent-ace0489"),
                Worktree,
                "Agent worktree for branch worktree-agent-ace0489 (not merged)",
                3 * GB + 600 * MB,
                3.0 * H,
                Check,
                "Its branch worktree-agent-ace0489 has commits main doesn't: removing it could lose work. 3.6 GB of it is build output, listed below.",
                Some(OpenFolder),
            );
            i.worktree = Some(WorktreeInfo {
                repo: format!("{P}/tideline-desktop"),
                branch: Some("worktree-agent-ace0489".into()),
                default_branch: Some("main".into()),
                merged: Some(false),
                dirty_files: Some(0),
                in_use: None,
                made_by: Some("2b8f0d11-slip-scope".into()),
                made_by_title: Some("scope-helper-rows".into()),
            });
            i.refused = Some("Its branch isn't merged.".into());
            i
        },
        {
            let mut i = item(
                &format!("{P}/tideline-desktop/.claude/worktrees/agent-ace0489/target"),
                BuildOutput,
                "Rust build output in worktree agent-ace0489",
                3 * GB + 580 * MB,
                3.0 * H,
                Safe,
                REBUILD,
                Some(Clear),
            );
            i.within = Some(format!("{P}/tideline-desktop/.claude/worktrees/agent-ace0489"));
            i
        },
        {
            let mut i = item(
                &format!("{P}/tideline-desktop/.claude/worktrees/focus-card-tidy"),
                Worktree,
                "Agent worktree for branch focus-card-tidy (merged)",
                412 * MB,
                26.0 * H,
                Safe,
                "Its branch focus-card-tidy is in main; removing it deletes only this copy (the branch stays). 380 MB of it is build output, listed below.",
                Some(RemoveWorktree),
            );
            i.command = Some("git -C ~/Projects/tideline-desktop worktree remove .claude/worktrees/focus-card-tidy".into());
            i.worktree = Some(WorktreeInfo {
                repo: format!("{P}/tideline-desktop"),
                branch: Some("focus-card-tidy".into()),
                default_branch: Some("main".into()),
                merged: Some(true),
                dirty_files: Some(0),
                in_use: None,
                made_by: Some("0f9e8d7c-slip-unknown".into()),
                made_by_title: Some("unfinished-work-check".into()),
            });
            i
        },
        {
            let mut i = item(
                &format!("{P}/tideline-desktop/.claude/worktrees/focus-card-tidy/target"),
                BuildOutput,
                "Rust build output in worktree focus-card-tidy",
                380 * MB,
                26.0 * H,
                Safe,
                REBUILD,
                Some(Clear),
            );
            i.within = Some(format!("{P}/tideline-desktop/.claude/worktrees/focus-card-tidy"));
            i
        },
        item(
            "/home/sam/.cache/tideline-smoke",
            Cache,
            "Tideline's smoke-test clips and screens (~/.cache/tideline-smoke)",
            954 * MB,
            5.0 * H,
            Safe,
            "Test or tool output: the next run makes it again.",
            Some(Clear),
        ),
        item("/home/sam/.claude/projects", Transcripts, "Claude conversations here (31 sessions)", 420 * MB, 60.0, Keep, CONV, None),
        item(&format!("{P}/tideline-desktop/.git"), Git, "Git history (.git)", 310 * MB, 2.0 * H, Keep, "The project's history. Never delete it.", None),
    ];
    // The demo's scope-helper-rows session is working here.
    for i in slip.iter_mut().filter(|i| i.action == Some(Clear) && !i.path.contains(".cache")) {
        i.refused = Some("An agent in this project is working; it may be building into this. Try when it's idle.".into());
    }
    let phase = vec![
        item(
            &format!("{P}/Skyhook/archive"),
            Archive,
            "Archive or exported builds (archive)",
            3 * GB + 100 * MB,
            40.0 * 24.0 * H,
            Keep,
            "May be the only copy of an old version or a shipped build. Wake won't delete it.",
            Some(OpenFolder),
        ),
        item(
            "/home/sam/.cache/skyhook-probe",
            Cache,
            "Skyhook's probe recordings (~/.cache/skyhook-probe)",
            2 * GB + 700 * MB,
            2.0 * 24.0 * H,
            Safe,
            "Test or tool output: the next run makes it again.",
            Some(Clear),
        ),
        item(
            &format!("{P}/Skyhook/Builds"),
            Archive,
            "Archive or exported builds (Builds)",
            2 * GB + 100 * MB,
            9.0 * 24.0 * H,
            Keep,
            "May be the only copy of an old version or a shipped build. Wake won't delete it.",
            Some(OpenFolder),
        ),
        item(
            &format!("{P}/Skyhook/game/.godot"),
            BuildOutput,
            "Godot import cache in game",
            640 * MB,
            2.0 * 24.0 * H,
            Safe,
            "Godot imports the assets again next time the project opens.",
            Some(Clear),
        ),
        item("/home/sam/.claude/projects", Transcripts, "Claude conversations here (12 sessions)", 180 * MB, 4.0 * 24.0 * H, Keep, CONV, None),
    ];
    let school = vec![
        item("/home/sam/.claude/projects", Transcripts, "Claude conversations here (22 sessions)", 610 * MB, 40.0, Keep, CONV, None),
        item(
            &format!("{P}/Gradebook/node_modules"),
            BuildOutput,
            "JavaScript packages (node_modules)",
            420 * MB,
            6.0 * 24.0 * H,
            Safe,
            "Comes back with npm install (needs the network).",
            Some(Clear),
        ),
        item(&format!("{P}/Gradebook/tmp"), BuildOutput, "Rails temporary files (tmp)", 181 * MB, 11.0 * H, Safe, "Rails makes it again; restart a running dev server afterwards.", Some(Clear)),
        cmd(
            item(
                "/home/sam/.claude/jobs",
                JobFiles,
                "Background job folders here (4)",
                96 * MB,
                11.0 * H,
                Check,
                "Each job's tmp folder holds its work files, and hand-back files (briefs, reports) live there too: look before deleting.",
                None,
            ),
            "du -sh ~/.claude/jobs/{7c1e9a40,3c3c1b2a,5e0d9c61,a83f27d0}",
            "shows what's in them, biggest first; changes nothing",
        ),
    ];
    let west = vec![item(
        &format!("{P}/waypoint/.venv"),
        BuildOutput,
        "Python environment (.venv)",
        380 * MB,
        5.0 * 24.0 * H,
        Safe,
        "Comes back by installing the packages again (needs the network).",
        Some(Clear),
    )];
    let glim = vec![item(
        &format!("{P}/fernlight/node_modules"),
        BuildOutput,
        "JavaScript packages (node_modules)",
        210 * MB,
        3.0 * 24.0 * H,
        Safe,
        "Comes back with npm install (needs the network).",
        Some(Clear),
    )];
    let pd = |id: &str, extra: u64, items: Vec<DiskItem>| {
        let top: u64 = items.iter().filter(|i| i.within.is_none()).map(|i| i.bytes).sum();
        let rebuildable = items.iter().filter(|i| matches!(i.kind, BuildOutput | Cache) && i.safety == Safe).map(|i| i.bytes).sum();
        ProjectDisk { project: id.into(), bytes: top + extra, rebuildable, items, measured_at: Some(ago(12 * MIN)), partial: false }
    };
    let projects = vec![pd("tideline-desktop", 210 * MB, slip), pd("Skyhook", 380 * MB, phase), pd("Gradebook", 260 * MB, school), pd("waypoint", 40 * MB, west), pd("fernlight", 18 * MB, glim)];
    let home = "/home/sam";
    let machine_items = vec![
        cmd(
            item("/var/lib/flatpak", System, "Flatpak apps and runtimes (system)", 20 * GB, 9.0 * 24.0 * H, Keep, "Installed apps and what they run on.", None),
            "flatpak uninstall --unused",
            "removes runtimes no installed app uses; asks first",
        ),
        cmd(
            item(&format!("{home}/.local/share/flatpak"), System, "Flatpak apps and runtimes (yours)", 11 * GB, 3.0 * 24.0 * H, Keep, "Installed apps and what they run on.", None),
            "flatpak uninstall --unused",
            "removes runtimes no installed app uses; asks first",
        ),
        cmd(
            item(
                &format!("{home}/.local/share/containers"),
                System,
                "Containers: devbox (running), gradebook-postgres (running), docs-preview (stopped 6 days), old-toolbox (stopped 2 days)",
                6 * GB + 800 * MB,
                0.0,
                Keep,
                "3.7 GB is in stopped containers. Wake never removes a container.",
                None,
            ),
            "podman ps -a",
            "lists them; a stopped distrobox still holds what you installed in it, so remove one only if you're sure (distrobox rm <name>)",
        ),
        cmd(
            item(&format!("{home}/.local/share/containers"), System, "Container images", 6 * GB + 600 * MB, 0.0, Keep, "1.5 GB of it nothing uses.", None),
            "podman image prune",
            "removes images nothing uses and nothing names (dangling); asks first",
        ),
        cmd(
            item(&format!("{home}/.local/share/containers"), System, "Container volumes", 2 * GB + 300 * MB, 0.0, Keep, "10 MB nothing uses.", None),
            "podman volume ls",
            "lists them; volumes hold data such as databases",
        ),
        item(&format!("{home}/.claude/projects"), Transcripts, "Claude conversations with no project (96 sessions)", 2 * GB + 100 * MB, 2.0 * H, Keep, CONV, None),
        cmd(
            item(&format!("{home}/.rustup"), System, "Rust toolchains (rustup)", GB + 600 * MB, 6.0 * 24.0 * H, Keep, "Every Rust project builds with these.", None),
            "rustup toolchain list",
            "lists them; rustup toolchain uninstall <name> removes one you no longer use",
        ),
        item(
            &format!("{home}/.cargo/registry"),
            Cache,
            "Downloaded Rust crates (cargo registry)",
            GB + 300 * MB,
            3.0 * H,
            Safe,
            "Cargo downloads them again on the next build (needs the network).",
            Some(Clear),
        ),
        cmd(
            item(
                &format!("{home}/.local/share/Trash"),
                System,
                "The Trash",
                GB + 200 * MB,
                20.0 * 24.0 * H,
                Check,
                "What you (or Wake's Clear) moved there: the space comes back only when it is emptied.",
                None,
            ),
            "gio trash --empty",
            "empties the Trash for good",
        ),
        item(&format!("{home}/.codex"), Transcripts, "Codex conversations (~/.codex)", 606 * MB, 8.0 * 24.0 * H, Keep, "Deleting loses those sessions' history.", None),
        cmd(
            item(
                &format!("{home}/.claude/jobs"),
                JobFiles,
                "Background job folders with no project (38)",
                512 * MB,
                2.0 * H,
                Check,
                "Each job's tmp folder holds its work files, and hand-back files (briefs, reports) live there too: look before deleting.",
                None,
            ),
            "du -sh ~/.claude/jobs/{4be1a7d2,9c03f5e8,d17a6b40,3f8e2c95,60b4d1aa,e2975c3f}",
            "shows what's in them, biggest first; changes nothing",
        ),
        item(
            &format!("{home}/.cache/supertuxkart"),
            Cache,
            "supertuxkart cache (~/.cache/supertuxkart)",
            345 * MB,
            30.0 * 24.0 * H,
            Safe,
            "A cache: the app fills it again when it needs it.",
            Some(Clear),
        ),
        item(&format!("{home}/.cache/Homebrew"), Cache, "Homebrew's downloads (~/.cache/Homebrew)", 289 * MB, 12.0 * 24.0 * H, Safe, "A cache: the app fills it again when it needs it.", Some(Clear)),
    ];
    let mrebuild: u64 = machine_items.iter().filter(|i| matches!(i.kind, BuildOutput | Cache)).map(|i| i.bytes).sum();
    let rebuildable = projects.iter().map(|p| p.rebuildable).sum::<u64>() + mrebuild;
    DiskReport {
        measured_at: Some(ago(12 * MIN)),
        measuring: None,
        projects,
        machine: Some(MachineDisk {
            home_mount: "/home".into(),
            home_total: 244 * GB,
            home_free: 86 * GB,
            root_note: Some("/ shows as full: on an atomic system such as Bazzite, that is the read-only system image, always full by design. Nothing there needs cleaning.".into()),
            items: machine_items,
            rebuildable: mrebuild,
        }),
        rebuildable,
    }
}

/// The demo's processes added up the way the real ones are.
fn resources_of(procs: &[AgentProc], sessions: &[SessionSummary]) -> Resources {
    let top: u64 = procs.iter().filter(|p| !procs.iter().any(|q| q.children.contains(&p.pid))).map(|p| p.rss_bytes).sum();
    let ov = ProcOverview { procs: procs.to_vec(), total_rss_bytes: top, ..Default::default() };
    let project_of = |sid: &str| sessions.iter().find(|s| s.id == sid).and_then(|s| s.project.clone());
    wake_core::procs::resources(&ov, &project_of)
}

/// The demo's guesses: its background job started in ~ that changed files in Skyhook, and the
/// home session with no project.
fn attribution_of(sessions: &[SessionSummary]) -> Vec<Attribution> {
    let mut out = Vec::new();
    for s in sessions {
        if s.id.starts_with("91aa4e7c") && s.project.as_deref() == Some("Skyhook") {
            out.push(Attribution { session: s.id.clone(), project: s.project.clone(), how: "files".into(), words: "probably Skyhook (it changed files there)".into(), started_at_home: true });
        } else if s.project.is_none() {
            out.push(Attribution { session: s.id.clone(), project: None, how: "none".into(), words: "no project".into(), started_at_home: true });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_totals_add_up_and_nothing_risky_offers_a_delete() {
        let r = disk_report();
        for p in &r.projects {
            for i in &p.items {
                if matches!(i.kind, DiskKind::Archive | DiskKind::Folder | DiskKind::Git | DiskKind::Transcripts | DiskKind::JobFiles | DiskKind::System) {
                    assert!(!matches!(i.action, Some(DiskAction::Clear) | Some(DiskAction::RemoveWorktree)), "{}", i.name);
                }
            }
            assert!(p.bytes >= p.rebuildable, "{}", p.project);
        }
        let slip = r.project("tideline-desktop").unwrap();
        assert_eq!(util::bytes(slip.bytes), "21 GB");
        assert!(r.machine.as_ref().unwrap().items.iter().all(|i| i.kind != DiskKind::System || i.command.is_some()));
    }
}
