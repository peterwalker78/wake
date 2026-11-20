//! Subcommands: `wake , <idea>`, `wake ideas`, `wake index`, `wake doctor`, `wake ps`, `wake disk`, `wake behind`, `wake help`.
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
use wake_core::store::{self, Store};
use wake_core::util;
use wake_core::{Live, LiveConfig, ProcKind, Source, State};

const HELP: &str = "\
wake                 open Wake
wake , <idea...>     keep an idea; the project comes from the folder you're in
wake ideas           list the ideas waiting to be sorted
wake index           index your agent sessions once and say what was found
wake ps              agent processes: which PID is which task, memory, and which look stale
wake disk            what takes disk space, by project and for the machine, in plain words
                     (measures at low priority; read-only; may take a minute)
wake behind          tracked projects that are behind their remote and need a pull (asks the
                     remote with git ls-remote; never fetches; exits 1 when one is behind)
wake doctor          check what Wake can see on this machine
wake help            this text

Wake reads ~/.claude and your git repos. It writes only to ~/.local/share/wake/.
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
            out!("kept · {}", name.as_deref().unwrap_or("Unsorted"));
            0
        }
        Err(e) => {
            out!("Could not keep that idea: {e}");
            1
        }
    }
}

fn cmd_ideas() -> u8 {
    let s = store();
    let projects = s.load_projects();
    let ideas: Vec<_> = s.list_ideas().into_iter().filter(|i| i.state == "inbox").collect();
    if ideas.is_empty() {
        out!("No ideas waiting.");
        return 0;
    }
    for i in ideas.iter().take(40) {
        let name = i.project.as_deref().and_then(|p| store::project_by_id(p, &projects)).map(|p| p.name.as_str()).unwrap_or("Unsorted");
        out!("{:<18} {}", util::take_chars(name, 18), i.text);
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
    let (_, total) = live.indexer().progress();
    out!("Indexed {} sessions ({changed} of {total} files read) in {dt:.1} s; peak memory {peak} MB.", counts.iter().map(|c| c.1).sum::<u32>());
    for (name, n) in counts.iter().take(12) {
        out!("  {:<22} {n}", util::take_chars(name, 22));
    }
    0
}

fn cmd_doctor() -> u8 {
    let mut ok = true;
    let mut line = |good: bool, text: &str| {
        ok &= good;
        out!("{}{}", if good { "  ok   " } else { "  !!   " }, text);
    };
    let paths = store::Paths::from_env();
    let claude = paths.claude.display().to_string();
    line(paths.claude.join("projects").is_dir(), &format!("Claude transcripts in {claude}/projects"));
    line(paths.claude.join("sessions").is_dir(), &format!("Claude live sessions in {claude}/sessions"));
    let which = |n: &str| std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(n).is_file()));
    line(which("git"), "git is installed");
    line(which("qdbus-qt6") || which("qdbus"), "qdbus is installed (needed to switch Konsole tabs)");
    let live = Live::new(LiveConfig::real());
    let tabs = live.desktop().konsole_map(true);
    let problem = live.desktop().problem();
    line(problem.is_none(), &problem.unwrap_or_else(|| format!("Konsole can be reached over D-Bus ({} tabs)", tabs.len())));
    line(paths.runtime.join("working").is_dir(), &format!("agent-notify notes in {}", paths.runtime.join("working").display()));
    let projects: Vec<_> = live.store().load_projects().into_iter().filter(|p| !p.removed).collect();
    if let Some(problem) = live.store().projects_problem() {
        line(false, &problem);
    }
    let names: Vec<&str> = projects.iter().take(8).map(|p| p.name.as_str()).collect();
    line(!projects.is_empty(), &format!("{} projects tracked: {}", projects.len(), names.join(", ")));
    out!("\nWake writes only to {}", paths.data.display());
    if ok { 0 } else { 1 }
}

fn mem(b: u64) -> String {
    if b >= 1 << 30 { format!("{:.1}G", b as f64 / (1u64 << 30) as f64) } else { format!("{}M", b >> 20) }
}

fn idle(s: Option<u64>) -> String {
    match s {
        None => "-".into(),
        Some(s) if s < 60 => "now".into(),
        Some(s) => util::ago(Some(s as f64)),
    }
}

/// `wake disk`: measure once and print each project's items and the machine's (added 2026-11-20).
fn cmd_disk() -> u8 {
    use wake_core::disk::human;
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
        out!("{} free of {} on {} · {} could be rebuilt (measured in {:.0} s)", human(m.home_free), human(m.home_total), m.home_mount, human(r.rebuildable), started.elapsed().as_secs_f64());
        if let Some(n) = &m.root_note {
            out!("{n}");
        }
    }
    let safety = |s: wake_core::DiskSafety| match s {
        wake_core::DiskSafety::Safe => "safe",
        wake_core::DiskSafety::Check => "look first",
        wake_core::DiskSafety::Keep => "keep",
    };
    let mut ps: Vec<&wake_core::ProjectDisk> = r.projects.iter().collect();
    ps.sort_by_key(|p| std::cmp::Reverse(p.bytes));
    for p in ps {
        out!();
        out!("{}  {}{} · {} rebuildable", name(&p.project), if p.partial { ">= " } else { "" }, human(p.bytes), human(p.rebuildable));
        for i in &p.items {
            let pad = if i.within.is_some() { "    " } else { "  " };
            out!("{pad}{:>7}  {:<11} {}{}", human(i.bytes), safety(i.safety), i.name, i.refused.as_deref().map(|r| format!("  [{r}]")).unwrap_or_default());
        }
    }
    if let Some(m) = &r.machine {
        out!();
        out!("Outside projects");
        for i in &m.items {
            out!("  {:>7}  {:<11} {}{}", human(i.bytes), safety(i.safety), i.name, i.command.as_deref().map(|c| format!("  ({c})")).unwrap_or_default());
        }
    }
    let unattributed = ov.attribution.as_ref().map_or(0, |a| a.iter().filter(|x| x.how == "none" && x.started_at_home).count());
    let guessed: Vec<String> = ov.attribution.iter().flatten().filter(|x| x.guessed()).map(|x| x.words.clone()).collect();
    out!();
    out!("Sessions guessed into a project: {} · still \"no project (~)\": {}", guessed.len(), unattributed);
    let titles: Vec<&str> =
        ov.attribution.iter().flatten().filter(|x| x.how == "none" && x.started_at_home).filter_map(|x| ov.sessions.iter().find(|s| s.id == x.session)).map(|s| s.title.as_str()).take(8).collect();
    if !titles.is_empty() {
        out!("  {}", titles.join(" · "));
    }
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
    out!("{:<9} {:<8} {:>6} {:>5}  {:<8} {:<8} {:<16} {:<34} {}", h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7], h[8]);
    for p in &o.procs {
        // pty hosts and attach clients fold in silently; a subagent folds in but keeps its task
        let sub = p.kind == ProcKind::Subagent;
        let folded = !sub && o.procs.iter().any(|q| q.children.contains(&p.pid));
        let kind = match p.kind {
            ProcKind::Session => "session",
            ProcKind::Daemon => "daemon",
            ProcKind::PtyHost => {
                if p.command.contains(" attach ") {
                    "attach"
                } else {
                    "pty"
                }
            }
            ProcKind::Spare => "spare",
            ProcKind::Stray => "stray",
            ProcKind::Subagent => "subagent",
        };
        let state = match p.state {
            State::Working => "working",
            State::Waiting => "waiting",
            State::Idle => "idle",
            State::Ended => "ended",
            State::Unknown => "unknown",
        };
        let where_ = match &p.location {
            Some(l) => l.label.clone().unwrap_or_else(|| format!("tab {}", l.tab_index + 1)).replace("Konsole", "K"),
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
        let cpu = p.cpu_percent.map(|c| format!("{c:.0}%")).unwrap_or_else(|| "-".into());
        out!(
            "{:<9} {:<8} {:>6} {:>5}  {:<8} {:<8} {:<16} {:<34} {}",
            pid,
            kind,
            mem(p.rss_bytes),
            cpu,
            if folded { "" } else { state },
            if folded { String::new() } else { idle(p.idle_seconds) },
            if folded { String::new() } else { where_ },
            util::take_chars(&task, 34),
            if folded { "" } else { p.project_name.as_deref().unwrap_or("") }
        );
        if let Some(h) = p.helpers.as_ref().filter(|h| !h.is_empty()) {
            out!("{:<9} subagents running: {}", "", h.join(" · "));
        }
        if let Some(r) = &p.stale_reason {
            out!("{:<9} stale: {r}", "");
        } else if p.kind == ProcKind::Stray
            && let Some(t) = &p.title
        {
            out!("{:<9} stray: {t}", "");
        }
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
                format!("{n} {} ({} session{})", mem(p.rss_bytes), p.sessions, if p.sessions == 1 { "" } else { "s" })
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
                quiet.push(format!("{} ({why})", wake_core::util::take_chars(&tilde(&c.path), 60)));
            }
        }
        let refs: Vec<&RepoCheck> = checks.iter().collect();
        let dirty = |path: &str| repos.iter().find(|r| r.state.path == path).map_or(0, |r| r.state.dirty_files + r.state.untracked);
        if let Some(b) = remote::behind_for(&p.id, &p.name, &p.roots, &refs, &dirty) {
            behind += 1;
            out!("{:<18} {}", util::take_chars(&p.name, 18), b.line);
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

fn tilde(p: &str) -> String {
    let home = store::home_dir();
    // On Fedora Atomic $HOME is /home/x but paths are often /var/home/x: try both.
    let real = std::fs::canonicalize(&home).unwrap_or_else(|_| home.clone());
    let path = std::path::Path::new(p);
    match path.strip_prefix(&home).or_else(|_| path.strip_prefix(&real)) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => p.to_string(),
    }
}
