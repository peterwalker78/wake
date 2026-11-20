//! Where the app's data comes from.

use std::sync::Arc;

use wake_core::Source;

/// THE SEAM: the real source for this machine. `wake_core::Live` has landed, so this is it; to run
/// the window on made-up data instead (as before it landed), return `demo_source()` here and make
/// `is_stand_in` return true so the window says so.
pub fn live_source() -> Arc<dyn Source> {
    // `start` runs the index, Konsole and git threads; without them the window only sees live sessions.
    let live = Arc::new(wake_core::Live::start());
    // Feed the Slipstream card (card.json) and act on its "go there" presses, once a second.
    let tick = Arc::downgrade(&live);
    let _ = std::thread::Builder::new().name("wake-plugin".into()).spawn(move || {
        while let Some(l) = tick.upgrade() {
            let _ = l.plugin_tick(false);
            drop(l);
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    });
    live
}

/// True while `live_source` is a stand-in for the real thing, so the window shows "demo data".
pub fn is_stand_in() -> bool {
    false
}

/// `wake --demo`: the made-up machine in `demo.rs`.
pub fn demo_source() -> Arc<dyn Source> {
    Arc::new(crate::demo::DemoSource::new())
}
