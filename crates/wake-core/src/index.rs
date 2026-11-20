//! Transcript indexer.
//!
//! Streams Claude Code transcripts (~/.claude/projects/<dir>/<session>.jsonl) and Codex rollouts
//! (~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl) line by line into small per-session summaries in
//! <data>/index-v2/sessions/<id>.json, with per-file (size, mtime, offset) bookkeeping in
//! index-v2/files.json so later runs only read appended bytes. The summary files keep the Python
//! version's JSON shape, so its summaries of transcripts Claude has since deleted are carried over.
//!
//! Speed and memory: lines are classified with cheap byte checks first. Tool-result user lines (the
//! bulk of the bytes) are never JSON-decoded; metadata lines we do not need are skipped by prefix.
//! Subagent transcripts are never parsed during the overview index; they are counted from the
//! directory listing and their small .meta.json files, and summarised lazily per session on demand.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::store::{self, Paths, ProjectRec, Writer};
use crate::types::{Agent, FileTouch, Prompt, SessionSummary, State, Step, SubagentCounts, SubagentDetail, SubagentSummary};
use crate::util::{self, contains, find, rfind};

pub const PROMPT_LIST_MAX: usize = 200;
pub const PROMPT_TEXT_MAX: usize = 400;
pub const FILES_KEEP: usize = 400;
pub const FILES_OUT: usize = 100;
pub const LAST_TEXT_MAX: usize = 4000;
/// Seconds since a subagent file was written for it to count as running.
pub const RUNNING_WINDOW: f64 = 300.0;
/// Gaps longer than this do not count as active minutes.
pub const ACTIVE_GAP: f64 = 300.0;

const WRAPPERS: &[&str] = &[
    "<command-",
    "<local-command",
    "<system-reminder",
    "<task-notification",
    "Caveat:",
    "[Request interrupted",
    "<bash-input",
    "<bash-stdout",
    "<bash-stderr",
    "<user-memory-input",
    "Base directory for this skill",
];
const CLAUDE_META_TYPES: &[&[u8]] = &[b"ai-title", b"custom-title", b"last-prompt", b"cost-state", b"pr-link", b"worktree-state", b"summary", b"agent-name"];
const EDIT_TOOLS: &[&str] = &["Edit", "Write", "NotebookEdit", "MultiEdit"];
const CODEX_WRAPPERS: &[&str] = &["<environment_context", "<user_instructions", "<permissions", "# AGENTS.md", "<skills_instructions", "<turn_aborted", "<user_shell_command"];

// ------------------------------------------------------------------ ordered map

/// A small insertion-ordered string map, stored as a JSON object (Python dicts keep order, and
/// "most turns" ties go to the first seen).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OMap<V>(pub Vec<(String, V)>);

impl<V> OMap<V> {
    pub fn get(&self, k: &str) -> Option<&V> {
        self.0.iter().find(|(key, _)| key == k).map(|(_, v)| v)
    }
    pub fn get_mut(&mut self, k: &str) -> Option<&mut V> {
        self.0.iter_mut().find(|(key, _)| key == k).map(|(_, v)| v)
    }
    pub fn insert(&mut self, k: String, v: V) {
        match self.get_mut(&k) {
            Some(slot) => *slot = v,
            None => self.0.push((k, v)),
        }
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn iter(&self) -> impl Iterator<Item = (&String, &V)> {
        self.0.iter().map(|(k, v)| (k, v))
    }
}

impl<V: Serialize> Serialize for OMap<V> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_map(self.0.iter().map(|(k, v)| (k, v)))
    }
}

impl<'de, V: Deserialize<'de>> Deserialize<'de> for OMap<V> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V2<V>(std::marker::PhantomData<V>);
        impl<'de, V: Deserialize<'de>> Visitor<'de> for V2<V> {
            type Value = OMap<V>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<OMap<V>, A::Error> {
                let mut out = Vec::new();
                while let Some((k, v)) = m.next_entry::<String, V>()? {
                    out.push((k, v));
                }
                Ok(OMap(out))
            }
            fn visit_unit<E>(self) -> Result<OMap<V>, E> {
                Ok(OMap(Vec::new()))
            }
        }
        d.deserialize_any(V2(std::marker::PhantomData))
    }
}

// ------------------------------------------------------------------ the accumulator

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PromptRec {
    pub at: Option<String>,
    pub text: String,
}

/// Everything Wake keeps about one session. Same JSON shape as the Python `acc` dict.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Acc {
    pub id: String,
    pub agent: String,
    pub source: Option<String>,
    pub started_at: Option<String>,
    pub last_at: Option<String>,
    pub prompts: u32,
    pub prompts_list: Vec<PromptRec>,
    pub first_prompt: Option<String>,
    pub files: OMap<u32>,
    pub commands: u32,
    pub tests: u32,
    pub pr_links: Vec<String>,
    pub cost: Option<f64>,
    pub ai_title: Option<String>,
    pub custom_title: Option<String>,
    pub last_prompt_meta: Option<String>,
    pub agent_name: Option<String>,
    pub away_summary: Option<String>,
    pub away_at: Option<String>,
    pub cwd: Option<String>,
    pub launch_cwd: Option<String>,
    pub git_branch: Option<String>,
    pub proj_turns: OMap<u32>,
    pub worktrees: OMap<Option<String>>,
    pub cwd_seen: OMap<u32>,
    pub last_text: Option<String>,
    pub last_text_at: Option<String>,
    pub last_kind: Option<String>,
    pub last_user_at: Option<String>,
    pub active_sec: f64,
    pub prev_ts: Option<f64>,
    pub subagents_total: u32,
    pub deleted_by_claude: bool,
    /// How many bytes of `source` this summary has read. Saved in the same atomic write as the
    /// summary itself, so after a crash the next run resumes exactly where this file left off and
    /// never counts a line twice. (None in files written before this existed: files.json then says.)
    pub scan_offset: Option<u64>,
}

pub fn new_acc(sid: &str, path: Option<&str>, agent: &str) -> Acc {
    Acc { id: sid.into(), agent: agent.into(), source: path.map(Into::into), ..Default::default() }
}

/// In-memory copy without the bulky lists.
pub fn lite(acc: &Acc) -> Acc {
    let mut out = acc.clone();
    // Kept small: the files it changed, folded to their folder a few levels down, which is all
    // attribution needs ("which project did it change files in") when it worked from `~`.
    out.files = fold_files(&acc.files);
    if let Some(last) = acc.prompts_list.last() {
        out.prompts_list = vec![last.clone()];
    }
    out
}

fn touch_ts(acc: &mut Acc, ts: Option<&str>) {
    let Some(ts) = ts else { return };
    if ts.is_empty() {
        return;
    }
    if acc.started_at.as_deref().is_none_or(|s| ts < s) {
        acc.started_at = Some(ts.into());
    }
    if acc.last_at.as_deref().is_none_or(|s| ts > s) {
        acc.last_at = Some(ts.into());
    }
    let Some(e) = util::parse_iso(ts) else { return };
    if let Some(prev) = acc.prev_ts {
        let gap = e - prev;
        if gap > 0.0 && gap <= ACTIVE_GAP {
            acc.active_sec += gap;
        }
    }
    if acc.prev_ts.is_none_or(|p| e > p) {
        acc.prev_ts = Some(e);
    }
}

fn set_cwd(acc: &mut Acc, cwd: Option<&str>) {
    let Some(cwd) = cwd.filter(|c| !c.is_empty()) else { return };
    acc.cwd = Some(cwd.into());
    if acc.launch_cwd.is_none() {
        acc.launch_cwd = Some(cwd.into());
    }
    let key = store::norm_abs(cwd).unwrap_or_default();
    if let Some(n) = acc.cwd_seen.get_mut(&key) {
        *n += 1;
    } else if acc.cwd_seen.len() < 200 {
        acc.cwd_seen.0.push((key, 1));
    }
}

fn add_prompt(acc: &mut Acc, text: &str, ts: Option<&str>, cwd: Option<&str>) {
    acc.prompts += 1;
    let short = util::take_chars(text, PROMPT_TEXT_MAX);
    if acc.first_prompt.is_none() {
        acc.first_prompt = Some(short.clone());
    }
    acc.prompts_list.push(PromptRec { at: ts.map(Into::into), text: short });
    if acc.prompts_list.len() > PROMPT_LIST_MAX {
        acc.prompts_list.remove(0);
    }
    acc.last_user_at = ts.map(Into::into);
    acc.last_text = None;
    acc.last_kind = None;
    let key = cwd.and_then(store::norm_abs).unwrap_or_default();
    match acc.proj_turns.get_mut(&key) {
        Some(n) => *n += 1,
        None => acc.proj_turns.0.push((key, 1)),
    }
}

fn add_file(acc: &mut Acc, path: Option<&str>) {
    let Some(path) = path.filter(|p| !p.is_empty()) else { return };
    if let Some(n) = acc.files.get_mut(path) {
        *n += 1;
    } else if acc.files.len() < FILES_KEEP {
        acc.files.0.push((path.into(), 1));
    }
}

fn bash(acc: &mut Acc, cmd: Option<&str>) {
    acc.commands += 1;
    if cmd.is_some_and(util::is_test_command) {
        acc.tests += 1;
    }
}

// ------------------------------------------------------------------ small helpers

/// The user's home, written short: /home/sam/x -> ~/x.
pub fn short_path(p: &str, home: &Path) -> String {
    let h = home.to_string_lossy().into_owned();
    let rh = store::realpath(home).to_string_lossy().into_owned();
    for pre in [rh, h] {
        if p == pre {
            return "~".into();
        }
        if let Some(rest) = p.strip_prefix(&pre)
            && rest.starts_with('/')
        {
            return format!("~{rest}");
        }
    }
    p.to_string()
}

/// User message content -> the person's own prompt text, or None if it is not one.
pub fn clean_prompt(content: &Value) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    let mut images = 0;
    match content {
        Value::String(s) => parts.push(s),
        Value::Array(blocks) => {
            for b in blocks {
                let Some(o) = b.as_object() else { continue };
                match o.get("type").and_then(Value::as_str) {
                    Some("text") => parts.push(o.get("text").and_then(Value::as_str).unwrap_or("")),
                    Some("image") => images += 1,
                    Some("tool_result") => return None,
                    _ => {}
                }
            }
        }
        _ => return None,
    }
    let mut out: Vec<String> = Vec::new();
    for s in parts {
        let s = s.trim();
        if s.is_empty() || WRAPPERS.iter().any(|w| s.starts_with(w)) {
            continue;
        }
        let mut s = s.to_string();
        if s.contains("<pasted_content") {
            s = util::replace_tagged(&s, "<pasted_content", "</pasted_content>", true, "[pasted text]");
        }
        if s.contains("<system-reminder>") {
            s = util::replace_tagged(&s, "<system-reminder>", "</system-reminder>", false, "").trim().to_string();
        }
        if !s.is_empty() {
            out.push(s);
        }
    }
    let text = out.join(" ").trim().to_string();
    let text = if text.is_empty() && images > 0 {
        "[image]".to_string()
    } else if images > 0 {
        format!("[image] {text}")
    } else {
        text
    };
    if text.is_empty() { None } else { Some(text) }
}

pub fn ends_with_question(text: Option<&str>) -> bool {
    let Some(t) = text else { return false };
    let t = t.trim_end().trim_end_matches(['*', '_', '`', ')', '"', '\'', ' ', '\n']);
    t.ends_with('?')
}

fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

/// The value of the first `"timestamp":"…"` (10–40 chars) in a raw line.
fn ts_search(line: &[u8]) -> Option<String> {
    const KEY: &[u8] = b"\"timestamp\":\"";
    let mut from = 0;
    while let Some(i) = find(&line[from..], KEY) {
        let start = from + i + KEY.len();
        let len = line[start..].iter().take(41).position(|&c| c == b'"');
        if let Some(n) = len
            && (10..=40).contains(&n)
        {
            return Some(String::from_utf8_lossy(&line[start..start + n]).into_owned());
        }
        from = start;
    }
    None
}

/// Tool names in `"type":"tool_use","id":"…","name":"…"` blocks of a raw line.
fn tool_names(line: &[u8]) -> Vec<String> {
    const KEY: &[u8] = b"\"type\":\"tool_use\",\"id\":\"";
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = find(&line[from..], KEY) {
        let start = from + i + KEY.len();
        from = start;
        let Some(q) = line[start..].iter().position(|&c| c == b'"') else { break };
        let rest = &line[start + q..];
        if let Some(r) = rest.strip_prefix(b"\",\"name\":\"")
            && let Some(e) = r.iter().position(|&c| c == b'"')
            && e > 0
        {
            out.push(String::from_utf8_lossy(&r[..e]).into_owned());
        }
    }
    out
}

#[cfg(test)]
thread_local! {
    /// Every line handed to the JSON decoder in this thread (tests check what is never decoded).
    pub static DECODED: std::cell::RefCell<Vec<Vec<u8>>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn decode(line: &[u8]) -> Option<Value> {
    #[cfg(test)]
    DECODED.with(|d| d.borrow_mut().push(line[..line.len().min(60)].to_vec()));
    serde_json::from_slice(line).ok()
}

// ------------------------------------------------------------------ Claude records

fn claude_meta(acc: &mut Acc, d: &Value) {
    let cut = |v: Option<&str>, n| v.filter(|x| !x.is_empty()).map(|x| util::take_chars(x, n));
    match s(d, "type") {
        Some("ai-title") => {
            if let Some(t) = cut(s(d, "aiTitle"), 200) {
                acc.ai_title = Some(t);
            }
        }
        Some("custom-title") => {
            if let Some(t) = cut(s(d, "customTitle"), 200) {
                acc.custom_title = Some(t);
            }
        }
        Some("last-prompt") => {
            if let Some(lp) = s(d, "lastPrompt") {
                acc.last_prompt_meta = Some(util::take_chars(lp, PROMPT_TEXT_MAX));
            }
        }
        Some("cost-state") => {
            if let Some(c) = d.get("totalCostUSD").and_then(Value::as_f64) {
                acc.cost = Some((c * 10000.0).round() / 10000.0);
            }
        }
        Some("pr-link") => {
            if let Some(u) = s(d, "prUrl")
                && !acc.pr_links.iter().any(|x| x == u)
                && acc.pr_links.len() < 20
            {
                acc.pr_links.push(u.into());
            }
        }
        Some("worktree-state") => {
            let ws = d.get("worktreeSession");
            let wp = ws.and_then(|w| s(w, "worktreePath"));
            let oc = ws.and_then(|w| s(w, "originalCwd"));
            if let (Some(wp), Some(oc)) = (wp, oc) {
                let key = store::realpath(Path::new(wp)).to_string_lossy().into_owned();
                acc.worktrees.insert(key, store::norm_abs(oc));
            }
        }
        Some("summary") => {
            if acc.ai_title.is_none()
                && let Some(t) = s(d, "summary")
            {
                acc.ai_title = Some(util::take_chars(t, 200));
            }
        }
        Some("agent-name") => {
            if let Some(n) = s(d, "agentName") {
                acc.agent_name = Some(util::take_chars(n, 100));
            }
        }
        _ => {}
    }
}

fn claude_user(acc: &mut Acc, d: &Value) {
    if d.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return;
    }
    let ts = s(d, "timestamp");
    touch_ts(acc, ts);
    set_cwd(acc, s(d, "cwd"));
    if let Some(b) = s(d, "gitBranch").filter(|b| !b.is_empty()) {
        acc.git_branch = Some(b.into());
    }
    if d.get("isMeta").is_some_and(|m| m.as_bool() == Some(true)) || d.get("toolUseResult").is_some() {
        return;
    }
    let content = d.get("message").and_then(|m| m.get("content")).unwrap_or(&Value::Null);
    if let Some(text) = clean_prompt(content) {
        add_prompt(acc, &text, ts, s(d, "cwd"));
    }
}

fn claude_assistant(acc: &mut Acc, d: &Value) {
    if d.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return;
    }
    let ts = s(d, "timestamp");
    touch_ts(acc, ts);
    set_cwd(acc, s(d, "cwd"));
    let Some(content) = d.get("message").and_then(|m| m.get("content")).and_then(Value::as_array) else {
        return;
    };
    for b in content {
        match s(b, "type") {
            Some("text") => {
                let txt = s(b, "text").unwrap_or("").trim();
                if !txt.is_empty() {
                    acc.last_text = Some(util::last_chars(txt, LAST_TEXT_MAX));
                    acc.last_text_at = ts.map(Into::into);
                    acc.last_kind = Some("text".into());
                }
            }
            Some("tool_use") => {
                acc.last_kind = Some("tool".into());
                let name = s(b, "name").unwrap_or("");
                let inp = b.get("input").filter(|i| i.is_object()).unwrap_or(&Value::Null);
                if EDIT_TOOLS.contains(&name) {
                    add_file(acc, s(inp, "file_path").or_else(|| s(inp, "notebook_path")));
                } else if name == "Bash" {
                    bash(acc, s(inp, "command"));
                }
            }
            _ => {}
        }
    }
}

fn claude_system(acc: &mut Acc, d: &Value) {
    let ts = s(d, "timestamp");
    touch_ts(acc, ts);
    if s(d, "subtype") == Some("away_summary") {
        let text = match d.get("content") {
            Some(Value::String(c)) => c.clone(),
            Some(Value::Array(bs)) => bs.iter().map(|b| s(b, "text").unwrap_or("")).collect::<Vec<_>>().join(" "),
            _ => String::new(),
        };
        let t = text.trim();
        if !t.is_empty() {
            acc.away_summary = Some(util::take_chars(t, 1000));
            acc.away_at = ts.map(Into::into);
        }
    }
}

fn dispatch(acc: &mut Acc, d: &Value) {
    match s(d, "type") {
        Some("user") => claude_user(acc, d),
        Some("assistant") => claude_assistant(acc, d),
        Some("system") => claude_system(acc, d),
        Some(t) if CLAUDE_META_TYPES.contains(&t.as_bytes()) => claude_meta(acc, d),
        _ => {}
    }
}

/// Fold one raw transcript line into acc, decoding JSON only when it can matter.
pub fn claude_line(acc: &mut Acc, line: &[u8]) {
    if line.starts_with(b"{\"type\":\"") {
        let end = line[9..].iter().position(|&c| c == b'"').map(|e| 9 + e).unwrap_or(line.len());
        if CLAUDE_META_TYPES.contains(&&line[9..end])
            && let Some(d) = decode(line)
        {
            claude_meta(acc, &d);
        }
        return;
    }
    let head = &line[..line.len().min(800)];
    let pre = match find(head, b"\"message\":") {
        Some(mi) => &head[..mi],
        None => head,
    };
    if contains(pre, b"\"type\":\"user\"") {
        if contains(pre, b"\"isSidechain\":true") {
            return;
        }
        // tool results are most of the bytes: never decode them
        if contains(line, b"\"toolUseResult\"") || contains(head, b"\"type\":\"tool_result\"") {
            let ts = ts_search(line);
            touch_ts(acc, ts.as_deref());
            if let Some(i) = rfind(line, b"\"cwd\":\"") {
                let start = i + 7;
                if let Some(j) = line[start..].iter().position(|&c| c == b'"') {
                    let cwd = String::from_utf8_lossy(&line[start..start + j]).into_owned();
                    set_cwd(acc, Some(&cwd));
                }
            }
            return;
        }
    } else if contains(pre, b"\"type\":\"system\"") {
        if !contains(pre, b"away_summary") {
            let ts = ts_search(&line[..line.len().min(2000)]);
            touch_ts(acc, ts.as_deref());
            return;
        }
    } else if contains(pre, b"\"type\":\"attachment\"") || contains(pre, b"\"type\":\"progress\"") || (!contains(head, b"\"role\":\"assistant\"") && line.len() > 500_000) {
        return;
    }
    if let Some(d) = decode(line)
        && d.is_object()
    {
        dispatch(acc, &d);
    }
}

// ------------------------------------------------------------------ Codex records

fn codex_ptype(head: &[u8]) -> Option<&[u8]> {
    const KEY: &[u8] = b"\"payload\":{\"type\":\"";
    let i = find(head, KEY)? + KEY.len();
    let n = head[i..].iter().take_while(|c| c.is_ascii_lowercase() || **c == b'_').count();
    if n > 0 && head.get(i + n) == Some(&b'"') { Some(&head[i..i + n]) } else { None }
}

/// File names in `*** Add|Update|Delete File: <name>` lines of an apply_patch body, in order.
fn patch_files(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(i) = rest.find("*** ") {
        let after = &rest[i + 4..];
        let mut adv = i + 4;
        for kind in ["Add File: ", "Update File: ", "Delete File: "] {
            if let Some(name) = after.strip_prefix(kind) {
                let end = name.find(['\n', '\\', '"']).unwrap_or(name.len());
                if end > 0 {
                    out.push(name[..end].to_string());
                }
                adv = i + 4 + kind.len() + end;
                break;
            }
        }
        rest = &rest[adv..];
    }
    out
}

pub fn codex_line(acc: &mut Acc, line: &[u8]) {
    let head = &line[..line.len().min(300)];
    if contains(head, b"\"type\":\"session_meta\"") || contains(head, b"\"type\":\"turn_context\"") {
        let Some(d) = decode(line) else { return };
        let p = d.get("payload").unwrap_or(&Value::Null);
        if s(&d, "type") == Some("session_meta") {
            touch_ts(acc, s(p, "timestamp").or_else(|| s(&d, "timestamp")));
        }
        set_cwd(acc, s(p, "cwd"));
        return;
    }
    if !contains(head, b"\"type\":\"response_item\"") {
        if contains(head, b"\"task_complete\"") {
            let Some(d) = decode(line) else { return };
            if let Some(m) = d.get("payload").and_then(|p| s(p, "last_agent_message")) {
                let m = m.trim();
                if !m.is_empty() {
                    acc.last_text = Some(util::last_chars(m, LAST_TEXT_MAX));
                    acc.last_kind = Some("text".into());
                }
            }
        }
        return;
    }
    let ptype = codex_ptype(head);
    if !matches!(ptype, Some(b"message" | b"function_call" | b"custom_tool_call" | b"local_shell_call")) {
        let ts = ts_search(head);
        touch_ts(acc, ts.as_deref());
        return;
    }
    let Some(d) = decode(line) else { return };
    let ts = s(&d, "timestamp").map(String::from);
    touch_ts(acc, ts.as_deref());
    let p = d.get("payload").unwrap_or(&Value::Null);
    if ptype == Some(b"message") {
        let texts: Vec<&str> = p.get("content").and_then(Value::as_array).map(|cs| cs.iter().filter(|c| c.is_object()).map(|c| s(c, "text").unwrap_or("")).collect()).unwrap_or_default();
        match s(p, "role") {
            Some("user") => {
                let parts: Vec<&str> = texts.iter().map(|t| t.trim()).filter(|t| !t.is_empty() && !CODEX_WRAPPERS.iter().any(|w| t.starts_with(w))).collect();
                let text = parts.join(" ");
                let text = text.trim();
                if !text.is_empty() {
                    let cwd = acc.cwd.clone();
                    add_prompt(acc, text, ts.as_deref(), cwd.as_deref());
                }
            }
            Some("assistant") => {
                let txt = texts.join(" ");
                let txt = txt.trim();
                if !txt.is_empty() {
                    acc.last_text = Some(util::last_chars(txt, LAST_TEXT_MAX));
                    acc.last_text_at = ts.clone();
                    acc.last_kind = Some("text".into());
                }
            }
            _ => {}
        }
        return;
    }
    acc.last_kind = Some("tool".into());
    let body = match p.get("input").or_else(|| p.get("arguments")) {
        Some(Value::String(b)) => b.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    };
    if body.contains("*** Begin Patch") || s(p, "name") == Some("apply_patch") {
        for f in patch_files(&body) {
            let f = f.trim();
            let full = match &acc.cwd {
                Some(c) if !f.starts_with('/') => format!("{}/{}", c.trim_end_matches('/'), f),
                _ => f.to_string(),
            };
            add_file(acc, Some(&full));
        }
    } else {
        bash(acc, Some(&body));
    }
}

// ------------------------------------------------------------------ file scanning

pub type LineFn = fn(&mut Acc, &[u8]);

/// Fold complete lines from `offset` onwards into acc. Returns the new offset; a partial last line
/// is left for next time.
pub fn scan_file(path: &Path, acc: &mut Acc, offset: u64, line_fn: LineFn) -> std::io::Result<u64> {
    if store::is_forbidden(path) {
        return Ok(offset);
    }
    let mut f = fs::File::open(path)?;
    if offset > 0 {
        f.seek(SeekFrom::Start(offset))?;
    }
    let mut r = BufReader::with_capacity(256 * 1024, f);
    let mut pos = offset;
    let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
    loop {
        buf.clear();
        let n = r.read_until(b'\n', &mut buf)?;
        if n == 0 || buf.last() != Some(&b'\n') {
            break;
        }
        pos += n as u64;
        let line = buf.trim_ascii();
        if !line.is_empty() {
            line_fn(acc, line);
        }
        if buf.capacity() > 4 * 1024 * 1024 {
            buf = Vec::with_capacity(64 * 1024); // don't keep one huge line's buffer around
        }
    }
    Ok(pos)
}

// ------------------------------------------------------------------ subagents

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct SubMeta {
    pub agent_type: Option<String>,
    pub description: Option<String>,
    pub finished: Option<bool>,
    pub tool_use_id: Option<String>,
}

type MetaCache = HashMap<PathBuf, ((u64, f64), SubMeta)>;
static META_CACHE: OnceLock<Mutex<MetaCache>> = OnceLock::new();

/// Mtimes compared after a trip through JSON (serde_json may parse a float one ulp off).
fn same_time(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

fn mtime_of(m: &fs::Metadata) -> f64 {
    use std::os::unix::fs::MetadataExt;
    m.mtime() as f64 + m.mtime_nsec() as f64 / 1e9
}

fn read_meta(path: &Path) -> SubMeta {
    let Ok(st) = fs::metadata(path) else { return SubMeta::default() };
    let key = (st.len(), mtime_of(&st));
    let cache = META_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some((k, m)) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(path)
        && *k == key
    {
        return m.clone();
    }
    let meta: SubMeta = store::read_json(path).unwrap_or_default();
    let mut c = cache.lock().unwrap_or_else(|e| e.into_inner());
    if c.len() > 5000 {
        c.clear();
    }
    c.insert(path.to_path_buf(), (key, meta.clone()));
    meta
}

pub fn subagent_dir(acc: &Acc) -> Option<PathBuf> {
    let src = acc.source.as_deref()?;
    if acc.agent != "claude" {
        return None;
    }
    Some(Path::new(src).parent()?.join(&acc.id).join("subagents"))
}

pub struct SubFile {
    pub agent_id: String,
    pub path: PathBuf,
    pub size: u64,
    pub mtime: f64,
    pub meta: SubMeta,
}

/// A session's subagent files, cheaply (stat + tiny meta files), oldest first.
pub fn list_subagents(acc: &Acc) -> Vec<SubFile> {
    let Some(d) = subagent_dir(acc) else { return Vec::new() };
    let Ok(rd) = fs::read_dir(&d) else { return Vec::new() };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        let Some(aid) = n.strip_prefix("agent-").and_then(|x| x.strip_suffix(".jsonl")) else { continue };
        let p = d.join(&n);
        let Ok(st) = fs::metadata(&p) else { continue };
        let meta = read_meta(&d.join(format!("agent-{aid}.meta.json")));
        out.push(SubFile { agent_id: aid.into(), path: p, size: st.len(), mtime: mtime_of(&st), meta });
    }
    out.sort_by(|a, b| a.mtime.total_cmp(&b.mtime));
    out
}

/// "running" | "done" | "unknown"
pub fn sub_state(meta: &SubMeta, mtime: f64, now: f64) -> &'static str {
    if meta.finished == Some(true) {
        "done"
    } else if now - mtime < RUNNING_WINDOW {
        "running"
    } else {
        "unknown"
    }
}

pub fn subagent_counts(acc: &Acc, now: f64) -> SubagentCounts {
    let subs = list_subagents(acc);
    if subs.is_empty() {
        return SubagentCounts { total: acc.subagents_total, running: 0 };
    }
    let running = subs.iter().filter(|s| sub_state(&s.meta, s.mtime, now) == "running").count() as u32;
    SubagentCounts { total: subs.len() as u32, running }
}

fn first_user_text(path: &Path, limit: usize) -> Option<String> {
    let f = fs::File::open(path).ok()?;
    for (i, line) in BufReader::new(f).split(b'\n').enumerate() {
        if i > limit {
            break;
        }
        let Ok(line) = line else { break };
        if !contains(&line[..line.len().min(800)], b"\"type\":\"user\"") {
            continue;
        }
        let Some(d) = decode(&line) else { continue };
        let c = d.get("message").and_then(|m| m.get("content"));
        let text = match c {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(bs)) => bs.iter().filter(|b| s(b, "type") == Some("text")).map(|b| s(b, "text").unwrap_or("")).collect::<Vec<_>>().join(" "),
            _ => continue,
        };
        if !text.trim().is_empty() {
            return Some(text.trim().to_string());
        }
    }
    None
}

fn verb_of(name: &str) -> &'static str {
    match name {
        "Read" => "read",
        "Grep" | "Glob" | "LS" => "searched",
        "Edit" | "MultiEdit" | "NotebookEdit" => "edited",
        "Write" => "wrote",
        "Bash" => "ran",
        "WebFetch" | "WebSearch" => "fetched",
        "AskUserQuestion" | "SendMessage" => "asked",
        _ => "other",
    }
}

fn step_of(name: &str, inp: Option<&Value>, home: &Path) -> (String, String) {
    let verb = verb_of(name);
    let inp = inp.filter(|i| i.is_object()).unwrap_or(&Value::Null);
    let what = ["file_path", "notebook_path", "description", "command", "pattern", "url", "query", "path"].iter().find_map(|k| s(inp, k).filter(|v| !v.is_empty())).unwrap_or("");
    let what = if what.starts_with('/') { short_path(what, home) } else { what.to_string() };
    let mut what = util::take_chars(&util::squash(&what), 160);
    if verb == "other" {
        what = util::take_chars(&if what.is_empty() { name.to_string() } else { format!("{name}: {what}") }, 160);
    }
    (verb.into(), what)
}

fn handback_from_assistant(d: &Value) -> Option<String> {
    let content = d.get("message")?.get("content")?.as_array()?;
    let mut txt = None;
    for b in content {
        let t = s(b, "type");
        if t == Some("tool_use")
            && matches!(s(b, "name"), Some("SubagentHandback" | "StructuredOutput"))
            && let Some(m) = b.get("input").and_then(|i| s(i, "message")).map(str::trim).filter(|m| !m.is_empty())
        {
            return Some(m.into());
        }
        if t == Some("text")
            && let Some(x) = s(b, "text").map(str::trim).filter(|x| !x.is_empty())
        {
            txt = Some(x.to_string());
        }
    }
    txt
}

fn tail_lines(path: &Path, size: u64, nbytes: u64) -> Vec<Vec<u8>> {
    let Ok(mut f) = fs::File::open(path) else { return Vec::new() };
    let start = size.saturating_sub(nbytes);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut data = Vec::new();
    let _ = f.take(nbytes).read_to_end(&mut data);
    let mut lines: Vec<Vec<u8>> = data.split(|&c| c == b'\n').map(|l| l.to_vec()).collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    lines.retain(|l| !l.trim_ascii().is_empty());
    lines
}

/// What Wake keeps about a subagent (cache format: the Python summary dict).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct SubSum {
    pub agent_id: String,
    pub task: String,
    #[serde(rename = "type")]
    pub kind: String,
    /// running | done | unknown
    pub state: String,
    pub started_at: Option<String>,
    pub last_at: Option<String>,
    pub seconds: u32,
    pub tools: OMap<u32>,
    pub handed_back: Option<String>,
    pub live: Option<String>,
}

impl SubSum {
    pub fn to_contract(&self) -> SubagentSummary {
        SubagentSummary {
            agent_id: self.agent_id.clone(),
            task: self.task.clone(),
            kind: self.kind.clone(),
            state: match self.state.as_str() {
                "running" => State::Working,
                "done" => State::Ended,
                _ => State::Unknown,
            },
            started_at: self.started_at.clone(),
            last_at: self.last_at.clone(),
            seconds: self.seconds,
            tools: self.tools.0.clone(),
            handed_back: self.handed_back.clone(),
            live: self.live.clone(),
        }
    }
}

pub struct SubDetail {
    pub summary: SubSum,
    pub steps: Vec<Step>,
    pub handed_back_full: Option<String>,
}

/// Summarise one subagent transcript (and its steps when `steps`). Streams the file once.
pub fn summarize_subagent(f: &SubFile, steps: bool, now: f64, home: &Path) -> SubDetail {
    let mut tools: OMap<u32> = OMap::default();
    let bump = |tools: &mut OMap<u32>, n: &str| match tools.get_mut(n) {
        Some(c) => *c += 1,
        None => tools.0.push((n.into(), 1)),
    };
    let mut started: Option<String> = None;
    let mut last: Option<String> = None;
    let mut step_list: std::collections::VecDeque<Step> = std::collections::VecDeque::new();
    if let Ok(file) = fs::File::open(&f.path) {
        let mut r = BufReader::new(file);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match r.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if buf.last() != Some(&b'\n') {
                break;
            }
            let line = &buf[..];
            let head = &line[..line.len().min(800)];
            let is_asst = contains(head, b"\"role\":\"assistant\"");
            let ts = ts_search(head).or_else(|| if is_asst { ts_search(line) } else { None });
            if let Some(ts) = ts {
                if started.is_none() {
                    started = Some(ts.clone());
                }
                if last.as_deref().is_none_or(|l| ts.as_str() > l) {
                    last = Some(ts);
                }
            }
            if !is_asst {
                continue;
            }
            if !steps {
                for n in tool_names(line) {
                    bump(&mut tools, &n);
                }
                continue;
            }
            let Some(d) = decode(line) else { continue };
            if let Some(content) = d.get("message").and_then(|m| m.get("content")).and_then(Value::as_array) {
                for b in content {
                    if s(b, "type") == Some("tool_use") {
                        let name = s(b, "name").unwrap_or("?");
                        bump(&mut tools, name);
                        let (verb, what) = step_of(name, b.get("input"), home);
                        step_list.push_back(Step { at: s(&d, "timestamp").map(Into::into), verb, what });
                        if step_list.len() > 60 {
                            step_list.pop_front();
                        }
                    }
                }
            }
        }
    }
    let mut handed: Option<String> = None;
    let mut last_step: Option<(String, String)> = None;
    for line in tail_lines(&f.path, f.size, 512 * 1024).iter().rev() {
        if !contains(&line[..line.len().min(800)], b"\"role\":\"assistant\"") {
            continue;
        }
        let Some(d) = decode(line) else { continue };
        if last_step.is_none()
            && let Some(content) = d.get("message").and_then(|m| m.get("content")).and_then(Value::as_array)
        {
            for b in content {
                if s(b, "type") == Some("tool_use") {
                    last_step = Some(step_of(s(b, "name").unwrap_or("?"), b.get("input"), home));
                }
            }
        }
        handed = handback_from_assistant(&d);
        if handed.is_some() {
            break;
        }
    }
    let state = sub_state(&f.meta, f.mtime, now);
    let task = f.meta.description.clone().filter(|d| !d.is_empty()).or_else(|| first_user_text(&f.path, 40)).unwrap_or_default();
    let live = match (&last_step, state) {
        (Some((v, w)), "running") => Some(util::take_chars(format!("{v} {w}").trim(), 160)),
        _ => None,
    };
    if state == "running" {
        handed = None;
    }
    let seconds = match (started.as_deref().and_then(util::parse_iso), last.as_deref().and_then(util::parse_iso)) {
        (Some(a), Some(b)) => (b - a).max(0.0) as u32,
        _ => 0,
    };
    let mut sorted = tools.0.clone();
    sorted.sort_by_key(|a| std::cmp::Reverse(a.1)); // stable: ties keep first-seen order, like Counter.most_common
    let summary = SubSum {
        agent_id: f.agent_id.clone(),
        task: util::take_chars(&util::squash(&task), 300),
        kind: f.meta.agent_type.clone().unwrap_or_else(|| "agent".into()),
        state: state.into(),
        started_at: started,
        last_at: last.or_else(|| Some(util::iso_from_epoch(f.mtime))),
        seconds,
        tools: OMap(sorted),
        handed_back: handed.as_deref().map(|h| util::take_chars(h, 300)),
        live,
    };
    SubDetail { summary, steps: step_list.into_iter().collect(), handed_back_full: handed.as_deref().map(|h| util::take_chars(h, 4000)) }
}

// ------------------------------------------------------------------ summaries

/// (project id, name) by most user turns; worktree folders fold back to their repo.
/// The files a session changed, folded to their first seven path components (such as
/// `/home/sam/Projects/wake/crates/x`), edits summed; at most 40 folders.
pub fn fold_files(files: &OMap<u32>) -> OMap<u32> {
    let mut out: OMap<u32> = OMap::default();
    for (path, n) in files.iter() {
        let short: String = {
            let parts: Vec<&str> = path.split('/').take(8).collect();
            parts.join("/")
        };
        let full = out.len() >= 40;
        match out.get_mut(&short) {
            Some(c) => *c += n,
            None if !full => out.insert(short, *n),
            None => {}
        }
    }
    out
}

/// How Wake decided a session's project (added 2026-11-20).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum How {
    /// Where its turns ran (each record's `cwd`): sure.
    Worked,
    /// It ran from elsewhere (often `~`) but changed files in the project: a good guess.
    Files,
    /// Only its title names the project ("night-shift-rota-update"): a guess.
    Title,
}

impl How {
    pub fn word(self) -> &'static str {
        match self {
            How::Worked => "worked",
            How::Files => "files",
            How::Title => "title",
        }
    }
}

/// Lowercase words of `s`, split on anything not a letter or digit.
fn words_of(s: &str) -> Vec<String> {
    s.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(String::from).collect()
}

/// The project a title names: its whole name as consecutive words ("night-shift" in
/// "night-shift-rota-update"), else a long first word of a project's name that no other project
/// shares ("tideline" for tideline-desktop). One project only: two matches is no answer.
pub fn project_from_title(title: &str, projects: &[ProjectRec]) -> Option<String> {
    let tw = words_of(title);
    if tw.is_empty() {
        return None;
    }
    let live: Vec<&ProjectRec> = projects.iter().filter(|p| !p.removed).collect();
    let whole: Vec<&str> = live
        .iter()
        .filter(|p| {
            let nw = words_of(&p.name);
            !nw.is_empty() && tw.windows(nw.len()).any(|w| w == nw.as_slice())
        })
        .map(|p| p.id.as_str())
        .collect();
    if whole.len() == 1 {
        return Some(whole[0].to_string());
    }
    if whole.len() > 1 {
        return None;
    }
    let firsts: Vec<(&str, String)> = live.iter().filter_map(|p| words_of(&p.name).into_iter().next().map(|w| (p.id.as_str(), w))).collect();
    let hits: Vec<&str> = firsts.iter().filter(|(_, w)| w.chars().count() >= 6 && firsts.iter().filter(|(_, o)| o == w).count() == 1 && tw.contains(w)).map(|(id, _)| *id).collect();
    if hits.len() == 1 { Some(hits[0].to_string()) } else { None }
}

/// A session's project and how Wake knows: where its turns ran, else (a session started in `~`,
/// as every background job is) the files it changed, else the words of its title.
pub fn attribute_how(acc: &Acc, projects: &[ProjectRec], home: &Path) -> (Option<String>, Option<String>, Option<How>) {
    let name_of = |pid: &str| store::project_by_id(pid, projects).map(|p| p.name.clone());
    let (pid, _) = attribute_by_cwd(acc, projects, home);
    if let Some(pid) = pid {
        let n = name_of(&pid);
        return (Some(pid), n, Some(How::Worked));
    }
    let mut counts: Vec<(String, u32)> = Vec::new();
    for (path, n) in acc.files.iter() {
        if let Some(pid) = store::project_for_path(path, projects, false, home) {
            match counts.iter_mut().find(|(p, _)| *p == pid) {
                Some((_, c)) => *c += n,
                None => counts.push((pid, *n)),
            }
        }
    }
    if let Some((pid, _)) = counts.iter().fold(None::<&(String, u32)>, |b, c| if b.is_none_or(|b| c.1 > b.1) { Some(c) } else { b }) {
        return (Some(pid.clone()), name_of(pid), Some(How::Files));
    }
    for t in [acc.custom_title.as_deref(), acc.ai_title.as_deref(), acc.agent_name.as_deref()].into_iter().flatten() {
        if let Some(pid) = project_from_title(t, projects) {
            let n = name_of(&pid);
            return (Some(pid), n, Some(How::Title));
        }
    }
    (None, None, None)
}

pub fn attribute(acc: &Acc, projects: &[ProjectRec], home: &Path) -> (Option<String>, Option<String>) {
    let (p, n, _) = attribute_how(acc, projects, home);
    (p, n)
}

/// Where its turns ran: the project with most user turns, else most records.
fn attribute_by_cwd(acc: &Acc, projects: &[ProjectRec], home: &Path) -> (Option<String>, Option<String>) {
    let tally = |paths: &OMap<u32>| -> Vec<(String, u32)> {
        let mut counts: Vec<(String, u32)> = Vec::new();
        for (path, n) in paths.iter() {
            if path.is_empty() {
                continue;
            }
            let mut path = path.as_str();
            for (wp, orig) in acc.worktrees.iter() {
                if path == wp || (path.starts_with(wp.as_str()) && path.as_bytes().get(wp.len()) == Some(&b'/')) {
                    if let Some(o) = orig {
                        path = o;
                    }
                    break;
                }
            }
            if let Some(pid) = store::project_for_path(path, projects, false, home) {
                match counts.iter_mut().find(|(p, _)| *p == pid) {
                    Some((_, c)) => *c += n,
                    None => counts.push((pid, *n)),
                }
            }
        }
        counts
    };
    let mut counts = tally(&acc.proj_turns);
    if counts.is_empty() {
        counts = tally(&acc.cwd_seen);
    }
    let mut best: Option<&(String, u32)> = None;
    for c in &counts {
        if best.is_none_or(|b| c.1 > b.1) {
            best = Some(c);
        }
    }
    let Some((pid, _)) = best else { return (None, None) };
    let name = store::project_by_id(pid, projects).map(|p| p.name.clone());
    (Some(pid.clone()), name)
}

pub fn title_of(acc: &Acc) -> String {
    let t = acc.custom_title.clone().or_else(|| acc.ai_title.clone()).filter(|t| !t.is_empty());
    let t = t.unwrap_or_else(|| {
        let fp = acc.first_prompt.as_deref().or(acc.last_prompt_meta.as_deref()).unwrap_or("");
        util::take_chars(&util::squash(fp), 60)
    });
    if t.is_empty() { "Untitled session".into() } else { t }
}

pub fn resume_command(acc: &Acc, home: &Path) -> String {
    if acc.agent == "codex" {
        return format!("codex resume {}", acc.id);
    }
    let cmd = format!("claude --resume {}", acc.id);
    let Some(cwd) = acc.launch_cwd.as_deref() else { return cmd };
    let sp = short_path(cwd, home);
    let q = if sp == "~" {
        "~".to_string()
    } else if let Some(rest) = sp.strip_prefix("~/") {
        let plain = !rest.is_empty() && rest.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.' || c == '/' || c == '-');
        format!("~/{}", if plain { rest.to_string() } else { util::shell_quote(rest) })
    } else {
        util::shell_quote(&sp)
    };
    format!("cd {q} && {cmd}")
}

/// SessionSummary minus live fields (state, evidence, question, location are filled later).
pub fn base_summary(acc: &Acc, projects: &[ProjectRec], home: &Path) -> SessionSummary {
    let (pid, pname) = attribute(acc, projects, home);
    let last_prompt = acc.prompts_list.last().map(|p| p.text.clone()).or_else(|| acc.last_prompt_meta.clone());
    SessionSummary {
        id: acc.id.clone(),
        agent: if acc.agent == "codex" { Agent::Codex } else { Agent::Claude },
        project: pid,
        project_name: pname,
        title: title_of(acc),
        state: State::Ended,
        evidence: String::new(),
        started_at: acc.started_at.clone(),
        last_at: acc.last_at.clone(),
        minutes: (acc.active_sec / 60.0).round() as u32,
        prompts: acc.prompts,
        last_prompt: last_prompt.map(|p| util::take_chars(&p, 200)),
        away_summary: acc.away_summary.clone(),
        question: None,
        subagents: SubagentCounts { total: acc.subagents_total, running: 0 },
        cwd: acc.cwd.clone(),
        location: None,
        resume_command: Some(resume_command(acc, home)),
        job_id: None,
        pid: None,
        deleted_by_claude: acc.deleted_by_claude,
    }
}

pub fn prompts_out(acc: &Acc) -> Vec<Prompt> {
    acc.prompts_list.iter().map(|p| Prompt { at: p.at.clone(), text: p.text.clone() }).collect()
}

pub fn files_out(acc: &Acc) -> Vec<FileTouch> {
    let mut files = acc.files.0.clone();
    files.sort_by_key(|a| std::cmp::Reverse(a.1));
    files.into_iter().take(FILES_OUT).map(|(path, edits)| FileTouch { path, edits }).collect()
}

// ------------------------------------------------------------------ indexer

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileRec {
    pub size: u64,
    pub mtime: f64,
    pub offset: u64,
    pub session: String,
    pub agent: String,
}

fn codex_id(path: &str) -> String {
    let name = Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let stem = name.strip_suffix(".jsonl").unwrap_or(&name);
    util::uuid_tail(stem).unwrap_or(stem).to_string()
}

fn safe_name(sid: &str) -> String {
    sid.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect()
}

#[derive(Default)]
struct IxState {
    sessions: HashMap<String, Arc<Acc>>,
    files: HashMap<String, FileRec>,
    done: u32,
    total: u32,
    /// Transcripts changed lately that gave Wake nothing it understood (kept until they do).
    odd: std::collections::HashSet<String>,
    stats: IndexStats,
}

/// What the last indexing pass couldn't use. Files that couldn't be read or saved are tried again
/// on every pass, so these say how things stand now, not a running total.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexStats {
    /// Transcripts that couldn't be opened or read.
    pub unreadable: u32,
    /// Why the first of them couldn't be read, in words ("permission denied").
    pub unreadable_why: Option<String>,
    /// Transcripts changed in the last week that gave Wake nothing at all (no prompt, no time, no
    /// title): a sign that the agent changed how it writes them.
    pub not_understood: u32,
    /// Session summaries Wake couldn't save in its data folder.
    pub not_saved: u32,
}

/// How recent a transcript must be for "nothing understood" to count.
const ODD_WITHIN: f64 = 7.0 * 86400.0;

impl IndexStats {
    /// One plain sentence when anything couldn't be used, else None.
    pub fn sentence(&self) -> Option<String> {
        let s = |n: u32| if n == 1 { "" } else { "s" };
        let mut parts = Vec::new();
        if self.unreadable > 0 {
            let why = self.unreadable_why.as_ref().map(|w| format!(" ({w})")).unwrap_or_default();
            parts.push(format!("couldn't read {} transcript{}{why}", self.unreadable, s(self.unreadable)));
        }
        if self.not_understood > 0 {
            let noun = if parts.is_empty() { format!(" transcript{}", s(self.not_understood)) } else { String::new() };
            parts.push(format!("couldn't understand {}{noun}", self.not_understood));
        }
        if self.not_saved > 0 {
            parts.push(format!("couldn't save {} session summar{} in its data folder", self.not_saved, if self.not_saved == 1 { "y" } else { "ies" }));
        }
        if parts.is_empty() {
            return None;
        }
        let last = parts.pop().unwrap_or_default();
        let list = if parts.is_empty() { last } else { format!("{} and {last}", parts.join(", ")) };
        let format = if self.not_understood > 0 { " Claude Code may have changed its format." } else { "" };
        Some(format!("Wake {list}.{format} Run `wake doctor`."))
    }
}

/// What indexing one transcript came to.
enum Indexed {
    Done { understood: bool },
    Unreadable(std::io::Error),
    NotSaved,
}

/// Owns index-v2/files.json and index-v2/sessions/*.json. One writer (`run`), many readers.
pub struct Indexer {
    paths: Paths,
    writer: Writer,
    state: RwLock<IxState>,
    run_lock: Mutex<()>,
    loaded: AtomicBool,
}

impl Indexer {
    pub fn new(paths: Paths) -> Indexer {
        let writer = Writer::new(&paths.data);
        Indexer { paths, writer, state: RwLock::new(IxState::default()), run_lock: Mutex::new(()), loaded: AtomicBool::new(false) }
    }

    fn sessions_dir(&self) -> PathBuf {
        self.paths.index_dir().join("sessions")
    }
    fn session_path(&self, sid: &str) -> PathBuf {
        self.sessions_dir().join(format!("{}.json", safe_name(sid)))
    }
    fn files_path(&self) -> PathBuf {
        self.paths.index_dir().join("files.json")
    }
    fn sub_cache_path(&self, sid: &str) -> PathBuf {
        self.paths.index_dir().join("subagents").join(format!("{}.json", safe_name(sid)))
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, IxState> {
        self.state.read().unwrap_or_else(|e| e.into_inner())
    }
    fn write(&self) -> std::sync::RwLockWriteGuard<'_, IxState> {
        self.state.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Load what earlier runs saved. The first time, carry over the Python index's summaries of
    /// transcripts Claude has since deleted.
    pub fn load(&self) {
        let _ = self.writer.ensure_data_dir();
        let _ = self.writer.ensure_dir(&self.sessions_dir());
        let first = !self.files_path().exists();
        if first {
            self.import_python_index();
        }
        let files: HashMap<String, FileRec> = store::read_json(&self.files_path()).unwrap_or_default();
        let mut sessions = HashMap::new();
        if let Ok(rd) = fs::read_dir(self.sessions_dir()) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                if n.ends_with(".json")
                    && !n.starts_with('.')
                    && let Some(acc) = store::read_json::<Acc>(&e.path())
                    && !acc.id.is_empty()
                {
                    sessions.insert(acc.id.clone(), Arc::new(lite(&acc)));
                }
            }
        }
        let mut st = self.write();
        st.files = files;
        st.sessions = sessions;
        self.loaded.store(true, Ordering::SeqCst);
    }

    fn import_python_index(&self) {
        let old = self.paths.old_index_dir().join("sessions");
        let Ok(rd) = fs::read_dir(&old) else { return };
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if !n.ends_with(".json") || n.starts_with('.') {
                continue;
            }
            let Some(mut acc) = store::read_json::<Acc>(&e.path()) else { continue };
            if acc.id.is_empty() {
                continue;
            }
            let gone = acc.deleted_by_claude || acc.source.as_deref().is_none_or(|s| !Path::new(s).exists());
            if gone && !self.session_path(&acc.id).exists() {
                acc.deleted_by_claude = true;
                let _ = self.writer.write_json(&self.session_path(&acc.id), &acc);
            }
        }
    }

    pub fn discover(&self) -> Vec<(String, &'static str)> {
        let mut out = Vec::new();
        let list = |d: &Path| -> Vec<PathBuf> {
            let mut v: Vec<PathBuf> = fs::read_dir(d).map(|rd| rd.flatten().map(|e| e.path()).collect()).unwrap_or_default();
            v.sort();
            v
        };
        for dir in list(&self.paths.claude.join("projects")) {
            if !dir.is_dir() {
                continue;
            }
            for p in list(&dir) {
                if p.extension().is_some_and(|x| x == "jsonl") && !store::is_forbidden(&p) && p.is_file() {
                    out.push((p.to_string_lossy().into_owned(), "claude"));
                }
            }
        }
        for y in list(&self.paths.codex.join("sessions")) {
            for m in list(&y) {
                for d in list(&m) {
                    for p in list(&d) {
                        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        if name.starts_with("rollout-") && name.ends_with(".jsonl") && !store::is_forbidden(&p) {
                            out.push((p.to_string_lossy().into_owned(), "codex"));
                        }
                    }
                }
            }
        }
        out
    }

    pub fn read_acc(&self, sid: &str) -> Option<Acc> {
        store::read_json(&self.session_path(sid))
    }

    fn index_one(&self, path: &str, agent: &str, size: u64, mtime: f64) -> Indexed {
        let sid = if agent == "codex" { codex_id(path) } else { Path::new(path).file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default() };
        let prev = self.read().files.get(path).cloned();
        let mut acc = None;
        let mut offset = 0;
        if let Some(a) = self.read_acc(&sid)
            && a.source.as_deref() == Some(path)
        {
            // The summary's own offset is the truth; files.json can lag it after a crash.
            let resume = a.scan_offset.or_else(|| prev.as_ref().filter(|p| p.session == sid).map(|p| p.offset));
            if let Some(at) = resume
                && size >= at
            {
                acc = Some(a);
                offset = at;
            }
        }
        let mut acc = acc.unwrap_or_else(|| new_acc(&sid, Some(path), agent));
        acc.source = Some(path.into());
        acc.deleted_by_claude = false;
        let f: LineFn = if agent == "codex" { codex_line } else { claude_line };
        let offset = match scan_file(Path::new(path), &mut acc, offset, f) {
            Ok(o) => o,
            Err(e) => return Indexed::Unreadable(e),
        };
        if agent == "claude"
            && let Some(d) = subagent_dir(&acc)
            && let Ok(rd) = fs::read_dir(&d)
        {
            acc.subagents_total = rd.flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl")).count() as u32;
        }
        acc.scan_offset = Some(offset);
        if self.writer.write_json(&self.session_path(&sid), &acc).is_err() {
            // Not saved: leave the bookkeeping alone so the next run reads these lines again.
            return Indexed::NotSaved;
        }
        // Understood: something at all came out of it (a prompt, a time, a title, a summary line...).
        let understood = size == 0 || {
            let mut blank = new_acc(&sid, Some(path), agent);
            (blank.scan_offset, blank.subagents_total) = (acc.scan_offset, acc.subagents_total);
            serde_json::to_value(&blank).ok() != serde_json::to_value(&acc).ok()
        };
        let mut st = self.write();
        st.files.insert(path.into(), FileRec { size, mtime, offset, session: sid.clone(), agent: agent.into() });
        st.sessions.insert(sid, Arc::new(lite(&acc)));
        Indexed::Done { understood }
    }

    /// One incremental pass; only changed files are read. Returns how many files changed.
    pub fn run(&self) -> u32 {
        let _g = self.run_lock.lock().unwrap_or_else(|e| e.into_inner());
        if !self.loaded.load(Ordering::SeqCst) {
            self.load();
        }
        let found = self.discover();
        {
            let mut st = self.write();
            st.done = 0;
            st.total = found.len() as u32;
        }
        let mut seen = std::collections::HashSet::new();
        let mut dirty = 0u32;
        let mut stats = IndexStats::default();
        let now = util::now();
        for (path, agent) in &found {
            seen.insert(path.clone());
            if let Ok(m) = fs::metadata(path) {
                let (size, mtime) = (m.len(), mtime_of(&m));
                let unchanged = self.read().files.get(path).is_some_and(|p| p.size == size && same_time(p.mtime, mtime));
                if !unchanged {
                    match self.index_one(path, agent, size, mtime) {
                        Indexed::Done { understood } => {
                            let mut st = self.write();
                            if understood || now - mtime > ODD_WITHIN {
                                st.odd.remove(path);
                            } else {
                                st.odd.insert(path.clone());
                            }
                        }
                        // Gone between listing and opening: not a problem.
                        Indexed::Unreadable(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Indexed::Unreadable(e) => {
                            stats.unreadable += 1;
                            stats.unreadable_why.get_or_insert_with(|| e.kind().to_string());
                        }
                        Indexed::NotSaved => stats.not_saved += 1,
                    }
                    dirty += 1;
                    if dirty.is_multiple_of(25) {
                        self.save_files();
                    }
                }
            }
            self.write().done += 1;
        }
        // transcripts Claude has deleted: keep the summary, flag it
        let gone: Vec<(String, String)> = self.read().files.iter().filter(|(p, _)| !seen.contains(*p)).map(|(p, r)| (p.clone(), r.session.clone())).collect();
        for (p, sid) in gone {
            if let Some(mut acc) = self.read_acc(&sid)
                && !acc.deleted_by_claude
            {
                acc.deleted_by_claude = true;
                let _ = self.writer.write_json(&self.session_path(&sid), &acc);
                self.write().sessions.insert(sid.clone(), Arc::new(lite(&acc)));
            }
            self.write().files.remove(&p);
            dirty += 1;
        }
        let stale: Vec<String> =
            self.read().sessions.values().filter(|a| !a.deleted_by_claude && a.source.as_ref().is_some_and(|s| !seen.contains(s) && !Path::new(s).exists())).map(|a| a.id.clone()).collect();
        for sid in stale {
            let mut full = self.read_acc(&sid).or_else(|| self.read().sessions.get(&sid).map(|a| (**a).clone()));
            if let Some(full) = full.as_mut() {
                full.deleted_by_claude = true;
                let _ = self.writer.write_json(&self.session_path(&sid), &*full);
                self.write().sessions.insert(sid, Arc::new(lite(full)));
            }
        }
        if dirty > 0 || !self.files_path().exists() {
            self.save_files();
        }
        let mut st = self.write();
        st.odd.retain(|p| seen.contains(p));
        stats.not_understood = st.odd.len() as u32;
        st.stats = stats;
        dirty
    }

    /// What the last pass couldn't use (all zeros before the first pass ends).
    pub fn stats(&self) -> IndexStats {
        self.read().stats.clone()
    }

    fn save_files(&self) {
        let snap = self.read().files.clone();
        let _ = self.writer.write_json(&self.files_path(), &snap);
    }

    // ---- queries

    pub fn progress(&self) -> (u32, u32) {
        let st = self.read();
        (st.done, st.total)
    }

    pub fn loaded(&self) -> bool {
        self.loaded.load(Ordering::SeqCst)
    }

    pub fn all_lite(&self) -> Vec<Arc<Acc>> {
        self.read().sessions.values().cloned().collect()
    }

    pub fn get_lite(&self, sid: &str) -> Option<Arc<Acc>> {
        self.read().sessions.get(sid).cloned()
    }

    pub fn file_offset(&self, path: &str) -> Option<u64> {
        self.read().files.get(path).map(|f| f.offset)
    }

    /// Subagent summaries for a session, cached by file size, mtime and state.
    pub fn subagents(&self, acc: &Acc) -> Vec<SubSum> {
        #[derive(Serialize, Deserialize, Clone, Default)]
        struct Hit {
            size: u64,
            mtime: f64,
            state: String,
            summary: SubSum,
        }
        let cpath = self.sub_cache_path(&acc.id);
        let mut cache: OMap<Hit> = store::read_json(&cpath).unwrap_or_default();
        let subs = list_subagents(acc);
        if subs.is_empty() {
            let mut v: Vec<SubSum> = cache.0.into_iter().map(|(_, h)| h.summary).collect();
            v.sort_by(|a, b| a.started_at.cmp(&b.started_at));
            return v;
        }
        let now = util::now();
        let mut out = Vec::new();
        let mut changed = false;
        for f in &subs {
            let state = sub_state(&f.meta, f.mtime, now);
            if let Some(h) = cache.get(&f.agent_id)
                && h.size == f.size
                && same_time(h.mtime, f.mtime)
                && h.state == state
            {
                out.push(h.summary.clone());
                continue;
            }
            let d = summarize_subagent(f, false, now, &self.paths.home);
            cache.insert(f.agent_id.clone(), Hit { size: f.size, mtime: f.mtime, state: state.into(), summary: d.summary.clone() });
            out.push(d.summary);
            changed = true;
        }
        if changed {
            let _ = self.writer.write_json(&cpath, &cache);
        }
        out
    }

    pub fn subagent_detail(&self, sid: &str, aid: &str) -> Option<SubagentDetail> {
        let acc = self.read_acc(sid)?;
        if let Some(f) = list_subagents(&acc).into_iter().find(|f| f.agent_id == aid) {
            let d = summarize_subagent(&f, true, util::now(), &self.paths.home);
            return Some(SubagentDetail { summary: d.summary.to_contract(), steps: d.steps, handed_back_full: d.handed_back_full });
        }
        #[derive(Deserialize)]
        struct Hit {
            summary: SubSum,
        }
        let cache: HashMap<String, Hit> = store::read_json(&self.sub_cache_path(sid)).unwrap_or_default();
        let h = cache.get(aid)?;
        Some(SubagentDetail { summary: h.summary.to_contract(), steps: Vec::new(), handed_back_full: h.summary.handed_back.clone() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::store::testenv::Env;

    struct Fx {
        env: Env,
        ix: Indexer,
        projects: Vec<ProjectRec>,
    }

    fn setup() -> Fx {
        let env = Env::new();
        let ix = Indexer::new(env.paths.clone());
        ix.run();
        let projects = Store::new(env.paths.clone()).load_projects();
        Fx { env, ix, projects }
    }

    impl Fx {
        fn summary(&self, sid: &str) -> SessionSummary {
            base_summary(&self.ix.get_lite(sid).unwrap(), &self.projects, &self.env.paths.home)
        }
    }

    #[test]
    fn prompts_and_title() {
        let f = setup();
        let s = f.summary("sess-aaaa");
        assert_eq!(s.title, "Parser tidy");
        assert_eq!(s.prompts, 4);
        let acc = f.ix.read_acc("sess-aaaa").unwrap();
        let texts: Vec<&str> = acc.prompts_list.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(texts[1], "[pasted text] fix this trace");
        assert!(!texts.join(" ").contains("secret"));
        assert_eq!(texts[2], "now run the tests again");
        assert_eq!(s.last_prompt.as_deref(), Some("check beta builds"));
        assert_eq!(s.away_summary.as_deref(), Some("You tidied the parser; tests pass."));
        assert_eq!(s.started_at.as_deref(), Some("2026-11-19T10:00:00.000Z"));
        assert_eq!(s.last_at.as_deref(), Some("2026-11-19T10:04:00.000Z"));
        assert_eq!(s.minutes, 4);
    }

    #[test]
    fn attribution_folds_worktrees() {
        let f = setup();
        let s = f.summary("sess-aaaa");
        assert_eq!(s.project.as_deref(), Some("alpha"));
        assert_eq!(s.project_name.as_deref(), Some("alpha"));
        assert_eq!(f.summary("sess-bbbb").project, None);
    }

    #[test]
    fn detail_counts() {
        let f = setup();
        let acc = f.ix.read_acc("sess-aaaa").unwrap();
        let files = files_out(&acc);
        assert_eq!(files[0].edits, 2);
        assert!(files[0].path.ends_with("/src/parser.py"));
        assert_eq!(acc.commands, 2);
        assert_eq!(acc.tests, 1);
        assert_eq!(acc.pr_links, ["https://github.com/example/alpha/pull/7"]);
        assert_eq!(acc.cost, Some(1.25));
    }

    #[test]
    fn tool_results_and_blobs_never_decoded() {
        let f = setup();
        DECODED.with(|d| d.borrow_mut().clear());
        let mut acc = new_acc("x", None, "claude");
        scan_file(&f.env.transcript("sess-aaaa"), &mut acc, 0, claude_line).unwrap();
        let decoded = DECODED.with(|d| d.borrow().clone());
        assert!(!decoded.is_empty());
        // the markers sit past the first 60 bytes, so check the whole lines by their uuid prefix instead
        let raw = fs::read(f.env.transcript("sess-aaaa")).unwrap();
        for line in raw.split(|&c| c == b'\n') {
            if contains(line, b"HUGE_TOOL_RESULT") || contains(line, b"NEVER_PARSE_ME") {
                let head = &line[..line.len().min(60)];
                assert!(!decoded.iter().any(|d| d == head), "decoded {}", String::from_utf8_lossy(head));
            }
        }
        assert_eq!(acc.prompts, 4);
    }

    #[test]
    fn incremental_append_and_partial_line() {
        let f = setup();
        let path = f.env.transcript("sess-bbbb");
        let ps = path.to_string_lossy().into_owned();
        let before = f.ix.file_offset(&ps).unwrap();
        let rec = serde_json::json!({"parentUuid": null, "isSidechain": false, "type": "user",
            "message": {"role": "user", "content": "one more question"}, "uuid": "z",
            "timestamp": "2026-11-19T12:00:00.000Z", "cwd": format!("{}/beta", f.env.paths.projects.display()),
            "sessionId": "sess-bbbb"});
        let line = format!("{rec}\n");
        let mut data = fs::read_to_string(&path).unwrap();
        data.push_str(&line);
        data.push_str(&line[..30]);
        fs::write(&path, &data).unwrap();
        f.ix.run();
        assert_eq!(f.ix.file_offset(&ps).unwrap(), before + line.len() as u64);
        let s = f.summary("sess-bbbb");
        assert_eq!(s.prompts, 2);
        assert_eq!(s.project.as_deref(), Some("beta"));
        data.push_str(&line[30..]);
        fs::write(&path, &data).unwrap();
        f.ix.run();
        assert_eq!(f.summary("sess-bbbb").prompts, 3);
    }

    #[test]
    fn counts_what_it_could_not_read_understand_or_save() {
        use std::os::unix::fs::PermissionsExt;
        let f = setup();
        assert_eq!(f.ix.stats(), IndexStats::default());
        assert_eq!(f.ix.stats().sentence(), None);
        let dir = f.env.transcript("x").parent().unwrap().to_path_buf();
        // a transcript in a shape Wake doesn't know: no prompt, no time
        fs::write(dir.join("sess-odd1.jsonl"), "{\"kind\":\"turn\",\"body\":\"hello\"}\n").unwrap();
        // one that can't be opened
        let locked = dir.join("sess-locked.jsonl");
        fs::write(&locked, "{\"type\":\"summary\",\"summary\":\"Old work\"}\n").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        // Root (as in CI's container) reads it anyway, so the permission cases only hold for a
        // normal user; the format case holds for everyone.
        let enforced = fs::File::open(&locked).is_err();
        f.ix.run();
        let st = f.ix.stats();
        if !enforced {
            assert_eq!((st.unreadable, st.not_understood, st.not_saved), (0, 1, 0), "{st:?}");
            return;
        }
        assert_eq!((st.unreadable, st.not_understood, st.not_saved), (1, 1, 0), "{st:?}");
        assert_eq!(st.sentence().as_deref(), Some("Wake couldn't read 1 transcript (permission denied) and couldn't understand 1. Claude Code may have changed its format. Run `wake doctor`."));
        // the odd one stays counted on later passes (it isn't read again), until it is gone
        f.ix.run();
        assert_eq!(f.ix.stats().not_understood, 1);
        fs::remove_file(dir.join("sess-odd1.jsonl")).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o600)).unwrap();
        f.ix.run();
        assert_eq!(f.ix.stats(), IndexStats::default());
        // summaries that can't be saved
        let sessions = f.env.paths.index_dir().join("sessions");
        fs::set_permissions(&sessions, fs::Permissions::from_mode(0o500)).unwrap();
        fs::write(dir.join("sess-new1.jsonl"), "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"},\"timestamp\":\"2026-11-19T12:00:00.000Z\"}\n").unwrap();
        f.ix.run();
        fs::set_permissions(&sessions, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(f.ix.stats().not_saved, 1);
        assert_eq!(f.ix.stats().sentence().as_deref(), Some("Wake couldn't save 1 session summary in its data folder. Run `wake doctor`."));
    }

    /// A crash after a session file was written but before files.json caught up must not count the
    /// same lines twice when the next run starts.
    #[test]
    fn crash_between_writes_does_not_double_count() {
        let f = setup();
        let path = f.env.transcript("sess-bbbb");
        let files_json = f.env.paths.index_dir().join("files.json");
        let old_files = fs::read(&files_json).unwrap(); // files.json as it was before the crash
        let rec = serde_json::json!({"type": "user", "message": {"role": "user", "content": "and the tests?"}, "uuid": "y",
            "timestamp": "2026-11-19T12:00:00.000Z", "sessionId": "sess-bbbb"});
        let mut data = fs::read_to_string(&path).unwrap();
        data.push_str(&format!("{rec}\n"));
        fs::write(&path, &data).unwrap();
        f.ix.run();
        let full = f.ix.read_acc("sess-bbbb").unwrap();
        assert_eq!(full.prompts, 2);
        assert_eq!(full.scan_offset, Some(data.len() as u64));
        // the crash: the summary was saved, files.json was not
        fs::write(&files_json, &old_files).unwrap();
        let after = Indexer::new(f.env.paths.clone());
        after.run();
        let acc = after.read_acc("sess-bbbb").unwrap();
        assert_eq!((acc.prompts, acc.prompts_list.len()), (2, 2));
        // and if files.json lost the file entirely, the summary still resumes rather than restarting
        fs::write(&files_json, "{}").unwrap();
        let again = Indexer::new(f.env.paths.clone());
        again.run();
        assert_eq!(again.read_acc("sess-bbbb").unwrap().prompts, 2);
    }

    #[test]
    fn unchanged_files_are_not_reread() {
        let f = setup();
        assert_eq!(f.ix.run(), 0);
        let fresh = Indexer::new(f.env.paths.clone()); // after a restart, from files.json
        assert_eq!(fresh.run(), 0);
    }

    #[test]
    fn deleted_transcript_keeps_summary() {
        let f = setup();
        fs::remove_file(f.env.transcript("sess-bbbb")).unwrap();
        f.ix.run();
        let s = f.summary("sess-bbbb");
        assert!(s.deleted_by_claude);
        assert_eq!(s.prompts, 1);
        let fresh = Indexer::new(f.env.paths.clone());
        fresh.run();
        assert!(fresh.get_lite("sess-bbbb").unwrap().deleted_by_claude);
    }

    #[test]
    fn python_index_carried_over() {
        let env = Env::new();
        let old = env.paths.old_index_dir().join("sessions");
        fs::create_dir_all(&old).unwrap();
        fs::write(
            old.join("gone.json"),
            r#"{"id":"gone","agent":"claude","source":"/nonexistent/gone.jsonl","startedAt":"2026-01-01T00:00:00.000Z",
            "lastAt":"2026-01-01T01:00:00.000Z","prompts":3,"promptsList":[{"at":null,"text":"hi"}],"firstPrompt":"hi",
            "files":{"/a":2,"/b":1},"commands":0,"tests":0,"prLinks":[],"cost":null,"aiTitle":"Old work","customTitle":null,
            "lastPromptMeta":null,"agentName":null,"awaySummary":null,"awayAt":null,"cwd":"/x","launchCwd":"/x","gitBranch":null,
            "projTurns":{"/x":3},"worktrees":{},"lastText":null,"lastTextAt":null,"lastKind":null,"lastUserAt":null,
            "activeSec":120,"prevTs":1767229200.0,"subagentsTotal":0,"deletedByClaude":false,"cwdSeen":{"/x":4}}"#,
        )
        .unwrap();
        let ix = Indexer::new(env.paths.clone());
        ix.run();
        let a = ix.get_lite("gone").unwrap();
        assert!(a.deleted_by_claude);
        assert_eq!(title_of(&a), "Old work");
        assert_eq!(ix.read_acc("gone").unwrap().files.0, vec![("/a".to_string(), 2), ("/b".to_string(), 1)]);
    }

    #[test]
    fn subagents() {
        let f = setup();
        let acc = f.ix.get_lite("sess-aaaa").unwrap();
        let dir = subagent_dir(&acc).unwrap();
        let a2 = dir.join("agent-a2.jsonl");
        let now = util::now();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        fs::File::options().write(true).open(&a2).unwrap().set_modified(old).unwrap();
        let c = subagent_counts(&acc, now);
        assert_eq!((c.total, c.running), (2, 0));
        fs::File::options().write(true).open(&a2).unwrap().set_modified(std::time::SystemTime::now()).unwrap();
        let c = subagent_counts(&acc, util::now());
        assert_eq!((c.total, c.running), (2, 1));
        let subs = f.ix.subagents(&f.ix.read_acc("sess-aaaa").unwrap());
        let a1 = subs.iter().find(|s| s.agent_id == "a1").unwrap();
        assert_eq!(a1.task, "Review parser");
        assert_eq!(a1.state, "done");
        assert_eq!(a1.handed_back.as_deref(), Some("No bugs found. The parser change is safe."));
        assert_eq!(a1.tools.0, vec![("Read".into(), 1), ("Grep".into(), 1), ("SubagentHandback".into(), 1)]);
        assert_eq!(a1.seconds, 25);
        let s2 = subs.iter().find(|s| s.agent_id == "a2").unwrap();
        assert_eq!(s2.task, "Explore the beta build scripts");
        assert_eq!(s2.state, "running");
        assert_eq!(s2.live.as_deref(), Some("ran Dry-run the build"));
        let d = f.ix.subagent_detail("sess-aaaa", "a1").unwrap();
        let verbs: Vec<&str> = d.steps.iter().map(|s| s.verb.as_str()).collect();
        assert_eq!(verbs, ["read", "searched", "other"]);
        assert_eq!(d.handed_back_full.as_deref(), Some("No bugs found. The parser change is safe."));
        assert_eq!(d.summary.state, State::Ended);
    }

    #[test]
    fn codex_adapter() {
        let f = setup();
        let cid = "0193c2aa-0000-7c93-9c1d-5e2f8a0b4c17";
        let s = f.summary(cid);
        assert_eq!(s.agent, Agent::Codex);
        assert_eq!(s.prompts, 1);
        assert_eq!(s.title, "Add a README to beta");
        assert_eq!(s.project.as_deref(), Some("beta"));
        let acc = f.ix.read_acc(cid).unwrap();
        assert!(files_out(&acc)[0].path.ends_with("/beta/README.md"));
        assert_eq!(s.resume_command.as_deref(), Some(format!("codex resume {cid}").as_str()));
    }

    #[test]
    fn helpers() {
        assert!(ends_with_question(Some("Shall I go ahead?**")));
        assert!(!ends_with_question(Some("Done. Any questions? No.")));
        assert_eq!(clean_prompt(&Value::from("<command-name>/clear</command-name>")), None);
        assert_eq!(clean_prompt(&serde_json::json!([{"type": "tool_result", "content": "x"}])), None);
        assert_eq!(clean_prompt(&serde_json::json!([{"type": "image"}])).as_deref(), Some("[image]"));
        assert_eq!(codex_id("/x/rollout-2026-11-19T11-00-00-0193c2aa-0000-7c93-9c1d-5e2f8a0b4c17.jsonl"), "0193c2aa-0000-7c93-9c1d-5e2f8a0b4c17");
        assert_eq!(patch_files("*** Begin Patch\n*** Update File: b.rs\n*** Add File: a.md\n"), ["b.rs", "a.md"]);
        assert_eq!(tool_names(br#"{"type":"tool_use","id":"t1","name":"Read","input":{}},{"type":"tool_use","id":"t2","name":"Bash"}"#), ["Read", "Bash"]);
        assert_eq!(ts_search(br#"{"timestamp":"short","x":1,"timestamp":"2026-11-19T10:00:00.000Z"}"#).as_deref(), Some("2026-11-19T10:00:00.000Z"));
        let home = Path::new("/var/home/u");
        let mut acc = new_acc("s1", None, "claude");
        acc.launch_cwd = Some("/var/home/u/Projects/my app".into());
        assert_eq!(resume_command(&acc, home), "cd ~/'Projects/my app' && claude --resume s1");
        acc.launch_cwd = Some("/var/home/u".into());
        assert_eq!(resume_command(&acc, home), "cd ~ && claude --resume s1");
    }
}
