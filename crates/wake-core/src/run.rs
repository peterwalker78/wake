//! Running other programs: git, gdbus, `claude logs`, a new terminal window. Always with no shell,
//! no input and a time limit, in a process group of their own so a hung one is ended with
//! everything it started. [`Runner`] is the injectable form the desktop parts take, so tests never
//! start anything.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Qt 6's qdbus as Wake writes it in steps; the real runner runs [`qdbus_program`] for it.
pub const QDBUS: &str = "qdbus-qt6";
pub const GDBUS: &str = "gdbus";
/// How long a desktop call (gdbus, qdbus, opening a window) may take.
pub const TIMEOUT: Duration = Duration::from_secs(2);

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

/// SIGTERM to the process group `child` leads (it must have been spawned with `process_group(0)`),
/// SIGKILL to it half a second later if the child is still there, then reap the child. The child
/// is not reaped until the end, so its pid, and so the group id, can't have been reused.
pub fn end_group(child: &mut std::process::Child) {
    crate::sys::kill_group(child.id(), libc::SIGTERM);
    for _ in 0..50 {
        if !matches!(child.try_wait(), Ok(None)) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    // try_wait just said the child is still there.
    crate::sys::kill_group(child.id(), libc::SIGKILL);
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

/// The first `name` on $PATH that is a file, as a full path.
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| p.is_file())
}

/// Qt 6's qdbus here: `qdbus-qt6` (Fedora, Arch), else `qdbus6` (Debian and Ubuntu name it so).
/// Never Qt 5's `qdbus`. Looked up on PATH once; `QDBUS` when neither is there.
pub fn qdbus_program() -> &'static str {
    static FOUND: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    FOUND.get_or_init(|| [QDBUS, "qdbus6"].into_iter().find(|n| which(n).is_some()).unwrap_or(QDBUS))
}

/// The real [`Runner`]: `setsid` launches detached, everything else through [`run_with_timeout`].
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::scratch;
    use crate::util::shell_quote;

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
        let mark = scratch("group");
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
}
