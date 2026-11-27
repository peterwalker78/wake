//! New project (added 2026-11-27): the person's own AI asks a few questions about what they want
//! to make, then hands back a plan; once the person has checked it, Wake makes the folder (and runs
//! `git init` if they chose git), writes the description the agent reads first (CLAUDE.md for
//! Claude, AGENTS.md for Codex), tracks it, and opens a new terminal window with the agent on its
//! first task: setting up the rest, explaining as it goes and asking before installing anything.
//!
//! The interview runs through Questions (questions.rs) as a thread of its own kind: no tools at all
//! (it reads nothing on the machine), in an empty folder of Wake's, and never shown in Ask's list or
//! in Wake's session lists. This module holds the interviewer's instructions, the plan and its
//! tolerant parser, and the folder rules; `Live::create_project` (source.rs) does the making.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::Agent;

/// `QThread::kind` for an interview: Ask leaves these threads out.
pub const KIND: &str = "newproject";

/// The interview thread's title (shown only where Wake names its own processes, in Agents).
pub const TITLE: &str = "New project interview";

/// What Wake sends first, so the AI asks its first question.
pub const OPENER: &str = "I'd like to start a new project. Please ask me your first question.";

/// What "You decide" sends.
pub const YOU_DECIDE: &str = "You decide.";

/// The interviewer's instructions (Claude's system prompt; put before the first message for the
/// others).
pub const INTERVIEWER: &str = "You are helping someone start a new project from Wake, a calm desktop app for working with \
coding agents. They may be a beginner. Interview them briefly, then hand back a plan. You cannot see or use anything on their \
computer, and nothing is made until they have checked the plan, so never offer to make or run anything yourself.

How to ask:
- Ask exactly one question per message. Keep each message short: a sentence of reply at most, then the question.
- Use plain, friendly words and no jargon. If a technical word is needed, explain it in a few words.
- Cover these topics, in about this order, and skip any the person has already answered:
  1. The goal: what they want to make or do, and who it is for.
  2. Whether it is a technical project (an app, a website, a script) or not (writing, planning, research, organising, and so on).
  3. For a technical project, the tech to use. If they don't know, suggest one beginner-friendly choice and say why in one sentence.
  4. Whether to track changes with git. Explain it plainly: git keeps a history of every change, so they can go back to an earlier version at any time. Suggest yes for anything technical.
  5. A short name for the project.
- Ask a follow-up only when an answer is too unclear to plan with. Ask about six questions at most in all.
- If they answer \"you decide\", make a sensible choice, say what you chose in a few words, and move on.

When you have enough (or after about six questions), reply with a short plain summary of the plan (two to four sentences) \
followed by exactly one fenced ```json block, and nothing after it. Never put a json block in any earlier message. The block holds:
{\"name\": \"Project name\", \"folder_name\": \"lowercase-with-dashes\", \"summary\": \"one or two sentences\", \"technical\": true, \
\"tech\": [\"...\"], \"git\": true, \"description\": \"...\", \"first_task\": \"...\"}
- description: Markdown for the project's instructions file, which the coding agent reads every time it starts work here. \
Include a heading with the name; what the project is, its goal and who it is for; the tech (or, for a non-technical project, the \
kind of work and the files it will be, such as Markdown documents); and how to work with this person: explain things in plain \
words, keep steps small, and ask before installing anything, spending money or doing anything that can't be undone.
- first_task: the agent's first job, written to the agent: set up the starting files and folders for this project, explaining \
each step in plain words and asking before installing anything, then suggest what to do next.";

/// What the interview hands back, and what the person checks and edits before anything is made.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub name: String,
    /// The folder's name under the projects folder: lowercase letters, digits and dashes.
    pub folder_name: String,
    pub summary: String,
    pub technical: bool,
    pub tech: Vec<String>,
    pub git: bool,
    /// Becomes CLAUDE.md (Claude) or AGENTS.md (Codex).
    pub description: String,
    /// The agent's first task, sent as its brief.
    pub first_task: String,
}

/// The longest folder name Wake makes.
pub const MAX_SLUG: usize = 64;

/// `s` as a folder name Wake will make: lowercase letters, digits and single dashes, at most 64
/// characters, no dash at either end. Empty when nothing usable is left.
pub fn slug(s: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in s.trim().to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            if dash && !out.is_empty() {
                out.push('-');
            }
            dash = false;
            out.push(c);
        } else {
            dash = true;
        }
        if out.len() >= MAX_SLUG {
            break;
        }
    }
    out.truncate(MAX_SLUG);
    out.trim_end_matches('-').to_string()
}

/// True for a folder name Wake makes as it is (what [`slug`] gives back unchanged).
pub fn valid_slug(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_SLUG && slug(s) == s
}

/// The new project's folder, `<projects>/<folder>`, or one plain sentence on why Wake won't make it.
pub fn check_folder(projects: &Path, folder: &str) -> Result<PathBuf, String> {
    if folder.trim().is_empty() {
        return Err("Give the folder a name.".into());
    }
    if !valid_slug(folder) {
        return Err(format!("A folder name can only have lowercase letters, numbers and dashes, at most {MAX_SLUG} of them."));
    }
    let p = projects.join(folder);
    if p.symlink_metadata().is_ok() {
        return Err("A folder with that name already exists. Pick another name.".into());
    }
    Ok(p)
}

/// The file the agent reads first in the new folder.
pub fn file_name(agent: Agent) -> &'static str {
    match agent {
        Agent::Claude => "CLAUDE.md",
        _ => "AGENTS.md",
    }
}

/// The brief: the first task, then where to read about the project.
pub fn brief_text(plan: &Plan, agent: Agent) -> String {
    format!("{}\n\nRead {} first.", plan.first_task.trim(), file_name(agent))
}

/// A description written from the plan's other parts, for when the AI gave none.
pub fn default_description(name: &str, summary: &str, technical: bool, tech: &[String]) -> String {
    let mut d = format!("# {name}\n\n");
    if !summary.trim().is_empty() {
        d.push_str(summary.trim());
        d.push_str("\n\n");
    }
    if technical && !tech.is_empty() {
        d.push_str("## Tech\n\n");
        for t in tech {
            d.push_str(&format!("- {t}\n"));
        }
        d.push('\n');
    } else if !technical {
        d.push_str("This is not a software project: the work is writing and organising, kept as plain text and Markdown files.\n\n");
    }
    d.push_str("## Working with me\n\n- I may be new to this: explain things in plain words and keep steps small.\n- Ask before installing anything, spending money, or doing anything that can't be undone.\n");
    d
}

/// The first task when the AI gave none.
pub fn default_first_task(technical: bool) -> String {
    if technical {
        "Set up the starting files and folders for this project. Explain each step in plain words as you go, and ask me before installing anything. When it's set up, suggest what we could do next."
            .into()
    } else {
        "Set up a simple folder structure and the first documents for this project. Explain what each one is for in plain words, and ask me before installing anything. Then suggest what we could do next.".into()
    }
}

/// The fenced blocks in `text`: (info string, body), in order. An unclosed fence runs to the end.
fn fenced(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut open: Option<(String, String)> = None;
    for line in text.lines() {
        let t = line.trim_start();
        match &mut open {
            None => {
                if let Some(rest) = t.strip_prefix("```") {
                    open = Some((rest.trim().to_lowercase(), String::new()));
                }
            }
            Some((info, body)) => {
                if t.trim_end() == "```" {
                    out.push((std::mem::take(info), std::mem::take(body)));
                    open = None;
                } else {
                    body.push_str(line);
                    body.push('\n');
                }
            }
        }
    }
    if let Some((info, body)) = open {
        out.push((info, body));
    }
    out
}

fn string(v: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|k| match v.get(*k) {
            Some(Value::String(s)) => Some(s.trim().to_string()),
            Some(Value::Number(n)) => Some(n.to_string()),
            _ => None,
        })
        .unwrap_or_default()
}

fn boolean(v: &Value, key: &str) -> Option<bool> {
    match v.get(key)? {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.trim().to_lowercase().as_str() {
            "yes" | "true" | "y" => Some(true),
            "no" | "false" | "n" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn list(v: &Value, key: &str) -> Vec<String> {
    match v.get(key) {
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
        Some(Value::String(s)) => s.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
        _ => Vec::new(),
    }
}

/// The plan in an interview's last message: its last fenced json block that holds a JSON object.
/// Missing fields get sensible defaults; None when there is no such block. Never panics.
pub fn parse_plan(text: &str) -> Option<Plan> {
    let v = fenced(text)
        .into_iter()
        .rev()
        .filter(|(info, body)| info == "json" || (info.is_empty() && body.trim_start().starts_with('{')))
        .find_map(|(_, body)| serde_json::from_str::<Value>(&body).ok().filter(Value::is_object))?;
    let mut name = crate::util::squash(&string(&v, &["name", "title"]));
    let mut folder = slug(&string(&v, &["folder_name", "folder"]));
    if folder.is_empty() {
        folder = slug(&name);
    }
    if folder.is_empty() {
        folder = "new-project".into();
    }
    if name.is_empty() {
        name = folder.replace('-', " ");
    }
    let summary = string(&v, &["summary"]);
    let tech = list(&v, "tech");
    let technical = boolean(&v, "technical").unwrap_or(!tech.is_empty());
    let git = boolean(&v, "git").unwrap_or(technical);
    let mut description = string(&v, &["description"]);
    if description.is_empty() {
        description = default_description(&name, &summary, technical, &tech);
    }
    let mut first_task = string(&v, &["first_task", "firstTask"]);
    if first_task.is_empty() {
        first_task = default_first_task(technical);
    }
    Some(Plan { name, folder_name: folder, summary, technical, tech, git, description, first_task })
}

/// The message as the person reads it: without its plan block (the review shows the plan).
pub fn without_plan(text: &str) -> String {
    let mut out = String::new();
    let mut skipping = false;
    for line in text.lines() {
        let t = line.trim();
        if !skipping && (t == "```json" || t == "```JSON") {
            skipping = true;
            continue;
        }
        if skipping {
            if t == "```" {
                skipping = false;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "Here's the plan: a small recipe website for your family.\n\n```json\n{\"name\": \"Family Recipes\", \"folder_name\": \"family-recipes\", \"summary\": \"A website with the family's recipes.\", \"technical\": true, \"tech\": [\"HTML\", \"CSS\"], \"git\": true, \"description\": \"# Family Recipes\\n\\nA website.\", \"first_task\": \"Set up the starting files.\"}\n```\n";

    #[test]
    fn a_good_plan_reads_as_written() {
        let p = parse_plan(GOOD).unwrap();
        assert_eq!(p.name, "Family Recipes");
        assert_eq!(p.folder_name, "family-recipes");
        assert_eq!(p.tech, ["HTML", "CSS"]);
        assert!(p.technical && p.git);
        assert_eq!(p.description, "# Family Recipes\n\nA website.");
        assert_eq!(p.first_task, "Set up the starting files.");
        assert_eq!(without_plan(GOOD), "Here's the plan: a small recipe website for your family.");
    }

    #[test]
    fn missing_fields_get_sensible_defaults() {
        let p = parse_plan("Done.\n```json\n{\"name\": \"My Novel!\", \"technical\": \"no\"}\n```").unwrap();
        assert_eq!(p.folder_name, "my-novel");
        assert!(!p.technical && !p.git, "no git unless asked for a non-technical project");
        assert!(p.description.starts_with("# My Novel!\n"));
        assert!(p.first_task.contains("folder structure"));
        let p = parse_plan("```json\n{\"tech\": \"Python, Flask\"}\n```").unwrap();
        assert_eq!((p.name.as_str(), p.folder_name.as_str()), ("new project", "new-project"));
        assert_eq!(p.tech, ["Python", "Flask"]);
        assert!(p.technical && p.git);
        // A plain fence holding an object counts too; a folder name is made safe.
        let p = parse_plan("```\n{\"name\": \"x\", \"folder_name\": \"../../etc/Passwd Thing\"}\n```").unwrap();
        assert_eq!(p.folder_name, "etc-passwd-thing");
    }

    #[test]
    fn junk_is_no_plan() {
        for junk in ["", "What would you like to make?", "```json\nnot json\n```", "```json\n[1, 2]\n```", "```rust\n{\"name\": \"x\"}\n```", "```json\n{\"name\": "] {
            assert_eq!(parse_plan(junk), None, "{junk:?}");
        }
    }

    #[test]
    fn the_last_json_block_wins() {
        let two = "```json\n{\"name\": \"First\"}\n```\nOn second thought:\n```json\n{\"name\": \"Second\"}\n```\n";
        assert_eq!(parse_plan(two).unwrap().name, "Second");
        // A broken last block falls back to the one before it.
        let broken = "```json\n{\"name\": \"First\"}\n```\n```json\n{oops\n```\n";
        assert_eq!(parse_plan(broken).unwrap().name, "First");
    }

    #[test]
    fn slugs_are_safe_folder_names() {
        assert_eq!(slug("  Family Recipes! "), "family-recipes");
        assert_eq!(slug("../x/./y"), "x-y");
        assert_eq!(slug("Café"), "caf");
        assert_eq!(slug("---"), "");
        assert_eq!(slug(&"a".repeat(80)).len(), 64);
        assert!(!slug(&format!("{}-b", "a".repeat(63))).ends_with('-'));
        assert!(valid_slug("family-recipes") && valid_slug("a1"));
        for bad in ["", "Family", "a--b", "-a", "a-", "a b", "a/b", ".."] {
            assert!(!valid_slug(bad), "{bad}");
        }
        assert!(!valid_slug(&"a".repeat(65)));
    }

    #[test]
    fn folders_must_be_new_and_safe() {
        let s = crate::testing::scratch("np-folder");
        std::fs::create_dir_all(s.0.join("taken")).unwrap();
        assert_eq!(check_folder(&s.0, "fresh").unwrap(), s.0.join("fresh"));
        assert!(check_folder(&s.0, "taken").unwrap_err().contains("already exists"));
        assert!(check_folder(&s.0, "Bad Name").unwrap_err().contains("lowercase"));
        assert!(check_folder(&s.0, "").is_err());
        assert_eq!(file_name(Agent::Claude), "CLAUDE.md");
        assert_eq!(file_name(Agent::Codex), "AGENTS.md");
        let p = Plan { first_task: "Set it up.".into(), ..Plan::default() };
        assert_eq!(brief_text(&p, Agent::Codex), "Set it up.\n\nRead AGENTS.md first.");
    }
}
