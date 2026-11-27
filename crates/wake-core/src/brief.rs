//! Starting an agent on ideas (added 2026-11-20): the brief Wake writes, and the one command a new
//! terminal window runs for it.
//!
//! The brief goes in a file under `<data>/briefs/<id>.md` (Wake's own folder), never on the
//! command line, so nothing in an idea (quotes, `$`, backticks, line breaks) is ever seen by a
//! shell. What is typed is fixed: `claude "$(cat '<path>')"` (or `codex`), and it is typed, not
//! run: the person presses Enter in the tab, as with reopening a session.

use std::path::{Path, PathBuf};

use crate::store::{StoreError, Writer};

/// The agents Wake can start with a brief as their first prompt, interactively.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Claude,
    /// `codex [PROMPT]` starts the interactive CLI with that first prompt (checked with `codex --help`).
    Codex,
}

impl Agent {
    pub const ALL: [Agent; 2] = [Agent::Claude, Agent::Codex];

    pub fn id(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Agent::Claude => "Claude",
            Agent::Codex => "Codex",
        }
    }

    pub fn from_id(id: &str) -> Option<Agent> {
        Agent::ALL.into_iter().find(|a| a.id() == id)
    }
}

/// What the person asked for: these ideas, in this project, with this brief, to this agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartAgent {
    pub project: String,
    pub ideas: Vec<String>,
    pub brief: String,
    pub agent: String,
    /// The brief's file name (without `.md`), from [`new_id`]: the dialog shows the command with it.
    pub id: String,
}

/// The lead line and the ideas as a short list: the brief as the dialog first shows it.
pub fn brief_text(project_name: &str, ideas: &[&str]) -> String {
    let mut out = format!("Ideas for {project_name}, captured in Wake:\n");
    for i in ideas {
        // A multi-line idea stays one item: its later lines are indented under its dash.
        let mut lines = i.trim().lines();
        if let Some(first) = lines.next() {
            out.push_str(&format!("\n- {first}"));
        }
        for l in lines {
            out.push_str(&format!("\n  {l}"));
        }
    }
    out
}

/// A fresh brief id: `b` and twelve hex digits.
pub fn new_id() -> String {
    format!("b{}", crate::store::random_hex(6))
}

fn id_ok(id: &str) -> bool {
    id.len() == 13 && id.starts_with('b') && id[1..].chars().all(|c| c.is_ascii_hexdigit())
}

/// Where the brief with this id lives.
pub fn path(data: &Path, id: &str) -> PathBuf {
    data.join("briefs").join(format!("{id}.md"))
}

/// A path Wake will put between single quotes in a typed command: no quote, no control character.
fn path_ok(p: &str) -> bool {
    p.starts_with('/') && !p.chars().any(|c| c == '\'' || c == '\\' || c.is_control())
}

/// The command typed into the tab, or None when the path can't be typed safely.
pub fn command(agent: Agent, path: &Path) -> Option<String> {
    let p = path.to_str().filter(|p| path_ok(p))?;
    Some(format!("{} -- \"$(cat '{p}')\"", agent.id()))
}

/// None when `cmd` is exactly the command Wake types for a brief in `briefs` (one of the agents,
/// one brief file named by its id, nothing else), else why it was refused.
pub fn check_command(cmd: &str, briefs: &Path) -> Option<String> {
    let refuse = || Some("Refused to type this command: it isn't Wake's command for a brief.".to_string());
    let Some((prog, rest)) = cmd.split_once(' ') else { return refuse() };
    let Some(agent) = Agent::from_id(prog) else { return refuse() };
    let Some(path) = rest.strip_prefix("-- \"$(cat '").and_then(|r| r.strip_suffix("')\"")) else { return refuse() };
    let p = Path::new(path);
    let in_briefs = p.parent() == Some(briefs);
    let id_named = p.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(".md")).is_some_and(id_ok);
    if !in_briefs || !id_named || command(agent, p).as_deref() != Some(cmd) {
        return refuse();
    }
    None
}

/// Writes the brief (mode 0600, the folder 0700) and returns its path.
pub fn write(writer: &Writer, data: &Path, id: &str, text: &str) -> Result<PathBuf, StoreError> {
    if !id_ok(id) {
        return Err(StoreError("That brief id isn't one Wake makes.".into()));
    }
    let text = text.trim();
    if text.is_empty() {
        return Err(StoreError("The brief is empty.".into()));
    }
    let p = path(data, id);
    writer.write_atomic(&p, format!("{text}\n").as_bytes())?;
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_brief_is_a_lead_line_and_a_short_list() {
        let b = brief_text("Wake", &["Start agents from ideas", "  two\nlines  "]);
        assert_eq!(b, "Ideas for Wake, captured in Wake:\n\n- Start agents from ideas\n- two\n  lines");
    }

    #[test]
    fn the_command_is_fixed_and_checked() {
        let briefs = Path::new("/home/me/.local/share/wake/briefs");
        let p = briefs.join("b0123456789ab.md");
        let c = command(Agent::Claude, &p).unwrap();
        assert_eq!(c, "claude -- \"$(cat '/home/me/.local/share/wake/briefs/b0123456789ab.md')\"");
        assert_eq!(check_command(&c, briefs), None);
        assert_eq!(check_command(&command(Agent::Codex, &p).unwrap(), briefs), None);
        for bad in [
            "claude \"$(cat '/etc/passwd')\"",
            "claude \"$(cat '/home/me/.local/share/wake/briefs/../x.md')\"",
            "claude \"$(cat '/home/me/.local/share/wake/briefs/b0123456789ab.md')\"; rm -rf ~",
            "sh \"$(cat '/home/me/.local/share/wake/briefs/b0123456789ab.md')\"",
            "claude \"$(cat '/home/me/.local/share/wake/briefs/evil.md')\"",
            "claude --resume x",
            "",
        ] {
            assert!(check_command(bad, briefs).is_some(), "{bad}");
        }
        assert_eq!(command(Agent::Claude, Path::new("/home/o'brien/b.md")), None, "a quote in the path is never typed");
        assert_eq!(command(Agent::Claude, Path::new("/tmp/a\nb.md")), None);
    }

    #[test]
    fn a_shell_hands_the_agent_the_brief_byte_for_byte() {
        // What bash does with the typed line: the brief arrives as one argument, nothing expanded.
        let dir = std::env::temp_dir().join(format!("wake-brief-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let w = Writer::new(&dir);
        let nasty = ["it's \"quoted\"", "costs $HOME and $(id)", "`touch /tmp/pwned-wake`", "back\\slash; && | > *"];
        let text = brief_text("O'Neil's \"app\"", &nasty);
        let id = new_id();
        let p = write(&w, &dir, &id, &text).unwrap();
        let typed = command(Agent::Claude, &p).unwrap();
        assert_eq!(check_command(&typed, &dir.join("briefs")), None);
        // A shell function stands in for claude; $# proves the brief is one argument.
        let script = format!("f() {{ printf '%s|%s|%s' \"$#\" \"$1\" \"$2\"; }}; {}", typed.replacen("claude", "f", 1));
        let out = std::process::Command::new("bash").arg("-c").arg(&script).output().unwrap();
        assert_eq!(String::from_utf8(out.stdout).unwrap(), format!("2|--|{text}"));
        assert!(!Path::new("/tmp/pwned-wake").exists());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(p.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        assert!(write(&w, &dir, "../x", "hi").is_err());
        assert!(write(&w, &dir, &new_id(), "   ").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
