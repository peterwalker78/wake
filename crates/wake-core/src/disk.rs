//! What takes disk space, in plain words (added 2026-11-20): per project (build output, agent
//! worktrees, archives and other big folders, its Claude conversations and job folders) and for
//! the machine (free space, caches, toolchains, containers, apps, the Trash).
//!
//! Measuring is read-only: a `du`-style walk with `lstat` that never follows a symlink, never
//! leaves the filesystem it started on, counts hard-linked files once, and stops at a time limit
//! (the size is then "at least"). It runs on Wake's own thread at the lowest CPU and I/O priority,
//! hourly and when asked, and the last result is kept in `~/.local/share/wake/disk.json`.
//!
//! The only writes, each after the person confirms it:
//! - **Clear** moves build output or a cache to the Trash (the freedesktop Trash spec: an info
//!   file, then a rename on the same disk; never a copy, never `rm`). Refused while an agent in
//!   that project is working, while any process works inside it, for anything Wake didn't list,
//!   and for anything that isn't build output or a cache.
//! - **Remove worktree** runs `git worktree remove <path>` (never `--force`) for an agent worktree
//!   whose branch is merged, whose tree is clean and that nobody is inside.
//!
//! Conversations, `~/.claude`, distroboxes, containers, flatpak and toolchains are never touched:
//! for those Wake shows the command a person would use and what it does.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::store::{self, ProjectRec};
use crate::types::*;
use crate::util;

/// How often the whole machine is measured again.
pub const EVERY: Duration = Duration::from_secs(3600);
/// Time limit for one project's folders.
pub const PROJECT_LIMIT: Duration = Duration::from_secs(120);
/// Time limit for one machine item (a cache folder, flatpak).
pub const ITEM_LIMIT: Duration = Duration::from_secs(30);
/// `podman system df` gets this long; it isn't shown when it doesn't answer.
pub const PODMAN_LIMIT: Duration = Duration::from_secs(5);
const MB: u64 = 1 << 20;
/// Smallest "other big folder" worth naming.
const BIG_FOLDER: u64 = 100 * MB;
/// Smallest build output or cache worth listing.
const SMALL: u64 = MB;

// ---------------------------------------------------------------- walking

/// What a walk found.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Tally {
    /// Allocated bytes (`du`), hard links once.
    pub bytes: u64,
    /// Bytes not inside a classified folder (the "loose" part, for big-folder naming).
    pub loose: u64,
    /// Newest modification time seen (epoch seconds).
    pub newest: f64,
    /// Stopped at the time limit.
    pub partial: bool,
}

impl Tally {
    fn add(&mut self, o: &Tally) {
        self.bytes += o.bytes;
        self.newest = self.newest.max(o.newest);
        self.partial |= o.partial;
    }
}

fn mtime(m: &fs::Metadata) -> f64 {
    m.mtime() as f64 + m.mtime_nsec() as f64 / 1e9
}

/// A `du` that never follows a symlink, never crosses onto another filesystem and stops at its
/// deadline.
pub struct Walker {
    deadline: Instant,
    seen: HashSet<(u64, u64)>,
    count: u64,
    stopped: bool,
}

impl Walker {
    pub fn new(limit: Duration) -> Walker {
        Walker { deadline: Instant::now() + limit, seen: HashSet::new(), count: 0, stopped: false }
    }

    /// Past the deadline (looked at every 256 entries, so the clock costs nothing).
    fn late(&mut self) -> bool {
        self.count += 1;
        if self.count.is_multiple_of(256) && !self.stopped {
            self.stopped = Instant::now() > self.deadline;
        }
        self.stopped
    }

    fn own(&mut self, m: &fs::Metadata) -> u64 {
        if !m.is_dir() && m.nlink() > 1 && !self.seen.insert((m.dev(), m.ino())) {
            return 0;
        }
        m.blocks() * 512
    }

    /// The whole tree under `path` (itself included).
    pub fn walk(&mut self, path: &Path) -> Tally {
        let mut t = Tally::default();
        let Ok(top) = fs::symlink_metadata(path) else { return t };
        t.bytes += self.own(&top);
        t.newest = mtime(&top);
        if !top.is_dir() {
            t.loose = t.bytes;
            return t;
        }
        let dev = top.dev();
        let mut stack = vec![path.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                if self.late() {
                    t.partial = true;
                    t.loose = t.bytes;
                    return t;
                }
                let Ok(m) = e.metadata() else { continue }; // DirEntry::metadata is lstat
                if m.dev() != dev {
                    continue;
                }
                t.bytes += self.own(&m);
                t.newest = t.newest.max(mtime(&m));
                if m.is_dir() {
                    stack.push(e.path());
                }
            }
        }
        t.loose = t.bytes;
        t
    }
}

// ---------------------------------------------------------------- classifying

/// What a folder in a project is, as Wake recognises it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    RustTarget,
    NodeModules,
    PyVenv,
    GodotCache,
    WebCache,
    WebDist,
    RailsTmp,
    RailsLog,
    RubyGems,
    ToolCache,
    Archive,
    Git,
    /// `<repo>/.claude/worktrees`: each folder in it is an agent worktree.
    Worktrees,
}

impl Class {
    fn kind(self) -> DiskKind {
        match self {
            Class::Archive => DiskKind::Archive,
            Class::Git => DiskKind::Git,
            Class::Worktrees => DiskKind::Worktree,
            _ => DiskKind::BuildOutput,
        }
    }

    fn name(self, dir: &str) -> String {
        match self {
            Class::RustTarget => "Rust build output".into(),
            Class::NodeModules => "JavaScript packages (node_modules)".into(),
            Class::PyVenv => format!("Python environment ({dir})"),
            Class::GodotCache => "Godot import cache".into(),
            Class::WebCache => format!("Web build cache ({dir})"),
            Class::WebDist => format!("Built site ({dir})"),
            Class::RailsTmp => "Rails temporary files (tmp)".into(),
            Class::RailsLog => "Rails log files".into(),
            Class::RubyGems => "Ruby gems (vendor/bundle)".into(),
            Class::ToolCache => format!("Tool cache ({dir})"),
            Class::Archive => format!("Archive or exported builds ({dir})"),
            Class::Git => "Git history (.git)".into(),
            Class::Worktrees => "Agent worktrees".into(),
        }
    }

    fn cost(self, bytes: u64) -> String {
        match self {
            Class::RustTarget if bytes > 5 << 30 => "Rebuilds on the next build: a long one on this computer (tens of minutes, memory-heavy).".into(),
            Class::RustTarget => "Rebuilds on the next build (a few minutes).".into(),
            Class::NodeModules => "Comes back with npm install (needs the network).".into(),
            Class::PyVenv => "Comes back by installing the packages again (needs the network).".into(),
            Class::GodotCache => "Godot imports the assets again next time the project opens.".into(),
            Class::WebCache | Class::WebDist => "The next build makes it again.".into(),
            Class::RailsTmp => "Rails makes it again; restart a running dev server afterwards.".into(),
            Class::RailsLog => "Only old log lines are lost.".into(),
            Class::RubyGems => "Comes back with bundle install (needs the network).".into(),
            Class::ToolCache => "The tool fills it again next run.".into(),
            Class::Archive => "May be the only copy of an old version or a shipped build. Wake won't delete it.".into(),
            Class::Git => "The project's history. Never delete it.".into(),
            Class::Worktrees => String::new(),
        }
    }

    fn safety(self) -> DiskSafety {
        match self {
            Class::Archive | Class::Git => DiskSafety::Keep,
            Class::Worktrees => DiskSafety::Check,
            _ => DiskSafety::Safe,
        }
    }
}

/// Whether `dir`'s .gitignore (or the project root's) ignores `name` — a sign a `dist` or `build`
/// folder is generated rather than written by hand.
fn ignored(dir: &Path, root: &Path, name: &str) -> bool {
    for g in [dir.join(".gitignore"), root.join(".gitignore")] {
        let mut s = String::new();
        if fs::File::open(&g).and_then(|f| f.take(64 * 1024).read_to_string(&mut s)).is_err() {
            continue;
        }
        if s.lines().map(str::trim).any(|l| {
            let l = l.trim_start_matches('/').trim_end_matches('/');
            l == name || l == format!("{name}/") || l == format!("**/{name}")
        }) {
            return true;
        }
    }
    false
}

/// What the folder `name` inside `dir` is, if Wake knows; `names` are `dir`'s entries.
pub fn classify(dir: &Path, root: &Path, names: &HashSet<String>, name: &str, depth: usize) -> Option<Class> {
    let child = dir.join(name);
    let has = |n: &str| names.contains(n);
    let lower = name.to_lowercase();
    Some(match name {
        ".git" => Class::Git,
        "worktrees" if dir.file_name().is_some_and(|n| n == ".claude") => Class::Worktrees,
        "target" if has("Cargo.toml") || child.join("CACHEDIR.TAG").exists() || child.join(".rustc_info.json").exists() => Class::RustTarget,
        "node_modules" if has("package.json") => Class::NodeModules,
        ".venv" | "venv" | "env" if child.join("pyvenv.cfg").exists() => Class::PyVenv,
        ".godot" | ".import" if has("project.godot") => Class::GodotCache,
        ".next" | ".nuxt" | ".svelte-kit" | ".parcel-cache" | ".turbo" | ".angular" | ".vite" if has("package.json") => Class::WebCache,
        "dist" | "build" | "out" if has("package.json") && ignored(dir, root, name) => Class::WebDist,
        "tmp" if has("Gemfile") && has("config") => Class::RailsTmp,
        "log" if has("Gemfile") && has("config") => Class::RailsLog,
        ".pytest_cache" | ".mypy_cache" | ".ruff_cache" | ".tox" | ".nox" | ".gradle" | ".flatpak-builder" => Class::ToolCache,
        "bundle" if dir.file_name().is_some_and(|n| n == "vendor") && dir.parent().is_some_and(|p| p.join("Gemfile").exists()) => Class::RubyGems,
        _ if depth == 0 && matches!(lower.as_str(), "archive" | "archives" | "old" | "backup" | "backups" | "builds" | "exports" | "export" | "releases") => Class::Archive,
        _ => return None,
    })
}

/// A classified folder found in a project.
#[derive(Debug, Clone)]
pub struct Found {
    pub path: PathBuf,
    pub class: Class,
    pub tally: Tally,
    /// The worktree it sits in, if any.
    pub within: Option<PathBuf>,
}

/// The result of walking one project root.
#[derive(Debug, Clone, Default)]
pub struct TreeScan {
    pub total: Tally,
    pub found: Vec<Found>,
    /// Top-level folders by their loose size (outside classified folders).
    pub top: Vec<(PathBuf, Tally)>,
    /// Each worktree: its path and whole tally.
    pub worktrees: Vec<(PathBuf, Tally)>,
}

/// Walks a project root, classifying as it goes.
pub fn scan_tree(w: &mut Walker, root: &Path) -> TreeScan {
    let mut out = TreeScan::default();
    let Ok(m) = fs::symlink_metadata(root) else { return out };
    if !m.is_dir() {
        return out;
    }
    let mut top = Vec::new();
    let t = scan_dir(w, root, root, m.dev(), 0, None, &mut out, Some(&mut top));
    out.total = t;
    out.total.bytes += w.own(&m);
    out.top = top;
    out
}

#[expect(clippy::too_many_arguments, reason = "the recursive walk's own state, passed down each level")]
fn scan_dir(w: &mut Walker, dir: &Path, root: &Path, dev: u64, depth: usize, within: Option<&Path>, out: &mut TreeScan, mut top: Option<&mut Vec<(PathBuf, Tally)>>) -> Tally {
    let mut t = Tally::default();
    let Ok(rd) = fs::read_dir(dir) else { return t };
    let entries: Vec<fs::DirEntry> = rd.flatten().collect();
    let names: HashSet<String> = entries.iter().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    for e in entries {
        if w.late() {
            t.partial = true;
            break;
        }
        let Ok(m) = e.metadata() else { continue };
        if m.dev() != dev {
            continue;
        }
        let own = w.own(&m);
        t.bytes += own;
        t.loose += own;
        t.newest = t.newest.max(mtime(&m));
        if !m.is_dir() {
            continue;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        let path = e.path();
        let sub = if depth >= 40 {
            Tally::default()
        } else {
            match classify(dir, root, &names, &name, depth) {
                Some(Class::Worktrees) if within.is_none() => {
                    let mut all = Tally::default();
                    if let Ok(rd) = fs::read_dir(&path) {
                        for wt in rd.flatten() {
                            let Ok(wm) = wt.metadata() else { continue };
                            if !wm.is_dir() || wm.dev() != dev {
                                continue;
                            }
                            let wp = wt.path();
                            let mut wtt = scan_dir(w, &wp, &wp, dev, 1, Some(&wp), out, None);
                            wtt.bytes += w.own(&wm);
                            out.worktrees.push((wp, wtt));
                            all.add(&wtt);
                        }
                    }
                    all.loose = 0;
                    all
                }
                Some(c) => {
                    let mut ct = w.walk(&path);
                    ct.bytes = ct.bytes.saturating_sub(own); // its own entry is counted above
                    out.found.push(Found { path: path.clone(), class: c, tally: ct, within: within.map(Path::to_path_buf) });
                    Tally { bytes: ct.bytes, loose: 0, newest: ct.newest, partial: ct.partial }
                }
                None => scan_dir(w, &path, root, dev, depth + 1, within, out, None),
            }
        };
        t.bytes += sub.bytes;
        t.loose += sub.loose;
        t.newest = t.newest.max(sub.newest);
        t.partial |= sub.partial;
        if let Some(top) = top.as_deref_mut() {
            top.push((path, Tally { loose: sub.loose + own, ..sub }));
        }
    }
    t
}

// ---------------------------------------------------------------- worktrees (read-only git)

/// The branch a worktree has checked out, from its `.git` file and HEAD (no git needed).
pub fn worktree_branch(wt: &Path) -> Option<String> {
    let mut s = String::new();
    fs::File::open(wt.join(".git")).ok()?.take(4096).read_to_string(&mut s).ok()?;
    let gitdir = s.lines().find_map(|l| l.strip_prefix("gitdir:"))?.trim();
    let gitdir = if Path::new(gitdir).is_absolute() { PathBuf::from(gitdir) } else { wt.join(gitdir) };
    let mut head = String::new();
    fs::File::open(gitdir.join("HEAD")).ok()?.take(4096).read_to_string(&mut head).ok()?;
    let head = head.trim();
    Some(match head.strip_prefix("ref: refs/heads/") {
        Some(b) => b.to_string(),
        None => format!("detached at {}", util::take_chars(head, 8)),
    })
}

/// The repo's default branch: what origin/HEAD names, else main or master if there is one.
pub fn default_branch(repo: &Path) -> Option<String> {
    if let Ok(o) = crate::gitinfo::git(repo, &["rev-parse", "--abbrev-ref", "origin/HEAD"]) {
        let o = o.trim();
        if let Some(b) = o.strip_prefix("origin/") {
            return Some(b.to_string());
        }
    }
    ["main", "master"].into_iter().find(|b| crate::gitinfo::git(repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{b}")]).is_ok()).map(String::from)
}

/// Whether `branch`'s commits are all in `default` (here or on origin). None when git can't say.
pub fn is_merged(repo: &Path, branch: &str, default: &str) -> Option<bool> {
    if branch.starts_with("detached") || branch.starts_with('-') || default.starts_with('-') {
        return None;
    }
    let mut answered = false;
    for target in [format!("refs/heads/{default}"), format!("refs/remotes/origin/{default}")] {
        match crate::gitinfo::git_code(repo, &["merge-base", "--is-ancestor", &format!("refs/heads/{branch}"), &target], &[], crate::gitinfo::TIMEOUT) {
            Ok((0, ..)) => return Some(true),
            Ok((1, ..)) => answered = true,
            _ => {}
        }
    }
    answered.then_some(false)
}

/// Files changed or new in a worktree.
pub fn dirty_files(wt: &Path) -> Option<u32> {
    let o = crate::gitinfo::git(wt, &["status", "--porcelain=v1", "-b"]).ok()?;
    Some(o.lines().filter(|l| !l.starts_with("##") && !l.trim().is_empty()).count() as u32)
}

/// A process of this user whose working folder is `path` or inside it: (pid, its short name).
pub fn process_inside(path: &Path, proc_root: &Path, uid: u32) -> Option<(u32, String)> {
    let want = canonical(path);
    let rd = fs::read_dir(proc_root).ok()?;
    for e in rd.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
        if e.metadata().map_or(true, |m| m.uid() != uid) {
            continue;
        }
        let Ok(cwd) = fs::read_link(e.path().join("cwd")) else { continue };
        if folder_views(&cwd, &e.path()).iter().any(|c| c.starts_with(path) || c.starts_with(&want)) {
            let comm = fs::read_to_string(e.path().join("comm")).unwrap_or_default().trim().to_string();
            return Some((pid, comm));
        }
    }
    None
}

/// The ways a process's working folder can be written on the host. A process in a distrobox or
/// toolbox reports its folder as the container sees it (`/home/u/x` where the host has
/// `/var/home/u/x` behind the `/home` link), so the folder is also resolved on the host and, when
/// the process has its own root, through `/proc/<pid>/root`.
fn folder_views(cwd: &Path, proc_dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![cwd.to_path_buf()];
    if cwd.exists() {
        out.push(canonical(cwd));
    }
    if let Ok(root) = fs::read_link(proc_dir.join("root"))
        && root != Path::new("/")
        && let Ok(inner) = cwd.strip_prefix("/")
    {
        out.push(canonical(&root.join(inner)));
    }
    out
}

/// Whether a cargo or rustc process of this user is running (the registry is in use).
pub fn cargo_running(proc_root: &Path, uid: u32) -> bool {
    let Ok(rd) = fs::read_dir(proc_root) else { return false };
    rd.flatten().any(|e| {
        e.file_name().to_str().is_some_and(|n| n.parse::<u32>().is_ok())
            && e.metadata().is_ok_and(|m| m.uid() == uid)
            && matches!(fs::read_to_string(e.path().join("comm")).unwrap_or_default().trim(), "cargo" | "rustc" | "cargo-clippy" | "clippy-driver")
    })
}

// ---------------------------------------------------------------- inputs from the rest of Wake

/// A session, as disk measuring needs it.
#[derive(Debug, Clone, Default)]
pub struct SessionDisk {
    pub id: String,
    pub project: Option<String>,
    pub title: String,
    pub started_at: Option<String>,
    /// Its transcript file (`~/.claude/projects/<dir>/<id>.jsonl`).
    pub transcript: Option<PathBuf>,
    /// Every folder it worked in (record cwds, worktrees).
    pub folders: Vec<String>,
    pub working: bool,
}

/// A live agent process: pid, session id, title.
#[derive(Debug, Clone, Default)]
pub struct LiveDisk {
    pub pid: u32,
    pub session: String,
    pub title: String,
}

/// Where things are.
#[derive(Debug, Clone)]
pub struct Places {
    pub home: PathBuf,
    pub claude: PathBuf,
    pub codex: PathBuf,
    pub proc_root: PathBuf,
    pub uid: u32,
    /// The Trash (`$XDG_DATA_HOME/Trash`).
    pub trash: PathBuf,
}

impl Places {
    /// The real machine's, with symlinks resolved (on Bazzite /home is a link to /var/home, and
    /// project roots are written either way).
    pub fn real(paths: &store::Paths) -> Places {
        Places {
            home: canonical(&paths.home),
            claude: canonical(&paths.claude),
            codex: canonical(&paths.codex),
            proc_root: PathBuf::from("/proc"),
            uid: crate::sys::uid(),
            trash: canonical(&paths.data_home.join("Trash")),
        }
    }
}

/// `p` with symlinks resolved in the part that exists (the last part kept as written, so a
/// symlink there is never followed).
pub fn canonical(p: &Path) -> PathBuf {
    if let Ok(c) = fs::canonicalize(p)
        && fs::symlink_metadata(p).is_ok_and(|m| !m.file_type().is_symlink())
    {
        return c;
    }
    match (p.parent(), p.file_name()) {
        (Some(parent), Some(name)) => canonical(parent).join(name),
        _ => p.to_path_buf(),
    }
}

/// "sleep (PID 33087), run by the session \"tideline-app-shortcut-jump\"": a process, and the live
/// agent session above it in the process tree when there is one.
fn who(pid: u32, comm: &str, live: &[LiveDisk], proc_root: &Path) -> String {
    let mut p = pid;
    for _ in 0..64 {
        if let Some(l) = live.iter().find(|l| l.pid == p) {
            return if p == pid { format!("the session \"{}\" (PID {pid})", l.title) } else { format!("{comm} (PID {pid}), run by the session \"{}\"", l.title) };
        }
        match crate::procs::read_stat(proc_root, p).map(|s| s.ppid) {
            Some(up) if up > 1 => p = up,
            _ => break,
        }
    }
    format!("{comm} (PID {pid})")
}

fn iso(t: f64) -> Option<String> {
    (t > 0.0).then(|| util::iso_from_epoch(t))
}

// ---------------------------------------------------------------- one project

/// Measures one project's folders and builds its items (conversations and caches elsewhere are
/// added by [`measure`]).
pub fn measure_project(p: &ProjectRec, sessions: &[SessionDisk], live: &[LiveDisk], places: &Places, limit: Duration) -> ProjectDisk {
    let mut w = Walker::new(limit);
    let working = sessions.iter().any(|s| s.working && s.project.as_deref() == Some(p.id.as_str()));
    let mut out = ProjectDisk { project: p.id.clone(), measured_at: Some(util::now_iso()), ..Default::default() };
    let mut seen_roots: Vec<PathBuf> = Vec::new();
    for r in &p.roots {
        let Some(root) = store::norm_path(r, &places.home).map(PathBuf::from) else { continue };
        if seen_roots.iter().any(|s| root.starts_with(s)) {
            continue;
        }
        seen_roots.push(root.clone());
        let scan = scan_tree(&mut w, &root);
        out.bytes += scan.total.bytes;
        out.partial |= scan.total.partial;
        for (wt, t) in &scan.worktrees {
            out.items.push(worktree_item(wt, t, &scan.found, sessions, live, places));
        }
        for f in &scan.found {
            if f.class == Class::Git && f.tally.bytes < 2 * BIG_FOLDER {
                continue;
            }
            if f.tally.bytes < SMALL {
                continue;
            }
            let dir = f.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let mut name = f.class.name(&dir);
            if let Some(wt) = &f.within {
                name = format!("{name} in worktree {}", wt.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
            } else if f.path.parent() != Some(root.as_path()) && !matches!(f.class, Class::Git | Class::RubyGems) {
                // Not at the top: say where ("Rust build output in crates/x").
                if let Some(rel) = f.path.parent().and_then(|pp| pp.strip_prefix(&root).ok()) {
                    name = format!("{name} in {}", rel.display());
                }
            }
            let action = match f.class {
                Class::Archive => Some(DiskAction::OpenFolder),
                Class::Git | Class::Worktrees => None,
                _ => Some(DiskAction::Clear),
            };
            let refused = (action == Some(DiskAction::Clear)).then(|| clear_refusal(&f.path, working, &p.name, places)).flatten();
            out.items.push(DiskItem {
                path: f.path.display().to_string(),
                kind: f.class.kind(),
                name,
                bytes: f.tally.bytes,
                partial: f.tally.partial,
                last_used: iso(f.tally.newest),
                safety: f.class.safety(),
                cost: f.class.cost(f.tally.bytes),
                action,
                command: None,
                command_note: None,
                worktree: None,
                within: f.within.as_ref().map(|x| x.display().to_string()),
                refused,
            });
        }
        let mut top: Vec<&(PathBuf, Tally)> = scan.top.iter().filter(|(_, t)| t.loose >= BIG_FOLDER).collect();
        top.sort_by_key(|x| std::cmp::Reverse(x.1.loose));
        for (path, t) in top.into_iter().take(4) {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if name == ".git" || name == ".claude" {
                continue;
            }
            out.items.push(DiskItem {
                path: path.display().to_string(),
                kind: DiskKind::Folder,
                name: format!("Folder {name}"),
                bytes: t.loose,
                partial: t.partial,
                last_used: iso(t.newest),
                safety: DiskSafety::Keep,
                cost: "Wake doesn't know what this is. Look inside before deleting anything.".into(),
                action: Some(DiskAction::OpenFolder),
                command: None,
                command_note: None,
                worktree: None,
                within: None,
                refused: None,
            });
        }
    }
    finish(&mut out);
    out
}

/// Totals and order: biggest first, each worktree's build output right after its worktree.
fn finish(out: &mut ProjectDisk) {
    out.rebuildable = out.items.iter().filter(|i| rebuildable(i)).map(|i| i.bytes).sum();
    let mut top: Vec<DiskItem> = out.items.iter().filter(|i| i.within.is_none()).cloned().collect();
    top.sort_by_key(|x| std::cmp::Reverse(x.bytes));
    let mut ordered = Vec::new();
    for t in top {
        let inner: Vec<DiskItem> = out.items.iter().filter(|i| i.within.as_deref() == Some(t.path.as_str())).cloned().collect();
        ordered.push(t);
        ordered.extend(inner);
    }
    out.items = ordered;
}

fn rebuildable(i: &DiskItem) -> bool {
    matches!(i.kind, DiskKind::BuildOutput | DiskKind::Cache) && i.safety == DiskSafety::Safe
}

fn worktree_item(wt: &Path, t: &Tally, found: &[Found], sessions: &[SessionDisk], live: &[LiveDisk], places: &Places) -> DiskItem {
    // repo/.claude/worktrees/<name>
    let repo = wt.parent().and_then(Path::parent).and_then(Path::parent).map(Path::to_path_buf).unwrap_or_default();
    let wname = wt.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let branch = worktree_branch(wt);
    let default = default_branch(&repo);
    let merged = match (&branch, &default) {
        (Some(b), Some(d)) => is_merged(&repo, b, d),
        _ => None,
    };
    let dirty = dirty_files(wt);
    let ws = wt.display().to_string();
    let inside = |f: &str| f == ws || f.starts_with(&format!("{ws}/"));
    let made: Option<&SessionDisk> = sessions.iter().filter(|s| s.folders.iter().any(|f| inside(f))).min_by(|a, b| a.started_at.cmp(&b.started_at));
    let in_use = process_inside(wt, &places.proc_root, places.uid).map(|(pid, comm)| who(pid, &comm, live, &places.proc_root));
    let build: u64 = found.iter().filter(|f| f.within.as_deref() == Some(wt)).map(|f| f.tally.bytes).sum();
    let state = match merged {
        Some(true) => "merged",
        Some(false) => "not merged",
        None => "merge unknown",
    };
    let bname = branch.clone().unwrap_or_else(|| wname.clone());
    let mut cost = match merged {
        Some(true) => format!("Its branch {bname} is in {}; removing it deletes only this copy (the branch stays).", default.as_deref().unwrap_or("the default branch")),
        Some(false) => format!("Its branch {bname} has commits {} doesn't: removing it could lose work.", default.as_deref().unwrap_or("the default branch")),
        None => "Wake couldn't tell whether its work is merged.".into(),
    };
    if build > 0 {
        cost.push_str(&format!(" {} of it is build output, listed below.", util::bytes(build)));
    }
    let refused = if merged != Some(true) {
        Some("Its branch isn't merged.".to_string())
    } else if let Some(u) = &in_use {
        Some(format!("Someone is inside it: {u}."))
    } else if dirty.is_some_and(|d| d > 0) {
        Some(format!("{} not committed in it.", if dirty == Some(1) { "1 file".to_string() } else { format!("{} files", dirty.unwrap_or(0)) }))
    } else {
        None
    };
    DiskItem {
        path: ws.clone(),
        kind: DiskKind::Worktree,
        name: format!("Agent worktree for branch {bname} ({state})"),
        bytes: t.bytes,
        partial: t.partial,
        last_used: iso(t.newest),
        safety: if merged == Some(true) { DiskSafety::Safe } else { DiskSafety::Check },
        cost,
        action: Some(if merged == Some(true) { DiskAction::RemoveWorktree } else { DiskAction::OpenFolder }),
        command: Some(format!("git -C {} worktree remove {}", util::shell_quote(&util::tilde(&repo, &places.home)), util::shell_quote(&format!(".claude/worktrees/{wname}")))),
        command_note: Some("deletes the copy; refuses if anything in it isn't committed".into()),
        worktree: Some(WorktreeInfo {
            repo: repo.display().to_string(),
            branch,
            default_branch: default,
            merged,
            dirty_files: dirty,
            in_use,
            made_by: made.map(|s| s.id.clone()),
            made_by_title: made.map(|s| s.title.clone()),
        }),
        within: None,
        refused,
    }
}

/// Why Clear can't run on `path` now, or None.
fn clear_refusal(path: &Path, working: bool, project_name: &str, places: &Places) -> Option<String> {
    if working {
        return Some(format!("An agent in {project_name} is working; it may be building into this. Try when it's idle."));
    }
    if let Some((pid, comm)) = process_inside(path, &places.proc_root, places.uid) {
        return Some(format!("{comm} (PID {pid}) is working inside it."));
    }
    None
}

// ---------------------------------------------------------------- conversations and job folders

/// Job folder by session id: `~/.claude/jobs/<id>/state.json` names its `sessionId`.
pub fn job_dirs(claude: &Path) -> HashMap<String, PathBuf> {
    let mut out = HashMap::new();
    let Ok(rd) = fs::read_dir(claude.join("jobs")) else { return out };
    for e in rd.flatten() {
        let mut raw = Vec::new();
        if fs::File::open(e.path().join("state.json")).and_then(|f| f.take(1 << 20).read_to_end(&mut raw)).is_err() {
            continue;
        }
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&raw)
            && let Some(sid) = v.get("sessionId").and_then(|s| s.as_str())
        {
            out.insert(sid.to_string(), e.path());
        }
    }
    out
}

/// One session's conversation on disk: its transcript and its subagents' folder.
fn transcript_tally(w: &mut Walker, t: &Path) -> Tally {
    let mut out = w.walk(t);
    let dir = t.with_extension("");
    if dir.is_dir() {
        out.add(&w.walk(&dir));
    }
    out
}

/// Conversation and job-folder items for a group of sessions (one project, or none).
fn conversation_items(sessions: &[&SessionDisk], jobs: &HashMap<String, PathBuf>, places: &Places, w: &mut Walker, who: &str) -> Vec<DiskItem> {
    let mut out = Vec::new();
    let mut conv = Tally::default();
    let mut n = 0;
    for s in sessions {
        if let Some(t) = &s.transcript {
            let x = transcript_tally(w, t);
            if x.bytes > 0 {
                n += 1;
                conv.add(&x);
            }
        }
    }
    if conv.bytes > 0 {
        out.push(DiskItem {
            path: places.claude.join("projects").display().to_string(),
            kind: DiskKind::Transcripts,
            name: format!("Claude conversations {who} ({n} {})", if n == 1 { "session" } else { "sessions" }),
            bytes: conv.bytes,
            partial: conv.partial,
            last_used: iso(conv.newest),
            safety: DiskSafety::Keep,
            cost: "Deleting one loses that session's history and resume; Wake keeps its summary. Claude deletes conversations older than its cleanup period (30 days unless cleanupPeriodDays in ~/.claude/settings.json says otherwise) by itself.".into(),
            action: None,
            command: None,
            command_note: None,
            worktree: None,
            within: None,
            refused: None,
        });
    }
    let mut jt = Tally::default();
    let mut ids = Vec::new();
    for s in sessions {
        if let Some(j) = jobs.get(&s.id) {
            let x = w.walk(j);
            jt.add(&x);
            ids.push((x.bytes, j.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()));
        }
    }
    if jt.bytes >= SMALL {
        ids.sort_by_key(|x| std::cmp::Reverse(x.0));
        let names: Vec<String> = ids.iter().take(6).map(|(_, n)| n.clone()).collect();
        let names: Vec<String> = names.iter().map(|n| util::shell_quote(n)).collect();
        let command = if names.len() == 1 { format!("ls -la ~/.claude/jobs/{}/tmp", names[0]) } else { format!("cd ~/.claude/jobs && du -sh {}", names.join(" ")) };
        out.push(DiskItem {
            path: places.claude.join("jobs").display().to_string(),
            kind: DiskKind::JobFiles,
            name: format!("Background job folders {who} ({})", ids.len()),
            bytes: jt.bytes,
            partial: jt.partial,
            last_used: iso(jt.newest),
            safety: DiskSafety::Check,
            cost: "Each job's tmp folder holds its work files, and hand-back files (briefs, reports) live there too: look before deleting.".into(),
            action: None,
            command: Some(command),
            command_note: Some("shows what's in them, biggest first; changes nothing".into()),
            worktree: None,
            within: None,
            refused: None,
        });
    }
    out
}

// ---------------------------------------------------------------- the machine

/// What a folder in ~/.cache is, in a few words, when Wake knows the name.
fn cache_words(name: &str) -> String {
    match name {
        "Homebrew" => "Homebrew's downloads".into(),
        "mozilla" => "Firefox's cache".into(),
        "thumbnails" => "Picture previews".into(),
        "mesa_shader_cache" | "mesa_shader_cache_db" | "radv_builtin_shaders" | "nvidia" => "Graphics shader cache".into(),
        "pip" => "Python packages pip downloaded".into(),
        "go-build" => "Go's build cache".into(),
        "fontconfig" => "Font cache".into(),
        "ms-playwright" => "Playwright's browsers".into(),
        "electron" => "Electron downloads".into(),
        "bazaar" | "Bazaar" => "Bazaar (app store) cache".into(),
        _ => format!("{name} cache"),
    }
}

/// The project a ~/.cache folder belongs to by its name ("skyhook-probe" -> Skyhook), if any.
pub fn cache_project(name: &str, projects: &[ProjectRec]) -> Option<String> {
    let n = name.to_lowercase();
    projects.iter().filter(|p| !p.removed).find_map(|p| {
        let pname = p.name.to_lowercase();
        let first = pname.split(|c: char| !c.is_alphanumeric()).next().unwrap_or("").to_string();
        let starts = |w: &str| w.len() >= 4 && (n == w || n.starts_with(&format!("{w}-")) || n.starts_with(&format!("{w}_")));
        (starts(&pname) || starts(&first)).then(|| p.id.clone())
    })
}

/// A cache item (Clear moves it to the Trash).
fn cache_item(path: &Path, t: &Tally, name: String, cost: &str) -> DiskItem {
    DiskItem {
        path: path.display().to_string(),
        kind: DiskKind::Cache,
        name,
        bytes: t.bytes,
        partial: t.partial,
        last_used: iso(t.newest),
        safety: DiskSafety::Safe,
        cost: cost.into(),
        action: Some(DiskAction::Clear),
        command: None,
        command_note: None,
        worktree: None,
        within: None,
        refused: None,
    }
}

/// One item on the machine's list; `command` is the command a person would use and what it does.
fn system_item(path: &Path, t: &Tally, kind: DiskKind, name: &str, safety: DiskSafety, cost: &str, command: Option<(&str, &str)>) -> DiskItem {
    DiskItem {
        path: path.display().to_string(),
        kind,
        name: name.into(),
        bytes: t.bytes,
        partial: t.partial,
        last_used: iso(t.newest),
        safety,
        cost: cost.into(),
        action: None,
        command: command.map(|(c, _)| c.to_string()),
        command_note: command.map(|(_, note)| note.to_string()),
        worktree: None,
        within: None,
        refused: None,
    }
}

/// Podman's own sizes (`podman system df`), when it answers within a few seconds.
pub type CommandRunner = dyn Fn(&[&str], Duration) -> Option<String> + Send + Sync;

pub fn real_runner() -> Box<CommandRunner> {
    Box::new(|argv: &[&str], limit: Duration| {
        let mut cmd = std::process::Command::new(argv[0]);
        cmd.args(&argv[1..]);
        match crate::run::run_with_timeout(cmd, limit) {
            Ok((0, out, _)) => Some(out),
            _ => None,
        }
    })
}

/// Parses `podman system df` ("Images  12  3  6.6GB  1.5GB (23%)") into (type, size, reclaimable).
pub fn parse_podman_df(out: &str) -> Vec<(String, u64, u64)> {
    let size = |s: &str| -> Option<u64> {
        let s = s.trim();
        let i = s.find(|c: char| c.is_ascii_alphabetic())?;
        let n: f64 = s[..i].parse().ok()?;
        let mult = match s[i..].to_ascii_uppercase().as_str() {
            "B" => 1.0,
            "KB" | "KIB" => 1e3,
            "MB" | "MIB" => 1e6,
            "GB" | "GIB" => 1e9,
            "TB" | "TIB" => 1e12,
            _ => return None,
        };
        Some((n * mult) as u64)
    };
    out.lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let kind = match f.first()? {
                &"Local" => "Volumes".to_string(), // "Local Volumes"
                k => k.to_string(),
            };
            let f: Vec<&str> = if kind == "Volumes" { f[1..].to_vec() } else { f };
            Some((kind, size(f.get(3)?)?, size(f.get(4)?)?))
        })
        .collect()
}

/// The machine's items: everything outside projects.
pub fn measure_machine(projects: &[ProjectRec], places: &Places, run: &CommandRunner, nobody: &[&SessionDisk], jobs: &HashMap<String, PathBuf>) -> (MachineDisk, Vec<(String, DiskItem)>) {
    let home = &places.home;
    let mut items = Vec::new();
    let mut for_projects = Vec::new();
    let (home_free, home_total) = crate::sys::statvfs(home).unwrap_or((0, 0));
    let home_mount = if home.starts_with("/var/home") { "/var/home".to_string() } else { home.display().to_string() };
    let root_note = crate::sys::statvfs(Path::new("/")).and_then(|(free, total)| {
        (total > 0 && free * 100 / total < 2 && Path::new("/run/ostree-booted").exists())
            .then(|| "/ shows as full: on an atomic system such as Bazzite, that is the read-only system image, always full by design. Nothing there needs cleaning.".to_string())
    });

    // ~/.cache, folder by folder.
    if let Ok(rd) = fs::read_dir(home.join(".cache")) {
        for e in rd.flatten() {
            let Ok(m) = e.metadata() else { continue };
            if !m.is_dir() {
                continue;
            }
            let t = Walker::new(ITEM_LIMIT).walk(&e.path());
            if t.bytes < 50 * MB {
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            let words = cache_words(&name);
            match cache_project(&name, projects) {
                Some(pid) => for_projects.push((pid, cache_item(&e.path(), &t, format!("{words} (~/.cache/{name})"), "Test or tool output: the next run makes it again."))),
                None => items.push(cache_item(&e.path(), &t, format!("{words} (~/.cache/{name})"), "A cache: the app fills it again when it needs it.")),
            }
        }
    }
    // Cargo's downloads, and toolchains.
    let reg = home.join(".cargo/registry");
    if reg.is_dir() {
        let t = Walker::new(ITEM_LIMIT).walk(&reg);
        let mut it = cache_item(&reg, &t, "Downloaded Rust crates (cargo registry)".into(), "Cargo downloads them again on the next build (needs the network).");
        if cargo_running(&places.proc_root, places.uid) {
            it.refused = Some("A cargo build is running and may be reading these.".into());
        }
        items.push(it);
    }
    let rustup = home.join(".rustup");
    if rustup.is_dir() {
        let t = Walker::new(ITEM_LIMIT).walk(&rustup);
        items.push(system_item(
            &rustup,
            &t,
            DiskKind::System,
            "Rust toolchains (rustup)",
            DiskSafety::Keep,
            "Every Rust project builds with these.",
            Some(("rustup toolchain list", "lists them; rustup toolchain uninstall <name> removes one you no longer use")),
        ));
    }
    // Conversations and job folders with no project.
    let mut w = Walker::new(ITEM_LIMIT * 2);
    items.extend(conversation_items(nobody, jobs, places, &mut w, "with no project"));
    if places.codex.is_dir() {
        let t = Walker::new(ITEM_LIMIT).walk(&places.codex);
        if t.bytes >= SMALL {
            items.push(system_item(&places.codex, &t, DiskKind::Transcripts, "Codex conversations (~/.codex)", DiskSafety::Keep, "Deleting loses those sessions' history.", None));
        }
    }
    // The Trash: what Clear filled, until it is emptied.
    let trash = places.trash.join("files");
    if trash.is_dir() {
        let t = Walker::new(ITEM_LIMIT).walk(&places.trash);
        if t.bytes >= SMALL {
            items.push(system_item(
                &places.trash,
                &t,
                DiskKind::System,
                "The Trash",
                DiskSafety::Check,
                "What you (or Wake's Clear) moved there: the space comes back only when it is emptied.",
                Some(("gio trash --empty", "empties the Trash for good")),
            ));
        }
    }
    // Flatpak apps and runtimes.
    for (p, name) in [(PathBuf::from("/var/lib/flatpak"), "Flatpak apps and runtimes (system)"), (home.join(".local/share/flatpak"), "Flatpak apps and runtimes (yours)")] {
        if p.is_dir() {
            let t = Walker::new(ITEM_LIMIT).walk(&p);
            if t.bytes >= 10 * MB {
                items.push(system_item(
                    &p,
                    &t,
                    DiskKind::System,
                    name,
                    DiskSafety::Keep,
                    "Installed apps and what they run on.",
                    Some(("flatpak uninstall --unused", "removes runtimes no installed app uses; asks first")),
                ));
            }
        }
    }
    // Containers (distroboxes, the deploy builder): podman's own figures, if it answers quickly.
    if let Some(out) = run(&["podman", "system", "df"], PODMAN_LIMIT) {
        let boxes = run(&["podman", "ps", "-a", "--format", "{{.Names}}|{{.Status}}"], PODMAN_LIMIT).unwrap_or_default();
        let names: Vec<String> = boxes
            .lines()
            .filter_map(|l| l.split_once('|'))
            .map(|(n, st)| {
                let st = st.to_lowercase();
                let word = if st.starts_with("up") {
                    "running".to_string()
                } else if st.starts_with("exited") {
                    format!("stopped{}", st.split_once(')').map(|(_, r)| r.trim().trim_end_matches(" ago").to_string()).filter(|r| !r.is_empty()).map(|r| format!(" {r}")).unwrap_or_default())
                } else {
                    st.clone()
                };
                format!("{n} ({word})")
            })
            .collect();
        let storage = home.join(".local/share/containers");
        for (kind, size, reclaim) in parse_podman_df(&out) {
            let t = Tally { bytes: size, ..Default::default() };
            let (name, cmd, note, cost) = match kind.as_str() {
                "Images" => (
                    "Container images".to_string(),
                    "podman image prune",
                    "removes images nothing uses and nothing names (dangling); asks first",
                    format!("{} of it nothing uses.", util::bytes(reclaim)),
                ),
                "Containers" => (
                    if names.is_empty() { "Containers".to_string() } else { format!("Containers: {}", names.join(", ")) },
                    "podman ps -a",
                    "lists them; a stopped distrobox still holds what you installed in it, so remove one only if you're sure (distrobox rm <name>)",
                    format!("{} is in stopped containers. Wake never removes a container.", util::bytes(reclaim)),
                ),
                _ => ("Container volumes".to_string(), "podman volume ls", "lists them; volumes hold data such as databases", format!("{} nothing uses.", util::bytes(reclaim))),
            };
            items.push(system_item(&storage, &t, DiskKind::System, &name, DiskSafety::Keep, &cost, Some((cmd, note))));
        }
    }
    items.retain(|i| i.bytes > 0);
    items.sort_by_key(|x| std::cmp::Reverse(x.bytes));
    let rebuildable = items.iter().filter(|i| rebuildable(i)).map(|i| i.bytes).sum();
    (MachineDisk { home_mount, home_total, home_free, root_note, items, rebuildable }, for_projects)
}

/// Everything: each project, then the machine. `progress` hears each project's result as it lands
/// (with what is being measured next), so the person sees numbers before the slow parts finish.
#[expect(clippy::too_many_arguments, reason = "one caller (Live), each argument a different part of what it knows")]
pub fn measure(
    projects: &[ProjectRec],
    only: Option<&str>,
    sessions: &[SessionDisk],
    live: &[LiveDisk],
    places: &Places,
    run: &CommandRunner,
    last: Option<&DiskReport>,
    progress: &dyn Fn(&DiskReport),
) -> DiskReport {
    let mut report = last.cloned().unwrap_or_default();
    let jobs = job_dirs(&places.claude);
    let tracked: Vec<&ProjectRec> = projects.iter().filter(|p| !p.removed && only.is_none_or(|o| o == p.id)).collect();
    // The machine's caches first (they include project caches such as skyhook-probe).
    let nobody: Vec<&SessionDisk> = sessions.iter().filter(|s| s.project.as_ref().is_none_or(|p| store::project_by_id(p, projects).is_none())).collect();
    let mut extras: Vec<(String, DiskItem)> = Vec::new();
    if only.is_none() {
        report.measuring = Some("measuring caches and apps".into());
        progress(&report);
        let (machine, ex) = measure_machine(projects, places, run, &nobody, &jobs);
        report.machine = Some(machine);
        extras = ex;
    } else if let Some(prev) = last {
        // Keep what the last full pass found outside the project's folders (its caches).
        for pd in prev.projects.iter().filter(|pd| only == Some(pd.project.as_str())) {
            for i in pd.items.iter().filter(|i| i.kind == DiskKind::Cache) {
                extras.push((pd.project.clone(), i.clone()));
            }
        }
    }
    for p in tracked {
        report.measuring = Some(format!("measuring {}", p.name));
        progress(&report);
        let mut pd = measure_project(p, sessions, live, places, PROJECT_LIMIT);
        let mine: Vec<&SessionDisk> = sessions.iter().filter(|s| s.project.as_deref() == Some(p.id.as_str())).collect();
        let mut w = Walker::new(ITEM_LIMIT);
        let conv = conversation_items(&mine, &jobs, places, &mut w, "here");
        for i in conv.into_iter().chain(extras.iter().filter(|(id, _)| *id == p.id).map(|(_, i)| i.clone())) {
            pd.bytes += i.bytes;
            pd.items.push(i);
        }
        finish(&mut pd);
        report.projects.retain(|x| x.project != p.id);
        report.projects.push(pd);
    }
    report.projects.retain(|x| projects.iter().any(|p| p.id == x.project && !p.removed));
    report.rebuildable = report.projects.iter().map(|p| p.rebuildable).sum::<u64>() + report.machine.as_ref().map_or(0, |m| m.rebuildable);
    report.measuring = None;
    if only.is_none() {
        report.measured_at = Some(util::now_iso());
    }
    report
}

// ---------------------------------------------------------------- the two writes

/// Why `path` must never be cleared, whatever the report says.
fn forbidden(path: &Path, places: &Places) -> Option<&'static str> {
    let path = &canonical(path);
    let home = &places.home;
    let bad_roots = [places.claude.clone(), places.codex.clone(), home.join(".gemini"), home.join(".local/share/flatpak"), home.join(".local/share/containers"), places.trash.clone()];
    if bad_roots.iter().any(|r| path.starts_with(r)) {
        return Some("Wake never deletes anything there.");
    }
    if !path.starts_with(home) || path == home || path.components().count() <= home.components().count() + 1 {
        return Some("Wake only clears folders inside your home, never a top-level one.");
    }
    None
}

/// Today's local time as the Trash spec writes it: 2026-11-20T13:05:09.
fn local_stamp() -> String {
    let tm = crate::sys::local_time(util::now() as i64);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec)
}

/// Percent-encodes a path for a .trashinfo file (keeps `/` and unreserved characters).
fn url_path(p: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut out = String::new();
    for &b in p.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Moves `path` to the Trash at `trash` as the freedesktop Trash spec says: reserve a name with
/// its `info/<name>.trashinfo` (created exclusively), then rename into `files/<name>`. Only on the
/// same filesystem: refuses rather than copy and delete. Returns where it went.
pub fn move_to_trash(path: &Path, trash: &Path) -> Result<PathBuf, String> {
    let m = fs::symlink_metadata(path).map_err(|_| "It isn't there any more.".to_string())?;
    let files = trash.join("files");
    let info = trash.join("info");
    for d in [&files, &info] {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(d).map_err(|e| format!("Couldn't make the Trash folder: {e}"))?;
    }
    let tm = fs::metadata(&files).map_err(|e| e.to_string())?;
    if tm.dev() != m.dev() {
        return Err("It is on another disk from your Trash, so moving it there would mean copying it. Wake doesn't; delete it yourself if you're sure.".into());
    }
    let base = path.file_name().map_or_else(|| "item".into(), |n| n.to_string_lossy().into_owned());
    for n in 1..1000 {
        let name = if n == 1 { base.clone() } else { format!("{base}.{n}") };
        let ip = info.join(format!("{name}.trashinfo"));
        let mut f = match fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&ip) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("Couldn't write to the Trash: {e}")),
        };
        if files.join(&name).symlink_metadata().is_ok() {
            drop(f);
            let _ = fs::remove_file(&ip);
            continue;
        }
        let body = format!("[Trash Info]\nPath={}\nDeletionDate={}\n", url_path(path), local_stamp());
        if let Err(e) = f.write_all(body.as_bytes()) {
            let _ = fs::remove_file(&ip);
            return Err(format!("Couldn't write to the Trash: {e}"));
        }
        drop(f);
        return match fs::rename(path, files.join(&name)) {
            Ok(()) => Ok(files.join(&name)),
            Err(e) => {
                let _ = fs::remove_file(&ip);
                Err(format!("Couldn't move it to the Trash: {e}"))
            }
        };
    }
    Err("The Trash has too many items with this name.".into())
}

/// Clear: move one listed build-output or cache folder to the Trash. `working`: an agent in its
/// project is working now. The report is Wake's last measurement; only what it lists can go.
pub fn clear(report: &DiskReport, path: &str, working: Option<&str>, places: &Places, dry_run: bool) -> ActionResult {
    let fail = |m: String| ActionResult { ok: false, message: m, steps: vec![] };
    let item = report.projects.iter().flat_map(|p| p.items.iter()).chain(report.machine.iter().flat_map(|m| m.items.iter())).find(|i| i.path == path);
    let Some(item) = item else { return fail("Wake only clears what it listed. Measure again and pick it there.".into()) };
    if item.action != Some(DiskAction::Clear) || !matches!(item.kind, DiskKind::BuildOutput | DiskKind::Cache) {
        return fail("Wake only clears build output and caches.".into());
    }
    let p = Path::new(path);
    if let Some(why) = forbidden(p, places) {
        return fail(why.into());
    }
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return fail("That isn't a folder any more; nothing was moved.".into()),
        Err(_) => return fail("It isn't there any more; nothing was moved.".into()),
    }
    if let Some(project) = working {
        return fail(format!("An agent in {project} is working and may be building into this. Nothing was moved; try when it's idle."));
    }
    if let Some((pid, comm)) = process_inside(p, &places.proc_root, places.uid) {
        return fail(format!("{comm} (PID {pid}) is working inside it. Nothing was moved."));
    }
    if p.ends_with(".cargo/registry") && cargo_running(&places.proc_root, places.uid) {
        return fail("A cargo build is running and may be reading these. Nothing was moved.".into());
    }
    let step = format!("move {} to the Trash ({})", util::tilde(p, &places.home), util::tilde(&places.trash, &places.home));
    if dry_run {
        return ActionResult { ok: true, message: "Dry run.".into(), steps: vec![step] };
    }
    match move_to_trash(p, &places.trash) {
        Ok(_) => ActionResult { ok: true, message: format!("Moved {} ({}) to the Trash. The space comes back when the Trash is emptied.", item.name, util::bytes(item.bytes)), steps: vec![step] },
        Err(e) => ActionResult { ok: false, message: e, steps: vec![step] },
    }
}

/// Remove worktree: `git -C <repo> worktree remove <path>` for a listed agent worktree whose branch
/// is merged, whose tree is clean and that nobody is inside. Never `--force`.
pub fn remove_worktree(report: &DiskReport, path: &str, places: &Places, dry_run: bool) -> ActionResult {
    let fail = |m: String| ActionResult { ok: false, message: m, steps: vec![] };
    let Some(item) = report.projects.iter().flat_map(|p| p.items.iter()).find(|i| i.path == path && i.kind == DiskKind::Worktree) else {
        return fail("Wake only removes worktrees it listed. Measure again and pick it there.".into());
    };
    let wt = Path::new(path);
    let Some(info) = &item.worktree else { return fail("Wake couldn't read this worktree.".into()) };
    let repo = Path::new(&info.repo);
    if !wt.starts_with(repo.join(".claude/worktrees")) || !wt.is_dir() {
        return fail("That isn't an agent worktree Wake can remove.".into());
    }
    // Everything again, now: the report may be an hour old.
    let branch = worktree_branch(wt);
    let default = default_branch(repo);
    let merged = match (&branch, &default) {
        (Some(b), Some(d)) => is_merged(repo, b, d),
        _ => None,
    };
    if merged != Some(true) {
        return fail(format!(
            "Its branch {} isn't merged into {}, so removing it could lose work. Nothing was removed.",
            branch.as_deref().unwrap_or("?"),
            default.as_deref().unwrap_or("the default branch")
        ));
    }
    match dirty_files(wt) {
        Some(0) => {}
        Some(n) => return fail(format!("{n} {} not committed in it. Nothing was removed.", if n == 1 { "file is" } else { "files are" })),
        None => return fail("Wake couldn't read its git status. Nothing was removed.".into()),
    }
    if let Some((pid, comm)) = process_inside(wt, &places.proc_root, places.uid) {
        return fail(format!("{comm} (PID {pid}) is working inside it. Nothing was removed."));
    }
    let step = format!("git -C {} worktree remove {}", util::shell_quote(&info.repo), util::shell_quote(path));
    if dry_run {
        return ActionResult { ok: true, message: "Dry run.".into(), steps: vec![step] };
    }
    let mut cmd = crate::gitinfo::command(repo);
    cmd.args(["worktree", "remove"]).arg(wt);
    match crate::run::run_with_timeout(cmd, Duration::from_secs(60)) {
        Ok((0, ..)) => ActionResult { ok: true, message: format!("Removed the worktree for {}. Its branch is still there, merged.", branch.as_deref().unwrap_or("its branch")), steps: vec![step] },
        Ok((_, out, err)) => {
            let last = format!("{out}\n{err}").lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("").to_string();
            ActionResult { ok: false, message: format!("git didn't remove it: {}", util::take_chars(&last, 200)), steps: vec![step] }
        }
        Err(e) => ActionResult { ok: false, message: format!("git didn't remove it: {e}"), steps: vec![step] },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::tests::g;
    use crate::testing::scratch;

    fn put(p: &Path, bytes: usize) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, vec![b'x'; bytes]).unwrap();
    }

    fn places(root: &Path) -> Places {
        Places {
            home: root.to_path_buf(),
            claude: root.join(".claude"),
            codex: root.join(".codex"),
            proc_root: root.join("proc"), // empty: nobody is inside anything
            uid: crate::sys::uid(),
            trash: root.join(".local/share/Trash"),
        }
    }

    fn proj(id: &str, root: &Path) -> ProjectRec {
        serde_json::from_value(serde_json::json!({"id": id, "name": id, "roots": [root]})).unwrap()
    }

    fn no_run() -> Box<CommandRunner> {
        Box::new(|_: &[&str], _| None)
    }

    #[test]
    fn walks_count_blocks_once_and_never_follow_symlinks() {
        let s = scratch("disk-walk");
        let s0 = canonical(&s.0);
        let r = s0.join("tree");
        put(&r.join("a/one"), 100_000);
        put(&r.join("a/two"), 50_000);
        fs::hard_link(r.join("a/one"), r.join("a/one-again")).unwrap();
        // A big folder outside, linked from inside: never counted.
        put(&s0.join("outside/big"), 3_000_000);
        std::os::unix::fs::symlink(s0.join("outside"), r.join("link")).unwrap();
        let t = Walker::new(Duration::from_secs(10)).walk(&r);
        let du = |p: &Path| fs::metadata(p).unwrap().blocks() * 512;
        let expect = du(&r) + du(&r.join("a")) + du(&r.join("a/one")) + du(&r.join("a/two")) + fs::symlink_metadata(r.join("link")).unwrap().blocks() * 512;
        assert_eq!(t.bytes, expect);
        assert!(t.bytes < 1_000_000);
        assert!(!t.partial);
        // A deadline in the past stops at once and says so.
        let mut w = Walker::new(Duration::ZERO);
        std::thread::sleep(Duration::from_millis(2));
        for i in 0..600 {
            put(&r.join(format!("many/{i}")), 10);
        }
        assert!(w.walk(&r).partial);
    }

    #[test]
    fn classifies_build_output_worktrees_archives_and_big_folders() {
        let s = scratch("disk-classify");
        let s0 = canonical(&s.0);
        let r = s0.join("Projects/app");
        put(&r.join("Cargo.toml"), 10);
        put(&r.join("target/debug/big"), 3 << 20);
        put(&r.join("target/CACHEDIR.TAG"), 10);
        put(&r.join("web/package.json"), 10);
        put(&r.join("web/node_modules/x/index.js"), 2 << 20);
        put(&r.join("web/dist/app.js"), 2 << 20);
        put(&r.join("web/.gitignore"), 0);
        fs::write(r.join("web/.gitignore"), "/dist\nnode_modules\n").unwrap();
        put(&r.join("site/package.json"), 10);
        put(&r.join("site/build/hand-written.js"), 2 << 20); // not ignored: not build output
        put(&r.join("Archive/v1.zip"), 2 << 20);
        put(&r.join("src/main.rs"), 100);
        put(&r.join("assets/huge.bin"), 0);
        // A worktree with its own build output.
        let wt = r.join(".claude/worktrees/agent-1");
        put(&wt.join("Cargo.toml"), 10);
        put(&wt.join("target/debug/x"), 2 << 20);
        let mut w = Walker::new(Duration::from_secs(20));
        let scan = scan_tree(&mut w, &r);
        let class_of = |p: &Path| scan.found.iter().find(|f| f.path == p).map(|f| f.class);
        assert_eq!(class_of(&r.join("target")), Some(Class::RustTarget));
        assert_eq!(class_of(&r.join("web/node_modules")), Some(Class::NodeModules));
        assert_eq!(class_of(&r.join("web/dist")), Some(Class::WebDist));
        assert_eq!(class_of(&r.join("site/build")), None);
        assert_eq!(class_of(&r.join("Archive")), Some(Class::Archive));
        assert_eq!(class_of(&wt.join("target")), Some(Class::RustTarget));
        assert_eq!(scan.found.iter().find(|f| f.path == wt.join("target")).unwrap().within.as_deref(), Some(wt.as_path()));
        assert_eq!(scan.worktrees.len(), 1);
        // Everything counted once: the total is the plain walk's.
        assert_eq!(scan.total.bytes, Walker::new(Duration::from_secs(20)).walk(&r).bytes);

        let pl = places(&s0);
        let pd = measure_project(&proj("app", &r), &[], &[], &pl, Duration::from_secs(20));
        let item = |p: &Path| pd.items.iter().find(|i| i.path == p.display().to_string()).unwrap().clone();
        let t = item(&r.join("target"));
        assert_eq!((t.kind, t.safety, t.action), (DiskKind::BuildOutput, DiskSafety::Safe, Some(DiskAction::Clear)));
        assert_eq!(t.name, "Rust build output");
        assert!(t.cost.starts_with("Rebuilds on the next build"));
        let a = item(&r.join("Archive"));
        assert_eq!((a.kind, a.safety, a.action), (DiskKind::Archive, DiskSafety::Keep, Some(DiskAction::OpenFolder)));
        let inner = item(&wt.join("target"));
        assert_eq!(inner.within.as_deref(), Some(wt.display().to_string().as_str()));
        assert!(inner.name.ends_with("in worktree agent-1"), "{}", inner.name);
        // The worktree comes right before its own build output.
        let wi = pd.items.iter().position(|i| i.kind == DiskKind::Worktree).unwrap();
        assert_eq!(pd.items[wi + 1].path, inner.path);
        assert_eq!(pd.rebuildable, t.bytes + item(&r.join("web/node_modules")).bytes + item(&r.join("web/dist")).bytes + inner.bytes);
        assert!(pd.bytes >= pd.rebuildable);
    }

    #[test]
    fn worktrees_say_merged_or_not_and_who_made_them() {
        let s = scratch("disk-wt");
        let s0 = canonical(&s.0);
        let repo = s0.join("Projects/r");
        fs::create_dir_all(&repo).unwrap();
        g(&repo, &["init", "-q", "-b", "main"]);
        g(&repo, &["config", "user.email", "t@t"]);
        g(&repo, &["config", "user.name", "t"]);
        fs::write(repo.join("a.txt"), "a\n").unwrap();
        g(&repo, &["add", "."]);
        g(&repo, &["commit", "-qm", "a"]);
        let merged = repo.join(".claude/worktrees/done");
        let open = repo.join(".claude/worktrees/wip");
        g(&repo, &["worktree", "add", "-q", "-b", "done", merged.to_str().unwrap()]);
        g(&repo, &["worktree", "add", "-q", "-b", "wip", open.to_str().unwrap()]);
        fs::write(open.join("b.txt"), "b\n").unwrap();
        g(&open, &["add", "."]);
        g(&open, &["commit", "-qm", "b"]);
        assert_eq!(worktree_branch(&merged).as_deref(), Some("done"));
        assert_eq!(default_branch(&repo).as_deref(), Some("main"));
        assert_eq!(is_merged(&repo, "done", "main"), Some(true));
        assert_eq!(is_merged(&repo, "wip", "main"), Some(false));

        let pl = places(&s0);
        let maker =
            SessionDisk { id: "s1".into(), title: "tidy the focus card".into(), folders: vec![merged.display().to_string()], started_at: Some("2026-11-20T10:00:00Z".into()), ..Default::default() };
        let pd = measure_project(&proj("r", &repo), &[maker], &[], &pl, Duration::from_secs(20));
        let wt = |p: &Path| pd.items.iter().find(|i| i.path == p.display().to_string()).unwrap().clone();
        let d = wt(&merged);
        assert_eq!(d.name, "Agent worktree for branch done (merged)");
        assert_eq!(d.action, Some(DiskAction::RemoveWorktree));
        assert_eq!(d.refused, None);
        assert_eq!(d.worktree.as_ref().unwrap().made_by_title.as_deref(), Some("tidy the focus card"));
        let o = wt(&open);
        assert_eq!(o.name, "Agent worktree for branch wip (not merged)");
        assert_eq!(o.action, Some(DiskAction::OpenFolder));
        assert!(o.refused.is_some());

        let report = DiskReport { projects: vec![pd.clone()], ..Default::default() };
        // Refused: not merged.
        let r = remove_worktree(&report, &open.display().to_string(), &pl, false);
        assert!(!r.ok && r.message.contains("isn't merged"), "{r:?}");
        assert!(open.exists());
        // Refused: something not committed.
        fs::write(merged.join("new.txt"), "x").unwrap();
        let r = remove_worktree(&report, &merged.display().to_string(), &pl, false);
        assert!(!r.ok && r.message.contains("not committed"), "{r:?}");
        fs::remove_file(merged.join("new.txt")).unwrap();
        // Not listed: refused.
        assert!(!remove_worktree(&report, &repo.display().to_string(), &pl, false).ok);
        // Dry run, then for real: the copy goes, the branch stays.
        let dry = remove_worktree(&report, &merged.display().to_string(), &pl, true);
        assert!(dry.ok && dry.steps[0].contains("worktree remove"), "{dry:?}");
        assert!(merged.exists());
        let r = remove_worktree(&report, &merged.display().to_string(), &pl, false);
        assert!(r.ok, "{r:?}");
        assert!(!merged.exists());
        assert!(g(&repo, &["branch", "--list", "done"]).contains("done"));
    }

    #[test]
    fn clear_moves_to_the_trash_and_refuses_what_it_must() {
        let s = scratch("disk-clear");
        let s0 = canonical(&s.0);
        let r = s0.join("Projects/app");
        put(&r.join("Cargo.toml"), 10);
        put(&r.join("target/debug/big"), 2 << 20);
        put(&r.join("Archive/v1.zip"), 2 << 20);
        let pl = places(&s0);
        let pd = measure_project(&proj("app", &r), &[], &[], &pl, Duration::from_secs(20));
        let report = DiskReport { projects: vec![pd], ..Default::default() };
        let target = r.join("target").display().to_string();
        // An agent at work in the project: refused, nothing moved.
        let x = clear(&report, &target, Some("app"), &pl, false);
        assert!(!x.ok && x.message.contains("working"), "{x:?}");
        assert!(r.join("target").exists());
        // Archives and anything not listed: refused.
        assert!(!clear(&report, &r.join("Archive").display().to_string(), None, &pl, false).ok);
        assert!(!clear(&report, &r.join("src").display().to_string(), None, &pl, false).ok);
        // Dry run moves nothing.
        assert!(clear(&report, &target, None, &pl, true).ok);
        assert!(r.join("target").exists());
        // For real: into the Trash, with its info file.
        let x = clear(&report, &target, None, &pl, false);
        assert!(x.ok, "{x:?}");
        assert!(!r.join("target").exists());
        assert!(pl.trash.join("files/target/debug/big").exists());
        let info = fs::read_to_string(pl.trash.join("info/target.trashinfo")).unwrap();
        assert!(info.starts_with("[Trash Info]\nPath=/"), "{info}");
        assert!(info.contains("/Projects/app/target\nDeletionDate=20"), "{info}");
        // A second one with the same name gets its own.
        put(&r.join("target/debug/big"), 10);
        assert!(clear(&report, &target, None, &pl, false).ok);
        assert!(pl.trash.join("files/target.2").exists());
        assert!(pl.trash.join("info/target.2.trashinfo").exists());
        // A symlink in its place: refused, the link stays.
        std::os::unix::fs::symlink(s0.join("elsewhere"), r.join("target")).unwrap();
        assert!(!clear(&report, &target, None, &pl, false).ok);
        assert!(r.join("target").symlink_metadata().is_ok());
        // Never inside ~/.claude, whatever a report says.
        let conv = s0.join(".claude/projects/x");
        put(&conv.join("a.jsonl"), 10);
        let mut fake = report.clone();
        let bogus = DiskItem { path: conv.display().to_string(), kind: DiskKind::Cache, action: Some(DiskAction::Clear), ..fake.projects[0].items[0].clone() };
        fake.projects[0].items.push(bogus);
        assert!(!clear(&fake, &conv.display().to_string(), None, &pl, false).ok);
        assert!(conv.exists());
    }

    #[test]
    fn conversations_and_job_folders_go_to_their_sessions_project() {
        let s = scratch("disk-conv");
        let s0 = canonical(&s.0);
        let pl = places(&s0);
        let r = s0.join("Projects/app");
        put(&r.join("README"), 10);
        // A background job launched in ~: its transcript sits under the home folder's directory.
        let t = pl.claude.join("projects/-home/sid-1.jsonl");
        put(&t, 2 << 20);
        put(&pl.claude.join("projects/-home/sid-1/subagents/a.jsonl"), 1 << 20);
        put(&pl.claude.join("jobs/sid1job/state.json"), 0);
        fs::write(pl.claude.join("jobs/sid1job/state.json"), r#"{"sessionId":"sid-1"}"#).unwrap();
        put(&pl.claude.join("jobs/sid1job/tmp/brief.md"), 2 << 20);
        let sessions = vec![
            SessionDisk { id: "sid-1".into(), project: Some("app".into()), transcript: Some(t.clone()), ..Default::default() },
            SessionDisk { id: "sid-2".into(), project: None, transcript: Some(t.clone()), ..Default::default() },
        ];
        let rep = measure(&[proj("app", &r)], None, &sessions, &[], &pl, &*no_run(), None, &|_| {});
        let pd = rep.project("app").unwrap();
        let conv = pd.items.iter().find(|i| i.kind == DiskKind::Transcripts).unwrap();
        assert!(conv.bytes >= 3 << 20, "{}", conv.bytes);
        assert_eq!(conv.action, None);
        assert_eq!(conv.safety, DiskSafety::Keep);
        let jobs = pd.items.iter().find(|i| i.kind == DiskKind::JobFiles).unwrap();
        assert_eq!(jobs.command.as_deref(), Some("ls -la ~/.claude/jobs/sid1job/tmp"));
        // The session with no project lands on the machine's list.
        let m = rep.machine.as_ref().unwrap();
        assert!(m.items.iter().any(|i| i.kind == DiskKind::Transcripts && i.name.contains("with no project")));
        assert!(rep.measured_at.is_some() && rep.measuring.is_none());
    }

    #[test]
    fn caches_named_for_a_project_go_to_it() {
        let projects = vec![proj("Skyhook", Path::new("/x")), proj("tideline-desktop", Path::new("/y")), proj("wake", Path::new("/z"))];
        assert_eq!(cache_project("skyhook-probe", &projects).as_deref(), Some("Skyhook"));
        assert_eq!(cache_project("tideline-smoke", &projects).as_deref(), Some("tideline-desktop"));
        assert_eq!(cache_project("wake-shots", &projects).as_deref(), Some("wake"));
        assert_eq!(cache_project("wakeup", &projects), None);
        assert_eq!(cache_project("mozilla", &projects), None);
    }

    #[test]
    fn podman_df_parses() {
        let out = "TYPE           TOTAL       ACTIVE      SIZE        RECLAIMABLE\nImages         12          3           6.6GB       1.5GB (23%)\nContainers     6           1           6.8GB       3.7GB (54%)\nLocal Volumes  4           2           2.3GB       10.5MB (0%)\n";
        let v = parse_podman_df(out);
        assert_eq!(v.len(), 3);
        assert_eq!(v[0], ("Images".into(), 6_600_000_000, 1_500_000_000));
        assert_eq!(v[2].0, "Volumes");
        assert_eq!(v[2].2, 10_500_000);
    }

    #[test]
    fn a_process_in_a_container_is_found_through_its_own_view_of_the_folder() {
        // The host keeps homes in var/home with a home link to it (as on Bazzite); a build in a
        // distrobox reports its folder through the link.
        let s0 = scratch("disk-inside");
        let r = fs::canonicalize(&s0.0).unwrap();
        fs::create_dir_all(r.join("var/home/u/proj/target")).unwrap();
        std::os::unix::fs::symlink(r.join("var/home"), r.join("home")).unwrap();
        let p = r.join("proc/4242");
        fs::create_dir_all(&p).unwrap();
        fs::write(p.join("comm"), "cargo\n").unwrap();
        std::os::unix::fs::symlink(r.join("home/u/proj"), p.join("cwd")).unwrap();
        let (uid, procs) = (crate::sys::uid(), r.join("proc"));
        assert_eq!(process_inside(&r.join("var/home/u/proj"), &procs, uid), Some((4242, "cargo".into())));
        assert_eq!(process_inside(&r.join("home/u/proj"), &procs, uid), Some((4242, "cargo".into())));
        assert_eq!(process_inside(&r.join("var/home/u/proj/target"), &procs, uid), None, "a folder below the process is not in use");
        // A folder only the process's own root has: found through /proc/<pid>/root.
        fs::create_dir_all(r.join("ctr/rootfs/work")).unwrap();
        fs::create_dir_all(r.join("proc/4343")).unwrap();
        fs::write(r.join("proc/4343/comm"), "make\n").unwrap();
        std::os::unix::fs::symlink("/work", r.join("proc/4343/cwd")).unwrap();
        std::os::unix::fs::symlink(r.join("ctr/rootfs"), r.join("proc/4343/root")).unwrap();
        assert_eq!(process_inside(&r.join("ctr/rootfs/work"), &procs, uid), Some((4343, "make".into())));
    }
}
