//! Where the app's data comes from.

use std::sync::Arc;

use wake_core::Source;

/// The real source for this machine. `quiet` (screenshots) is set before anything starts: no
/// plugin card, no notifications.
pub fn live_source(quiet: bool) -> Arc<dyn Source> {
    let live = Arc::new(wake_core::Live::new(wake_core::LiveConfig::real()));
    if quiet {
        live.set_options(&wake_core::settings::CoreOptions { quiet: true, remote_every: None, notify_behind: false, ..Default::default() });
    }
    // The index, windows and git threads; without them the window only sees live sessions.
    live.start_background();
    if !quiet {
        live.start_plugin();
    }
    live
}

/// `wake --demo`: the made-up machine in `demo.rs`.
pub fn demo_source() -> Arc<dyn Source> {
    Arc::new(crate::demo::DemoSource::new())
}
