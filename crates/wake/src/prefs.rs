//! The person's settings while Wake runs: read once at start from ~/.config/wake/settings.toml
//! (wake-core's `settings`), changed by the Settings view, written back off the UI thread.
//!
//! A demo or a screenshot never writes the file. For screenshots, `WAKE_THEME`, `WAKE_ACCENT` and
//! `WAKE_DENSITY` override what the file says, for that run only.

use std::cell::{Cell, RefCell};

use gtk4::{gio, glib};
use wake_core::settings::{self, Settings};

use crate::app::{Ui, View};
use crate::sort::{self, Sort};

thread_local! {
    static S: RefCell<Settings> = RefCell::new(Settings::default());
    static NOTE: RefCell<Option<String>> = const { RefCell::new(None) };
    static SAVES: Cell<bool> = const { Cell::new(false) };
}

fn dir() -> std::path::PathBuf {
    wake_core::store::Paths::from_env().config
}

/// Reads the settings file; `saves` is false for a demo or a screenshot.
pub fn init(saves: bool) {
    let (mut s, note) = settings::load(&dir());
    for (var, field, allowed) in [("WAKE_THEME", 0, &settings::THEMES[..]), ("WAKE_ACCENT", 1, &settings::ACCENTS[..])] {
        if let Ok(v) = std::env::var(var)
            && allowed.contains(&v.as_str())
        {
            match field {
                0 => s.theme = v,
                _ => s.accent = v,
            }
        }
    }
    // For a screenshot of reduced motion.
    if let Ok(v) = std::env::var("WAKE_MOTION")
        && settings::MOTIONS.contains(&v.as_str())
    {
        s.motion = v;
    }
    S.with(|c| *c.borrow_mut() = s);
    NOTE.with(|n| *n.borrow_mut() = note);
    SAVES.with(|c| c.set(saves));
}

pub fn get() -> Settings {
    S.with(|c| c.borrow().clone())
}

/// What went wrong reading the file, in one sentence, if anything did.
pub fn note() -> Option<String> {
    NOTE.with(|n| n.borrow().clone())
}

pub fn saves() -> bool {
    SAVES.with(Cell::get)
}

/// Changes the settings and writes them (off the UI thread). Says so in a toast if that fails.
pub fn change(ui: &Ui, f: impl FnOnce(&mut Settings)) {
    let s = S.with(|c| {
        let mut c = c.borrow_mut();
        f(&mut c);
        c.clone()
    });
    if !saves() {
        return;
    }
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let d = dir();
        if let Ok(Err(e)) = gio::spawn_blocking(move || settings::save(&d, &s)).await {
            crate::app::toast(&ui, &e, true);
        }
    });
}

/// A view's remembered sort.
pub fn sort_of(view: View) -> Sort {
    S.with(|c| c.borrow().sorts.get(view.id()).and_then(|s| sort::parse(s)))
}

pub fn set_sort(ui: &Ui, view: View, s: &Sort) {
    let id = view.id().to_string();
    let s = s.clone();
    change(ui, move |x| match s {
        Some((k, rev)) => {
            x.sorts.insert(id, sort::format(&k, rev));
        }
        None => {
            x.sorts.remove(&id);
        }
    });
}
