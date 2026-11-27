//! The few libc calls the core makes, each wrapped as a safe function with its `unsafe` block
//! explained. Nothing else in the core calls libc functions directly.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::process::Command;

/// This process's real user id.
pub fn uid() -> u32 {
    // SAFETY: getuid(2) takes nothing, touches no memory of ours and cannot fail.
    unsafe { libc::getuid() }
}

/// Clock ticks per second (`_SC_CLK_TCK`), the unit of /proc's times; 100 when it can't be read.
pub fn clock_ticks() -> f64 {
    // SAFETY: sysconf(3) only reads a configuration value.
    let t = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if t > 0 { t as f64 } else { 100.0 }
}

/// Send `sig` to the process group `group` leads (kill(2) with the group id negated). Which
/// processes that reaches is the caller's to make sure of: only a group whose leader it spawned
/// and has not reaped, so the id can't have been reused.
pub fn kill_group(group: u32, sig: i32) {
    // SAFETY: kill(2) takes two integers and touches no memory of ours.
    unsafe { libc::kill(-(group as libc::pid_t), sig) };
}

/// A pidfd for `pid` (pidfd_open(2)): while it is open, the pid can't be reused.
pub fn pidfd_open(pid: u32) -> io::Result<OwnedFd> {
    // SAFETY: the syscall takes two integers and returns a new descriptor or -1.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::c_int, 0 as libc::c_uint) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pidfd_open returned a new descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(raw as i32) })
}

/// Send `sig` to the process `pidfd` refers to (pidfd_send_signal(2)).
pub fn pidfd_send_signal(pidfd: &OwnedFd, sig: i32) -> io::Result<()> {
    // SAFETY: an open descriptor we own, a signal number, no siginfo (null is allowed) and no flags.
    let r = unsafe { libc::syscall(libc::SYS_pidfd_send_signal, pidfd.as_raw_fd(), sig, std::ptr::null::<libc::siginfo_t>(), 0 as libc::c_uint) };
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// The size of the terminal `fd` is open on (rows, columns), by TIOCGWINSZ; None if it isn't one.
pub fn window_size(fd: &impl AsRawFd) -> Option<(u16, u16)> {
    let mut ws = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: TIOCGWINSZ only writes a winsize into `ws`, which lives for the call.
    let r = unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCGWINSZ, &mut ws) };
    (r == 0).then_some((ws.ws_row, ws.ws_col))
}

/// Free and total bytes of the filesystem holding `path`.
pub fn statvfs(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs is plain old data, for which all zeros is a valid value.
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: a valid C string and an out-struct of the right type, both live for the call.
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let f = s.f_frsize as u64;
    Some((s.f_bavail as u64 * f, s.f_blocks as u64 * f))
}

/// Run the calling thread at the lowest CPU priority and the idle I/O class (`nice 19`,
/// `ionice -c 3`), so measuring never competes with a build or the desktop.
pub fn lower_priority() {
    // SAFETY: plain syscalls with integer arguments, on the calling thread only.
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        libc::setpriority(libc::PRIO_PROCESS, tid, 19);
        // IOPRIO_WHO_PROCESS = 1 (a thread id works), IOPRIO_CLASS_IDLE = 3 << 13.
        libc::syscall(libc::SYS_ioprio_set, 1, tid as libc::c_long, (3 << 13) as libc::c_long);
    }
}

/// Have the child `cmd` starts get SIGTERM when Wake ends (prctl PR_SET_PDEATHSIG).
pub fn end_with_parent(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: the closure runs in the child between fork and exec and calls only prctl(2), which
    // is async-signal-safe and allocates nothing.
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
}

/// `t` (seconds since the epoch) in local time, as libc breaks it down.
pub fn local_time(t: i64) -> libc::tm {
    let t = t as libc::time_t;
    // SAFETY: tm is plain old data, for which all zeros is a valid value.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: valid pointers to a time and an out-struct, both live for the call.
    unsafe { libc::localtime_r(&t, &mut tm) };
    tm
}
