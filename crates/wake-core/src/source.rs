//! `Live`: the real [`Source`], reading this machine.
//!
//! Heavy work runs on Live's own threads once [`Live::start_background`] is called: indexing every
//! 10 s, where sessions are every 5 s, git totals every minute. `Source` methods answer from what is
//! ready. The command line uses `Live::new` without threads and calls `index_now` itself.
//!
//! Where each session is (which terminal window) is decided in one place,
//! [`Inner::refresh_locations`], from one scan of /proc: the terminal app holding the other end of
//! an agent's pty shows it, whichever app that is. Every view and the Agents rows read that one
//! map; "go to" and hold-to-peek work it out afresh for their one session.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use crate::desktop::{self, Desktop, Placee, Runner};
use crate::index::{self, Acc, Indexer};
use crate::live::{self, LiveInfo};
use crate::look::{self, Look, ScreenCache};
use crate::procs::{self, Actor, Explorer, Inputs, RealActor, SessionView};
use crate::remote::{self, Notifier, RepoCheck, Watch};
use crate::store::{self, IdeaRec, Paths, PauseRec, ProjectRec, Store};
use crate::types::*;
use crate::util;
use crate::{Source, brief, gitinfo};
use crate::{attrib, disk, log};

pub const RECENT_DAYS: f64 = 14.0;
const INDEX_EVERY: Duration = Duration::from_secs(10);
const SIDE_EVERY: Duration = Duration::from_secs(5);
const CARD_EVERY: f64 = 15.0;

/// How a `Live` reaches the machine. Tests swap in fakes.
pub struct LiveConfig {
    pub paths: Paths,
    pub proc_root: PathBuf,
    /// Runs gdbus (and qdbus for KWin) for the desktop part, and `claude logs`.
    pub runner: Runner,
    /// Path to systemd-run, Some("") for none, None to look it up.
    pub systemd_run: Option<String>,
    pub actor: Arc<dyn Actor>,
    /// How new windows open in the default terminal app (`desktop::terminal_launcher`); None
    /// when the system names none.
    pub terminal: Option<Vec<String>>,
}

impl LiveConfig {
    pub fn real() -> LiveConfig {
        LiveConfig {
            paths: Paths::from_env(),
            proc_root: PathBuf::from("/proc"),
            runner: desktop::default_runner(),
            systemd_run: None,
            actor: Arc::new(RealActor { root: PathBuf::from("/proc") }),
            terminal: desktop::terminal_launcher(),
        }
    }
}

type LiveMap = Arc<HashMap<String, LiveInfo>>;

/// Which terminal window shows which agent process (by owner pid), as last worked out.
#[derive(Default)]
struct Locations {
    map: HashMap<u32, Location>,
    /// When they were last worked out; None until the first time.
    ok_at: Option<Instant>,
}

struct Inner {
    paths: Paths,
    store: Store,
    indexer: Indexer,
    desktop: Desktop,
    explorer: Explorer,
    actor: Arc<dyn Actor>,
    proc_root: PathBuf,
    locations: RwLock<Locations>,
    live_cache: Mutex<Option<(Instant, LiveMap)>>,
    index_error: Mutex<Option<String>>,
    side_error: Mutex<Option<String>>,
    stop: AtomicBool,
    background: AtomicBool,
    /// (last card written, the request file's mtime last seen, last jump made for a request)
    plugin: Mutex<(Option<Instant>, Option<f64>, Option<Instant>)>,
    /// Runs read-only commands for `screen` (`claude logs`).
    runner: Runner,
    look: Look,
    screens: ScreenCache,
    /// Quick questions answered by the person's own claude (Questions view).
    questions: crate::questions::Questions,
    /// Behind the remote: each repo's last check, and the alarm per behind-episode.
    remote: Mutex<RemoteState>,
    /// Sends the one "you're building while behind" notification; tests swap in a fake.
    notifier: Mutex<Notifier>,
    /// What Settings asks of the core: how often to ask remotes, whether to notify.
    opts: Mutex<crate::settings::CoreOptions>,
    /// Disk, resources and attribution (added 2026-11-20).
    overrides: attrib::Overrides,
    procs_cache: Mutex<Option<(Instant, ProcOverview)>>,
    disk: Mutex<DiskState>,
    disk_places: disk::Places,
}

/// The last disk measurement and what has been asked for since.
#[derive(Default)]
struct DiskState {
    report: Option<DiskReport>,
    /// Projects to measure now ("" = everything).
    asked: Vec<String>,
}

#[derive(Default)]
struct RemoteState {
    /// By repo path.
    checks: HashMap<String, RepoCheck>,
    watch: Watch,
    /// Projects to ask about now, whatever the schedule says ("" = all).
    asked: Vec<String>,
}

/// The real source. Cheap to share behind an `Arc`.
pub struct Live {
    inner: Arc<Inner>,
}

impl Drop for Live {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::SeqCst);
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn wait(stop: &AtomicBool, d: Duration) {
    let end = Instant::now() + d;
    while !stop.load(Ordering::SeqCst) && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(200));
    }
}

impl Live {
    pub fn new(cfg: LiveConfig) -> Live {
        let overrides = attrib::Overrides::new(&cfg.paths.data);
        let disk_state = DiskState { report: store::read_json(&cfg.paths.data.join("disk.json")), asked: Vec::new() };
        let disk_places = disk::Places { proc_root: cfg.proc_root.clone(), ..disk::Places::real(&cfg.paths) };
        let store = Store::new(cfg.paths.clone());
        let _ = store.ensure_data_dir();
        let questions = crate::questions::Questions::new(&cfg.paths, crate::questions::find_claude(&cfg.paths));
        for ai in [crate::questions::Ai::Codex, crate::questions::Ai::Gemini] {
            questions.set_tool(ai, crate::questions::find_tool(&cfg.paths, ai));
        }
        Live {
            inner: Arc::new(Inner {
                indexer: Indexer::new(cfg.paths.clone()),
                desktop: Desktop::new(cfg.runner.clone(), &cfg.proc_root, cfg.systemd_run.as_deref()).with_launcher(cfg.terminal.clone()),
                look: Look::new(cfg.runner.clone(), &std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default(), cfg.paths.data.join("kwin"), std::process::id(), true),
                runner: cfg.runner.clone(),
                screens: ScreenCache::default(),
                explorer: Explorer::new(&cfg.proc_root),
                actor: cfg.actor,
                proc_root: cfg.proc_root,
                paths: cfg.paths,
                store,
                locations: RwLock::new(Locations::default()),
                live_cache: Mutex::new(None),
                index_error: Mutex::new(None),
                side_error: Mutex::new(None),
                stop: AtomicBool::new(false),
                background: AtomicBool::new(false),
                plugin: Mutex::new((None, None, None)),
                questions,
                remote: Mutex::new(RemoteState::default()),
                notifier: Mutex::new(remote::runner_notifier(cfg.runner.clone())),
                opts: Mutex::new(crate::settings::CoreOptions::default()),
                overrides,
                procs_cache: Mutex::new(None),
                disk: Mutex::new(disk_state),
                disk_places,
            }),
        }
    }

    /// The real machine, with background threads running.
    pub fn start() -> Live {
        let l = Live::new(LiveConfig::real());
        l.start_background();
        l
    }

    /// Index, windows and git threads. They stop when this `Live` is dropped.
    pub fn start_background(&self) {
        if self.inner.background.swap(true, Ordering::SeqCst) {
            return;
        }
        let a = self.inner.clone();
        let _ = std::thread::Builder::new().name("wake-index".into()).spawn(move || {
            while !a.stop.load(Ordering::SeqCst) {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| a.indexer.run()));
                let now = match r {
                    Ok(_) => a.indexer.stats().sentence(),
                    Err(e) => Some(format!("Indexing failed; Wake keeps showing what it had. ({})", panic_text(&e))),
                };
                let mut cur = lock(&a.index_error);
                if now.is_some() && *cur != now {
                    log::note_error(&a.paths, "Reading transcripts", now.as_deref().unwrap_or_default());
                }
                *cur = now;
                drop(cur);
                wait(&a.stop, INDEX_EVERY);
            }
        });
        let b = self.inner.clone();
        let _ = std::thread::Builder::new().name("wake-side".into()).spawn(move || {
            let mut last_git: Option<Instant> = None;
            let mut first = true;
            while !b.stop.load(Ordering::SeqCst) {
                // A panic in one pass is recorded and the loop goes on, so locations and git totals
                // never silently freeze for the rest of the session.
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    // Locations first, once, so a fresh window does not show every session as
                    // "background" while git runs; then git (milliseconds per project; an unchecked
                    // project would read as "all on GitHub"), then locations every pass.
                    if first {
                        b.refresh_locations();
                    }
                    if last_git.is_none_or(|t| t.elapsed().as_secs_f64() > gitinfo::CACHE_SECONDS) {
                        last_git = Some(Instant::now());
                        for p in b.store.load_projects() {
                            if b.stop.load(Ordering::SeqCst) {
                                break;
                            }
                            if !p.removed {
                                gitinfo::project_git(&p, true);
                            }
                        }
                    }
                    // Behind the remote: notice building starting in a project that is behind.
                    b.watch_behind();
                    if !first {
                        b.refresh_locations();
                    }
                }));
                first = false;
                let mut cur = lock(&b.side_error);
                if let Err(e) = &r
                    && cur.is_none()
                {
                    log::note_error(&b.paths, "Checking git and where sessions are", &panic_text(e));
                }
                *cur = r.err().map(|_| "Checking git and where sessions are failed once; Wake keeps trying.".to_string());
                drop(cur);
                wait(&b.stop, SIDE_EVERY);
            }
        });
        // Disk: measured hourly and when asked, at the lowest CPU and I/O priority.
        let d = self.inner.clone();
        let _ = std::thread::Builder::new().name("wake-disk".into()).spawn(move || {
            disk::lower_priority();
            let measured = lock(&d.disk).report.as_ref().and_then(|r| r.measured_at.as_deref().and_then(util::parse_iso));
            // A measurement under an hour old (from disk.json) waits for its hour; else start soon,
            // after Wake's own start-up work. Wall-clock seconds, so a suspend counts as time gone.
            let every = disk::EVERY.as_secs_f64();
            let mut next_full = match measured {
                Some(t) if util::now() - t < every => t + every,
                _ => util::now() + 20.0,
            };
            while !d.stop.load(Ordering::SeqCst) {
                let asked = std::mem::take(&mut lock(&d.disk).asked);
                let full = util::now() >= next_full || asked.iter().any(|a| a.is_empty());
                if full {
                    if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| d.measure_disk(None))) {
                        log::note_error(&d.paths, "Measuring the disk", &panic_text(&e));
                    }
                    next_full = util::now() + every;
                } else {
                    for p in asked {
                        if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| d.measure_disk(Some(&p)))) {
                            log::note_error(&d.paths, "Measuring the disk", &panic_text(&e));
                        }
                    }
                }
                while !d.stop.load(Ordering::SeqCst) && util::now() < next_full && lock(&d.disk).asked.is_empty() {
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        });
        // Asking remotes can take seconds per repo (20 s at worst, offline), so it has its own
        // thread and never holds up locations or git totals. One repo at a time.
        let c = self.inner.clone();
        let _ = std::thread::Builder::new().name("wake-remote".into()).spawn(move || {
            wait(&c.stop, Duration::from_secs(3));
            while !c.stop.load(Ordering::SeqCst) {
                if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| c.check_remotes_pass())) {
                    log::note_error(&c.paths, "Asking the remotes", &panic_text(&e));
                }
                let end = Instant::now() + Duration::from_secs(30);
                while !c.stop.load(Ordering::SeqCst) && Instant::now() < end && lock(&c.remote).asked.is_empty() {
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        });
    }

    /// Replace the notification sender (tests; never send a real one from a test).
    pub fn set_notifier(&self, n: Notifier) {
        *lock(&self.inner.notifier) = n;
    }

    /// One indexing pass now (the command line uses this; the app has the background thread).
    pub fn index_now(&self) -> u32 {
        self.inner.indexer.run()
    }

    pub fn indexer(&self) -> &Indexer {
        &self.inner.indexer
    }

    pub fn store(&self) -> &Store {
        &self.inner.store
    }

    pub fn paths(&self) -> &Paths {
        &self.inner.paths
    }

    pub fn desktop(&self) -> &Desktop {
        &self.inner.desktop
    }

    /// Measure the disk now, on this thread, at the lowest priority (the command line's `wake
    /// disk`); keeps the result in disk.json like the background thread does.
    pub fn measure_disk_now(&self) -> Option<DiskReport> {
        disk::lower_priority();
        self.inner.measure_disk(None);
        let projects = self.inner.store.load_projects();
        let sessions = self.inner.all_summaries(&projects, true);
        self.inner.disk_report(&sessions)
    }

    /// Place every agent process in its terminal window now.
    pub fn refresh_locations(&self) {
        self.inner.refresh_locations();
    }

    /// The Slipstream plugin bridge; call it about once a second. Writes card/card.json every 15 s
    /// and acts on a new jump request from the plugin (at most 30 s old), going to that session.
    pub fn plugin_tick(&self, dry_run: bool) -> Option<ActionResult> {
        if lock(&self.inner.opts).quiet {
            return None;
        }
        let due = lock(&self.inner.plugin).0.is_none_or(|t| t.elapsed().as_secs_f64() > CARD_EVERY);
        if due {
            lock(&self.inner.plugin).0 = Some(Instant::now());
            let _ = self.write_card();
        }
        let req = self.inner.paths.plugin_state.join("request.json");
        let m = std::fs::metadata(&req).ok().map(|m| {
            use std::os::unix::fs::MetadataExt;
            m.mtime() as f64 + m.mtime_nsec() as f64 / 1e9
        });
        let changed = {
            let mut g = lock(&self.inner.plugin);
            let c = m.is_some() && m != g.1;
            if c {
                g.1 = m;
            }
            c
        };
        if changed { self.card_request(&req, dry_run) } else { None }
    }

    /// card/card.json for the Slipstream plugin: counts, the one waiting longest, live sessions.
    pub fn write_card(&self) -> Result<(), String> {
        let ov = self.overview();
        let rank = |s: State| match s {
            State::Waiting => 0,
            State::Working => 1,
            _ => 2,
        };
        let mut live: Vec<&SessionSummary> = ov.sessions.iter().filter(|s| matches!(s.state, State::Waiting | State::Working | State::Idle)).collect();
        live.sort_by(|a, b| (rank(a.state), a.last_at.as_deref().unwrap_or("")).cmp(&(rank(b.state), b.last_at.as_deref().unwrap_or(""))));
        let ok = |id: &str| !id.is_empty() && id.len() <= 80 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        let state_s = |s: State| match s {
            State::Waiting => "waiting",
            State::Working => "working",
            _ => "idle",
        };
        let rows: Vec<Value> = live
            .iter()
            .filter(|s| ok(&s.id))
            .take(50)
            .map(|s| {
                serde_json::json!({"id": s.id, "title": s.title, "project": s.project_name.clone().unwrap_or_else(|| "home".into()),
                "state": state_s(s.state)})
            })
            .collect();
        let top = rows.iter().find(|r| r["state"] == "waiting").map(|r| serde_json::json!({"id": r["id"], "title": r["title"], "project": r["project"]}));
        let card = serde_json::json!({"working": ov.counts.working, "waiting": ov.counts.waiting, "idle": ov.counts.idle,
            "ideas": ov.ideas, "top": top, "sessions": rows, "updatedAt": util::now_iso()});
        self.inner.store.writer.write_json(&self.inner.paths.data.join("card").join("card.json"), &card).map_err(|e| e.0)
    }

    /// The plugin asks to jump to a session. Only a live session on the card Wake wrote, at most
    /// one jump every 3 s, and never a reopen: anyone who can write the request file gets no more
    /// than the plugin's own button.
    fn card_request(&self, req_path: &Path, dry_run: bool) -> Option<ActionResult> {
        let req: Value = store::read_json_capped(req_path, 64 * 1024, true).unwrap_or(Value::Null);
        let rid = match req.get("id") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            _ => return None,
        };
        let done = self.inner.paths.data.join("card").join("last-request");
        let last = std::fs::read_to_string(&done).unwrap_or_default();
        if rid.is_empty() || rid.len() > 128 || rid == last.trim() || req.get("action").and_then(Value::as_str) != Some("jump") {
            return None;
        }
        let _ = self.inner.store.writer.write_atomic(&done, rid.as_bytes());
        let at = req.get("at").and_then(Value::as_str).and_then(util::parse_iso)?;
        let age = util::now() - at;
        if !(-5.0..=30.0).contains(&age) {
            return None;
        }
        let session = req.get("session").and_then(Value::as_str)?;
        let card: Value = store::read_json(&self.inner.paths.data.join("card").join("card.json"))?;
        let on_card = card.get("sessions").and_then(Value::as_array).is_some_and(|rows| rows.iter().any(|r| r.get("id").and_then(Value::as_str) == Some(session)));
        if !on_card || !self.inner.live().get(session).is_some_and(|i| i.alive) {
            return None;
        }
        {
            let mut g = lock(&self.inner.plugin);
            if g.2.is_some_and(|t| t.elapsed() < Duration::from_secs(3)) {
                return None;
            }
            g.2 = Some(Instant::now());
        }
        Some(self.go_to_logged(session, dry_run, false))
    }
}

impl Inner {
    fn live(&self) -> Arc<HashMap<String, LiveInfo>> {
        let mut c = lock(&self.live_cache);
        if let Some((at, data)) = c.as_ref()
            && at.elapsed().as_secs_f64() < 1.0
        {
            return data.clone();
        }
        let prev = c.as_ref().map(|(_, d)| d.clone()).unwrap_or_default();
        let mut data = live::read_live_after(&self.paths, &self.proc_root, &prev);
        for (sid, info) in live::read_codex_live(&self.proc_root, procs::my_uid()) {
            data.entry(sid).or_insert(info);
        }
        let data = Arc::new(data);
        *c = Some((Instant::now(), data.clone()));
        data
    }

    /// Every agent process that could be in a window, with whom a window showing it is credited
    /// to: an attach client's goes to the session it attaches to.
    fn placees(&self, all: &[procs::ProcInfo], infos: &HashMap<String, LiveInfo>) -> Vec<Placee> {
        all.iter()
            .filter_map(|p| {
                let agent = procs::identify(p, infos)?;
                let role = if agent == Agent::Claude { procs::role_of(&p.argv) } else { procs::Role::Main };
                let owner = match role {
                    procs::Role::Daemon | procs::Role::PtyHost => return None,
                    procs::Role::Attach(job) => infos.values().find(|i| i.alive && i.job_id.as_deref() == Some(job.as_str())).map_or(p.pid, |i| i.pid),
                    _ => p.pid,
                };
                Some(Placee { pid: p.pid, owner })
            })
            .collect()
    }

    /// The terminal window process `pid` reads its terminal from: the terminal app holding the
    /// other end of its pty, never an agent (a background session's own host).
    fn window_of(&self, pid: u32, agents: &[u32], masters: &HashMap<u32, Vec<u32>>) -> Option<Location> {
        let n = procs::pts_of(&self.proc_root, pid)?;
        let holder = *masters.get(&n)?.iter().find(|h| !agents.contains(h))?;
        Some(self.desktop.window(holder))
    }

    /// The window a Claude dashboard (`claude agents`) shows session `info` in. A dashboard shows
    /// one session at a time inside one process, and nothing outside it says which; but a session's
    /// terminal takes the size of the window showing it. Newest dashboard first when two fit.
    fn dashboard_of(&self, info: &LiveInfo, all: &[procs::ProcInfo], agents: &[u32], masters: &HashMap<u32, Vec<u32>>) -> Option<Location> {
        let size = crate::vt::term_size(info.pid)?;
        let mut fits: Vec<u32> =
            all.iter().filter(|p| agents.contains(&p.pid) && p.argv.get(1).map(String::as_str) == Some("agents") && crate::vt::term_size(p.pid) == Some(size)).map(|p| p.pid).collect();
        fits.sort_unstable_by(|a, b| b.cmp(a));
        fits.into_iter().find_map(|pid| self.window_of(pid, agents, masters))
    }

    /// The terminal window showing a live session, worked out now: an attach client's (for a
    /// background session), its own, or a dashboard's that has it open.
    fn find_window(&self, info: &LiveInfo) -> Option<Location> {
        let infos = self.live();
        let all = self.explorer.scan();
        let masters = procs::pty_masters(&self.proc_root);
        let agents: Vec<u32> = all.iter().filter(|p| procs::identify(p, &infos).is_some()).map(|p| p.pid).collect();
        let attach = self.placees(&all, &infos).into_iter().filter(|p| p.owner == info.pid && p.pid != info.pid).map(|p| p.pid);
        attach.chain([info.pid]).find_map(|pid| self.window_of(pid, &agents, &masters)).or_else(|| self.dashboard_of(info, &all, &agents, &masters))
    }

    /// The one place locations are decided (see the module doc): which terminal window shows each
    /// agent process (the Agents view's Where), and each live session (an attach client's window
    /// is its session's), read from /proc. Works with any terminal app.
    fn refresh_locations(&self) {
        let infos = self.live();
        let all = self.explorer.scan();
        let masters = procs::pty_masters(&self.proc_root);
        let agents: Vec<u32> = all.iter().filter(|p| procs::identify(p, &infos).is_some()).map(|p| p.pid).collect();
        let mut map: HashMap<u32, Location> = HashMap::new();
        let mut placees = self.placees(&all, &infos);
        // A session's own terminal before an attach client's.
        placees.sort_by_key(|p| (p.pid != p.owner, p.pid));
        for p in placees {
            if let Some(loc) = self.window_of(p.pid, &agents, &masters) {
                map.entry(p.owner).or_insert_with(|| loc.clone());
                map.entry(p.pid).or_insert(loc);
            }
        }
        let unplaced: Vec<&LiveInfo> = infos.values().filter(|i| i.alive && !i.codex && !map.contains_key(&i.pid)).collect();
        for i in unplaced {
            if let Some(loc) = self.dashboard_of(i, &all, &agents, &masters) {
                map.insert(i.pid, loc);
            }
        }
        *self.locations.write().unwrap_or_else(|e| e.into_inner()) = Locations { map, ok_at: Some(Instant::now()) };
    }

    /// In the background the side thread keeps locations fresh; without it (the command line, tests
    /// that never start it) work them out now when they are more than a couple of seconds old.
    fn locations_now(&self) {
        if self.background.load(Ordering::SeqCst) {
            return;
        }
        let stale = self.locations.read().unwrap_or_else(|e| e.into_inner()).ok_at.is_none_or(|t| t.elapsed().as_secs_f64() > 2.0);
        if stale {
            self.refresh_locations();
        }
    }

    fn location(&self, pid: u32) -> Option<Location> {
        self.locations.read().unwrap_or_else(|e| e.into_inner()).map.get(&pid).cloned()
    }

    /// Why the locations shown may be wrong: only before they are first worked out.
    fn locations_unsure(&self) -> Option<String> {
        let l = self.locations.read().unwrap_or_else(|e| e.into_inner());
        l.ok_at.is_none().then(|| "Wake is still finding which window each session is in.".to_string())
    }

    fn summary(&self, acc: &Acc, projects: &[ProjectRec], infos: &HashMap<String, LiveInfo>, now: f64, counts: bool) -> SessionSummary {
        let mut s = index::base_summary(acc, projects, &self.paths.home);
        let info = infos.get(&acc.id);
        live::apply_live(&mut s, Some(acc), info, now);
        if counts {
            s.subagents = index::subagent_counts(acc, now);
        }
        if let Some(i) = info.filter(|i| i.alive) {
            s.location = self.location(i.pid);
        }
        s
    }

    fn all_summaries(&self, projects: &[ProjectRec], recent_only: bool) -> Vec<SessionSummary> {
        let infos = self.live();
        let now = util::now();
        let cutoff = util::iso_from_epoch(now - RECENT_DAYS * 86400.0);
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        // Sessions Questions started are not work: they stay out of every list.
        let hider = self.questions.hider();
        let answers = self.overrides.all();
        for acc in self.indexer.all_lite() {
            if hider.hides(&acc.id, acc.launch_cwd.as_deref()) {
                continue;
            }
            let info = infos.get(&acc.id);
            let is_live = info.is_some_and(|i| i.alive);
            let recent = acc.last_at.as_deref().unwrap_or("") >= cutoff.as_str();
            if recent_only && !(is_live || recent) {
                continue;
            }
            seen.insert(acc.id.clone());
            let untitled = acc.prompts == 0 && acc.ai_title.is_none() && acc.custom_title.is_none();
            let name = if is_live { live::real_name(info) } else { None };
            if untitled && name.is_none() {
                continue; // empty session (or an idle one never used): nothing to show
            }
            let mut s = self.summary(&acc, projects, &infos, now, is_live || recent);
            attrib::apply_to(&answers, &mut s, projects);
            if untitled && let Some(n) = name {
                s.title = n;
            }
            out.push(s);
        }
        for (sid, info) in infos.iter() {
            if !seen.contains(sid) && info.alive && live::real_name(Some(info)).is_some() && !hider.hides(sid, info.cwd.as_deref()) {
                let mut s = live::live_only_summary(info, projects, &self.paths.home, now);
                s.location = self.location(info.pid);
                attrib::apply_to(&answers, &mut s, projects);
                out.push(s);
            }
        }
        out.sort_by(|a, b| b.last_at.cmp(&a.last_at));
        out
    }

    fn project_summary(&self, p: &ProjectRec, sessions: &[SessionSummary], ideas: &[IdeaRec], pauses: &HashMap<String, PauseRec>, now: f64) -> ProjectSummary {
        let cutoff = util::iso_from_epoch(now - RECENT_DAYS * 86400.0);
        let mine: Vec<&SessionSummary> = sessions.iter().filter(|s| s.project.as_deref() == Some(p.id.as_str())).collect();
        let mut livec = LiveCounts::default();
        for s in &mine {
            match s.state {
                State::Working => livec.working += 1,
                State::Waiting => livec.waiting += 1,
                State::Idle => livec.idle += 1,
                _ => {}
            }
        }
        let last = mine.iter().filter_map(|s| s.last_at.clone()).max();
        let mut by_last = mine.clone();
        by_last.sort_by(|a, b| b.last_at.cmp(&a.last_at));
        ProjectSummary {
            id: p.id.clone(),
            name: p.name.clone(),
            roots: p.roots.clone(),
            removed: p.removed,
            desc: p.desc.clone(),
            next: p.next.clone(),
            why: p.why.clone(),
            live: livec,
            last_at: last,
            sessions_14d: mine.iter().filter(|s| s.last_at.as_deref().unwrap_or("") >= cutoff.as_str()).count() as u32,
            ideas: ideas.iter().filter(|i| i.project.as_deref() == Some(p.id.as_str()) && i.state == "inbox").count() as u32,
            git: gitinfo::cached_totals(p).unwrap_or_default(),
            paused: pauses.get(&p.id).map(|r| PauseNote { at: r.at.clone(), note: r.note.clone() }),
            last_summary: by_last.iter().find_map(|s| s.away_summary.clone()),
            git_checked: gitinfo::cached_totals(p).is_some(),
            color: Some(p.color.as_deref().and_then(store::colour_name).unwrap_or_else(|| store::default_colour(&p.name)).to_string()),
            remote_problem: self.remote_problem(p),
            group: p.group.clone(),
            importance: p.importance.clone(),
        }
    }

    /// "Wake couldn't ask GitHub (offline)." when the last check of any of the project's repos got
    /// no answer from its remote.
    fn remote_problem(&self, p: &ProjectRec) -> Option<String> {
        let repos = gitinfo::cached_repos(p)?;
        let r = lock(&self.remote);
        repos.iter().filter_map(|x| r.checks.get(&x.state.path)).find_map(|c| c.problem.as_ref().map(|why| format!("Wake couldn't ask {} ({why}).", c.host)))
    }

    /// Ask the remote of every repo that is due (ten minutes since its last check) or asked for.
    fn check_remotes_pass(&self) {
        let asked = std::mem::take(&mut lock(&self.remote).asked);
        let all = asked.iter().any(|a| a.is_empty());
        for p in self.store.load_projects().into_iter().filter(|p| !p.removed) {
            let wanted = all || asked.contains(&p.id);
            let repos = match gitinfo::cached_repos(&p) {
                Some(r) if !wanted => r,
                _ => gitinfo::project_git(&p, wanted).0,
            };
            for r in repos {
                if self.stop.load(Ordering::SeqCst) {
                    return;
                }
                // Settings may turn the schedule off; an explicit ask still goes through.
                let every = lock(&self.opts).remote_every;
                // By the wall clock too (when the last answer came), which keeps running through a
                // suspend; an Instant doesn't.
                let old = |c: &RepoCheck, every: Duration| c.at.elapsed() >= every || c.asked_at.as_deref().and_then(util::parse_iso).is_some_and(|t| util::now() - t >= every.as_secs_f64());
                let due = every.is_some_and(|every| lock(&self.remote).checks.get(&r.state.path).is_none_or(|c| old(c, every)));
                if due || wanted {
                    let c = remote::check_repo(&r.state.path);
                    lock(&self.remote).checks.insert(r.state.path.clone(), c);
                }
            }
        }
    }

    /// Every tracked project that is behind, from git's cache and the last remote checks (never
    /// blocks on git or the network).
    fn behind_list(&self, projects: &[ProjectRec]) -> Vec<Behind> {
        let r = lock(&self.remote);
        let mut out = Vec::new();
        for p in projects.iter().filter(|p| !p.removed) {
            let Some(repos) = gitinfo::cached_repos(p) else { continue };
            // A repo not asked yet still counts as behind when its last fetch says so.
            let local: Vec<RepoCheck> = repos
                .iter()
                .filter(|x| !r.checks.contains_key(&x.state.path))
                .filter_map(|x| {
                    let n = x.state.behind.filter(|n| *n > 0)?;
                    Some(RepoCheck {
                        path: x.state.path.clone(),
                        upstream: x.state.upstream.clone().unwrap_or_default(),
                        host: "the remote".into(),
                        outcome: remote::Outcome::Behind { count: Some(n), ahead: x.state.ahead.unwrap_or(0) },
                        remote_sha: None,
                        problem: None,
                        asked_at: None,
                        at: Instant::now(),
                    })
                })
                .collect();
            let checks: Vec<&RepoCheck> = repos.iter().filter_map(|x| r.checks.get(&x.state.path)).chain(local.iter()).collect();
            let dirty = |path: &str| repos.iter().find(|x| x.state.path == path).map_or(0, |x| x.state.dirty_files + x.state.untracked);
            if let Some(mut b) = remote::behind_for(&p.id, &p.name, &p.roots, &checks, &dirty) {
                b.alarm = r.watch.alarm(&p.id);
                out.push(b);
            }
        }
        out
    }

    /// Every 5 s: while a project is behind, keep its git fresh and watch for building starting
    /// in it (a session going from idle to working, or its tree going from clean to dirty). The
    /// first time per behind-episode: the alarm, and one desktop notification.
    fn watch_behind(&self) {
        let projects = self.store.load_projects();
        let behind_ids: Vec<String> = self.behind_list(&projects).into_iter().map(|b| b.project).collect();
        if behind_ids.is_empty() {
            let mut r = lock(&self.remote);
            for p in &projects {
                r.watch.observe(&p.id, false, &[], None);
            }
            return;
        }
        for p in projects.iter().filter(|p| behind_ids.contains(&p.id)) {
            // Fresh status (dirty files, HEAD) and, without the network, whether a pull or commit
            // here has changed the answer.
            let (repos, _) = gitinfo::project_git(p, true);
            for x in repos {
                let prev = lock(&self.remote).checks.get(&x.state.path).filter(|c| c.is_behind()).cloned();
                if let Some(prev) = prev {
                    let c = remote::recheck(&prev);
                    lock(&self.remote).checks.insert(x.state.path.clone(), c);
                }
            }
        }
        let behind = self.behind_list(&projects);
        let sessions = self.all_summaries(&projects, true);
        let mut notices = Vec::new();
        {
            let mut r = lock(&self.remote);
            for p in &projects {
                let b = behind.iter().find(|b| b.project == p.id);
                let working: Vec<String> = sessions.iter().filter(|s| s.project.as_deref() == Some(p.id.as_str()) && s.state == State::Working).map(|s| s.id.clone()).collect();
                let dirty = gitinfo::cached_totals(p).map(|t| t.dirty_files > 0);
                if let Some(b) = b
                    && r.watch.observe(&p.id, true, &working, dirty).is_some()
                {
                    notices.push(remote::notice(b));
                } else if b.is_none() {
                    r.watch.observe(&p.id, false, &[], None);
                }
            }
        }
        // Settings can keep the desktop notification quiet; the banner shows either way.
        if !lock(&self.opts).notify_behind {
            return;
        }
        let send = lock(&self.notifier).clone();
        for n in notices {
            send(&n);
        }
    }

    fn idea_out(&self, i: &IdeaRec, projects: &[ProjectRec]) -> Idea {
        Idea {
            id: i.id.clone(),
            at: i.at.clone(),
            text: i.text.clone(),
            project: i.project.clone(),
            project_name: i.project.as_deref().and_then(|p| store::project_by_id(p, projects)).map(|p| p.name.clone()),
            source: i.source.clone(),
            state: i.state.clone(),
            sent_at: i.sent_at.clone(),
            brief: i.brief.clone(),
        }
    }

    fn session_view(&self, info: &LiveInfo, projects: &[ProjectRec], now: f64) -> SessionView {
        let acc = self.indexer.get_lite(&info.session_id);
        let mut s = match &acc {
            Some(a) => index::base_summary(a, projects, &self.paths.home),
            None => live::live_only_summary(info, projects, &self.paths.home, now),
        };
        live::apply_live(&mut s, acc.as_deref(), Some(info), now);
        self.overrides.apply(&mut s, projects);
        let used_acc = acc.as_ref().is_some_and(|a| a.prompts > 0 || a.ai_title.is_some() || a.custom_title.is_some());
        let name = live::real_name(Some(info));
        let title = if used_acc { Some(s.title.clone()) } else { name.clone() };
        let last = s.last_at.as_deref().and_then(util::parse_iso);
        let newest = [info.status_updated_at, last].into_iter().flatten().fold(None, |a: Option<f64>, b| Some(a.map_or(b, |a| a.max(b))));
        SessionView {
            title,
            project_name: s.project_name.clone(),
            state: Some(s.state),
            idle_since: match s.state {
                State::Working => None,
                _ => newest,
            },
            used: used_acc || name.is_some(),
        }
    }

    fn notes(&self) -> HashMap<u32, WorkingNote> {
        let mut out = HashMap::new();
        let Ok(rd) = std::fs::read_dir(self.paths.runtime.join("working")) else { return out };
        for e in rd.flatten() {
            if let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok())
                && let Some((n, _)) = live::read_working_note(&self.paths, pid)
            {
                out.insert(pid, n);
            }
        }
        out
    }

    fn title_for(&self, sid: &str, info: Option<&LiveInfo>, projects: &[ProjectRec]) -> String {
        let acc = self.indexer.get_lite(sid);
        let mut title = match &acc {
            Some(a) => index::title_of(a),
            None => info.and_then(|i| i.name.clone()).unwrap_or_else(|| "This session".into()),
        };
        if title == "Untitled session" {
            title = live::real_name(info).unwrap_or_else(|| "This session".into());
        }
        let pname = match self.overrides.get(sid) {
            Some(choice) => choice.as_deref().and_then(|id| store::project_by_id(id, projects)).map(|p| p.name.clone()),
            None => acc.as_ref().and_then(|a| index::attribute(a, projects, &self.paths.home).1),
        };
        if let Some(pname) = pname {
            title = format!("{pname}, {title}");
        }
        title
    }
}

/// What a caught panic said, for Wake's log ("it failed" when it said nothing readable).
fn panic_text(e: &Box<dyn std::any::Any + Send>) -> String {
    let said = e.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| e.downcast_ref::<String>().cloned());
    format!("Wake hit a bug here: {}", said.unwrap_or_else(|| "it failed".into()))
}

/// `~/…` for a path under home.
fn tilde(p: &str, home: &Path) -> String {
    util::tilde(Path::new(p), home)
}

/// A session id's first eight characters, as Wake's log names sessions.
fn short_id(sid: &str) -> String {
    sid.chars().take(8).collect()
}

/// `message` with a session's title taken out (titles can be prompt text, kept out of the log).
fn without(message: &str, title: &str) -> String {
    if title.trim().is_empty() { message.to_string() } else { message.replace(title, "the session") }
}

fn snippet(text: Option<&str>, q: &str) -> Option<String> {
    let text = text?;
    let low = text.to_lowercase();
    let i = low.find(q)?;
    // map the byte offset in the lowercased text back to a char index (lowercasing can change lengths)
    let ci = low[..i].chars().count();
    let start = ci.saturating_sub(40);
    let s: String = text.chars().skip(start).take(120).collect();
    let s = util::squash(&s);
    Some(if start > 0 { format!("...{s}") } else { s })
}

fn checkpoint_of(r: &PauseRec) -> Checkpoint {
    Checkpoint {
        project: r.project.clone(),
        at: r.at.clone(),
        note: r.note.clone(),
        next: r.next.clone(),
        git: r
            .git
            .iter()
            .map(|(path, m)| {
                (
                    path.clone(),
                    m.get("branch").and_then(Value::as_str).map(String::from),
                    m.get("head").and_then(Value::as_str).map(String::from),
                    m.get("ahead").and_then(Value::as_u64).map(|a| a as u32),
                )
            })
            .collect(),
        sessions: r.sessions.clone(),
    }
}

impl Source for Live {
    fn overview(&self) -> Overview {
        let i = &self.inner;
        i.locations_now();
        let projects = i.store.load_projects();
        let sessions = i.all_summaries(&projects, true);
        let ideas = i.store.list_ideas();
        let pauses = i.store.last_pauses();
        let now = util::now();
        let mut counts = LiveCounts::default();
        for s in &sessions {
            match s.state {
                State::Working => counts.working += 1,
                State::Waiting => counts.waiting += 1,
                State::Idle => counts.idle += 1,
                _ => {}
            }
        }
        let (done, total) = i.indexer.progress();
        let sessions_for_res = sessions.clone();
        let problems: Vec<String> = [lock(&i.index_error).clone(), i.store.projects_problem(), lock(&i.side_error).clone()].into_iter().flatten().collect();
        Overview {
            generated_at: util::now_iso(),
            indexing: Progress { done, total },
            counts,
            ideas: ideas.iter().filter(|x| x.state == "inbox").count() as u32,
            projects: projects.iter().map(|p| i.project_summary(p, &sessions, &ideas, &pauses, now)).collect(),
            sessions,
            index_error: if problems.is_empty() { None } else { Some(problems.join(" ")) },
            locations_unsure: i.locations_unsure(),
            behind: Some(i.behind_list(&projects)),
            resources: Some(i.resources(&sessions_for_res)),
            disk: i.disk_report(&sessions_for_res),
            attribution: Some(i.attribution(&sessions_for_res, &projects)),
            groups: i.store.groups(),
        }
    }

    fn session(&self, id: &str) -> Option<SessionDetail> {
        let i = &self.inner;
        let projects = i.store.load_projects();
        let infos = i.live();
        let info = infos.get(id);
        let now = util::now();
        let Some(acc) = i.indexer.read_acc(id) else {
            let info = info.filter(|x| x.alive)?;
            let mut s = live::live_only_summary(info, &projects, &i.paths.home, now);
            s.location = i.location(info.pid);
            i.overrides.apply(&mut s, &projects);
            return Some(SessionDetail { summary: s, prompts: vec![], files: vec![], commands: 0, tests: 0, pr_links: vec![], cost_usd: None, subagents: vec![], transcript: None });
        };
        let mut s = index::base_summary(&acc, &projects, &i.paths.home);
        live::apply_live(&mut s, Some(&acc), info, now);
        i.overrides.apply(&mut s, &projects);
        if let Some(inf) = info.filter(|x| x.alive) {
            s.location = i.location(inf.pid);
        }
        let subs: Vec<SubagentSummary> = i.indexer.subagents(&acc).iter().map(|x| x.to_contract()).collect();
        s.subagents = SubagentCounts { total: if subs.is_empty() { s.subagents.total } else { subs.len() as u32 }, running: subs.iter().filter(|x| x.state == State::Working).count() as u32 };
        Some(SessionDetail {
            summary: s,
            prompts: index::prompts_out(&acc),
            files: index::files_out(&acc),
            commands: acc.commands,
            tests: acc.tests,
            pr_links: acc.pr_links.clone(),
            cost_usd: acc.cost,
            subagents: subs,
            transcript: acc.source.as_deref().map(|p| tilde(p, &i.paths.home)),
        })
    }

    fn subagent(&self, session: &str, agent_id: &str) -> Option<SubagentDetail> {
        self.inner.indexer.subagent_detail(session, agent_id)
    }

    fn project(&self, id: &str) -> Option<ProjectDetail> {
        let i = &self.inner;
        let projects = i.store.load_projects();
        let p = store::project_by_id(id, &projects)?;
        let sessions = i.all_summaries(&projects, false);
        let ideas = i.store.list_ideas();
        let pauses = i.store.last_pauses();
        let mut summ = i.project_summary(p, &sessions, &ideas, &pauses, util::now());
        let (repos, failed) = gitinfo::project_git(p, false);
        summ.git = gitinfo::git_totals(&repos);
        let mine: Vec<SessionSummary> = sessions.into_iter().filter(|s| s.project.as_deref() == Some(id)).take(100).collect();
        let pause = pauses.get(id);
        let mut since = Vec::new();
        if let Some(pause) = pause {
            for r in &repos {
                let subj = gitinfo::commits_since(&r.state.path, &pause.at, 20);
                if !subj.is_empty() {
                    let name = Path::new(&r.state.path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    since.push(format!("{} commit{} in {}: {}", subj.len(), if subj.len() == 1 { "" } else { "s" }, name, subj.iter().take(5).cloned().collect::<Vec<_>>().join("; ")));
                }
            }
            let later: Vec<&SessionSummary> = mine.iter().filter(|s| s.last_at.as_deref().unwrap_or("") > pause.at.as_str()).collect();
            if !later.is_empty() {
                since.push(format!(
                    "{} session{} since you paused: {}",
                    later.len(),
                    if later.len() == 1 { "" } else { "s" },
                    later.iter().take(5).map(|s| s.title.as_str()).collect::<Vec<_>>().join("; ")
                ));
            }
        }
        Some(ProjectDetail {
            summary: summ,
            repos: repos.into_iter().map(|r| r.state).collect(),
            sessions: mine,
            ideas: ideas.iter().filter(|x| x.project.as_deref() == Some(id)).map(|x| i.idea_out(x, &projects)).collect(),
            last_pause: pause.map(checkpoint_of),
            since_pause: since,
            could_not_check: failed,
        })
    }

    fn search(&self, query: &str) -> Vec<(SessionSummary, String)> {
        let i = &self.inner;
        let q = query.trim().to_lowercase();
        if q.chars().count() < 2 {
            return Vec::new();
        }
        let projects = i.store.load_projects();
        let infos = i.live();
        let now = util::now();
        let hider = i.questions.hider();
        let mut accs = i.indexer.all_lite();
        accs.retain(|a| !hider.hides(&a.id, a.launch_cwd.as_deref()));
        accs.sort_by(|a, b| b.last_at.cmp(&a.last_at));
        let mut hits = Vec::new();
        for acc in accs {
            let title = index::title_of(&acc);
            // Each snippet says what it matched ("prompt: …"), so the window can open that part.
            let mut m = [("title", Some(title.as_str())), ("recap", acc.away_summary.as_deref()), ("prompt", acc.first_prompt.as_deref()), ("prompt", acc.last_prompt_meta.as_deref())]
                .into_iter()
                .find_map(|(k, f)| snippet(f, &q).map(|m| format!("{k}: {m}")));
            if m.is_none()
                && let Some(full) = i.indexer.read_acc(&acc.id)
            {
                m = full.prompts_list.iter().rev().find_map(|p| snippet(Some(&p.text), &q)).map(|m| format!("prompt: {m}"));
                if m.is_none() {
                    m = full.files.iter().find(|(p, _)| p.to_lowercase().contains(&q)).map(|(p, _)| format!("file: {}", index::short_path(p, &i.paths.home)));
                }
            }
            if let Some(m) = m {
                let mut s = i.summary(&acc, &projects, &infos, now, false);
                i.overrides.apply(&mut s, &projects);
                hits.push((s, m));
                if hits.len() >= 50 {
                    break;
                }
            }
        }
        hits
    }

    fn ideas(&self) -> Vec<Idea> {
        let projects = self.inner.store.load_projects();
        self.inner.store.list_ideas().iter().map(|x| self.inner.idea_out(x, &projects)).collect()
    }

    fn add_idea(&self, text: &str, project: Option<&str>) -> Result<Idea, String> {
        let projects = self.inner.store.load_projects();
        if let Some(p) = project
            && store::project_by_id(p, &projects).is_none()
        {
            return Err("No project with that id.".into());
        }
        let rec = self.inner.store.add_idea(text, project, "typed", None).map_err(|e| e.0)?;
        Ok(self.inner.idea_out(&rec, &projects))
    }

    fn set_idea(&self, id: &str, state: Option<&str>, project: Option<Option<&str>>) -> Result<Idea, String> {
        let projects = self.inner.store.load_projects();
        if let Some(Some(p)) = project
            && store::project_by_id(p, &projects).is_none()
        {
            return Err("No project with that id.".into());
        }
        let rec = self.inner.store.set_idea(id, state, project).map_err(|e| e.0)?;
        Ok(self.inner.idea_out(&rec, &projects))
    }

    fn pause(&self, project: &str, note: &str, next: &str) -> Result<Checkpoint, String> {
        let i = &self.inner;
        let projects = i.store.load_projects();
        let p = store::project_by_id(project, &projects).ok_or("No project with that id.")?;
        let (repos, _) = gitinfo::project_git(p, true);
        let mut git = Map::new();
        for r in &repos {
            git.insert(r.state.path.clone(), serde_json::json!({"branch": r.state.branch, "head": r.state.head, "ahead": r.state.ahead}));
        }
        let prev = i.store.last_pauses().remove(&p.id);
        let sessions: Vec<String> = i
            .all_summaries(&projects, false)
            .into_iter()
            .filter(|s| s.project.as_deref() == Some(p.id.as_str()) && (s.state != State::Ended || prev.as_ref().is_none_or(|pr| s.last_at.as_deref().unwrap_or("") > pr.at.as_str())))
            .map(|s| s.id)
            .take(200)
            .collect();
        let rec = i.store.add_pause(&p.id, note, next, git, sessions).map_err(|e| e.0)?;
        // The next step also goes on the project itself, so it is still there after later pauses
        // without one and wherever the project is shown.
        if !next.trim().is_empty()
            && let Err(e) = i.store.set_project_next(&p.id, next)
        {
            return Err(format!("The pause was saved, but Wake couldn't save the next step on the project: {}", e.0));
        }
        Ok(checkpoint_of(&rec))
    }

    fn add_project(&self, path: &str) -> Result<ProjectSummary, String> {
        let p = self.inner.store.add_project(path).map_err(|e| e.0)?;
        Ok(self.project_summary_now(&p))
    }

    fn remove_project(&self, id: &str, removed: bool) -> Result<ProjectSummary, String> {
        let p = self.inner.store.set_project_removed(id, removed).map_err(|e| e.0)?;
        Ok(self.project_summary_now(&p))
    }

    fn go_to(&self, session: &str, dry_run: bool) -> ActionResult {
        self.go_to_logged(session, dry_run, true)
    }

    fn processes(&self) -> ProcOverview {
        self.inner.proc_overview()
    }

    // ---- Disk, resources and attribution (added 2026-11-20) ----

    fn measure_disk(&self, project: Option<&str>) {
        let mut d = lock(&self.inner.disk);
        let p = project.unwrap_or("").to_string();
        if !d.asked.contains(&p) {
            d.asked.push(p);
        }
    }

    fn clear_disk(&self, path: &str, dry_run: bool) -> ActionResult {
        let i = &self.inner;
        let Some(report) = lock(&i.disk).report.clone() else {
            return ActionResult { ok: false, message: "Wake hasn't measured the disk yet.".into(), steps: vec![] };
        };
        let owner = report.projects.iter().find(|p| p.items.iter().any(|x| x.path == path)).map(|p| p.project.clone());
        let projects = i.store.load_projects();
        let working = owner.as_deref().and_then(|id| {
            let busy = i.all_summaries(&projects, true).iter().any(|s| s.state == State::Working && s.project.as_deref() == Some(id));
            busy.then(|| store::project_by_id(id, &projects).map_or(id.to_string(), |p| p.name.clone()))
        });
        let r = disk::clear(&report, path, working.as_deref(), &i.disk_places, dry_run);
        if r.ok && !dry_run {
            i.forget_disk_item(path, owner.as_deref());
        }
        if !dry_run {
            log::record_action(&i.paths, "move to Trash", &tilde(path, &i.paths.home), r.ok, &r.message, &r.steps);
        }
        r
    }

    fn remove_worktree(&self, path: &str, dry_run: bool) -> ActionResult {
        let i = &self.inner;
        let Some(report) = lock(&i.disk).report.clone() else {
            return ActionResult { ok: false, message: "Wake hasn't measured the disk yet.".into(), steps: vec![] };
        };
        let owner = report.projects.iter().find(|p| p.items.iter().any(|x| x.path == path)).map(|p| p.project.clone());
        let r = disk::remove_worktree(&report, path, &i.disk_places, dry_run);
        if r.ok && !dry_run {
            i.forget_disk_item(path, owner.as_deref());
        }
        if !dry_run {
            log::record_action(&i.paths, "remove worktree", &tilde(path, &i.paths.home), r.ok, &r.message, &r.steps);
        }
        r
    }

    fn set_session_project(&self, session: &str, project: Option<Option<&str>>) -> Result<(), String> {
        let i = &self.inner;
        if let Some(Some(p)) = project
            && store::project_by_id(p, &i.store.load_projects()).is_none()
        {
            return Err("No project with that id.".into());
        }
        i.overrides.set(&i.store.writer, session, project)?;
        *lock(&i.procs_cache) = None;
        Ok(())
    }

    fn stop(&self, pid: u32, start_ticks: Option<u64>, how: StopHow, dry_run: bool) -> ActionResult {
        let i = &self.inner;
        *lock(&i.live_cache) = None;
        let infos = i.live();
        let projects = i.store.load_projects();
        let title = |info: &LiveInfo| Some(i.title_for(&info.session_id, Some(info), &projects));
        let r = procs::stop(&i.explorer, i.actor.as_ref(), &infos, &title, pid, start_ticks, how, dry_run);
        *lock(&i.live_cache) = None;
        if !dry_run {
            // The task's title and PID and which agent it is: never its command line.
            // Titles can be prompt text: the session's short id instead.
            let info = infos.values().find(|l| l.alive && l.pid == pid);
            let name = info.map_or(format!("PID {pid}"), |l| format!("session {} (PID {pid})", short_id(&l.session_id)));
            let message = info.and_then(title).map_or(r.message.clone(), |t| without(&r.message, &t));
            let agent = procs::read_proc(&i.explorer.root, pid).and_then(|p| procs::identify(&p, &infos)).map(|a| format!(", {a:?}")).unwrap_or_default();
            let what = match how {
                StopHow::Kill => "force",
                _ => "stop",
            };
            log::record_action(&i.paths, what, &format!("{name}{agent}"), r.ok, &message, &r.steps);
        }
        r
    }

    fn screen(&self, session: &str) -> Option<Screen> {
        let i = &self.inner;
        let infos = i.live();
        let info = infos.get(session).filter(|x| x.alive)?.clone();
        // A background session's recent output; a terminal app's own screen can't be read.
        let job = info.job_id.clone()?;
        Some(i.screens.get_or(session, || look::read_logs(&i.runner, &job, crate::vt::term_size(info.pid))))
    }

    fn agent_choices(&self) -> Vec<brief::Agent> {
        let mut v = vec![brief::Agent::Claude];
        if desktop::which("codex").is_some() {
            v.push(brief::Agent::Codex);
        }
        v
    }

    fn brief_command(&self, agent: brief::Agent, id: &str) -> Option<String> {
        brief::command(agent, &brief::path(&self.inner.paths.data, id))
    }

    fn start_agent(&self, req: &brief::StartAgent, dry_run: bool) -> ActionResult {
        let r = self.start_agent_steps(req, dry_run);
        if !dry_run {
            let agent = brief::Agent::from_id(&req.agent).map_or(req.agent.clone(), |a| a.label().to_string());
            log::record_action(&self.inner.paths, "start an agent", &format!("{agent} in {}", self.project_words(&req.project)), r.ok, &r.message, &r.steps);
        }
        r
    }

    fn look_kind(&self) -> LookKind {
        self.inner.look.kind()
    }

    fn look_start(&self, session: &str, dry_run: bool) -> ActionResult {
        let i = &self.inner;
        let infos = i.live();
        let Some(info) = infos.get(session).filter(|x| x.alive) else {
            return i.look.start(None, dry_run);
        };
        // The window showing it, worked out now (it may have moved since the last pass).
        let Some(loc) = i.find_window(info) else {
            return i.look.start(None, dry_run);
        };
        let mut r = i.look.start(Some(&loc), dry_run);
        if !r.ok {
            // The terminal is there but no window holds it (tmux, say): the preview, quietly.
            r.message = "This agent's terminal has no window to go to; showing a preview.".into();
        }
        r
    }

    fn look_end(&self, stay: bool, dry_run: bool) -> ActionResult {
        self.inner.look.end(stay, dry_run)
    }

    fn look_ended(&self) -> Option<bool> {
        self.inner.look.ended()
    }

    fn questions(&self, since: u64) -> Option<QOverview> {
        self.inner.questions.snapshot(since)
    }

    fn ask(&self, thread: Option<&str>, question: &str, project: Option<&str>) -> Result<String, String> {
        let ctx = match thread {
            Some(id) => self.question_context(self.inner.questions.thread_project(id).ok_or("That thread is gone.")?.as_deref())?,
            None => self.question_context(project)?,
        };
        self.inner.questions.ask(thread, question, ctx)
    }

    fn retry_question(&self, thread: &str) -> Result<(), String> {
        let project = self.inner.questions.thread_project(thread).ok_or("That thread is gone.")?;
        self.inner.questions.retry(thread, self.question_context(project.as_deref())?)
    }

    fn cancel_question(&self, thread: &str) -> bool {
        self.inner.questions.cancel(thread)
    }

    fn delete_thread(&self, thread: &str) -> Result<(), String> {
        self.inner.questions.delete(thread)
    }

    fn set_options(&self, o: &crate::settings::CoreOptions) {
        if o.quiet {
            self.set_notifier(Arc::new(|_: &remote::Notice| {}));
        }
        *lock(&self.inner.opts) = *o;
        self.inner.questions.set_ai(o.question_ai);
    }

    fn question_ais(&self) -> Vec<crate::questions::AiChoice> {
        self.inner.questions.choices()
    }

    fn set_project_color(&self, id: &str, color: Option<&str>) -> Result<ProjectSummary, String> {
        let p = self.inner.store.set_project_color(id, color).map_err(|e| e.0)?;
        Ok(self.project_summary_now(&p))
    }

    fn set_next(&self, id: &str, next: &str) -> Result<ProjectSummary, String> {
        let p = self.inner.store.set_project_next(id, next).map_err(|e| e.0)?;
        Ok(self.project_summary_now(&p))
    }

    fn set_project_group(&self, id: &str, group: Option<&str>) -> Result<ProjectSummary, String> {
        let p = self.inner.store.set_project_group(id, group).map_err(|e| e.0)?;
        Ok(self.project_summary_now(&p))
    }

    fn set_project_importance(&self, id: &str, importance: Option<&str>) -> Result<ProjectSummary, String> {
        let p = self.inner.store.set_project_importance(id, importance).map_err(|e| e.0)?;
        Ok(self.project_summary_now(&p))
    }

    fn add_group(&self, name: &str) -> Result<String, String> {
        self.inner.store.add_group(name).map_err(|e| e.0)
    }

    fn remove_group(&self, name: &str) -> Result<usize, String> {
        self.inner.store.remove_group(name).map_err(|e| e.0)
    }

    fn order_groups(&self, order: &[String]) -> Result<(), String> {
        self.inner.store.order_groups(order).map_err(|e| e.0)
    }

    fn check_remotes(&self, project: Option<&str>) {
        lock(&self.inner.remote).asked.push(project.unwrap_or("").to_string());
    }

    fn ack_behind(&self, project: &str) {
        lock(&self.inner.remote).watch.ack(project);
    }

    fn pull(&self, project: &str, dry_run: bool) -> ActionResult {
        let r = self.pull_steps(project, dry_run);
        if !dry_run {
            log::record_action(&self.inner.paths, "pull", &self.project_words(project), r.ok, &r.message, &r.steps);
        }
        r
    }

    fn mark_read(&self, thread: &str) {
        self.inner.questions.mark_read(thread)
    }

    fn note_error(&self, context: &str, message: &str) {
        log::note_error(&self.inner.paths, context, message);
    }
}

impl Live {
    /// A project in words for Wake's log: its name and first folder.
    fn project_words(&self, id: &str) -> String {
        let projects = self.inner.store.load_projects();
        match store::project_by_id(id, &projects) {
            Some(p) => match p.roots.first() {
                Some(r) => format!("{} ({})", p.name, tilde(r, &self.inner.paths.home)),
                None => p.name.clone(),
            },
            None => format!("project {id}"),
        }
    }

    fn start_agent_steps(&self, req: &brief::StartAgent, dry_run: bool) -> ActionResult {
        let i = &self.inner;
        let fail = |m: &str| ActionResult { ok: false, message: m.into(), steps: vec![] };
        let projects = i.store.load_projects();
        let Some(p) = store::project_by_id(&req.project, &projects).filter(|p| !p.removed) else {
            return fail("No tracked project with that id.");
        };
        let Some(agent) = brief::Agent::from_id(&req.agent).filter(|a| self.agent_choices().contains(a)) else {
            return fail("That agent isn't installed here.");
        };
        if req.ideas.is_empty() {
            return fail("Pick at least one idea.");
        }
        let ideas = i.store.list_ideas();
        for id in &req.ideas {
            match ideas.iter().find(|x| &x.id == id) {
                None => return fail("One of those ideas is gone."),
                Some(x) if x.project.as_deref() != Some(p.id.as_str()) => {
                    return fail(&format!("Those ideas aren't all for {}. Start one agent per project.", p.name));
                }
                Some(_) => {}
            }
        }
        if req.brief.trim().is_empty() {
            return fail("The brief is empty.");
        }
        let Some(root) = p.roots.iter().find(|r| std::path::Path::new(r).is_dir()) else {
            return fail(&format!("{}'s folder is no longer there.", p.name));
        };
        let briefs = i.paths.data.join("briefs");
        let path = brief::path(&i.paths.data, &req.id);
        let Some(cmd) = brief::command(agent, &path) else {
            return fail("Wake can't type a command for a brief in this folder (its path has a quote in it).");
        };
        if !dry_run && let Err(e) = brief::write(&i.store.writer, &i.paths.data, &req.id, &req.brief) {
            return fail(&format!("Couldn't save the brief: {}", e.0));
        }
        let mut steps = vec![format!("write {}", path.display())];
        let mut r = i.desktop.start_typed(root, &cmd, &briefs, dry_run);
        steps.append(&mut r.steps);
        r.steps = steps;
        if !r.ok {
            return r;
        }
        if !dry_run && let Err(e) = i.store.mark_sent(&req.ideas, &p.id, Some(&req.id)) {
            r.message = format!("{} (Wake couldn't mark the ideas as sent: {})", r.message, e.0);
            return r;
        }
        let pre = if dry_run { "Dry run. " } else { "" };
        r.message = format!(
            "{pre}{} is starting in a new terminal window in {}.",
            agent.label(),
            match std::path::Path::new(root).strip_prefix(&i.paths.home) {
                Ok(rest) => format!("~/{}", rest.display()),
                Err(_) => root.clone(),
            }
        );
        r
    }

    fn pull_steps(&self, project: &str, dry_run: bool) -> ActionResult {
        let i = &self.inner;
        let refuse = |m: &str| ActionResult { ok: false, message: m.into(), steps: vec![] };
        let projects = i.store.load_projects();
        let Some(p) = store::project_by_id(project, &projects).filter(|p| !p.removed) else {
            return refuse("That project isn't tracked any more.");
        };
        // Decide on what the repo says now, not on what was shown a while ago.
        let (repos, _) = gitinfo::project_git(p, true);
        for x in &repos {
            let prev = lock(&i.remote).checks.get(&x.state.path).cloned();
            if let Some(prev) = prev {
                let c = remote::recheck(&prev);
                lock(&i.remote).checks.insert(x.state.path.clone(), c);
            }
        }
        let Some(b) = i.behind_list(std::slice::from_ref(p)).into_iter().next() else {
            return refuse(&format!("{} isn't behind any more; nothing to pull.", p.name));
        };
        if !b.can_pull {
            return refuse(&if b.ahead > 0 {
                format!("Your copy and {} both have new commits, so Wake won't pull. Ask your agent to merge them, or run `git pull` in a terminal.", b.host)
            } else {
                "Some files have changes that aren't committed yet. Commit them first; Wake only pulls when everything is committed.".to_string()
            });
        }
        let r = remote::pull_ff_only(&b.repo, dry_run);
        if r.ok && !dry_run {
            gitinfo::project_git(p, true);
            let c = remote::check_repo(&b.repo);
            lock(&i.remote).checks.insert(b.repo.clone(), c);
        }
        r
    }

    /// Go to a session; also says what that was (go to / reopen) and to what, for Wake's log.
    /// Go to a session and log it; `may_reopen` false (the plugin's request) never starts an
    /// ended one again.
    fn go_to_logged(&self, session: &str, dry_run: bool, may_reopen: bool) -> ActionResult {
        let (r, what, target) = self.go_to_steps(session, dry_run, may_reopen);
        if !dry_run {
            // Titles can be prompt text: the log gets the session's short id instead.
            let i = &self.inner;
            let title = i.title_for(session, i.live().get(session), &i.store.load_projects());
            log::record_action(&i.paths, what, &target, r.ok, &without(&r.message, &title), &r.steps);
        }
        r
    }

    fn go_to_steps(&self, session: &str, dry_run: bool, may_reopen: bool) -> (ActionResult, &'static str, String) {
        let i = &self.inner;
        let fail = |m: &str| ActionResult { ok: false, message: m.into(), steps: vec![] };
        if session.is_empty() {
            return (fail("Say which session."), "go to", String::new());
        }
        let infos = i.live();
        let info = infos.get(session);
        let projects = i.store.load_projects();
        let acc = i.indexer.get_lite(session);
        if let Some(info) = info.filter(|x| x.alive) {
            // Worked out now, not from the last pass: a second attach is worse than a short wait.
            let loc = i.find_window(info);
            let title = i.title_for(session, Some(info), &projects);
            let target = format!("session {} (PID {})", short_id(session), info.pid);
            let Some(loc) = loc else {
                let Some(job) = info.job_id.as_ref().filter(|j| desktop::plain_id(j)) else {
                    return (fail(&format!("Wake can't tell which window {title} is in. It may be inside another program, such as tmux.")), "go to", target);
                };
                // A background session no window shows: a new window attached to it.
                let cmd = format!("claude attach {job}");
                let cwd = info.cwd.clone().or_else(|| acc.as_ref().and_then(|a| a.launch_cwd.clone()));
                let mut r = i.desktop.reopen(cwd.as_deref(), &cmd, dry_run);
                if r.ok {
                    let pre = if dry_run { "Dry run. " } else { "" };
                    r.message = format!("{pre}{title} runs in the background. Opened it in a new terminal window ({cmd}).");
                }
                return (r, "reopen", target);
            };
            // Straight to the window, by the desktop's own means; never a notification.
            let window = loc.app.as_deref().map_or("its window".to_string(), |a| format!("its {a} window"));
            let b = i.look.bring(loc.pid, dry_run);
            let r = if b.ok {
                let pre = if dry_run { "Dry run. " } else { "" };
                ActionResult { ok: true, message: format!("{pre}Went to {title}, in {window}."), steps: b.steps }
            } else {
                // No desktop link (Wayland lets no app raise another's window): say where it is.
                ActionResult { ok: false, message: format!("{title} is in {window}. This desktop doesn't let Wake bring it forward, so switch to it yourself."), steps: b.steps }
            };
            return (r, "go to", target);
        }
        if !may_reopen {
            return (fail("That session has ended."), "go to", format!("session {}", short_id(session)));
        }
        let Some(acc) = acc else {
            return (fail("No session with that id."), "reopen", format!("session {session}"));
        };
        let cmd = info.and_then(|x| x.resume_command.clone()).unwrap_or_else(|| if acc.agent == "codex" { format!("codex resume {session}") } else { format!("claude --resume {session}") });
        let cwd = acc.launch_cwd.clone().or_else(|| info.and_then(|x| x.cwd.clone()));
        let r = i.desktop.reopen(cwd.as_deref(), &cmd, dry_run);
        let target = match &cwd {
            Some(c) => format!("session {} in {}", short_id(session), tilde(c, &i.paths.home)),
            None => format!("session {}", short_id(session)),
        };
        (r, "reopen", target)
    }

    /// Where a question runs: Wake's questions folder, or the project's first folder.
    fn question_context(&self, project: Option<&str>) -> Result<crate::questions::Context, String> {
        let q = &self.inner.questions;
        let Some(id) = project else { return Ok(crate::questions::Context { project: None, cwd: q.dir() }) };
        let projects = self.inner.store.load_projects();
        let p = store::project_by_id(id, &projects).ok_or("That project isn't tracked any more.")?;
        let root = p.roots.iter().find(|r| Path::new(r).is_dir()).ok_or_else(|| format!("{}'s folder isn't there any more.", p.name))?;
        Ok(crate::questions::Context { project: Some(p.id.clone()), cwd: PathBuf::from(root) })
    }

    fn project_summary_now(&self, p: &ProjectRec) -> ProjectSummary {
        let i = &self.inner;
        let projects = i.store.load_projects();
        i.project_summary(p, &i.all_summaries(&projects, true), &i.store.list_ideas(), &i.store.last_pauses(), util::now())
    }
}

impl Inner {
    /// Agent processes now (Agents view), also kept for the overview's resources.
    fn proc_overview(&self) -> ProcOverview {
        let i = self;
        let infos = i.live();
        let projects = i.store.load_projects();
        let now = util::now();
        let notes = i.notes();
        i.locations_now();
        let view = |info: &LiveInfo| i.session_view(info, &projects, now);
        let locate = |pid: u32, _job: Option<&str>| -> Option<Location> { i.location(pid) };
        let mut ov = i.explorer.processes(&Inputs { live: &infos, view: &view, locate: &locate, notes: &notes, windows_known: i.locations_unsure().is_none(), now });
        i.questions.hider().label_procs(&mut ov);
        *lock(&i.procs_cache) = Some((Instant::now(), ov.clone()));
        ov
    }

    /// Memory and CPU by session and project, from a process scan at most 3 s old.
    fn resources(&self, sessions: &[SessionSummary]) -> Resources {
        let cached = lock(&self.procs_cache).as_ref().filter(|(at, _)| at.elapsed() < Duration::from_secs(3)).map(|(_, o)| o.clone());
        let ov = cached.unwrap_or_else(|| self.proc_overview());
        let project_of = |sid: &str| sessions.iter().find(|s| s.id == sid).and_then(|s| s.project.clone());
        procs::resources(&ov, &project_of)
    }

    /// How each session's project was decided, when not simply by where it worked.
    fn attribution(&self, sessions: &[SessionSummary], projects: &[ProjectRec]) -> Vec<Attribution> {
        let answers = self.overrides.all();
        let mut out = Vec::new();
        for s in sessions {
            let acc = self.indexer.get_lite(&s.id);
            let launch = acc.as_ref().and_then(|a| a.launch_cwd.clone()).or_else(|| s.cwd.clone());
            let how = if answers.contains_key(&s.id) {
                "set"
            } else {
                match acc.as_ref().map(|a| index::attribute_how(a, projects, &self.paths.home).2) {
                    Some(Some(index::How::Worked)) => continue,
                    Some(Some(h)) => h.word(),
                    _ if s.project.is_some() => continue, // a live-only session placed by its folder
                    _ => "none",
                }
            };
            let proj = s.project.as_deref().zip(s.project_name.as_deref());
            out.push(attrib::describe(&s.id, proj, how, launch.as_deref(), &self.paths.home));
        }
        out
    }

    /// The last disk report, with refusals brought up to date (who is working now).
    fn disk_report(&self, sessions: &[SessionSummary]) -> Option<DiskReport> {
        let mut r = lock(&self.disk).report.clone()?;
        for p in &mut r.projects {
            let busy = sessions.iter().any(|s| s.state == State::Working && s.project.as_deref() == Some(p.project.as_str()));
            for it in p.items.iter_mut().filter(|it| it.action == Some(DiskAction::Clear)) {
                if busy {
                    it.refused = Some("An agent in this project is working; it may be building into this. Try when it's idle.".into());
                } else if it.refused.as_deref().is_some_and(|w| w.starts_with("An agent in")) {
                    it.refused = None;
                }
            }
        }
        Some(r)
    }

    /// What disk measuring needs to know about sessions and live agents.
    fn disk_inputs(&self, projects: &[ProjectRec]) -> (Vec<disk::SessionDisk>, Vec<disk::LiveDisk>) {
        let sums = self.all_summaries(projects, false);
        let by_id: HashMap<&str, &SessionSummary> = sums.iter().map(|s| (s.id.as_str(), s)).collect();
        let answers = self.overrides.all();
        let mut sessions = Vec::new();
        for acc in self.indexer.all_lite() {
            let project = match answers.get(&acc.id) {
                Some(choice) => choice.clone(),
                None => by_id.get(acc.id.as_str()).and_then(|s| s.project.clone()).or_else(|| index::attribute(&acc, projects, &self.paths.home).0),
            };
            let mut folders: Vec<String> = acc.cwd_seen.iter().map(|(k, _)| k.clone()).collect();
            folders.extend(acc.worktrees.iter().map(|(k, _)| k.clone()));
            sessions.push(disk::SessionDisk {
                id: acc.id.clone(),
                project,
                title: index::title_of(&acc),
                started_at: acc.started_at.clone(),
                transcript: (acc.agent != "codex").then(|| acc.source.clone().map(PathBuf::from)).flatten(),
                folders,
                working: by_id.get(acc.id.as_str()).is_some_and(|s| s.state == State::Working),
            });
        }
        let live = self
            .live()
            .values()
            .filter(|l| l.alive)
            .map(|l| disk::LiveDisk { pid: l.pid, session: l.session_id.clone(), title: by_id.get(l.session_id.as_str()).map(|s| s.title.clone()).unwrap_or_default() })
            .collect();
        (sessions, live)
    }

    /// Measure (everything, or one project) and keep the result, in memory and in disk.json.
    fn measure_disk(&self, only: Option<&str>) {
        let projects = self.store.load_projects();
        let (sessions, live) = self.disk_inputs(&projects);
        let last = lock(&self.disk).report.clone();
        let run = disk::real_runner();
        let progress = |r: &DiskReport| lock(&self.disk).report = Some(r.clone());
        let report = disk::measure(&projects, only, &sessions, &live, &self.disk_places, &*run, last.as_ref(), &progress);
        let _ = self.store.writer.write_json(&self.paths.data.join("disk.json"), &report);
        lock(&self.disk).report = Some(report);
    }

    /// After a Clear or a Remove: drop the item from the report at once (the next measurement
    /// confirms), and measure that project again soon.
    fn forget_disk_item(&self, path: &str, project: Option<&str>) {
        let mut d = lock(&self.disk);
        if let Some(r) = d.report.as_mut() {
            let gone = |i: &DiskItem| i.path == path || i.within.as_deref() == Some(path);
            for p in &mut r.projects {
                let freed: u64 = p.items.iter().filter(|i| gone(i) && i.within.as_deref() != Some(path)).map(|i| i.bytes).sum();
                let rebuilt: u64 = p.items.iter().filter(|i| gone(i) && matches!(i.kind, DiskKind::BuildOutput | DiskKind::Cache)).map(|i| i.bytes).sum();
                p.items.retain(|i| !gone(i));
                p.bytes = p.bytes.saturating_sub(freed);
                p.rebuildable = p.rebuildable.saturating_sub(rebuilt);
            }
            if let Some(m) = r.machine.as_mut() {
                let rebuilt: u64 = m.items.iter().filter(|i| i.path == path).map(|i| i.bytes).sum();
                m.items.retain(|i| i.path != path);
                m.rebuildable = m.rebuildable.saturating_sub(rebuilt);
            }
            r.rebuildable = r.projects.iter().map(|p| p.rebuildable).sum::<u64>() + r.machine.as_ref().map_or(0, |m| m.rebuildable);
        }
        if let Some(p) = project {
            d.asked.push(p.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    /// On this machine, read only: where a peek at each live session would fly (a dry run, so
    /// nothing moves). `cargo test -p wake-core real_peek_targets -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_peek_targets() {
        let l = Live::new(LiveConfig::real());
        l.inner.refresh_locations();
        for (id, info) in l.inner.live().iter().filter(|(_, i)| i.alive) {
            let r = l.look_start(id, true);
            let pid = r.steps.iter().find_map(|s| s.rsplit(' ').next().filter(|_| s.contains(".Start")).map(str::to_string));
            let what = pid.as_deref().and_then(|p| std::fs::read_to_string(format!("/proc/{p}/comm")).ok()).unwrap_or_default();
            println!("{} pid {} job {:?}: ok {} -> {:?} {} | {}", &id[..8], info.pid, info.job_id, r.ok, pid, what.trim(), r.message);
        }
    }

    use super::*;
    use crate::desktop::tests::{FakeBus, launcher};
    use crate::procs::tests::{FakeActor, FakeProc};
    use crate::store::testenv::Env;
    use std::fs;

    /// When the fake session process (this test's pid) started, in clock ticks.
    const START: u64 = 777;

    /// A fake /proc holding the session process (this test's pid), so nothing reads the real
    /// desktop: not even the terminal the tests run in.
    fn fake_proc() -> FakeProc {
        let fp = FakeProc::new();
        fp.add(std::process::id(), 1, &["/usr/bin/claude"], "/usr/bin/claude", 1000, 1, START, procs::my_uid());
        fp
    }

    fn live_with(env: &Env, fp: &FakeProc, runner: Runner, actor: Arc<FakeActor>) -> Live {
        Live::new(LiveConfig { paths: env.paths.clone(), proc_root: fp.root().to_path_buf(), runner, systemd_run: Some(String::new()), actor, terminal: Some(launcher()) })
    }

    fn make(env: &Env, bus: &FakeBus) -> (Live, Arc<FakeActor>, FakeProc) {
        let fp = fake_proc();
        let actor = Arc::new(FakeActor { calls: Mutex::new(vec![]) });
        let l = live_with(env, &fp, bus.runner(), actor.clone());
        l.index_now();
        (l, actor, fp)
    }

    /// Process `pid` reads its terminal from /dev/pts/`n`, whose other end terminal app `term`
    /// holds (a fake /proc's fd and fdinfo entries).
    fn in_window(fp: &FakeProc, pid: u32, n: u32, term: u32) {
        let root = fp.root();
        fs::create_dir_all(root.join(format!("{pid}/fd"))).unwrap();
        let _ = fs::remove_file(root.join(format!("{pid}/fd/0")));
        std::os::unix::fs::symlink(format!("/dev/pts/{n}"), root.join(format!("{pid}/fd/0"))).unwrap();
        if !root.join(term.to_string()).exists() {
            fp.add(term, 1, &["/usr/bin/some-terminal"], "/usr/bin/some-terminal", 1000, 1, 5, procs::my_uid());
        }
        fs::create_dir_all(root.join(format!("{term}/fd"))).unwrap();
        fs::create_dir_all(root.join(format!("{term}/fdinfo"))).unwrap();
        let fd = 100 + n;
        let _ = fs::remove_file(root.join(format!("{term}/fd/{fd}")));
        std::os::unix::fs::symlink("/dev/ptmx", root.join(format!("{term}/fd/{fd}"))).unwrap();
        fs::write(root.join(format!("{term}/fdinfo/{fd}")), format!("pos:\t0\nflags:\t02\ntty-index:\t{n}\n")).unwrap();
    }

    fn write_session(env: &Env, sid: &str, extra: Value) {
        let pid = std::process::id();
        let mut rec = serde_json::json!({"pid": pid, "sessionId": sid, "status": "idle",
            "procStart": START.to_string()});
        if let (Some(o), Some(e)) = (rec.as_object_mut(), extra.as_object()) {
            for (k, v) in e {
                o.insert(k.clone(), v.clone());
            }
        }
        fs::write(env.paths.claude.join("sessions").join(format!("{pid}.json")), rec.to_string()).unwrap();
    }

    #[test]
    fn overview_shape() {
        let env = Env::new();
        let (l, _, _fp) = make(&env, &FakeBus::new());
        let o = l.overview();
        assert_eq!(o.indexing.done, o.indexing.total);
        // the fixtures are dated 2026-11-19; recent while the tests run near then, so check by id
        let all = l.inner.all_summaries(&l.inner.store.load_projects(), false);
        let ids: Vec<&str> = all.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids[0], "0193c2aa-0000-7c93-9c1d-5e2f8a0b4c17");
        let s = all.iter().find(|s| s.id == "sess-aaaa").unwrap();
        assert_eq!(s.state, State::Ended);
        let p = o.projects.iter().find(|p| p.id == "alpha").unwrap();
        assert_eq!(p.name, "alpha");
        assert_eq!(p.last_summary.as_deref().filter(|_| p.last_at.is_some()), p.last_summary.as_deref());
        // the summary serialises with the contract's names
        let v = serde_json::to_value(s).unwrap();
        for k in ["id", "agent", "project", "title", "state", "evidence", "subagents", "resume_command", "deleted_by_claude"] {
            assert!(v.get(k).is_some(), "{k}");
        }
    }

    #[test]
    fn last_summary_comes_from_the_newest_recap() {
        let env = Env::new();
        let (l, _, _fp) = make(&env, &FakeBus::new());
        let projects = l.inner.store.load_projects();
        let all = l.inner.all_summaries(&projects, false);
        let alpha = store::project_by_id("alpha", &projects).unwrap();
        let ps = l.inner.project_summary(alpha, &all, &[], &HashMap::new(), util::parse_iso("2026-11-20T00:00:00Z").unwrap());
        assert_eq!(ps.last_summary.as_deref(), Some("You tidied the parser; tests pass."));
        assert_eq!(ps.sessions_14d, 1);
    }

    #[test]
    fn session_subagent_search() {
        let env = Env::new();
        let (l, _, _fp) = make(&env, &FakeBus::new());
        let d = l.session("sess-aaaa").unwrap();
        assert!(d.transcript.as_deref().is_some_and(|t| t.ends_with("/projects/-home/sess-aaaa.jsonl")), "{:?}", d.transcript);
        assert_eq!(d.subagents.len(), 2);
        assert_eq!(d.summary.subagents.total, 2);
        assert_eq!(l.subagent("sess-aaaa", "a1").unwrap().steps.len(), 3);
        assert!(l.session("nope").is_none());
        let r = l.search("parser.py");
        assert_eq!(r.iter().map(|(s, _)| s.id.as_str()).collect::<Vec<_>>(), ["sess-aaaa"]);
        assert!(r[0].1.starts_with("file: ") && r[0].1.contains("parser.py"), "{}", r[0].1);
        assert!(l.search("x").is_empty());
    }

    #[test]
    fn ideas_pause_projects() {
        let env = Env::new();
        let (l, _, _fp) = make(&env, &FakeBus::new());
        let i = l.add_idea("try a dark theme", Some("alpha")).unwrap();
        assert_eq!((i.project_name.as_deref(), i.state.as_str()), (Some("alpha"), "inbox"));
        assert_eq!(l.set_idea(&i.id, Some("done"), None).unwrap().state, "done");
        assert_eq!(l.ideas()[0].state, "done");
        assert!(l.add_idea("", None).is_err());
        assert!(l.add_idea("x", Some("nope")).is_err());
        let cp = l.pause("alpha", "stopping", "parser").unwrap();
        assert!(cp.sessions.contains(&"sess-aaaa".to_string()));
        let pd = l.project("alpha").unwrap();
        assert_eq!(pd.last_pause.unwrap().note, "stopping");
        assert_eq!(pd.summary.paused.unwrap().note, "stopping");
        // The next step is kept on the project too, and a later pause without one leaves it.
        assert_eq!(pd.summary.next, "parser");
        l.pause("alpha", "again", "").unwrap();
        assert_eq!(l.project("alpha").unwrap().summary.next, "parser");
        // set_next changes the project only and adds no pause.
        let pauses = l.inner.store.list_pauses(Some("alpha")).len();
        assert_eq!(l.set_next("alpha", " write the docs ").unwrap().next, "write the docs");
        assert_eq!(l.inner.store.list_pauses(Some("alpha")).len(), pauses);
        assert!(l.set_next("nope", "x").is_err());
        assert!(l.remove_project("alpha", true).unwrap().removed);
        assert!(!l.remove_project("alpha", false).unwrap().removed);
        let nd = env.paths.home.join("elsewhere");
        fs::create_dir_all(&nd).unwrap();
        assert_eq!(l.add_project(&nd.to_string_lossy()).unwrap().id, "elsewhere");
        assert!(l.add_project("/no/such").is_err());
    }

    #[test]
    fn starting_an_agent_writes_the_brief_types_the_command_and_marks_the_ideas_sent() {
        let env = Env::new();
        let bus = FakeBus::new();
        let (l, _, _fp) = make(&env, &bus);
        let a = l.add_idea("say \"hi\" for $5 `now`", Some("alpha")).unwrap();
        let b = l.add_idea("second", Some("alpha")).unwrap();
        let other = l.add_idea("elsewhere", Some("beta")).unwrap();
        let loose = l.add_idea("unsorted", None).unwrap();
        let id = brief::new_id();
        let text = brief::brief_text("alpha", &[&a.text, &b.text]);
        let req = |ideas: Vec<String>| brief::StartAgent { project: "alpha".into(), ideas, brief: text.clone(), agent: "claude".into(), id: id.clone() };
        // Mixing projects, or an idea with none, is refused before anything happens.
        for ideas in [vec![a.id.clone(), other.id.clone()], vec![loose.id.clone()]] {
            let r = l.start_agent(&req(ideas), false);
            assert!(!r.ok && r.message.contains("aren't all for alpha"), "{r:?}");
        }
        assert!(!l.start_agent(&brief::StartAgent { agent: "gemini".into(), ..req(vec![a.id.clone()]) }, false).ok);
        assert!(bus.writes().is_empty());
        let path = brief::path(&env.paths.data, &id);
        // A dry run says what it would do and writes nothing.
        let r = l.start_agent(&req(vec![a.id.clone(), b.id.clone()]), true);
        assert!(r.ok, "{r:?}");
        assert!(!path.exists() && bus.writes().is_empty());
        assert_eq!(l.ideas().iter().filter(|i| i.state == "sent").count(), 0);
        // For real: the brief file, a new window running the command, the ideas sent.
        let r = l.start_agent(&req(vec![a.id.clone(), b.id.clone()]), false);
        assert!(r.ok, "{r:?}");
        assert_eq!(fs::read_to_string(&path).unwrap(), format!("{text}\n"));
        let cmd = l.brief_command(brief::Agent::Claude, &id).unwrap();
        let started: Vec<Vec<String>> = bus.writes().into_iter().filter(|c| c[0] == "setsid").collect();
        assert_eq!(started.len(), 1);
        let alpha = env.paths.projects.join("alpha").to_string_lossy().to_string();
        assert_eq!(started[0][2..5], [launcher()[0].as_str(), "/bin/sh", "-c"], "a window of the default terminal app");
        assert!(started[0][5].contains(&format!("; cd {} && {cmd}; exec ", util::shell_quote(&alpha))), "the window opens in the project's folder: {:?}", started[0]);
        let ideas = l.ideas();
        for x in [&a, &b] {
            let i = ideas.iter().find(|i| i.id == x.id).unwrap();
            assert_eq!((i.state.as_str(), i.project.as_deref()), ("sent", Some("alpha")));
            assert!(i.sent_at.is_some());
            assert_eq!(i.brief.as_deref(), Some(id.as_str()), "the idea leads to its brief");
        }
        assert_eq!(ideas.iter().find(|i| i.id == other.id).unwrap().state, "inbox");
        assert!(log::recent(&env.paths, 5).iter().any(|l| l.contains("  start an agent  Claude in alpha (~/Projects/alpha)  ok  ")), "{:?}", log::recent(&env.paths, 5));
        assert_eq!(l.overview().projects.iter().find(|p| p.id == "alpha").unwrap().ideas, 0, "sent ideas leave the count to sort");
    }

    #[test]
    fn go_to_goes_to_the_window_or_reopens() {
        let env = Env::new();
        let bus = FakeBus::new();
        let (l, _, fp) = make(&env, &bus);
        // ended -> reopen in the launch folder with the resume command, dry run
        let r = l.go_to("sess-aaaa", true);
        assert!(r.ok, "{r:?}");
        assert!(r.steps[0].starts_with("setsid -f /usr/bin/xdg-terminal-exec /bin/sh -c") && r.steps[0].contains("claude --resume sess-aaaa; exec "), "{:?}", r.steps);
        assert!(r.steps[0].contains(&env.paths.home.to_string_lossy().to_string()));
        assert!(!l.go_to("nope", true).ok);
        // live, background, no window -> a new window running `claude attach <job>`
        write_session(&env, "sess-aaaa", serde_json::json!({"jobId": "jobtest01", "cwd": env.paths.home.to_string_lossy()}));
        *lock(&l.inner.live_cache) = None;
        let r = l.go_to("sess-aaaa", true);
        assert!(r.ok, "{r:?}");
        assert!(r.message.contains("runs in the background") && r.message.contains("new terminal window"), "{r:?}");
        assert!(r.steps[0].contains("claude attach jobtest01; exec "), "{:?}", r.steps);
        // live in a terminal window -> that window; this desktop has no link, so Wake says where
        in_window(&fp, std::process::id(), 7, 3264);
        let r = l.go_to("sess-aaaa", true);
        assert!(!r.ok && r.message.contains("is in its window") && r.message.contains("switch to it yourself"), "{r:?}");
        assert!(bus.writes().is_empty());
        assert!(log::recent(&env.paths, 5).is_empty(), "a dry run was logged");
        // a real one is logged with the task and its PID
        let r = l.go_to("sess-aaaa", false);
        let lines = log::recent(&env.paths, 5);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("  go to  session sess-aaa (PID ") && lines[0].contains(&format!("(PID {})  {}", std::process::id(), if r.ok { "ok" } else { "failed" })), "{lines:?}");
        let title = l.inner.title_for("sess-aaaa", None, &l.inner.store.load_projects());
        assert!(!lines[0].contains(&title), "the title stays out of the log: {lines:?}");
        assert!(!bus.writes().iter().any(|c| c[0] == "setsid"), "never a second window for a session that has one");
    }

    #[test]
    fn a_running_codex_session_is_never_reopened() {
        let env = Env::new();
        let bus = FakeBus::new();
        let (l, _, fp) = make(&env, &bus);
        let sid = "0193c2aa-0000-7c93-9c1d-5e2f8a0b4c17";
        // Ended: reopened with `codex resume`.
        let r = l.go_to(sid, true);
        assert!(r.steps.iter().any(|s| s.contains(&format!("codex resume {sid}"))), "{r:?}");
        assert_eq!(l.session(sid).unwrap().summary.pid, None);
        // Running: a codex process holds its rollout file open.
        let pid = 5300;
        fp.add(pid, 1, &["/usr/bin/codex"], "/usr/bin/codex", 1000, 1, 5, procs::my_uid());
        let rollout = env.paths.codex.join(format!("sessions/2026/11/19/rollout-2026-11-19T11-00-00-{sid}.jsonl"));
        fs::create_dir_all(fp.root().join(format!("{pid}/fd"))).unwrap();
        std::os::unix::fs::symlink(&rollout, fp.root().join(format!("{pid}/fd/3"))).unwrap();
        *lock(&l.inner.live_cache) = None;
        assert_eq!(l.session(sid).unwrap().summary.pid, Some(pid), "a live session carries its pid");
        let r = l.go_to(sid, true);
        assert!(!r.ok && r.message.contains("can't tell which window"), "{r:?}");
        assert!(!r.steps.iter().any(|s| s.contains("codex resume")), "{r:?}");
        // In a terminal window: that window, never a second `codex resume`.
        in_window(&fp, pid, 4, 3300);
        let r = l.go_to(sid, true);
        assert!(r.message.contains("is in its window") || r.message.contains("Went to"), "{r:?}");
        assert!(!bus.writes().iter().any(|c| c[0] == "setsid"));
    }

    #[test]
    fn every_agent_is_placed_in_the_window_of_its_terminal_whatever_the_app() {
        let env = Env::new();
        let (l, _, fp) = make(&env, &FakeBus::new());
        assert!(l.inner.locations_unsure().unwrap().contains("still finding"));
        l.inner.refresh_locations();
        assert_eq!(l.inner.locations_unsure(), None);
        write_session(&env, "sess-aaaa", serde_json::json!({"jobId": "jobtest01"}));
        *lock(&l.inner.live_cache) = None;
        // A background session: its own terminal is its host's (an agent), so no window yet.
        let host = 5100;
        fp.add(host, 1, &["/usr/bin/claude", "--bg-pty-host"], "/usr/bin/claude", 1000, 1, 5, procs::my_uid());
        in_window(&fp, std::process::id(), 3, host);
        l.inner.refresh_locations();
        assert_eq!(l.inner.location(std::process::id()), None, "a background host is no window");
        // Attached in some terminal app: the session is in that app's window.
        let client = 5200;
        fp.add(client, 1, &["/usr/bin/claude", "attach", "jobtest01"], "/usr/bin/claude", 1000, 1, 5, procs::my_uid());
        in_window(&fp, client, 9, 6000);
        l.inner.refresh_locations();
        assert_eq!(l.inner.location(std::process::id()).map(|x| x.pid), Some(6000));
        assert_eq!(l.inner.location(client).map(|x| x.pid), Some(6000), "the client's own row says so too");
        assert_eq!(l.inner.find_window(&l.inner.live()["sess-aaaa"]).map(|x| x.pid), Some(6000));
    }

    #[test]
    fn stopping_is_logged_without_the_processs_command_line() {
        use crate::procs::tests::FakeProc;
        let env = Env::new();
        let fp = FakeProc::new();
        fp.add(4321, 1, &["/usr/bin/claude", "--api-key", "sk-SECRET-123", "do the thing"], "/usr/bin/claude", 1000, 1, 100, procs::my_uid());
        let bus = FakeBus::new();
        let actor = Arc::new(FakeActor { calls: Mutex::new(vec![]) });
        let l = live_with(&env, &fp, bus.runner(), actor.clone());
        assert!(l.stop(4321, None, StopHow::Kill, true).ok);
        assert!(log::recent(&env.paths, 5).is_empty(), "a dry run was logged");
        let r = l.stop(4321, None, StopHow::Terminate, false);
        assert!(r.ok, "{r:?}");
        assert_eq!(actor.calls.lock().unwrap().len(), 1);
        let lines = log::recent(&env.paths, 5);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("  stop  PID 4321, Claude  ok  Wake asked PID 4321 to exit."), "{}", lines[0]);
        assert!(lines[0].ends_with("steps: kill -TERM 4321"), "{}", lines[0]);
        let all = fs::read_to_string(log::path(&env.paths)).unwrap();
        assert!(!all.contains("SECRET") && !all.contains("do the thing") && !all.contains("api-key"), "{all}");
    }

    #[test]
    fn screen_reads_the_background_logs_and_never_an_ended_session() {
        let env = Env::new();
        let bus = FakeBus::new();
        let inner = bus.runner();
        let seen: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(vec![]));
        let seen2 = seen.clone();
        // `claude logs` is faked here; everything else is the fake bus.
        let runner: Runner = Arc::new(move |argv: &[String], t: Duration| {
            seen2.lock().unwrap().push(argv.to_vec());
            if argv[0] == "claude" {
                return (0, "\u{1b}[32mworking on it\u{1b}[0m\n".into());
            }
            inner(argv, t)
        });
        let fp = fake_proc();
        let l = live_with(&env, &fp, runner, Arc::new(FakeActor { calls: Mutex::new(vec![]) }));
        l.index_now();
        assert_eq!(l.screen("sess-aaaa"), None, "an ended session has no screen");
        write_session(&env, "sess-aaaa", serde_json::json!({}));
        *lock(&l.inner.live_cache) = None;
        assert_eq!(l.screen("sess-aaaa"), None, "a terminal app's own screen can't be read");
        write_session(&env, "sess-aaaa", serde_json::json!({"jobId": "jobtest01"}));
        *lock(&l.inner.live_cache) = None;
        let s = l.screen("sess-aaaa").unwrap();
        assert_eq!(s.lines, vec!["working on it"]);
        assert_eq!(s.source, "claude logs");
        assert_eq!(l.screen("sess-aaaa").unwrap(), s, "a second poll within 250 ms shares the read");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.iter().filter(|c| c[0] == "claude").count(), 1);
        assert!(seen.iter().all(|c| c[0] != "claude" || c[1] == "logs"), "no other claude command, ever");
        assert!(bus.writes().is_empty());
    }

    #[test]
    fn hides_unused_sessions_and_shows_named_live_ones() {
        let env = Env::new();
        let (l, _, _fp) = make(&env, &FakeBus::new());
        write_session(&env, "brand-new-1234", serde_json::json!({"name": "brand-new", "cwd": "/"}));
        *lock(&l.inner.live_cache) = None;
        let all = l.inner.all_summaries(&l.inner.store.load_projects(), true);
        assert!(all.iter().all(|s| s.id != "brand-new-1234")); // name is its own id prefix: unused
        write_session(&env, "brand-new-1234", serde_json::json!({"name": "fix-the-parser", "cwd": "/"}));
        *lock(&l.inner.live_cache) = None;
        let all = l.inner.all_summaries(&l.inner.store.load_projects(), true);
        let s = all.iter().find(|s| s.id == "brand-new-1234").unwrap();
        assert_eq!(s.title, "fix-the-parser");
        assert_eq!(s.state, State::Idle);
    }

    #[test]
    fn card_and_plugin_request() {
        let env = Env::new();
        let bus = FakeBus::new();
        let (l, _, _fp) = make(&env, &bus);
        // A live background session: on the card, and a jump opens a window attached to it.
        write_session(&env, "sess-aaaa", serde_json::json!({"jobId": "jobtest01"}));
        assert!(l.plugin_tick(true).is_none());
        let card: Value = store::read_json(&env.paths.data.join("card/card.json")).unwrap();
        for k in ["working", "waiting", "idle", "ideas", "top", "sessions", "updatedAt"] {
            assert!(card.get(k).is_some(), "{k}");
        }
        fs::create_dir_all(&env.paths.plugin_state).unwrap();
        let req = env.paths.plugin_state.join("request.json");
        let ask = |id: &str, session: &str, at: &str| fs::write(&req, serde_json::json!({"id": id, "action": "jump", "session": session, "at": at}).to_string()).unwrap();
        // An ended session is never reopened for the plugin.
        ask("r0", "sess-bbbb", &util::now_iso());
        assert!(l.plugin_tick(true).is_none());
        ask("r1", "sess-aaaa", &util::now_iso());
        let r = l.plugin_tick(true).expect("acted on the request");
        assert!(r.ok && r.message.starts_with("Dry run."), "{r:?}");
        assert_eq!(fs::read_to_string(env.paths.data.join("card/last-request")).unwrap(), "r1");
        // At most one jump every 3 s.
        ask("r1b", "sess-aaaa", &util::now_iso());
        assert!(l.plugin_tick(true).is_none());
        lock(&l.inner.plugin).2 = None;
        // the same request again (touched) is ignored; an old one or one from the future is
        // recorded but not acted on
        fs::write(&req, serde_json::json!({"id": "r1b", "action": "jump", "session": "sess-aaaa", "at": util::now_iso()}).to_string() + " ").unwrap();
        assert!(l.plugin_tick(true).is_none());
        ask("r2", "sess-aaaa", "2020-01-01T00:00:00Z");
        assert!(l.plugin_tick(true).is_none());
        ask("r3", "sess-aaaa", &util::iso_from_epoch(util::now() + 60.0));
        assert!(l.plugin_tick(true).is_none());
        // A request that is a symlink is never read.
        let real = env.paths.plugin_state.join("elsewhere.json");
        fs::write(&real, serde_json::json!({"id": "r4", "action": "jump", "session": "sess-aaaa", "at": util::now_iso()}).to_string()).unwrap();
        fs::remove_file(&req).unwrap();
        std::os::unix::fs::symlink(&real, &req).unwrap();
        assert!(l.plugin_tick(true).is_none());
        assert!(bus.writes().is_empty());
    }

    #[test]
    fn processes_and_stop_never_act_in_dry_run() {
        let env = Env::new();
        let (l, actor, _fp) = make(&env, &FakeBus::new());
        let o = l.processes();
        assert!(o.procs.iter().all(|p| p.pid != std::process::id()));
        let r = l.stop(std::process::id(), None, StopHow::Kill, true);
        assert!(!r.ok);
        let r = l.stop(1, None, StopHow::Terminate, false);
        assert!(!r.ok);
        assert!(actor.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn a_broken_project_list_is_reported_in_the_overview() {
        let env = Env::new();
        let (l, _, _fp) = make(&env, &FakeBus::new());
        fs::write(env.paths.data.join("projects.json"), "{\"projects\": [").unwrap();
        let o = l.overview();
        assert!(o.index_error.as_deref().is_some_and(|e| e.contains("project list was damaged, so it started a new one")), "{:?}", o.index_error);
        assert!(!o.projects.is_empty());
    }

    #[test]
    fn building_while_behind_alarms_and_notifies_once_through_the_fake() {
        use crate::remote::tests::{commit, g, trio};
        let env = Env::new();
        let bus = FakeBus::new();
        let (l, _, _fp) = make(&env, &bus);
        let sent: Arc<Mutex<Vec<remote::Notice>>> = Arc::default();
        let s2 = sent.clone();
        l.set_notifier(Arc::new(move |n: &remote::Notice| s2.lock().unwrap().push(n.clone())));
        let t = trio();
        let here = t.here.to_string_lossy().into_owned();
        let p = l.add_project(&here).unwrap();
        // Someone pushes; Wake asks the remote (ls-remote against the scratch bare repo).
        commit(&t.other, "b.txt", "b\n");
        g(&t.other, &["push", "-q"]);
        l.check_remotes(Some(&p.id));
        l.inner.check_remotes_pass();
        let b = l.overview().behind.unwrap().into_iter().find(|b| b.project == p.id).unwrap();
        assert_eq!(b.count, None);
        assert!(b.can_pull && b.alarm.is_none());
        assert_eq!(b.line, "Someone pushed new commits to the remote that you don't have yet. Pull them before you start.");
        // Calm while idle: the first look is the baseline.
        l.inner.watch_behind();
        l.inner.watch_behind();
        assert!(sent.lock().unwrap().is_empty());
        // A session starts working in it: one alarm, one notification.
        write_session(&env, "build-1", serde_json::json!({"name": "fix-the-build", "cwd": here, "status": "busy", "statusUpdatedAt": (util::now() * 1000.0) as i64}));
        *lock(&l.inner.live_cache) = None;
        l.inner.watch_behind();
        let b = l.overview().behind.unwrap().into_iter().find(|b| b.project == p.id).unwrap();
        assert_eq!(b.alarm.as_deref(), Some("a session started working"));
        assert_eq!(sent.lock().unwrap().len(), 1);
        assert!(sent.lock().unwrap()[0].body.contains("press Pull"));
        // Files change too: still the one notification.
        std::fs::write(t.here.join("new.txt"), "n").unwrap();
        l.inner.watch_behind();
        assert_eq!(sent.lock().unwrap().len(), 1);
        // Acknowledged: the alarm goes, and the marker stays.
        l.ack_behind(&p.id);
        let b = l.overview().behind.unwrap().into_iter().find(|b| b.project == p.id).unwrap();
        assert!(b.alarm.is_none());
        assert!(!b.can_pull, "an untracked file makes the tree not clean");
        assert!(!l.pull(&p.id, true).ok);
        std::fs::remove_file(t.here.join("new.txt")).unwrap();
        assert!(l.pull(&p.id, true).ok);
        assert!(!t.here.join("b.txt").exists());
        // Pulled: caught up, the episode ends.
        let r = l.pull(&p.id, false);
        assert!(r.ok, "{r:?}");
        l.inner.watch_behind();
        assert!(l.overview().behind.unwrap().iter().all(|b| b.project != p.id));
        assert_eq!(sent.lock().unwrap().len(), 1);
        assert!(bus.writes().is_empty(), "no real D-Bus call went out");
    }
}
