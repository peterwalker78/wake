//! Read-only, lock-free git loose ends per repo.
//!
//! Every call is `git --no-optional-locks -c core.fsmonitor=false -C <dir> <subcommand>` with
//! GIT_OPTIONAL_LOCKS=0, and only these subcommands: rev-parse, status --porcelain=v1 -b,
//! rev-list --count, for-each-ref, worktree list --porcelain, log -n, plus (for "behind the
//! remote", see remote.rs) ls-remote, merge-base --is-ancestor and cat-file -e. Never fetch.

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::store::ProjectRec;
use crate::types::{GitTotals, RepoState};
use crate::util;

pub const TIMEOUT: Duration = Duration::from_secs(8);
pub const CACHE_SECONDS: f64 = 60.0;

#[derive(Debug, Clone, PartialEq)]
pub struct GitError(pub String);

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GitError {}

/// Refuse anything but the read-only subcommands Wake uses.
pub fn check_args(args: &[&str]) -> Result<(), GitError> {
    let first = args.first().copied().unwrap_or("");
    let ok = match first {
        "rev-parse" | "rev-list" | "for-each-ref" => true,
        "status" => args.get(1..3) == Some(&["--porcelain=v1", "-b"][..]),
        "worktree" => args[1..] == ["list", "--porcelain"],
        "log" => args.get(1).is_some_and(|a| a.starts_with("-n")),
        // Asks the remote what a branch points at; reads nothing and writes nothing locally. No
        // options but --get-url (so never --upload-pack), and no argument that looks like one.
        "ls-remote" => match args.get(1).copied() {
            Some("--get-url") => args.len() == 3 && !args[2].starts_with('-'),
            Some(_) => args.len() == 3 && args[1..].iter().all(|a| !a.starts_with('-')),
            None => false,
        },
        "merge-base" => args.len() == 4 && args[1] == "--is-ancestor" && args[2..].iter().all(|a| !a.starts_with('-')),
        "cat-file" => args.len() == 3 && args[1] == "-e" && !args[2].starts_with('-'),
        _ => false,
    };
    if ok { Ok(()) } else { Err(GitError(format!("git {} is not allowed in Wake", args.join(" ")))) }
}

/// Run one allowed git command, read-only, with a timeout.
pub fn git(repo: &Path, args: &[&str]) -> Result<String, GitError> {
    let (code, out, err) = git_code(repo, args, &[], TIMEOUT)?;
    if code != 0 {
        let e = util::take_chars(err.trim(), 300);
        return Err(GitError(if e.is_empty() { "git failed".into() } else { e }));
    }
    Ok(out)
}

/// Like [`git`], but hands back the exit code (for `merge-base --is-ancestor` and `cat-file -e`,
/// which answer with it) and takes extra environment and a timeout (for `ls-remote`).
pub fn git_code(repo: &Path, args: &[&str], env: &[(&str, &str)], timeout: Duration) -> Result<(i32, String, String), GitError> {
    check_args(args)?;
    let mut cmd = command(repo);
    cmd.args(args).envs(env.iter().copied());
    crate::run::run_with_timeout(cmd, timeout).map_err(|e| GitError(if e.kind() == std::io::ErrorKind::TimedOut { "timed out".into() } else { e.to_string() }))
}

/// `git --no-optional-locks -c core.fsmonitor=false -C <repo>` with a clean, prompt-free environment.
pub(crate) fn command(repo: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("--no-optional-locks").args(["-c", "core.fsmonitor=false", "-C"]).arg(repo);
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("GIT_") {
            cmd.env_remove(k);
        }
    }
    cmd.envs([("GIT_OPTIONAL_LOCKS", "0"), ("GIT_TERMINAL_PROMPT", "0"), ("LC_ALL", "C"), ("GIT_PAGER", "cat"), ("PAGER", "cat")]);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd
}

pub fn is_repo(p: &Path) -> bool {
    p.join(".git").exists()
}

/// A root that is a repo -> [root]; otherwise its child repos (one and two levels down).
pub fn repos_for_root(root: &Path) -> Vec<String> {
    if !root.is_dir() {
        return Vec::new();
    }
    if is_repo(root) {
        return vec![root.to_string_lossy().into_owned()];
    }
    let list = |p: &Path| -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(p).map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
        v.sort();
        v
    };
    let mut out = Vec::new();
    for k in list(root) {
        if k.starts_with('.') {
            continue;
        }
        let p = root.join(&k);
        if !p.is_dir() {
            continue;
        }
        if is_repo(&p) {
            out.push(p.to_string_lossy().into_owned());
            continue;
        }
        for g in list(&p) {
            let q = p.join(&g);
            if !g.starts_with('.') && q.is_dir() && is_repo(&q) {
                out.push(q.to_string_lossy().into_owned());
            }
        }
    }
    out
}

fn track(s: &str) -> (u32, u32, bool) {
    let (mut ahead, mut behind, mut gone) = (0, 0, false);
    for part in s.split(',') {
        let part = part.trim();
        if let Some(n) = part.strip_prefix("ahead ") {
            ahead = n.parse().unwrap_or(0);
        } else if let Some(n) = part.strip_prefix("behind ") {
            behind = n.parse().unwrap_or(0);
        } else if part == "gone" {
            gone = true;
        }
    }
    (ahead, behind, gone)
}

/// '## main...origin/main [ahead 1, behind 2]' -> (branch, upstream, ahead, behind).
pub fn parse_branch_line(ln: &str) -> (String, Option<String>, Option<u32>, Option<u32>) {
    let mut body = ln.get(3..).unwrap_or("").trim();
    if body.starts_with("HEAD (no branch)") {
        return ("(detached)".into(), None, None, None);
    }
    for pre in ["No commits yet on ", "Initial commit on "] {
        if let Some(b) = body.strip_prefix(pre) {
            body = b;
        }
    }
    let mut tr = "";
    if body.ends_with(']')
        && let Some(i) = body.rfind(" [")
    {
        tr = &body[i + 2..body.len() - 1];
        body = &body[..i];
    }
    let Some((branch, upstream)) = body.split_once("...") else {
        return (body.into(), None, None, None);
    };
    let (a, b, gone) = track(tr);
    if gone {
        return (branch.into(), None, None, None);
    }
    (branch.into(), Some(upstream.into()), Some(a), Some(b))
}

/// RepoState plus the ahead count across all branches (for totals).
#[derive(Debug, Clone)]
pub struct Repo {
    pub state: RepoState,
    pub ahead_all_branches: u32,
}

fn utc(iso: &str) -> String {
    match util::parse_iso(iso) {
        Some(t) => {
            let s = util::iso_from_epoch(t);
            format!("{}Z", &s[..19])
        }
        None => iso.into(),
    }
}

pub fn repo_state(path: &str) -> Result<Repo, GitError> {
    let p = Path::new(path);
    let st = git(p, &["status", "--porcelain=v1", "-b"])?;
    let (mut branch, mut upstream, mut ahead, mut behind) = (None, None, None, None);
    let (mut dirty, mut untracked) = (0, 0);
    for ln in st.lines() {
        if ln.starts_with("## ") {
            let (b, u, a, be) = parse_branch_line(ln);
            branch = Some(b);
            upstream = u;
            ahead = a;
            behind = be;
        } else if ln.starts_with("??") {
            untracked += 1;
        } else if !ln.trim().is_empty() {
            dirty += 1;
        }
    }
    let head = git(p, &["rev-parse", "--short", "HEAD"]).ok().map(|h| h.trim().to_string());
    let (mut branches, mut no_up, mut ahead_all) = (0, 0, 0);
    if let Ok(refs) = git(p, &["for-each-ref", "--format=%(refname:short)%09%(upstream:short)%09%(upstream:track)", "refs/heads"]) {
        for ln in refs.lines() {
            let parts: Vec<&str> = ln.split('\t').collect();
            if parts[0].is_empty() {
                continue;
            }
            branches += 1;
            let up = parts.get(1).copied().unwrap_or("");
            let tr = parts.get(2).copied().unwrap_or("");
            if up.is_empty() || tr.contains("gone") {
                no_up += 1;
            } else {
                ahead_all += track(tr.trim_matches(['[', ']'])).0;
            }
        }
    }
    let worktrees = git(p, &["worktree", "list", "--porcelain"]).map_or(1, |wt| wt.lines().filter(|l| l.starts_with("worktree ")).count() as u32);
    let (mut last_at, mut subject) = (None, None);
    if let Ok(lg) = git(p, &["log", "-n", "1", "--format=%cI%x09%s"]) {
        let lg = lg.trim();
        if !lg.is_empty() {
            let (a, s) = lg.split_once('\t').unwrap_or((lg, ""));
            last_at = Some(utc(a));
            subject = Some(util::take_chars(s, 200)).filter(|s| !s.is_empty());
        }
    }
    let remote_host = remote_url(p, upstream.as_deref()).map(|u| crate::remote::host_of_url(&u));
    Ok(Repo {
        state: RepoState {
            path: path.into(),
            branch,
            head,
            upstream,
            ahead,
            behind,
            dirty_files: dirty,
            untracked,
            branches_no_upstream: no_up,
            branches,
            worktrees,
            last_commit_at: last_at,
            last_commit_subject: subject,
            remote_host,
        },
        ahead_all_branches: ahead_all,
    })
}

/// The URL of the repo's remote, read locally (`ls-remote --get-url` answers with the name itself
/// when there is no such remote): the upstream's remote first, then `origin`, then any remote it
/// has branches from. None when it has none.
fn remote_url(p: &Path, upstream: Option<&str>) -> Option<String> {
    let mut names: Vec<String> = upstream.and_then(|u| u.split_once('/')).map(|(r, _)| r.to_string()).into_iter().collect();
    names.push("origin".into());
    if let Ok(refs) = git(p, &["for-each-ref", "--format=%(refname)", "refs/remotes"]) {
        names.extend(refs.lines().filter_map(|l| l.strip_prefix("refs/remotes/")?.split_once('/')).map(|(r, _)| r.to_string()));
    }
    let mut seen = std::collections::HashSet::new();
    names.into_iter().filter(|n| !n.is_empty() && !n.starts_with('-') && seen.insert(n.clone())).find_map(|n| {
        let url = git(p, &["ls-remote", "--get-url", &n]).ok()?.trim().to_string();
        (!url.is_empty() && url != n).then_some(url)
    })
}

/// Subjects of commits on HEAD after a time (log -n only).
pub fn commits_since(path: &str, since_iso: &str, limit: u32) -> Vec<String> {
    let n = limit.to_string();
    let since = format!("--since={since_iso}");
    git(Path::new(path), &["log", "-n", &n, &since, "--format=%s"]).map(|out| out.lines().filter(|l| !l.trim().is_empty()).map(String::from).collect()).unwrap_or_default()
}

pub fn git_totals(repos: &[Repo]) -> GitTotals {
    GitTotals {
        ahead_total: repos.iter().map(|r| r.ahead_all_branches).sum(),
        // Branches of a repo with no remote have nowhere to go: not a loose end.
        no_upstream: repos.iter().filter(|r| r.state.remote_host.is_some()).map(|r| r.state.branches_no_upstream).sum(),
        dirty_files: repos.iter().map(|r| r.state.dirty_files + r.state.untracked).sum(),
        no_remote: repos.iter().filter(|r| r.state.remote_host.is_none()).count() as u32,
        github: repos.iter().any(|r| r.state.remote_host.is_some()) && repos.iter().filter_map(|r| r.state.remote_host.as_deref()).all(|h| h == "GitHub"),
    }
}

type Key = (String, Vec<String>);
type Entry = (Instant, Vec<Repo>, Vec<String>);
static CACHE: OnceLock<Mutex<HashMap<Key, Entry>>> = OnceLock::new();

fn cache() -> &'static Mutex<HashMap<Key, Entry>> {
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// (repos, could-not-check) for a project, cached for a minute.
pub fn project_git(project: &ProjectRec, fresh: bool) -> (Vec<Repo>, Vec<String>) {
    let key = (project.id.clone(), project.roots.clone());
    if !fresh
        && let Some((at, r, f)) = cache().lock().unwrap_or_else(|e| e.into_inner()).get(&key)
        && at.elapsed().as_secs_f64() < CACHE_SECONDS
    {
        return (r.clone(), f.clone());
    }
    let (mut repos, mut failed) = (Vec::new(), Vec::new());
    for root in &project.roots {
        let rp = Path::new(root);
        if !rp.is_dir() {
            failed.push(format!("{root}: folder not found"));
            continue;
        }
        let found = repos_for_root(rp);
        if found.is_empty() {
            failed.push(format!("{root}: no git repository here"));
        }
        for r in found {
            match repo_state(&r) {
                Ok(s) => repos.push(s),
                Err(e) => failed.push(format!("{r}: {}", e.0)),
            }
        }
    }
    cache().lock().unwrap_or_else(|e| e.into_inner()).insert(key, (Instant::now(), repos.clone(), failed.clone()));
    (repos, failed)
}

/// A project's repos from the cache only (never blocks on git); None if not read yet.
pub fn cached_repos(project: &ProjectRec) -> Option<Vec<Repo>> {
    let key = (project.id.clone(), project.roots.clone());
    cache().lock().unwrap_or_else(|e| e.into_inner()).get(&key).map(|(_, r, _)| r.clone())
}

/// Totals from the cache only (never blocks on git); None if not computed yet.
pub fn cached_totals(project: &ProjectRec) -> Option<GitTotals> {
    let key = (project.id.clone(), project.roots.clone());
    cache().lock().unwrap_or_else(|e| e.into_inner()).get(&key).map(|(_, r, _)| git_totals(r))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Scratch, scratch};
    use std::fs;
    use std::path::PathBuf;

    /// Test setup only: builds a throwaway repo in a scratch folder (the one place git writes).
    fn make_repo() -> Scratch {
        let s = scratch("git");
        let (dir, g) = (&s.0, |args: &[&str]| crate::remote::tests::g(&s.0, args));
        g(&["init", "-q", "-b", "main"]);
        fs::write(dir.join("a.txt"), "a\n").unwrap();
        g(&["add", "a.txt"]);
        g(&["commit", "-q", "-m", "First commit"]);
        g(&["branch", "side"]);
        fs::write(dir.join("a.txt"), "a\nb\n").unwrap();
        fs::write(dir.join("new.txt"), "n\n").unwrap();
        s
    }

    fn snapshot(git_dir: &Path) -> HashMap<PathBuf, (i64, u64)> {
        use std::os::unix::fs::MetadataExt;
        let mut out = HashMap::new();
        let mut stack = vec![git_dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in fs::read_dir(&d).unwrap().flatten() {
                let p = e.path();
                if e.file_name() == "objects" {
                    continue;
                }
                if let Ok(m) = fs::symlink_metadata(&p) {
                    out.insert(p.clone(), (m.mtime_nsec() + m.mtime() * 1_000_000_000, m.len()));
                    if m.is_dir() {
                        stack.push(p);
                    }
                }
            }
        }
        out
    }

    #[test]
    fn repo_state_reads_a_repo() {
        let r = make_repo();
        let st = repo_state(&r.0.to_string_lossy()).unwrap().state;
        assert_eq!(st.branch.as_deref(), Some("main"));
        assert_eq!(st.upstream, None);
        assert_eq!(st.dirty_files, 1);
        assert_eq!(st.untracked, 1);
        assert_eq!(st.branches, 2);
        assert_eq!(st.branches_no_upstream, 2);
        assert_eq!(st.worktrees, 1);
        assert_eq!(st.last_commit_subject.as_deref(), Some("First commit"));
        assert!(st.last_commit_at.unwrap().ends_with('Z'));
        assert_eq!(st.remote_host, None);
        let t = git_totals(&[repo_state(&r.0.to_string_lossy()).unwrap()]);
        assert_eq!((t.no_remote, t.github, t.no_upstream), (1, false, 0));
        // Test setup only: give it a remote (nothing is fetched).
        let ok = Command::new("git").arg("-C").arg(&r.0).args(["remote", "add", "origin", "https://github.com/someone/thing.git"]).env("GIT_CONFIG_GLOBAL", "/dev/null").status().unwrap().success();
        assert!(ok);
        let rs = repo_state(&r.0.to_string_lossy()).unwrap();
        assert_eq!(rs.state.remote_host.as_deref(), Some("GitHub"));
        let t = git_totals(&[rs]);
        assert_eq!((t.no_remote, t.github, t.no_upstream), (0, true, 2));
    }

    #[test]
    fn read_only_on_fixture_repo() {
        let r = make_repo();
        let before = snapshot(&r.0.join(".git"));
        repo_state(&r.0.to_string_lossy()).unwrap();
        assert_eq!(before, snapshot(&r.0.join(".git")));
    }

    #[test]
    fn read_only_on_a_real_repo() {
        // Wake's own checkout: reading it must not touch its index or leave lock files.
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(repo) = fs::canonicalize(repo) else { return };
        if !repo.join(".git").is_dir() {
            return;
        }
        let index = repo.join(".git/index");
        let mt = |p: &Path| fs::metadata(p).ok().and_then(|m| m.modified().ok());
        let before = mt(&index);
        let snap = snapshot(&repo.join(".git"));
        repo_state(&repo.to_string_lossy()).unwrap();
        assert_eq!(before, mt(&index));
        let after = snapshot(&repo.join(".git"));
        let created: Vec<_> = after.keys().filter(|p| !snap.contains_key(*p)).filter(|p| p.to_string_lossy().ends_with(".lock")).collect();
        assert!(created.is_empty(), "{created:?}");
    }

    #[test]
    fn disallowed_commands() {
        for args in [
            &["fetch"][..],
            &["status"],
            &["worktree", "add", "x"],
            &["log"],
            &["commit", "-m", "x"],
            &["pull", "--ff-only"],
            &["ls-remote", "--upload-pack=touch /tmp/x", "origin"],
            &["ls-remote", "origin", "--upload-pack=x"],
            &["ls-remote", "origin"],
            &["merge-base", "a", "b"],
            &["cat-file", "-p", "HEAD"],
        ] {
            assert!(git(Path::new("/tmp"), args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn branch_line() {
        let p = parse_branch_line;
        assert_eq!(p("## main...origin/main [ahead 2, behind 1]"), ("main".into(), Some("origin/main".into()), Some(2), Some(1)));
        assert_eq!(p("## fix.v2...origin/fix.v2"), ("fix.v2".into(), Some("origin/fix.v2".into()), Some(0), Some(0)));
        assert_eq!(p("## feature"), ("feature".into(), None, None, None));
        assert_eq!(p("## No commits yet on main"), ("main".into(), None, None, None));
        assert_eq!(p("## HEAD (no branch)"), ("(detached)".into(), None, None, None));
        assert_eq!(p("## x...origin/x [gone]"), ("x".into(), None, None, None));
    }

    #[test]
    fn project_git_reports_missing() {
        let r = make_repo();
        let proj = ProjectRec {
            id: "t".into(),
            name: "t".into(),
            roots: vec![r.0.to_string_lossy().into_owned(), "/nonexistent/wake".into()],
            desc: String::new(),
            next: String::new(),
            why: String::new(),
            removed: false,
            added: String::new(),
            color: None,
            group: None,
            importance: None,
            extra: Default::default(),
        };
        let (repos, failed) = project_git(&proj, true);
        assert_eq!(repos.len(), 1);
        assert_eq!(failed.len(), 1);
        assert_eq!(git_totals(&repos).dirty_files, 2);
        assert_eq!(cached_totals(&proj).unwrap().dirty_files, 2);
    }
}
