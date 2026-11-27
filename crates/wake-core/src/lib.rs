//! Wake's core: everything Wake knows, with no user interface. The GTK app and the `wake` command both
//! use it through [`Source`].

#[expect(clippy::type_complexity, reason = "Checkpoint::git, part of the frozen contract")]
pub mod types;

pub mod attrib;
pub mod brief;
pub mod desktop;
pub mod disk;
pub mod gitinfo;
pub mod index;
pub mod live;
pub mod log;
pub mod look;
pub mod procs;
pub mod questions;
pub mod remote;
pub mod run;
pub mod settings;
pub mod source;
pub mod store;
mod sys;
#[cfg(test)]
mod testing;
pub mod util;
pub mod vt;

pub use source::{Live, LiveConfig};
pub use types::*;

/// Everything an interface can ask of Wake. `Live` (the real one) reads this machine; the app also has a
/// demo source for development.
///
/// Which calls may block (in `Live`; call those off the UI thread, as the app's `bg()` does):
/// - Cheap, answering from memory and small files: `overview`, `ideas`, `processes` (a /proc scan;
///   the windows come from the background thread's map).
/// - Disk: `session` (one summary file, plus re-reading any subagent transcript that changed, so
///   every refresh while a subagent runs), `subagent`, `search` (may read every session's summary
///   file once per query), `add_idea`, `set_idea`, `add_project`, `remove_project`.
/// - Git, up to 8 s per command: `project` (about five commands per repo, plus the log since the last
///   pause), `pause`.
/// - D-Bus and subprocesses: `go_to` (a /proc scan for the session's window, then the desktop's
///   request to bring it forward, or a new terminal window, 2 s per call); `stop` (`claude stop`
///   can take up to 20 s; signals are immediate).
///
/// Without the background threads (`Live::new` without `start_background`, as the command line
/// uses it), `overview` and `processes` also work out the windows themselves (a /proc scan).
pub trait Source: Send + Sync {
    fn overview(&self) -> Overview;
    fn session(&self, id: &str) -> Option<SessionDetail>;
    fn subagent(&self, session: &str, agent_id: &str) -> Option<SubagentDetail>;
    fn project(&self, id: &str) -> Option<ProjectDetail>;
    fn search(&self, query: &str) -> Vec<(SessionSummary, String)>;
    fn ideas(&self) -> Vec<Idea>;
    fn add_idea(&self, text: &str, project: Option<&str>) -> Result<Idea, String>;
    fn set_idea(&self, id: &str, state: Option<&str>, project: Option<Option<&str>>) -> Result<Idea, String>;
    fn pause(&self, project: &str, note: &str, next: &str) -> Result<Checkpoint, String>;
    fn add_project(&self, path: &str) -> Result<ProjectSummary, String>;
    fn remove_project(&self, id: &str, removed: bool) -> Result<ProjectSummary, String>;
    /// Go to a session: bring the terminal window showing it forward, or open a new terminal window
    /// running `claude attach` / the resume command. Blocks on D-Bus.
    fn go_to(&self, session: &str, dry_run: bool) -> ActionResult;
    /// Agent processes, sampled now (CPU needs two samples a second or more apart).
    fn processes(&self) -> ProcOverview;
    /// Stop one agent process. Refuses anything that isn't one of this user's agent processes, and,
    /// when `start_ticks` is given (always, from the UI), anything that isn't the very process that was
    /// shown and confirmed: a pid reused since then is refused ("That process has already ended;
    /// nothing was stopped."), never signalled. May block up to 20 s (`claude stop`).
    fn stop(&self, pid: u32, start_ticks: Option<u64>, how: StopHow, dry_run: bool) -> ActionResult;

    // ---- Hold-to-peek ----

    /// What a background session's terminal shows now (read-only): `claude logs <jobId>`. None for
    /// any other session (a terminal app's own screen can't be read). Blocks up to 2.5 s; repeated
    /// calls within 250 ms share one read.
    fn screen(&self, session: &str) -> Option<Screen>;
    /// How this desktop lets Wake show an agent's window (found once; cheap after that, but the
    /// first call asks D-Bus).
    fn look_kind(&self) -> LookKind {
        LookKind::Preview
    }
    /// Fly to the session's window and highlight it, keeping the keyboard on Wake. ok=false (with
    /// a sentence) means "show the preview instead". Blocks on D-Bus (2 s per call).
    fn look_start(&self, _session: &str, _dry_run: bool) -> ActionResult {
        ActionResult { ok: false, message: "This desktop can't fly to a window; showing a preview.".into(), steps: vec![] }
    }
    /// End the look: back to Wake, or (`stay`) over to the agent's window. Blocks on D-Bus.
    fn look_end(&self, _stay: bool, _dry_run: bool) -> ActionResult {
        ActionResult { ok: true, message: String::new(), steps: vec![] }
    }
    /// Some(stayed) once the desktop has ended a look by itself (read once). Cheap.
    fn look_ended(&self) -> Option<bool> {
        None
    }

    // ---- Questions ----

    /// Every question thread and what is being answered, or None when nothing changed since `since`
    /// (the `seq` of the last overview; 0 for the first call). Cheap: from memory (the first call
    /// reads the thread files).
    fn questions(&self, since: u64) -> Option<QOverview>;
    /// Ask a question: a follow-up in `thread`, or a new thread (in `project`, whose files Claude may
    /// read but never change; None for "just a question"). Queued behind any question being
    /// answered; returns the thread's id at once. Writes the thread file.
    fn ask(&self, thread: Option<&str>, question: &str, project: Option<&str>) -> Result<String, String>;
    /// Ask a thread's last question again after it failed or was stopped.
    fn retry_question(&self, _thread: &str) -> Result<(), String> {
        Err("Questions aren't available here.".into())
    }
    /// Stop the answer being written for `thread` (or drop its queued question). True if there was one.
    /// Signals only the `claude` process Wake started for it.
    fn cancel_question(&self, thread: &str) -> bool;
    /// Delete a thread (stopping its answer first). Claude's own transcript stays where it is.
    fn delete_thread(&self, thread: &str) -> Result<(), String>;
    /// The person has seen the thread's latest answer.
    fn mark_read(&self, thread: &str);

    // ---- Behind the remote ----

    /// Ask the remotes again now (`git ls-remote`, never a fetch): one project's repos, or every
    /// tracked project's. Returns at once; the answer shows in a later overview's `behind`.
    fn check_remotes(&self, _project: Option<&str>) {}
    /// The person has seen the "building while behind" alarm for this project: quiet until it
    /// catches up and falls behind again.
    fn ack_behind(&self, project: &str);
    /// `git pull --ff-only` in the project's behind repo: the one git write in Wake, only after the
    /// person confirms it, and refused unless the tree is clean and the branch can fast-forward.
    /// Blocks on the network (up to 2 min).
    fn pull(&self, project: &str, dry_run: bool) -> ActionResult;

    // ---- Settings ----

    /// What the person chose in Settings that the core acts on: which AI answers new questions, how
    /// often to ask remotes (never, with None), whether to send the "behind" notification. Cheap.
    fn set_options(&self, _o: &settings::CoreOptions) {}
    /// Each AI Questions could use, whether Wake can keep it read-only here, and a hint. Cheap
    /// (looks for the programs, never runs them).
    fn question_ais(&self) -> Vec<questions::AiChoice>;
    /// Set (or clear, with None) a project's dot colour, one of `store::COLOUR_NAMES`. Writes
    /// projects.json.
    fn set_project_color(&self, id: &str, color: Option<&str>) -> Result<ProjectSummary, String>;
    /// Set a project's next step without pausing it. Writes projects.json only.
    fn set_next(&self, id: &str, next: &str) -> Result<ProjectSummary, String>;
    /// Put a project in a group (None: no group). Writes projects.json.
    fn set_project_group(&self, id: &str, group: Option<&str>) -> Result<ProjectSummary, String>;
    /// Add a project group with nothing in it yet; a name in use is that group. Returns the name
    /// as kept. Writes projects.json.
    fn add_group(&self, name: &str) -> Result<String, String>;
    /// Put the project groups in this order (as the person dragged them). Writes projects.json.
    fn order_groups(&self, order: &[String]) -> Result<(), String>;
    /// Remove a project group; its projects stay tracked, in no group. Returns how many were in
    /// it. Writes projects.json.
    fn remove_group(&self, name: &str) -> Result<usize, String>;
    /// Set a project's importance: "high", "low", or None for normal. Writes projects.json.
    fn set_project_importance(&self, id: &str, importance: Option<&str>) -> Result<ProjectSummary, String>;

    // ---- Disk, resources, attribution ----
    // The numbers themselves come in `Overview::disk`, `Overview::resources` and `Overview::attribution`.

    /// Measure the disk again now: one project's folders, or (None) everything. Returns at once;
    /// the answer shows in a later overview's `disk` (its `measuring` says what is being measured).
    fn measure_disk(&self, _project: Option<&str>) {}
    /// Move one listed build-output or cache folder to the Trash (`DiskAction::Clear`). Only after
    /// the person confirms; refused while an agent in that project is working, while a process is
    /// inside it, and for anything Wake didn't list. Blocks briefly (a rename).
    fn clear_disk(&self, _path: &str, _dry_run: bool) -> ActionResult {
        ActionResult { ok: false, message: "Clearing isn't available here.".into(), steps: vec![] }
    }
    /// `git worktree remove` for one listed agent worktree whose branch is merged, whose tree is
    /// clean and that nobody is inside (`DiskAction::RemoveWorktree`). Only after the person
    /// confirms. Blocks on git (up to a minute).
    fn remove_worktree(&self, _path: &str, _dry_run: bool) -> ActionResult {
        ActionResult { ok: false, message: "Removing worktrees isn't available here.".into(), steps: vec![] }
    }
    /// The person says which project a session belongs to: Some(Some(id)), Some(None) for "no
    /// project", None to go back to Wake's own guess. Kept in Wake's store. Cheap.
    fn set_session_project(&self, session: &str, project: Option<Option<&str>>) -> Result<(), String>;

    // ---- Starting an agent on ideas ----

    /// The agents a brief can be sent to here: Claude, and Codex when it is installed. Cheap
    /// (looks for the programs, never runs them).
    fn agent_choices(&self) -> Vec<brief::Agent>;
    /// The exact line Wake would type for a brief with this id, to show before it is sent. Cheap.
    fn brief_command(&self, agent: brief::Agent, id: &str) -> Option<String>;
    /// Start an agent on ideas: write the brief to `<data>/briefs/<id>.md`, open a new window of the
    /// default terminal app in the project's folder running the agent on it, and mark the ideas as
    /// sent. Refuses ideas from another project.
    fn start_agent(&self, req: &brief::StartAgent, dry_run: bool) -> ActionResult;

    // ---- Wake's log ----

    /// Record an error the person was shown (an error toast) in Wake's log, `log::note_error`.
    /// `context` says where ("Shown in Projects"). Cheap (one appended line).
    fn note_error(&self, _context: &str, _message: &str) {}
}
