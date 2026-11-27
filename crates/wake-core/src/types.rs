//! The shapes Wake's core hands to its interfaces. This file is the contract between `wake-core` and
//! the GTK app: change it only by adding `Option` fields or new variants at the end, never by renaming.
//! Times are ISO-8601 UTC strings, sizes in bytes.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Working,
    Waiting,
    Idle,
    Ended,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Claude,
    Codex,
    Gemini,
}

/// Where a live session can be seen: a Konsole tab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub konsole_pid: u32,
    pub service: String,
    pub window: u32,
    pub session: u32,
    pub tab_index: u32,
    pub tab_title: String,
    /// How to name this tab to a person, unique across every Konsole on the desktop: "Konsole · tab 2"
    /// with one window, "Konsole 2 · tab 1" with several. (`window` is only unique within one Konsole
    /// process: two Konsoles each have a window 1, so "Konsole {window} · tab {n}" can name two tabs.)
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubagentCounts {
    pub total: u32,
    pub running: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub agent: Agent,
    pub project: Option<String>,
    pub project_name: Option<String>,
    pub title: String,
    pub state: State,
    /// How Wake knows the state, in plain words: "working, last moved 4 s ago", "ended 3 h ago".
    pub evidence: String,
    pub started_at: Option<String>,
    pub last_at: Option<String>,
    pub minutes: u32,
    pub prompts: u32,
    pub last_prompt: Option<String>,
    /// Claude's own "while you were away" recap, the last one.
    pub away_summary: Option<String>,
    /// Last assistant text when it ends in a question and the session is idle.
    pub question: Option<String>,
    pub subagents: SubagentCounts,
    pub cwd: Option<String>,
    pub location: Option<Location>,
    pub resume_command: Option<String>,
    /// The background job id, for `claude attach <id>` / `claude stop <id>`.
    pub job_id: Option<String>,
    pub pid: Option<u32>,
    pub deleted_by_claude: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prompt {
    pub at: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileTouch {
    pub path: String,
    pub edits: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentSummary {
    pub agent_id: String,
    pub task: String,
    /// The subagent type, e.g. "Explore", "general-purpose".
    pub kind: String,
    /// Working, Ended (finished) or Unknown.
    pub state: State,
    pub started_at: Option<String>,
    pub last_at: Option<String>,
    pub seconds: u32,
    /// Tool name -> uses, most used first.
    pub tools: Vec<(String, u32)>,
    pub handed_back: Option<String>,
    /// For a running one: what it is doing now, e.g. "Bash: cargo test -p slipstream bar".
    pub live: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub at: Option<String>,
    /// read | searched | edited | wrote | ran | fetched | asked | other
    pub verb: String,
    pub what: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentDetail {
    pub summary: SubagentSummary,
    pub steps: Vec<Step>,
    pub handed_back_full: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDetail {
    pub summary: SessionSummary,
    pub prompts: Vec<Prompt>,
    pub files: Vec<FileTouch>,
    pub commands: u32,
    pub tests: u32,
    pub pr_links: Vec<String>,
    pub cost_usd: Option<f64>,
    pub subagents: Vec<SubagentSummary>,
    /// Where the agent's own transcript is, as `~/…` (a background session's job id, for `claude
    /// attach`, is `summary.job_id`). Added 2026-11-20.
    #[serde(default)]
    pub transcript: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LiveCounts {
    pub working: u32,
    pub waiting: u32,
    pub idle: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GitTotals {
    pub ahead_total: u32,
    pub no_upstream: u32,
    pub dirty_files: u32,
    /// How many of the project's repos have no remote at all (only on this computer). Added
    /// 2026-11-20.
    #[serde(default)]
    pub no_remote: u32,
    /// True when it has a remote and every remote is on github.com, so "not on GitHub" is true.
    /// Added 2026-11-20.
    #[serde(default)]
    pub github: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PauseNote {
    pub at: String,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSummary {
    pub id: String,
    pub name: String,
    pub roots: Vec<String>,
    pub removed: bool,
    pub desc: String,
    pub next: String,
    pub why: String,
    pub live: LiveCounts,
    pub last_at: Option<String>,
    pub sessions_14d: u32,
    pub ideas: u32,
    pub git: GitTotals,
    pub paused: Option<PauseNote>,
    /// Claude's last "while you were away" recap in this project, if any.
    pub last_summary: Option<String>,
    /// False until Wake has read this project's git once; `git` is all zeros until then.
    #[serde(default)]
    pub git_checked: bool,
    /// The project's dot colour (a swatch name from `store::COLOUR_NAMES`): the one picked, else
    /// a stable one from the name. Added 2026-11-20.
    #[serde(default)]
    pub color: Option<String>,
    /// Set when the last check of one of its repos got no answer from the remote, as a sentence:
    /// "Wake couldn't ask GitHub (offline)." (so "in step" isn't claimed). Added 2026-11-20.
    #[serde(default)]
    pub remote_problem: Option<String>,
    /// The group the person put it in ("School", "Games"), if any. Added 2026-11-25.
    #[serde(default)]
    pub group: Option<String>,
    /// "high" or "low" when the person set it; None is normal. Added 2026-11-25.
    #[serde(default)]
    pub importance: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoState {
    pub path: String,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub upstream: Option<String>,
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
    pub dirty_files: u32,
    pub untracked: u32,
    pub branches_no_upstream: u32,
    pub branches: u32,
    pub worktrees: u32,
    pub last_commit_at: Option<String>,
    pub last_commit_subject: Option<String>,
    /// Where its remote lives, as `remote::host_of_url` names it ("GitHub", "GitLab", "Codeberg",
    /// else "the remote"); None when the repo has no remote. Added 2026-11-20.
    #[serde(default)]
    pub remote_host: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub project: String,
    pub at: String,
    pub note: String,
    pub next: String,
    /// repo path -> (branch, head, ahead)
    pub git: Vec<(String, Option<String>, Option<String>, Option<u32>)>,
    pub sessions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Idea {
    pub id: String,
    pub at: String,
    pub text: String,
    pub project: Option<String>,
    pub project_name: Option<String>,
    /// typed | shell | prompt
    pub source: String,
    /// inbox | done | sent (sent to an agent, added 2026-11-20)
    pub state: String,
    /// When it was sent to an agent (state "sent"); its project is the one it was sent for.
    /// Added 2026-11-20.
    #[serde(default)]
    pub sent_at: Option<String>,
    /// The id of the brief it was sent in (`<data>/briefs/<id>.md`), when Wake kept it. Added
    /// 2026-11-20.
    #[serde(default)]
    pub brief: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectDetail {
    pub summary: ProjectSummary,
    pub repos: Vec<RepoState>,
    pub sessions: Vec<SessionSummary>,
    pub ideas: Vec<Idea>,
    pub last_pause: Option<Checkpoint>,
    pub since_pause: Vec<String>,
    pub could_not_check: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Progress {
    pub done: u32,
    pub total: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Overview {
    pub generated_at: String,
    pub indexing: Progress,
    pub counts: LiveCounts,
    pub ideas: u32,
    pub projects: Vec<ProjectSummary>,
    /// All live sessions plus the last 14 days, newest first.
    pub sessions: Vec<SessionSummary>,
    pub index_error: Option<String>,
    /// Set while Wake cannot be sure where live sessions are: it has not finished looking at Konsole
    /// yet, or Konsole did not answer (then the tabs shown are the last ones seen, under 15 s old).
    /// A live session with no `location` should then read "checking" or "unsure", not "background".
    #[serde(default)]
    pub locations_unsure: Option<String>,
    /// Tracked projects that are behind their remote (added 2026-11-20), at most one entry each.
    /// None until Wake has looked; Some(empty) when none is behind.
    #[serde(default)]
    pub behind: Option<Vec<Behind>>,
    /// Memory and CPU by session and by project (added 2026-11-20). Kept out of the sessions and
    /// projects themselves so a view that compares them to decide whether to redraw doesn't
    /// redraw every few seconds as CPU moves. None when processes weren't read.
    #[serde(default)]
    pub resources: Option<Resources>,
    /// What takes disk space, by project and for the machine (added 2026-11-20): the last
    /// measurement, refreshed hourly in the background. None until the first one is in.
    #[serde(default)]
    pub disk: Option<DiskReport>,
    /// How each session's project was decided, for those not placed by where they worked: set by
    /// the person, guessed (marked "probably"), or none (added 2026-11-20).
    #[serde(default)]
    pub attribution: Option<Vec<Attribution>>,
    /// The person's project groups by name, including those with no project in them yet (added
    /// 2026-11-27).
    #[serde(default)]
    pub groups: Vec<String>,
}

// ---- agent processes (the Agents view) ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProcKind {
    /// A Claude/Codex/Gemini process that owns one session.
    Session,
    /// Claude's background supervisor that keeps background sessions alive.
    Daemon,
    /// A pseudo-terminal host or attach client serving a background session.
    PtyHost,
    /// A pre-started spare session nobody has used yet.
    Spare,
    /// An agent process Wake can't tie to any session.
    Stray,
    /// A Claude/Codex/Gemini process with another agent session above it in its process tree
    /// (agent-notify's rule): a helper that session started. Folded into that session's row and
    /// memory; never a spare, a stray, a session of its own, or stale on its own.
    Subagent,
}

/// What agent-notify's note for this process says; the same data the screensaver's scope draws from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkingNote {
    /// Line 1: the name the readout shows. Empty means "use the folder".
    pub name: String,
    /// Line 2 is `idle`.
    pub idle: bool,
    /// Lines 3+: one per helper (subagent) the agent has working, by its task.
    pub helpers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentProc {
    pub pid: u32,
    pub ppid: u32,
    pub kind: ProcKind,
    pub agent: Option<Agent>,
    /// Short command line, never more than 120 chars, never showing secrets passed as arguments.
    pub command: String,
    pub session_id: Option<String>,
    pub job_id: Option<String>,
    pub title: Option<String>,
    pub project_name: Option<String>,
    pub state: State,
    pub idle_seconds: Option<u64>,
    pub rss_bytes: u64,
    /// Percent of one CPU, averaged since the last sample (None on the first sample).
    pub cpu_percent: Option<f32>,
    pub started_at: Option<String>,
    pub location: Option<Location>,
    pub note: Option<WorkingNote>,
    /// Why Wake thinks this one is probably stale, in plain words; None if it looks in use.
    pub stale_reason: Option<String>,
    /// Children counted into rss_bytes (pty host, helpers), for the tree.
    pub children: Vec<u32>,
    /// The process's start time (clock ticks since boot, /proc/<pid>/stat field 22). With the pid it
    /// names this exact process: pass it back to `Source::stop` so a reused pid is never signalled.
    #[serde(default)]
    pub start_ticks: Option<u64>,
    /// For a background session: the subagents its job record (`~/.claude/jobs/<jobId>/state.json`,
    /// `fan` entries of kind `agent` not yet done) has running, by their task, as agent-notify reads
    /// them. Live, unlike the note's helper lines. None when there is no readable job record.
    #[serde(default)]
    pub helpers: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProcOverview {
    pub generated_at: String,
    pub procs: Vec<AgentProc>,
    pub total_rss_bytes: u64,
    pub stale_rss_bytes: u64,
    pub stale_count: u32,
    /// Notes in $XDG_RUNTIME_DIR/slipstream/working whose process is gone.
    pub dead_notes: Vec<u32>,
}

/// The outcome of an action, always with a sentence for the person.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionResult {
    pub ok: bool,
    pub message: String,
    /// The exact commands or signals it did (or, dry run, would) use.
    pub steps: Vec<String>,
}

/// How to stop an agent process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopHow {
    /// `claude stop <jobId>` for a background session: the agent's own clean stop.
    AgentStop,
    /// SIGTERM, the polite signal.
    Terminate,
    /// SIGKILL, only after Terminate failed and the person confirmed again.
    Kill,
}

/// What an agent's terminal shows right now, for looking in without going there (hold-to-peek).
/// Read-only: Konsole's displayed text, or `claude logs` for a background session.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Screen {
    /// The last lines shown (at most 400), oldest first, without colour codes.
    pub lines: Vec<String>,
    /// Where they came from: a tab's label such as "Konsole 2 · tab 1", or "claude logs".
    pub source: String,
    /// When they were read (ISO-8601 UTC).
    pub at: String,
    /// A plain sentence when the screen could not be read this time.
    pub note: Option<String>,
}

/// How Wake can show the person an agent's window while they hold the peek key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LookKind {
    /// Slipstream flies to the window's workspace and rings it (its Look request).
    Slipstream,
    /// KDE Plasma: a KWin script switches to the window's desktop and highlights it.
    Kwin,
    /// No way to fly there: Wake shows a live text preview of the agent's terminal itself.
    Preview,
}

// ---- Questions (added 2026-11-20): quick questions answered by the person's own `claude` ----

/// One question and Claude's answer to it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QTurn {
    pub question: String,
    /// Markdown, as Claude wrote it. Grows while the answer streams in.
    #[serde(default)]
    pub answer: String,
    pub asked_at: String,
    /// When the answer finished; None while waiting or answering, or when it failed.
    #[serde(default)]
    pub answered_at: Option<String>,
    /// One plain line when this question could not be answered (claude missing, not logged in,
    /// stopped, ...). `Source::retry_question` asks it again.
    #[serde(default)]
    pub error: Option<String>,
}

/// A thread of questions: the first question and its follow-ups, one Claude conversation.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QThread {
    pub id: String,
    /// The first question, shortened.
    pub title: String,
    /// The project whose files Claude may read (never change); None for "just a question".
    #[serde(default)]
    pub project: Option<String>,
    /// Claude's session id, for `--resume` on follow-ups.
    #[serde(default)]
    pub claude_session: Option<String>,
    pub created: String,
    #[serde(default)]
    pub turns: Vec<QTurn>,
    /// Which AI answers this thread ("claude", "codex", "gemini"); None = Claude (threads from
    /// before 2026-11-20). `claude_session` holds that AI's own session id.
    #[serde(default)]
    pub ai: Option<String>,
}

/// Every question thread and what is happening to them now.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QOverview {
    /// Newest asked first.
    pub threads: Vec<QThread>,
    /// The thread being answered now (one at a time).
    pub answering: Option<String>,
    /// What Claude is doing for it, in a few words ("searching the web"), if not just writing.
    pub activity: Option<String>,
    /// Threads with a question queued behind the one being answered, in order.
    pub waiting: Vec<String>,
    /// Threads whose answer arrived since the person last looked at them.
    pub unread: Vec<String>,
    /// Changes whenever anything above does: pass it back to `Source::questions` to hear only of changes.
    pub seq: u64,
    /// Why the threads could not all be read, in plain words (a broken file is set aside).
    pub problem: Option<String>,
}

// ---- Behind the remote (added 2026-11-20): a project needs a pull before building ----

/// A project whose branch is behind its remote: someone pushed, and this computer hasn't pulled.
/// Read with `git ls-remote` (never a fetch) every ten minutes, and from the local tracking ref.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Behind {
    pub project: String,
    pub project_name: String,
    /// The repo that is behind (the most behind, when several are).
    pub repo: String,
    /// "origin/main".
    pub upstream: String,
    /// "GitHub", "GitLab" or "the remote".
    pub host: String,
    /// How many commits behind, when they are on this computer; None when the remote has commits
    /// this computer hasn't fetched yet (so they can't be counted without fetching).
    pub count: Option<u32>,
    /// Commits here the remote doesn't have: above 0 means the branches have diverged.
    pub ahead: u32,
    /// Files changed or new in the working tree (pulling may need a stash first).
    pub dirty: u32,
    /// "Pull now" is offered: clean tree, and the branch can fast-forward.
    pub can_pull: bool,
    /// The command that fixes it: "git pull --ff-only".
    pub fix: String,
    /// One line for the person: "Behind origin/main by 3. Pull before you build: git pull --ff-only".
    pub line: String,
    /// When the remote was last asked (ISO-8601 UTC); None if it couldn't be reached and this
    /// comes from the last fetch alone.
    pub checked_at: Option<String>,
    /// Other repos in the project that are behind too.
    pub others: u32,
    /// Set when the person started building while behind (a session began working, or files
    /// changed): why, in a few words. Cleared by `Source::ack_behind` until the project catches up
    /// and falls behind again.
    pub alarm: Option<String>,
}

// ---- Resources by project (added 2026-11-20) ----

/// One live session's processes: the agent, its pty host, attach clients and helper agents.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionResources {
    pub session: String,
    /// The agent process itself.
    pub pid: u32,
    /// Its start time, as `AgentProc::start_ticks` (pass to `Source::stop`).
    pub start_ticks: Option<u64>,
    /// Resident memory of every process below, added up.
    pub rss_bytes: u64,
    /// The agent's own memory, without its terminal host, attach clients and helpers.
    pub own_rss_bytes: u64,
    /// Percent of one CPU, all its processes together (None until two samples are in).
    pub cpu_percent: Option<f32>,
    /// Every process counted: the agent first, then its pty host, attach clients, helpers.
    pub pids: Vec<u32>,
}

/// One project's live sessions added up; `project` None is everything not in a project.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProjectResources {
    pub project: Option<String>,
    pub rss_bytes: u64,
    pub cpu_percent: Option<f32>,
    pub sessions: u32,
    pub processes: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Resources {
    pub sessions: Vec<SessionResources>,
    /// Biggest first.
    pub projects: Vec<ProjectResources>,
    /// Every agent process (daemon and spares too), for "agents use 1.8 GB".
    pub total_rss_bytes: u64,
}

impl Resources {
    pub fn session(&self, id: &str) -> Option<&SessionResources> {
        self.sessions.iter().find(|s| s.session == id)
    }
    pub fn project(&self, id: &str) -> Option<&ProjectResources> {
        self.projects.iter().find(|p| p.project.as_deref() == Some(id))
    }
}

// ---- Attribution (added 2026-11-20) ----

/// How a session's project was decided, when it wasn't simply where it worked.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Attribution {
    pub session: String,
    /// The project now shown for it (the same as the session's `project`).
    pub project: Option<String>,
    /// "set" (by the person), "files" (it changed files there), "title" (its title names it) or
    /// "none".
    pub how: String,
    /// For a person: "probably night-shift (from its title)", "no project", "wake (you set it)".
    pub words: String,
    /// It was started in the home folder, as background jobs are.
    pub started_at_home: bool,
}

impl Attribution {
    /// A guess Wake should mark "probably".
    pub fn guessed(&self) -> bool {
        self.how == "files" || self.how == "title"
    }
}

// ---- Disk (added 2026-11-20): what takes the space, in plain words ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiskKind {
    /// Build output a build makes again (cargo target, node_modules, .venv, dist, caches).
    BuildOutput,
    /// A cache that fills itself again (~/.cache/<x>, the cargo registry, test output).
    Cache,
    /// An agent's worktree: a copy of the repo on its own branch.
    Worktree,
    /// Old versions, exported builds: may be the only copy.
    Archive,
    /// A big folder Wake can't name: shown, never judged.
    Folder,
    /// The repo's own history.
    Git,
    /// Claude's conversations (transcripts and subagents).
    Transcripts,
    /// Background jobs' folders (`~/.claude/jobs/<id>`), where hand-back files live.
    JobFiles,
    /// Toolchains, installed apps, containers, the Trash: shown with the command a person would use.
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiskSafety {
    /// Comes back by itself or with the next build.
    Safe,
    /// Probably fine, but look first (a worktree, job files).
    Check,
    /// History or maybe the only copy: keep unless you know.
    Keep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiskAction {
    /// Move it to the Trash (build output and caches only).
    Clear,
    /// `git worktree remove` (merged and nobody inside only).
    RemoveWorktree,
    /// Open it in the file manager; nothing else.
    OpenFolder,
}

/// A worktree's state, for "Remove worktree".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WorktreeInfo {
    /// The repo it belongs to.
    pub repo: String,
    pub branch: Option<String>,
    /// The branch it was compared with ("main").
    pub default_branch: Option<String>,
    /// Its branch's commits are all in the default branch (None: couldn't tell).
    pub merged: Option<bool>,
    /// Files changed or new in it (None: couldn't tell).
    pub dirty_files: Option<u32>,
    /// Who is inside it now, in words ("multi-terminal-agent-workspace, PID 3007709"); None if nobody.
    pub in_use: Option<String>,
    /// The session that made it (worked in it first), and its title.
    pub made_by: Option<String>,
    pub made_by_title: Option<String>,
}

/// One thing taking space.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiskItem {
    pub path: String,
    pub kind: DiskKind,
    /// Plain words: "Rust build output", "Agent worktree for branch fix-x (merged)".
    pub name: String,
    pub bytes: u64,
    /// Measuring stopped at its time limit: at least this much.
    #[serde(default)]
    pub partial: bool,
    /// When anything in it last changed (ISO-8601 UTC).
    pub last_used: Option<String>,
    pub safety: DiskSafety,
    /// What deleting it costs: "rebuilds on the next build (a few minutes here)".
    pub cost: String,
    /// What Wake offers; None = nothing but the command below.
    pub action: Option<DiskAction>,
    /// The command a person would use, for things Wake never touches (shown, copyable).
    pub command: Option<String>,
    /// What that command does, in a few words.
    pub command_note: Option<String>,
    pub worktree: Option<WorktreeInfo>,
    /// The item this one sits inside (a worktree's build output), so totals count it once.
    pub within: Option<String>,
    /// Why the action is refused right now ("an agent in wake is working"); None = allowed.
    #[serde(default)]
    pub refused: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProjectDisk {
    pub project: String,
    /// The project's folders plus its items elsewhere (caches, conversations, job folders).
    pub bytes: u64,
    /// What a build or the app makes again by itself: build output and caches.
    pub rebuildable: u64,
    /// Biggest first; a worktree's build output right after its worktree.
    pub items: Vec<DiskItem>,
    pub measured_at: Option<String>,
    #[serde(default)]
    pub partial: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MachineDisk {
    /// "/var/home".
    pub home_mount: String,
    pub home_total: u64,
    pub home_free: u64,
    /// One sentence about `/` when it looks full but is the read-only system image (normal).
    pub root_note: Option<String>,
    /// Things outside any project: caches, toolchains, containers, apps, the Trash.
    pub items: Vec<DiskItem>,
    pub rebuildable: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DiskReport {
    /// When the last full measurement finished.
    pub measured_at: Option<String>,
    /// Set while measuring: what, in a few words ("measuring wake").
    pub measuring: Option<String>,
    pub projects: Vec<ProjectDisk>,
    pub machine: Option<MachineDisk>,
    /// Build output and caches everywhere: what could be cleared and made again.
    pub rebuildable: u64,
}

impl DiskReport {
    pub fn project(&self, id: &str) -> Option<&ProjectDisk> {
        self.projects.iter().find(|p| p.project == id)
    }
}
