//! Subcommands: `wake , <idea>`, `wake ideas`, `wake index`, `wake doctor`, `wake ps`, `wake disk`, `wake behind`, `wake version`, `wake help`.
//! Returns None when there is no subcommand, so main opens the window. None of these start GTK.

use std::process::ExitCode;

/// println! that ends quietly when the reader has gone (`wake ps | head`).
macro_rules! out {
    ($($t:tt)*) => {{
        use std::io::Write as _;
        if writeln!(std::io::stdout(), $($t)*).is_err() {
            std::process::exit(0);
        }
    }};
}
use std::time::Instant;

use wake_core::procs;
use wake_core::store::{self, FileCheck, Store};
use wake_core::util;
use wake_core::{Live, LiveConfig, ProcKind, Source, State};

const VERSION: &str = env!("CARGO_PKG_VERSION");

const HELP: &str = "\
wake                 open Wake
wake --demo          open Wake on made-up data, to see what it shows
wake find            bring Wake forward with Find open, to find a session by name;
                     bind it to a key in your desktop's shortcut settings
wake , <idea...>     keep an idea; the project comes from the folder you're in
wake ideas           list the ideas waiting to be sorted
wake index           index your agent sessions once and say what was found
wake ps              agent processes: which PID is which task, memory, and which look stale
wake disk            what takes disk space, by project and for the machine, in plain words
                     (measures at low priority; read-only; may take a minute)
wake behind          tracked projects that are behind their remote and need a pull (asks the
                     remote with git ls-remote; never fetches; exits 1 when one is behind)
wake doctor          check what Wake can see on this computer, and what to install
wake version         which version of Wake this is
wake help            this text

In the window, press ? to see every key.
Wake reads ~/.claude and your git repos. It writes only to ~/.local/share/wake/
and its settings to ~/.config/wake/.
It never stops a process from the command line; use the Agents view for that.";

pub fn run(args: &[String]) -> Option<ExitCode> {
    let (cmd, rest) = args.split_first()?;
    let code = match cmd.as_str() {
        "," | "note" => cmd_idea(rest),
        c if c.starts_with(',') => {
            let mut words = vec![c[1..].to_string()];
            words.extend(rest.iter().cloned());
            cmd_idea(&words)
        }
        "ideas" => cmd_ideas(),
        "index" => cmd_index(),
        "doctor" => cmd_doctor(),
        "ps" => cmd_ps(),
        "disk" => cmd_disk(),
        "behind" => cmd_behind(),
        "version" | "--version" | "-V" => {
            out!("wake {VERSION}");
            0
        }
        // Opens the window, so it belongs to the app.
        "find" => return None,
        "help" | "-h" | "--help" => {
            out!("{HELP}");
            0
        }
        // Options for the window (e.g. --screenshot, --demo) belong to the app.
        c if c.starts_with("--") => return None,
        other => {
            out!("Unknown command '{other}'.\n\n{HELP}");
            2
        }
    };
    Some(ExitCode::from(code))
}

fn store() -> Store {
    Store::new(store::Paths::from_env())
}

fn cmd_idea(words: &[String]) -> u8 {
    let text = words.join(" ");
    let text = text.trim();
    if text.is_empty() {
        out!("Nothing to keep. Usage: wake , <your idea>");
        return 2;
    }
    let s = store();
    let cwd = std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).ok();
    match s.add_idea(text, None, "shell", cwd.as_deref()) {
        Ok(idea) => {
            let projects = s.load_projects();
            let name = idea.project.as_deref().and_then(|p| store::project_by_id(p, &projects)).map(|p| p.name.clone());
            out!("kept · {}", plain(name.as_deref().unwrap_or("Unsorted")));
            0
        }
        Err(e) => {
            out!("Could not keep that idea: {}", plain(&e.to_string()));
            1
        }
    }
}

fn cmd_ideas() -> u8 {
    let s = store();
    let projects = s.load_projects();
    let ideas: Vec<_> = s.list_ideas().into_iter().filter(|i| i.state == wake_core::IdeaState::Inbox).collect();
    if ideas.is_empty() {
        out!("No ideas waiting.");
        return 0;
    }
    for i in ideas.iter().take(40) {
        let name = i.project.as_deref().and_then(|p| store::project_by_id(p, &projects)).map_or("Unsorted", |p| p.name.as_str());
        out!("{:<18} {}", util::take_chars(&plain(name), 18), plain(&i.text));
    }
    0
}

/// Peak and current memory of this process, from /proc/self/status.
fn memory() -> (u64, u64) {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let kb = |key: &str| s.lines().find_map(|l| l.strip_prefix(key)).and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok()).unwrap_or(0);
    (kb("VmHWM:") / 1024, kb("VmRSS:") / 1024)
}

fn cmd_index() -> u8 {
    let live = Live::new(LiveConfig::real());
    let t0 = Instant::now();
    let changed = live.index_now();
    let dt = t0.elapsed().as_secs_f64();
    let (peak, _) = memory();
    let projects = live.store().load_projects();
    let mut counts: Vec<(String, u32)> = Vec::new();
    for acc in live.indexer().all_lite() {
        let name = wake_core::index::attribute(&acc, &projects, &live.paths().home).1.unwrap_or_else(|| "home".into());
        match counts.iter_mut().find(|(n, _)| *n == name) {
            Some((_, c)) => *c += 1,
            None => counts.push((name, 1)),
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let read = if changed == 1 { "1 new or changed transcript read".to_string() } else { format!("{changed} new or changed transcripts read") };
    out!("Indexed {} sessions in {dt:.1} s ({read}); peak memory {peak} MB.", counts.iter().map(|c| c.1).sum::<u32>());
    // What it couldn't read, understand or save, in words.
    match live.indexer().stats().sentence() {
        Some(s) => out!("{}", plain(s.trim_end_matches(" Run `wake doctor`."))),
        None => out!("Every transcript was read and understood."),
    }
    for (name, n) in counts.iter().take(12) {
        out!("  {:<22} {n}", util::take_chars(&plain(name), 22));
    }
    0
}

/// `wake doctor`: what Wake can see on this computer, and what to install for anything missing.
/// Read-only: it never repairs, moves or creates a file (opening Wake does that, and says so).
/// Exits 1 only when something Wake needs is missing (a `!!` line).
fn cmd_doctor() -> u8 {
    let paths = store::Paths::from_env();
    let mut d = Doctor::default();
    out!("Wake {VERSION}: what it can see on this computer");
    out!("ok = working · -- = optional, not set up here · !! = needed and missing");
    doctor_sessions(&mut d, &paths);
    doctor_programs(&mut d, &paths);
    doctor_desktop(&mut d, &paths);
    doctor_files(&mut d, &paths);
    doctor_log(&mut d, &paths);
    out!();
    out!("Wake writes only to {} and its settings to {}.", tilde(&paths.data), tilde(paths.config.join("settings.toml")));
    out!("If Wake shows the wrong thing, include this output and {} in your report.", tilde(paths.data.join("wake.log")));
    u8::from(d.needed)
}

#[derive(Default)]
struct Doctor {
    needed: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Mark {
    Ok,
    /// Optional and missing: the line says what you would gain.
    Optional,
    /// Needed and missing: makes doctor exit 1.
    Needed,
}

impl Doctor {
    fn heading(&self, h: &str) {
        out!();
        out!("{h}");
    }

    fn ok(&mut self, text: &str) {
        self.line(Mark::Ok, text, "");
    }

    /// Every line that isn't ok says what's missing, what the person loses, and how to fix it.
    fn line(&mut self, mark: Mark, text: &str, fix: &str) {
        let m = match mark {
            Mark::Ok => "ok",
            Mark::Optional => "--",
            Mark::Needed => "!!",
        };
        self.needed |= mark == Mark::Needed;
        out!("  {m}   {}", plain(text));
        if mark != Mark::Ok && !fix.is_empty() {
            out!("       Fix: {}", plain(fix));
        }
    }
}

fn install(fedora: &str, debian: &str) -> String {
    format!("Fedora: sudo dnf install {fedora} · Debian, Ubuntu: sudo apt install {debian}")
}

fn on_path(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()))
}

/// "1 session", "3 sessions": `word` with an s for any number but one.
fn plural(n: usize, word: &str) -> String {
    crate::fmt::pl(n as u64, word, &format!("{word}s"))
}

fn doctor_sessions(d: &mut Doctor, paths: &store::Paths) {
    d.heading("Sessions");
    let found = wake_core::index::Indexer::new(paths.clone()).discover();
    let claude = found.iter().filter(|f| f.1 == wake_core::Agent::Claude).count();
    let codex = found.len() - claude;
    let (claude_dir, codex_dir) = (tilde(paths.claude.join("projects")), tilde(paths.codex.join("sessions")));
    if found.is_empty() {
        d.line(
            Mark::Needed,
            &format!("No agent sessions found in {claude_dir} or {codex_dir}, so Wake has nothing to show yet."),
            "run `claude` (or `codex`) in a project folder once. Wake lists it from then on.",
        );
    }
    if claude > 0 {
        d.ok(&format!("Claude Code: {} saved in {claude_dir}", plural(claude, "session")));
        doctor_open_now(d, paths);
    }
    if codex > 0 {
        d.ok(&format!("Codex: {} saved in {codex_dir}", plural(codex, "session")));
    }
    doctor_index(d, paths, &found);
}

/// Open Claude sessions, and whether Wake can tell their processes are agents. Inside a toolbox
/// or distrobox it often can't, and then Agents quietly shows fewer than are running.
fn doctor_open_now(d: &mut Doctor, paths: &store::Paths) {
    let dir = paths.claude.join("sessions");
    if !dir.is_dir() {
        d.line(
            Mark::Optional,
            &format!("Wake can't tell which Claude Code sessions are open: {} isn't there yet.", tilde(&dir)),
            "start `claude` in a project; the folder appears while it runs. If it still doesn't, update Claude Code.",
        );
        return;
    }
    let proc = std::path::Path::new("/proc");
    let pids: Vec<u32> = wake_core::live::read_live(paths, proc).into_values().filter(|l| l.alive).map(|l| l.pid).collect();
    let seen = pids.iter().filter(|&&pid| procs::read_proc(proc, pid).is_some_and(|p| procs::agent_of(&p.argv, p.exe.as_deref()).is_some())).count();
    if pids.is_empty() {
        d.ok("Open now: no Claude Code session");
    } else if seen == pids.len() {
        d.ok(&format!("Open now: {}, and Wake can see all of their processes", plural(pids.len(), "Claude Code session")));
    } else {
        d.line(
            Mark::Needed,
            &format!("Wake can see {seen} of {} agent processes, so Agents shows fewer than are running and leaves the rest out of its memory totals.", pids.len()),
            "this usually means Wake is running inside a toolbox or distrobox. Run Wake on the host instead.",
        );
    }
}

fn doctor_index(d: &mut Doctor, paths: &store::Paths, found: &[(String, wake_core::Agent)]) {
    let dir = paths.index_dir();
    let Some(read) = store::read_json::<std::collections::HashMap<String, serde_json::Value>>(&dir.join("files.json")) else {
        d.ok("Index: not made yet. Wake makes it the first time it opens (or run `wake index`).");
        return;
    };
    let kept = std::fs::read_dir(dir.join("sessions")).map_or(0, |rd| rd.flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".json")).count());
    let new = found.iter().filter(|(p, _)| !read.contains_key(p)).count();
    let mut text = format!("Index: {} kept", plural(kept, "session"));
    if new > 0 {
        text.push_str(&format!("; {} not read yet (Wake reads them when it opens)", plural(new, "new transcript")));
    }
    d.ok(&text);
}

fn doctor_programs(d: &mut Doctor, paths: &store::Paths) {
    use wake_core::Agent;
    use wake_core::questions::find_tool;
    d.heading("Programs");
    if on_path("git") {
        d.ok("git");
    } else {
        d.line(Mark::Needed, "git isn't installed. Wake reads each project's state with it.", &install("git", "git"));
    }
    let tools: Vec<(Agent, Option<std::path::PathBuf>)> = Agent::ALL.iter().map(|&ai| (ai, find_tool(paths, ai))).collect();
    let other_agent = tools.iter().any(|(ai, p)| *ai != Agent::Claude && p.is_some());
    for (ai, found) in &tools {
        match (ai, found) {
            (_, Some(p)) => d.ok(&format!("{}: {}", ai.id(), tilde(p))),
            (Agent::Claude, None) => d.line(
                if other_agent { Mark::Optional } else { Mark::Needed },
                "claude: Wake can't find it (it looked on your PATH and in ~/.local/bin). Questions and Start an agent need it.",
                "install Claude Code, or set WAKE_CLAUDE to where claude is.",
            ),
            (Agent::Codex, None) => {
                d.line(Mark::Optional, "codex isn't installed. With it, Questions can also ask Codex.", "only if you want it, install the Codex CLI, or set WAKE_CODEX to where it is.")
            }
            (Agent::Gemini, None) => {
                d.line(Mark::Optional, "gemini isn't installed. With it, Questions can also ask Gemini.", "only if you want it, install Gemini CLI, or set WAKE_GEMINI to where it is.")
            }
        }
    }
}

fn doctor_desktop(d: &mut Doctor, paths: &store::Paths) {
    use wake_core::{desktop, run};
    d.heading("Desktop");
    // New windows (Go to for a session in none, Start an agent) open in the default terminal app.
    match desktop::terminal_launcher() {
        Some(l) => d.ok(&format!("default terminal app: opened through {}", tilde(std::path::Path::new(&l[0])))),
        None => d.line(
            Mark::Optional,
            "Wake can't find your default terminal app, so Go to and Start an agent can only give you a command to copy.",
            "install xdg-terminal-exec, or set TERMINAL to your terminal app.",
        ),
    }
    // KDE Plasma's Peek talks to KWin through Qt 6's qdbus (qdbus-qt6, or qdbus6).
    let kde = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().split(':').any(|x| x.eq_ignore_ascii_case("KDE"));
    if kde && !on_path(run::qdbus_program()) {
        d.line(Mark::Optional, "qdbus-qt6 isn't installed, so Peek on KDE Plasma shows a preview instead of going to the window.", &install("qt6-qttools", "qdbus-qt6"));
    }
    if on_path(run::GDBUS) {
        d.ok("gdbus");
    } else {
        d.line(Mark::Optional, "gdbus isn't installed. Peek on Slipstream and KDE Plasma, and the notice when a project is behind, need it.", &install("glib2", "libglib2.0-bin"));
    }
    let kind = wake_core::look::detect(&run::default_runner(), &std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default());
    match kind {
        wake_core::LookKind::Slipstream => d.ok("Peek: Slipstream flies to the session's window"),
        wake_core::LookKind::Kwin => d.ok("Peek: KDE Plasma switches to the session's window"),
        wake_core::LookKind::Preview => d.ok("Peek: Wake shows the session's terminal text (no Slipstream or KDE Plasma found)"),
    }
    if kind != wake_core::LookKind::Slipstream {
        d.line(
            Mark::Optional,
            "Slipstream isn't running. On Slipstream, peek and Go to fly to the agent's window, and Wake adds a card to its plugin panel.",
            "only if you want it, see https://github.com/peterwalker78/slipstream-desktop",
        );
    }
    let notes = paths.runtime.join("working");
    if notes.is_dir() {
        d.ok(&format!("agent-notify: its notes are in {}", tilde(&notes)));
    } else {
        d.line(Mark::Optional, "agent-notify isn't running. With it, Agents draws each agent's activity trace.", "only if you want it, see https://github.com/peterwalker78/agent-notify");
    }
}

fn doctor_files(d: &mut Doctor, paths: &store::Paths) {
    d.heading("Wake's own files");
    doctor_projects(d, paths);
    doctor_settings(d, paths);
    doctor_ideas(d, paths);
    doctor_broken(d, paths);
    doctor_self(d, paths);
}

/// The tracked projects, read straight from projects.json. `Store::load_projects` is not used:
/// it moves a broken file aside and writes a fresh one; `store::check_projects` only reads.
fn doctor_projects(d: &mut Doctor, paths: &store::Paths) {
    let path = paths.data.join("projects.json");
    let file = tilde(&path);
    match store::check_projects(paths) {
        FileCheck::Missing => {
            d.ok(&format!("Project list: not made yet. Wake makes it from {} the first time it opens.", tilde(&paths.projects)));
            return;
        }
        FileCheck::Unreadable(why) => {
            d.line(Mark::Needed, &format!("Wake can't read its project list ({file}: {why})."), &format!("check who owns it: ls -l {file}"));
            return;
        }
        FileCheck::Damaged(why) => {
            d.line(
                Mark::Needed,
                &format!("Wake can't understand its project list ({file}: {why}). When it next opens, it keeps that file as projects.json.broken-<time> and starts a fresh list."),
                "open Wake, then copy any descriptions or next steps you need from the kept file.",
            );
            return;
        }
        FileCheck::Fine(_) => {}
    }
    let projects: Vec<store::ProjectRec> =
        std::fs::read(&path).ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok()).and_then(|v| serde_json::from_value(v.get("projects")?.clone()).ok()).unwrap_or_default();
    let names: Vec<&str> = projects.iter().filter(|p| !p.removed).map(|p| p.name.as_str()).collect();
    if names.is_empty() {
        d.line(Mark::Optional, &format!("No projects yet: Wake looks in {} and the folders your sessions ran in.", tilde(&paths.projects)), "open Projects → Track a folder for others.");
        return;
    }
    let mut text = format!("{} tracked: {}", plural(names.len(), "project"), names.iter().take(8).copied().collect::<Vec<_>>().join(", "));
    if names.len() > 8 {
        text.push_str(&format!(" … and {} more", names.len() - 8));
    }
    d.ok(&text);
}

/// settings.toml, if Wake can't read or understand it (`settings::check` only reads;
/// `settings::load` would move a broken file aside).
fn doctor_settings(d: &mut Doctor, paths: &store::Paths) {
    let file = tilde(paths.config.join("settings.toml"));
    let why = match wake_core::settings::check(&paths.config) {
        FileCheck::Unreadable(why) => format!("can't read {file} ({why})"),
        FileCheck::Damaged(why) => format!("can't understand {file} ({why})"),
        FileCheck::Missing | FileCheck::Fine(_) => return,
    };
    d.line(Mark::Optional, &format!("Wake {why}, so it uses its default settings."), &format!("correct or delete {file}."));
}

/// Lines of ideas.jsonl that Ideas can't show (`list_ideas` skips them quietly).
fn doctor_ideas(d: &mut Doctor, paths: &store::Paths) {
    let n = store::unreadable_idea_lines(paths);
    if n > 0 {
        let (what, verb) = if n == 1 { ("1 saved idea".to_string(), "it") } else { (format!("{n} saved ideas"), "them") };
        d.line(
            Mark::Optional,
            &format!("{what} can't be read, so Ideas doesn't show {verb}."),
            &format!("{} still in {}.", if n == 1 { "it is" } else { "they are" }, tilde(paths.data.join("ideas.jsonl"))),
        );
    }
}

/// Files Wake kept aside because it couldn't understand them (`*.broken-*`).
fn doctor_broken(d: &mut Doctor, paths: &store::Paths) {
    for f in store::broken_files(paths) {
        d.line(Mark::Optional, &format!("Wake kept a file it couldn't understand: {}", tilde(&f)), "copy what you need from it, then delete it.");
    }
}

/// Wake's own memory (the window, when it is open) and its index on disk.
fn doctor_self(d: &mut Doctor, paths: &store::Paths) {
    let index = match dir_size(&paths.index_dir()) {
        0 => "no index yet".to_string(),
        n => format!("index {} on disk", util::bytes(n)),
    };
    let me = std::process::id();
    let window = procs::Explorer::new(std::path::Path::new("/proc")).scan().into_iter().filter(|p| p.comm == "wake" && p.pid != me).map(|p| p.rss_bytes).max();
    match window {
        Some(rss) => d.ok(&format!("Wake itself: {} of memory, {index}", util::bytes(rss))),
        None => d.ok(&format!("Wake itself: not open now; {index}")),
    }
}

/// Space a folder takes on disk (allocated blocks, as `du` counts them).
fn dir_size(dir: &std::path::Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    rd.flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map_or(0, |m| std::os::unix::fs::MetadataExt::blocks(&m) * 512),
            _ => 0,
        })
        .sum()
}

/// The last few things Wake did, and what went wrong, from its log.
fn doctor_log(d: &mut Doctor, paths: &store::Paths) {
    let lines = wake_core::log::recent(paths, 5);
    if lines.is_empty() {
        return;
    }
    d.heading("What Wake did lately");
    for l in lines {
        out!("  {}", plain(&l));
    }
}

/// One line saying what each kind and state in the table means, for only those that appear.
fn ps_legend(seen: &[&str]) -> Option<String> {
    const WORDS: [(&str, &str); 7] = [
        ("daemon", "Claude's background service that keeps background sessions running"),
        ("session", "an agent working on one task"),
        ("terminal", "the terminal a background session runs in or is shown in"),
        ("spare", "started ahead of time, not used yet"),
        ("stray", "an agent process Wake can't tie to a session"),
        ("subagent", "a helper the session above it started"),
        ("lost track", "Wake can't tell what it is doing"),
    ];
    let parts: Vec<String> = WORDS.iter().filter(|(w, _)| seen.contains(w)).map(|(w, m)| format!("{w} = {m}")).collect();
    if parts.is_empty() { None } else { Some(parts.join(" · ")) }
}

fn idle(s: Option<u64>) -> String {
    match s {
        None => "-".into(),
        Some(s) if s < 60 => "now".into(),
        Some(s) => util::span(s),
    }
}

/// `wake disk`: measure once and print each project's items and the machine's (added 2026-11-20).
fn cmd_disk() -> u8 {
    use wake_core::util::bytes;
    let live = Live::new(LiveConfig::real());
    live.index_now();
    let started = Instant::now();
    let Some(r) = live.measure_disk_now() else {
        out!("Couldn't measure.");
        return 1;
    };
    let ov = live.overview();
    let name = |id: &str| ov.projects.iter().find(|p| p.id == id).map_or(id.to_string(), |p| p.name.clone());
    if let Some(m) = &r.machine {
        out!("{} free of {} on {} · {} could be rebuilt (measured in {:.0} s)", bytes(m.home_free), bytes(m.home_total), m.home_mount, bytes(r.rebuildable), started.elapsed().as_secs_f64());
        if let Some(n) = &m.root_note {
            out!("{n}");
        }
    }
    let safety = |s: wake_core::DiskSafety| match s {
        wake_core::DiskSafety::Safe => "safe",
        wake_core::DiskSafety::Check => "look first",
        wake_core::DiskSafety::Keep => "keep",
    };
    let mut seen: Vec<wake_core::DiskSafety> = Vec::new();
    let mut ps: Vec<&wake_core::ProjectDisk> = r.projects.iter().filter(|p| !p.items.is_empty()).collect();
    ps.sort_by_key(|p| std::cmp::Reverse(p.bytes));
    for p in ps {
        out!();
        out!("{}  {}{} · {} rebuildable", plain(&name(&p.project)), if p.partial { ">= " } else { "" }, bytes(p.bytes), bytes(p.rebuildable));
        for i in &p.items {
            let pad = if i.within.is_some() { "    " } else { "  " };
            seen.push(i.safety);
            out!("{pad}{:>7}  {:<11} {}  {}{}", bytes(i.bytes), safety(i.safety), plain(&i.name), tilde(&i.path), i.refused.as_deref().map(|r| format!("  [{}]", plain(r))).unwrap_or_default());
        }
    }
    if let Some(m) = r.machine.as_ref().filter(|m| !m.items.is_empty()) {
        out!();
        out!("Outside projects");
        for i in &m.items {
            seen.push(i.safety);
            out!("  {:>7}  {:<11} {}  {}{}", bytes(i.bytes), safety(i.safety), plain(&i.name), tilde(&i.path), i.command.as_deref().map(|c| format!("  ({})", plain(c))).unwrap_or_default());
        }
    }
    let legend: Vec<&str> = [
        (wake_core::DiskSafety::Safe, "safe = comes back by itself (build output, caches)"),
        (wake_core::DiskSafety::Check, "look first = check before clearing"),
        (wake_core::DiskSafety::Keep, "keep = may be the only copy; leave it unless you know"),
    ]
    .into_iter()
    .filter(|(s, _)| seen.contains(s))
    .map(|(_, w)| w)
    .collect();
    out!();
    if !legend.is_empty() {
        out!("{}", legend.join(" · "));
    }
    out!("To clear something, open Wake → Projects → Disk details. It moves things to the Trash.");
    0
}

fn cmd_ps() -> u8 {
    let live = Live::new(LiveConfig::real());
    live.index_now(); // titles and projects; incremental, so quick after the first run
    let _ = live.processes(); // first CPU sample
    std::thread::sleep(std::time::Duration::from_millis(1000));
    let o = live.processes();
    out!("{}", procs::headline(&o));
    out!();
    let h = ["PID", "KIND", "MEM", "CPU", "STATE", "IDLE", "WHERE", "TASK", "PROJECT"];
    out!("{:<9} {:<8} {:>6} {:>5}  {:<10} {:<8} {:<16} {:<34} {}", h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7], h[8]);
    let mut seen: Vec<&str> = Vec::new();
    for p in &o.procs {
        // pty hosts and attach clients fold in silently; a subagent folds in but keeps its task
        let sub = p.kind == ProcKind::Subagent;
        let folded = !sub && o.procs.iter().any(|q| q.children.contains(&p.pid));
        let kind = match p.kind {
            ProcKind::Session => "session",
            ProcKind::Daemon => "daemon",
            ProcKind::PtyHost => "terminal",
            ProcKind::Spare => "spare",
            ProcKind::Stray => "stray",
            ProcKind::Subagent => "subagent",
        };
        let state = match p.state {
            State::Working => "working",
            State::Waiting => "waiting",
            State::Idle => "idle",
            State::Ended => "ended",
            State::Unknown => "lost track",
        };
        let where_ = match &p.location {
            Some(l) => l.app.clone().unwrap_or_else(|| "window".into()),
            None if p.job_id.is_some() => "background".into(),
            None => "-".into(),
        };
        let pid = if folded || sub { format!(" └{}", p.pid) } else { p.pid.to_string() };
        // a stray has no task: show what it is running instead, so it can be judged one by one
        let task = if folded {
            String::new()
        } else if p.kind == ProcKind::Stray {
            p.command.clone()
        } else if sub {
            p.title.as_ref().map_or_else(|| p.command.clone(), |t| format!("{t} ({})", p.command))
        } else {
            p.title.clone().unwrap_or_else(|| p.command.clone())
        };
        for word in [kind, if folded { "" } else { state }] {
            if !seen.contains(&word) {
                seen.push(word);
            }
        }
        let cpu = p.cpu_percent.map_or_else(|| "-".into(), |c| format!("{c:.0}%"));
        out!(
            "{:<9} {:<8} {:>6} {:>5}  {:<10} {:<8} {:<16} {:<34} {}",
            pid,
            kind,
            util::bytes(p.rss_bytes),
            cpu,
            if folded { "" } else { state },
            if folded { String::new() } else { idle(p.idle_seconds) },
            if folded { String::new() } else { plain(&where_) },
            util::take_chars(&plain(&task), 34),
            plain(if folded { "" } else { p.project_name.as_deref().unwrap_or("") })
        );
        if let Some(h) = p.helpers.as_ref().filter(|h| !h.is_empty()) {
            out!("{:<9} subagents running: {}", "", plain(&h.join(" · ")));
        }
        if !p.tasks.is_empty() {
            let tasks: Vec<String> = p.tasks.iter().map(|t| format!("{} ({})", t.label, crate::why::task_kind(&t.kind))).collect();
            out!("{:<9} tasks running: {}", "", plain(&tasks.join(" · ")));
        }
        if let Some(r) = &p.stale_reason {
            out!("{:<9} stale: {}", "", plain(r));
        } else if p.kind == ProcKind::Stray
            && let Some(t) = &p.title
        {
            out!("{:<9} stray: {}", "", plain(t));
        }
    }
    if let Some(legend) = ps_legend(&seen) {
        out!();
        out!("{legend}");
    }
    if !o.dead_notes.is_empty() {
        let pids: Vec<String> = o.dead_notes.iter().map(|p| p.to_string()).collect();
        out!("\nNotes left by processes that are gone: {} (Slipstream clears these itself).", pids.join(", "));
    }
    // By project (added 2026-11-20): each session with its terminal, attach clients and helpers.
    if let Some(r) = live.overview().resources {
        let names: std::collections::HashMap<String, String> = live.overview().projects.into_iter().map(|p| (p.id, p.name)).collect();
        let parts: Vec<String> = r
            .projects
            .iter()
            .map(|p| {
                let n = p.project.as_ref().map_or_else(|| "no project".to_string(), |id| names.get(id).cloned().unwrap_or_else(|| id.clone()));
                format!("{} {} ({} session{})", plain(&n), util::bytes(p.rss_bytes), p.sessions, if p.sessions == 1 { "" } else { "s" })
            })
            .collect();
        if !parts.is_empty() {
            out!("\nBy project: {}", parts.join(" · "));
        }
    }
    0
}

/// `wake behind`: ask each tracked repo's remote (ls-remote, no fetch, no pull) and list what is
/// behind. Read-only.
fn cmd_behind() -> u8 {
    use wake_core::gitinfo;
    use wake_core::remote::{self, Outcome, RepoCheck};
    let s = store();
    let projects: Vec<_> = s.load_projects().into_iter().filter(|p| !p.removed).collect();
    let (mut behind, mut repos_n, mut quiet) = (0, 0, Vec::new());
    for p in &projects {
        let (repos, _) = gitinfo::project_git(p, true);
        let checks: Vec<RepoCheck> = repos.iter().map(|r| remote::check_repo(&r.state.path)).collect();
        repos_n += checks.len();
        for c in &checks {
            if let Outcome::CouldNotCheck(why) = &c.outcome {
                quiet.push(format!("{} ({})", wake_core::util::take_chars(&tilde(&c.path), 60), plain(why)));
            }
        }
        let refs: Vec<&RepoCheck> = checks.iter().collect();
        let dirty = |path: &str| repos.iter().find(|r| r.state.path == path).map_or(0, |r| r.state.dirty_files + r.state.untracked);
        if let Some(b) = remote::behind_for(&p.id, &p.name, &p.roots, &refs, &dirty) {
            behind += 1;
            out!("{:<18} {}", util::take_chars(&plain(&p.name), 18), plain(&b.line));
            let mut more = format!("in {}", tilde(&b.repo));
            if b.others > 0 {
                more.push_str(&format!(" ({} more {} behind)", b.others, if b.others == 1 { "repo" } else { "repos" }));
            }
            out!("{:<18} {more}", "");
        }
    }
    if behind == 0 {
        out!("Nothing is behind ({repos_n} repos in {} projects checked).", projects.len());
    }
    if !quiet.is_empty() {
        out!("\nCouldn't check {}: {}", quiet.len(), quiet.join(", "));
    }
    if behind > 0 { 1 } else { 0 }
}

/// A path with home as `~`, safe to print.
fn tilde(p: impl AsRef<std::path::Path>) -> String {
    plain(&crate::fmt::tilde(p))
}

/// Names, titles, paths and commands come from files and processes Wake doesn't control: printed
/// without control characters, so none can move the cursor, recolour the terminal or start a line.
fn plain(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printed_names_lose_their_control_characters() {
        assert_eq!(plain("fix \u{1b}[2Jthe\nlogin\u{7}"), "fix [2Jthelogin");
        assert_eq!(plain("café · 日本"), "café · 日本");
    }
}
