//! Test and screenshot hooks: `--screenshot`, `--show` and the motion frames, and the peek's
//! `look:`/`hold:` hooks. Nothing here runs in normal use.

use std::{cell::RefCell, path::PathBuf, sync::Arc, time::Duration};

use gtk4::{self as gtk, gdk, glib, gsk, prelude::*};
use wake_core::*;

use crate::{
    agents,
    app::{IdeaTab, Ui, View, find_session, is_live, move_sel, refresh, render_all, restore_sel, set_view, toast},
    dialogs, panel, peek, prefs, setview, switcher, theme, views, w,
};

/// Opens what `--show` asks for, once the data is in.
pub fn apply_show(ui: &Ui, show: &str) {
    let (what, arg) = show.split_once(':').map_or((show, None), |(a, b)| (a, Some(b.to_string())));
    let first_session = |pred: &dyn Fn(&SessionSummary) -> bool| ui.st.borrow().ov.as_ref().and_then(|ov| ov.sessions.iter().find(|s| pred(s)).map(|s| s.id.clone()));
    match what {
        // Test hook: scroll the current view, then force a full rebuild, to prove refreshes keep the place.
        "scroll" => {
            // Wait until the view is long enough to scroll, scroll it, then force a full rebuild.
            let px: f64 = arg.as_deref().and_then(|a| a.parse().ok()).unwrap_or(400.0);
            let ui2 = ui.clone();
            let mut n = 0;
            glib::timeout_add_local(Duration::from_millis(300), move || {
                n += 1;
                let view = ui2.st.borrow().view;
                let a = ui2.w.views[&view].scroll.vadjustment();
                if a.upper() - a.page_size() < px && n < 40 {
                    return glib::ControlFlow::Continue;
                }
                a.set_value(px);
                eprintln!("scroll set to {:.0} (of {:.0}); forcing a rebuild", a.value(), a.upper() - a.page_size());
                ui2.st.borrow_mut().drawn.clear();
                refresh(&ui2);
                glib::ControlFlow::Break
            });
        }
        // Test hook: open the session panel with the most prompts, scroll it, force a full redraw
        // and a focus restore, to prove the panel keeps the reader's place.
        "panel-scroll" => {
            let px: f64 = arg.as_deref().and_then(|a| a.parse().ok()).unwrap_or(500.0);
            let id = ui.st.borrow().ov.as_ref().and_then(|ov| ov.sessions.iter().max_by_key(|s| s.prompts).map(|s| s.id.clone()));
            let Some(id) = id else { return };
            panel::open_session(ui, &id, true);
            // Every prompt is on the "You asked" page: scroll that.
            panel::open_page(ui, "asked");
            let ui2 = ui.clone();
            let mut n = 0;
            glib::timeout_add_local(Duration::from_millis(300), move || {
                n += 1;
                if n == 1 {
                    // "show all" was set after the first draw: draw once with every prompt.
                    panel::render(&ui2);
                }
                let Some(a) = panel::scroll_adj(&ui2) else { return glib::ControlFlow::Continue };
                if a.upper() - a.page_size() < px && n < 40 {
                    return glib::ControlFlow::Continue;
                }
                a.set_value(px);
                eprintln!("panel scroll set to {:.0} (of {:.0}); forcing a redraw with focus inside", a.value(), a.upper() - a.page_size());
                panel::focus_primary(&ui2);
                crate::w::keep_scroll(&a, px);
                let ui3 = ui2.clone();
                glib::timeout_add_local_once(Duration::from_millis(700), move || {
                    ui3.st.borrow_mut().panel_drawn = None;
                    panel::render(&ui3);
                });
                glib::ControlFlow::Break
            });
        }
        "agents-open" => {
            let pid = {
                let st = ui.st.borrow();
                let with_subs = |sid: &str| st.ov.as_ref().is_some_and(|ov| ov.sessions.iter().any(|s| s.id == sid && s.subagents.total > 0));
                st.procs.as_ref().and_then(|p| p.procs.iter().find(|a| a.session_id.as_deref().is_some_and(with_subs)).map(|a| a.pid))
            };
            if let Some(pid) = pid {
                ui.st.borrow_mut().sel = Some(pid.to_string());
                agents::set_open(ui, pid, true);
            }
        }
        // Check: opening the side panel must not change the window's minimum width (a tiling
        // compositor scales a window whose minimum outgrows its tile). Prints both, and whether the
        // content fits the window at its current width.
        "measure" => {
            let report = |ui: &Ui, when: &str| {
                let child = ui.window.child().expect("window content");
                let (min, nat, _, _) = child.measure(gtk::Orientation::Horizontal, -1);
                let width = ui.window.width();
                // The body's own content as laid out now (rail and main column; its minimum is its
                // narrowest layout's).
                let inner_min = ui.w.rail.measure(gtk::Orientation::Horizontal, -1).0 + ui.w.body.first_child().map_or(0, |m| m.measure(gtk::Orientation::Horizontal, -1).0);
                if std::env::var("WAKE_MEASURE_TREE").is_ok_and(|v| !v.is_empty()) {
                    fn walk(wd: &gtk::Widget, depth: usize) {
                        if depth > 12 || !wd.is_visible() {
                            return;
                        }
                        let m = wd.measure(gtk::Orientation::Horizontal, -1).0;
                        if m >= 250 && depth > 4 {
                            println!("{}{} {:?} min {m}", "  ".repeat(depth), wd.type_().name(), wd.css_classes());
                        }
                        for x in w::children(wd) {
                            walk(&x, depth + 1);
                        }
                    }
                    walk(ui.w.column.upcast_ref(), 0);
                }
                println!(
                    "measure {when}: window content min {min} natural {nat}; width {width}; body content min {inner_min} → {}",
                    if inner_min <= width && min <= width { "fits" } else { "OVERFLOWS" }
                );
            };
            report(ui, "panel closed");
            let id = ui.st.borrow().ov.as_ref().and_then(|ov| ov.sessions.iter().find(|s| is_live(s.state)).or(ov.sessions.first()).map(|s| s.id.clone()));
            if let Some(id) = id {
                panel::open_session(ui, &id, false);
            }
            let ui2 = ui.clone();
            glib::timeout_add_local_once(Duration::from_millis(900), move || report(&ui2, "panel open"));
        }
        "clips" => crate::layout::hook(ui),
        "peek" => peek::show_for_shot(ui, arg),
        "hold" => hold(ui, arg),
        "look" => {
            if let Some(pid) = arg.and_then(|a| a.parse().ok()) {
                look_at(ui, pid);
            }
        }
        "questions" => crate::questions::show(ui, arg),
        "behind" | "behind-pull" | "behind-wait" => crate::behind::show(ui, what, arg),
        "disk" | "disk-machine" | "disk-clear" | "disk-remove" | "set-project" => crate::disk::show(ui, what, arg), // disk.rs hook
        // Projects with "Track a folder" open.
        "track" => {
            set_view(ui, View::Projects);
            ui.st.borrow_mut().adding = true;
            ui.st.borrow_mut().drawn.remove(&View::Projects);
            views::render_view(ui);
        }
        // Ideas on one of its lists: ideas:sent, ideas:done.
        "ideas" => {
            set_view(ui, View::Ideas);
            ui.st.borrow_mut().idea_tab = IdeaTab::of(arg.as_deref().unwrap_or("inbox"));
            ui.st.borrow_mut().drawn.remove(&View::Ideas);
            views::render_view(ui);
        }
        // An error toast (a long one unless given): how it wraps.
        "toast" => toast(
            ui,
            arg.as_deref()
                .unwrap_or("Couldn't stop 3 of 5: claude (PID 48211) belongs to another user, so Wake can't stop it; codex (PID 50122) had already ended; claude (PID 50310) didn't stop when asked."),
            true,
        ),
        "switcher" => switcher::open(ui),
        "switcher-subs" => {
            switcher::open(ui);
            let id = first_session(&|s| s.subagents.running > 0 && s.subagents.total > 1);
            switcher::show_subs_of(ui, id);
        }
        "session" => {
            if let Some(id) = arg.or_else(|| first_session(&|s| s.subagents.total > 0)) {
                panel::open_session(ui, &id, true);
                // Screenshots of the panel's sections opened: WAKE_PANEL_OPEN=subs,asked,files,details.
                if let Ok(open) = std::env::var("WAKE_PANEL_OPEN") {
                    // The first one named opens as its page.
                    let k = ["subs", "asked", "recap", "files", "details", "sessions", "ideas", "git", "pdetails", "colour"].into_iter().find(|k| open.split(',').any(|x| x == *k));
                    ui.st.borrow_mut().panel_page = k;
                    panel::render(ui);
                }
            }
        }
        "project" => {
            let id = arg.or_else(|| ui.st.borrow().ov.as_ref().and_then(|ov| ov.projects.first().map(|p| p.id.clone())));
            if let Some(id) = id {
                panel::open_project(ui, &id, true);
                if let Ok(open) = std::env::var("WAKE_PANEL_OPEN") {
                    let k = ["sessions", "ideas", "git", "pdetails", "colour"].into_iter().find(|k| open.split(',').any(|x| x == *k));
                    ui.st.borrow_mut().panel_page = k;
                    panel::render(ui);
                }
            }
        }
        "keys" => match arg {
            // Replays keys through the window's own handler (focus screenshots).
            Some(seq) => crate::keynav::replay(ui, &seq),
            None => dialogs::keys(ui),
        },
        // Ideas: the ticked ideas of the project with the most to sort, and (start-agent) the dialog.
        "ticked" | "start-agent" => {
            let picked = {
                let st = ui.st.borrow();
                let ideas: Vec<&Idea> = st.ideas.iter().flatten().filter(|i| i.state == "inbox" && i.project.is_some()).collect();
                let best = ideas.iter().map(|i| &i.project).max_by_key(|p| ideas.iter().filter(|i| &i.project == *p).count()).cloned().flatten();
                ideas.iter().filter(|i| i.project == best).map(|i| i.id.clone()).collect::<Vec<_>>()
            };
            ui.st.borrow_mut().picked = picked;
            views::render_chips(ui);
            views::render_view(ui);
            if what == "start-agent" {
                crate::start::open(ui, None);
            }
        }
        // Settings: the keyboard on one setting (setsel:theme), for a focus screenshot.
        "setsel" => {
            ui.st.borrow_mut().sel = arg.clone();
            setview::keyboard(ui, true);
            restore_sel(ui, View::Settings, true);
            if std::env::var_os("WAKE_SHOT_DEBUG").is_some() {
                eprintln!("setsel: focus on {:?}", gtk::prelude::GtkWindowExt::focus(&ui.window).map(|f| (f.type_(), f.widget_name())));
            }
        }
        // Settings: Theme flipped through System and back a few times, quickly, to prove no
        // change re-enters itself (theme-cycle, or theme-cycle:light to end on light).
        "theme-cycle" => {
            let end = arg.unwrap_or_else(|| prefs::get().theme);
            let mut steps: Vec<String> = ["system", "dark", "system", "light", "system"].iter().map(|s| s.to_string()).collect();
            steps.push(end);
            for (i, t) in steps.into_iter().enumerate() {
                let ui = ui.clone();
                glib::timeout_add_local_once(Duration::from_millis(120 * i as u64), move || {
                    setview::apply(&ui, "theme", &t);
                    eprintln!("theme now {t} (light: {})", theme::is_light());
                });
            }
        }
        // A drop-down list opened, for screenshots: dropdown:capture (the capture bar's project
        // picker) or dropdown:questions (the Questions context picker).
        // Settings' drop-down once its column has its final place.
        "dropdown" if arg.as_deref() == Some("settings") => {
            let ui = ui.clone();
            glib::timeout_add_local_once(Duration::from_millis(500), move || apply_show(&ui, "dropdown:settings-now"));
        }
        "dropdown" => {
            let dd = match arg.as_deref() {
                Some("questions") => w::find(&ui.w.views[&View::Questions].root, |w| w.is::<gtk::DropDown>()),
                Some("settings-now") => w::find(&ui.w.views[&View::Settings].root, |w| w.is::<gtk::DropDown>()),
                _ => None,
            };
            if let Some(dd) = dd.and_downcast::<gtk::DropDown>()
                && let Some(overlay) = ui.window.child().and_downcast::<gtk::Overlay>()
                && let Some(at) = dd.compute_point(&ui.window, &gtk::graphene::Point::new(0.0, 0.0))
            {
                // Inside the window (so `window.wake popover …` applies), where GTK would open it:
                // below the drop-down, or above when there is no room.
                let pop = shot_list(&dd);
                let w = pop.measure(gtk::Orientation::Horizontal, -1).1;
                let h = pop.measure(gtk::Orientation::Vertical, w).1;
                let below = at.y() as i32 + dd.height();
                let y = if below + h > ui.window.height() { at.y() as i32 - h } else { below };
                pop.set_halign(gtk::Align::Start);
                pop.set_valign(gtk::Align::Start);
                pop.set_margin_start((at.x() as i32 + (dd.width() - w) / 2).max(0));
                pop.set_margin_top(y.max(0));
                if std::env::var_os("WAKE_SHOT_DEBUG").is_some() {
                    eprintln!("dropdown list at {:?} size {w}x{h}", (at.x(), y));
                }
                overlay.add_overlay(&pop);
            } else if std::env::var_os("WAKE_SHOT_DEBUG").is_some() {
                eprintln!("dropdown list: nothing to open");
            }
        }
        "pause" => {
            let id = arg.or_else(|| ui.st.borrow().ov.as_ref().and_then(|ov| ov.projects.first().map(|p| p.id.clone())));
            if let Some(id) = id {
                dialogs::pause(ui, &id);
            }
        }
        "stale" => agents::stop_all_stale(ui),
        "stale-only" => {
            ui.st.borrow_mut().stale_only = true;
            views::render_chips(ui);
            agents::render(ui);
        }
        "stop" => {
            let pid = arg.and_then(|a| a.parse().ok()).or_else(|| ui.st.borrow().procs.as_ref().and_then(|p| p.procs.iter().find(|p| p.stale_reason.is_some()).map(|p| p.pid)));
            if let Some(pid) = pid {
                agents::stop_one(ui, pid);
            }
        }
        _ => eprintln!(
            "wake: --show takes scroll[:px], panel-scroll[:px], agents-open, readouts, switcher, switcher-subs, session[:id], project[:id], pause, stale, stop[:pid], toast[:text], tabs-unknown or track"
        ),
    }
}

/// A drop-down's open list as GTK draws it (popover > contents > listview of rows), built inside
/// the window for a screenshot: same CSS nodes, so the same rules apply.
fn shot_list(dd: &gtk::DropDown) -> gtk::Widget {
    let pop = w::named_box("popover");
    let contents = w::named_box("contents");
    contents.set_parent(&pop);
    // GTK's own nodes: popover > contents > scrolledwindow > listview > row, the chosen row selected.
    let sw = w::named_box("scrolledwindow");
    sw.set_parent(&contents);
    let lv = w::named_box("listview");
    lv.set_parent(&sw);
    lv.set_size_request(dd.width().max(160), -1);
    if let Some(model) = dd.model() {
        for i in 0..model.n_items() {
            let text = model.item(i).and_downcast::<gtk::StringObject>().map(|s| s.string().to_string()).unwrap_or_default();
            let row = w::named_box("row");
            let l = gtk::Label::new(Some(&text));
            l.set_xalign(0.0);
            l.set_parent(&row);
            if i == dd.selected() {
                row.set_state_flags(gtk::StateFlags::SELECTED, false);
            }
            row.set_parent(&lv);
        }
    }
    pop
}

pub fn screenshot(ui: &Ui, path: PathBuf, show: Option<String>) {
    let ui = ui.clone();
    let mut tries = 0;
    let mut shown = false;
    let mut settled = false;
    let mut failed = 0;
    glib::timeout_add_local(Duration::from_millis(700), move || {
        tries += 1;
        // GTK clears focus-visible on its own (Broadway sends no keys to keep it on): draw focus as
        // it looks after keyboard use. Set a tick ahead of the shot, so the styles have caught up.
        if std::env::var_os("WAKE_SHOT_FOCUS").is_some() {
            ui.window.set_focus_visible(true);
        }
        // Wait for the first full index (and a refresh after it), so a shot shows real data.
        let ready = {
            let st = ui.st.borrow();
            st.procs.is_some() && st.ov.as_ref().is_some_and(|o| o.indexing.total > 0 && o.indexing.done >= o.indexing.total && !o.projects.iter().all(|p| p.last_at.is_none()))
        };
        if !ready {
            if tries % 4 == 0 {
                refresh(&ui);
            }
            if tries < 40 {
                return glib::ControlFlow::Continue;
            }
        }
        if !ready && tries < 14 {
            return glib::ControlFlow::Continue;
        }
        if let Some(show) = &show
            && !shown
        {
            shown = true;
            apply_show(&ui, show);
            // The clip check takes its own time and quits when it is done.
            if show == "clips" {
                return glib::ControlFlow::Break;
            }
            return glib::ControlFlow::Continue;
        }
        if tries < 3 {
            return glib::ControlFlow::Continue;
        }
        // Broadway's frame clock may not tick with no browser attached; the readouts are drawn as
        // they stand a moment after the view comes up.
        if !settled {
            settled = true;
            ui.scope.borrow_mut().appeared(glib::monotonic_time() as f64 / 1e6);
            for r in ui.readouts.borrow().iter() {
                r.queue_draw();
            }
            return glib::ControlFlow::Continue;
        }
        // One more overview just before the shot, so it shows the settled state (git included).
        // A scroll test needs longer: the hook waits for the list to load, then rebuilds it.
        let long = show.as_deref().is_some_and(|s| s.starts_with("measure") || s.starts_with("peek") || s.starts_with("scroll") || s.starts_with("panel-scroll") || s.starts_with("agents-open"));
        // Behind-the-remote shots of real data wait for the first ls-remote pass.
        let slow = show.as_deref().is_some_and(|s| s.starts_with("behind"));
        // A key replay: its keys (150 ms apart) and a beat for what they open.
        let keyed = show.as_deref().is_some_and(|s| s.starts_with("keys:"));
        // Long enough for every key of the replay (150 ms apart), then a beat.
        let keys_for = show.as_deref().and_then(|s| s.strip_prefix("keys:")).map_or(0, |k| k.split(',').count() * 150 / 700 + 4);
        if tries < 6 || (long && tries < 24) || (slow && tries < 45) || (keyed && tries < 10.max(keys_for)) {
            refresh(&ui);
            return glib::ControlFlow::Continue;
        }
        if show.as_deref().is_some_and(|s| s.starts_with("scroll")) {
            let view = ui.st.borrow().view;
            let a = ui.w.views[&view].scroll.vadjustment();
            println!("scroll after rebuild: {:.0} (of {:.0})", a.value(), (a.upper() - a.page_size()).max(0.0));
        }
        if show.as_deref().is_some_and(|s| s.starts_with("panel-scroll"))
            && let Some(a) = panel::scroll_adj(&ui)
        {
            println!("panel scroll after redraw: {:.0} (of {:.0})", a.value(), (a.upper() - a.page_size()).max(0.0));
        }
        if let Ok(frames) = std::env::var("WAKE_SHOT_FRAMES") {
            motion_frames(&ui, &path, &frames, &std::env::var("WAKE_SHOT_MOTION").unwrap_or_default());
            return glib::ControlFlow::Break;
        }
        if std::env::var_os("WAKE_SHOT_DEBUG").is_some() {
            let f = gtk::prelude::GtkWindowExt::focus(&ui.window);
            eprintln!("shot: focus {:?}, window focus-visible {}", f.map(|f| (f.type_().name(), f.state_flags())), ui.window.gets_focus_visible());
        }
        match save_png(&ui, &path) {
            Ok(()) => println!("saved {}", path.display()),
            // The window can be mid-layout (the caret, a pop): try again a few times.
            Err(_) if tries < 16 || failed < 6 => {
                failed += 1;
                return glib::ControlFlow::Continue;
            }
            Err(err) => eprintln!("wake: screenshot failed: {err} (window {}x{}, mapped {})", ui.window.width(), ui.window.height(), ui.window.is_mapped()),
        }
        if let Some(app) = ui.window.application() {
            app.quit();
        }
        glib::ControlFlow::Break
    });
}

/// For checking motion: a frame before `action`, then one at each of `frames` (milliseconds after
/// it, comma-separated), saved beside `path` as `<name>-f<ms>.png`, then quits. Actions: `panel`
/// (open a session's panel), `close`, `fold:<key>`, `sub` (open a subagent's steps in the panel),
/// `done` (unfold the finished subagents), `down` (selection down), `switcher`, `pause` (a
/// dialog), `agents-open`, `toast`. Needs a backend whose frame clock runs (Broadway's doesn't
/// without a browser attached; a headless Weston does).
fn motion_frames(ui: &Ui, path: &std::path::Path, frames: &str, action: &str) {
    let stem = path.with_extension("");
    let name = move |tag: &str| std::path::PathBuf::from(format!("{}-{tag}.png", stem.display()));
    if let Err(e) = save_png(ui, &name("before")) {
        eprintln!("wake: frame failed: {e}");
    }
    let click = |root: &gtk::Widget, name: &str| {
        let found = if name.ends_with('*') { w::find(root, |w| w.widget_name().starts_with(name.trim_end_matches('*')) && w.is::<gtk::Button>() && w.is_mapped()) } else { w::named(root, name) };
        match found.and_downcast::<gtk::Button>() {
            Some(b) => b.emit_clicked(),
            None => eprintln!("wake: nothing named {name} to click"),
        }
    };
    let drawer = ui.w.drawer.clone().upcast::<gtk::Widget>();
    let view = ui.st.borrow().view;
    match action.split_once(':').map_or((action, ""), |(a, b)| (a, b)) {
        ("panel", arg) => {
            let id = if arg.is_empty() { ui.st.borrow().ov.as_ref().and_then(|ov| ov.sessions.iter().find(|s| s.subagents.total > 0).map(|s| s.id.clone())) } else { Some(arg.to_string()) };
            if let Some(id) = id {
                panel::open_session(ui, &id, false);
            }
        }
        ("close", _) => panel::close(ui),
        ("fold", key) => click(&drawer, &format!("fold-{key}")),
        ("sub", _) => click(&drawer, "sub-*"),
        ("done", _) => click(&drawer, "subs-done"),
        ("down", _) => move_sel(ui, 1),
        ("switcher", _) => switcher::open(ui),
        ("pause", _) => {
            let first = ui.st.borrow().ov.as_ref().and_then(|ov| ov.projects.first().map(|p| p.id.clone()));
            if let Some(id) = first {
                dialogs::pause(ui, &id);
            }
        }
        ("agents-open", _) => {
            let pid = {
                let st = ui.st.borrow();
                let with_subs = |sid: &str| st.ov.as_ref().is_some_and(|ov| ov.sessions.iter().any(|s| s.id == sid && s.subagents.total > 0));
                st.procs.as_ref().and_then(|p| p.procs.iter().find(|a| a.session_id.as_deref().is_some_and(with_subs)).map(|a| a.pid))
            };
            if let Some(pid) = pid {
                agents::set_open(ui, pid, true);
            }
        }
        ("toast", _) => toast(ui, "Next step saved.", false),
        (other, _) => eprintln!("wake: no motion action {other} (view {view:?})"),
    }
    let mut times: Vec<u64> = frames.split(',').filter_map(|t| t.trim().parse().ok()).collect();
    times.sort_unstable();
    times.reverse();
    // Taken on the frame clock (each when its frame comes, by the frame's own time), so every
    // saved frame is one GTK really drew, however long saving takes.
    let t0: std::cell::Cell<Option<i64>> = Default::default();
    let times = RefCell::new(times);
    let ui2 = ui.clone();
    ui.window.add_tick_callback(move |_, clock| {
        let now = clock.frame_time() / 1000;
        let start = *t0.get().get_or_insert(now);
        t0.set(Some(start));
        let elapsed = (now - start) as u64;
        let mut times = times.borrow_mut();
        // One frame per tick: the next is drawn before it is taken.
        if let Some(&t) = times.last().filter(|t| **t <= elapsed) {
            times.pop();
            let p = name(&format!("f{t:03}"));
            if std::env::var_os("WAKE_SHOT_DEBUG").is_some() {
                eprintln!("frame {t} (at {elapsed} ms): panel opacity {:.2} revealed {}", ui2.w.drawer.opacity(), ui2.w.drawer_rev.is_child_revealed());
            }
            match save_png(&ui2, &p) {
                Ok(()) => println!("saved {} at {elapsed} ms", p.display()),
                Err(e) => eprintln!("wake: frame failed: {e}"),
            }
        }
        if times.is_empty() {
            if let Some(app) = ui2.window.application() {
                app.quit();
            }
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    });
}

/// As Slipstream Settings' `--screenshot`: the window drawn into a texture by the Cairo renderer.
/// A floating peek preview (its own surface) is drawn over the window where it sits.
pub fn save_png(ui: &Ui, path: &std::path::Path) -> Result<(), String> {
    let window = &ui.window;
    let paintable = gtk::WidgetPaintable::new(Some(window));
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, window.width() as f64, window.height() as f64);
    if let Some((layer, x, y)) = peek::shot_layer(ui) {
        let p = gtk::WidgetPaintable::new(Some(&layer));
        snapshot.save();
        snapshot.translate(&gtk::graphene::Point::new(x as f32, y as f32));
        p.snapshot(&snapshot, layer.width() as f64, layer.height() as f64);
        snapshot.restore();
    }
    let node = snapshot.to_node().ok_or("the window drew nothing")?;
    let renderer = gsk::CairoRenderer::new();
    renderer.realize_for_display(&WidgetExt::display(window)).map_err(|e| e.to_string())?;
    let texture = renderer.render_texture(node, None);
    renderer.unrealize();
    texture.save_to_png(path).map_err(|e| e.to_string())
}

/// The `look:` hook's desktop link (the real one) and the process whose window every peek flies
/// to instead of a session's window.
static LOOK: std::sync::Mutex<Option<(Arc<wake_core::look::Look>, u32)>> = std::sync::Mutex::new(None);

/// The `look:` hook's link and process, once `--show look:PID` or `hold:look=PID` has set them.
pub fn look() -> Option<(Arc<wake_core::look::Look>, u32)> {
    LOOK.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Test hook `--show look:PID`: every peek flies to the window holding process PID (a stand-in
/// for an agent's terminal), through the real desktop link, and the keyboard is put on the first
/// Monitor row so real Space/Tab presses (from the compositor) can drive it.
pub fn look_at(ui: &Ui, pid: u32) {
    let link = wake_core::look::Look::new(
        wake_core::desktop::default_runner(),
        &std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default(),
        wake_core::store::Paths::from_env().data.join("kwin"),
        std::process::id(),
        true,
    );
    let kind = link.kind();
    *LOOK.lock().unwrap_or_else(|e| e.into_inner()) = Some((Arc::new(link), pid));
    ui.peek.borrow_mut().kind = Some(kind);
    // The rows and details say what holding Space does, which this changes.
    render_all(ui);
    eprintln!("wake: look hook: peeks fly to pid {pid}'s window ({kind:?})");
    focus_first_row(ui);
}

/// The keyboard on the first row of the current table (Monitor if no table is showing).
fn focus_first_row(ui: &Ui) -> Option<String> {
    let view = ui.st.borrow().view;
    let view = if matches!(view, View::Monitor | View::Agents) { view } else { View::Monitor };
    set_view(ui, view);
    let list = ui.w.views[&view].list.clone();
    for x in w::children(&list) {
        if let Some(row) = x.downcast_ref::<gtk::ListBoxRow>()
            && row.is_focusable()
            && !row.widget_name().is_empty()
        {
            let name = row.widget_name().to_string();
            ui.st.borrow_mut().sel = Some(name.clone());
            list.select_row(Some(row));
            row.grab_focus();
            eprintln!("wake: hold hook: the keyboard is on row {name}");
            return Some(name);
        }
    }
    eprintln!("wake: hold hook: no row to put the keyboard on");
    None
}

/// Test hook `--show hold[:SESSION][,stay]` or `--show hold:look=PID[,stay]`: a real hold, fed
/// through the same path as key events (`on_press`/`on_release`): Space down, GTK-style key
/// repeats from 400 ms, then a release at 2.4 s (or Enter at 2.4 s and the release after it).
/// With `WAKE_HOLD_SHOT=<png>` the window is saved while the peek is on. Quits when done.
pub fn hold(ui: &Ui, arg: Option<String>) {
    let arg = arg.unwrap_or_default();
    if std::env::var_os("WAKE_HOLD_SHOT").is_some() {
        ui.window.set_size_request(1300, 900);
    }
    let stay = arg.split(',').any(|a| a == "stay");
    let first = arg.split(',').next().unwrap_or("").to_string();
    if let Some(pid) = first.strip_prefix("look=").and_then(|p| p.parse().ok()) {
        look_at(ui, pid);
    }
    let session = (!first.is_empty() && !first.starts_with("look=") && first != "stay").then_some(first);
    // The row: the session asked for (Monitor), else the first row.
    let row = match &session {
        Some(id) => {
            set_view(ui, View::Monitor);
            let list = ui.w.views[&View::Monitor].list.clone();
            let found = w::children(&list).filter_map(|x| x.downcast::<gtk::ListBoxRow>().ok()).filter(|r| r.widget_name().starts_with(id.as_str())).last();
            found.map(|r| {
                let name = r.widget_name().to_string();
                ui.st.borrow_mut().sel = Some(name.clone());
                list.select_row(Some(&r));
                r.grab_focus();
                name
            })
        }
        None => focus_first_row(ui),
    };
    let Some(row) = row else {
        eprintln!("wake: hold hook: no row for {session:?}");
        return;
    };
    let bg = find_session(ui, &row).map(|s| s.location.is_none());
    eprintln!("wake: hold hook: holding Space on {row} (background, no tab: {bg:?}), {}", if stay { "Enter to stay" } else { "then letting go" });
    let ui = ui.clone();
    let t0 = peek::now_ms();
    let mut sent_enter = false;
    let mut shot = false;
    let r = peek::on_press(&ui, gdk::Key::space, gdk::ModifierType::empty());
    eprintln!("wake: hold hook: first press {}", if r.is_some() { "taken by the peek" } else { "NOT taken by the peek" });
    glib::timeout_add_local(Duration::from_millis(33), move || {
        let t = peek::now_ms() - t0;
        if (400..2400).contains(&t) {
            // GTK's key repeat: more presses of the same key while it is held.
            let _ = peek::on_press(&ui, gdk::Key::space, gdk::ModifierType::empty());
        }
        if !shot && t >= 1600 {
            shot = true;
            let mode = ui.peek.borrow().mode;
            eprintln!("wake: hold hook: at {t} ms the peek is {:?} / {mode:?}", ui.peek.borrow().phase);
            if let Ok(path) = std::env::var("WAKE_HOLD_SHOT") {
                match save_png(&ui, std::path::Path::new(&path)) {
                    Ok(()) => eprintln!("wake: hold hook: saved {path}"),
                    Err(e) => eprintln!("wake: hold hook: shot failed: {e}"),
                }
            }
        }
        if stay && !sent_enter && t >= 2400 {
            sent_enter = true;
            let _ = peek::on_press(&ui, gdk::Key::Return, gdk::ModifierType::empty());
        }
        if t >= if stay { 2600 } else { 2400 } {
            peek::on_release(&ui, gdk::Key::space);
            let ui = ui.clone();
            glib::timeout_add_local_once(Duration::from_millis(1500), move || {
                let p = ui.peek.borrow();
                eprintln!("wake: hold hook: done; phase {:?}, mode {:?}", p.phase, p.mode);
                drop(p);
                if std::env::var_os("WAKE_HOLD_STAY_OPEN").is_none()
                    && let Some(app) = ui.window.application()
                {
                    app.quit();
                }
            });
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    });
}
