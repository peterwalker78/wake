//! Test helpers shared by the core's tests.

use std::fs;
use std::path::{Path, PathBuf};

/// A scratch folder for one test, removed when dropped. It lives under the build's target folder
/// (never /tmp, which may be RAM), canonical, so tests can compare paths with what they read back.
pub struct Scratch(pub PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A new, empty scratch folder named after `name`.
pub fn scratch(name: &str) -> Scratch {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let dir = base().join(format!("{name}-{}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    Scratch(fs::canonicalize(dir).unwrap())
}

/// `<target>/test-scratch`, where `<target>` holds this test binary (`<target>/<profile>/deps/…`),
/// so `CARGO_TARGET_DIR` is followed. Wake reads `.claude/worktrees/…` in a path as an agent's
/// worktree, so when the target folder is inside one (a checkout in an agent worktree), the
/// repository's own target folder above it is used instead: the folders the tests make must look
/// like anyone's.
fn base() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let target = exe.ancestors().nth(3).map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target"));
    let text = target.to_string_lossy();
    match text.find("/.claude/") {
        Some(i) => PathBuf::from(&text[..i]).join("target/test-scratch"),
        None => target.join("test-scratch"),
    }
}
