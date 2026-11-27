//! Which project a session belongs to, when the person says so (added 2026-11-20).
//!
//! Wake works out a session's project from where its turns ran; a background job started in `~`
//! falls back to the files it changed, then to its title's words (see `index::attribute_how`),
//! and those guesses are marked "probably". "Set project" (the session panel, or `P` on a row)
//! stores the person's answer here, in Wake's own store: `~/.local/share/wake/session-projects.json`,
//! `{"sessions": {"<session id>": "<project id>" | null}}`, null meaning "no project". Read
//! tolerantly (a broken file is ignored, never overwritten until the next "Set project").

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::store::{self, ProjectRec, Writer};
use crate::types::{Attribution, SessionSummary};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    sessions: HashMap<String, Option<String>>,
}

/// Session id -> project id, or None for "no project".
type Answers = HashMap<String, Option<String>>;

/// The person's answers, read again whenever the file changes.
pub struct Overrides {
    path: PathBuf,
    cache: Mutex<(Option<SystemTime>, Answers)>,
}

impl Overrides {
    pub fn new(data: &Path) -> Overrides {
        Overrides { path: data.join("session-projects.json"), cache: Mutex::new((None, HashMap::new())) }
    }

    /// Every answer: session id -> project id, or None for "no project".
    pub fn all(&self) -> HashMap<String, Option<String>> {
        let m = std::fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        let mut c = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if m != c.0 {
            c.1 = store::read_json::<File>(&self.path).map(|f| f.sessions).unwrap_or_default();
            c.0 = m;
        }
        c.1.clone()
    }

    pub fn get(&self, session: &str) -> Option<Option<String>> {
        self.all().get(session).cloned()
    }

    /// Set (Some) or forget (None, back to Wake's own guess) the person's answer for a session.
    pub fn set(&self, writer: &Writer, session: &str, project: Option<Option<&str>>) -> Result<(), String> {
        if session.is_empty() || session.len() > 100 {
            return Err("Say which session.".into());
        }
        let mut all = self.all();
        match project {
            None => {
                all.remove(session);
            }
            Some(p) => {
                all.insert(session.to_string(), p.map(String::from));
            }
        }
        writer.write_json(&self.path, &File { sessions: all }).map_err(|e| e.0)?;
        *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = (None, HashMap::new());
        Ok(())
    }

    /// Put the person's answer, if any, on a summary. True when there was one.
    pub fn apply(&self, s: &mut SessionSummary, projects: &[ProjectRec]) -> bool {
        apply_to(&self.all(), s, projects)
    }
}

pub fn apply_to(all: &HashMap<String, Option<String>>, s: &mut SessionSummary, projects: &[ProjectRec]) -> bool {
    let Some(choice) = all.get(&s.id) else { return false };
    match choice.as_deref().and_then(|id| store::project_by_id(id, projects)) {
        Some(p) => {
            s.project = Some(p.id.clone());
            s.project_name = Some(p.name.clone());
        }
        None if choice.is_none() => {
            s.project = None;
            s.project_name = None;
        }
        None => return false, // a project since removed from projects.json: Wake's own guess
    }
    true
}

/// How a session's project was decided, for the overview: `how` is "set", "files", "title" or
/// "none"; sessions placed by where they worked are left out (nothing to say).
pub fn describe(session: &str, project: Option<(&str, &str)>, how: &str, launch_cwd: Option<&str>, home: &Path) -> Attribution {
    let project_name = project.map(|p| p.1);
    let at_home = launch_cwd.is_some_and(|c| store::norm_abs(c).as_deref() == store::norm_abs(&home.to_string_lossy()).as_deref());
    let words = match (how, project_name) {
        ("set", Some(n)) => format!("{n} (you set it)"),
        ("set", None) => "no project (you set it)".to_string(),
        ("files", Some(n)) => format!("probably {n} (it changed files there)"),
        ("title", Some(n)) => format!("probably {n} (from its title)"),
        _ => "no project".to_string(),
    };
    Attribution { session: session.to_string(), project: project.map(|p| p.0.to_string()), how: how.to_string(), words, started_at_home: at_home }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{self, Acc, How, OMap};

    fn proj(id: &str, root: &str) -> ProjectRec {
        serde_json::from_value(serde_json::json!({"id": id, "name": id, "roots": [root]})).unwrap()
    }

    fn projects() -> Vec<ProjectRec> {
        vec![proj("night-shift", "/h/Projects/night-shift"), proj("tideline-desktop", "/h/Projects/tideline-desktop"), proj("wake", "/h/Projects/wake"), proj("Gradebook", "/h/Projects/Gradebook")]
    }

    #[test]
    fn titles_name_projects_only_when_clear() {
        let p = projects();
        assert_eq!(index::project_from_title("night-shift-rota-update", &p).as_deref(), Some("night-shift"));
        assert_eq!(index::project_from_title("tideline-app-shortcut-jump", &p).as_deref(), Some("tideline-desktop"));
        assert_eq!(index::project_from_title("gradebook teacher invites", &p).as_deref(), Some("Gradebook"));
        // "night" alone is not "night-shift"; short first words never count.
        assert_eq!(index::project_from_title("night-bus-timetable", &p), None);
        assert_eq!(index::project_from_title("uk-vs-us-datacenter-comparison", &p), None);
        // Two projects named: no answer.
        assert_eq!(index::project_from_title("wake and night-shift notes", &p), None);
    }

    #[test]
    fn home_sessions_fall_back_to_files_then_title() {
        let p = projects();
        let home = Path::new("/h");
        let mut a = Acc { id: "s1".into(), launch_cwd: Some("/h".into()), ..Default::default() };
        a.proj_turns = OMap(vec![("/h".into(), 3)]);
        a.custom_title = Some("night-shift-rota-update".into());
        assert_eq!(index::attribute_how(&a, &p, home), (Some("night-shift".into()), Some("night-shift".into()), Some(How::Title)));
        a.files = OMap(vec![("/h/Projects/wake/crates/wake-core/src/disk.rs".into(), 4)]);
        // Files beat the title; and survive the folding the in-memory copy does.
        let lite = index::lite(&a);
        assert_eq!(index::attribute_how(&lite, &p, home).2, Some(How::Files));
        assert_eq!(index::attribute_how(&lite, &p, home).0.as_deref(), Some("wake"));
        a.proj_turns = OMap(vec![("/h/Projects/tideline-desktop".into(), 1)]);
        assert_eq!(index::attribute_how(&a, &p, home).2, Some(How::Worked));
    }

    #[test]
    fn the_persons_answer_wins_and_is_kept_in_wakes_store() {
        let s = crate::testing::scratch("attrib");
        let w = Writer::new(&s.0);
        let o = Overrides::new(&s.0);
        let p = projects();
        let mut sum: SessionSummary = serde_json::from_value(serde_json::json!({
            "id": "s1", "agent": "claude", "project": "wake", "project_name": "wake", "title": "t", "state": "idle",
            "evidence": "", "started_at": null, "last_at": null, "minutes": 0, "prompts": 0, "last_prompt": null,
            "away_summary": null, "question": null, "subagents": {"total": 0, "running": 0}, "cwd": null, "location": null,
            "resume_command": null, "job_id": null, "pid": null, "deleted_by_claude": false}))
        .unwrap();
        assert!(!o.apply(&mut sum, &p));
        o.set(&w, "s1", Some(Some("night-shift"))).unwrap();
        assert!(o.apply(&mut sum, &p));
        assert_eq!(sum.project_name.as_deref(), Some("night-shift"));
        o.set(&w, "s1", Some(None)).unwrap();
        assert!(o.apply(&mut sum, &p));
        assert_eq!(sum.project, None);
        o.set(&w, "s1", None).unwrap();
        assert_eq!(o.get("s1"), None);
        assert!(s.0.join("session-projects.json").exists());
        let d = describe("s1", Some(("night-shift", "night-shift")), "title", Some("/h"), Path::new("/h"));
        assert_eq!(d.words, "probably night-shift (from its title)");
        assert!(d.started_at_home);
        assert_eq!(describe("s2", None, "none", Some("/h"), Path::new("/h")).words, "no project");
    }
}
