//! Wake's own log: one plain line for each thing Wake did that changed something (stopping an agent,
//! pulling, moving a folder to the Trash, starting an agent, ...) and each error the person was shown,
//! so "what did Wake do?" always has an answer.
//!
//! It lives in Wake's data folder (`~/.local/share/wake/wake.log`), is only ever appended to, and is
//! kept small: past 256 KB it becomes `wake.log.1` (replacing the older one) and a new file starts.
//!
//! What it never holds: prompts, transcript text, or another process's command line (a stray's
//! arguments can hold keys). Callers pass what Wake itself said and did; the log keeps one line of
//! it and hides any password written into a URL.

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::store::{Paths, Writer};
use crate::util;

/// Past this the log becomes `wake.log.1`.
pub const ROTATE_AT: u64 = 256 * 1024;
/// Longest message or step kept, in characters.
const MAX_FIELD: usize = 600;

static LOG_LOCK: Mutex<()> = Mutex::new(());

/// Where the log is.
pub fn path(paths: &Paths) -> PathBuf {
    paths.data.join("wake.log")
}

/// Record something Wake did that changed something, or tried to. `what` is a short verb ("stop",
/// "force", "pull", "move to Trash", "remove worktree", "start an agent", "go to", "reopen");
/// `target` says what it was done to, in words (a task title and PID, a project and its `~/` folder);
/// `steps` are the exact steps from the action's result. Never pass a prompt or a command line Wake
/// read from another process. Writing the log never fails the action: errors are ignored.
pub fn record_action(paths: &Paths, what: &str, target: &str, ok: bool, message: &str, steps: &[String]) {
    let mut line = format!("{}  {}  {}  {}  {}", stamp(), clean(what), clean(target), if ok { "ok" } else { "failed" }, clean(message));
    if !steps.is_empty() {
        line.push_str("  steps: ");
        line.push_str(&steps.iter().map(|s| clean(s)).collect::<Vec<_>>().join(" ; "));
    }
    append(paths, &line);
}

/// Record an error the person was shown (a toast, a banner) or one that happened out of sight (a
/// background pass that failed). `context` says where ("Pull", "background indexing").
pub fn note_error(paths: &Paths, context: &str, message: &str) {
    append(paths, &format!("{}  error  {}  failed  {}", stamp(), clean(context), clean(message)));
}

/// The last `n` lines, oldest first (reading the previous file too when the current one is short).
pub fn recent(paths: &Paths, n: usize) -> Vec<String> {
    if n == 0 {
        return vec![];
    }
    let p = path(paths);
    let mut lines = tail_lines(&p, n);
    if lines.len() < n {
        let mut older = tail_lines(&rotated(&p), n - lines.len());
        older.append(&mut lines);
        lines = older;
    }
    lines
}

fn rotated(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".1");
    PathBuf::from(s)
}

/// Seconds-precision UTC time, `2026-11-20 14:03:09 UTC`.
fn stamp() -> String {
    let iso = util::now_iso();
    format!("{} {} UTC", &iso[..10], &iso[11..19])
}

/// One line, bounded, with any `user:password@` in a URL hidden.
fn clean(s: &str) -> String {
    let one: String = s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let one = util::take_chars(one.split_whitespace().collect::<Vec<_>>().join(" ").as_str(), MAX_FIELD);
    hide_url_passwords(&one)
}

fn hide_url_passwords(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        let end = tail.find(|c: char| c == '/' || c.is_whitespace() || c == '\'' || c == '"').unwrap_or(tail.len());
        match tail[..end].rfind('@') {
            Some(at) => {
                out.push_str("***");
                out.push_str(&tail[at..end]);
            }
            None => out.push_str(&tail[..end]),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

fn append(paths: &Paths, line: &str) {
    let _g = LOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if Writer::new(&paths.data).ensure_data_dir().is_err() {
        return;
    }
    let p = path(paths);
    if fs::metadata(&p).is_ok_and(|m| m.len() > ROTATE_AT) {
        let _ = fs::rename(&p, rotated(&p));
    }
    if let Ok(mut f) = fs::OpenOptions::new().append(true).create(true).mode(0o600).open(&p) {
        let _ = f.write_all(format!("{line}\n").as_bytes());
    }
}

/// The last `n` lines of a file, reading at most its last 256 KB plus a little.
fn tail_lines(p: &Path, n: usize) -> Vec<String> {
    let Ok(mut f) = fs::File::open(p) else {
        return vec![];
    };
    let len = f.metadata().map_or(0, |m| m.len());
    let from = len.saturating_sub(ROTATE_AT + 64 * 1024);
    if f.seek(SeekFrom::Start(from)).is_err() {
        return vec![];
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return vec![];
    }
    let text = String::from_utf8_lossy(&buf);
    let mut all: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
    if from > 0 && !all.is_empty() {
        all.remove(0); // probably cut in the middle
    }
    all[all.len().saturating_sub(n)..].iter().map(|s| s.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::scratch;

    #[test]
    fn writes_one_plain_line_per_entry_and_reads_them_back() {
        let dir = scratch("wakelog");
        let paths = Paths::under(&dir.0);
        assert!(recent(&paths, 5).is_empty());
        record_action(&paths, "stop", "'Fix the parser' (PID 4242), Claude", true, "Asked 'Fix the parser' (PID 4242) to exit.\nmore", &["kill -TERM 4242".into()]);
        note_error(&paths, "Pull", "Couldn't reach https://bob:s3cret@example.com/x.git");
        let lines = recent(&paths, 5);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].ends_with("stop  'Fix the parser' (PID 4242), Claude  ok  Asked 'Fix the parser' (PID 4242) to exit. more  steps: kill -TERM 4242"), "{}", lines[0]);
        assert!(lines[0].contains(" UTC  "));
        assert!(lines[1].contains("error  Pull  failed  Couldn't reach https://***@example.com/x.git"), "{}", lines[1]);
        assert!(!lines[1].contains("s3cret"));
        assert_eq!(recent(&paths, 1), lines[1..].to_vec());
        assert_eq!(path(&paths), dir.0.join("data/wake.log"));
    }

    #[test]
    fn rotates_past_256_kb_and_keeps_one_old_file() {
        let dir = scratch("wakelog-rotate");
        let paths = Paths::under(&dir.0);
        let long = "x".repeat(500);
        for i in 0..1500 {
            record_action(&paths, "go to", &format!("task {i}"), true, &long, &[]);
        }
        let cur = fs::metadata(path(&paths)).unwrap().len();
        let old = fs::metadata(rotated(&path(&paths))).unwrap().len();
        assert!(cur <= ROTATE_AT + 1024 && old <= ROTATE_AT + 1024, "{cur} {old}");
        assert!(!dir.0.join("data/wake.log.2").exists());
        let last = recent(&paths, 3);
        assert_eq!(last.len(), 3);
        assert!(last[2].contains("task 1499"));
        // Reading back crosses into the older file when asked for more than the new one holds.
        let lines_now = fs::read_to_string(path(&paths)).unwrap().lines().count();
        let more = recent(&paths, lines_now + 2);
        assert_eq!(more.len(), lines_now + 2);
        assert!(more.last().unwrap().contains("task 1499"));
    }

    #[test]
    fn hides_passwords_in_urls_only() {
        assert_eq!(hide_url_passwords("git@github.com:me/x.git"), "git@github.com:me/x.git");
        assert_eq!(hide_url_passwords("a https://t0ken@github.com/me/x b https://ok.org/a@b"), "a https://***@github.com/me/x b https://ok.org/a@b");
    }
}
