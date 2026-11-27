//! Behind the remote: has someone pushed commits this computer hasn't pulled?
//!
//! Read-only. The local tracking ref (`origin/main`) is only as fresh as the last fetch, and Wake
//! never fetches (a fetch writes refs), so Wake asks the remote what the branch points at with
//! `git ls-remote <remote> <branch>`, which writes nothing, and compares that commit with HEAD
//! locally (`cat-file -e`, `merge-base --is-ancestor`, `rev-list --count`). It runs with no
//! prompts (GIT_TERMINAL_PROMPT=0, ssh BatchMode, no askpass) and a hard timeout; offline or
//! signed out is "couldn't check", never a warning.
//!
//! [`Watch`] notices the moment worth being loud about: a project is behind and the person starts
//! building in it (a session goes from idle to working, or the working tree goes from clean to
//! dirty). It raises one alarm per behind-episode, which the person can acknowledge, and asks for
//! one desktop notification per episode.
//!
//! The one git write in Wake is [`pull_ff_only`]: `git pull --ff-only`, only on a clean tree that
//! can fast-forward, and only after the person confirms it.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::gitinfo::{self, GitError};
use crate::run::Runner;
use crate::types::{ActionResult, Behind};
use crate::util;

/// The longest one `ls-remote` may take (ssh gives up connecting after 8 s).
pub const NET_TIMEOUT: Duration = Duration::from_secs(20);
/// The longest a pull may take.
pub const PULL_TIMEOUT: Duration = Duration::from_secs(120);
/// The fix Wake suggests and, when asked, runs.
pub const PULL: &str = "git pull --ff-only";

/// Environment for talking to a remote: never prompt, never pop up a password dialog.
pub const NET_ENV: [(&str, &str); 6] = [
    ("GIT_TERMINAL_PROMPT", "0"),
    // No shared connection: an ssh master left behind would hold the output pipe open.
    ("GIT_SSH_COMMAND", "ssh -o BatchMode=yes -o ConnectTimeout=8 -o ControlMaster=no -o ControlPath=none"),
    ("GIT_ASKPASS", "/bin/false"),
    ("SSH_ASKPASS", "/bin/false"),
    ("SSH_ASKPASS_REQUIRE", "never"),
    ("GIT_MERGE_AUTOEDIT", "no"),
];

/// Where a repo stands against its remote.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Detached HEAD, or a branch that tracks nothing on a remote.
    NoUpstream,
    /// HEAD has everything the remote has; `ahead` commits are not pushed yet.
    InStep { ahead: u32 },
    /// The remote has commits HEAD doesn't: `count` of them when they are on this computer (None:
    /// not fetched yet), and `ahead` commits here the remote doesn't have.
    Behind { count: Option<u32>, ahead: u32 },
    /// The remote couldn't be asked and the last fetch shows nothing to pull: offline, signed
    /// out, or the branch is gone there. Quiet.
    CouldNotCheck(String),
}

/// One repo's answer.
#[derive(Debug, Clone)]
pub struct RepoCheck {
    pub path: String,
    /// "origin/main" (empty with no upstream).
    pub upstream: String,
    /// "GitHub", "GitLab" or "the remote".
    pub host: String,
    pub outcome: Outcome,
    /// What the remote's branch pointed at when asked; None if it couldn't be asked.
    pub remote_sha: Option<String>,
    /// Why the remote couldn't be asked, in a few words.
    pub problem: Option<String>,
    /// When the remote was last asked successfully (ISO-8601 UTC).
    pub asked_at: Option<String>,
    /// When this check ran, for scheduling.
    pub at: Instant,
}

impl RepoCheck {
    pub fn is_behind(&self) -> bool {
        matches!(self.outcome, Outcome::Behind { .. })
    }
}

/// The branch HEAD is on and what it tracks, read locally.
struct Tracking {
    remote: String,
    /// refs/heads/main on the remote.
    remote_ref: String,
    /// refs/remotes/origin/main here.
    local_ref: String,
    /// origin/main.
    short: String,
}

fn tracking(p: &Path) -> Option<Tracking> {
    let full = gitinfo::git(p, &["rev-parse", "--symbolic-full-name", "HEAD"]).ok()?;
    let full = full.trim();
    if !full.starts_with("refs/heads/") {
        return None;
    }
    let out = gitinfo::git(p, &["for-each-ref", "--format=%(upstream:remotename)%09%(upstream:remoteref)%09%(upstream)%09%(upstream:short)", full]).ok()?;
    let line = out.lines().next()?;
    let f: Vec<&str> = line.split('\t').collect();
    let (remote, remote_ref, local_ref, short) = (f.first()?, f.get(1)?, f.get(2)?, f.get(3)?);
    // "." is a branch tracking another local branch: nothing to ask a remote about.
    if remote.is_empty() || *remote == "." || remote_ref.is_empty() || remote.starts_with('-') || remote_ref.starts_with('-') {
        return None;
    }
    Some(Tracking { remote: remote.to_string(), remote_ref: remote_ref.to_string(), local_ref: local_ref.to_string(), short: short.to_string() })
}

fn rev(p: &Path, what: &str) -> Option<String> {
    gitinfo::git(p, &["rev-parse", "--verify", "--quiet", what]).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn count(p: &Path, range: &str) -> Option<u32> {
    gitinfo::git(p, &["rev-list", "--count", range]).ok()?.trim().parse().ok()
}

fn has_commit(p: &Path, sha: &str) -> bool {
    matches!(gitinfo::git_code(p, &["cat-file", "-e", &format!("{sha}^{{commit}}")], &[], gitinfo::TIMEOUT), Ok((0, _, _)))
}

fn is_ancestor(p: &Path, a: &str, b: &str) -> Option<bool> {
    match gitinfo::git_code(p, &["merge-base", "--is-ancestor", a, b], &[], gitinfo::TIMEOUT) {
        Ok((0, _, _)) => Some(true),
        Ok((1, _, _)) => Some(false),
        _ => None,
    }
}

/// "GitHub" for a github.com remote, and so on; the URL is read locally (`ls-remote --get-url`).
fn host(p: &Path, remote: &str) -> String {
    host_of_url(&gitinfo::git(p, &["ls-remote", "--get-url", remote]).unwrap_or_default())
}

/// A full commit id: 40 hex characters (SHA-1) or 64 (SHA-256).
fn is_sha(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// "GitHub" for a github.com URL, "GitLab", "Codeberg", else "the remote".
pub fn host_of_url(url: &str) -> String {
    let url = url.to_lowercase();
    if url.contains("github.com") {
        "GitHub".into()
    } else if url.contains("gitlab") {
        "GitLab".into()
    } else if url.contains("codeberg.org") {
        "Codeberg".into()
    } else {
        "the remote".into()
    }
}

/// Why a remote couldn't be asked, in a few quiet words.
pub fn problem_words(err: &str) -> String {
    let e = err.to_lowercase();
    if e.contains("timed out") || e.contains("could not resolve") || e.contains("unreachable") || e.contains("connection refused") || e.contains("no route") || e.contains("temporary failure") {
        "offline".into()
    } else if e.contains("permission denied")
        || e.contains("authentication")
        || e.contains("could not read username")
        || e.contains("could not read password")
        || e.contains("host key verification")
        || e.contains("terminal prompts disabled")
    {
        "not signed in".into()
    } else if e.contains("does not appear to be a git repository") || e.contains("not found") || e.contains("not exist") {
        "the remote isn't there".into()
    } else {
        "the remote didn't answer".into()
    }
}

/// HEAD against what the remote's branch points at (`remote_sha`, None when it couldn't be asked).
fn compare(p: &Path, t: &Tracking, remote_sha: Option<&str>) -> Option<Outcome> {
    let head = rev(p, "HEAD")?;
    let up = rev(p, &t.local_ref);
    let local = |up: &str| (count(p, &format!("HEAD..{up}")).unwrap_or(0), count(p, &format!("{up}..HEAD")).unwrap_or(0));
    let Some(sha) = remote_sha else {
        // Only the last fetch to go on.
        let (behind, ahead) = up.as_deref().map_or((0, 0), local);
        return Some(if behind > 0 { Outcome::Behind { count: Some(behind), ahead } } else { Outcome::InStep { ahead } });
    };
    if up.as_deref() == Some(sha) || has_commit(p, sha) {
        if sha == head || is_ancestor(p, sha, &head) == Some(true) {
            return Some(Outcome::InStep { ahead: count(p, &format!("{sha}..HEAD")).unwrap_or(0) });
        }
        let (behind, ahead) = local(sha);
        return Some(Outcome::Behind { count: Some(behind.max(1)), ahead });
    }
    // The remote's commit isn't here, so HEAD can't contain it: new commits, not fetched yet.
    let ahead = up.as_deref().map_or(0, |u| local(u).1);
    Some(Outcome::Behind { count: None, ahead })
}

/// Ask the remote, then compare. Blocks up to [`NET_TIMEOUT`] plus a few local git commands.
pub fn check_repo(path: &str) -> RepoCheck {
    let p = Path::new(path);
    let mut c = RepoCheck { path: path.into(), upstream: String::new(), host: "the remote".into(), outcome: Outcome::NoUpstream, remote_sha: None, problem: None, asked_at: None, at: Instant::now() };
    let Some(t) = tracking(p) else { return c };
    c.upstream.clone_from(&t.short);
    let url = gitinfo::git(p, &["ls-remote", "--get-url", &t.remote]).unwrap_or_default().trim().to_string();
    c.host = host_of_url(&url);
    // Asked by address from outside the repo, so nothing in the repo's own config (a credential
    // helper, an upload-pack or proxy command, put there by whoever made it) runs in the background.
    let url = if url.starts_with('.') { p.join(&url).to_string_lossy().into_owned() } else { url };
    let asked = if url.is_empty() { Err(GitError("no address for the remote".into())) } else { gitinfo::git_code(Path::new("/"), &["ls-remote", &url, &t.remote_ref], &NET_ENV, NET_TIMEOUT) };
    match asked {
        Ok((0, out, _)) => match out.lines().find_map(|l| l.split_once('\t').filter(|(_, r)| *r == t.remote_ref).map(|(s, _)| s.trim().to_string())) {
            Some(sha) if !is_sha(&sha) => c.problem = Some(format!("{} gave an answer Wake can't read", c.host)),
            Some(sha) => {
                c.remote_sha = Some(sha);
                c.asked_at = Some(util::now_iso());
            }
            None => c.problem = Some(format!("{} isn't on {} any more", t.remote_ref.trim_start_matches("refs/heads/"), c.host)),
        },
        Ok((_, _, err)) => c.problem = Some(problem_words(&err)),
        Err(GitError(e)) => c.problem = Some(problem_words(&e)),
    }
    c.outcome = match compare(p, &t, c.remote_sha.as_deref()) {
        Some(Outcome::InStep { .. }) if c.problem.is_some() => Outcome::CouldNotCheck(c.problem.clone().unwrap_or_default()),
        Some(o) => o,
        None => Outcome::CouldNotCheck("couldn't read this repo".into()),
    };
    c
}

/// The same check again without the network, against the commit the remote had last time: after
/// a pull or a commit here, the answer changes at once rather than in ten minutes.
pub fn recheck(prev: &RepoCheck) -> RepoCheck {
    let p = Path::new(&prev.path);
    let mut c = prev.clone();
    let Some(t) = tracking(p) else {
        c.outcome = Outcome::NoUpstream;
        return c;
    };
    c.upstream.clone_from(&t.short);
    // A fetch since (a pull, say) may have moved the tracking ref past what the remote said.
    let remote = match (&prev.remote_sha, rev(p, &t.local_ref)) {
        (Some(r), Some(up)) if r != &up && is_ancestor(p, r, &up) == Some(true) => Some(up),
        (r, _) => r.clone(),
    };
    c.outcome = match compare(p, &t, remote.as_deref()) {
        Some(Outcome::InStep { .. }) if c.problem.is_some() => Outcome::CouldNotCheck(c.problem.clone().unwrap_or_default()),
        Some(o) => o,
        None => Outcome::CouldNotCheck("couldn't read this repo".into()),
    };
    c
}

/// Files changed or new in the working tree (tracked changes plus untracked files).
pub fn dirty_files(path: &str) -> Option<u32> {
    let st = gitinfo::git(Path::new(path), &["status", "--porcelain=v1", "-b"]).ok()?;
    Some(st.lines().filter(|l| !l.starts_with("## ") && !l.trim().is_empty()).count() as u32)
}

/// The one-line message, in plain words, and whether "Pull now" may be offered.
pub fn words(c: &RepoCheck, dirty: u32, repo_label: Option<&str>) -> Option<(String, bool)> {
    let Outcome::Behind { count, ahead } = c.outcome else { return None };
    let commits = match count {
        Some(n) => util_pl(n, "commit", "commits"),
        None => "new commits".into(),
    };
    let pushed = format!("Someone pushed {commits} to {} that you don't have yet", c.host);
    let pushed = match repo_label {
        Some(name) => format!("In {name}: {}{}", pushed[..1].to_lowercase(), &pushed[1..]),
        None => pushed,
    };
    let can_pull = ahead == 0 && dirty == 0;
    let mut line = if ahead > 0 {
        format!(
            "{pushed}, and {} here {} pushed. Wake can't pull this safely: ask your agent to merge them, or run `git pull` in a terminal.",
            util_pl(ahead, "commit", "commits"),
            if ahead == 1 { "isn't" } else { "aren't" }
        )
    } else {
        format!("{pushed}. Pull them before you start.")
    };
    if dirty > 0 && ahead == 0 {
        line.push_str(&format!(" {} changes that aren't committed yet: commit them first, then pull.", if dirty == 1 { "1 file has".to_string() } else { format!("{dirty} files have") }));
    }
    Some((line, can_pull))
}

/// A failed `git pull --ff-only` in plain words, with what to do next. `host` is "GitHub", "the
/// remote" and so on.
pub fn pull_failure_words(git_said: &str, host: &str) -> String {
    let e = git_said.to_lowercase();
    if e.contains("not possible to fast-forward") || e.contains("diverg") || e.contains("non-fast-forward") {
        return format!("Your copy and {host} both have new commits, so Wake won't pull. Ask your agent to merge them, or run `git pull` in a terminal.");
    }
    if e.contains("would be overwritten") || e.contains("commit your changes or stash them") {
        return "The pull would change files you have changed and not committed, so git stopped. Commit them first, then pull.".into();
    }
    if e.contains("timed out") && !e.contains("connect") {
        return format!("{} took too long to answer, so the pull stopped. Try again later.", capital(host));
    }
    match problem_words(git_said).as_str() {
        "offline" => format!("Wake couldn't reach {host}. Check you're online, then try again."),
        "not signed in" => format!("{} didn't let Wake in. Run `git pull` in a terminal once, to sign in.", capital(host)),
        "the remote isn't there" => format!("{} says this repository isn't there any more. Check its address with `git remote -v` in a terminal.", capital(host)),
        _ => "The pull didn't work.".into(),
    }
}

fn capital(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

fn util_pl(n: u32, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Build a project's [`Behind`] from its repos' checks (None when none is behind). `dirty` gives
/// each repo's changed-or-new file count.
pub fn behind_for(project: &str, name: &str, roots: &[String], checks: &[&RepoCheck], dirty: &dyn Fn(&str) -> u32) -> Option<Behind> {
    let mut behind: Vec<&&RepoCheck> = checks.iter().filter(|c| c.is_behind()).collect();
    // Unknown counts ("not fetched yet") first, then the most behind.
    behind.sort_by_key(|c| match c.outcome {
        Outcome::Behind { count: None, .. } => 0,
        Outcome::Behind { count: Some(n), .. } => u32::MAX - n,
        _ => u32::MAX,
    });
    let c = behind.first()?;
    let d = dirty(&c.path);
    // Name the repo only when the project has more than this one folder.
    let label = (!roots.iter().any(|r| r == &c.path)).then(|| Path::new(&c.path).file_name().map(|n| n.to_string_lossy().into_owned())).flatten();
    let (line, can_pull) = words(c, d, label.as_deref())?;
    let Outcome::Behind { count, ahead } = c.outcome else { return None };
    Some(Behind {
        project: project.into(),
        project_name: name.into(),
        repo: c.path.clone(),
        upstream: c.upstream.clone(),
        host: c.host.clone(),
        count,
        ahead,
        dirty: d,
        can_pull,
        fix: if ahead > 0 { "git pull".into() } else { PULL.into() },
        line,
        checked_at: c.asked_at.clone(),
        others: behind.len() as u32 - 1,
        alarm: None,
    })
}

/// "How far behind" in a few words, for the banner and the notification.
pub fn how_far(b: &Behind) -> String {
    match b.count {
        Some(n) => format!("{} behind {}", util_pl(n, "commit", "commits"), b.host),
        None => format!("behind: {} has new commits", b.host),
    }
}

// ---------- pulling: the one git write, after the person confirms ----------

/// `git pull --ff-only` in `path`, only when its tree is clean and its branch tracks a remote.
/// Dry run returns the step without running it. Never call this without the person's confirmation.
pub fn pull_ff_only(path: &str, dry_run: bool) -> ActionResult {
    let p = Path::new(path);
    let step = format!("git -C {} pull --ff-only", util::shell_quote(path));
    let fail = |m: &str| ActionResult { ok: false, message: m.into(), steps: vec![step.clone()] };
    let Some(t) = tracking(p) else {
        return fail("This branch doesn't track a remote branch, so there is nothing to pull.");
    };
    match dirty_files(path) {
        Some(0) => {}
        Some(n) => {
            return fail(&format!(
                "{} changes that aren't committed yet. Commit them first; Wake only pulls when everything is committed.",
                if n == 1 { "1 file has".to_string() } else { format!("{n} files have") }
            ));
        }
        None => return fail("Couldn't read this repo."),
    }
    if dry_run {
        return ActionResult { ok: true, message: "Dry run.".into(), steps: vec![step] };
    }
    let mut cmd = gitinfo::command(p);
    cmd.args(["pull", "--ff-only"]).envs(NET_ENV.iter().copied());
    let (ok, out) = match crate::run::run_with_timeout(cmd, PULL_TIMEOUT) {
        Ok((code, out, err)) => (code == 0, format!("{out}\n{err}")),
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => (false, "timed out".into()),
        Err(e) => (false, e.to_string()),
    };
    let last = out.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("").to_string();
    let last = util::take_chars(&last, 200);
    let message = if ok {
        format!("Pulled. {last}")
    } else {
        // Plain words first, then git's own line for anyone who wants it.
        let said = if last.is_empty() { "Git gave no reason.".to_string() } else { format!("Git said: {last}") };
        format!("{} {said}", pull_failure_words(&out, &host(p, &t.remote)))
    };
    ActionResult { ok, message, steps: vec![step] }
}

// ---------- noticing when building starts while behind ----------

/// What the person is told when they start building while behind.
#[derive(Debug, Clone, PartialEq)]
pub struct Notice {
    pub project: String,
    pub summary: String,
    pub body: String,
}

/// Sends a desktop notification. Tests and development use a fake that records.
pub type Notifier = Arc<dyn Fn(&Notice) + Send + Sync>;

/// The notice for a project that is behind.
pub fn notice(b: &Behind) -> Notice {
    let next = if b.can_pull { "Open Wake and press Pull before you build." } else { "Ask your agent to pull them in before you build." };
    Notice { project: b.project.clone(), summary: format!("{} is behind: pull before you build", b.project_name), body: format!("{} is {}. {next}", b.project_name, how_far(b)) }
}

/// The `gdbus` call for a critical notification from "Wake".
pub fn notify_argv(n: &Notice) -> Vec<String> {
    [
        crate::run::GDBUS,
        "call",
        "--session",
        "--dest",
        "org.freedesktop.Notifications",
        "--object-path",
        "/org/freedesktop/Notifications",
        "--method",
        "org.freedesktop.Notifications.Notify",
        "'Wake'",
        "0",
        "'dialog-warning'",
    ]
    .iter()
    .map(|s| s.to_string())
    // The body may be read as markup (the summary never is): a project named "R&D" stays as written.
    .chain([
        util::gvariant_str_quote(&n.summary),
        util::gvariant_str_quote(&n.body.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")),
        "@as []".into(),
        "{'urgency': <byte 2>}".into(),
        "0".into(),
    ])
    .collect()
}

/// The real sender, through `run`. Sends nothing when WAKE_NO_NOTIFY is set or on GTK's Broadway
/// backend (screenshots): only a person at the desktop should ever get one.
pub fn runner_notifier(run: Runner) -> Notifier {
    Arc::new(move |n: &Notice| {
        if std::env::var_os("WAKE_NO_NOTIFY").is_some() || std::env::var("GDK_BACKEND").is_ok_and(|b| b == "broadway") {
            return;
        }
        let _ = run(&notify_argv(n), Duration::from_secs(3));
    })
}

#[derive(Default)]
struct Seen {
    /// Sessions working at the last look; None until this behind-episode's first look.
    working: Option<HashSet<String>>,
    dirty: Option<bool>,
    notified: bool,
    acked: bool,
    alarm: Option<String>,
}

/// One behind-episode per project: from the moment it is seen behind until it isn't. Within an
/// episode: the first time building starts, an alarm and one notification; after the person
/// acknowledges, nothing more until the next episode.
#[derive(Default)]
pub struct Watch {
    seen: HashMap<String, Seen>,
}

impl Watch {
    /// Look at a project now. `working`: its sessions working now; `dirty`: whether its tree has
    /// changes (None when unknown). Returns why, when a notification should go out now.
    pub fn observe(&mut self, project: &str, behind: bool, working: &[String], dirty: Option<bool>) -> Option<String> {
        if !behind {
            // Caught up (or never behind): the episode is over, and the next one starts afresh.
            self.seen.remove(project);
            return None;
        }
        let s = self.seen.entry(project.to_string()).or_default();
        let now: HashSet<String> = working.iter().cloned().collect();
        // A session already working when the project is first seen behind is building on old
        // code too, so the episode's first look counts it as well as a newly started one.
        let started = match &s.working {
            None => !now.is_empty(),
            Some(prev) => now.iter().any(|w| !prev.contains(w)),
        };
        let dirtied = s.dirty == Some(false) && dirty == Some(true);
        s.working = Some(now);
        if dirty.is_some() {
            s.dirty = dirty;
        }
        if s.acked || !(started || dirtied) {
            return None;
        }
        let why = if started { "a session started working" } else { "files changed" };
        s.alarm = Some(why.to_string());
        if s.notified {
            return None;
        }
        s.notified = true;
        Some(why.to_string())
    }

    /// The person has seen the alarm: quiet until this project catches up and falls behind again.
    pub fn ack(&mut self, project: &str) {
        if let Some(s) = self.seen.get_mut(project) {
            s.acked = true;
            s.alarm = None;
        }
    }

    pub fn alarm(&self, project: &str) -> Option<String> {
        self.seen.get(project).and_then(|s| s.alarm.clone())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::testing::{Scratch, scratch};
    use std::fs;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::sync::Mutex;

    /// Test setup only: git that writes, in throwaway repos.
    pub fn g(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    pub fn commit(dir: &Path, file: &str, text: &str) {
        fs::write(dir.join(file), text).unwrap();
        g(dir, &["add", file]);
        g(dir, &["commit", "-q", "-m", &format!("change {file}")]);
    }

    /// A bare "remote", a clone `here` (the laptop) and a clone `other` (someone else).
    pub struct Trio {
        pub s: Scratch,
        pub remote: PathBuf,
        pub here: PathBuf,
        pub other: PathBuf,
    }

    pub fn trio() -> Trio {
        let s = scratch("behind");
        let remote = s.0.join("remote.git");
        let here = s.0.join("here");
        let other = s.0.join("other");
        g(&s.0, &["init", "-q", "--bare", "-b", "main", remote.to_str().unwrap()]);
        g(&s.0, &["clone", "-q", remote.to_str().unwrap(), here.to_str().unwrap()]);
        g(&here, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        commit(&here, "a.txt", "a\n");
        g(&here, &["push", "-q", "-u", "origin", "main"]);
        g(&s.0, &["clone", "-q", remote.to_str().unwrap(), other.to_str().unwrap()]);
        Trio { s, remote, here, other }
    }

    fn snapshot(git_dir: &Path) -> Vec<(PathBuf, u64, std::time::SystemTime)> {
        let mut out = Vec::new();
        let mut stack = vec![git_dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in fs::read_dir(&d).unwrap().flatten() {
                let m = e.metadata().unwrap();
                if m.is_dir() {
                    stack.push(e.path());
                } else {
                    out.push((e.path(), m.len(), m.modified().unwrap()));
                }
            }
        }
        out.sort();
        out
    }

    fn s(p: &Path) -> String {
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn in_step_after_clone() {
        let t = trio();
        let c = check_repo(&s(&t.here));
        assert_eq!(c.outcome, Outcome::InStep { ahead: 0 });
        assert_eq!(c.upstream, "origin/main");
        assert!(c.asked_at.is_some());
        assert_eq!(c.host, "the remote");
        // A command in the repo's own config never runs for the background check.
        let mark = t.s.0.join("ran");
        g(&t.here, &["config", "remote.origin.uploadpack", &format!("touch {}; git-upload-pack", mark.display())]);
        assert_eq!(check_repo(&s(&t.here)).outcome, Outcome::InStep { ahead: 0 });
        assert!(!mark.exists());
    }

    #[test]
    fn behind_with_the_commits_only_on_the_remote_and_nothing_written() {
        let t = trio();
        commit(&t.other, "b.txt", "b\n");
        commit(&t.other, "c.txt", "c\n");
        g(&t.other, &["push", "-q"]);
        let before = snapshot(&t.here.join(".git"));
        let c = check_repo(&s(&t.here));
        assert_eq!(c.outcome, Outcome::Behind { count: None, ahead: 0 });
        // ls-remote, not fetch: not one file in .git changed (no refs, no FETCH_HEAD, no objects).
        assert_eq!(before, snapshot(&t.here.join(".git")));
        let (line, can_pull) = words(&c, 0, None).unwrap();
        assert_eq!(line, "Someone pushed new commits to the remote that you don't have yet. Pull them before you start.");
        assert!(can_pull);
    }

    #[test]
    fn behind_with_the_commits_fetched_counts_them() {
        let t = trio();
        commit(&t.other, "b.txt", "b\n");
        commit(&t.other, "c.txt", "c\n");
        commit(&t.other, "d.txt", "d\n");
        g(&t.other, &["push", "-q"]);
        g(&t.here, &["fetch", "-q"]); // setup: the person fetched earlier
        let c = check_repo(&s(&t.here));
        assert_eq!(c.outcome, Outcome::Behind { count: Some(3), ahead: 0 });
        assert_eq!(words(&c, 0, None).unwrap().0, "Someone pushed 3 commits to the remote that you don't have yet. Pull them before you start.");
    }

    #[test]
    fn behind_with_objects_local_but_tracking_ref_old() {
        // The remote's commit is here (fetched into another ref) but origin/main is old.
        let t = trio();
        commit(&t.other, "b.txt", "b\n");
        g(&t.other, &["push", "-q"]);
        let sha = g(&t.other, &["rev-parse", "HEAD"]).trim().to_string();
        // By URL, so git doesn't also update origin/main as it would fetching from "origin".
        g(&t.here, &["fetch", "-q", t.remote.to_str().unwrap(), "main:refs/tmp/x"]);
        assert_ne!(rev(&t.here, "origin/main").unwrap(), sha);
        let c = check_repo(&s(&t.here));
        assert_eq!(c.outcome, Outcome::Behind { count: Some(1), ahead: 0 });
    }

    #[test]
    fn ahead_is_not_behind() {
        let t = trio();
        commit(&t.here, "mine.txt", "m\n");
        let c = check_repo(&s(&t.here));
        assert_eq!(c.outcome, Outcome::InStep { ahead: 1 });
        assert!(words(&c, 0, None).is_none());
    }

    #[test]
    fn diverged_is_behind_but_cannot_fast_forward() {
        let t = trio();
        commit(&t.other, "b.txt", "b\n");
        g(&t.other, &["push", "-q"]);
        commit(&t.here, "mine.txt", "m\n");
        g(&t.here, &["fetch", "-q"]);
        let c = check_repo(&s(&t.here));
        assert_eq!(c.outcome, Outcome::Behind { count: Some(1), ahead: 1 });
        let (line, can_pull) = words(&c, 0, None).unwrap();
        assert!(!can_pull);
        assert!(line.contains("1 commit here isn't pushed"), "{line}");
        assert!(line.ends_with("ask your agent to merge them, or run `git pull` in a terminal."), "{line}");
        // pulling anyway is refused by git, and said plainly with git's line after it
        let r = pull_ff_only(&s(&t.here), false);
        assert!(!r.ok);
        assert!(r.message.starts_with("Your copy and the remote both have new commits, so Wake won't pull."), "{}", r.message);
        assert!(r.message.contains("Git said: "), "{}", r.message);
    }

    #[test]
    fn offline_remote_is_quiet_unless_the_last_fetch_says_behind() {
        let t = trio();
        let gone = t.s.0.join("moved-away.git");
        fs::rename(&t.remote, &gone).unwrap();
        let c = check_repo(&s(&t.here));
        assert!(matches!(c.outcome, Outcome::CouldNotCheck(_)), "{:?}", c.outcome);
        assert!(c.problem.is_some());
        assert!(words(&c, 0, None).is_none());
        // Back, with a commit fetched but not pulled; then offline again: the last fetch still counts.
        fs::rename(&gone, &t.remote).unwrap();
        commit(&t.other, "b.txt", "b\n");
        g(&t.other, &["push", "-q"]);
        g(&t.here, &["fetch", "-q"]);
        fs::rename(&t.remote, &gone).unwrap();
        let c = check_repo(&s(&t.here));
        assert_eq!(c.outcome, Outcome::Behind { count: Some(1), ahead: 0 });
        assert!(c.asked_at.is_none());
    }

    #[test]
    fn dirty_tree_is_said_and_blocks_pull() {
        let t = trio();
        commit(&t.other, "b.txt", "b\n");
        g(&t.other, &["push", "-q"]);
        fs::write(t.here.join("a.txt"), "edited\n").unwrap();
        let c = check_repo(&s(&t.here));
        let d = dirty_files(&s(&t.here)).unwrap();
        assert_eq!(d, 1);
        let (line, can_pull) = words(&c, d, None).unwrap();
        assert!(!can_pull);
        assert!(line.ends_with("1 file has changes that aren't committed yet: commit them first, then pull."), "{line}");
        let r = pull_ff_only(&s(&t.here), false);
        assert!(!r.ok);
        assert!(r.message.contains("Commit them first"), "{}", r.message);
        for line in [line, r.message] {
            assert!(!line.contains("stash") && !line.contains("--ff-only") && !line.contains("origin/"), "{line}");
        }
    }

    #[test]
    fn pull_fast_forwards_a_clean_scratch_repo_and_recheck_sees_it() {
        let t = trio();
        commit(&t.other, "b.txt", "b\n");
        g(&t.other, &["push", "-q"]);
        let c = check_repo(&s(&t.here));
        assert!(c.is_behind());
        let dry = pull_ff_only(&s(&t.here), true);
        assert!(dry.ok && dry.steps[0].ends_with("pull --ff-only"), "{dry:?}");
        assert!(!t.here.join("b.txt").exists(), "a dry run pulls nothing");
        let r = pull_ff_only(&s(&t.here), false);
        assert!(r.ok, "{r:?}");
        assert!(t.here.join("b.txt").exists());
        assert_eq!(recheck(&c).outcome, Outcome::InStep { ahead: 0 });
    }

    #[test]
    fn recheck_without_network_notices_a_pull_done_elsewhere() {
        let t = trio();
        commit(&t.other, "b.txt", "b\n");
        g(&t.other, &["push", "-q"]);
        let c = check_repo(&s(&t.here));
        assert!(c.is_behind());
        g(&t.here, &["pull", "-q", "--ff-only"]); // the person pulls in a terminal
        fs::rename(&t.remote, t.s.0.join("away.git")).unwrap(); // and recheck must not need the remote
        assert_eq!(recheck(&c).outcome, Outcome::InStep { ahead: 0 });
    }

    #[test]
    fn no_upstream_and_detached() {
        let t = trio();
        g(&t.here, &["checkout", "-q", "-b", "local-only"]);
        assert_eq!(check_repo(&s(&t.here)).outcome, Outcome::NoUpstream);
        g(&t.here, &["checkout", "-q", "--detach"]);
        assert_eq!(check_repo(&s(&t.here)).outcome, Outcome::NoUpstream);
        assert!(!pull_ff_only(&s(&t.here), true).ok);
    }

    #[test]
    fn behind_for_picks_the_unfetched_one_and_names_a_sub_repo() {
        let mk = |path: &str, count: Option<u32>| RepoCheck {
            path: path.into(),
            upstream: "origin/main".into(),
            host: "GitHub".into(),
            outcome: Outcome::Behind { count, ahead: 0 },
            remote_sha: None,
            problem: None,
            asked_at: Some("2026-11-20T10:00:00Z".into()),
            at: Instant::now(),
        };
        let a = mk("/p/game", Some(2));
        let b = mk("/p/site", None);
        let b2 = behind_for("pb", "Skyhook", &["/p".into()], &[&a, &b], &|_| 0).unwrap();
        assert_eq!(b2.repo, "/p/site");
        assert_eq!(b2.others, 1);
        assert_eq!(b2.line, "In site: someone pushed new commits to GitHub that you don't have yet. Pull them before you start.");
        let one = behind_for("g", "Game", &["/p/game".into()], &[&a], &|_| 0).unwrap();
        assert_eq!(one.line, "Someone pushed 2 commits to GitHub that you don't have yet. Pull them before you start.");
        assert_eq!(notice(&one).body, "Game is 2 commits behind GitHub. Open Wake and press Pull before you build.");
    }

    #[test]
    fn problems_in_plain_words() {
        assert_eq!(problem_words("ssh: Could not resolve hostname github.com"), "offline");
        assert_eq!(problem_words("timed out"), "offline");
        assert_eq!(problem_words("git@github.com: Permission denied (publickey)."), "not signed in");
        assert_eq!(problem_words("fatal: could not read Username for 'https://github.com': terminal prompts disabled"), "not signed in");
        let diverged = "hint: Diverging branches can't be fast-forwarded, you need to either:\nfatal: Not possible to fast-forward, aborting.";
        assert_eq!(pull_failure_words(diverged, "GitHub"), "Your copy and GitHub both have new commits, so Wake won't pull. Ask your agent to merge them, or run `git pull` in a terminal.");
        assert_eq!(pull_failure_words("ssh: Could not resolve hostname github.com", "GitHub"), "Wake couldn't reach GitHub. Check you're online, then try again.");
        assert_eq!(pull_failure_words("git@github.com: Permission denied (publickey).", "GitHub"), "GitHub didn't let Wake in. Run `git pull` in a terminal once, to sign in.");
        assert_eq!(pull_failure_words("odd", "the remote"), "The pull didn't work.");
    }

    #[test]
    fn notify_call_is_critical_and_from_wake() {
        let n = Notice { project: "p".into(), summary: "It's behind".into(), body: "a\\b".into() };
        let a = notify_argv(&n);
        assert_eq!(a[0], "gdbus");
        assert_eq!(a[9], "'Wake'");
        assert_eq!(a[12], "'It\\'s behind'");
        assert_eq!(a[13], "'a\\\\b'");
        assert_eq!(a[15], "{'urgency': <byte 2>}");
        let a = notify_argv(&Notice { project: "p".into(), summary: "R&D".into(), body: "R&D <b>".into() });
        assert_eq!((a[12].as_str(), a[13].as_str()), ("'R&D'", "'R&amp;D &lt;b&gt;'"));
    }

    #[test]
    fn only_a_full_commit_id_counts() {
        assert!(is_sha(&"a".repeat(40)) && is_sha(&"0".repeat(64)));
        for bad in ["", "abc", &"g".repeat(40), &"a".repeat(41), "--upload-pack=x"] {
            assert!(!is_sha(bad), "{bad}");
        }
    }

    #[test]
    fn starting_to_build_while_behind_alarms_once_per_episode() {
        let mut w = Watch::default();
        let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // Not behind: nothing, whatever happens.
        assert_eq!(w.observe("p", false, &ids(&[]), Some(false)), None);
        assert_eq!(w.observe("p", false, &ids(&["s1"]), Some(true)), None);
        // Falls behind while s1 is already working: it is building on old code, so alarm at once.
        assert_eq!(w.observe("p", true, &ids(&["s1"]), Some(false)).as_deref(), Some("a session started working"));
        assert!(w.alarm("p").is_some());
        assert_eq!(w.observe("p", true, &ids(&["s1"]), Some(false)), None);
        // s2 goes from idle to working in the same episode: the alarm stays, no second notification.
        assert_eq!(w.observe("p", true, &ids(&["s1", "s2"]), Some(false)), None);
        assert!(w.alarm("p").is_some());
        // More building in the same episode: the alarm stays, no second notification.
        assert_eq!(w.observe("p", true, &ids(&["s1", "s2"]), Some(true)), None);
        assert_eq!(w.observe("p", true, &ids(&["s3"]), Some(true)), None);
        assert!(w.alarm("p").is_some());
        // Acknowledged: quiet for the rest of the episode.
        w.ack("p");
        assert_eq!(w.alarm("p"), None);
        assert_eq!(w.observe("p", true, &ids(&["s4"]), Some(true)), None);
        assert_eq!(w.alarm("p"), None);
        // Catches up, falls behind again: a new episode, a new notification on the next start.
        assert_eq!(w.observe("p", false, &ids(&[]), Some(false)), None);
        assert_eq!(w.observe("p", true, &ids(&[]), Some(false)), None);
        assert_eq!(w.observe("p", true, &ids(&[]), Some(true)).as_deref(), Some("files changed"));
    }

    #[test]
    fn clean_to_dirty_is_an_edge_but_unknown_is_not() {
        let mut w = Watch::default();
        assert_eq!(w.observe("p", true, &[], None), None);
        assert_eq!(w.observe("p", true, &[], Some(true)), None, "unknown -> dirty is not clean -> dirty");
        assert_eq!(w.observe("p", true, &[], Some(false)), None);
        assert_eq!(w.observe("p", true, &[], None), None);
        assert_eq!(w.observe("p", true, &[], Some(true)).as_deref(), Some("files changed"));
    }

    #[test]
    fn the_fake_notifier_records_and_the_real_one_is_never_used_in_tests() {
        let sent: Arc<Mutex<Vec<Notice>>> = Arc::default();
        let s2 = sent.clone();
        let fake: Notifier = Arc::new(move |n: &Notice| s2.lock().unwrap().push(n.clone()));
        fake(&Notice { project: "p".into(), summary: "s".into(), body: "b".into() });
        assert_eq!(sent.lock().unwrap().len(), 1);
    }
}
