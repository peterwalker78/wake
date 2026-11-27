//! Wake's settings: `~/.config/wake/settings.toml`.
//!
//! Read tolerantly: a missing file is the defaults; a value of the wrong kind or out of range is
//! that setting's default; keys Wake doesn't know are kept and written back as they were. A file
//! that isn't TOML at all is moved aside (`settings.toml.broken-<time>`, never overwritten) with one
//! plain sentence for the person, and Wake starts from the defaults. Written atomically (a
//! temporary file in the same folder, then a rename), mode 0600, in a 0700 folder.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use toml::{Table, Value};

use crate::questions::Ai;
use crate::store::Writer;
use crate::util;

pub const FILE: &str = "settings.toml";
pub const THEMES: [&str; 3] = ["dark", "light", "system"];
pub const ACCENTS: [&str; 4] = ["cyan", "mint", "violet", "blue"];
pub const DENSITIES: [&str; 2] = ["reduced", "detailed"];
/// "full": panels slide, sections open and rows fade (each 150–220 ms); "reduced": nothing moves.
/// The desktop's own "reduce animation" setting is honoured either way.
pub const MOTIONS: [&str; 2] = ["full", "reduced"];
/// Projects as a grid of cards or a list. Added 2026-11-25.
pub const PROJECT_LAYOUTS: [&str; 2] = ["grid", "list"];
/// Projects grouped by the person's own groups, by activity (automatic), or not at all.
pub const PROJECT_GROUPINGS: [&str; 3] = ["groups", "activity", "none"];
/// How often to ask the remotes, in minutes; 0 is never.
pub const REMOTE_MINUTES: [u32; 4] = [0, 5, 10, 30];

const HEADER: &str = "# Wake's settings. Wake rewrites this file when you change something in its Settings view (key ,).\n\
# Keys Wake doesn't know are kept; comments are not.\n\n";

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// Which AI answers new questions: "claude", "codex" or "gemini".
    pub ai: String,
    /// "dark", "light" or "system" (follow the desktop's light/dark preference).
    pub theme: String,
    pub accent: String,
    /// "reduced" (folded panel sections, small waves) or "detailed".
    pub density: String,
    /// "full" or "reduced" motion (added 2026-11-20).
    pub motion: String,
    /// The view Wake opens on (an id such as "monitor").
    pub start_view: String,
    pub remote_minutes: u32,
    /// Send the one "building while behind" desktop notification (the banner always shows).
    pub notify_behind: bool,
    /// Per view id: "column" or "column reversed". A view not listed uses Wake's own order.
    pub sorts: BTreeMap<String, String>,
    /// Projects: "grid" or "list" (added 2026-11-25).
    pub projects_layout: String,
    /// Projects: "groups", "activity" or "none" (added 2026-11-25).
    pub projects_group: String,
    /// The file as read, so keys Wake doesn't know survive a save.
    raw: Table,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            ai: "claude".into(),
            theme: "dark".into(),
            accent: "cyan".into(),
            density: "reduced".into(),
            motion: "full".into(),
            start_view: "monitor".into(),
            remote_minutes: 10,
            notify_behind: true,
            sorts: BTreeMap::new(),
            projects_layout: "grid".into(),
            projects_group: "groups".into(),
            raw: Table::new(),
        }
    }
}

/// What the core needs from the settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CoreOptions {
    pub question_ai: Ai,
    /// None: never ask the remotes.
    pub remote_every: Option<Duration>,
    pub notify_behind: bool,
    /// For screenshots: no Slipstream plugin card or jump requests, and no desktop notification
    /// of any kind (a no-op notifier). Never set from the settings file.
    pub quiet: bool,
}

impl Default for CoreOptions {
    fn default() -> Self {
        Settings::default().core()
    }
}

fn short_word(s: &str) -> bool {
    !s.is_empty() && s.len() <= 40 && s.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'_' || c == b' ')
}

impl Settings {
    pub fn core(&self) -> CoreOptions {
        CoreOptions {
            question_ai: Ai::from_id(&self.ai).unwrap_or_default(),
            remote_every: (self.remote_minutes > 0).then(|| Duration::from_secs(u64::from(self.remote_minutes) * 60)),
            notify_behind: self.notify_behind,
            quiet: false,
        }
    }

    /// The settings in `t`, each one falling back to its default when missing or not understood.
    pub fn from_table(t: Table) -> Settings {
        let d = Settings::default();
        let get = |sec: &str, key: &str| t.get(sec).and_then(Value::as_table).and_then(|s| s.get(key)).cloned();
        let word = |sec: &str, key: &str, allowed: &[&str], dflt: &str| {
            get(sec, key).and_then(|v| v.as_str().map(str::to_lowercase)).filter(|v| allowed.contains(&v.as_str())).unwrap_or_else(|| dflt.to_string())
        };
        let ais: Vec<&str> = Ai::ALL.iter().map(|a| a.id()).collect();
        let mut sorts = BTreeMap::new();
        if let Some(s) = t.get("sort").and_then(Value::as_table) {
            for (k, v) in s {
                if let Some(v) = v.as_str().filter(|v| short_word(v)).filter(|_| short_word(k)) {
                    sorts.insert(k.clone(), v.to_string());
                }
            }
        }
        Settings {
            ai: word("questions", "ai", &ais, &d.ai),
            theme: word("look", "theme", &THEMES, &d.theme),
            accent: word("look", "accent", &ACCENTS, &d.accent),
            density: word("look", "density", &DENSITIES, &d.density),
            motion: word("look", "motion", &MOTIONS, &d.motion),
            start_view: get("look", "start_view").and_then(|v| v.as_str().map(str::to_lowercase)).filter(|v| short_word(v)).unwrap_or(d.start_view),
            remote_minutes: get("git", "check_every_minutes").and_then(|v| v.as_integer()).and_then(|n| u32::try_from(n).ok()).filter(|n| REMOTE_MINUTES.contains(n)).unwrap_or(d.remote_minutes),
            notify_behind: get("git", "notify_when_behind").and_then(|v| v.as_bool()).unwrap_or(d.notify_behind),
            sorts,
            projects_layout: word("projects", "layout", &PROJECT_LAYOUTS, &d.projects_layout),
            projects_group: word("projects", "group_by", &PROJECT_GROUPINGS, &d.projects_group),
            raw: t,
        }
    }

    /// The table to write: the file as read, with Wake's own keys set from these settings.
    pub fn to_table(&self) -> Table {
        let mut t = self.raw.clone();
        let mut section = |name: &str, pairs: Vec<(&str, Value)>| {
            let entry = t.entry(name.to_string()).or_insert_with(|| Value::Table(Table::new()));
            if !entry.is_table() {
                *entry = Value::Table(Table::new());
            }
            if let Value::Table(s) = entry {
                for (k, v) in pairs {
                    s.insert(k.into(), v);
                }
            }
        };
        section("questions", vec![("ai", Value::String(self.ai.clone()))]);
        section(
            "look",
            vec![
                ("theme", Value::String(self.theme.clone())),
                ("accent", Value::String(self.accent.clone())),
                ("density", Value::String(self.density.clone())),
                ("motion", Value::String(self.motion.clone())),
                ("start_view", Value::String(self.start_view.clone())),
            ],
        );
        section("projects", vec![("layout", Value::String(self.projects_layout.clone())), ("group_by", Value::String(self.projects_group.clone()))]);
        section("git", vec![("check_every_minutes", Value::Integer(i64::from(self.remote_minutes))), ("notify_when_behind", Value::Boolean(self.notify_behind))]);
        // The sort table is Wake's entirely: a view back on Wake's order has no line.
        t.insert("sort".into(), Value::Table(self.sorts.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect()));
        t
    }
}

/// The settings file in `dir`, read. The note, if any, is one sentence for the person.
pub fn load(dir: &Path) -> (Settings, Option<String>) {
    let path = dir.join(FILE);
    let home = crate::store::home_dir();
    let text = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Settings::default(), None),
        Err(e) => {
            return (Settings::default(), Some(format!("Wake couldn't open its settings file {}, so it is using the defaults and left the file alone. ({e})", util::tilde(&path, &home))));
        }
    };
    match parse(&text) {
        Ok(t) => (Settings::from_table(t), None),
        Err(e) => match set_aside(&path) {
            Some(aside) => (Settings::default(), Some(format!("Wake's settings file was damaged, so it is using the defaults. Your old settings are kept as {}. ({e})", util::tilde(&aside, &home)))),
            None => (Settings::default(), Some(format!("Wake's settings file was damaged and couldn't be moved aside, so Wake is using the defaults and won't save over it. ({e})"))),
        },
    }
}

fn parse(text: &[u8]) -> Result<Table, String> {
    std::str::from_utf8(text).map_err(|e| e.to_string()).and_then(|s| s.parse::<Table>().map_err(|e| first_line(&e.to_string())))
}

/// The settings file, read only for `wake doctor`: unlike [`load`], a damaged file stays where it is.
pub fn check(dir: &Path) -> crate::store::FileCheck {
    use crate::store::FileCheck;
    match fs::read(dir.join(FILE)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => FileCheck::Missing,
        Err(e) => FileCheck::Unreadable(e.kind().to_string()),
        Ok(b) => match parse(&b) {
            Ok(t) => FileCheck::Fine(t.len()),
            Err(e) => FileCheck::Damaged(e),
        },
    }
}

fn first_line(s: &str) -> String {
    util::squash(s.lines().find(|l| !l.trim().is_empty()).unwrap_or("not TOML"))
}

/// Moves a broken file aside without ever replacing another file. The new name, or None.
fn set_aside(path: &Path) -> Option<PathBuf> {
    let stamp = util::now_iso().replace(':', "-");
    let dir = path.parent()?;
    let aside = (0..100).map(|n| dir.join(if n == 0 { format!("{FILE}.broken-{stamp}") } else { format!("{FILE}.broken-{stamp}-{n}") })).find(|p| !p.exists())?;
    // hard_link + remove never replaces an existing file, unlike rename
    (fs::hard_link(path, &aside).is_ok() && fs::remove_file(path).is_ok()).then_some(aside)
}

/// Writes the settings into `dir` atomically. Refuses to replace a file that isn't TOML (it may
/// hold the person's own words): `load` moves such a file aside first.
pub fn save(dir: &Path, s: &Settings) -> Result<(), String> {
    let path = dir.join(FILE);
    if let Ok(b) = fs::read(&path)
        && std::str::from_utf8(&b).ok().and_then(|t| t.parse::<Table>().ok()).is_none()
    {
        return Err(format!("{} can't be understood, so Wake won't save over it.", path.display()));
    }
    let body = toml::to_string(&s.to_table()).map_err(|e| format!("Wake couldn't write its settings: {e}"))?;
    let w = Writer::new(dir);
    w.ensure_data_dir().map_err(|e| e.0)?;
    w.write_atomic(&path, format!("{HEADER}{body}").as_bytes()).map_err(|e| format!("Wake couldn't save its settings: {}", e.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Scratch, scratch};
    use std::os::unix::fs::PermissionsExt;

    /// A settings folder not made yet, inside a scratch folder that goes when that is dropped.
    fn tmp(name: &str) -> (Scratch, PathBuf) {
        let s = scratch(&format!("settings-{name}"));
        let d = s.0.join("wake");
        (s, d)
    }

    #[test]
    fn missing_file_is_the_defaults_and_a_save_round_trips() {
        let (_s, dir) = tmp("round");
        let (s, note) = load(&dir);
        assert_eq!(s, Settings::default());
        assert!(note.is_none());
        let mut s = s;
        s.ai = "codex".into();
        s.theme = "light".into();
        s.density = "detailed".into();
        s.motion = "reduced".into();
        s.remote_minutes = 0;
        s.notify_behind = false;
        s.sorts.insert("monitor".into(), "last reversed".into());
        s.projects_layout = "list".into();
        s.projects_group = "activity".into();
        save(&dir, &s).unwrap();
        let mode = fs::metadata(dir.join(FILE)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        let (back, note) = load(&dir);
        assert!(note.is_none());
        assert_eq!((back.ai.as_str(), back.theme.as_str(), back.density.as_str(), back.remote_minutes, back.notify_behind), ("codex", "light", "detailed", 0, false));
        assert_eq!(back.motion, "reduced");
        assert_eq!((back.projects_layout.as_str(), back.projects_group.as_str()), ("list", "activity"));
        assert_eq!(back.sorts.get("monitor").map(String::as_str), Some("last reversed"));
        assert_eq!(back.core().remote_every, None);
        assert_eq!(back.core().question_ai, Ai::Codex);
    }

    #[test]
    fn unknown_keys_are_kept_and_odd_values_fall_back() {
        let (_s, dir) = tmp("keep");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(FILE), "future = 3\n[look]\ntheme = \"purple\"\naccent = \"MINT\"\nshiny = true\n[git]\ncheck_every_minutes = 7\nnotify_when_behind = \"yes\"\n[other]\nx = [1, 2]\n")
            .unwrap();
        let (s, note) = load(&dir);
        assert!(note.is_none());
        assert_eq!(s.theme, "dark", "not a theme Wake has");
        assert_eq!(s.accent, "mint", "case does not matter");
        assert_eq!(s.remote_minutes, 10, "7 isn't offered");
        assert!(s.notify_behind, "not a yes/no");
        save(&dir, &s).unwrap();
        let text = fs::read_to_string(dir.join(FILE)).unwrap();
        let t: Table = text.parse().unwrap();
        assert_eq!(t["future"].as_integer(), Some(3));
        assert_eq!(t["look"]["shiny"].as_bool(), Some(true));
        assert_eq!(t["other"]["x"].as_array().map(Vec::len), Some(2));
        assert_eq!(t["look"]["theme"].as_str(), Some("dark"));
        assert!(text.starts_with("# Wake's settings."));
    }

    #[test]
    fn a_broken_file_is_set_aside_with_a_plain_note() {
        let (_s, dir) = tmp("broken");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(FILE), "[look\ntheme = dark").unwrap();
        assert!(matches!(check(&dir), crate::store::FileCheck::Damaged(_)));
        assert!(dir.join(FILE).exists(), "checking moves nothing");
        let (s, note) = load(&dir);
        assert_eq!(s, Settings::default());
        let note = note.expect("says so");
        assert!(note.starts_with("Wake's settings file was damaged, so it is using the defaults. Your old settings are kept as ") && note.contains("settings.toml.broken-"), "{note}");
        assert_eq!(check(&dir), crate::store::FileCheck::Missing);
        assert!(!dir.join(FILE).exists());
        let kept: Vec<_> = fs::read_dir(&dir).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with("settings.toml.broken-")).collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(fs::read_to_string(kept[0].path()).unwrap(), "[look\ntheme = dark", "kept as it was");
        // the next save makes a fresh file beside it
        save(&dir, &s).unwrap();
        assert!(load(&dir).1.is_none());
        // a broken file appearing later is never saved over
        fs::write(dir.join(FILE), "not = [toml").unwrap();
        assert!(save(&dir, &s).is_err());
        assert_eq!(fs::read_to_string(dir.join(FILE)).unwrap(), "not = [toml");
    }
}
