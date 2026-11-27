//! Questions: quick questions answered by the person's own `claude`, kept as threads.
//!
//! A question runs `claude -p --output-format stream-json --verbose --include-partial-messages`
//! (flags checked against `claude --help`, Claude Code 2.1.329), the question on stdin, and the
//! answer streams into the thread as Claude writes it. Follow-ups `--resume` the thread's session.
//! One question runs at a time (each claude process holds about 0.5 GB); others
//! wait in a queue. Claude may only search and read: "just a question" runs in
//! `<data>/questions/` with WebSearch and WebFetch; a project question runs in the project's folder
//! with reading inside that folder and WebSearch (no WebFetch). Nothing that changes files is ever
//! allowed, only the person's own settings load (`--setting-sources user`) and no hooks run.
//!
//! Store: `<data>/questions/threads/<id>.json`, one file per thread, written atomically; a file that
//! can't be understood is moved aside (`<id>.json.broken-<time>`), never overwritten.
//! `<data>/questions/sessions.jsonl` remembers every Claude session a question used, so Wake's own
//! views can leave those transcripts out of Monitor, Projects and History even after a thread is
//! deleted (see [`Hider`]).
//!
//! Stopping an answer signals only the child Wake spawned, through its `Child` handle (never by a
//! pid looked up later), and the child is asked to die with Wake (PR_SET_PDEATHSIG).
//!
//! Other AIs (added 2026-11-20; Settings picks which answers new questions, and each thread keeps
//! the one that answered it):
//! - **Codex** (`codex exec`, checked against codex-cli 0.171.0): `exec --json --sandbox read-only`
//!   with approvals off and no MCP servers, the question on stdin; follow-ups
//!   `exec … resume <thread id>`. Its sandbox lets it read and search but never write.
//! - **Gemini** (`gemini`, checked against 0.64.0): headless (`-p`) with `--output-format
//!   stream-json`, `--approval-mode plan` (read-only) and an `--admin-policy` file Wake writes in
//!   its questions folder that denies every tool except reading, searching and fetching (admin
//!   rules outrank any user or project rule). Not offered when `/etc/gemini-cli/policies` holds a
//!   policy, because then gemini ignores `--admin-policy`. Follow-ups `--resume <session id>`.
//!
//! New project interviews (added 2026-11-27, newproject.rs) are threads of their own kind
//! (`QThread::kind`), run by the same queue and worker: with no tools at all (Claude `--tools ""`;
//! Gemini under a policy that denies every tool), in an empty folder of Wake's
//! (`<data>/questions/new-project`, so the Hider leaves their sessions out like any question's),
//! and never in Ask's list. Codex can't be run with no tools, so an interview asked while Codex
//! answers questions goes to Claude.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::Value;

use crate::store::{self, Paths, Writer};
use crate::types::{Agent, ProcKind, ProcOverview, QOverview, QThread, QTurn};
use crate::util;

/// Tools for "just a question".
pub const JUST_TOOLS: &str = "WebSearch,WebFetch";
/// Tools for a question about a project: reading and searching only. No WebFetch: what it reads
/// in the project must not be able to send anything out.
pub const PROJECT_TOOLS: &str = "Read,Grep,Glob,WebSearch";
/// Allowed without asking in a project question: reading inside the question's folder only (a
/// path rule; Claude applies Read rules to Grep and Glob too). Reading elsewhere would need a
/// permission no one is there to give.
pub const PROJECT_ALLOWED: &str = "Read(./**),WebSearch";
/// Settings for every question: none of the person's or the project's hooks run.
const QUESTION_SETTINGS: &str = r#"{"disableAllHooks":true}"#;
/// Denied whatever else a settings file may allow.
pub const DENIED_TOOLS: &str = "Edit,Write,Bash,NotebookEdit";
/// Added to Claude's system prompt for every question.
const SYSTEM_NOTE: &str = "The person is asking a quick question from Wake, a calm console for their coding agents. \
Answer plainly and briefly in Markdown. You can search and read, but you cannot change any files or run commands.";

/// The environment variable that names this AI's program instead of $PATH: `WAKE_CLAUDE`.
fn env_var(ai: Agent) -> String {
    format!("WAKE_{}", ai.id().to_uppercase())
}

/// The AI a thread was asked of (threads from before there was a choice are Claude's).
pub fn thread_ai(t: &QThread) -> Agent {
    t.ai.as_deref().and_then(Agent::from_id).unwrap_or_default()
}

/// One AI as Settings offers it: whether Wake can use it here, and a short plain hint.
#[derive(Debug, Clone, PartialEq)]
pub struct AiChoice {
    pub ai: Agent,
    pub usable: bool,
    pub hint: String,
}

/// Where gemini looks for system policies; any there make it ignore `--admin-policy`.
pub const GEMINI_SYSTEM_POLICIES: &str = "/etc/gemini-cli/policies";

/// Gemini's rules for Wake's questions: no tool at all, except reading, searching and fetching.
pub const GEMINI_POLICY: &str = "# Written by Wake for its Questions view: Gemini may read, search and fetch pages, nothing else.\n\
# Wake rewrites this file before every Gemini question.\n\
[[rule]]\ntoolName = \"*\"\ndecision = \"deny\"\npriority = 100\n\n\
[[rule]]\ntoolName = [\"read_file\", \"read_many_files\", \"list_directory\", \"glob\", \"grep_search\", \"google_web_search\", \"web_fetch\"]\ndecision = \"allow\"\npriority = 900\n";

/// Gemini's rules for a New project interview: no tool at all.
pub const GEMINI_INTERVIEW_POLICY: &str = "# Written by Wake for its New project interview: Gemini may use no tool at all.\n\
# Wake rewrites this file before every interview message.\n\
[[rule]]\ntoolName = \"*\"\ndecision = \"deny\"\npriority = 100\n";

const MAX_QUESTION: usize = 20_000;
const MAX_ANSWER: usize = 200_000;
const MAX_THREADS: usize = 2000;
const MAX_FILE: u64 = 8 << 20;
const TITLE_CHARS: usize = 60;

// ------------------------------------------------------------------ the stream-json format

/// One line of `claude -p --output-format stream-json --verbose --include-partial-messages`, as
/// far as Questions cares. Lines from subagents (`parent_tool_use_id` set) are `Other`.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// `{"type":"system","subtype":"init","session_id":…}`
    Init {
        session_id: String,
    },
    /// A new assistant message begins (`stream_event` / `message_start`).
    MessageStart,
    /// Text as it is written (`stream_event` / `content_block_delta` / `text_delta`).
    TextDelta(String),
    /// Claude starts using a tool (`content_block_start` of a `tool_use` or `server_tool_use` block).
    ToolStart(String),
    /// A whole assistant message (`{"type":"assistant"}`): its text, for when no deltas came.
    Message {
        text: String,
    },
    /// `{"type":"result"}`: the end, with the session id and whether it failed.
    Result {
        session_id: Option<String>,
        is_error: bool,
        text: Option<String>,
    },
    /// A whole message that is its own paragraph (Codex's `agent_message` items).
    Block(String),
    /// Something went wrong but the run may go on; kept in case no result says why it ended.
    Warn(String),
    Other,
}

/// One line of `ai`'s streaming output.
pub fn parse_line_for(ai: Agent, line: &str) -> Event {
    match ai {
        Agent::Claude => parse_line(line),
        Agent::Codex => parse_codex(line),
        Agent::Gemini => parse_gemini(line),
    }
}

/// One line of `codex exec --json`: `thread.started`, `item.started` / `item.completed` (an
/// `agent_message`, a `command_execution`, a `web_search`, `reasoning`…), `turn.completed`,
/// `turn.failed`, `error`.
pub fn parse_codex(line: &str) -> Event {
    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else { return Event::Other };
    let s = |v: &Value, p: &str| v.pointer(p).and_then(Value::as_str).map(String::from);
    match v.get("type").and_then(Value::as_str) {
        Some("thread.started") => s(&v, "/thread_id").map_or(Event::Other, |session_id| Event::Init { session_id }),
        Some("item.started") => match v.pointer("/item/type").and_then(Value::as_str) {
            Some(kind @ ("command_execution" | "web_search" | "mcp_tool_call" | "file_change" | "reasoning")) => Event::ToolStart(kind.into()),
            _ => Event::Other,
        },
        Some("item.completed") => match v.pointer("/item/type").and_then(Value::as_str) {
            Some("agent_message") => s(&v, "/item/text").map_or(Event::Other, Event::Block),
            Some("error") => s(&v, "/item/message").map_or(Event::Other, Event::Warn),
            _ => Event::Other,
        },
        Some("turn.completed") => Event::Result { session_id: None, is_error: false, text: None },
        Some("turn.failed") => Event::Result { session_id: None, is_error: true, text: s(&v, "/error/message") },
        Some("error") => s(&v, "/message").map_or(Event::Other, Event::Warn),
        _ => Event::Other,
    }
}

/// One line of `gemini --output-format stream-json`: `init`, `message` (the assistant's in
/// pieces), `tool_use`, `tool_result`, `error`, `result`.
pub fn parse_gemini(line: &str) -> Event {
    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else { return Event::Other };
    let s = |p: &str| v.pointer(p).and_then(Value::as_str).map(String::from);
    match v.get("type").and_then(Value::as_str) {
        Some("init") => s("/session_id").map_or(Event::Other, |session_id| Event::Init { session_id }),
        Some("message") if v.get("role").and_then(Value::as_str) == Some("assistant") => s("/content").map_or(Event::Other, Event::TextDelta),
        Some("tool_use") => Event::ToolStart(s("/tool_name").unwrap_or_default()),
        Some("error") if v.get("severity").and_then(Value::as_str) != Some("warning") => s("/message").map_or(Event::Other, Event::Warn),
        Some("result") => {
            let ok = v.get("status").and_then(Value::as_str) == Some("success");
            Event::Result { session_id: None, is_error: !ok, text: if ok { None } else { s("/error/message").or_else(|| Some("Gemini stopped with an error.".into())) } }
        }
        _ => Event::Other,
    }
}

pub fn parse_line(line: &str) -> Event {
    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else { return Event::Other };
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(String::from);
    if v.get("parent_tool_use_id").is_some_and(|p| !p.is_null()) {
        return Event::Other;
    }
    match v.get("type").and_then(Value::as_str) {
        Some("system") if v.get("subtype").and_then(Value::as_str) == Some("init") => match s(&v, "session_id") {
            Some(session_id) => Event::Init { session_id },
            None => Event::Other,
        },
        Some("stream_event") => {
            let Some(e) = v.get("event") else { return Event::Other };
            match e.get("type").and_then(Value::as_str) {
                Some("message_start") => Event::MessageStart,
                Some("content_block_delta") => {
                    let d = e.get("delta");
                    match d.and_then(|d| d.get("type")).and_then(Value::as_str) {
                        Some("text_delta") => d.and_then(|d| s(d, "text")).map_or(Event::Other, Event::TextDelta),
                        _ => Event::Other,
                    }
                }
                Some("content_block_start") => {
                    let b = e.get("content_block");
                    match b.and_then(|b| b.get("type")).and_then(Value::as_str) {
                        Some("tool_use" | "server_tool_use") => Event::ToolStart(b.and_then(|b| s(b, "name")).unwrap_or_default()),
                        _ => Event::Other,
                    }
                }
                _ => Event::Other,
            }
        }
        Some("assistant") => {
            let text: String = v
                .pointer("/message/content")
                .and_then(Value::as_array)
                .map(|blocks| blocks.iter().filter(|b| b.get("type").and_then(Value::as_str) == Some("text")).filter_map(|b| b.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n\n"))
                .unwrap_or_default();
            Event::Message { text }
        }
        Some("result") => Event::Result {
            session_id: s(&v, "session_id"),
            is_error: v.get("is_error").and_then(Value::as_bool).unwrap_or(false) || v.get("subtype").and_then(Value::as_str).is_some_and(|t| t != "success"),
            text: s(&v, "result").or_else(|| v.get("errors").and_then(Value::as_array).and_then(|a| a.iter().filter_map(Value::as_str).next()).map(String::from)),
        },
        _ => Event::Other,
    }
}

/// What a tool use looks like to the person, in a few words.
pub fn tool_words(name: &str) -> &'static str {
    match name {
        "WebSearch" | "web_search" | "google_web_search" => "searching the web",
        "WebFetch" | "web_fetch" => "reading a web page",
        "Read" | "read_file" | "read_many_files" => "reading files",
        "Grep" | "Glob" | "grep_search" | "glob" | "list_directory" => "searching the project",
        "command_execution" => "looking through files",
        "reasoning" => "thinking",
        _ => "working",
    }
}

/// An answer as the events build it up.
#[derive(Debug, Default, Clone)]
pub struct Answer {
    pub text: String,
    pub session: Option<String>,
    pub activity: Option<&'static str>,
    /// Some once the result came: Ok, or Err with one plain line.
    pub done: Option<Result<(), String>>,
    /// The last problem reported along the way, for when the run ends without a result.
    pub warning: Option<String>,
    /// Whose answer this is (for its error lines).
    pub ai: Agent,
    streamed: bool,
    need_break: bool,
}

impl Answer {
    fn push(&mut self, t: &str) {
        if self.text.len() >= MAX_ANSWER {
            return;
        }
        if self.need_break {
            self.need_break = false;
            let trimmed = self.text.trim_end_matches(' ');
            if !trimmed.is_empty() && !trimmed.ends_with("\n\n") {
                self.text.truncate(trimmed.len());
                self.text.push_str(if self.text.ends_with('\n') { "\n" } else { "\n\n" });
            }
        }
        self.text.push_str(t);
    }

    pub fn feed(&mut self, ev: Event) {
        match ev {
            Event::Init { session_id } => self.session = Some(session_id),
            Event::MessageStart => {
                self.streamed = false;
                self.need_break = !self.text.is_empty();
            }
            Event::TextDelta(t) => {
                self.streamed = true;
                self.activity = None;
                self.push(&t);
            }
            Event::ToolStart(name) => {
                self.activity = Some(tool_words(&name));
                // What comes after a tool is a new paragraph (Gemini sends no message starts).
                self.need_break = !self.text.is_empty();
            }
            Event::Block(t) => {
                if !t.trim().is_empty() {
                    self.need_break = !self.text.is_empty();
                    self.streamed = true;
                    self.activity = None;
                    self.push(&t);
                }
            }
            Event::Warn(w) => self.warning = Some(w),
            Event::Message { text } => {
                if !self.streamed && !text.is_empty() {
                    self.streamed = true;
                    self.push(&text);
                }
            }
            Event::Result { session_id, is_error, text } => {
                if session_id.is_some() {
                    self.session = session_id;
                }
                self.activity = None;
                if is_error {
                    let fallback = format!("{} stopped with an error.", self.ai.label());
                    let raw = text.or_else(|| self.warning.clone()).unwrap_or(fallback);
                    self.done = Some(Err(plain_error_for(self.ai, &raw)));
                } else {
                    if self.text.trim().is_empty()
                        && let Some(t) = text
                    {
                        self.text = t;
                    }
                    self.done = Some(Ok(()));
                }
            }
            Event::Other => {}
        }
    }
}

/// One plain line for the person from whatever `ai` said went wrong.
pub fn plain_error_for(ai: Agent, raw: &str) -> String {
    let low = raw.to_lowercase();
    let signed_out = ["not logged in", "/login", "invalid api key", "please log in", "authentication", "auth method", "401 unauthorized", "codex login", "gemini_api_key"];
    if signed_out.iter().any(|w| low.contains(w)) {
        return format!("{} isn't logged in. Run {} once in a terminal to log in, then retry.", ai.label(), ai.id());
    }
    let fallback = format!("{} stopped with an error.", ai.label());
    let line = raw.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or(&fallback);
    let line = util::squash(line);
    if line.chars().count() > 200 { format!("{}…", line.chars().take(199).collect::<String>()) } else { line }
}

/// The arguments for one question. `resume` continues `session`; otherwise a new conversation
/// starts under that id (so Wake knows it before Claude writes anything).
pub fn claude_args(session: &str, resume: bool, project: bool) -> Vec<String> {
    let (tools, allowed) = if project { (PROJECT_TOOLS, PROJECT_ALLOWED) } else { (JUST_TOOLS, JUST_TOOLS) };
    let mut a: Vec<String> = ["-p", "--output-format", "stream-json", "--verbose", "--include-partial-messages"].map(String::from).to_vec();
    a.push(if resume { "--resume".into() } else { "--session-id".into() });
    a.push(session.into());
    for (flag, v) in [("--tools", tools), ("--allowedTools", allowed), ("--disallowedTools", DENIED_TOOLS)] {
        a.push(flag.into());
        a.push(v.into());
    }
    // Nothing may ask for permission (no one is there to answer); no MCP servers (they could act);
    // only the person's own settings, never the project's, and no hooks at all.
    a.extend(["--permission-prompts", "none", "--strict-mcp-config", "--setting-sources", "user", "--settings", QUESTION_SETTINGS, "--append-system-prompt", SYSTEM_NOTE].map(String::from));
    a
}

/// The arguments for one message of a New project interview: like a question's, but with no tool
/// at all (`--tools ""`), every tool that changes anything denied besides, and the interviewer's
/// instructions as the whole system prompt.
pub fn interview_args(session: &str, resume: bool) -> Vec<String> {
    let mut a: Vec<String> = ["-p", "--output-format", "stream-json", "--verbose", "--include-partial-messages"].map(String::from).to_vec();
    a.push(if resume { "--resume".into() } else { "--session-id".into() });
    a.push(session.into());
    a.extend(["--tools", "", "--disallowedTools", DENIED_TOOLS].map(String::from));
    a.extend(
        ["--permission-prompts", "none", "--strict-mcp-config", "--setting-sources", "user", "--settings", QUESTION_SETTINGS, "--system-prompt", crate::newproject::INTERVIEWER].map(String::from),
    );
    a
}

/// Wake's note to the AI: the question note, or the interviewer's instructions.
fn note(interview: bool) -> &'static str {
    if interview { crate::newproject::INTERVIEWER } else { SYSTEM_NOTE }
}

/// The first question as sent: Codex takes no system prompt, so Wake's note goes before it
/// (Claude gets it as `--append-system-prompt`, Gemini after the question with `-p`).
pub fn first_prompt(ai: Agent, question: &str) -> String {
    first_prompt_for(ai, question, false)
}

fn first_prompt_for(ai: Agent, question: &str, interview: bool) -> String {
    match ai {
        Agent::Codex => format!("{}\n\n{question}", note(interview)),
        _ => question.to_string(),
    }
}

/// A follow-up for an AI whose session can't be resumed: the thread so far, then the question.
pub fn thread_so_far(turns: &[QTurn], question: &str) -> String {
    thread_so_far_for(turns, question, false)
}

fn thread_so_far_for(turns: &[QTurn], question: &str, interview: bool) -> String {
    let mut out = format!("{}\n\nThe conversation so far:\n", note(interview));
    for t in turns.iter().filter(|t| t.answered_at.is_some()) {
        out.push_str(&format!("\nQuestion: {}\nAnswer: {}\n", t.question, t.answer));
    }
    out.push_str(&format!("\nThe new question: {question}"));
    out
}

/// The arguments for one question to Codex: `codex exec --json --sandbox read-only`, approvals off,
/// no MCP servers, the prompt on stdin (`-`). `resume` continues that thread (session) id.
pub fn codex_args(resume: Option<&str>) -> Vec<String> {
    let mut a: Vec<String> =
        ["exec", "--json", "--sandbox", "read-only", "--skip-git-repo-check", "--color", "never", "-c", "sandbox_mode=\"read-only\"", "-c", "approval_policy=\"never\"", "-c", "mcp_servers={}"]
            .map(String::from)
            .to_vec();
    if let Some(id) = resume {
        a.push("resume".into());
        a.push(id.into());
    }
    a.push("-".into());
    a
}

/// The arguments for one question to Gemini: headless, streaming JSON, plan (read-only) mode and
/// Wake's admin policy; the question comes on stdin and `-p` adds Wake's note after it.
pub fn gemini_args(session: &str, resume: bool, policy: &Path) -> Vec<String> {
    gemini_args_with(session, resume, policy, SYSTEM_NOTE)
}

fn gemini_args_with(session: &str, resume: bool, policy: &Path, note: &str) -> Vec<String> {
    // No MCP server and no extension starts: a project's own config could name any command.
    let mut a: Vec<String> = ["--output-format", "stream-json", "--approval-mode", "plan", "--allowed-mcp-server-names", "none", "--extensions", "none", "--admin-policy"].map(String::from).to_vec();
    a.push(policy.to_string_lossy().into_owned());
    a.push(if resume { "--resume".into() } else { "--session-id".into() });
    a.push(session.into());
    a.push("-p".into());
    a.push(note.into());
    a
}

/// The `claude` to run: $WAKE_CLAUDE, else the first on $PATH, else ~/.local/bin/claude (the
/// installer's place, often missing from a desktop session's PATH).
pub fn find_claude(paths: &Paths) -> PathBuf {
    find_tool(paths, Agent::Claude).unwrap_or_else(|| paths.home.join(".local/bin/claude"))
}

/// Where `ai`'s program is: its $WAKE_… variable, else $PATH, else the places installers use that
/// a desktop session's PATH often lacks (~/.local/bin, Homebrew on Linux). None if not installed.
pub fn find_tool(paths: &Paths, ai: Agent) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(env_var(ai)).filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(p));
    }
    crate::run::which(ai.id()).or_else(|| [paths.home.join(".local/bin"), PathBuf::from("/home/linuxbrew/.linuxbrew/bin")].into_iter().map(|d| d.join(ai.id())).find(|c| c.is_file()))
}

/// Whether Wake can keep `ai` read-only here, given where its program is (None: not installed)
/// and whether the system has gemini policies of its own.
pub fn choice(ai: Agent, program: Option<&Path>, gemini_system_policies: bool) -> AiChoice {
    let (usable, hint) = match (ai, program) {
        (Agent::Claude, _) => (true, "Searches and reads; Wake turns off every tool that changes files.".to_string()),
        (_, None) => (false, format!("{} isn't installed here.", ai.label())),
        (Agent::Codex, Some(_)) => (true, "Runs in Codex's read-only sandbox: it reads and searches, never writes.".to_string()),
        (Agent::Gemini, Some(_)) if gemini_system_policies => (false, format!("Not offered: a system policy in {GEMINI_SYSTEM_POLICIES} would override the rules that keep it read-only.")),
        (Agent::Gemini, Some(_)) => (true, "Runs in plan mode with Wake's rules: it reads and searches, every tool that writes is off.".to_string()),
    };
    AiChoice { ai, usable, hint }
}

/// The choice for an AI whose program isn't here.
pub fn not_installed(ai: Agent) -> AiChoice {
    AiChoice { ai, usable: false, hint: format!("{} isn't installed here.", ai.label()) }
}

/// True when gemini's system policy folder holds a policy (it then ignores `--admin-policy`).
pub fn gemini_has_system_policies(dir: &Path) -> bool {
    fs::read_dir(dir).is_ok_and(|rd| rd.flatten().any(|e| e.path().extension().is_some_and(|x| x == "toml")))
}

// ------------------------------------------------------------------ the store

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    if fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf)).is_err() {
        let t = util::now().to_bits() ^ ((std::process::id() as u64) << 32);
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (t.rotate_left(i as u32 * 7) as u8) ^ (i as u8).wrapping_mul(53);
        }
    }
    buf
}

/// A random (version 4) UUID, as `--session-id` wants.
pub fn new_uuid() -> String {
    let mut b: [u8; 16] = random_bytes();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

fn new_thread_id() -> String {
    let b: [u8; 5] = random_bytes();
    format!("q{}", b.iter().map(|x| format!("{x:02x}")).collect::<String>())
}

/// Ids are file names: short, lowercase letters, digits and dashes only.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

/// The first question, on one line, shortened.
pub fn title_of(question: &str) -> String {
    let one = util::squash(question);
    if one.chars().count() <= TITLE_CHARS { one } else { format!("{}…", one.chars().take(TITLE_CHARS - 1).collect::<String>().trim_end()) }
}

/// The thread files.
#[derive(Debug, Clone)]
pub struct QStore {
    dir: PathBuf,
    writer: Writer,
}

impl QStore {
    pub fn new(paths: &Paths) -> QStore {
        QStore { dir: paths.data.join("questions"), writer: Writer::new(&paths.data) }
    }

    /// Where "just a question" runs.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn threads_dir(&self) -> PathBuf {
        self.dir.join("threads")
    }

    fn sessions_path(&self) -> PathBuf {
        self.dir.join("sessions.jsonl")
    }

    pub fn ensure(&self) -> Result<(), String> {
        self.writer.ensure_data_dir().map_err(|e| e.0)?;
        self.writer.ensure_dir(&self.threads_dir()).map_err(|e| e.0)
    }

    /// Every thread that can be read, newest asked first, and a sentence about any that could not
    /// (each such file is moved aside, never overwritten).
    pub fn load_all(&self) -> (Vec<QThread>, Option<String>) {
        let mut out = Vec::new();
        let mut kept = Vec::new();
        let Ok(rd) = fs::read_dir(self.threads_dir()) else { return (out, None) };
        for e in rd.flatten() {
            let p = e.path();
            let Some(stem) = p.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(".json")).map(String::from) else { continue };
            if !valid_id(&stem) || store::is_forbidden(&p) {
                continue;
            }
            let bytes = match fs::File::open(&p) {
                Ok(f) => {
                    let mut buf = Vec::new();
                    if f.take(MAX_FILE).read_to_end(&mut buf).is_err() {
                        continue;
                    }
                    buf
                }
                Err(_) => continue,
            };
            match serde_json::from_slice::<QThread>(&bytes) {
                Ok(mut t) => {
                    t.id = stem;
                    out.push(t);
                }
                Err(_) => {
                    let stamp = util::now_iso().replace(':', "-");
                    let aside = (0..100).map(|n| self.threads_dir().join(if n == 0 { format!("{stem}.json.broken-{stamp}") } else { format!("{stem}.json.broken-{stamp}-{n}") })).find(|a| !a.exists());
                    // hard_link + remove never replaces an existing file, unlike rename
                    if let Some(a) = aside
                        && fs::hard_link(&p, &a).is_ok()
                        && fs::remove_file(&p).is_ok()
                    {
                        kept.push(a.display().to_string());
                    }
                }
            }
        }
        out.sort_by(|a, b| last_asked(b).cmp(last_asked(a)).then_with(|| a.id.cmp(&b.id)));
        out.truncate(MAX_THREADS);
        let problem = (!kept.is_empty()).then(|| {
            format!("{} question thread{} could not be read and {} kept aside: {}", kept.len(), if kept.len() == 1 { "" } else { "s" }, if kept.len() == 1 { "was" } else { "were" }, kept.join(", "))
        });
        (out, problem)
    }

    pub fn save(&self, t: &QThread) -> Result<(), String> {
        if !valid_id(&t.id) {
            return Err("That thread id can't be a file name.".into());
        }
        self.writer.write_json(&self.threads_dir().join(format!("{}.json", t.id)), t).map_err(|e| e.0)
    }

    pub fn delete(&self, id: &str) -> Result<(), String> {
        if !valid_id(id) {
            return Err("No such thread.".into());
        }
        match fs::remove_file(self.threads_dir().join(format!("{id}.json"))) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("Wake couldn't delete that thread: {e}")),
        }
    }

    pub fn known_sessions(&self) -> HashSet<String> {
        store::iter_jsonl(&self.sessions_path()).filter_map(|v| v.get("session").and_then(Value::as_str).map(String::from)).collect()
    }

    pub fn remember_session(&self, sid: &str) {
        let _ = self.writer.append_jsonl(&self.sessions_path(), &serde_json::json!({"session": sid, "at": util::now_iso()}));
    }
}

/// True for a New project interview's thread.
fn is_interview(t: &QThread) -> bool {
    t.kind.as_deref() == Some(crate::newproject::KIND)
}

fn last_asked(t: &QThread) -> &str {
    t.turns.last().map_or(t.created.as_str(), |x| x.asked_at.as_str())
}

// ------------------------------------------------------------------ the runner

/// Where a question runs: the project (None for "just a question") and its folder.
#[derive(Debug, Clone, PartialEq)]
pub struct Context {
    pub project: Option<String>,
    pub cwd: PathBuf,
}

struct Job {
    thread: String,
    turn: usize,
    ctx: Context,
}

/// How one question is sent: to which AI, in which session, and the words that go on stdin.
struct Plan {
    ai: Agent,
    /// A New project interview: no tools, the interviewer's instructions.
    interview: bool,
    /// None for a new Codex thread (Codex picks its own id).
    session: Option<String>,
    resume: bool,
    prompt: String,
}

struct Run {
    thread: String,
    child: Option<Arc<Mutex<Child>>>,
    pid: Option<u32>,
    activity: Option<&'static str>,
    cancelled: bool,
}

#[derive(Default)]
struct St {
    loaded: bool,
    threads: Vec<QThread>,
    known: HashSet<String>,
    queue: VecDeque<Job>,
    running: Option<Run>,
    unread: Vec<String>,
    seq: u64,
    problem: Option<String>,
    worker: bool,
}

struct Shared {
    store: QStore,
    /// Each AI's program (Claude's always, the others when installed).
    tools: Mutex<HashMap<Agent, PathBuf>>,
    /// Which AI answers new threads.
    current: Mutex<Agent>,
    /// The questions folder as written and as the filesystem resolves it (~ may be a symlink).
    dirs: Vec<String>,
    st: Mutex<St>,
    cv: Condvar,
    /// An answer with no output for this long is stopped ([`IDLE_LIMIT`]; shorter in tests).
    idle_limit: Mutex<Duration>,
}

/// A running answer that prints nothing for this long has stalled: Wake stops it.
pub const IDLE_LIMIT: Duration = Duration::from_secs(600);

/// The questions engine: threads in memory, the queue, and the one worker that runs claude.
/// Cheap to clone. Nothing starts until the first question.
#[derive(Clone)]
pub struct Questions {
    sh: Arc<Shared>,
}

/// What Wake's other views need to leave question sessions out, taken once per refresh.
#[derive(Debug, Clone, Default)]
pub struct Hider {
    dirs: Vec<String>,
    known: HashSet<String>,
    /// Claude session -> thread title.
    titles: HashMap<String, String>,
    /// The claude process answering now, and its thread's title.
    running: Option<(u32, String)>,
}

impl Hider {
    /// True for a session a question started: by its id, or because it ran in the questions folder.
    pub fn hides(&self, session: &str, cwd: Option<&str>) -> bool {
        self.known.contains(session)
            || cwd.is_some_and(|c| {
                let c = c.trim_end_matches('/');
                self.dirs.iter().any(|d| c == d || c.strip_prefix(d.as_str()).is_some_and(|r| r.starts_with('/')))
            })
    }

    /// Name question processes in the Agents view ("Wake question: <title>"); one answering now is
    /// in use, never stale.
    pub fn label_procs(&self, ov: &mut ProcOverview) {
        for p in ov.procs.iter_mut() {
            let running = self.running.as_ref().filter(|(pid, _)| *pid == p.pid).map(|(_, t)| t.clone());
            let title = running.clone().or_else(|| p.session_id.as_ref().and_then(|s| self.titles.get(s).cloned()));
            let Some(title) = title else { continue };
            p.title = Some(format!("Wake question: {title}"));
            if matches!(p.kind, ProcKind::Stray | ProcKind::Spare) {
                p.kind = ProcKind::Session;
            }
            if running.is_some() && p.stale_reason.take().is_some() {
                ov.stale_count = ov.stale_count.saturating_sub(1);
                ov.stale_rss_bytes = ov.stale_rss_bytes.saturating_sub(p.rss_bytes);
            }
        }
    }
}

fn lock(m: &Mutex<St>) -> MutexGuard<'_, St> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Questions {
    pub fn new(paths: &Paths, claude: PathBuf) -> Questions {
        let store = QStore::new(paths);
        let lexical = store.dir().to_string_lossy().trim_end_matches('/').to_string();
        let real = store::realpath(store.dir()).to_string_lossy().trim_end_matches('/').to_string();
        // Fedora Atomic's /home is /var/home, but a container may mount it as a real /home: a
        // transcript can name the folder either way, whichever side asked.
        let mut dirs: Vec<String> = Vec::new();
        for d in [lexical, real] {
            let other = d.strip_prefix("/var/home/").map(|r| format!("/home/{r}")).or_else(|| d.strip_prefix("/home/").map(|r| format!("/var/home/{r}")));
            for x in std::iter::once(d).chain(other) {
                if !dirs.contains(&x) {
                    dirs.push(x);
                }
            }
        }
        let tools = Mutex::new(HashMap::from([(Agent::Claude, claude)]));
        Questions {
            sh: Arc::new(Shared { store, tools, current: Mutex::new(Agent::Claude), dirs, st: Mutex::new(St { seq: 1, ..St::default() }), cv: Condvar::new(), idle_limit: Mutex::new(IDLE_LIMIT) }),
        }
    }

    /// Use `program` for `ai` (Live sets the installed ones; tests a fake).
    pub fn set_tool(&self, ai: Agent, program: Option<PathBuf>) {
        let mut t = self.sh.tools.lock().unwrap_or_else(|e| e.into_inner());
        match program {
            Some(p) => t.insert(ai, p),
            None => t.remove(&ai),
        };
    }

    fn tool(&self, ai: Agent) -> Option<PathBuf> {
        self.sh.tools.lock().unwrap_or_else(|e| e.into_inner()).get(&ai).cloned()
    }

    /// Every AI, whether Wake can use it here, and a hint (for Settings).
    pub fn choices(&self) -> Vec<AiChoice> {
        let system = gemini_has_system_policies(Path::new(GEMINI_SYSTEM_POLICIES));
        // Claude's program is always set (its installer's usual place when it isn't on PATH), so
        // it counts as installed only when that file is there; said like the others when not.
        Agent::ALL
            .into_iter()
            .map(|ai| match self.tool(ai) {
                Some(p) if ai == Agent::Claude && !p.is_file() => not_installed(ai),
                p => choice(ai, p.as_deref(), system),
            })
            .collect()
    }

    /// Which AI answers new questions from now on (one Wake can't use here falls back to Claude).
    pub fn set_ai(&self, ai: Agent) {
        *self.sh.current.lock().unwrap_or_else(|e| e.into_inner()) = ai;
    }

    fn current_ai(&self) -> Agent {
        let ai = *self.sh.current.lock().unwrap_or_else(|e| e.into_inner());
        if self.choices().iter().any(|c| c.ai == ai && c.usable) { ai } else { Agent::Claude }
    }

    /// Where "just a question" runs.
    pub fn dir(&self) -> PathBuf {
        self.sh.store.dir().to_path_buf()
    }

    fn loaded(&self) -> MutexGuard<'_, St> {
        let mut st = lock(&self.sh.st);
        if !st.loaded {
            st.loaded = true;
            let (mut threads, problem) = self.sh.store.load_all();
            // An interview left from before can't be picked up again: its dialog is gone.
            threads.retain(|t| {
                let old = is_interview(t);
                if old {
                    let _ = self.sh.store.delete(&t.id);
                }
                !old
            });
            st.known = self.sh.store.known_sessions();
            for t in threads.iter_mut() {
                if let Some(s) = &t.claude_session {
                    st.known.insert(s.clone());
                }
                // Wake was closed while this was being answered (or waiting): say so, keep what came.
                let mut changed = false;
                for turn in t.turns.iter_mut().filter(|x| x.answered_at.is_none() && x.error.is_none()) {
                    turn.error = Some("Not answered: Wake closed before Claude finished.".into());
                    changed = true;
                }
                if changed {
                    let _ = self.sh.store.save(t);
                }
            }
            st.threads = threads;
            st.problem = problem;
            st.seq += 1;
        }
        st
    }

    pub fn snapshot(&self, since: u64) -> Option<QOverview> {
        let st = self.loaded();
        if since != 0 && since == st.seq {
            return None;
        }
        // Ask's threads only: interviews belong to the New project dialog.
        let hidden: HashSet<&str> = st.threads.iter().filter(|t| is_interview(t)).map(|t| t.id.as_str()).collect();
        let mut waiting: Vec<String> = Vec::new();
        for j in st.queue.iter().filter(|j| !hidden.contains(j.thread.as_str())) {
            if !waiting.contains(&j.thread) {
                waiting.push(j.thread.clone());
            }
        }
        let running = st.running.as_ref().filter(|r| !hidden.contains(r.thread.as_str()));
        Some(QOverview {
            threads: st.threads.iter().filter(|t| !is_interview(t)).cloned().collect(),
            answering: running.map(|r| r.thread.clone()),
            activity: running.and_then(|r| r.activity).map(String::from),
            waiting,
            unread: st.unread.iter().filter(|u| !hidden.contains(u.as_str())).cloned().collect(),
            seq: st.seq,
            problem: st.problem.clone(),
        })
    }

    pub fn hider(&self) -> Hider {
        let st = self.loaded();
        let titles: HashMap<String, String> = st.threads.iter().filter_map(|t| t.claude_session.as_ref().map(|s| (s.clone(), t.title.clone()))).collect();
        let running = st.running.as_ref().and_then(|r| {
            let title = st.threads.iter().find(|t| t.id == r.thread).map(|t| t.title.clone())?;
            Some((r.pid?, title))
        });
        Hider { dirs: self.sh.dirs.clone(), known: st.known.clone(), titles, running }
    }

    /// The project a thread was asked in (Some(None) for "just a question"; None if no such thread).
    pub fn thread_project(&self, id: &str) -> Option<Option<String>> {
        self.loaded().threads.iter().find(|t| t.id == id).map(|t| t.project.clone())
    }

    /// Ask: a follow-up in `thread`, or a new thread with `ctx`. Returns the thread id at once.
    pub fn ask(&self, thread: Option<&str>, question: &str, ctx: Context) -> Result<String, String> {
        self.ask_as(thread, question, ctx, None)
    }

    /// Where interviews run: an empty folder of Wake's, inside the questions folder.
    pub fn interview_dir(&self) -> PathBuf {
        self.sh.store.dir().join("new-project")
    }

    /// The AI that runs a New project interview: the one answering questions, except Codex, which
    /// can't be run with no tools at all (Claude then).
    pub fn interview_ai(&self) -> Agent {
        match self.current_ai() {
            Agent::Codex => Agent::Claude,
            ai => ai,
        }
    }

    /// A New project interview: the person's answer in `thread`, or (None) a new interview that
    /// starts with `answer`. Same queue as questions; returns the thread id at once.
    pub fn interview(&self, thread: Option<&str>, answer: &str) -> Result<String, String> {
        if let Some(id) = thread
            && !self.is_interview(id)
        {
            return Err("That interview is gone.".into());
        }
        let dir = self.interview_dir();
        self.sh.store.ensure()?;
        self.sh.store.writer.ensure_dir(&dir).map_err(|e| e.0)?;
        self.ask_as(thread, answer, Context { project: None, cwd: dir }, Some(crate::newproject::KIND))
    }

    /// True for a New project interview's thread.
    pub fn is_interview(&self, id: &str) -> bool {
        self.loaded().threads.iter().any(|t| t.id == id && is_interview(t))
    }

    /// One interview as an overview holding just its thread, or None when there is no such
    /// interview or (`since`) nothing changed.
    pub fn interview_state(&self, id: &str, since: u64) -> Option<QOverview> {
        let st = self.loaded();
        if since != 0 && since == st.seq {
            return None;
        }
        let t = st.threads.iter().find(|t| t.id == id && is_interview(t))?.clone();
        let running = st.running.as_ref().filter(|r| r.thread == id);
        Some(QOverview {
            answering: running.map(|r| r.thread.clone()),
            activity: running.and_then(|r| r.activity).map(String::from),
            waiting: if st.queue.iter().any(|j| j.thread == id) { vec![id.to_string()] } else { Vec::new() },
            unread: Vec::new(),
            seq: st.seq,
            problem: None,
            threads: vec![t],
        })
    }

    /// Send an interview's last answer again after it failed or was stopped.
    pub fn retry_interview(&self, id: &str) -> Result<(), String> {
        if !self.is_interview(id) {
            return Err("That interview is gone.".into());
        }
        self.retry(id, Context { project: None, cwd: self.interview_dir() })
    }

    fn ask_as(&self, thread: Option<&str>, question: &str, ctx: Context, kind: Option<&str>) -> Result<String, String> {
        let question = question.trim();
        if question.is_empty() {
            return Err("Type a question first.".into());
        }
        if question.chars().count() > MAX_QUESTION {
            return Err("That question is too long for Wake to send.".into());
        }
        self.sh.store.ensure()?;
        let mut st = self.loaded();
        let turn = QTurn { question: question.into(), asked_at: util::now_iso(), ..QTurn::default() };
        let (id, idx) = match thread {
            Some(id) => {
                let pos = st.threads.iter().position(|t| t.id == id).ok_or("That thread is gone.")?;
                let mut t = st.threads.remove(pos);
                t.turns.push(turn);
                let idx = t.turns.len() - 1;
                self.sh.store.save(&t)?;
                st.threads.insert(0, t);
                (id.to_string(), idx)
            }
            None => {
                let ai = if kind.is_some() { self.interview_ai() } else { self.current_ai() };
                let t = QThread {
                    id: new_thread_id(),
                    title: if kind.is_some() { crate::newproject::TITLE.to_string() } else { title_of(question) },
                    project: ctx.project.clone(),
                    claude_session: None,
                    created: util::now_iso(),
                    turns: vec![turn],
                    ai: Some(ai.id().into()),
                    kind: kind.map(String::from),
                };
                self.sh.store.save(&t)?;
                let id = t.id.clone();
                st.threads.insert(0, t);
                (id, 0)
            }
        };
        st.queue.push_back(Job { thread: id.clone(), turn: idx, ctx });
        st.seq += 1;
        self.start_worker(&mut st);
        self.sh.cv.notify_all();
        Ok(id)
    }

    /// Ask a thread's last question again, if it failed or was stopped.
    pub fn retry(&self, thread: &str, ctx: Context) -> Result<(), String> {
        let mut st = self.loaded();
        let busy = st.running.as_ref().is_some_and(|r| r.thread == thread) || st.queue.iter().any(|j| j.thread == thread);
        if busy {
            return Err("That question is already being answered.".into());
        }
        let t = st.threads.iter_mut().find(|t| t.id == thread).ok_or("That thread is gone.")?;
        let idx = t.turns.len().checked_sub(1).ok_or("Nothing to retry.")?;
        let last = &mut t.turns[idx];
        if last.error.is_none() {
            return Err("That question was answered.".into());
        }
        last.error = None;
        last.answer.clear();
        last.answered_at = None;
        last.asked_at = util::now_iso();
        let t = t.clone();
        self.sh.store.save(&t)?;
        st.queue.push_back(Job { thread: thread.into(), turn: idx, ctx });
        st.seq += 1;
        self.start_worker(&mut st);
        self.sh.cv.notify_all();
        Ok(())
    }

    /// Stop `thread`'s answer, or drop its queued questions. True if there was anything.
    pub fn cancel(&self, thread: &str) -> bool {
        let mut st = self.loaded();
        let mut any = false;
        let dropped: Vec<Job> = {
            let (drop, keep): (VecDeque<Job>, VecDeque<Job>) = std::mem::take(&mut st.queue).into_iter().partition(|j| j.thread == thread);
            st.queue = keep;
            drop.into_iter().collect()
        };
        for j in dropped {
            any = true;
            if let Some(t) = st.threads.iter_mut().find(|t| t.id == j.thread)
                && let Some(turn) = t.turns.get_mut(j.turn)
            {
                turn.error = Some("Stopped before Claude answered.".into());
                let t = t.clone();
                let _ = self.sh.store.save(&t);
            }
        }
        let mut child = None;
        if let Some(r) = st.running.as_mut().filter(|r| r.thread == thread) {
            any = true;
            r.cancelled = true;
            child.clone_from(&r.child);
        }
        if any {
            st.seq += 1;
        }
        drop(st);
        if let Some(c) = child {
            stop_child(c);
        }
        any
    }

    pub fn delete(&self, thread: &str) -> Result<(), String> {
        self.cancel(thread);
        self.sh.store.delete(thread)?;
        let mut st = self.loaded();
        st.threads.retain(|t| t.id != thread);
        st.unread.retain(|t| t != thread);
        st.seq += 1;
        Ok(())
    }

    pub fn mark_read(&self, thread: &str) {
        let mut st = self.loaded();
        let before = st.unread.len();
        st.unread.retain(|t| t != thread);
        if st.unread.len() != before {
            st.seq += 1;
        }
    }

    fn start_worker(&self, st: &mut St) {
        if st.worker {
            return;
        }
        let me = self.clone();
        // The worker lives as long as Wake does: PR_SET_PDEATHSIG fires when the thread that
        // spawned a child ends, so it must not end while a child runs.
        if std::thread::Builder::new().name("wake-questions".into()).spawn(move || me.work()).is_ok() {
            st.worker = true;
        }
    }

    fn work(&self) {
        loop {
            let job = {
                let mut st = lock(&self.sh.st);
                loop {
                    if let Some(j) = st.queue.pop_front() {
                        st.running = Some(Run { thread: j.thread.clone(), child: None, pid: None, activity: None, cancelled: false });
                        st.seq += 1;
                        break j;
                    }
                    st = self.sh.cv.wait(st).unwrap_or_else(|e| e.into_inner());
                }
            };
            self.run(&job);
        }
    }

    /// Runs one question to the end. Never panics the worker: every failure ends as the turn's error.
    fn run(&self, job: &Job) {
        // Which conversation: continue the thread's once a turn has been answered; otherwise a new
        // one (Claude and Gemini start under an id Wake picks; Codex says its id when it starts).
        let plan = {
            let mut st = lock(&self.sh.st);
            let Some(t) = st.threads.iter_mut().find(|t| t.id == job.thread) else {
                st.running = None;
                st.seq += 1;
                return;
            };
            let Some(question) = t.turns.get(job.turn).map(|x| x.question.clone()) else {
                st.running = None;
                st.seq += 1;
                return;
            };
            let ai = thread_ai(t);
            let interview = is_interview(t);
            let answered_before = t.turns[..job.turn].iter().any(|x| x.answered_at.is_some());
            let (session, resume) = match (&t.claude_session, answered_before) {
                (Some(s), true) => (Some(s.clone()), true),
                _ if ai == Agent::Codex => (None, false),
                _ => (Some(new_uuid()), false),
            };
            let prompt = if resume {
                question
            } else if answered_before {
                // Its session is lost: send the thread so far instead.
                thread_so_far_for(&t.turns[..job.turn], &question, interview)
            } else {
                first_prompt_for(ai, &question, interview)
            };
            if !resume && let Some(sid) = &session {
                t.claude_session = Some(sid.clone());
                let t = t.clone();
                let _ = self.sh.store.save(&t);
                st.known.insert(sid.clone());
                self.sh.store.remember_session(sid);
            }
            Plan { ai, interview, session, resume, prompt }
        };
        let outcome = self.run_ai(job, &plan);
        let mut st = lock(&self.sh.st);
        let cancelled = st.running.as_ref().is_some_and(|r| r.cancelled);
        st.running = None;
        st.seq += 1;
        let (answer, result) = outcome;
        if let Some(s) = &answer.session
            && !st.known.contains(s)
        {
            st.known.insert(s.clone());
            self.sh.store.remember_session(s);
        }
        let Some(t) = st.threads.iter_mut().find(|t| t.id == job.thread) else { return };
        if let Some(s) = answer.session.clone() {
            t.claude_session = Some(s);
        }
        if let Some(turn) = t.turns.get_mut(job.turn) {
            turn.answer.clone_from(&answer.text);
            match (cancelled, result) {
                (true, _) => turn.error = Some("Stopped.".into()),
                (false, Ok(())) => turn.answered_at = Some(util::now_iso()),
                (false, Err(e)) => turn.error = Some(e),
            }
        }
        let t = t.clone();
        let _ = self.sh.store.save(&t);
        st.unread.retain(|x| x != &job.thread);
        st.unread.push(job.thread.clone());
    }

    /// The program and arguments for one run, in `cwd`.
    fn command(&self, job: &Job, plan: &Plan) -> Result<(PathBuf, Vec<String>), String> {
        let missing = |ai: Agent| format!("Wake couldn't find {} here. Install it, or pick another AI in Settings.", ai.id());
        match plan.ai {
            Agent::Claude => {
                let prog = self.tool(Agent::Claude).ok_or_else(|| missing(Agent::Claude))?;
                let session = plan.session.as_deref().unwrap_or_default();
                Ok((prog, if plan.interview { interview_args(session, plan.resume) } else { claude_args(session, plan.resume, job.ctx.project.is_some()) }))
            }
            // Codex can't be run with no tools: an interview is never sent to it.
            Agent::Codex if plan.interview => Err("Codex can't run the New project interview. Pick Claude or Gemini in Settings.".into()),
            Agent::Codex => {
                let prog = self.tool(Agent::Codex).ok_or_else(|| missing(Agent::Codex))?;
                Ok((prog, codex_args(plan.session.as_deref().filter(|_| plan.resume))))
            }
            Agent::Gemini => {
                let prog = self.tool(Agent::Gemini).ok_or_else(|| missing(Agent::Gemini))?;
                if gemini_has_system_policies(Path::new(GEMINI_SYSTEM_POLICIES)) {
                    return Err(format!("Wake won't ask Gemini here: a system policy in {GEMINI_SYSTEM_POLICIES} would override the rules that keep it read-only."));
                }
                // Wake's own rules, written fresh each time so nothing else can loosen them.
                self.sh.store.ensure()?;
                let (name, rules) = if plan.interview { ("gemini-interview.toml", GEMINI_INTERVIEW_POLICY) } else { ("gemini-readonly.toml", GEMINI_POLICY) };
                let policy = self.sh.store.dir().join(name);
                self.sh.store.writer.write_atomic(&policy, rules.as_bytes()).map_err(|e| e.0)?;
                Ok((prog, gemini_args_with(plan.session.as_deref().unwrap_or_default(), plan.resume, &policy, note(plan.interview))))
            }
        }
    }

    fn run_ai(&self, job: &Job, plan: &Plan) -> (Answer, Result<(), String>) {
        let ai = plan.ai;
        let mut ans = Answer { ai, ..Answer::default() };
        if job.ctx.project.is_none() {
            // The questions folder is Wake's own; a project folder is only ever read.
            let _ = self.sh.store.ensure();
        }
        if !job.ctx.cwd.is_dir() {
            return (ans, Err(format!("The folder {} isn't there any more.", job.ctx.cwd.display())));
        }
        let (prog, args) = match self.command(job, plan) {
            Ok(c) => c,
            Err(e) => return (ans, Err(e)),
        };
        let mut cmd = Command::new(&prog);
        cmd.args(args)
            .current_dir(&job.ctx.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Started from inside a Claude Code session, claude would refuse to nest.
            .env_remove("CLAUDECODE")
            .env_remove("CLAUDE_CODE_ENTRYPOINT")
            // Its own process group, so a stop reaches whatever it started too.
            .process_group(0);
        if ai == Agent::Gemini {
            // Signed out, gemini would open a browser to sign in (and read the question as the
            // answer to "continue?"); this way it only prints a link and fails, which Wake reports.
            cmd.env("NO_BROWSER", "true");
        }
        // The child dies (SIGTERM) when Wake does.
        crate::sys::end_with_parent(&mut cmd);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return (
                    ans,
                    Err(match ai {
                        Agent::Claude => format!("Wake couldn't find claude ({}). Install Claude Code, or set WAKE_CLAUDE to its path.", prog.display()),
                        _ => format!("Wake couldn't find {} ({}). Install it, or pick another AI in Settings.", ai.id(), prog.display()),
                    }),
                );
            }
            Err(e) => return (ans, Err(format!("Wake couldn't start {}: {e}", ai.id()))),
        };
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let pid = child.id();
        let child = Arc::new(Mutex::new(child));
        let cancelled = {
            let mut st = lock(&self.sh.st);
            match st.running.as_mut() {
                Some(r) => {
                    r.child = Some(child.clone());
                    r.pid = Some(pid);
                    st.seq += 1;
                    st.running.as_ref().is_some_and(|r| r.cancelled)
                }
                None => false,
            }
        };
        if cancelled {
            stop_child(child.clone());
        }
        if let Some(mut i) = stdin {
            let q = plan.prompt.clone();
            // A question larger than the pipe would block here while claude waits to start.
            let _ = std::thread::Builder::new().name("wake-question-in".into()).spawn(move || {
                let _ = i.write_all(q.as_bytes());
            });
        }
        let err_text = Arc::new(Mutex::new(String::new()));
        let err_reader = stderr.and_then(|e| {
            let sink = err_text.clone();
            std::thread::Builder::new()
                .name("wake-question-err".into())
                .spawn(move || {
                    let mut buf = Vec::new();
                    let _ = e.take(16 * 1024).read_to_end(&mut buf);
                    *sink.lock().unwrap_or_else(|e| e.into_inner()) = String::from_utf8_lossy(&buf).into_owned();
                })
                .ok()
        });
        // A stalled answer (nothing printed for the idle limit) is stopped as the Stop button would.
        let quiet_since = Arc::new(Mutex::new(std::time::Instant::now()));
        let finished = Arc::new(AtomicBool::new(false));
        let stalled = Arc::new(AtomicBool::new(false));
        {
            let (quiet, done, stall, c) = (quiet_since.clone(), finished.clone(), stalled.clone(), child.clone());
            let limit = *self.sh.idle_limit.lock().unwrap_or_else(|e| e.into_inner());
            let _ = std::thread::Builder::new().name("wake-question-watch".into()).spawn(move || {
                let tick = (limit / 10).min(Duration::from_secs(5));
                while !done.load(Ordering::SeqCst) {
                    std::thread::sleep(tick);
                    if quiet.lock().unwrap_or_else(|e| e.into_inner()).elapsed() > limit && !done.load(Ordering::SeqCst) {
                        stall.store(true, Ordering::SeqCst);
                        stop_child(c);
                        return;
                    }
                }
            });
        }
        if let Some(out) = stdout {
            let mut r = BufReader::new(out);
            let mut line = Vec::new();
            loop {
                line.clear();
                match r.read_until(b'\n', &mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => *quiet_since.lock().unwrap_or_else(|e| e.into_inner()) = std::time::Instant::now(),
                }
                let ev = parse_line_for(ai, &String::from_utf8_lossy(&line));
                if ev == Event::Other {
                    continue;
                }
                ans.feed(ev);
                let mut st = lock(&self.sh.st);
                // An id the AI chose itself (Codex) is known at once, so its session never shows
                // in Wake's lists, even while it is being written.
                if let Some(sid) = &ans.session
                    && !st.known.contains(sid)
                {
                    st.known.insert(sid.clone());
                    self.sh.store.remember_session(sid);
                }
                if let Some(r) = st.running.as_mut() {
                    r.activity = ans.activity;
                }
                if let Some(t) = st.threads.iter_mut().find(|t| t.id == job.thread)
                    && let Some(turn) = t.turns.get_mut(job.turn)
                    && turn.answer != ans.text
                {
                    turn.answer.clone_from(&ans.text);
                }
                st.seq += 1;
            }
        }
        // Reap it without holding the lock for long, so a stop can always get in.
        let status = loop {
            let s = child.lock().unwrap_or_else(|e| e.into_inner()).try_wait();
            match s {
                Ok(Some(s)) => break Some(s),
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(_) => break None,
            }
        };
        finished.store(true, Ordering::SeqCst);
        if let Some(h) = err_reader {
            let _ = h.join();
        }
        let result = match ans.done.clone() {
            Some(r) => r,
            None => {
                let err = err_text.lock().unwrap_or_else(|e| e.into_inner()).clone();
                match status {
                    _ if stalled.load(Ordering::SeqCst) => Err(format!("{} went quiet for too long, so Wake stopped it.", ai.label())),
                    Some(s) if s.success() && !ans.text.trim().is_empty() => Ok(()),
                    Some(s) if s.success() => Err(format!("{} ended without an answer.", ai.label())),
                    _ if !err.trim().is_empty() => Err(plain_error_for(ai, &err)),
                    _ if ans.warning.is_some() => Err(plain_error_for(ai, ans.warning.as_deref().unwrap_or_default())),
                    Some(s) => Err(format!("{} stopped with an error ({s}).", ai.id())),
                    None => Err(format!("{} stopped with an error.", ai.id())),
                }
            }
        };
        (ans, result)
    }
}

/// SIGTERM to the child Wake spawned and its process group (through its handle, so never a reused
/// pid), then SIGKILL if it is still there three seconds later.
fn stop_child(child: Arc<Mutex<Child>>) {
    let _ = std::thread::Builder::new().name("wake-question-stop".into()).spawn(move || {
        {
            let mut c = child.lock().unwrap_or_else(|e| e.into_inner());
            if matches!(c.try_wait(), Ok(None)) {
                // The child is not yet reaped (try_wait said so, under the lock the reaper also
                // takes), so its pid, which is also its group's id (process_group(0)), is still
                // this process's.
                crate::sys::kill_group(c.id(), libc::SIGTERM);
            }
        }
        for _ in 0..30 {
            std::thread::sleep(Duration::from_millis(100));
            if !matches!(child.lock().unwrap_or_else(|e| e.into_inner()).try_wait(), Ok(None)) {
                return;
            }
        }
        let mut c = child.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(c.try_wait(), Ok(None)) {
            // As above, under the same lock.
            crate::sys::kill_group(c.id(), libc::SIGKILL);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::testenv::Env;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;

    // Lines as Claude Code 2.1.329 wrote them for "What is 2+2? Answer in one word." (usage and
    // rate-limit details trimmed), plus a tool use and the error shapes.
    const INIT: &str = r#"{"type":"system","subtype":"init","cwd":"/home/sam/.local/share/wake/questions","session_id":"2d9e6b13-8a40-4f7c-9b21-5c3e7f0a1d84","tools":["WebFetch","WebSearch"],"model":"claude-opus-5-5"}"#;
    const HOOK: &str = r#"{"type":"system","subtype":"hook_started","hook_id":"b8f7","hook_name":"SessionStart:startup","hook_event":"SessionStart","uuid":"f2d4","session_id":"2d9e6b13-8a40-4f7c-9b21-5c3e7f0a1d84"}"#;
    const START: &str = r#"{"type":"stream_event","event":{"type":"message_start","message":{"model":"claude-opus-5-5","id":"msg_011","type":"message","role":"assistant","content":[]}},"session_id":"2d9e6b13-8a40-4f7c-9b21-5c3e7f0a1d84","parent_tool_use_id":null,"uuid":"aa"}"#;
    const BLOCK: &str = r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}},"session_id":"2d9e6b13-8a40-4f7c-9b21-5c3e7f0a1d84","parent_tool_use_id":null,"uuid":"2ccb"}"#;
    const DELTA: &str = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Four."}},"session_id":"2d9e6b13-8a40-4f7c-9b21-5c3e7f0a1d84","parent_tool_use_id":null,"uuid":"c548"}"#;
    const WHOLE: &str = r#"{"type":"assistant","message":{"model":"claude-opus-5-5","id":"msg_011","type":"message","role":"assistant","content":[{"type":"text","text":"Four."}]},"parent_tool_use_id":null,"session_id":"2d9e6b13-8a40-4f7c-9b21-5c3e7f0a1d84","uuid":"x"}"#;
    const RESULT: &str = r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":1085,"num_turns":1,"result":"Four.","stop_reason":"end_turn","session_id":"2d9e6b13-8a40-4f7c-9b21-5c3e7f0a1d84","total_cost_usd":0.408572,"terminal_reason":"completed"}"#;
    const TOOL: &str = r#"{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"server_tool_use","id":"srvtoolu_1","name":"web_search","input":{}}},"session_id":"s","parent_tool_use_id":null,"uuid":"t"}"#;
    const SUB_DELTA: &str =
        r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"subagent text"}},"session_id":"s","parent_tool_use_id":"toolu_9","uuid":"u"}"#;
    const LOGIN: &str = r#"{"type":"result","subtype":"success","is_error":true,"result":"Not logged in · Please run /login","session_id":"5d0e0000-0000-4000-8000-000000000000"}"#;
    const EXEC_ERR: &str = r#"{"type":"result","subtype":"error_during_execution","is_error":false,"errors":["Something broke\nat line 2"],"session_id":"6e"}"#;

    #[test]
    fn real_stream_lines_parse() {
        assert_eq!(parse_line(INIT), Event::Init { session_id: "2d9e6b13-8a40-4f7c-9b21-5c3e7f0a1d84".into() });
        assert_eq!(parse_line(HOOK), Event::Other);
        assert_eq!(parse_line(START), Event::MessageStart);
        assert_eq!(parse_line(BLOCK), Event::Other);
        assert_eq!(parse_line(DELTA), Event::TextDelta("Four.".into()));
        assert_eq!(parse_line(WHOLE), Event::Message { text: "Four.".into() });
        assert_eq!(parse_line(TOOL), Event::ToolStart("web_search".into()));
        assert_eq!(parse_line(SUB_DELTA), Event::Other, "a subagent's words are not the answer");
        assert_eq!(parse_line("not json"), Event::Other);
        match parse_line(RESULT) {
            Event::Result { session_id, is_error, text } => {
                assert_eq!(session_id.as_deref(), Some("2d9e6b13-8a40-4f7c-9b21-5c3e7f0a1d84"));
                assert!(!is_error);
                assert_eq!(text.as_deref(), Some("Four."));
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn deltas_build_the_answer_once_and_the_result_ends_it() {
        let mut a = Answer::default();
        for l in [HOOK, INIT, START, BLOCK, DELTA, WHOLE, RESULT] {
            a.feed(parse_line(l));
        }
        assert_eq!(a.text, "Four.", "the whole message repeats the deltas; it must not be added twice");
        assert_eq!(a.session.as_deref(), Some("2d9e6b13-8a40-4f7c-9b21-5c3e7f0a1d84"));
        assert_eq!(a.done, Some(Ok(())));
        // a second message (after a web search) starts a new paragraph
        let mut a = Answer::default();
        a.feed(Event::MessageStart);
        a.feed(Event::TextDelta("Let me look.".into()));
        a.feed(parse_line(TOOL));
        assert_eq!(a.activity, Some("searching the web"));
        a.feed(Event::MessageStart);
        a.feed(Event::TextDelta("It is 4.".into()));
        assert_eq!(a.activity, None);
        assert_eq!(a.text, "Let me look.\n\nIt is 4.");
        // without partial messages, whole messages still make the answer
        let mut a = Answer::default();
        for l in [INIT, WHOLE, RESULT] {
            a.feed(parse_line(l));
        }
        assert_eq!(a.text, "Four.");
    }

    #[test]
    fn error_results_become_one_plain_line() {
        let mut a = Answer::default();
        a.feed(parse_line(LOGIN));
        assert_eq!(a.done, Some(Err("Claude isn't logged in. Run claude once in a terminal to log in, then retry.".into())));
        let mut a = Answer::default();
        a.feed(parse_line(EXEC_ERR));
        assert_eq!(a.done, Some(Err("Something broke".into())));
        assert_eq!(plain_error_for(Agent::Claude, &"x".repeat(500)).chars().count(), 200);
    }

    #[test]
    fn arguments_never_allow_changing_files() {
        let a = claude_args("sid", false, false);
        let joined = a.join(" ");
        assert!(joined.starts_with("-p --output-format stream-json --verbose --include-partial-messages --session-id sid"));
        let after = |f: &str| a.iter().position(|x| x == f).map(|i| a[i + 1].clone()).unwrap();
        assert_eq!(after("--tools"), "WebSearch,WebFetch");
        assert_eq!(after("--allowedTools"), "WebSearch,WebFetch");
        assert_eq!(after("--disallowedTools"), "Edit,Write,Bash,NotebookEdit");
        assert_eq!(after("--permission-prompts"), "none");
        assert!(a.contains(&"--strict-mcp-config".to_string()));
        let p = claude_args("sid", true, true);
        assert_eq!(p[5..7], ["--resume".to_string(), "sid".to_string()]);
        let after = |f: &str| p.iter().position(|x| x == f).map(|i| p[i + 1].clone()).unwrap();
        assert_eq!(after("--tools"), "Read,Grep,Glob,WebSearch");
        assert_eq!(after("--allowedTools"), "Read(./**),WebSearch", "reading only inside the project");
        for bad in ["Edit", "Write", "Bash", "NotebookEdit", "WebFetch", "Read"] {
            assert!(!after("--allowedTools").split(',').any(|t| t == bad));
        }
        // Only the person's settings, and no hooks, for either kind.
        for v in [&a, &p] {
            let after = |f: &str| v.iter().position(|x| x == f).map(|i| v[i + 1].clone()).unwrap();
            assert_eq!(after("--setting-sources"), "user");
            assert_eq!(serde_json::from_str::<Value>(&after("--settings")).unwrap()["disableAllHooks"], true);
        }
    }

    #[test]
    fn store_round_trip_and_a_broken_file_is_set_aside() {
        let env = Env::new();
        let s = QStore::new(&env.paths);
        s.ensure().unwrap();
        let t = QThread {
            id: "qabc".into(),
            title: "Why is the sky blue?".into(),
            project: Some("alpha".into()),
            claude_session: Some("sid-1".into()),
            created: "2026-11-20T10:00:00Z".into(),
            turns: vec![QTurn {
                question: "Why is the sky blue?".into(),
                answer: "**Rayleigh** scattering.".into(),
                asked_at: "2026-11-20T10:00:00Z".into(),
                answered_at: Some("2026-11-20T10:00:09Z".into()),
                error: None,
            }],
            ai: None,
            kind: None,
        };
        s.save(&t).unwrap();
        let mode = fs::metadata(env.paths.data.join("questions/threads/qabc.json")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let (all, problem) = s.load_all();
        assert_eq!(all, vec![t.clone()]);
        assert!(problem.is_none());
        let broken = env.paths.data.join("questions/threads/qbad.json");
        fs::write(&broken, br#"{"id":"qbad","title":"half a fi"#).unwrap();
        let (all, problem) = s.load_all();
        assert_eq!(all.len(), 1);
        let problem = problem.expect("says a thread could not be read");
        assert!(problem.contains("qbad.json.broken-"), "{problem}");
        assert!(!broken.exists());
        let kept: Vec<_> = fs::read_dir(env.paths.data.join("questions/threads")).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with("qbad.json.broken-")).collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(fs::read(kept[0].path()).unwrap(), br#"{"id":"qbad","title":"half a fi"#, "kept as it was");
        s.delete("qabc").unwrap();
        assert!(s.load_all().0.is_empty());
        assert!(s.save(&QThread { id: "../escape".into(), ..t }).is_err());
    }

    #[test]
    fn the_hider_knows_question_sessions_by_folder_and_id() {
        let env = Env::new();
        let q = Questions::new(&env.paths, PathBuf::from("/nonexistent/claude"));
        q.sh.store.remember_session("project-question-sid");
        let h = q.hider();
        let dir = q.dir().to_string_lossy().into_owned();
        assert!(h.hides("any", Some(&dir)));
        assert!(h.hides("any", Some(&format!("{dir}/"))));
        assert!(h.hides("any", Some(&format!("{dir}/sub"))));
        assert!(!h.hides("any", Some(&format!("{dir}-other"))));
        assert!(h.hides("project-question-sid", Some("/home/x/Projects/alpha")));
        assert!(!h.hides("ordinary", Some("/home/x/Projects/alpha")));
        assert!(!h.hides("ordinary", None));
        let q = Questions::new(&Paths { data: PathBuf::from("/var/home/me/.local/share/wake"), ..env.paths.clone() }, PathBuf::new());
        assert!(q.hider().hides("x", Some("/home/me/.local/share/wake/questions")), "either spelling of home");
    }

    #[test]
    fn labels_question_processes_in_agents() {
        let mut h = Hider::default();
        h.titles.insert("sid".into(), "Why is the sky blue?".into());
        h.running = Some((42, "What is 2+2?".into()));
        let p = |pid: u32, sid: Option<&str>, stale: bool| crate::types::AgentProc {
            pid,
            ppid: 1,
            kind: ProcKind::Stray,
            agent: None,
            command: "claude -p".into(),
            session_id: sid.map(String::from),
            job_id: None,
            title: None,
            project_name: None,
            state: crate::types::State::Working,
            idle_seconds: None,
            rss_bytes: 100,
            cpu_percent: None,
            started_at: None,
            location: None,
            note: None,
            stale_reason: stale.then(|| "a stray".into()),
            children: vec![],
            start_ticks: None,
            helpers: None,
        };
        let mut ov = ProcOverview { procs: vec![p(42, None, true), p(7, Some("sid"), false), p(8, None, true)], stale_count: 2, stale_rss_bytes: 200, ..Default::default() };
        h.label_procs(&mut ov);
        assert_eq!(ov.procs[0].title.as_deref(), Some("Wake question: What is 2+2?"));
        assert_eq!(ov.procs[0].kind, ProcKind::Session);
        assert!(ov.procs[0].stale_reason.is_none(), "answering now is never stale");
        assert_eq!(ov.procs[1].title.as_deref(), Some("Wake question: Why is the sky blue?"));
        assert!(ov.procs[2].title.is_none());
        assert_eq!((ov.stale_count, ov.stale_rss_bytes), (1, 100));
    }

    // ---- with a fake claude ----

    fn fake_claude(env: &Env, body: &str) -> PathBuf {
        let p = env.root.join("fake-claude");
        fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn just(q: &Questions) -> Context {
        Context { project: None, cwd: q.dir() }
    }

    fn wait_idle(q: &Questions) -> QOverview {
        wait_idle_for(q, 20)
    }

    fn wait_idle_for(q: &Questions, secs: u64) -> QOverview {
        let end = Instant::now() + Duration::from_secs(secs);
        loop {
            let s = q.snapshot(0).unwrap();
            if s.answering.is_none() && s.waiting.is_empty() {
                return s;
            }
            assert!(Instant::now() < end, "still answering: {s:?}");
            std::thread::sleep(Duration::from_millis(30));
        }
    }

    #[test]
    fn a_fake_claude_answers_and_follow_ups_resume_its_session() {
        let env = Env::new();
        let log = env.root.join("args.log");
        let script = format!(
            r#"echo "$@" >> '{log}'; echo "cwd=$(pwd) q=$(cat)" >> '{log}'
echo '{START}'
echo '{{"type":"stream_event","event":{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"It is **4**."}}}},"parent_tool_use_id":null}}'
echo '{{"type":"result","subtype":"success","is_error":false,"result":"It is **4**.","session_id":"from-result-1"}}'"#,
            log = log.display()
        );
        let q = Questions::new(&env.paths, fake_claude(&env, &script));
        let id = q.ask(None, "  What is 2+2?  ", just(&q)).unwrap();
        let s = wait_idle(&q);
        let t = &s.threads[0];
        assert_eq!(t.id, id);
        assert_eq!(t.title, "What is 2+2?");
        assert_eq!(t.turns[0].answer, "It is **4**.");
        assert!(t.turns[0].answered_at.is_some() && t.turns[0].error.is_none());
        assert_eq!(t.claude_session.as_deref(), Some("from-result-1"), "the result's session id is the one to resume");
        assert_eq!(s.unread, vec![id.clone()]);
        q.mark_read(&id);
        assert!(q.snapshot(0).unwrap().unread.is_empty());
        q.ask(Some(&id), "And 3+3?", just(&q)).unwrap();
        wait_idle(&q);
        let args = fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = args.lines().collect();
        assert!(lines[0].contains("--session-id "), "{args}");
        assert!(lines[1].contains("q=What is 2+2?"), "{args}");
        assert!(lines[1].contains(&format!("cwd={}", store::realpath(&q.dir()).display())), "{args}");
        assert!(lines[2].contains("--resume from-result-1"), "{args}");
        // on disk, and every session it used is remembered for the hider
        let (disk, _) = q.sh.store.load_all();
        assert_eq!(disk[0].turns.len(), 2);
        let first_sid = lines[0].split("--session-id ").nth(1).unwrap().split(' ').next().unwrap().to_string();
        let h = q.hider();
        assert!(h.hides(&first_sid, None) && h.hides("from-result-1", None));
        let reopened = Questions::new(&env.paths, PathBuf::from("/nonexistent"));
        assert!(reopened.hider().hides(&first_sid, None), "remembered across restarts");
    }

    #[test]
    fn one_question_at_a_time_and_the_rest_wait() {
        let env = Env::new();
        let log = env.root.join("order.log");
        let script = format!(
            r#"cat > /dev/null; echo start >> '{log}'; sleep 0.3; echo end >> '{log}'
echo '{{"type":"result","subtype":"success","is_error":false,"result":"ok","session_id":"s"}}'"#,
            log = log.display()
        );
        let q = Questions::new(&env.paths, fake_claude(&env, &script));
        let a = q.ask(None, "one", just(&q)).unwrap();
        let b = q.ask(None, "two", just(&q)).unwrap();
        let c = q.ask(None, "three", just(&q)).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let s = q.snapshot(0).unwrap();
        assert_eq!(s.answering.as_deref(), Some(a.as_str()));
        assert_eq!(s.waiting, vec![b.clone(), c.clone()]);
        let s = wait_idle(&q);
        assert_eq!(fs::read_to_string(&log).unwrap(), "start\nend\n".repeat(3), "never two at once");
        assert!(s.threads.iter().all(|t| t.turns[0].answer == "ok"));
        // newest asked first
        assert_eq!(s.threads.iter().map(|t| t.id.clone()).collect::<Vec<_>>(), vec![c, b, a]);
    }

    #[test]
    fn stopping_kills_only_our_child_and_failures_can_be_retried() {
        let env = Env::new();
        let flag = env.root.join("fail-once");
        fs::write(&flag, "").unwrap();
        let script = format!(
            r#"cat > /dev/null
if [ -e '{flag}' ]; then rm '{flag}'; echo 'Error: Invalid API key · Please run /login' >&2; exit 1; fi
echo '{{"type":"result","subtype":"success","is_error":false,"result":"fine","session_id":"s2"}}'"#,
            flag = flag.display()
        );
        let q = Questions::new(&env.paths, fake_claude(&env, &script));
        let id = q.ask(None, "will fail", just(&q)).unwrap();
        let s = wait_idle(&q);
        assert_eq!(s.threads[0].turns[0].error.as_deref(), Some("Claude isn't logged in. Run claude once in a terminal to log in, then retry."));
        assert!(q.ask(Some("qnope"), "x", just(&q)).is_err());
        q.retry(&id, just(&q)).unwrap();
        let s = wait_idle(&q);
        assert_eq!(s.threads[0].turns.len(), 1, "a retry replaces the failed turn");
        assert_eq!(s.threads[0].turns[0].answer, "fine");
        assert!(q.retry(&id, just(&q)).is_err(), "nothing to retry once answered");

        // a slow one, stopped
        let slow = fake_claude(&env, "cat > /dev/null; exec sleep 30");
        let q = Questions::new(&env.paths, slow);
        let id = q.ask(None, "slow", just(&q)).unwrap();
        let queued = q.ask(None, "queued", just(&q)).unwrap();
        let end = Instant::now() + Duration::from_secs(5);
        while q.hider().running.is_none() {
            assert!(Instant::now() < end);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(q.cancel(&queued));
        let started = Instant::now();
        assert!(q.cancel(&id));
        let s = wait_idle(&q);
        assert!(started.elapsed() < Duration::from_secs(3), "stopped promptly");
        let by = |i: &str| s.threads.iter().find(|t| t.id == i).unwrap().turns[0].error.clone();
        assert_eq!(by(&id).as_deref(), Some("Stopped."));
        assert_eq!(by(&queued).as_deref(), Some("Stopped before Claude answered."));
        assert!(!q.cancel(&id), "nothing left to stop");
        q.delete(&id).unwrap();
        assert!(q.snapshot(0).unwrap().threads.iter().all(|t| t.id != id));
    }

    #[test]
    fn an_answer_that_goes_quiet_is_stopped() {
        let env = Env::new();
        let q = Questions::new(&env.paths, fake_claude(&env, &format!("cat > /dev/null; echo '{START}'; exec sleep 30")));
        *q.sh.idle_limit.lock().unwrap() = Duration::from_millis(500);
        let started = Instant::now();
        q.ask(None, "hello?", just(&q)).unwrap();
        let s = wait_idle(&q);
        assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
        assert_eq!(s.threads[0].turns[0].error.as_deref(), Some("Claude went quiet for too long, so Wake stopped it."));
    }

    #[test]
    fn a_missing_claude_is_a_plain_note_and_an_interrupted_answer_says_so() {
        let env = Env::new();
        let q = Questions::new(&env.paths, env.root.join("no-such-claude"));
        q.ask(None, "hello?", just(&q)).unwrap();
        let s = wait_idle(&q);
        assert!(s.threads[0].turns[0].error.as_deref().unwrap().starts_with("Wake couldn't find claude"));
        // a turn left unanswered on disk (Wake closed mid-answer)
        let mut t = s.threads[0].clone();
        t.id = "qopen".into();
        t.turns[0].error = None;
        t.turns[0].answer = "Half an ans".into();
        q.sh.store.save(&t).unwrap();
        let again = Questions::new(&env.paths, env.root.join("no-such-claude"));
        let s = again.snapshot(0).unwrap();
        let t = s.threads.iter().find(|t| t.id == "qopen").unwrap();
        assert_eq!(t.turns[0].answer, "Half an ans");
        assert!(t.turns[0].error.as_deref().unwrap().contains("Wake closed"));
        assert!(again.snapshot(s.seq).is_none(), "nothing changed since");
    }

    #[test]
    fn question_sessions_stay_out_of_monitor_projects_and_history() {
        use crate::Source;
        let env = Env::new();
        let actor = Arc::new(crate::procs::tests::FakeActor { calls: Mutex::new(vec![]) });
        let bus = crate::desktop::tests::FakeBus::new();
        let live = crate::source::Live::new(crate::source::LiveConfig { paths: env.paths.clone(), proc_root: PathBuf::from("/proc"), runner: bus.runner(), actor, terminal: None });
        let original = fs::read_to_string(env.paths.claude.join("projects/-home/sess-aaaa.jsonl")).unwrap();
        let home = format!("\"cwd\":\"{}\"", env.paths.home.display());
        assert!(original.contains(&home));
        let qdir = env.paths.data.join("questions");
        // one asked as "just a question" (its folder gives it away), one about a project (known by id)
        let just = original.replace("sess-aaaa", "qsess-just").replace(&home, &format!("\"cwd\":\"{}\"", qdir.display()));
        let proj = original.replace("sess-aaaa", "qsess-proj");
        let dir = env.paths.claude.join("projects/-questions");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("qsess-just.jsonl"), just).unwrap();
        fs::write(dir.join("qsess-proj.jsonl"), proj).unwrap();
        QStore::new(&env.paths).remember_session("qsess-proj");
        live.index_now();
        let ov = live.overview();
        let ids: Vec<&str> = ov.sessions.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&"sess-aaaa"), "{ids:?}");
        assert!(!ids.contains(&"qsess-just") && !ids.contains(&"qsess-proj"), "{ids:?}");
        let hits: Vec<String> = live.search("tidy the parser").into_iter().map(|(s, _)| s.id).collect();
        assert!(hits.contains(&"sess-aaaa".to_string()), "{hits:?}");
        assert!(!hits.iter().any(|h| h.starts_with("qsess")), "{hits:?}");
        for p in &ov.projects {
            if let Some(d) = live.project(&p.id) {
                assert!(d.sessions.iter().all(|s| !s.id.starts_with("qsess")));
            }
        }
    }

    // ---- Codex and Gemini ----

    // Lines as codex-cli 0.171.0 (`exec --json`) and gemini 0.64.0 (`--output-format stream-json`)
    // write them, trimmed.
    const CX_START: &str = r#"{"type":"thread.started","thread_id":"019a8a2e-5b0f-7d61-a4a7-1c2d3e4f5a6b"}"#;
    const CX_TURN: &str = r#"{"type":"turn.started"}"#;
    const CX_THINK: &str = r#"{"type":"item.started","item":{"id":"item_0","type":"reasoning","text":""}}"#;
    const CX_CMD: &str =
        r#"{"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"bash -lc 'rg -n parser'","aggregated_output":"","exit_code":null,"status":"in_progress"}}"#;
    const CX_MSG: &str = r#"{"type":"item.completed","item":{"id":"item_2","type":"agent_message","text":"Four."}}"#;
    const CX_DONE: &str = r#"{"type":"turn.completed","usage":{"input_tokens":10,"cached_input_tokens":0,"output_tokens":3}}"#;
    const CX_FAIL: &str = r#"{"type":"turn.failed","error":{"message":"stream error: 401 Unauthorized"}}"#;
    const GM_INIT: &str = r#"{"type":"init","timestamp":"2026-11-20T12:00:00.000Z","session_id":"6f1c0d3e-2b4a-4c5d-8e9f-0a1b2c3d4e5f","model":"gemini-3-pro"}"#;
    const GM_USER: &str = r#"{"type":"message","timestamp":"2026-11-20T12:00:00.100Z","role":"user","content":"What is 2+2?"}"#;
    const GM_A1: &str = r#"{"type":"message","timestamp":"2026-11-20T12:00:01.000Z","role":"assistant","content":"Fo","delta":true}"#;
    const GM_A2: &str = r#"{"type":"message","timestamp":"2026-11-20T12:00:01.100Z","role":"assistant","content":"ur.","delta":true}"#;
    const GM_TOOL: &str = r#"{"type":"tool_use","timestamp":"2026-11-20T12:00:01.200Z","tool_name":"google_web_search","tool_id":"t1","parameters":{"query":"x"}}"#;
    const GM_OK: &str = r#"{"type":"result","timestamp":"2026-11-20T12:00:02.000Z","status":"success","stats":{"total_tokens":20}}"#;
    const GM_ERR: &str = r#"{"type":"result","timestamp":"2026-11-20T12:00:02.000Z","status":"error","error":{"type":"FatalAuthenticationError","message":"Please set an Auth method in your settings.json"},"stats":{}}"#;

    #[test]
    fn codex_and_gemini_streams_make_the_same_answer() {
        assert_eq!(parse_codex(CX_START), Event::Init { session_id: "019a8a2e-5b0f-7d61-a4a7-1c2d3e4f5a6b".into() });
        assert_eq!(parse_codex(CX_TURN), Event::Other);
        assert_eq!(parse_codex(CX_CMD), Event::ToolStart("command_execution".into()));
        let mut a = Answer { ai: Agent::Codex, ..Answer::default() };
        for l in [CX_START, CX_TURN, CX_THINK] {
            a.feed(parse_codex(l));
        }
        assert_eq!(a.activity, Some("thinking"));
        a.feed(parse_codex(CX_MSG));
        a.feed(parse_codex(CX_CMD));
        assert_eq!(a.activity, Some("looking through files"));
        a.feed(parse_codex(r#"{"type":"item.completed","item":{"id":"item_3","type":"agent_message","text":"Really four."}}"#));
        a.feed(parse_codex(CX_DONE));
        assert_eq!(a.text, "Four.\n\nReally four.", "each message is a paragraph");
        assert_eq!(a.session.as_deref(), Some("019a8a2e-5b0f-7d61-a4a7-1c2d3e4f5a6b"));
        assert_eq!(a.done, Some(Ok(())));
        let mut a = Answer { ai: Agent::Codex, ..Answer::default() };
        a.feed(parse_codex(CX_FAIL));
        assert_eq!(a.done, Some(Err("Codex isn't logged in. Run codex once in a terminal to log in, then retry.".into())));

        assert_eq!(parse_gemini(GM_USER), Event::Other, "the question echoed back is not the answer");
        let mut g = Answer { ai: Agent::Gemini, ..Answer::default() };
        for l in [GM_INIT, GM_USER, GM_A1, GM_A2] {
            g.feed(parse_gemini(l));
        }
        assert_eq!(g.text, "Four.");
        g.feed(parse_gemini(GM_TOOL));
        assert_eq!(g.activity, Some("searching the web"));
        g.feed(parse_gemini(r#"{"type":"message","role":"assistant","content":"Checked: 4.","delta":true}"#));
        g.feed(parse_gemini(GM_OK));
        assert_eq!(g.text, "Four.\n\nChecked: 4.", "after a tool, a new paragraph");
        assert_eq!(g.session.as_deref(), Some("6f1c0d3e-2b4a-4c5d-8e9f-0a1b2c3d4e5f"));
        assert_eq!(g.done, Some(Ok(())));
        let mut g = Answer { ai: Agent::Gemini, ..Answer::default() };
        g.feed(parse_gemini(GM_ERR));
        assert_eq!(g.done, Some(Err("Gemini isn't logged in. Run gemini once in a terminal to log in, then retry.".into())));
        assert_eq!(parse_line_for(Agent::Claude, DELTA), Event::TextDelta("Four.".into()));
    }

    #[test]
    fn codex_and_gemini_arguments_keep_them_read_only() {
        let a = codex_args(None);
        assert_eq!(a[..4], ["exec", "--json", "--sandbox", "read-only"].map(String::from));
        assert!(a.windows(2).any(|w| w[0] == "-c" && w[1] == "approval_policy=\"never\""));
        assert!(a.windows(2).any(|w| w[0] == "-c" && w[1] == "mcp_servers={}"));
        assert!(a.contains(&"--skip-git-repo-check".to_string()));
        assert_eq!(a.last().map(String::as_str), Some("-"), "the question goes on stdin");
        assert!(!a.iter().any(|x| x.contains("danger") || x == "workspace-write" || x == "--full-auto"));
        let r = codex_args(Some("019a"));
        let at = r.iter().position(|x| x == "resume").unwrap();
        assert_eq!(r[at..], ["resume", "019a", "-"].map(String::from));
        assert!(r[..at].contains(&"read-only".to_string()), "the sandbox is set before resume too");

        let g = gemini_args("sid", false, Path::new("/d/q/gemini-readonly.toml"));
        let after = |v: &[String], f: &str| v.iter().position(|x| x == f).map(|i| v[i + 1].clone()).unwrap();
        assert_eq!(after(&g, "--approval-mode"), "plan");
        assert_eq!(after(&g, "--admin-policy"), "/d/q/gemini-readonly.toml");
        assert_eq!(after(&g, "--output-format"), "stream-json");
        assert_eq!(after(&g, "--session-id"), "sid");
        assert!(!g.iter().any(|x| x == "--yolo" || x == "-y" || x == "auto_edit"));
        assert_eq!((after(&g, "--allowed-mcp-server-names"), after(&g, "--extensions")), ("none".into(), "none".into()));
        assert_eq!(after(&gemini_args("sid", true, Path::new("/p")), "--resume"), "sid");
        // the policy denies everything, then allows reading and searching only
        let t: toml::Table = GEMINI_POLICY.parse().unwrap();
        let rules = t["rule"].as_array().unwrap();
        assert_eq!((rules[0]["toolName"].as_str(), rules[0]["decision"].as_str()), (Some("*"), Some("deny")));
        let allowed: Vec<&str> = rules[1]["toolName"].as_array().unwrap().iter().filter_map(|v| v.as_str()).collect();
        for bad in ["write_file", "replace", "run_shell_command", "save_memory"] {
            assert!(!allowed.contains(&bad), "{bad}");
        }
        assert!(rules[1]["priority"].as_integer() > rules[0]["priority"].as_integer());
    }

    #[test]
    fn only_installed_ais_wake_can_keep_read_only_are_offered() {
        assert!(choice(Agent::Claude, None, false).usable);
        // Claude's program missing from where Wake would run it: said like the others.
        let env = Env::new();
        let q = Questions::new(&env.paths, env.root.join("no-claude-here"));
        let c = q.choices().into_iter().find(|c| c.ai == Agent::Claude).unwrap();
        assert!(!c.usable && c.hint == "Claude isn't installed here.", "{c:?}");
        q.set_tool(Agent::Claude, Some(env.root.join("claude-bin")));
        fs::write(env.root.join("claude-bin"), "").unwrap();
        assert!(q.choices()[0].usable);
        let c = choice(Agent::Codex, None, false);
        assert!(!c.usable && c.hint == "Codex isn't installed here.");
        assert!(choice(Agent::Codex, Some(Path::new("/x/codex")), false).usable);
        assert!(choice(Agent::Gemini, Some(Path::new("/x/gemini")), false).usable);
        let g = choice(Agent::Gemini, Some(Path::new("/x/gemini")), true);
        assert!(!g.usable && g.hint.contains("system policy"), "{}", g.hint);
        let env = Env::new();
        let dir = env.root.join("policies");
        assert!(!gemini_has_system_policies(&dir));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("README"), "").unwrap();
        assert!(!gemini_has_system_policies(&dir));
        fs::write(dir.join("site.toml"), "").unwrap();
        assert!(gemini_has_system_policies(&dir));
        assert_eq!(Agent::from_id(" Codex "), Some(Agent::Codex));
        assert_eq!(Agent::from_id("bard"), None);
    }

    #[test]
    fn a_fake_codex_answers_and_its_follow_ups_resume_its_thread() {
        let env = Env::new();
        let log = env.root.join("codex.log");
        let script = format!(
            r#"echo "$@" >> '{log}'; echo "q=$(cat)" >> '{log}'
echo '{CX_START}'
echo '{CX_THINK}'
echo '{CX_MSG}'
echo '{CX_DONE}'"#,
            log = log.display()
        );
        let p = env.root.join("fake-codex");
        fs::write(&p, format!("#!/bin/sh\n{script}\n")).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        let q = Questions::new(&env.paths, PathBuf::from("/nonexistent/claude"));
        q.set_tool(Agent::Codex, Some(p));
        q.set_ai(Agent::Codex);
        let id = q.ask(None, "What is 2+2?", just(&q)).unwrap();
        let s = wait_idle(&q);
        let t = s.threads.iter().find(|t| t.id == id).unwrap();
        assert_eq!(t.ai.as_deref(), Some("codex"));
        assert_eq!(t.turns[0].answer, "Four.");
        assert!(t.turns[0].error.is_none(), "{:?}", t.turns[0].error);
        assert_eq!(t.claude_session.as_deref(), Some("019a8a2e-5b0f-7d61-a4a7-1c2d3e4f5a6b"));
        assert!(q.hider().hides("019a8a2e-5b0f-7d61-a4a7-1c2d3e4f5a6b", Some("/home/x/Projects/alpha")), "its session stays out of Wake's lists");
        // Settings change: the thread still follows up with Codex.
        q.set_ai(Agent::Claude);
        q.ask(Some(&id), "And 3+3?", just(&q)).unwrap();
        wait_idle(&q);
        let text = fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("exec --json --sandbox read-only") && !lines[0].contains("resume"), "{text}");
        assert!(lines[1].starts_with("q=The person is asking a quick question from Wake") && text.contains("\nWhat is 2+2?\n"), "Wake's note, then the question: {text}");
        assert!(lines.iter().any(|l| l.contains("resume 019a8a2e-5b0f-7d61-a4a7-1c2d3e4f5a6b -")), "{text}");
        assert!(text.contains("q=And 3+3?"), "a resumed follow-up is only the question: {text}");
    }

    #[test]
    fn a_fake_gemini_answers_under_wake_s_policy_and_resumes_its_session() {
        let env = Env::new();
        let log = env.root.join("gemini.log");
        let script = format!(
            r#"echo "$@" >> '{log}'; echo "q=$(cat)" >> '{log}'
echo '{GM_INIT}'
echo '{GM_USER}'
echo '{GM_A1}'
echo '{GM_A2}'
echo '{GM_OK}'"#,
            log = log.display()
        );
        let p = env.root.join("fake-gemini");
        fs::write(&p, format!("#!/bin/sh\n{script}\n")).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        let q = Questions::new(&env.paths, PathBuf::from("/nonexistent/claude"));
        q.set_tool(Agent::Gemini, Some(p));
        q.set_ai(Agent::Gemini);
        if !q.choices().iter().any(|c| c.ai == Agent::Gemini && c.usable) {
            return; // this machine has gemini system policies; Wake rightly won't use it
        }
        let id = q.ask(None, "What is 2+2?", just(&q)).unwrap();
        let s = wait_idle(&q);
        let t = s.threads.iter().find(|t| t.id == id).unwrap();
        assert_eq!((t.ai.as_deref(), t.turns[0].answer.as_str()), (Some("gemini"), "Four."));
        let policy = q.dir().join("gemini-readonly.toml");
        assert_eq!(fs::read_to_string(&policy).unwrap(), GEMINI_POLICY);
        q.ask(Some(&id), "And 3+3?", just(&q)).unwrap();
        wait_idle(&q);
        let text = fs::read_to_string(&log).unwrap();
        let first: Vec<&str> = text.lines().collect();
        let sid = first[0].split("--session-id ").nth(1).unwrap().split(' ').next().unwrap();
        assert!(first[0].contains("--approval-mode plan") && first[0].contains(&format!("--admin-policy {}", policy.display())), "{text}");
        assert_eq!(first[1], "q=What is 2+2?", "the note comes with -p, not twice");
        // the id Gemini reports is the one to resume
        assert!(text.contains("--resume 6f1c0d3e-2b4a-4c5d-8e9f-0a1b2c3d4e5f"), "{text}");
        assert!(q.hider().hides(sid, Some("/home/x/Projects/alpha")) && q.hider().hides("6f1c0d3e-2b4a-4c5d-8e9f-0a1b2c3d4e5f", None));
    }

    #[test]
    fn interview_arguments_allow_no_tools_at_all() {
        let a = interview_args("sid", false);
        assert!(a.join(" ").starts_with("-p --output-format stream-json --verbose --include-partial-messages --session-id sid"));
        let after = |f: &str| a.iter().position(|x| x == f).map(|i| a[i + 1].clone()).unwrap();
        assert_eq!(after("--tools"), "", "the CLI's way to allow none");
        assert!(!a.iter().any(|x| x == "--allowedTools"));
        assert_eq!(after("--disallowedTools"), DENIED_TOOLS);
        assert_eq!(after("--permission-prompts"), "none");
        assert_eq!(after("--setting-sources"), "user");
        assert_eq!(serde_json::from_str::<Value>(&after("--settings")).unwrap()["disableAllHooks"], true);
        assert!(a.contains(&"--strict-mcp-config".to_string()));
        assert_eq!(after("--system-prompt"), crate::newproject::INTERVIEWER);
        assert_eq!(interview_args("sid", true)[5..7], ["--resume".to_string(), "sid".to_string()]);
        assert!(GEMINI_INTERVIEW_POLICY.contains("toolName = \"*\"\ndecision = \"deny\"") && !GEMINI_INTERVIEW_POLICY.contains("allow"));
    }

    #[test]
    fn an_interview_runs_with_no_tools_and_stays_out_of_ask() {
        let env = Env::new();
        let log = env.root.join("interview.log");
        let script = format!(
            r#"for a in "$@"; do printf '[%s]' "$a"; done >> '{log}'; echo >> '{log}'; echo "cwd=$(pwd) q=$(cat)" >> '{log}'
echo '{START}'
echo '{{"type":"stream_event","event":{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"What would you like to make?"}}}},"parent_tool_use_id":null}}'
echo '{{"type":"result","subtype":"success","is_error":false,"result":"What would you like to make?","session_id":"iv-1"}}'"#,
            log = log.display()
        );
        let q = Questions::new(&env.paths, fake_claude(&env, &script));
        // Codex answers questions, but can't be run with no tools: the interview goes to Claude.
        q.set_tool(Agent::Codex, Some(PathBuf::from("/bin/true")));
        q.set_ai(Agent::Codex);
        assert_eq!(q.interview_ai(), Agent::Claude);
        let ask_id = q.ask(None, "A real question", just(&q)).unwrap();
        let _ = q.cancel(&ask_id);
        let id = q.interview(None, crate::newproject::OPENER).unwrap();
        let done = |id: &str| {
            let end = Instant::now() + Duration::from_secs(20);
            loop {
                let s = q.interview_state(id, 0).unwrap();
                if s.answering.is_none() && s.waiting.is_empty() {
                    return s.threads[0].clone();
                }
                assert!(Instant::now() < end);
                std::thread::sleep(Duration::from_millis(30));
            }
        };
        let t = done(&id);
        assert_eq!(t.turns[0].answer, "What would you like to make?");
        assert_eq!((t.ai.as_deref(), t.kind.as_deref(), t.title.as_str()), (Some("claude"), Some("newproject"), crate::newproject::TITLE));
        let ask = q.snapshot(0).unwrap();
        assert!(ask.threads.iter().all(|x| x.id != id) && !ask.unread.contains(&id), "never in Ask's list");
        assert!(ask.threads.iter().any(|x| x.id == ask_id));
        assert!(q.interview_state(&ask_id, 0).is_none(), "a question is not an interview");
        assert!(q.interview(Some(&ask_id), "hi").is_err());
        let text = fs::read_to_string(&log).unwrap();
        assert!(text.contains("[--tools][][--disallowedTools]"), "{text}");
        assert!(text.contains("[--setting-sources][user]") && text.contains("[--system-prompt][You are helping someone start a new project"), "{text}");
        assert!(text.contains(&format!("cwd={} q={}", store::realpath(&q.interview_dir()).display(), crate::newproject::OPENER)), "{text}");
        // Its sessions stay out of Wake's lists, by id and by folder.
        let h = q.hider();
        assert!(h.hides("iv-1", None) && h.hides("other", Some(&q.interview_dir().to_string_lossy())));
        // A follow-up resumes it; ending it deletes it.
        q.interview(Some(&id), "A website").unwrap();
        done(&id);
        assert!(fs::read_to_string(&log).unwrap().contains("[--resume][iv-1]"));
        q.delete(&id).unwrap();
        assert!(q.interview_state(&id, 0).is_none());
        // One left on disk from before is dropped when Wake starts again.
        let left = q.interview(None, crate::newproject::OPENER).unwrap();
        done(&left);
        let again = Questions::new(&env.paths, PathBuf::from("/nonexistent"));
        assert!(again.interview_state(&left, 0).is_none());
        assert!(!q.dir().join("threads").join(format!("{left}.json")).exists());
    }

    #[test]
    fn a_lost_session_sends_the_thread_so_far() {
        let turns = vec![QTurn { question: "Q1".into(), answer: "A1".into(), asked_at: "t".into(), answered_at: Some("t".into()), error: None }];
        let p = thread_so_far(&turns, "Q2");
        assert!(p.contains("Question: Q1\nAnswer: A1") && p.ends_with("The new question: Q2"), "{p}");
        assert_eq!(first_prompt(Agent::Claude, "x"), "x", "Claude gets its note as a system prompt");
        assert_eq!(first_prompt(Agent::Gemini, "x"), "x", "Gemini gets it with -p");
        assert!(first_prompt(Agent::Codex, "x").ends_with("\n\nx"));
    }

    /// The one end-to-end check against the real claude (costs a real request, so ignored):
    /// `cargo test -p wake-core real_claude -- --ignored --nocapture`. Writes a real thread to
    /// ~/.local/share/wake/questions, then checks Wake's own index leaves the session out.
    #[test]
    #[ignore = "runs the real claude once"]
    fn real_claude_answers_and_stays_out_of_wake_s_lists() {
        let paths = Paths::from_env();
        let q = Questions::new(&paths, find_claude(&paths));
        let id = q.ask(None, "What is 2+2? Answer in one word.", just(&q)).unwrap();
        let mut seen_partial = false;
        let end = Instant::now() + Duration::from_secs(180);
        let s = loop {
            let s = q.snapshot(0).unwrap();
            let t = s.threads.iter().find(|t| t.id == id).unwrap();
            seen_partial |= s.answering.is_some() && !t.turns[0].answer.is_empty();
            if s.answering.is_none() && s.waiting.is_empty() {
                break s;
            }
            assert!(Instant::now() < end, "no answer in 3 minutes");
            std::thread::sleep(Duration::from_millis(20));
        };
        let t = s.threads.iter().find(|t| t.id == id).unwrap().clone();
        println!("answer: {:?}; error: {:?}; session: {:?}; streamed while answering: {seen_partial}", t.turns[0].answer, t.turns[0].error, t.claude_session);
        assert!(t.turns[0].error.is_none(), "{:?}", t.turns[0].error);
        assert!(t.turns[0].answer.to_lowercase().contains("four") || t.turns[0].answer.contains('4'));
        check_real_lists(&t.claude_session.clone().unwrap());
    }

    /// One real New project interview with the real claude, answered from a script (real requests,
    /// so ignored): `cargo test -p wake-core real_claude_interview -- --ignored --nocapture`.
    /// Prints each message, checks the plan parses, then deletes the thread; makes no project.
    #[test]
    #[ignore = "runs the real claude several times"]
    fn real_claude_interview_plans_a_project() {
        let paths = Paths::from_env();
        let q = Questions::new(&paths, find_claude(&paths));
        let answers =
            ["A website with our family's recipes, for my parents and my kids.", "A website, so technical I think.", "You decide.", "Yes please.", "Family Recipes", "You decide.", "You decide."];
        let id = q.interview(None, crate::newproject::OPENER).unwrap();
        let mut plan = None;
        for turn in 0..=answers.len() {
            let end = Instant::now() + Duration::from_secs(180);
            let t = loop {
                let s = q.interview_state(&id, 0).unwrap();
                if s.answering.is_none() && s.waiting.is_empty() {
                    break s.threads[0].clone();
                }
                assert!(Instant::now() < end, "no answer in 3 minutes");
                std::thread::sleep(Duration::from_millis(50));
            };
            let last = t.turns.last().unwrap();
            assert!(last.error.is_none(), "{:?}", last.error);
            println!("--- AI ({}):\n{}\n", t.ai.as_deref().unwrap_or("claude"), last.answer);
            plan = crate::newproject::parse_plan(&last.answer);
            if plan.is_some() || turn == answers.len() {
                break;
            }
            println!("--- You: {}\n", answers[turn]);
            q.interview(Some(&id), answers[turn]).unwrap();
        }
        let sid = q.interview_state(&id, 0).and_then(|s| s.threads[0].claude_session.clone());
        q.delete(&id).unwrap();
        println!("plan: {plan:#?}");
        assert!(plan.is_some(), "no plan after {} answers", answers.len());
        if let Some(sid) = sid {
            check_real_lists(&sid);
        }
    }

    /// One real run each for Codex and Gemini (a real request each, so ignored):
    /// `cargo test -p wake-core real_other_ai -- --ignored --nocapture --test-threads 1`, or with
    /// `WAKE_REAL_AI=codex` / `gemini` for just one. Writes a real thread to
    /// ~/.local/share/wake/questions, then checks Wake's own lists leave its session out.
    #[test]
    #[ignore = "runs the real codex and gemini once each"]
    fn real_other_ai_answers_and_stays_out_of_wake_s_lists() {
        let only = std::env::var("WAKE_REAL_AI").ok();
        for ai in [Agent::Codex, Agent::Gemini] {
            if only.as_deref().is_some_and(|o| o != ai.id()) {
                continue;
            }
            let paths = Paths::from_env();
            let q = Questions::new(&paths, find_claude(&paths));
            q.set_tool(ai, find_tool(&paths, ai));
            q.set_ai(ai);
            assert!(q.choices().iter().any(|c| c.ai == ai && c.usable), "{} isn't usable here", ai.label());
            let id = q.ask(None, "What is 2+2? Answer in one word.", just(&q)).unwrap();
            let end = Instant::now() + Duration::from_secs(240);
            let s = loop {
                let s = q.snapshot(0).unwrap();
                if s.answering.is_none() && s.waiting.is_empty() {
                    break s;
                }
                assert!(Instant::now() < end, "no answer in 4 minutes");
                std::thread::sleep(Duration::from_millis(50));
            };
            let t = s.threads.iter().find(|t| t.id == id).unwrap().clone();
            println!("{}: answer {:?}; error {:?}; session {:?}", ai.label(), t.turns[0].answer, t.turns[0].error, t.claude_session);
            assert!(t.turns[0].error.is_none(), "{:?}", t.turns[0].error);
            assert!(t.turns[0].answer.to_lowercase().contains("four") || t.turns[0].answer.contains('4'));
            check_real_lists(&t.claude_session.clone().unwrap());
        }
    }

    /// Wake's real lists leave question sessions out (no claude run):
    /// `WAKE_Q_SID=<session> cargo test -p wake-core real_lists -- --ignored --nocapture`.
    #[test]
    #[ignore = "reads this machine's real transcripts"]
    fn real_lists_leave_question_sessions_out() {
        check_real_lists(&std::env::var("WAKE_Q_SID").unwrap_or_default());
    }

    fn check_real_lists(sid: &str) {
        use crate::Source;
        let live = crate::source::Live::new(crate::source::LiveConfig::real());
        live.index_now();
        let ov = live.overview();
        println!("overview: {} sessions", ov.sessions.len());
        let leaked: Vec<_> =
            ov.sessions.iter().filter(|x| x.id == sid || x.cwd.as_deref().is_some_and(|c| c.contains("/.local/share/wake/questions"))).map(|x| (&x.id, &x.cwd, &x.title, x.state)).collect();
        assert!(leaked.is_empty(), "{leaked:?}");
        let hits = live.search("What is 2+2");
        println!("history search for the question: {} hits", hits.len());
        assert!(!hits.iter().any(|(x, _)| x.id == sid));
    }
}
