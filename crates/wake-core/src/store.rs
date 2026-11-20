//! Paths, private directories, atomic JSON writes and Wake's own persistence.
//!
//! Wake writes only under its data directory (default ~/.local/share/wake, mode 0700); every write is
//! checked against it. Everything else (~/.claude, ~/.codex, project folders) is read only. The files
//! keep the Python version's formats so its data carries over: projects.json, ideas.jsonl,
//! pauses.jsonl. The index lives in `index-v2/`.
//!
//! Environment overrides (as in Python): WAKE_DATA_DIR, WAKE_CLAUDE_DIR, WAKE_CODEX_DIR,
//! WAKE_PROJECTS_DIR, WAKE_RUNTIME_DIR. Tests build a [`Paths`] directly instead.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

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
    /// Wake's settings folder (settings.toml): $WAKE_CONFIG_DIR, else $XDG_CONFIG_HOME/wake, else ~/.config/wake.
    pub config: PathBuf,
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key).filter(|v| !v.is_empty()).map(PathBuf::from)
}

pub fn home_dir() -> PathBuf {
    env_path("HOME").unwrap_or_else(|| PathBuf::from("/"))
}

impl Paths {
    pub fn from_env() -> Paths {
        let home = home_dir();
        let runtime = env_path("WAKE_RUNTIME_DIR").unwrap_or_else(|| {
            let base = env_path("XDG_RUNTIME_DIR").unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })));
            base.join("slipstream")
        });
        Paths {
            data: env_path("WAKE_DATA_DIR").unwrap_or_else(|| home.join(".local/share/wake")),
            claude: env_path("WAKE_CLAUDE_DIR").unwrap_or_else(|| home.join(".claude")),
            codex: env_path("WAKE_CODEX_DIR").unwrap_or_else(|| home.join(".codex")),
            projects: env_path("WAKE_PROJECTS_DIR").unwrap_or_else(|| home.join("Projects")),
            plugin_state: home.join(".local/state/slipstream/plugins/wake/state"),
            config: env_path("WAKE_CONFIG_DIR").or_else(|| env_path("XDG_CONFIG_HOME").filter(|p| p.is_absolute()).map(|p| p.join("wake"))).unwrap_or_else(|| home.join(".config/wake")),
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

/// Read a JSON file; None if missing or invalid. Refuses forbidden names.
pub fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    if is_forbidden(path) {
        return None;
    }
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Each parsable JSON line of a file, streamed.
pub fn iter_jsonl(path: &Path) -> impl Iterator<Item = Value> {
    let reader = if is_forbidden(path) { None } else { fs::File::open(path).ok().map(BufReader::new) };
    reader.into_iter().flat_map(|r| r.split(b'\n')).filter_map(|line| {
        let line = line.ok()?;
        let t = line.trim_ascii();
        if t.is_empty() {
            return None;
        }
        serde_json::from_slice(t).ok()
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
    /// inbox | done | sent (sent to an agent; added 2026-11-20)
    pub state: String,
    /// When it was sent to an agent (state "sent"), from its state record. Not in the idea's
    /// own record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GitMark {
    pub branch: Option<String>,
    pub head: Option<String>,
    pub ahead: Option<u32>,
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
    #[allow(dead_code)] // read only to check the file is one Wake understands
    projects: Vec<ProjectRec>,
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
                ProjectRec { id, name, roots, desc: String::new(), next: String::new(), why: String::new(), removed: false, added: at.clone(), color: None, extra: Map::new() }
            })
            .collect()
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
                            "Wake could not read its project list ({}: {e}). It left the file alone and is showing the folders in {} for now.",
                            path.display(),
                            self.paths.projects.display()
                        ));
                        return self.discover_projects();
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let projects = self.discover_projects();
                if self.ensure_data_dir().is_ok() {
                    let _ = self.save_projects_locked(&projects);
                }
                return projects;
            }
            Err(e) => {
                self.set_problem(format!(
                    "Wake could not open its project list ({}: {e}). It left the file alone and is showing the folders in {} for now.",
                    path.display(),
                    self.paths.projects.display()
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
                        "Wake's project list could not be understood ({e}). It was kept as {} and a fresh list was made from {}; your descriptions and next steps are in the kept file.",
                        aside.display(),
                        self.paths.projects.display()
                    ));
                    let _ = self.save_projects_locked(&projects);
                } else {
                    self.set_problem(format!(
                        "Wake's project list could not be understood ({e}) and could not be moved aside, so Wake left {} as it is and is not saving project changes.",
                        path.display()
                    ));
                }
                projects
            }
        }
    }

    fn save_projects_locked(&self, projects: &[ProjectRec]) -> SResult<()> {
        let path = self.projects_path();
        // Never replace a file Wake could not understand: it may hold the person's own words.
        if path.exists() && fs::read(&path).ok().is_none_or(|b| serde_json::from_slice::<ProjectsFile>(&b).is_err()) {
            return Err(StoreError(format!("{} could not be understood, so Wake will not overwrite it.", path.display())));
        }
        self.writer.write_json(&path, &serde_json::json!({ "projects": projects }))
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
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| ps.clone());
        let mut id = slug(&name);
        while projects.iter().any(|p| p.id == id) {
            id.push_str("-2");
        }
        let proj = ProjectRec { id, name, roots: vec![ps], desc: String::new(), next: String::new(), why: String::new(), removed: false, added: util::now_iso(), color: None, extra: Map::new() };
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
            state: "inbox".into(),
            sent_at: None,
        };
        self.ensure_data_dir()?;
        self.writer.append_jsonl(&self.ideas_path(), &rec)?;
        Ok(rec)
    }

    /// `project`: None leaves it, Some(None) clears it, Some(Some(id)) sets it.
    pub fn set_idea(&self, id: &str, state: Option<&str>, project: Option<Option<&str>>) -> SResult<IdeaRec> {
        let mut idea = self.list_ideas().into_iter().find(|i| i.id == id).ok_or_else(|| StoreError("No idea with that id.".into()))?;
        let mut rec = Map::new();
        rec.insert("id".into(), id.into());
        rec.insert("at".into(), util::now_iso().into());
        if let Some(s) = state {
            if s != "inbox" && s != "done" {
                return Err(StoreError("State must be inbox or done.".into()));
            }
            rec.insert("state".into(), s.into());
            idea.state = s.into();
            idea.sent_at = None;
        }
        if let Some(p) = project {
            rec.insert("project".into(), p.map(Value::from).unwrap_or(Value::Null));
            idea.project = p.map(Into::into);
        }
        self.writer.append_jsonl(&self.ideas_path(), &rec)?;
        Ok(idea)
    }

    /// Marks ideas as sent to an agent working on `project` (they leave the list to sort): one
    /// `{"id","state":"sent","at","project"}` record each. Unknown ids are refused before anything
    /// is written.
    pub fn mark_sent(&self, ids: &[String], project: &str) -> SResult<Vec<IdeaRec>> {
        let all = self.list_ideas();
        let mut out = Vec::new();
        for id in ids {
            let i = all.iter().find(|i| &i.id == id).ok_or_else(|| StoreError("No idea with that id.".into()))?;
            out.push(i.clone());
        }
        let at = util::now_iso();
        for i in &mut out {
            let rec = serde_json::json!({"id": i.id, "state": "sent", "at": at, "project": project});
            self.writer.append_jsonl(&self.ideas_path(), &rec)?;
            i.state = "sent".into();
            i.sent_at = Some(at.clone());
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
                    state: s("state").unwrap_or_else(|| "inbox".into()),
                    sent_at: None,
                };
                match at.get(id) {
                    Some(&i) => out[i] = rec,
                    None => {
                        at.insert(id.into(), out.len());
                        out.push(rec);
                    }
                }
            } else if let Some(&i) = at.get(id) {
                if let Some(st) = o.get("state").and_then(Value::as_str)
                    && (st == "inbox" || st == "done" || st == "sent")
                {
                    out[i].state = st.into();
                    out[i].sent_at = if st == "sent" { o.get("at").and_then(Value::as_str).map(String::from) } else { None };
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
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let base = std::env::temp_dir().join(format!("wake-rs-test-{}-{}", std::process::id(), n));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(&base).unwrap();
            let root = fs::canonicalize(&base).unwrap();
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
            Env { root, paths }
        }

        pub fn store(&self) -> Store {
            Store::new(self.paths.clone())
        }

        pub fn transcript(&self, sid: &str) -> PathBuf {
            self.paths.claude.join("projects/-home").join(format!("{sid}.jsonl"))
        }
    }

    impl Drop for Env {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
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
        s.set_idea(&a.id, Some("done"), None).unwrap();
        s.set_idea(&a.id, None, Some(Some("beta"))).unwrap();
        let ideas = s.list_ideas();
        let ia = ideas.iter().find(|i| i.id == a.id).unwrap();
        assert_eq!((ia.state.as_str(), ia.project.as_deref()), ("done", Some("beta")));
        assert_eq!(ideas.iter().find(|i| i.id == b.id).unwrap().state, "inbox");
        assert_eq!(fs::metadata(&env.paths.data).unwrap().permissions().mode() & 0o777, 0o700);
        assert!(s.add_idea("   ", None, "typed", None).is_err());
        assert!(s.set_idea(&a.id, Some("bogus"), None).is_err());
        assert!(s.set_idea("nope", Some("done"), None).is_err());
        assert!(s.set_idea(&a.id, Some("sent"), None).is_err(), "sent only through mark_sent, with its project");
    }

    #[test]
    fn sending_ideas_to_an_agent_is_a_state_with_its_time_and_project() {
        let env = Env::new();
        let s = env.store();
        let a = s.add_idea("first", Some("alpha"), "typed", None).unwrap();
        let b = s.add_idea("second", Some("alpha"), "typed", None).unwrap();
        assert!(s.mark_sent(&[a.id.clone(), "nope".into()], "alpha").is_err());
        assert_eq!(s.list_ideas().iter().filter(|i| i.state == "sent").count(), 0, "nothing written when one id is unknown");
        let sent = s.mark_sent(&[a.id.clone(), b.id.clone()], "alpha").unwrap();
        assert!(sent.iter().all(|i| i.state == "sent" && i.sent_at.is_some()));
        let ideas = s.list_ideas();
        let ia = ideas.iter().find(|i| i.id == a.id).unwrap();
        assert_eq!((ia.state.as_str(), ia.project.as_deref()), ("sent", Some("alpha")));
        assert_eq!(ia.sent_at, sent[0].sent_at);
        // The record on disk: state, time and project, nothing else.
        let text = fs::read_to_string(env.paths.data.join("ideas.jsonl")).unwrap();
        let last: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(last["state"], "sent");
        assert_eq!(last["project"], "alpha");
        assert!(!text.lines().next().unwrap().contains("sent_at"), "a new idea's record keeps the old format");
        // Back to sort: no longer sent.
        s.set_idea(&a.id, Some("inbox"), None).unwrap();
        let ia = s.list_ideas().into_iter().find(|i| i.id == a.id).unwrap();
        assert_eq!((ia.state.as_str(), ia.sent_at), ("inbox", None));
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
        assert_eq!((ideas[0].state.as_str(), ideas[0].source.as_str()), ("done", "shell"));
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
        assert!(problem.contains("could not be understood") && problem.contains("projects.json.broken-"), "{problem}");
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
