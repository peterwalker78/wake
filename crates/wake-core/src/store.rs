//! Paths, private directories, atomic JSON writes and Wake's own persistence.
//!
//! Wake writes only under its data directory (default ~/.local/share/wake, mode 0700); every write is
//! checked against it. Everything else (~/.claude, ~/.codex, project folders) is read only. The files
//! keep the Python version's formats so its data carries over: projects.json, ideas.jsonl,
//! pauses.jsonl. The index lives in `index-v2/`.
//!
//! Environment overrides (as in Python): WAKE_DATA_DIR, WAKE_CLAUDE_DIR, WAKE_CODEX_DIR,
//! WAKE_PROJECTS_DIR, WAKE_RUNTIME_DIR; and, as the agents themselves do, CLAUDE_CONFIG_DIR and
//! CODEX_HOME. Tests build a [`Paths`] directly instead.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::types::IdeaState;
use crate::util;

/// Where everything is. `Paths::from_env()` for the real machine.
#[derive(Debug, Clone)]
pub struct Paths {
    pub home: PathBuf,
    pub data: PathBuf,
    pub claude: PathBuf,
    pub codex: PathBuf,
    pub projects: PathBuf,
    /// Slipstream's runtime folder holding agent-notify notes (working/<pid>, resume/<pid>).
    pub runtime: PathBuf,
    /// Slipstream's state folder for Wake's plugin (request.json lives in it).
    pub plugin_state: PathBuf,
    /// Wake's settings folder (settings.toml): $WAKE_CONFIG_DIR, else `<config_home>/wake`.
    pub config: PathBuf,
    /// The desktop's data folder: $XDG_DATA_HOME, else ~/.local/share.
    pub data_home: PathBuf,
    /// The desktop's settings folder: $XDG_CONFIG_HOME, else ~/.config.
    pub config_home: PathBuf,
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// An XDG base folder: `$var` when it is an absolute path (the spec says to ignore any other),
/// else `fallback`.
fn xdg(var: &str, fallback: PathBuf) -> PathBuf {
    env_path(var).filter(|p| p.is_absolute()).unwrap_or(fallback)
}

pub fn home_dir() -> PathBuf {
    env_path("HOME").unwrap_or_else(|| PathBuf::from("/"))
}

impl Paths {
    pub fn from_env() -> Paths {
        let home = home_dir();
        let runtime = env_path("WAKE_RUNTIME_DIR").unwrap_or_else(|| xdg("XDG_RUNTIME_DIR", PathBuf::from(format!("/run/user/{}", crate::sys::uid()))).join("slipstream"));
        let config_home = xdg("XDG_CONFIG_HOME", home.join(".config"));
        Paths {
            data: env_path("WAKE_DATA_DIR").unwrap_or_else(|| home.join(".local/share/wake")),
            // Claude Code and Codex move their folders with these, so Wake follows them.
            claude: env_path("WAKE_CLAUDE_DIR").or_else(|| env_path("CLAUDE_CONFIG_DIR")).unwrap_or_else(|| home.join(".claude")),
            codex: env_path("WAKE_CODEX_DIR").or_else(|| env_path("CODEX_HOME")).unwrap_or_else(|| home.join(".codex")),
            projects: env_path("WAKE_PROJECTS_DIR").unwrap_or_else(|| home.join("Projects")),
            plugin_state: home.join(".local/state/slipstream/plugins/wake/state"),
            config: env_path("WAKE_CONFIG_DIR").unwrap_or_else(|| config_home.join("wake")),
            data_home: xdg("XDG_DATA_HOME", home.join(".local/share")),
            config_home,
            runtime,
            home,
        }
    }

    /// Everything under one root, for tests: home/, data/, claude/, codex/, run/slipstream.
    pub fn under(root: &Path) -> Paths {
        let home = root.join("home");
        Paths {
            data: root.join("data"),
            claude: root.join("claude"),
            codex: root.join("codex"),
            projects: home.join("Projects"),
            runtime: root.join("run/slipstream"),
            plugin_state: root.join("plugin-state"),
            config: root.join("config"),
            data_home: home.join(".local/share"),
            config_home: home.join(".config"),
            home,
        }
    }

    pub fn index_dir(&self) -> PathBuf {
        self.data.join("index-v2")
    }

    /// The Python version's index, read once to carry over summaries of deleted transcripts.
    pub fn old_index_dir(&self) -> PathBuf {
        self.data.join("index")
    }
}

// ------------------------------------------------------------------ guards

/// True for files Wake must never open (*.key, .env, .env.*, credentials*, *.pem).
pub fn is_forbidden(path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    name.ends_with(".key") || name == ".env" || name.starts_with(".env.") || name.starts_with("credentials") || name.ends_with(".pem")
}

/// Lexically normalise (no filesystem access): drop `.`, fold `..`.
pub fn lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Python's os.path.realpath: resolve symlinks of the part that exists, keep the rest as written.
pub fn realpath(p: &Path) -> PathBuf {
    let p = if p.is_absolute() { lexical(p) } else { lexical(&std::env::current_dir().unwrap_or_default().join(p)) };
    if let Ok(r) = fs::canonicalize(&p) {
        return r;
    }
    let mut tail = Vec::new();
    let mut cur = p.clone();
    loop {
        match cur.file_name() {
            Some(n) => tail.push(n.to_os_string()),
            None => return p,
        }
        if !cur.pop() {
            return p;
        }
        if let Ok(mut r) = fs::canonicalize(&cur) {
            for n in tail.iter().rev() {
                r.push(n);
            }
            return r;
        }
    }
}

fn inside(path: &Path, parent: &Path) -> bool {
    realpath(path).starts_with(realpath(parent))
}

#[derive(Debug)]
pub struct StoreError(pub String);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StoreError {}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError(e.to_string())
    }
}

pub type SResult<T> = Result<T, StoreError>;

/// Writes are only ever made through this, which refuses anything outside the data folder.
#[derive(Debug, Clone)]
pub struct Writer {
    data: PathBuf,
}

static APPEND_LOCK: Mutex<()> = Mutex::new(());

impl Writer {
    pub fn new(data: &Path) -> Writer {
        Writer { data: data.to_path_buf() }
    }

    fn check(&self, path: &Path) -> SResult<()> {
        if inside(path, &self.data) { Ok(()) } else { Err(StoreError(format!("Wake only writes inside {}, not {}", self.data.display(), path.display()))) }
    }

    pub fn ensure_dir(&self, path: &Path) -> SResult<()> {
        self.check(path)?;
        fs::DirBuilder::new().recursive(true).mode(0o700).create(path)?;
        Ok(())
    }

    pub fn ensure_data_dir(&self) -> SResult<()> {
        self.ensure_dir(&self.data)?;
        let _ = fs::set_permissions(&self.data, fs::Permissions::from_mode(0o700));
        Ok(())
    }

    pub fn write_atomic(&self, path: &Path, bytes: &[u8]) -> SResult<()> {
        self.check(path)?;
        let dir = path.parent().ok_or_else(|| StoreError("no folder".into()))?;
        self.ensure_dir(dir)?;
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = dir.join(format!(".tmp-{}-{}", std::process::id(), n));
        let result = (|| {
            let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
            f.write_all(bytes)?;
            f.sync_data().ok();
            fs::rename(&tmp, path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result.map_err(Into::into)
    }

    pub fn write_json<T: Serialize + ?Sized>(&self, path: &Path, v: &T) -> SResult<()> {
        let bytes = serde_json::to_vec(v).map_err(|e| StoreError(e.to_string()))?;
        self.write_atomic(path, &bytes)
    }

    pub fn append_jsonl<T: Serialize + ?Sized>(&self, path: &Path, v: &T) -> SResult<()> {
        self.check(path)?;
        if let Some(d) = path.parent() {
            self.ensure_dir(d)?;
        }
        let mut line = serde_json::to_vec(v).map_err(|e| StoreError(e.to_string()))?;
        line.push(b'\n');
        let _g = APPEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut f = fs::OpenOptions::new().append(true).create(true).mode(0o600).open(path)?;
        f.write_all(&line)?;
        Ok(())
    }
}

/// The largest JSON file Wake reads.
pub const JSON_CAP: u64 = 64 << 20;
/// The largest single line of a transcript Wake reads; a longer one is skipped.
pub const LINE_CAP: usize = 64 << 20;

/// Read a JSON file; None if missing, invalid, not a regular file or over [`JSON_CAP`]. Refuses
/// forbidden names.
pub fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    read_json_capped(path, JSON_CAP, false)
}

/// [`read_json`] with its own size cap; `no_follow` also refuses a symlink (for a file another
/// program writes, such as the plugin's request). Never waits on a pipe.
pub fn read_json_capped<T: for<'de> Deserialize<'de>>(path: &Path, cap: u64, no_follow: bool) -> Option<T> {
    use std::io::Read;
    if is_forbidden(path) {
        return None;
    }
    let flags = libc::O_NONBLOCK | if no_follow { libc::O_NOFOLLOW } else { 0 };
    let f = fs::OpenOptions::new().read(true).custom_flags(flags).open(path).ok()?;
    if !f.metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    f.take(cap + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > cap {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

/// The next line of `r` into `buf` (newline included), keeping at most `cap` bytes: a longer line
/// is read past without being held whole. (bytes read, ended with a newline, was too long).
pub fn read_line_capped(r: &mut impl BufRead, buf: &mut Vec<u8>, cap: usize) -> std::io::Result<(usize, bool, bool)> {
    let (mut n, mut over) = (0, false);
    loop {
        let avail = match r.fill_buf() {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if avail.is_empty() {
            return Ok((n, false, over));
        }
        let (len, ended) = memchr::memchr(b'\n', avail).map_or((avail.len(), false), |i| (i + 1, true));
        if !over && buf.len() + len > cap {
            over = true;
            *buf = Vec::new();
        }
        if !over {
            buf.extend_from_slice(&avail[..len]);
        }
        r.consume(len);
        n += len;
        if ended {
            return Ok((n, true, over));
        }
    }
}

/// Each parsable JSON line of a file, streamed (a line over [`LINE_CAP`] is skipped).
pub fn iter_jsonl(path: &Path) -> impl Iterator<Item = Value> {
    let mut reader = if is_forbidden(path) { None } else { fs::File::open(path).ok().map(BufReader::new) };
    let mut buf = Vec::new();
    std::iter::from_fn(move || {
        let r = reader.as_mut()?;
        loop {
            buf.clear();
            match read_line_capped(r, &mut buf, LINE_CAP) {
                Ok((0, _, _)) | Err(_) => return None,
                Ok((_, _, true)) => continue,
                Ok(_) => {}
            }
            let t = buf.trim_ascii();
            if !t.is_empty()
                && let Ok(v) = serde_json::from_slice(t)
            {
                return Some(v);
            }
        }
    })
}

// ------------------------------------------------------------------ path helpers

const WT_MARK: &str = "/.claude/worktrees/";

/// Fold `<repo>/.claude/worktrees/<name>[/…]` back to `<repo>` (the first such marker).
pub fn fold_worktree(p: &str) -> &str {
    match p.find(WT_MARK) {
        Some(i) if p.len() > i + WT_MARK.len() && !p[i + WT_MARK.len()..].starts_with('/') => &p[..i],
        _ => p,
    }
}

fn expand_user(p: &str, home: &Path) -> PathBuf {
    if p == "~" {
        home.to_path_buf()
    } else if let Some(rest) = p.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(p)
    }
}

static NORM_CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

/// Resolve symlinks (/home -> /var/home) and fold worktrees; `~` means `home` (pass `Paths.home`).
/// Cached; None for an empty path.
pub fn norm_path(p: &str, home: &Path) -> Option<String> {
    if p.is_empty() {
        return None;
    }
    // A path that starts with ~ depends on whose home it is, so that is part of the key.
    let key = if p.starts_with('~') { format!("{}\0{p}", home.display()) } else { p.to_string() };
    let cache = NORM_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(hit) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        return Some(hit.clone());
    }
    let real = realpath(&expand_user(p, home));
    let s = real.to_string_lossy();
    let out = fold_worktree(&s).to_string();
    let mut c = cache.lock().unwrap_or_else(|e| e.into_inner());
    if c.len() > 8192 {
        c.clear();
    }
    c.insert(key, out.clone());
    Some(out)
}

/// [`norm_path`] for paths that are already absolute, as every transcript `cwd` is. A `~` is left
/// as written rather than guessed from the environment.
pub fn norm_abs(p: &str) -> Option<String> {
    if p.starts_with('~') {
        return Some(p.to_string());
    }
    norm_path(p, Path::new("/"))
}

pub fn slug(name: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in name.to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            if dash && !out.is_empty() {
                out.push('-');
            }
            dash = false;
            out.push(c);
        } else {
            dash = true;
        }
    }
    if out.is_empty() { "project".into() } else { out }
}

// ------------------------------------------------------------------ records

/// The dot colours a project can have, by name (the app draws each in the current theme).
/// Fourteen, as far apart as they go (hues round the wheel, lightness varied so they stay apart for
/// colour-blind eyes too), none near amber, which means waiting.
pub const COLOUR_NAMES: [&str; 14] = ["red", "orange", "wine", "pink", "magenta", "purple", "indigo", "blue", "sky", "teal", "mint", "green", "lime", "slate"];

/// A colour name as Wake knows it now: one of `COLOUR_NAMES`, or an older name it replaced
/// (violet is purple now). None for anything else.
pub fn colour_name(c: &str) -> Option<&'static str> {
    let c = match c {
        "violet" => "purple",
        c => c,
    };
    COLOUR_NAMES.iter().copied().find(|n| *n == c)
}

/// A project's colour when none was picked: the same one every time for the same name.
pub fn default_colour(name: &str) -> &'static str {
    // FNV-1a: stable across runs and versions, unlike std's hasher.
    let mut h: u32 = 0x811c_9dc5;
    for b in name.to_lowercase().bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    // A final mix (MurmurHash3's) so similar names spread over the colours.
    h ^= h >> 16;
    h = h.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2_ae35);
    h ^= h >> 16;
    COLOUR_NAMES[(h % COLOUR_NAMES.len() as u32) as usize]
}

/// A project's importance, besides normal (None).
pub const IMPORTANCE: [&str; 2] = ["high", "low"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectRec {
    pub id: String,
    pub name: String,
    #[serde(default, deserialize_with = "null_default")]
    pub roots: Vec<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub desc: String,
    #[serde(default, deserialize_with = "null_default")]
    pub next: String,
    #[serde(default, deserialize_with = "null_default")]
    pub why: String,
    #[serde(default, deserialize_with = "null_default")]
    pub removed: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub added: String,
    /// The project's dot colour, a swatch name (see `colour_names`); None = picked from the name.
    /// Added 2026-11-20: older files have no such field, and it is left out while unset.
    #[serde(default, deserialize_with = "null_default", skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// The group the person put it in, if any. Added 2026-11-25; left out while unset.
    #[serde(default, deserialize_with = "null_default", skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// "high" or "low"; None is normal. Added 2026-11-25; left out while unset.
    #[serde(default, deserialize_with = "null_default", skip_serializing_if = "Option::is_none")]
    pub importance: Option<String>,
    /// Anything else a later version wrote, kept as is.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A `null` where a value belongs reads as the empty default (a hand edit, or another tool, may write
/// `"desc": null`), instead of making the whole projects.json unreadable.
fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IdeaRec {
    pub id: String,
    pub at: String,
    pub text: String,
    pub project: Option<String>,
    pub source: String,
    pub cwd: Option<String>,
    pub state: IdeaState,
    /// When it was sent to an agent (state "sent"), from its state record. Not in the idea's
    /// own record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent_at: Option<String>,
    /// The brief it was sent in (`<data>/briefs/<id>.md`), from its "sent" record; None for ideas
    /// sent before Wake kept it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brief: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PauseRec {
    pub project: String,
    pub at: String,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub next: String,
    /// repo path -> mark; a JSON object, kept in file order.
    #[serde(default)]
    pub git: Map<String, Value>,
    #[serde(default)]
    pub sessions: Vec<String>,
}

pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    let ok = fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf));
    if ok.is_err() {
        let t = util::now().to_bits() ^ ((std::process::id() as u64) << 32);
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (t >> ((i % 8) * 8)) as u8 ^ (i as u8).wrapping_mul(31);
        }
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

// ------------------------------------------------------------------ the store

#[derive(Deserialize)]
struct ProjectsFile {
    projects: Vec<ProjectRec>,
    /// The person's groups, including those with no project in them yet. Added 2026-11-27; older
    /// files have none, and their groups are the ones their projects name.
    #[serde(default, deserialize_with = "null_default")]
    groups: Vec<String>,
}

/// The groups kept on their own, in their order, then those only a tracked project names, A to Z.
fn all_groups(mut kept: Vec<String>, projects: &[ProjectRec]) -> Vec<String> {
    let mut extra: Vec<String> = Vec::new();
    for g in projects.iter().filter(|p| !p.removed).filter_map(|p| p.group.as_ref()) {
        if !kept.iter().chain(extra.iter()).any(|x| x.eq_ignore_ascii_case(g)) {
            extra.push(g.clone());
        }
    }
    extra.sort_by_key(|g| g.to_lowercase());
    kept.extend(extra);
    kept
}

/// A group's name as kept: trimmed, at most 40 characters; None if nothing is left.
fn group_name(g: &str) -> Option<String> {
    let g = g.trim();
    (!g.is_empty()).then(|| g.chars().take(40).collect())
}

/// Projects, ideas and pauses. Cheap to clone; serialises its own writes.
#[derive(Debug, Clone)]
pub struct Store {
    pub paths: Paths,
    pub writer: Writer,
    /// Why projects.json could not be used, in plain words, once that has happened.
    problem: std::sync::Arc<Mutex<Option<String>>>,
}

static PROJECTS_LOCK: Mutex<()> = Mutex::new(());

fn is_repo(p: &Path) -> bool {
    p.join(".git").exists()
}

fn listdir(p: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(p).map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    v.sort();
    v
}

impl Store {
    pub fn new(paths: Paths) -> Store {
        let writer = Writer::new(&paths.data);
        Store { paths, writer, problem: Default::default() }
    }

    pub fn ensure_data_dir(&self) -> SResult<()> {
        self.writer.ensure_data_dir()?;
        self.writer.ensure_dir(&self.paths.index_dir().join("sessions"))
    }

    fn projects_path(&self) -> PathBuf {
        self.paths.data.join("projects.json")
    }
    fn ideas_path(&self) -> PathBuf {
        self.paths.data.join("ideas.jsonl")
    }
    fn pauses_path(&self) -> PathBuf {
        self.paths.data.join("pauses.jsonl")
    }

    /// The default project list from ~/Projects/* and ~/Projects/*/* git repos.
    pub fn discover_projects(&self) -> Vec<ProjectRec> {
        let base = &self.paths.projects;
        let mut found: Vec<(String, Vec<String>)> = Vec::new();
        for name in listdir(base) {
            if name.starts_with('.') || name == "archive" || name.ends_with("-deploy") {
                continue;
            }
            let p = base.join(&name);
            if !p.is_dir() {
                continue;
            }
            let root = realpath(&p).to_string_lossy().into_owned();
            if is_repo(&p) {
                found.push((name, vec![root]));
                continue;
            }
            let kids = listdir(&p);
            if kids.iter().any(|k| !k.starts_with('.') && is_repo(&p.join(k))) {
                found.push((name, vec![root]));
            }
        }
        // foo-private folds into foo
        let names: Vec<String> = found.iter().map(|(n, _)| n.clone()).collect();
        for name in names {
            if let Some(base_name) = name.strip_suffix("-private")
                && let Some(bi) = found.iter().position(|(n, _)| n == base_name)
                && let Some(pi) = found.iter().position(|(n, _)| *n == name)
            {
                let roots = found.remove(pi).1;
                let bi = if pi < bi { bi - 1 } else { bi };
                found[bi].1.extend(roots);
            }
        }
        let at = util::now_iso();
        let mut used: Vec<String> = Vec::new();
        found
            .into_iter()
            .map(|(name, roots)| {
                let mut id = slug(&name);
                while used.contains(&id) {
                    id.push_str("-2");
                }
                used.push(id.clone());
                ProjectRec {
                    id,
                    name,
                    roots,
                    desc: String::new(),
                    next: String::new(),
                    why: String::new(),
                    removed: false,
                    added: at.clone(),
                    color: None,
                    group: None,
                    importance: None,
                    extra: Map::new(),
                }
            })
            .collect()
    }

    /// Only when the project list is first made: git repos agents have worked in outside
    /// ~/Projects (the nearest folder with `.git` above where a session ran, under home but never
    /// home itself, nor a hidden folder, nor a `*-deploy` copy).
    fn add_session_repos(&self, projects: &mut Vec<ProjectRec>) {
        let home = realpath(&self.paths.home);
        let under_projects = realpath(&self.paths.projects);
        let at = util::now_iso();
        for cwd in session_cwds(&self.paths) {
            let cwd = realpath(Path::new(fold_worktree(&cwd)));
            let mut d = cwd.as_path();
            let root = loop {
                if d == home || !d.starts_with(&home) {
                    break None;
                }
                if is_repo(d) {
                    break Some(d.to_path_buf());
                }
                match d.parent() {
                    Some(p) => d = p,
                    None => break None,
                }
            };
            let Some(root) = root else { continue };
            let hidden = root.strip_prefix(&home).is_ok_and(|r| r.components().any(|c| c.as_os_str().to_string_lossy().starts_with('.')));
            let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if hidden || name.is_empty() || name.ends_with("-deploy") || root.starts_with(&under_projects) {
                continue;
            }
            let rs = root.to_string_lossy().into_owned();
            if projects.iter().any(|p| p.roots.iter().any(|r| r == &rs || Path::new(&rs).starts_with(r))) {
                continue;
            }
            let mut id = slug(&name);
            while projects.iter().any(|p| p.id == id) {
                id.push_str("-2");
            }
            projects.push(ProjectRec {
                id,
                name,
                roots: vec![rs],
                desc: String::new(),
                next: String::new(),
                why: String::new(),
                removed: false,
                added: at.clone(),
                color: None,
                group: None,
                importance: None,
                extra: Map::new(),
            });
        }
    }

    pub fn load_projects(&self) -> Vec<ProjectRec> {
        let _g = PROJECTS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        self.load_projects_locked()
    }

    /// A plain sentence when projects.json could not be used (it is then kept aside, never
    /// overwritten), else None. The app shows it with the overview.
    pub fn projects_problem(&self) -> Option<String> {
        self.problem.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set_problem(&self, p: String) {
        *self.problem.lock().unwrap_or_else(|e| e.into_inner()) = Some(p);
    }

    /// The project list. Only a *missing* projects.json is created from ~/Projects. One that can't
    /// be read is left alone (and the list comes from ~/Projects for now, unsaved); one that can't
    /// be understood is moved aside as `projects.json.broken-<time>` first, so nothing the person
    /// wrote in it is lost.
    fn load_projects_locked(&self) -> Vec<ProjectRec> {
        let path = self.projects_path();
        let bytes = match fs::File::open(&path) {
            Ok(f) => {
                let mut buf = Vec::new();
                match std::io::Read::read_to_end(&mut std::io::Read::take(f, 64 * 1024 * 1024), &mut buf) {
                    Ok(_) => buf,
                    Err(e) => {
                        self.set_problem(format!(
                            "Wake couldn't read its project list {}, so it left the file alone and is showing the folders in {} for now. ({e})",
                            util::tilde(&path, &self.paths.home),
                            util::tilde(&self.paths.projects, &self.paths.home)
                        ));
                        return self.discover_projects();
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut projects = self.discover_projects();
                self.add_session_repos(&mut projects);
                if self.ensure_data_dir().is_ok() {
                    let _ = self.save_projects_locked(&projects);
                }
                return projects;
            }
            Err(e) => {
                self.set_problem(format!(
                    "Wake couldn't open its project list {}, so it left the file alone and is showing the folders in {} for now. ({e})",
                    util::tilde(&path, &self.paths.home),
                    util::tilde(&self.paths.projects, &self.paths.home)
                ));
                return self.discover_projects();
            }
        };
        match serde_json::from_slice::<ProjectsFile>(&bytes) {
            Ok(f) => f.projects,
            Err(e) => {
                let stamp = util::now_iso().replace(':', "-");
                let aside = (0..100)
                    .map(|n| self.paths.data.join(if n == 0 { format!("projects.json.broken-{stamp}") } else { format!("projects.json.broken-{stamp}-{n}") }))
                    .find(|p| !p.exists())
                    .unwrap_or_else(|| self.paths.data.join(format!("projects.json.broken-{stamp}-{}", random_hex(4))));
                // hard_link + remove never replaces an existing file, unlike rename
                let moved = self.writer.check(&aside).is_ok() && fs::hard_link(&path, &aside).is_ok() && fs::remove_file(&path).is_ok();
                let projects = self.discover_projects();
                if moved {
                    self.set_problem(format!(
                        "Wake's project list was damaged, so it started a new one from the folders in {}. Your old list, with your descriptions and next steps, is kept as {}. ({e})",
                        util::tilde(&self.paths.projects, &self.paths.home),
                        util::tilde(&aside, &self.paths.home)
                    ));
                    let _ = self.save_projects_locked(&projects);
                } else {
                    self.set_problem(format!(
                        "Wake's project list was damaged and couldn't be moved aside, so Wake left {} as it is and isn't saving project changes. ({e})",
                        util::tilde(&path, &self.paths.home)
                    ));
                }
                projects
            }
        }
    }

    fn save_projects_locked(&self, projects: &[ProjectRec]) -> SResult<()> {
        let groups = self.load_groups_locked();
        self.save_file_locked(projects, &groups)
    }

    /// The groups kept in projects.json (not those only named by a project). Empty when the file
    /// is missing or can't be read.
    fn load_groups_locked(&self) -> Vec<String> {
        fs::read(self.projects_path()).ok().and_then(|b| serde_json::from_slice::<ProjectsFile>(&b).ok()).map(|f| f.groups).unwrap_or_default()
    }

    fn save_file_locked(&self, projects: &[ProjectRec], groups: &[String]) -> SResult<()> {
        let path = self.projects_path();
        // Never replace a file Wake could not understand: it may hold the person's own words.
        if path.exists() && fs::read(&path).ok().is_none_or(|b| serde_json::from_slice::<ProjectsFile>(&b).is_err()) {
            return Err(StoreError(format!("{} could not be understood, so Wake will not overwrite it.", path.display())));
        }
        if groups.is_empty() {
            self.writer.write_json(&path, &serde_json::json!({ "projects": projects }))
        } else {
            self.writer.write_json(&path, &serde_json::json!({ "projects": projects, "groups": groups }))
        }
    }

    /// The person's groups, in their order: those kept on their own as the person arranged them,
    /// then any only a tracked project names (from files written before groups were kept), A to Z.
    pub fn groups(&self) -> Vec<String> {
        let _g = PROJECTS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        all_groups(self.load_groups_locked(), &self.load_projects_locked())
    }

    /// Puts the groups in this order. Names not in `order` keep their place after it; names that
    /// are no group are left out.
    pub fn order_groups(&self, order: &[String]) -> SResult<()> {
        let _g = PROJECTS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let projects = self.load_projects_locked();
        let known = all_groups(self.load_groups_locked(), &projects);
        let mut out: Vec<String> = Vec::new();
        for g in order.iter().chain(known.iter()) {
            if known.contains(g) && !out.contains(g) {
                out.push(g.clone());
            }
        }
        self.save_file_locked(&projects, &out)
    }

    /// Adds a group with no projects in it yet. A name already in use (in any case) is that group.
    pub fn add_group(&self, name: &str) -> SResult<String> {
        let name = group_name(name).ok_or_else(|| StoreError("A group needs a name.".into()))?;
        let _g = PROJECTS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let projects = self.load_projects_locked();
        let mut groups = self.load_groups_locked();
        if let Some(same) = groups.iter().chain(projects.iter().filter_map(|p| p.group.as_ref())).find(|g| g.eq_ignore_ascii_case(&name)) {
            let same = same.clone();
            if !groups.contains(&same) {
                groups.push(same.clone());
                self.save_file_locked(&projects, &groups)?;
            }
            return Ok(same);
        }
        groups.push(name.clone());
        self.save_file_locked(&projects, &groups)?;
        Ok(name)
    }

    /// Removes a group. Its projects stay tracked, in no group. Returns how many were in it.
    pub fn remove_group(&self, name: &str) -> SResult<usize> {
        let _g = PROJECTS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut projects = self.load_projects_locked();
        let mut groups = self.load_groups_locked();
        groups.retain(|g| g != name);
        let mut moved = 0;
        for p in projects.iter_mut().filter(|p| p.group.as_deref() == Some(name)) {
            p.group = None;
            moved += usize::from(!p.removed);
        }
        self.save_file_locked(&projects, &groups)?;
        Ok(moved)
    }

    pub fn add_project(&self, path: &str) -> SResult<ProjectRec> {
        let path = realpath(&expand_user(path, &self.paths.home));
        if !path.is_dir() {
            return Err(StoreError("That folder does not exist.".into()));
        }
        let ps = path.to_string_lossy().into_owned();
        let _g = PROJECTS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut projects = self.load_projects_locked();
        for p in projects.iter_mut() {
            if p.roots.iter().any(|r| realpath(Path::new(r)).to_string_lossy() == ps) {
                if p.removed {
                    p.removed = false;
                    let out = p.clone();
                    self.save_projects_locked(&projects)?;
                    return Ok(out);
                }
                return Ok(p.clone());
            }
        }
        let name = path.file_name().map_or_else(|| ps.clone(), |n| n.to_string_lossy().into_owned());
        let mut id = slug(&name);
        while projects.iter().any(|p| p.id == id) {
            id.push_str("-2");
        }
        let proj = ProjectRec {
            id,
            name,
            roots: vec![ps],
            desc: String::new(),
            next: String::new(),
            why: String::new(),
            removed: false,
            added: util::now_iso(),
            color: None,
            group: None,
            importance: None,
            extra: Map::new(),
        };
        projects.push(proj.clone());
        self.save_projects_locked(&projects)?;
        Ok(proj)
    }

    /// Set (or, with None, clear) a project's dot colour. Only the swatch names Wake knows are kept.
    pub fn set_project_color(&self, id: &str, color: Option<&str>) -> SResult<ProjectRec> {
        let color = match color {
            Some(c) => Some(colour_name(c).ok_or_else(|| StoreError(format!("Wake doesn't know the colour {c}.")))?),
            None => None,
        };
        let _g = PROJECTS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut projects = self.load_projects_locked();
        let p = projects.iter_mut().find(|p| p.id == id).ok_or_else(|| StoreError("No project with that id.".into()))?;
        p.color = color.map(String::from);
        let out = p.clone();
        self.save_projects_locked(&projects)?;
        Ok(out)
    }

    /// Put a project in a group (a short name of the person's own), or take it out with None.
    /// The group is kept on its own too, so it stays when its last project leaves it.
    pub fn set_project_group(&self, id: &str, group: Option<&str>) -> SResult<ProjectRec> {
        let group = group.and_then(group_name);
        let _g = PROJECTS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut projects = self.load_projects_locked();
        let mut groups = self.load_groups_locked();
        let p = projects.iter_mut().find(|p| p.id == id).ok_or_else(|| StoreError("No project with that id.".into()))?;
        p.group.clone_from(&group);
        let out = p.clone();
        if let Some(g) = group
            && !groups.contains(&g)
        {
            groups.push(g);
        }
        self.save_file_locked(&projects, &groups)?;
        Ok(out)
    }

    /// Set how much a project matters: "high", "low", or None for normal.
    pub fn set_project_importance(&self, id: &str, importance: Option<&str>) -> SResult<ProjectRec> {
        if let Some(i) = importance.filter(|i| !IMPORTANCE.contains(i)) {
            return Err(StoreError(format!("Wake doesn't know the importance {i}.")));
        }
        let _g = PROJECTS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut projects = self.load_projects_locked();
        let p = projects.iter_mut().find(|p| p.id == id).ok_or_else(|| StoreError("No project with that id.".into()))?;
        p.importance = importance.map(String::from);
        let out = p.clone();
        self.save_projects_locked(&projects)?;
        Ok(out)
    }

    /// Set a project's next step (what to do when you come back), kept in projects.json.
    pub fn set_project_next(&self, id: &str, next: &str) -> SResult<ProjectRec> {
        let _g = PROJECTS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut projects = self.load_projects_locked();
        let p = projects.iter_mut().find(|p| p.id == id).ok_or_else(|| StoreError("No project with that id.".into()))?;
        p.next = next.trim().to_string();
        let out = p.clone();
        self.save_projects_locked(&projects)?;
        Ok(out)
    }

    pub fn set_project_removed(&self, id: &str, removed: bool) -> SResult<ProjectRec> {
        let _g = PROJECTS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut projects = self.load_projects_locked();
        let p = projects.iter_mut().find(|p| p.id == id).ok_or_else(|| StoreError("No project with that id.".into()))?;
        p.removed = removed;
        let out = p.clone();
        self.save_projects_locked(&projects)?;
        Ok(out)
    }

    // ---- ideas

    pub fn add_idea(&self, text: &str, project: Option<&str>, source: &str, cwd: Option<&str>) -> SResult<IdeaRec> {
        let text = text.trim();
        if text.is_empty() {
            return Err(StoreError("An idea needs some text.".into()));
        }
        let source = if ["typed", "shell", "prompt"].contains(&source) { source } else { "typed" };
        let project = match (project, cwd) {
            (Some(p), _) => Some(p.to_string()),
            (None, Some(c)) => project_for_path(c, &self.load_projects(), false, &self.paths.home),
            _ => None,
        };
        let rec = IdeaRec {
            id: format!("i{}", random_hex(6)),
            at: util::now_iso(),
            text: util::take_chars(text, 4000),
            project,
            source: source.into(),
            cwd: cwd.map(Into::into),
            state: IdeaState::Inbox,
            sent_at: None,
            brief: None,
        };
        self.ensure_data_dir()?;
        self.writer.append_jsonl(&self.ideas_path(), &rec)?;
        Ok(rec)
    }

    /// `project`: None leaves it, Some(None) clears it, Some(Some(id)) sets it.
    pub fn set_idea(&self, id: &str, state: Option<IdeaState>, project: Option<Option<&str>>) -> SResult<IdeaRec> {
        let mut idea = self.list_ideas().into_iter().find(|i| i.id == id).ok_or_else(|| StoreError("No idea with that id.".into()))?;
        let mut rec = Map::new();
        rec.insert("id".into(), id.into());
        rec.insert("at".into(), util::now_iso().into());
        if let Some(s) = state {
            if !matches!(s, IdeaState::Inbox | IdeaState::Done) {
                return Err(StoreError("State must be inbox or done.".into()));
            }
            rec.insert("state".into(), serde_json::json!(s));
            idea.state = s;
            idea.sent_at = None;
            idea.brief = None;
        }
        if let Some(p) = project {
            rec.insert("project".into(), p.map_or(Value::Null, Value::from));
            idea.project = p.map(Into::into);
        }
        self.writer.append_jsonl(&self.ideas_path(), &rec)?;
        Ok(idea)
    }

    /// Marks ideas as sent to an agent working on `project` (they leave the list to sort): one
    /// `{"id","state":"sent","at","project"}` record each. Unknown ids are refused before anything
    /// is written.
    /// Mark ideas as sent to an agent for `project`, in the brief with id `brief` (kept on each
    /// idea's "sent" record so the idea can be traced to the brief).
    pub fn mark_sent(&self, ids: &[String], project: &str, brief: Option<&str>) -> SResult<Vec<IdeaRec>> {
        let all = self.list_ideas();
        let mut out = Vec::new();
        for id in ids {
            let i = all.iter().find(|i| &i.id == id).ok_or_else(|| StoreError("No idea with that id.".into()))?;
            out.push(i.clone());
        }
        let at = util::now_iso();
        for i in &mut out {
            let mut rec = serde_json::json!({"id": i.id, "state": IdeaState::Sent, "at": at, "project": project});
            if let Some(b) = brief {
                rec["brief"] = b.into();
            }
            self.writer.append_jsonl(&self.ideas_path(), &rec)?;
            i.state = IdeaState::Sent;
            i.sent_at = Some(at.clone());
            i.brief = brief.map(String::from);
            i.project = Some(project.into());
        }
        Ok(out)
    }

    /// Folded ideas, newest first.
    pub fn list_ideas(&self) -> Vec<IdeaRec> {
        let mut out: Vec<IdeaRec> = Vec::new();
        let mut at: HashMap<String, usize> = HashMap::new();
        for r in iter_jsonl(&self.ideas_path()) {
            let Some(o) = r.as_object() else { continue };
            let Some(id) = o.get("id").and_then(Value::as_str) else { continue };
            if o.contains_key("text") {
                let s = |k: &str| o.get(k).and_then(Value::as_str).map(String::from);
                let rec = IdeaRec {
                    id: id.into(),
                    at: s("at").unwrap_or_default(),
                    text: s("text").unwrap_or_default(),
                    project: s("project"),
                    source: s("source").unwrap_or_else(|| "typed".into()),
                    cwd: s("cwd"),
                    state: o.get("state").and_then(|v| IdeaState::deserialize(v).ok()).unwrap_or_default(),
                    sent_at: None,
                    brief: None,
                };
                match at.get(id) {
                    Some(&i) => out[i] = rec,
                    None => {
                        at.insert(id.into(), out.len());
                        out.push(rec);
                    }
                }
            } else if let Some(&i) = at.get(id) {
                if let Some(st) = o.get("state").and_then(|v| IdeaState::deserialize(v).ok())
                    && st != IdeaState::Unknown
                {
                    let sent = st == IdeaState::Sent;
                    out[i].state = st;
                    out[i].sent_at = if sent { o.get("at").and_then(Value::as_str).map(String::from) } else { None };
                    out[i].brief = if sent { o.get("brief").and_then(Value::as_str).map(String::from) } else { None };
                }
                if let Some(p) = o.get("project") {
                    out[i].project = p.as_str().map(String::from);
                }
            }
        }
        out.sort_by(|a, b| b.at.cmp(&a.at));
        out
    }

    // ---- pauses

    pub fn add_pause(&self, project: &str, note: &str, next: &str, git: Map<String, Value>, sessions: Vec<String>) -> SResult<PauseRec> {
        let rec =
            PauseRec { project: project.into(), at: util::now_iso(), note: util::take_chars(note, 4000), next: util::take_chars(next, 4000), git, sessions: sessions.into_iter().take(200).collect() };
        self.ensure_data_dir()?;
        self.writer.append_jsonl(&self.pauses_path(), &rec)?;
        Ok(rec)
    }

    pub fn list_pauses(&self, project: Option<&str>) -> Vec<PauseRec> {
        let mut out: Vec<PauseRec> = iter_jsonl(&self.pauses_path()).filter_map(|v| serde_json::from_value::<PauseRec>(v).ok()).filter(|r| project.is_none_or(|p| r.project == p)).collect();
        out.sort_by(|a, b| a.at.cmp(&b.at));
        out
    }

    /// project id -> last checkpoint.
    pub fn last_pauses(&self) -> HashMap<String, PauseRec> {
        let mut out = HashMap::new();
        for r in self.list_pauses(None) {
            out.insert(r.project.clone(), r);
        }
        out
    }
}

/// The folders agents' sessions started in, from the first lines of their transcripts (read only):
/// one Claude transcript per folder of `~/.claude/projects` (each folder is one starting place),
/// and every Codex rollout's opening line. Only the `cwd` is taken from what is read.
pub fn session_cwds(paths: &Paths) -> Vec<String> {
    let jsonl = |d: &Path| -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = fs::read_dir(d).map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "jsonl") && !is_forbidden(p)).collect()).unwrap_or_default();
        v.sort();
        v
    };
    let dirs = |d: &Path| -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = fs::read_dir(d).map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect()).unwrap_or_default();
        v.sort();
        v
    };
    let mut out: Vec<String> = Vec::new();
    let mut push = |c: String| {
        if !out.contains(&c) {
            out.push(c);
        }
    };
    for d in dirs(&paths.claude.join("projects")) {
        if let Some(c) = jsonl(&d).iter().take(5).find_map(|f| first_cwd(f)) {
            push(c);
        }
    }
    for y in dirs(&paths.codex.join("sessions")) {
        for m in dirs(&y) {
            for d in dirs(&m) {
                for f in jsonl(&d) {
                    if let Some(c) = first_cwd(&f) {
                        push(c);
                    }
                }
            }
        }
    }
    out
}

/// The first `cwd` in a transcript's first 40 lines (at most 1 MB read).
fn first_cwd(path: &Path) -> Option<String> {
    let f = fs::File::open(path).ok()?;
    let r = BufReader::new(std::io::Read::take(f, 1024 * 1024));
    for line in r.split(b'\n').take(40) {
        let line = line.ok()?;
        if !util::contains(&line, b"\"cwd\"") {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<Value>(&line) else { continue };
        let cwd = v.get("cwd").or_else(|| v.get("payload").and_then(|p| p.get("cwd"))).and_then(Value::as_str);
        if let Some(c) = cwd.filter(|c| c.starts_with('/')) {
            return Some(c.to_string());
        }
    }
    None
}

// ------------------------------------------------------------------ read-only checks (`wake doctor`)

/// How one of Wake's files reads, found without moving, renaming or rewriting anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileCheck {
    /// Not there (Wake makes it when needed).
    Missing,
    /// There, but it couldn't be opened or read (the reason in words).
    Unreadable(String),
    /// Read, but not in a form Wake understands (the parser's words).
    Damaged(String),
    /// Fine; for projects.json, how many projects it holds.
    Fine(usize),
}

/// projects.json, read only: unlike loading it, this never moves a damaged file aside.
pub fn check_projects(paths: &Paths) -> FileCheck {
    let path = paths.data.join("projects.json");
    match fs::read(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => FileCheck::Missing,
        Err(e) => FileCheck::Unreadable(e.kind().to_string()),
        Ok(b) => match serde_json::from_slice::<ProjectsFile>(&b) {
            Ok(f) => FileCheck::Fine(f.projects.len()),
            Err(e) => FileCheck::Damaged(e.to_string()),
        },
    }
}

/// How many non-empty lines of ideas.jsonl couldn't be read (not JSON, or not a record with an id).
pub fn unreadable_idea_lines(paths: &Paths) -> usize {
    let path = paths.data.join("ideas.jsonl");
    let Ok(f) = fs::File::open(&path) else { return 0 };
    BufReader::new(f)
        .split(b'\n')
        .filter(|line| match line {
            Err(_) => true,
            Ok(l) if l.trim_ascii().is_empty() => false,
            Ok(l) => serde_json::from_slice::<Value>(l.trim_ascii()).ok().is_none_or(|v| v.get("id").and_then(Value::as_str).is_none()),
        })
        .count()
}

/// Files Wake set aside because it couldn't understand them (`*.broken-*`), in its data and
/// settings folders, oldest name first.
pub fn broken_files(paths: &Paths) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = [&paths.data, &paths.config]
        .iter()
        .flat_map(|d| fs::read_dir(d).map(|rd| rd.flatten().map(|e| e.path()).collect::<Vec<_>>()).unwrap_or_default())
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().contains(".broken-")))
        .collect();
    out.sort();
    out.dedup();
    out
}

pub fn project_by_id<'a>(id: &str, projects: &'a [ProjectRec]) -> Option<&'a ProjectRec> {
    projects.iter().find(|p| p.id == id)
}

/// The project owning a path (longest matching root), or None. `~` means `home`.
pub fn project_for_path(path: &str, projects: &[ProjectRec], include_removed: bool, home: &Path) -> Option<String> {
    let p = norm_path(path, home)?;
    let mut best: Option<&str> = None;
    let mut best_len = 0usize;
    for proj in projects {
        if proj.removed && !include_removed {
            continue;
        }
        for r in &proj.roots {
            let Some(r) = norm_path(r, home) else { continue };
            let under = p == r || (p.starts_with(&r) && p.as_bytes().get(r.len()) == Some(&b'/'));
            if under && (best.is_none() || r.len() > best_len) {
                best = Some(&proj.id);
                best_len = r.len();
            }
        }
    }
    best.map(String::from)
}

#[cfg(test)]
pub(crate) mod testenv {
    //! A throwaway home with synthetic transcripts and projects, like tests/wakefix.py.
    use super::*;

    pub struct Env {
        pub root: PathBuf,
        pub paths: Paths,
        /// Removes the home when the Env is dropped.
        _dir: crate::testing::Scratch,
    }

    fn fixtures() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
    }

    fn copy_tree(src: &Path, dst: &Path, root: &str) {
        fs::create_dir_all(dst).unwrap();
        for e in fs::read_dir(src).unwrap() {
            let e = e.unwrap();
            let p = e.path();
            let out = dst.join(e.file_name());
            if p.is_dir() {
                copy_tree(&p, &out, root);
            } else {
                let data = fs::read_to_string(&p).unwrap().replace("@ROOT@", root);
                fs::write(out, data).unwrap();
            }
        }
    }

    impl Env {
        pub fn new() -> Env {
            let dir = crate::testing::scratch("env");
            let root = dir.0.clone();
            let paths = Paths::under(&root);
            for name in ["alpha", "beta", "beta-private", "beta-deploy", "archive", ".hidden"] {
                fs::create_dir_all(paths.projects.join(name).join(".git")).unwrap();
            }
            fs::create_dir_all(paths.projects.join("group/one/.git")).unwrap();
            fs::create_dir_all(paths.projects.join("group/two/.git")).unwrap();
            fs::create_dir_all(paths.projects.join("notes")).unwrap();
            let r = root.to_string_lossy().into_owned();
            copy_tree(&fixtures().join("claude"), &paths.claude, &r);
            copy_tree(&fixtures().join("codex"), &paths.codex, &r);
            fs::create_dir_all(paths.claude.join("sessions")).unwrap();
            fs::create_dir_all(paths.runtime.join("working")).unwrap();
            fs::create_dir_all(paths.runtime.join("resume")).unwrap();
            Env { root, paths, _dir: dir }
        }

        pub fn store(&self) -> Store {
            Store::new(self.paths.clone())
        }

        pub fn transcript(&self, sid: &str) -> PathBuf {
            self.paths.claude.join("projects/-home").join(format!("{sid}.jsonl"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testenv::Env;
    use super::*;

    #[test]
    fn never_writes_outside_data_dir() {
        let env = Env::new();
        let s = env.store();
        assert!(s.writer.write_json(&env.paths.claude.join("x.json"), &1).is_err());
        assert!(s.writer.append_jsonl(&env.paths.projects.join("alpha/x.jsonl"), &1).is_err());
        assert!(s.writer.write_json(&env.paths.data.join("../claude/y.json"), &1).is_err());
        assert!(!env.paths.claude.join("x.json").exists());
    }

    #[test]
    fn forbidden_names() {
        for n in ["123.abc.key", ".env", "credentials.json", "server.pem", ".env.local"] {
            assert!(is_forbidden(Path::new(&format!("/x/{n}"))), "{n}");
        }
        assert!(!is_forbidden(Path::new("/x/123.json")));
        let env = Env::new();
        let key = env.paths.data.join("123.abc.key");
        fs::create_dir_all(&env.paths.data).unwrap();
        fs::write(&key, "{}").unwrap();
        assert!(read_json::<Value>(&key).is_none());
    }

    #[test]
    fn ideas_fold() {
        let env = Env::new();
        let s = env.store();
        let cwd = format!("{}/alpha/src", env.paths.projects.display());
        let a = s.add_idea("first", None, "typed", Some(&cwd)).unwrap();
        let b = s.add_idea("second", Some("beta"), "typed", None).unwrap();
        assert_eq!(a.project.as_deref(), Some("alpha"));
        s.set_idea(&a.id, Some(IdeaState::Done), None).unwrap();
        s.set_idea(&a.id, None, Some(Some("beta"))).unwrap();
        let ideas = s.list_ideas();
        let ia = ideas.iter().find(|i| i.id == a.id).unwrap();
        assert_eq!((ia.state, ia.project.as_deref()), (IdeaState::Done, Some("beta")));
        assert_eq!(ideas.iter().find(|i| i.id == b.id).unwrap().state, IdeaState::Inbox);
        assert_eq!(fs::metadata(&env.paths.data).unwrap().permissions().mode() & 0o777, 0o700);
        assert!(s.add_idea("   ", None, "typed", None).is_err());
        assert!(s.set_idea(&a.id, Some(IdeaState::Unknown), None).is_err());
        assert!(s.set_idea("nope", Some(IdeaState::Done), None).is_err());
        assert!(s.set_idea(&a.id, Some(IdeaState::Sent), None).is_err(), "sent only through mark_sent, with its project");
    }

    #[test]
    fn sending_ideas_to_an_agent_is_a_state_with_its_time_and_project() {
        let env = Env::new();
        let s = env.store();
        let a = s.add_idea("first", Some("alpha"), "typed", None).unwrap();
        let b = s.add_idea("second", Some("alpha"), "typed", None).unwrap();
        assert!(s.mark_sent(&[a.id.clone(), "nope".into()], "alpha", None).is_err());
        assert_eq!(s.list_ideas().iter().filter(|i| i.state == IdeaState::Sent).count(), 0, "nothing written when one id is unknown");
        let sent = s.mark_sent(&[a.id.clone(), b.id.clone()], "alpha", Some("b123")).unwrap();
        assert!(sent.iter().all(|i| i.brief.as_deref() == Some("b123")));
        assert!(sent.iter().all(|i| i.state == IdeaState::Sent && i.sent_at.is_some()));
        let ideas = s.list_ideas();
        let ia = ideas.iter().find(|i| i.id == a.id).unwrap();
        assert_eq!((ia.state, ia.project.as_deref()), (IdeaState::Sent, Some("alpha")));
        assert_eq!(ia.sent_at, sent[0].sent_at);
        assert_eq!(ia.brief.as_deref(), Some("b123"));
        // The record on disk: state, time and project, nothing else.
        let text = fs::read_to_string(env.paths.data.join("ideas.jsonl")).unwrap();
        let last: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(last["state"], "sent");
        assert_eq!(last["project"], "alpha");
        assert!(!text.lines().next().unwrap().contains("sent_at"), "a new idea's record keeps the old format");
        // Back to sort: no longer sent.
        s.set_idea(&a.id, Some(IdeaState::Inbox), None).unwrap();
        let ia = s.list_ideas().into_iter().find(|i| i.id == a.id).unwrap();
        assert_eq!((ia.state, ia.sent_at), (IdeaState::Inbox, None));
    }

    /// Files as Wake wrote them before agents and idea states were enums: they load, and what is
    /// written back is byte for byte the same.
    #[test]
    fn files_from_before_the_enums_load_and_write_back_the_same() {
        fn same<T: serde::de::DeserializeOwned + Serialize>(json: &str) -> T {
            let v: T = serde_json::from_str(json).unwrap();
            assert_eq!(serde_json::to_string(&v).unwrap(), json);
            v
        }
        // index/sessions/<id>.json
        let acc: crate::index::Acc = same(
            r#"{"id":"s1","agent":"codex","source":"/h/.codex/sessions/2026/11/20/rollout-s1.jsonl","startedAt":"2026-11-20T10:00:00.000Z","lastAt":"2026-11-20T11:00:00.000Z","prompts":1,"promptsList":[{"at":"2026-11-20T10:00:00.000Z","text":"hi"}],"firstPrompt":"hi","files":{"/p/a.rs":2},"commands":1,"tests":0,"prLinks":[],"cost":null,"aiTitle":null,"customTitle":null,"lastPromptMeta":null,"agentName":null,"awaySummary":null,"awayAt":null,"cwd":"/p","launchCwd":"/p","gitBranch":"main","projTurns":{"/p":1},"worktrees":{},"cwdSeen":{"/p":1},"lastText":"done","lastTextAt":"2026-11-20T11:00:00.000Z","lastKind":"assistant","lastUserAt":null,"activeSec":120.0,"prevTs":1767229200.0,"subagentsTotal":0,"deletedByClaude":false,"scanOffset":4096}"#,
        );
        assert_eq!(acc.agent, crate::types::Agent::Codex);
        // index/files.json, one entry
        let f: crate::index::FileRec = same(r#"{"size":4096,"mtime":1767229200.5,"offset":4096,"session":"s1","agent":"claude"}"#);
        assert_eq!(f.agent, crate::types::Agent::Claude);
        // questions/threads/<id>.json
        let t: crate::types::QThread = same(
            r#"{"id":"q1","title":"Why","project":null,"claude_session":"abc","created":"2026-11-20T10:00:00.000Z","turns":[{"question":"Why?","answer":"Because.","asked_at":"2026-11-20T10:00:00.000Z","answered_at":"2026-11-20T10:00:05.000Z","error":null}],"ai":"gemini"}"#,
        );
        assert_eq!(crate::questions::thread_ai(&t), crate::types::Agent::Gemini);
        // ideas.jsonl: an idea's own record, then the ideas as Wake hands them out
        let i: IdeaRec = same(r#"{"id":"i1","at":"2026-11-19T10:00:00.000Z","text":"x","project":"alpha","source":"typed","cwd":"/h","state":"inbox"}"#);
        assert_eq!(i.state, IdeaState::Inbox);
        let i: crate::types::Idea = same(
            r#"{"id":"i1","at":"2026-11-19T10:00:00.000Z","text":"x","project":"alpha","project_name":"alpha","source":"typed","state":"sent","sent_at":"2026-11-19T11:00:00.000Z","brief":"b0123456789ab"}"#,
        );
        assert_eq!(i.state, IdeaState::Sent);
        // A state this Wake doesn't know still loads, and goes with the ideas to sort.
        let i: IdeaRec = serde_json::from_str(r#"{"id":"i2","at":"","text":"y","project":null,"source":"typed","cwd":null,"state":"kept"}"#).unwrap();
        assert_eq!(i.state, IdeaState::Unknown);
        // Folding ideas.jsonl: the state records main writes, sent with its brief.
        let env = Env::new();
        let s = env.store();
        s.ensure_data_dir().unwrap();
        fs::write(
            env.paths.data.join("ideas.jsonl"),
            "{\"id\":\"i1\",\"at\":\"2026-11-19T10:00:00.000Z\",\"text\":\"x\",\"project\":null,\"source\":\"typed\",\"cwd\":\"/h\",\"state\":\"inbox\"}\n\
             {\"id\":\"i2\",\"at\":\"2026-11-19T10:01:00.000Z\",\"text\":\"y\",\"project\":null,\"source\":\"typed\",\"cwd\":\"/h\",\"state\":\"kept\"}\n\
             {\"id\":\"i1\",\"state\":\"sent\",\"at\":\"2026-11-19T11:00:00.000Z\",\"project\":\"alpha\",\"brief\":\"b0123456789ab\"}\n\
             {\"id\":\"i2\",\"state\":\"bogus\",\"at\":\"2026-11-19T11:00:00.000Z\"}\n",
        )
        .unwrap();
        let ideas = s.list_ideas();
        let by = |id: &str| ideas.iter().find(|i| i.id == id).unwrap().clone();
        let i1 = by("i1");
        assert_eq!((i1.state, i1.project.as_deref(), i1.brief.as_deref()), (IdeaState::Sent, Some("alpha"), Some("b0123456789ab")));
        assert_eq!(by("i2").state, IdeaState::Unknown, "an unknown state record changes nothing");
    }

    #[test]
    fn ideas_read_python_records() {
        let env = Env::new();
        let s = env.store();
        s.ensure_data_dir().unwrap();
        fs::write(
            env.paths.data.join("ideas.jsonl"),
            "{\"id\":\"i1\",\"at\":\"2026-11-19T10:00:00.000Z\",\"text\":\"x\",\"project\":null,\"source\":\"shell\",\"cwd\":\"/h\",\"state\":\"inbox\"}\n\
             garbage\n{\"id\":\"i1\",\"state\":\"done\",\"at\":\"2026-11-19T11:00:00.000Z\"}\n",
        )
        .unwrap();
        let ideas = s.list_ideas();
        assert_eq!(ideas.len(), 1);
        assert_eq!((ideas[0].state, ideas[0].source.as_str()), (IdeaState::Done, "shell"));
    }

    #[test]
    fn next_step_is_kept_on_the_project() {
        let env = Env::new();
        let s = env.store();
        let id = s.load_projects()[0].id.clone();
        assert_eq!(s.set_project_next(&id, "  ship it\n").unwrap().next, "ship it");
        let p = s.load_projects().into_iter().find(|p| p.id == id).unwrap();
        assert_eq!(p.next, "ship it");
        assert!(s.set_project_next("nope", "x").is_err());
    }

    #[test]
    fn groups_and_importance_are_kept_on_the_project() {
        let env = Env::new();
        let s = env.store();
        let id = s.load_projects()[0].id.clone();
        assert_eq!(s.set_project_group(&id, Some("  Work ")).unwrap().group.as_deref(), Some("Work"));
        assert_eq!(s.set_project_importance(&id, Some("high")).unwrap().importance.as_deref(), Some("high"));
        let p = s.load_projects().into_iter().find(|p| p.id == id).unwrap();
        assert_eq!((p.group.as_deref(), p.importance.as_deref()), (Some("Work"), Some("high")));
        // Blank takes it out of its group; normal is no importance at all; unknown words are refused.
        assert_eq!(s.set_project_group(&id, Some("  ")).unwrap().group, None);
        assert_eq!(s.set_project_importance(&id, None).unwrap().importance, None);
        assert!(s.set_project_importance(&id, Some("urgent")).is_err());
        assert!(s.set_project_group("nope", Some("x")).is_err());
    }

    #[test]
    fn a_group_lives_on_its_own_until_it_is_removed() {
        let env = Env::new();
        let s = env.store();
        let ids: Vec<String> = s.load_projects().iter().map(|p| p.id.clone()).collect();
        // An empty group is kept, through other changes to the project list too.
        assert_eq!(s.add_group("  Clients ").unwrap(), "Clients");
        assert_eq!(s.add_group("clients").unwrap(), "Clients", "the same name in another case is the same group");
        assert!(s.add_group("   ").is_err());
        s.set_project_next(&ids[0], "write the tests").unwrap();
        assert_eq!(s.groups(), vec!["Clients".to_string()]);
        // A project's group is a group too, and stays when the project leaves it.
        s.set_project_group(&ids[0], Some("Work")).unwrap();
        s.set_project_group(&ids[0], None).unwrap();
        assert_eq!(s.groups(), vec!["Clients".to_string(), "Work".to_string()]);
        // Removing a group leaves its projects tracked, in no group.
        s.set_project_group(&ids[0], Some("Clients")).unwrap();
        assert_eq!(s.remove_group("Clients").unwrap(), 1);
        assert_eq!(s.groups(), vec!["Work".to_string()]);
        let p = s.load_projects().into_iter().find(|p| p.id == ids[0]).unwrap();
        assert_eq!((p.group, p.removed), (None, false));
        // Kept in projects.json beside the projects.
        let raw: serde_json::Value = serde_json::from_slice(&fs::read(s.projects_path()).unwrap()).unwrap();
        assert_eq!(raw["groups"], serde_json::json!(["Work"]));
    }

    #[test]
    fn groups_keep_the_order_they_were_put_in() {
        let env = Env::new();
        let s = env.store();
        for g in ["Zebra", "apple", "Mango"] {
            s.add_group(g).unwrap();
        }
        assert_eq!(s.groups(), ["Zebra", "apple", "Mango"], "in the order they were made, not A to Z");
        s.order_groups(&["Mango".into(), "Zebra".into()]).unwrap();
        assert_eq!(s.groups(), ["Mango", "Zebra", "apple"], "the ones not named keep their place after");
        s.order_groups(&["nope".into(), "apple".into()]).unwrap();
        assert_eq!(s.groups(), ["apple", "Mango", "Zebra"], "a name that is no group is left out");
        // A group only a project names (an older file) comes after, until it is put in order.
        let id = s.load_projects()[0].id.clone();
        let mut projects = s.load_projects();
        projects.iter_mut().find(|p| p.id == id).unwrap().group = Some("Old".into());
        {
            let _g = PROJECTS_LOCK.lock().unwrap();
            s.save_projects_locked(&projects).unwrap();
        }
        assert_eq!(s.groups(), ["apple", "Mango", "Zebra", "Old"]);
        s.order_groups(&["Old".into()]).unwrap();
        assert_eq!(s.groups(), ["Old", "apple", "Mango", "Zebra"]);
    }

    #[test]
    fn first_list_takes_repos_agents_worked_in_outside_projects_only_once() {
        let env = Env::new();
        let h = &env.paths.home;
        for d in ["code/tool/.git", "code/tool/src", ".config/dots/.git", "site-deploy/.git", "notes", "elsewhere/app/.git"] {
            fs::create_dir_all(h.join(d)).unwrap();
        }
        fs::create_dir_all(h.join("Projects/archive/old/.git")).unwrap();
        let session = |dir: &str, cwd: &Path| {
            let d = env.paths.claude.join("projects").join(dir);
            fs::create_dir_all(&d).unwrap();
            let line = serde_json::json!({"type": "user", "cwd": cwd.to_string_lossy(), "message": {"role": "user", "content": "hi"}});
            fs::write(d.join("s.jsonl"), format!("{{\"type\":\"summary\"}}\n{line}\n")).unwrap();
        };
        session("-a", &h.join("code/tool/src"));
        session("-b", &h.join(".config/dots"));
        session("-c", &h.join("site-deploy"));
        session("-d", &h.join("notes"));
        session("-e", h);
        session("-f", &h.join("Projects/archive/old"));
        let codex = env.paths.codex.join("sessions/2026/11/20");
        fs::create_dir_all(&codex).unwrap();
        let meta = serde_json::json!({"type": "session_meta", "payload": {"cwd": h.join("elsewhere/app").to_string_lossy()}});
        fs::write(codex.join("rollout-x.jsonl"), format!("{meta}\n")).unwrap();
        let s = env.store();
        let names = |s: &Store| s.load_projects().into_iter().map(|p| p.name).collect::<Vec<_>>();
        let first = names(&s);
        assert!(first.contains(&"tool".to_string()) && first.contains(&"app".to_string()), "{first:?}");
        for not in ["dots", "site-deploy", "notes", "home", "old"] {
            assert!(!first.contains(&not.to_string()), "{not} in {first:?}");
        }
        let tool = s.load_projects().into_iter().find(|p| p.name == "tool").unwrap();
        assert_eq!(tool.roots, [realpath(&h.join("code/tool")).to_string_lossy().to_string()]);
        // an existing list is never changed by later sessions
        fs::create_dir_all(h.join("more/.git")).unwrap();
        session("-g", &h.join("more"));
        assert_eq!(names(&s), first);
    }

    #[test]
    fn doctor_checks_read_without_repairing() {
        let env = Env::new();
        let p = &env.paths;
        assert_eq!(check_projects(p), FileCheck::Missing);
        let n = env.store().load_projects().len();
        assert_eq!(check_projects(p), FileCheck::Fine(n));
        fs::write(p.data.join("projects.json"), "{not json").unwrap();
        assert!(matches!(check_projects(p), FileCheck::Damaged(_)));
        assert_eq!(fs::read_to_string(p.data.join("projects.json")).unwrap(), "{not json", "left as it was");
        assert!(broken_files(p).is_empty());
        assert_eq!(unreadable_idea_lines(p), 0);
        fs::write(p.data.join("ideas.jsonl"), "{\"id\":\"i1\",\"text\":\"x\"}\n\ngarbage\n{\"no\":\"id\"}\n").unwrap();
        assert_eq!(unreadable_idea_lines(p), 2);
        fs::create_dir_all(&p.config).unwrap();
        fs::write(p.config.join("settings.toml.broken-2026"), "x").unwrap();
        fs::write(p.data.join("projects.json.broken-2026"), "x").unwrap();
        assert_eq!(broken_files(p), [p.config.join("settings.toml.broken-2026"), p.data.join("projects.json.broken-2026")]);
    }

    #[test]
    fn pauses() {
        let env = Env::new();
        let s = env.store();
        s.add_pause("alpha", "one", "", Map::new(), vec![]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        s.add_pause("alpha", "two", "", Map::new(), vec![]).unwrap();
        assert_eq!(s.last_pauses()["alpha"].note, "two");
    }

    #[test]
    fn project_discovery_and_paths() {
        let env = Env::new();
        let s = env.store();
        let projects = s.load_projects();
        let mut ids: Vec<&str> = projects.iter().map(|p| p.id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, ["alpha", "beta", "group"]);
        let beta = project_by_id("beta", &projects).unwrap();
        let names: Vec<_> = beta.roots.iter().map(|r| Path::new(r).file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["beta", "beta-private"]);
        let pp = env.paths.projects.display().to_string();
        let h = &env.paths.home;
        assert_eq!(project_for_path(&format!("{pp}/beta-private/x"), &projects, false, h).as_deref(), Some("beta"));
        assert_eq!(project_for_path(&format!("{pp}/group/two"), &projects, false, h).as_deref(), Some("group"));
        assert_eq!(project_for_path(&h.display().to_string(), &projects, false, h), None);
        // ~ means the home Wake was given, not the developer's $HOME
        assert_eq!(project_for_path("~/Projects/group/two", &projects, false, h).as_deref(), Some("group"));
        // projects.json written in the Python format and read back the same
        let again = s.load_projects();
        assert_eq!(again, projects);
        // add / remove / restore
        let elsewhere = env.paths.home.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        let p = s.add_project(&elsewhere.display().to_string()).unwrap();
        assert_eq!(p.id, "elsewhere");
        assert!(s.add_project("/no/such").is_err());
        assert!(s.set_project_removed("alpha", true).unwrap().removed);
        assert!(!s.set_project_removed("alpha", false).unwrap().removed);
    }

    #[test]
    fn a_broken_projects_json_is_moved_aside_never_overwritten() {
        let env = Env::new();
        let s = env.store();
        let path = env.paths.data.join("projects.json");
        fs::create_dir_all(&env.paths.data).unwrap();
        // a null where text belongs is read as empty, not as a broken file
        fs::write(&path, r#"{"projects":[{"id":"alpha","name":"alpha","roots":[],"desc":null,"next":"ship it","why":null,"removed":false,"added":"2026-01-01T00:00:00Z"}]}"#).unwrap();
        let ps = s.load_projects();
        assert_eq!((ps.len(), ps[0].desc.as_str(), ps[0].next.as_str()), (1, "", "ship it"));
        assert_eq!(s.projects_problem(), None);
        // cut off half way (a bad hand edit): kept aside byte for byte, a fresh list made, the problem said
        let broken = r#"{"projects":[{"id":"alpha","name":"alpha","desc":"my own words","next":"#;
        fs::write(&path, broken).unwrap();
        let ps = s.load_projects();
        assert_eq!(ps.len(), 3, "rediscovered from ~/Projects");
        let kept: Vec<_> = fs::read_dir(&env.paths.data).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with("projects.json.broken-")).map(|e| e.path()).collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(fs::read_to_string(&kept[0]).unwrap(), broken);
        let problem = s.projects_problem().unwrap();
        assert!(problem.starts_with("Wake's project list was damaged, so it started a new one") && problem.contains("projects.json.broken-"), "{problem}");
        assert_eq!(s.load_projects().len(), 3); // the fresh file reads back
        // valid JSON of the wrong shape is treated the same way
        fs::write(&path, r#"{"projects":"oops"}"#).unwrap();
        s.load_projects();
        let n = fs::read_dir(&env.paths.data).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with("projects.json.broken-")).count();
        assert!(n >= 1);
        assert!(path.is_file());
    }

    #[test]
    fn an_unreadable_projects_json_is_left_alone() {
        let env = Env::new();
        let s = env.store();
        let path = env.paths.data.join("projects.json");
        fs::create_dir_all(&path).unwrap(); // a folder: opening works, reading fails
        let ps = s.load_projects();
        assert_eq!(ps.len(), 3);
        assert!(s.projects_problem().unwrap().contains("left the file alone"));
        let elsewhere = env.paths.home.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        assert!(s.add_project(&elsewhere.display().to_string()).is_err(), "must not replace a file it could not read");
        assert!(path.is_dir());
    }

    #[test]
    fn worktree_fold_and_slug() {
        assert_eq!(fold_worktree("/x/repo/.claude/worktrees/feat/src"), "/x/repo");
        assert_eq!(fold_worktree("/x/repo/.claude/worktrees/"), "/x/repo/.claude/worktrees/");
        assert_eq!(norm_path("/x/repo/.claude/worktrees/feat/src", Path::new("/h")).as_deref(), Some("/x/repo"));
        assert_eq!(norm_path("~/a", Path::new("/nonexistent-home-1")).as_deref(), Some("/nonexistent-home-1/a"));
        assert_eq!(norm_path("~/a", Path::new("/nonexistent-home-2")).as_deref(), Some("/nonexistent-home-2/a"));
        assert_eq!(norm_abs("~/a").as_deref(), Some("~/a"));
        assert_eq!(slug("Skyhook"), "skyhook");
        assert_eq!(slug("tideline-desktop"), "tideline-desktop");
        assert_eq!(slug("--"), "project");
        assert_eq!(slug("A  b"), "a-b");
    }
}
