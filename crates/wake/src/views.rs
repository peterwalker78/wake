//! The top bar, the rail, the chips, the status bar and the Monitor, Projects, Ideas and History
//! views.
//!
//! Calm by design: rows keep their order while their state stays the same (sorted by when a
//! session started, never by when it last wrote), a refresh that changes nothing redraws nothing,
//! and ages, memory and CPU are updated in place (`Live`) instead of rebuilding rows.

use gtk4::{self as gtk, gdk, glib, prelude::*};
use wake_core::*;

use crate::{
    agents,
    app::{self, AppState, Filter, IdeaTab, Ui, View},
    fmt, panel, prefs, sort, theme, w,
};

/// Dim text in Pango markup, in the theme now showing (at least 4.5:1 on every background).
fn dim(t: &str) -> String {
    format!("<span foreground=\"{}\">{t}</span>", theme::hex("dim"))
}

/// Amber text in Pango markup: something that needs you.
fn amber(t: &str) -> String {
    format!("<span foreground=\"{}\">{t}</span>", theme::hex("amber"))
}

const CHIPS: [(Filter, &str); 6] =
    [(Filter::Live, "Open now"), (Filter::Waiting, "Waiting for you"), (Filter::Working, "Working"), (Filter::Idle, "Idle"), (Filter::Ended, "Ended"), (Filter::All, "All")];

// ---------- in-place updates ----------

/// A widget that a view updates in place from the latest data: an age, a memory figure.
type Update = Box<dyn Fn(&AppState, &gtk::Widget)>;

pub struct Live {
    widget: gtk::Widget,
    f: Update,
}

impl Live {
    pub fn apply(&self, st: &AppState) {
        (self.f)(st, &self.widget);
    }
}

/// `l`'s text follows `f` (None leaves it as it is).
pub fn live_text(ui: &Ui, view: View, l: &gtk::Label, f: impl Fn(&AppState) -> Option<String> + 'static) {
    let f = move |st: &AppState, w: &gtk::Widget| {
        if let (Some(t), Some(l)) = (f(st), w.downcast_ref::<gtk::Label>())
            && l.text() != t
        {
            l.set_text(&t);
        }
    };
    ui.live.borrow_mut().entry(view).or_default().push(Live { widget: l.clone().upcast(), f: Box::new(f) });
}

/// `wd`'s tooltip follows `f`.
pub fn live_tip(ui: &Ui, view: View, wd: &impl IsA<gtk::Widget>, f: impl Fn(&AppState) -> Option<String> + 'static) {
    let f = move |st: &AppState, w: &gtk::Widget| {
        if let Some(t) = f(st)
            && w.tooltip_text().as_deref() != Some(t.as_str())
        {
            w.set_tooltip_text(Some(&t));
        }
    };
    ui.live.borrow_mut().entry(view).or_default().push(Live { widget: wd.clone().upcast(), f: Box::new(f) });
}

fn session_in<'a>(st: &'a AppState, id: &str) -> Option<&'a SessionSummary> {
    st.ov.as_ref()?.sessions.iter().find(|s| s.id == id)
}

// ---------- order ----------

/// Needs-you first. "Lost track" sorts with the ended ones: there is nothing to do about it.
fn order(s: State) -> u8 {
    match s {
        State::Waiting => 0,
        State::Working => 1,
        State::Idle => 2,
        State::Unknown | State::Ended => 3,
    }
}

/// Monitor's order: by state, then newest *started* first. A session's start never changes, so two
/// working sessions never swap places because one of them wrote something.
pub fn by_session(a: &SessionSummary, b: &SessionSummary) -> std::cmp::Ordering {
    order(a.state).cmp(&order(b.state)).then_with(|| b.started_at.cmp(&a.started_at)).then_with(|| a.id.cmp(&b.id))
}

/// History's order: by the day each was last used (newest day first), then newest started first.
/// The day only changes once a day, so the list stays put while you read it.
pub fn by_recent_day(a: &SessionSummary, b: &SessionSummary) -> std::cmp::Ordering {
    // The local day, as History's day headings show it.
    let day = |s: &SessionSummary| s.last_at.as_deref().map(|t| fmt::day_key(t).unwrap_or_else(|| t.chars().take(10).collect()));
    day(b).cmp(&day(a)).then_with(|| b.started_at.cmp(&a.started_at)).then_with(|| a.id.cmp(&b.id))
}

/// Projects: waiting, then working, then loose ends in git, then the rest. Live projects (whose
/// last-touched time moves all the time) are by name; the others by when last touched.
pub fn project_rank(p: &ProjectSummary) -> u8 {
    if p.removed {
        5
    } else if p.live.waiting > 0 {
        0
    } else if p.live.working > 0 {
        1
    } else if p.live.idle > 0 {
        2
    } else if p.git.ahead_total + p.git.dirty_files > 0 {
        3
    } else {
        4
    }
}

pub fn by_project(a: &ProjectSummary, b: &ProjectSummary) -> std::cmp::Ordering {
    let (ra, rb) = (project_rank(a), project_rank(b));
    let name = || a.name.to_lowercase().cmp(&b.name.to_lowercase());
    ra.cmp(&rb).then_with(|| if ra <= 2 { name() } else { b.last_at.cmp(&a.last_at).then_with(name) })
}

fn matches(s: &SessionSummary, f: Filter) -> bool {
    match f {
        Filter::All => true,
        Filter::Live => app::is_live(s.state),
        Filter::Waiting => s.state == State::Waiting,
        Filter::Working => s.state == State::Working,
        Filter::Idle => s.state == State::Idle,
        Filter::Ended => s.state == State::Ended,
    }
}

fn live_count(p: &ProjectSummary) -> u32 {
    p.live.working + p.live.waiting + p.live.idle
}

fn dot_class(p: &ProjectSummary) -> &'static str {
    if p.live.waiting > 0 {
        "waiting"
    } else if p.live.working > 0 {
        "working"
    } else if p.live.idle > 0 {
        "idle"
    } else {
        "quiet"
    }
}

/// A project's dot: its own colour, filled while a session is open in it, a ring when none is,
/// and amber (the one colour that means "you") while one is waiting for you.
pub fn project_dot(p: &ProjectSummary) -> gtk::Box {
    let colour = crate::setview::colour_of(p);
    let (d, tip) = if p.live.waiting > 0 {
        (w::dot("waiting"), format!("{} waiting for you", p.live.waiting))
    } else if live_count(p) > 0 {
        let mut words = Vec::new();
        if p.live.working > 0 {
            words.push(format!("{} working", p.live.working));
        }
        if p.live.idle > 0 {
            words.push(format!("{} idle", p.live.idle));
        }
        (w::dot(&format!("pc-{colour}")), words.join(", "))
    } else {
        let d = w::dot(&format!("pc-{colour}"));
        d.add_css_class("ring");
        (d, "nothing open".to_string())
    };
    d.set_tooltip_text(Some(&tip));
    d
}

/// A project's colour name by its id, from the latest overview.
pub fn colour_by_id(ui: &Ui, id: &str) -> Option<String> {
    ui.st.borrow().ov.as_ref()?.projects.iter().find(|p| p.id == id).map(crate::setview::colour_of)
}

/// A small dot of a project's colour before its name on a session's row.
fn project_tag(ui: &Ui, id: Option<&str>) -> Option<gtk::Box> {
    let c = colour_by_id(ui, id?)?;
    let d = w::dot(&format!("pc-{c}"));
    d.add_css_class("tag");
    d.set_size_request(6, 6);
    Some(d)
}

/// A table header whose named columns sort the view: `keys` lines up with `cols` (None: that
/// column doesn't sort). The sorted column is brighter, with ▲ (its natural order) or ▼ (reversed).
pub fn sort_head(ui: &Ui, view: View, cols: &[Col], keys: &[Option<&'static str>]) -> gtk::Box {
    let cur = prefs::sort_of(view);
    let b = w::hbox(0, &["thead"]);
    for (i, (title, width, grow, right)) in cols.iter().enumerate() {
        let cell: gtk::Widget = match keys.get(i).copied().flatten() {
            None => {
                let l = w::clip(&title.to_uppercase(), &["th"]);
                l.set_tooltip_text(None);
                if *right {
                    l.set_xalign(1.0);
                }
                l.upcast()
            }
            Some(key) => {
                let btn = gtk::Button::new();
                btn.add_css_class("thb");
                let inner = w::hbox(0, &[]);
                if *right {
                    inner.append(&w::spacer());
                }
                inner.append(&w::label(&title.to_uppercase(), &["th"]));
                let name = title.split(" · ").next().unwrap_or(title).to_lowercase();
                let tip = match &cur {
                    Some((k, rev)) if k == key => {
                        btn.add_css_class("on");
                        let a = w::label(if *rev { "▼" } else { "▲" }, &["arrow"]);
                        a.set_valign(gtk::Align::Center);
                        inner.append(&a);
                        if *rev {
                            format!("Sorted by {name}, {}. Click for Wake's own order.", sort::words(key, true))
                        } else {
                            format!("Sorted by {name}, {}. Click to reverse.", sort::words(key, false))
                        }
                    }
                    _ => format!("Sort by {name}, {}", sort::words(key, false)),
                };
                btn.set_child(Some(&inner));
                btn.set_tooltip_text(Some(&tip));
                btn.update_property(&[gtk::accessible::Property::Label(&tip)]);
                let ui = ui.clone();
                btn.connect_clicked(move |_| {
                    let next = sort::next(&prefs::sort_of(view), key);
                    prefs::set_sort(&ui, view, &next);
                    ui.st.borrow_mut().drawn.remove(&view);
                    render_view(&ui);
                    // The header was rebuilt: give the keyboard back to the same column.
                    focus_header(&ui, view, key);
                });
                btn.set_widget_name(&format!("sort-{key}"));
                btn.upcast()
            }
        };
        b.append(&w::col(&cell, *width, *grow));
    }
    b
}

fn focus_header(ui: &Ui, view: View, key: &str) {
    fn find(w: &gtk::Widget, name: &str) -> Option<gtk::Widget> {
        if w.widget_name() == name {
            return Some(w.clone());
        }
        let mut c = w.first_child();
        while let Some(x) = c {
            if let Some(f) = find(&x, name) {
                return Some(f);
            }
            c = x.next_sibling();
        }
        None
    }
    if let Some(b) = find(ui.w.views[&view].head.upcast_ref(), &format!("sort-{key}")) {
        b.grab_focus();
    }
}

pub fn project_label(name: Option<&str>) -> String {
    name.map_or_else(|| "no project (~)".to_string(), str::to_string)
}

pub fn render_loading(ui: &Ui) {
    let v = ui.st.borrow().view;
    let vw = &ui.w.views[&v];
    w::clear(&vw.empty);
    vw.empty.append(&w::label("Reading what Wake knows…", &["loading"]));
}

// ---------- top, rail, status ----------

pub fn render_top(ui: &Ui) {
    let (c, ideas, stale, ix, err) = {
        let st = ui.st.borrow();
        let Some(ov) = &st.ov else { return };
        (ov.counts.clone(), ov.ideas, st.procs.as_ref().map(agents::stale_summary), ov.indexing.clone(), ov.index_error.clone())
    };
    let busy = ix.total > 0 && ix.done < ix.total;
    ui.w.indexing.set_visible(busy);
    if busy {
        ui.w.indexing.set_text(&format!("indexing {}/{}", ix.done, ix.total));
    }
    ui.w.clock.set_text(&fmt::clock());
    ui.w.banner.set_visible(err.is_some());
    if let Some(e) = &err {
        ui.w.banner.set_text(&format!("Wake couldn't read everything: {e}"));
    }
    // The only "needs you" summary: waiting, working, stale agents and what they hold, ideas.
    let lay = ui.st.borrow().lay;
    let short = lay.narrow;
    // Narrowest, like a phone's app bar: one badge that sums them up.
    let one = lay.rail == crate::layout::Rail::Hidden;
    let sig = format!("{}/{}/{ideas}/{:?}/{short}{one}", c.working, c.waiting, stale.map(|(n, b)| (n, b / (100 << 20))));
    if ui.st.borrow().top_drawn == sig {
        return;
    }
    ui.st.borrow_mut().top_drawn = sig;
    ui.w.counts.clear();
    let stat = |n: u32, word: &str, class: &str, tip: &str, act: Box<dyn Fn()>| {
        let b = gtk::Button::new();
        b.add_css_class("stat");
        b.add_css_class(if n > 0 { class } else { "zero" });
        let inner = w::hbox(7, &[]);
        inner.append(&w::dot("d"));
        inner.append(&w::label(&n.to_string(), &["n"]));
        inner.append(&w::label(word, &[]));
        b.set_child(Some(&inner));
        b.set_tooltip_text(Some(tip));
        b.connect_clicked(move |_| act());
        b
    };
    let set_filter = |f: Filter| {
        let ui = ui.clone();
        Box::new(move || {
            ui.st.borrow_mut().filter = f;
            app::set_view(&ui, View::Monitor);
            app::render_all(&ui);
        }) as Box<dyn Fn()>
    };
    // Only what is there: a zero says nothing, and ideas already have their count in the rail.
    let _ = ideas;
    if one {
        let n_stale = stale.map_or(0, |(n, _)| n);
        let mut words = Vec::new();
        if c.waiting > 0 {
            words.push(format!("{} waiting for you", c.waiting));
        }
        if c.working > 0 {
            words.push(format!("{} working", c.working));
        }
        if n_stale > 0 {
            words.push(format!("{n_stale} stale"));
        }
        let (n, class) = if c.waiting > 0 {
            (c.waiting, "wait")
        } else if c.working > 0 {
            (c.working, "work")
        } else {
            (n_stale, "stalec")
        };
        if words.is_empty() {
            ui.w.counts.add(&w::label("quiet", &["quiet-top"]), 1);
        } else {
            let tip = format!("{}. Click for the sessions waiting for you.", words.join(", "));
            let b = stat(n, "", class, &tip, set_filter(if c.waiting > 0 { Filter::Waiting } else { Filter::Live }));
            b.update_property(&[gtk::accessible::Property::Label(&words.join(", "))]);
            ui.w.counts.add(&b, 0);
        }
        return;
    }
    if c.waiting > 0 {
        // In a narrow window the words shorten; what still doesn't fit drops out from the end.
        let word = if short { "waiting" } else { "waiting for you" };
        ui.w.counts.add(&stat(c.waiting, word, "wait", "Show the sessions waiting for you", set_filter(Filter::Waiting)), 1);
    }
    if c.working > 0 {
        ui.w.counts.add(&stat(c.working, "working", "work", "Show the sessions at work", set_filter(Filter::Working)), 2);
    }
    if let Some((n, bytes)) = stale.filter(|(n, _)| *n > 0) {
        let ui2 = ui.clone();
        ui.w.counts.add(
            &stat(
                n,
                "stale",
                "stalec",
                &format!("Agents that look stale; they hold {}", fmt::bytes(bytes)),
                Box::new(move || {
                    ui2.st.borrow_mut().stale_only = true;
                    ui2.st.borrow_mut().drawn.remove(&View::Agents);
                    app::set_view(&ui2, View::Agents);
                    render_chips(&ui2);
                }),
            ),
            3,
        );
    }
    if c.waiting == 0 && c.working == 0 && stale.is_none_or(|(n, _)| n == 0) {
        ui.w.counts.add(&w::label("all quiet", &["quiet-top"]), 1);
    }
}

pub fn render_rail(ui: &Ui) {
    let (sig, view, pf, navs, ps) = {
        let st = ui.st.borrow();
        let Some(ov) = &st.ov else { return };
        let c = &ov.counts;
        // Counts as plain numbers, amber when something needs you; the keys are in the tooltips.
        let navs: Vec<(View, Option<(u32, bool)>)> = View::ALL
            .into_iter()
            .map(|v| {
                let n = match v {
                    View::Monitor => Some((c.working + c.waiting + c.idle, c.waiting > 0)),
                    View::Ideas if ov.ideas > 0 => Some((ov.ideas, false)),
                    View::Agents => st.procs.as_ref().map(agents::stale_summary).filter(|(n, _)| *n > 0).map(|(n, _)| (n, true)),
                    // Questions: (unread answers, answering now), drawn as a dot.
                    View::Questions => crate::questions::rail_badge(),
                    _ => None,
                };
                (v, n)
            })
            .collect();
        let mut ps: Vec<ProjectSummary> = ov.projects.iter().filter(|p| !p.removed).cloned().collect();
        ps.sort_by(by_project);
        let key: Vec<_> = ps.iter().map(|p| (&p.id, &p.name, dot_class(p), live_count(p), p.paused.is_some(), crate::setview::colour_of(p))).collect();
        let sig = format!("{:?}{:?}{navs:?}{key:?}{}", st.view, st.proj_filter, st.lay.slim());
        (sig, st.view, st.proj_filter.clone(), navs, ps)
    };
    let slim = ui.st.borrow().lay.slim();
    if ui.st.borrow().rail_drawn == sig {
        return;
    }
    ui.st.borrow_mut().rail_drawn = sig;
    // Slim (a narrow window): each view by its initial, each project by its dot, and the full form
    // beside it to slide out over the content when the pointer or the keyboard goes there.
    fill_rail(ui, &ui.w.nav, &ui.w.plist, slim, view, pf.as_deref(), &navs, &ps);
    if slim {
        // The keyboard stays on the same item when the full form is redrawn under it.
        let at = gtk::prelude::GtkWindowExt::focus(&ui.window).and_then(|f| {
            [&ui.w.peek_nav, &ui.w.peek_plist].into_iter().enumerate().find_map(|(k, b)| {
                let mut i = 0;
                let mut c = b.first_child();
                while let Some(x) = c {
                    if x == f || f.is_ancestor(&x) {
                        return Some((k, i));
                    }
                    i += 1;
                    c = x.next_sibling();
                }
                None
            })
        });
        fill_rail(ui, &ui.w.peek_nav, &ui.w.peek_plist, false, view, pf.as_deref(), &navs, &ps);
        if let Some((k, i)) = at {
            let b = if k == 0 { &ui.w.peek_nav } else { &ui.w.peek_plist };
            let mut c = b.first_child();
            for _ in 0..i {
                c = c.and_then(|x| x.next_sibling());
            }
            if let Some(x) = c {
                x.grab_focus();
            }
        }
    } else {
        w::clear(&ui.w.peek_nav);
        w::clear(&ui.w.peek_plist);
    }
}

/// Draws a rail's views and projects, full or slim. The slim strip's items take no keyboard focus
/// and have no tooltips: the full form slides out over them to show what they are, and takes the
/// keyboard.
#[allow(clippy::too_many_arguments)]
fn fill_rail(ui: &Ui, nav: &gtk::Box, plist: &gtk::Box, slim: bool, view: View, pf: Option<&str>, navs: &[(View, Option<(u32, bool)>)], ps: &[ProjectSummary]) {
    w::clear(nav);
    for &(v, n) in navs {
        let b = gtk::Button::new();
        if view == v {
            b.add_css_class("on");
        }
        b.set_tooltip_text(Some(&format!("{} (key {})", v.name(), v.key())));
        let inner = w::hbox(if slim { 2 } else { 6 }, &[]);
        let name = if slim { w::label(&v.name()[..1], &["ini"]) } else { w::label(v.name(), &[]) };
        name.set_hexpand(true);
        inner.append(&name);
        b.update_property(&[gtk::accessible::Property::Label(v.name())]);
        if v == View::Questions
            && let Some((unread, answering)) = n
        {
            inner.append(&w::dot(if answering { "working" } else { "back" }));
            b.set_tooltip_text(Some(&match (answering, unread) {
                (true, _) => "Questions (key 6): answering".to_string(),
                (false, n) => format!("Questions (key 6): {} new", fmt::pl(n, "answer", "answers")),
            }));
        } else if let Some((n, warn)) = n {
            let l = w::label(&n.to_string(), &["n"]);
            if warn {
                l.add_css_class("amber");
            }
            if v == View::Agents {
                b.set_tooltip_text(Some(&format!("Agents (key 2): {n} look stale")));
            }
            inner.append(&l);
        }
        if slim {
            b.set_tooltip_text(None);
            b.set_focusable(false);
        }
        b.set_child(Some(&inner));
        let ui2 = ui.clone();
        b.connect_clicked(move |_| app::set_view(&ui2, v));
        nav.append(&b);
    }
    w::clear(plist);
    if ps.is_empty() && !slim {
        plist.append(&w::wrap("No projects yet. Add one in Projects.", &["empty-r"]));
    }
    for p in ps {
        let b = gtk::Button::new();
        if pf == Some(p.id.as_str()) {
            b.add_css_class("on");
        }
        b.update_property(&[gtk::accessible::Property::Label(&p.name)]);
        let inner = w::hbox(if slim { 4 } else { 9 }, &[]);
        let dot = project_dot(p);
        inner.append(&dot);
        let n = live_count(p);
        if slim {
            dot.set_hexpand(true);
            b.set_focusable(false);
        } else {
            b.set_tooltip_text(Some(&match view {
                View::Projects => format!("Open {}", p.name),
                View::Ideas => format!("Show {} ideas", p.name),
                _ => format!("Show {} sessions", p.name),
            }));
            let name = w::clip(&p.name, &["pn"]);
            name.set_tooltip_text(None);
            inner.append(&name);
            if p.paused.is_some() && n == 0 {
                inner.append(&w::label("paused", &["pz"]));
            }
        }
        inner.append(&w::label(&if n > 0 { n.to_string() } else { String::new() }, &["pc"]));
        b.set_child(Some(&inner));
        if !slim {
            crate::disk::rail_tip(ui, &b, &p.id); // disk.rs hook: memory and disk on hover
        }
        let ui2 = ui.clone();
        let id = p.id.clone();
        b.connect_clicked(move |_| rail_project(&ui2, &id));
        plist.append(&b);
    }
}

/// A project in the rail: in Projects, select it and open its panel; in Monitor and Ideas, filter
/// the view by it (again to clear); elsewhere, Monitor filtered by it.
fn rail_project(ui: &Ui, id: &str) {
    app::set_rail_open(ui, false);
    let view = ui.st.borrow().view;
    if view == View::Projects {
        ui.st.borrow_mut().sel = Some(id.to_string());
        app::restore_sel(ui, View::Projects, false);
        panel::open_project(ui, id, false);
        return;
    }
    {
        let mut st = ui.st.borrow_mut();
        st.proj_filter = if st.proj_filter.as_deref() == Some(id) { None } else { Some(id.to_string()) };
        st.filter = if st.proj_filter.is_some() { Filter::All } else { Filter::Live };
        st.sel = None;
    }
    if !matches!(view, View::Monitor | View::Ideas) {
        app::set_view(ui, View::Monitor);
    }
    app::render_all(ui);
}

/// The status bar: whether the data is fresh, and at most four keys for where the keyboard is;
/// the rest are behind `?`.
pub fn render_status(ui: &Ui) {
    let (view, err, last_ok, any_stale, panel) = {
        let st = ui.st.borrow();
        if st.ov.is_none() {
            return;
        }
        let in_panel = st.drawer.is_some() && gtk::prelude::GtkWindowExt::focus(&ui.window).is_some_and(|f| f.is_ancestor(&ui.w.drawer));
        let panel = match &st.drawer {
            Some(app::Drawer::Session(_)) if in_panel => Some(true),
            Some(app::Drawer::Project(_)) if in_panel => Some(false),
            _ => None,
        };
        (st.view, st.ov_error.clone(), st.last_ok, st.procs.as_ref().is_some_and(|p| agents::stale_summary(p).0 > 0), panel)
    };
    let s = &ui.w.status;
    w::clear(s);
    let conn = w::hbox(6, &["a"]);
    let tabs_unknown = app::tabs_unknown(ui);
    let secs = last_ok.map(|t| (glib::monotonic_time() - t) / 1_000_000).unwrap_or(0);
    let (dot, words, class) = if ui.demo {
        ("demo", "demo data".to_string(), "")
    } else if let Some(err) = &err {
        ("bad", err.clone(), "bad")
    } else if secs > 15 {
        ("late", format!("last checked {} ago", fmt::span(secs as u64)), "late")
    } else if tabs_unknown.is_some() {
        ("late", "can't see Konsole tabs".to_string(), "late")
    } else {
        ("ok", "live".to_string(), "")
    };
    conn.append(&w::dot(dot));
    let l = w::label(&words, &[]);
    // A long error shortens rather than pushing the keys out of the window.
    l.set_ellipsize(gtk::pango::EllipsizeMode::End);
    conn.set_hexpand(true);
    if !class.is_empty() {
        l.add_css_class(class);
    }
    conn.append(&l);
    let tip = if ui.demo {
        "Made-up sessions and agents: nothing here is real.".to_string()
    } else if let Some(why) = tabs_unknown.filter(|_| err.is_none() && secs <= 15) {
        format!("{why} Run `wake doctor` in a terminal to see why.")
    } else {
        "Wake reads your sessions and git (read only) and never writes to your projects.".to_string()
    };
    conn.set_tooltip_text(Some(&tip));
    s.append(&conn);
    // Each key with its word is one piece; in a narrow window the later ones drop out (all of them
    // are behind `?`, which always stays).
    let keys = crate::layout::Squeeze::new(4, &[]);
    keys.set_halign(gtk::Align::End);
    let peek_word = crate::peek::hint_word(ui);
    let mut n = 0;
    let mut hint = |k: &[&str], word: &str| {
        let piece = w::hbox(4, &[]);
        if *k != ["?"] {
            n += 1;
        }
        for x in k {
            // "hold" is a word before the key, not a key.
            piece.append(&if *x == "hold" { w::label(x, &["hint"]) } else { w::kbd(x) });
        }
        piece.append(&w::label(word, &["hint"]));
        piece.append(&w::label("·", &["hint", "sep"]));
        keys.add(&piece, if *k == ["?"] { 0 } else { n });
    };
    match (panel, view) {
        (Some(true), _) => {
            hint(&["o"], "go to");
            hint(&["hold", "Space"], peek_word);
            hint(&["Esc"], "back to the list");
        }
        (Some(false), _) => {
            hint(&["p"], "pause");
            hint(&["Esc"], "back to the list");
        }
        (None, View::Monitor) => {
            hint(&["Enter"], "details");
            hint(&["hold", "Space"], peek_word);
            hint(&["o"], "go to");
        }
        (None, View::Agents) => {
            hint(&["Enter"], "details");
            hint(&["hold", "Space"], peek_word);
            if any_stale {
                hint(&["Shift", "S"], "stop all stale");
            } else {
                hint(&["s"], "stop");
            }
        }
        (None, View::Projects) => {
            hint(&["Enter"], "details");
            hint(&["p"], "pause");
        }
        (None, View::Ideas) => {
            hint(&["x"], "tick");
            hint(&["s"], "start an agent");
            hint(&["d"], "done");
            hint(&["Enter"], "sort");
        }
        (None, View::History) => {
            hint(&["/"], "search");
            hint(&["Enter"], "details");
            hint(&["o"], "go to");
        }
        (None, View::Questions) => {
            hint(&["Enter"], "ask");
            hint(&["n"], "new question");
            hint(&["Esc"], "stop an answer");
        }
        (None, View::Settings) => {
            hint(&["j", "k"], "move");
            hint(&["←", "→"], "change");
            hint(&["Space"], "switch or open");
        }
    }
    hint(&["?"], "all keys");
    // The separator after the last piece goes.
    if let Some(last) = keys.last_child().and_then(|p| p.last_child()) {
        last.set_visible(false);
    }
    s.append(&keys);
}

// ---------- chips ----------

fn chip(text: &str, count: Option<usize>, on: bool, act: impl Fn() + 'static) -> gtk::Button {
    let b = gtk::Button::new();
    b.add_css_class("chip");
    if on {
        b.add_css_class("on");
    }
    let inner = w::hbox(3, &[]);
    inner.append(&gtk::Label::new(Some(text)));
    if let Some(n) = count {
        inner.append(&w::label(&n.to_string(), &["c"]));
    }
    b.set_child(Some(&inner));
    b.connect_clicked(move |_| act());
    b
}

/// What the chips would show; when it hasn't changed they are left alone (focus and hover stay).
fn chips_sig(st: &AppState) -> String {
    let Some(ov) = &st.ov else { return String::new() };
    let counts: Vec<usize> = match st.view {
        View::Monitor => {
            let base: Vec<&SessionSummary> = ov.sessions.iter().filter(|s| st.proj_filter.is_none() || s.project == st.proj_filter).collect();
            CHIPS.iter().map(|(f, _)| base.iter().filter(|s| matches(s, *f)).count()).collect()
        }
        View::Agents => st.procs.as_ref().map_or(vec![], |p| vec![agents::row_count(p, false), agents::row_count(p, true)]),
        View::Ideas => {
            let all: Vec<&Idea> = st.ideas.iter().flatten().filter(|i| st.proj_filter.is_none() || i.project == st.proj_filter).collect();
            IdeaTab::ALL.iter().map(|t| all.iter().filter(|i| IdeaTab::of(&i.state) == *t).count()).collect()
        }
        View::Projects => vec![ov.projects.iter().filter(|p| p.removed).count()],
        View::History | View::Questions | View::Settings => vec![],
    };
    format!("{:?}{:?}{:?}{}{}{:?}{:?}{}{}{counts:?}", st.view, st.filter, st.proj_filter, st.stale_only, st.full_readouts, st.idea_tab, st.picked, st.show_removed, ov.projects.len())
}

pub fn render_chips(ui: &Ui) {
    // Settings has its own title, in its column.
    ui.w.chips.set_visible(ui.st.borrow().view != View::Settings);
    let sig = chips_sig(&ui.st.borrow());
    if ui.st.borrow().chips_drawn == sig {
        return;
    }
    ui.st.borrow_mut().chips_drawn = sig;
    let chips = &ui.w.chips;
    // The keyboard was on a chip: it stays on the chosen one once they are drawn again.
    let had_focus = gtk::prelude::GtkWindowExt::focus(&ui.window).is_some_and(|f| f.is_ancestor(chips));
    if had_focus {
        let chips = chips.clone();
        glib::idle_add_local_once(move || {
            let mut c = chips.first_child();
            while let Some(x) = c {
                if x.is::<gtk::Button>() && x.has_css_class("on") {
                    x.grab_focus();
                    return;
                }
                c = x.next_sibling();
            }
        });
    }
    chips.clear();
    let st = ui.st.borrow();
    let Some(ov) = &st.ov else { return };
    let pname = |id: &str| ov.projects.iter().find(|p| p.id == id).map_or(id.to_string(), |p| p.name.clone());
    let pf = st.proj_filter.clone().map(|id| {
        let ui = ui.clone();
        let b = chip(&format!("Project: {} ✕", pname(&id)), None, false, move || {
            {
                let mut st = ui.st.borrow_mut();
                st.proj_filter = None;
                st.filter = Filter::Live;
            }
            app::render_all(&ui);
        });
        b.add_css_class("pf");
        b.set_tooltip_text(Some("Clear the project filter (Esc)"));
        b
    });
    match st.view {
        View::Monitor => {
            chips.add_next(&w::label("Sessions", &["ttl"]));
            let base: Vec<&SessionSummary> = ov.sessions.iter().filter(|s| st.proj_filter.is_none() || s.project == st.proj_filter).collect();
            for (f, l) in CHIPS {
                let n = base.iter().filter(|s| matches(s, f)).count();
                // A filter with nothing in it says nothing: hidden, unless it is the one chosen
                // (or All, which is always there to go back to).
                if n == 0 && st.filter != f && l != "All" {
                    continue;
                }
                let ui = ui.clone();
                chips.add_next(&chip(l, Some(n), st.filter == f, move || {
                    ui.st.borrow_mut().filter = f;
                    app::render_all(&ui);
                }));
            }
            if let Some(pf) = pf {
                chips.add_next(&pf);
            }
            chips.add_next(&w::spacer());
            if let Some(id) = st.proj_filter.clone() {
                let ui = ui.clone();
                chips.add_next(&w::button("Project details", None, &["sm"], move || panel::open_project(&ui, &id, true)));
            }
        }
        View::Agents => {
            chips.add_next(&w::label("Agents", &["ttl"]));
            // The counts are the rows each chip shows.
            let (n, stale) = st.procs.as_ref().map_or((0, 0), |p| (agents::row_count(p, false), agents::row_count(p, true)));
            for (only, l, count) in [(false, "Everything", n), (true, "Looks stale", stale)] {
                let ui = ui.clone();
                chips.add_next(&chip(l, Some(count), st.stale_only == only, move || {
                    ui.st.borrow_mut().stale_only = only;
                    agents::render(&ui);
                    render_chips(&ui);
                }));
            }
            chips.add_next(&w::spacer());
            let ui2 = ui.clone();
            let full = st.full_readouts;
            let b = w::button(if full { "Small waves" } else { "Full readouts" }, Some("v"), &["ghost", "sm"], move || agents::toggle_readouts(&ui2));
            b.set_tooltip_text(Some(if full { "Show a small wave per agent" } else { "Show each agent's readout as your screensaver draws it" }));
            chips.add_next(&b);
            if stale > 0 {
                let ui2 = ui.clone();
                chips.add_next(&w::button(&format!("Stop all stale ({stale})"), Some("Shift+S"), &["danger"], move || agents::stop_all_stale(&ui2)));
            }
        }
        View::Ideas => {
            chips.add_next(&w::label("Ideas", &["ttl"]));
            let all: Vec<&Idea> = st.ideas.iter().flatten().filter(|i| st.proj_filter.is_none() || i.project == st.proj_filter).collect();
            for tab in IdeaTab::ALL {
                let n = all.iter().filter(|i| IdeaTab::of(&i.state) == tab).count();
                // "Sent to an agent" shows only once something has been sent.
                if tab == IdeaTab::Sent && n == 0 && st.idea_tab != tab {
                    continue;
                }
                let ui = ui.clone();
                chips.add_next(&chip(tab.label(), Some(n), st.idea_tab == tab, move || {
                    {
                        let mut st = ui.st.borrow_mut();
                        st.idea_tab = tab;
                        st.sel = None;
                    }
                    render_chips(&ui);
                    render_view(&ui);
                }));
            }
            if let Some(pf) = pf {
                chips.add_next(&pf);
            }
            chips.add_next(&w::spacer());
            if st.idea_tab != IdeaTab::Sort {
                // Nothing to tick or send in these lists.
            } else if st.picked.is_empty() {
                let m = w::hbox(6, &[]);
                m.append(&w::kbd("x"));
                m.append(&w::label("tick for a brief", &["meta"]));
                m.append(&w::kbd("s"));
                m.append(&w::label("start an agent", &["meta"]));
                chips.add_next(&m);
            } else {
                let n = st.picked.len() as u64;
                let ui2 = ui.clone();
                let start = w::button(&format!("Start an agent on {}", fmt::pl(n, "idea", "ideas")), Some("s"), &["violet"], move || crate::start::open(&ui2, None));
                start.set_tooltip_text(Some("Opens a Konsole tab in the project's folder with the agent and this brief typed in, waiting for your Enter"));
                chips.add_next(&start);
                let ui2 = ui.clone();
                chips.add_next(&w::button("Copy as a brief", None, &["ghost", "sm"], move || {
                    ui2.st.borrow_mut().show_brief = true;
                    render_view(&ui2);
                }));
                let ui2 = ui.clone();
                chips.add_next(&w::button("Clear ticks", None, &["ghost", "sm"], move || {
                    {
                        let mut st = ui2.st.borrow_mut();
                        st.picked.clear();
                        st.show_brief = false;
                    }
                    render_chips(&ui2);
                    render_view(&ui2);
                }));
            }
        }
        View::Projects => {
            chips.add_next(&w::label("Projects", &["ttl"]));
            let removed = ov.projects.iter().filter(|p| p.removed).count();
            if removed > 0 {
                let ui = ui.clone();
                let on = st.show_removed;
                chips.add_next(&chip("Show untracked", Some(removed), on, move || {
                    ui.st.borrow_mut().show_removed = !on;
                    app::render_all(&ui);
                }));
            }
            chips.add_next(&w::spacer());
            let ui = ui.clone();
            chips.add_next(&w::button("+ Track a folder", None, &["sm"], move || {
                ui.st.borrow_mut().adding = true;
                ui.st.borrow_mut().drawn.remove(&View::Projects);
                render_view(&ui);
            }));
        }
        View::History => {
            chips.add_next(&w::label("History", &["ttl"]));
            chips.add_next(&w::spacer());
            chips.add_next(&w::label("searches titles, your prompts, files touched and Claude's recaps", &["meta"]));
        }
        View::Questions => {
            chips.add_next(&w::label("Questions", &["ttl"]));
            chips.add_next(&w::spacer());
            chips.add_next(&w::label(&format!("answered by your own {}: it searches and reads, never changes anything", crate::setview::current_ai().id()), &["meta"]));
        }
        View::Settings => {
            chips.add_next(&w::label("Settings", &["ttl"]));
            chips.add_next(&w::spacer());
            let where_ = if prefs::saves() { "changes apply at once and are kept in ~/.config/wake/settings.toml" } else { "changes apply at once; not kept in a demo or a screenshot" };
            chips.add_next(&w::label(where_, &["meta"]));
        }
    }
}

// ---------- views ----------

pub fn render_view(ui: &Ui) {
    let v = ui.st.borrow().view;
    match v {
        View::Monitor => monitor(ui),
        View::Agents => agents::render(ui),
        View::Projects => projects(ui),
        View::Ideas => ideas(ui),
        View::History => history(ui),
        View::Questions => crate::questions::render(ui),
        View::Settings => crate::setview::render(ui),
    }
    app::tick_live(ui);
}

/// Whether the view needs drawing again for `sig`; if so, clears it and says whether its list had
/// the keyboard focus. Ages are not in any signature: they are updated in place.
pub fn begin(ui: &Ui, view: View, sig: String) -> Option<bool> {
    // Rows are drawn as columns or as two-line cells, by the list's width.
    let sig = format!("{sig}|{}", app::cards(&ui.st.borrow()));
    if ui.st.borrow().drawn.get(&view) == Some(&sig) {
        return None;
    }
    ui.st.borrow_mut().drawn.insert(view, sig);
    ui.live.borrow_mut().remove(&view);
    let vw = &ui.w.views[&view];
    // A refresh must never move the reader: remember where they were and put them back.
    let adj = vw.scroll.vadjustment();
    ui.st.borrow_mut().kept_scroll.insert(view, adj.value());
    w::keep_scroll(&adj, adj.value());
    let focus = gtk::prelude::GtkWindowExt::focus(&ui.window);
    let had_focus = focus.is_some_and(|f| f.is_ancestor(&vw.list) || f == *vw.list.upcast_ref::<gtk::Widget>());
    // The rows as they were, so `finish` can fade in the ones that arrive and out the ones that go.
    let mut old = Vec::new();
    let mut c = vw.list.first_child();
    while let Some(x) = c {
        c = x.next_sibling();
        if let Ok(row) = x.downcast::<gtk::ListBoxRow>()
            && !row.widget_name().is_empty()
        {
            old.push((row.widget_name().to_string(), row));
        }
    }
    let frame = frame_of(&ui.st.borrow(), view);
    BEFORE.with(|b| b.borrow_mut().insert(view, (frame, old)));
    w::clear(&vw.list);
    w::clear(&vw.head);
    w::clear(&vw.empty);
    Some(had_focus)
}

/// A view's frame (see `frame_of`) and its rows by key, as `begin` found them.
type Before = (String, Vec<(String, gtk::ListBoxRow)>);

thread_local! {
    /// Per view: what it was showing (its filter, sort...) and its rows, from the last `begin`.
    static BEFORE: std::cell::RefCell<std::collections::HashMap<View, Before>> = Default::default();
    static FRAME: std::cell::RefCell<std::collections::HashMap<View, String>> = Default::default();
}

/// What a view is showing, apart from the data: rows only arrive or leave (and fade) when this is
/// the same, not when the person picked another filter or sort.
fn frame_of(st: &AppState, view: View) -> String {
    format!("{:?}{:?}{:?}{:?}{}{}{}", st.filter, st.proj_filter, prefs::sort_of(view), st.idea_tab, st.stale_only, st.search_q, app::compact(st))
}

/// After a view has drawn its rows: fades in the rows that arrived and out the ones that left
/// (only when the data changed under the same filter and sort), then puts the selection back.
pub fn finish(ui: &Ui, view: View, had_focus: bool) {
    let (frame, old) = BEFORE.with(|b| b.borrow_mut().remove(&view)).unwrap_or_default();
    let now = frame_of(&ui.st.borrow(), view);
    let same = FRAME.with(|f| f.borrow_mut().insert(view, now.clone())).is_some_and(|f| f == now) && frame == now;
    let list = &ui.w.views[&view].list;
    if same && crate::motion::on() && !old.is_empty() {
        let mut keys = std::collections::HashSet::new();
        let mut c = list.first_child();
        while let Some(x) = c {
            c = x.next_sibling();
            let name = x.widget_name().to_string();
            if name.is_empty() {
                continue;
            }
            if !old.iter().any(|(k, _)| *k == name) {
                crate::motion::fade_in(&x);
            }
            keys.insert(name);
        }
        for (i, (key, row)) in old.into_iter().enumerate() {
            if keys.contains(&key) {
                continue;
            }
            // Back where it was for a moment, fading, then gone. It can't be selected meanwhile.
            row.set_widget_name("");
            row.set_selectable(false);
            row.set_activatable(false);
            row.set_focusable(false);
            row.set_opacity(1.0);
            list.insert(&row, i as i32);
            let (list2, row2) = (list.clone(), row.clone());
            crate::motion::fade_out(&row, move || {
                if row2.parent().is_some() {
                    list2.remove(&row2);
                }
            });
        }
    }
    app::restore_sel(ui, view, had_focus);
    // How to open a row, shown on hovering it (Agents says it on the rows that have details).
    if app::opens_panel(view) && view != View::Agents {
        let mut c = list.first_child();
        while let Some(x) = c {
            c = x.next_sibling();
            if let Ok(row) = x.downcast::<gtk::ListBoxRow>()
                && row.is_selectable()
                && !row.widget_name().is_empty()
                && row.tooltip_text().is_none()
            {
                row.set_tooltip_text(Some(ROW_HINT));
            }
        }
    }
}

/// The hint on a row that opens the side panel.
pub const ROW_HINT: &str = "Double-click for details · Enter";

/// A heading row in a table (a day in History, a group in Agents): never selected or focused.
/// A heading in a list, with an optional dim line under it saying what the group is.
pub fn group_row(title: &str, sub: Option<&str>) -> gtk::ListBoxRow {
    let r = gtk::ListBoxRow::new();
    r.set_selectable(false);
    r.set_activatable(false);
    r.set_focusable(false);
    r.add_css_class("grouprow");
    let head = w::label(&title.to_uppercase(), &["pgrp"]);
    match sub {
        Some(sub) => {
            let b = w::vbox(0, &[]);
            b.append(&head);
            b.append(&w::wrap(sub, &["pgrpsub"]));
            r.set_child(Some(&b));
        }
        None => r.set_child(Some(&head)),
    }
    r
}

pub fn list_row(key: &str, child: &impl IsA<gtk::Widget>) -> gtk::ListBoxRow {
    let r = gtk::ListBoxRow::new();
    r.set_widget_name(key);
    r.set_child(Some(child));
    r
}

pub type Col = (&'static str, i32, bool, bool);

/// The session table's columns: what it is and where, on two lines each. With the side panel
/// open, State · where makes way so the question stays readable beside the panel (the state is
/// then a dot by the name).
fn session_cols(now: &'static str, compact: bool) -> Vec<Col> {
    // With the panel open, State · where narrows to a dot and a short place ("K2·1") rather than
    // going: every other row still says what it is doing and where.
    let state: Col = if compact { ("Where", 74, false, false) } else { ("State · where", 150, false, false) };
    vec![("Project · session", 230, false, false), state, (now, 240, true, false), ("Last", 76, false, true)]
}

/// Which of the session columns sort, lined up with `session_cols`.
fn session_keys(_compact: bool) -> Vec<Option<&'static str>> {
    vec![Some("project"), Some("state"), None, Some("last")]
}

/// A place in a few characters, for the narrow column: "K2·1", "K·3", "bg", "—".
pub fn short_loc(loc: &str) -> String {
    let l = loc.trim();
    if let Some(rest) = l.strip_prefix("Konsole") {
        let (win, tab) = rest.split_once("· tab").unwrap_or((rest, ""));
        return format!("K{}·{}", win.trim(), tab.trim());
    }
    match l {
        "background" => "bg".into(),
        "not open" | "" => "—".into(),
        "checking…" => "…".into(),
        "kept by Wake" => "kept".into(),
        other => other.chars().take(6).collect(),
    }
}

/// One session as a table row; `now` is the "Now" cell. In a narrow list, a two-line cell.
pub fn session_row(ui: &Ui, view: View, s: &SessionSummary, now: gtk::Widget, compact: bool) -> gtk::ListBoxRow {
    if app::cards(&ui.st.borrow()) {
        return session_card(ui, view, s, now);
    }
    let cols = session_cols("", compact);
    let pj = w::vbox(1, &["pj"]);
    let top = w::hbox(7, &[]);
    if let Some(tag) = project_tag(ui, s.project.as_deref()) {
        top.append(&tag);
    }
    top.append(&w::clip(&crate::disk::project_words(ui, s), &["pname"])); // disk.rs hook: "probably X"
    if view == View::Monitor
        && let Some(m) = crate::behind::pill(ui, s.project.as_deref())
    {
        top.append(&m);
    }
    pj.append(&top);
    let line = w::hbox(6, &[]);
    let agent = match s.agent {
        Agent::Claude => "claude",
        Agent::Codex => "codex",
        Agent::Gemini => "gemini",
    };
    // Most sessions are Claude's: name the agent only when it is another one.
    if s.agent != Agent::Claude {
        line.append(&w::label(agent, &["ag", agent]));
    }
    line.append(&w::clip(&s.title, &["title"]));
    if s.deleted_by_claude {
        let k = w::label("· kept by Wake", &["kept-by"]);
        k.set_tooltip_text(Some("Claude deleted its copy; Wake kept this summary"));
        line.append(&k);
    }
    pj.append(&line);
    let mut cells: Vec<(gtk::Widget, i32, bool)> = vec![(pj.upcast(), cols[0].1, false)];
    if compact {
        let (text, ..) = app::loc_for(ui, s);
        let (class, word) = w::state_word(s.state);
        let both = w::hbox(6, &["state", class]);
        both.append(&w::dot(class));
        let l = w::clip(&short_loc(&text), &["loc", "plain"]);
        l.set_tooltip_text(Some(&format!("{word} · {text}")));
        both.append(&l);
        cells.push((both.upcast(), cols[1].1, false));
    } else {
        // How Wake knows is in the badge's tooltip, kept current in place.
        let state = w::state_badge(s.state);
        state.set_tooltip_text(Some(&s.evidence));
        let id = s.id.clone();
        live_tip(ui, view, &state, move |st| session_in(st, &id).map(|s| s.evidence.clone()));
        let both = w::vbox(3, &[]);
        both.append(&state);
        both.append(&loc_cell(ui, s));
        cells.push((both.upcast(), cols[1].1, false));
    }
    let last = w::label(&fmt::ago(s.last_at.as_deref()), &["num"]);
    last.set_xalign(1.0);
    last.set_hexpand(true);
    let id = s.id.clone();
    live_text(ui, view, &last, move |st| session_in(st, &id).map(|s| fmt::ago(s.last_at.as_deref())));
    let n = cols.len();
    cells.push((now, cols[n - 2].1, true));
    cells.push((last.upcast(), cols[n - 1].1, false));
    let r = w::row(cells, &["tr"]);
    list_row(&s.id, &r)
}

/// A session as a phone's list shows it: the state dot, project and age on the first line, the
/// title and where it is on the second, then what it is doing now.
fn session_card(ui: &Ui, view: View, s: &SessionSummary, now: gtk::Widget) -> gtk::ListBoxRow {
    let card = w::vbox(3, &["pj", "card"]);
    let top = w::hbox(7, &[]);
    let (class, word) = w::state_word(s.state);
    if let Some(tag) = project_tag(ui, s.project.as_deref()) {
        top.append(&tag);
    }
    top.append(&w::clip(&crate::disk::project_words(ui, s), &["pname"]));
    if view == View::Monitor
        && let Some(m) = crate::behind::pill(ui, s.project.as_deref())
    {
        top.append(&m);
    }
    let last = w::label(&fmt::ago(s.last_at.as_deref()), &["num"]);
    let id = s.id.clone();
    live_text(ui, view, &last, move |st| session_in(st, &id).map(|s| fmt::ago(s.last_at.as_deref())));
    top.append(&last);
    card.append(&top);
    let line = w::hbox(8, &[]);
    line.append(&w::clip(&s.title, &["title"]));
    let (text, ..) = app::loc_for(ui, s);
    let l = w::label(&short_loc(&text), &["loc", "plain", "state", class]);
    l.set_tooltip_text(Some(&format!("{word} · {text}")));
    line.append(&l);
    card.append(&line);
    now.set_hexpand(true);
    w::shrinkable(&now);
    card.append(&now);
    let r = w::hbox(0, &["tr", "cardrow"]);
    card.set_hexpand(true);
    r.append(&card);
    list_row(&s.id, &r)
}

/// The table's header, or in a narrow list (two-line rows, no columns) a "Sort ▾" menu with the
/// same orders.
pub fn table_head(ui: &Ui, view: View, cols: &[Col], keys: &[Option<&'static str>]) -> gtk::Widget {
    if !app::cards(&ui.st.borrow()) {
        return sort_head(ui, view, cols, keys).upcast();
    }
    let cur = prefs::sort_of(view);
    let b = w::hbox(8, &["thead", "cardhead"]);
    let now = match &cur {
        Some((k, rev)) => {
            let name = cols.iter().zip(keys).find(|(_, x)| x.as_deref() == Some(k.as_str())).map_or(k.clone(), |(c, _)| c.0.split(" · ").next().unwrap_or(c.0).to_lowercase());
            format!("BY {}, {}", name.to_uppercase(), sort::words(k, *rev).to_uppercase())
        }
        None => "WAKE'S ORDER".into(),
    };
    let l = w::clip(&now, &["th"]);
    b.append(&l);
    let menu = w::vbox(2, &["sortmenu"]);
    let pop = gtk::Popover::new();
    let add = |label: String, next: Option<(String, bool)>, on: bool| {
        let item = gtk::Button::with_label(&label);
        item.add_css_class("menuitem");
        if on {
            item.add_css_class("on");
        }
        let (ui, pop) = (ui.clone(), pop.clone());
        item.connect_clicked(move |_| {
            pop.popdown();
            prefs::set_sort(&ui, view, &next);
            ui.st.borrow_mut().drawn.remove(&view);
            render_view(&ui);
        });
        menu.append(&item);
    };
    add("Wake's order".into(), None, cur.is_none());
    for ((title, ..), key) in cols.iter().zip(keys) {
        let Some(key) = key else { continue };
        let name = title.split(" · ").next().unwrap_or(title);
        for rev in [false, true] {
            let on = cur.as_ref().is_some_and(|(k, r)| k == key && *r == rev);
            add(format!("{name}: {}", sort::words(key, rev)), Some((key.to_string(), rev)), on);
        }
    }
    pop.set_child(Some(&menu));
    let mb = gtk::MenuButton::new();
    mb.set_label("Sort");
    mb.add_css_class("sortbtn");
    mb.set_popover(Some(&pop));
    mb.set_tooltip_text(Some("Change the order"));
    b.append(&mb);
    b.upcast()
}

fn loc_cell(ui: &Ui, s: &SessionSummary) -> gtk::Widget {
    let (text, class, go, tip) = app::loc_for(ui, s);
    if go {
        let b = gtk::Button::with_label(&text);
        if let Some(l) = b.child().and_downcast::<gtk::Label>() {
            l.set_ellipsize(gtk::pango::EllipsizeMode::End);
        }
        b.add_css_class("loc");
        if !class.is_empty() {
            b.add_css_class(class);
        }
        b.set_tooltip_text(Some(&tip));
        let ui = ui.clone();
        let id = s.id.clone();
        b.connect_clicked(move |_| app::go_to(&ui, &id));
        b.set_halign(gtk::Align::Start);
        b.upcast()
    } else {
        let l = w::label(&text, &["loc", class]);
        l.set_ellipsize(gtk::pango::EllipsizeMode::End);
        if !tip.is_empty() {
            l.set_tooltip_text(Some(&tip));
        }
        l.set_halign(gtk::Align::Start);
        l.upcast()
    }
}

/// What the "Now" cell says, as (pango markup, tooltip, whether it is a question).
pub fn now_text(s: &SessionSummary) -> (String, String, bool) {
    let e = w::esc;
    if s.state == State::Waiting
        && let Some(q) = s.question.as_deref()
    {
        let lead = if crate::why::asks_question(s) { "Asks:" } else { "Wants a decision:" };
        return (format!("{lead} {}", e(&fmt::clip(q, 400))), q.to_string(), true);
    }
    let prompt = s.last_prompt.as_deref().map(|p| (format!("{} {}", dim("you asked:"), e(&fmt::clip(p, 300))), p.to_string()));
    let recap = s.away_summary.as_deref().map(fmt::recap).filter(|r| !r.is_empty()).map(|r| (format!("{} {}", dim("recap:"), e(&fmt::clip(&r, 300))), r));
    let (m, tip) = if s.state == State::Working { prompt.or(recap) } else { recap.or(prompt) }.unwrap_or_else(|| (dim("nothing asked yet"), String::new()));
    // Why it stopped comes first: "finished, handed back · recap: …".
    match crate::why::stop_reason(s) {
        Some(r) => {
            let words = if r.class == "dim" { dim(&e(&r.words)) } else { e(&r.words) };
            (format!("{words} {} {m}", dim("·")), format!("{}\n{tip}", r.words), false)
        }
        None => (m, tip, false),
    }
}

/// The "Now" cell: the question when it is asking (amber, up to two lines), else what you last
/// asked or Claude's recap, with running subagents as a small pill.
pub fn now_cell(s: &SessionSummary) -> gtk::Widget {
    let (m, tip, question) = now_text(s);
    let cell = w::hbox(8, &[]);
    if s.state == State::Working && s.subagents.running > 0 {
        let pill = w::label(&crate::why::sub_count(&s.subagents), &["pill"]);
        pill.set_tooltip_text(Some("Its subagents: the running ones first in its details (Enter)"));
        pill.set_valign(gtk::Align::Center);
        cell.append(&pill);
    }
    let l = if question {
        let l = w::wrap("", &["happ", "ask"]);
        l.set_lines(2);
        l.set_ellipsize(gtk::pango::EllipsizeMode::End);
        l
    } else {
        w::clip("", &["happ"])
    };
    l.set_markup(&m);
    l.set_tooltip_text((!tip.is_empty()).then_some(tip.as_str()));
    cell.append(&l);
    cell.upcast()
}

/// What a session row shows that, when it changes, is worth a redraw (no ages, no evidence).
fn stable_key(s: &SessionSummary) -> String {
    let key = (&s.id, s.state, &s.title, &s.project_name, &s.location, &s.last_prompt, &s.question, &s.away_summary, s.subagents.running, s.deleted_by_claude, &s.resume_command, &s.started_at);
    serde_json::to_string(&key).unwrap_or_default()
}

/// Whether Wake is still finding the tabs or has given up: the rows' "Where" words change with it
/// even when no session does.
fn loc_sig(ui: &Ui) -> String {
    format!("{}{}", app::unsure(ui), app::tabs_unknown(ui).is_some())
}

fn monitor(ui: &Ui) {
    let (rows, total, filter, pf, pname, compact) = {
        let st = ui.st.borrow();
        let Some(ov) = &st.ov else { return };
        let mut rows: Vec<SessionSummary> = ov.sessions.iter().filter(|s| (st.proj_filter.is_none() || s.project == st.proj_filter) && matches(s, st.filter)).cloned().collect();
        rows.sort_by(by_session);
        if let Some((k, rev)) = prefs::sort_of(View::Monitor) {
            sort::sessions(&mut rows, &k, rev, by_session);
        }
        let pname = st.proj_filter.as_ref().map(|id| ov.projects.iter().find(|p| &p.id == id).map_or(id.clone(), |p| p.name.clone()));
        (rows, ov.sessions.len(), st.filter, st.proj_filter.clone(), pname, app::compact(&st))
    };
    let stable: Vec<String> = rows.iter().map(stable_key).collect();
    let colours: Vec<String> = ui.st.borrow().ov.as_ref().map(|o| o.projects.iter().map(crate::setview::colour_of).collect()).unwrap_or_default();
    let sig = format!("{filter:?}{pf:?}{compact}{stable:?}{}{:?}{colours:?}{}", crate::behind::sig(ui), prefs::sort_of(View::Monitor), loc_sig(ui));
    let Some(had_focus) = begin(ui, View::Monitor, sig) else { return };
    let vw = &ui.w.views[&View::Monitor];
    if total == 0 {
        vw.empty.append(&w::empty("No sessions in the last 14 days.", "Start Claude in any project. It shows up here within a few seconds."));
        return;
    }
    if rows.is_empty() {
        let what = match filter {
            Filter::Live => "Nothing is open right now".to_string(),
            Filter::All => "No sessions".into(),
            f => format!("No {} sessions", CHIPS.iter().find(|c| c.0 == f).map_or("", |c| c.1).to_lowercase()),
        };
        let b = w::empty(&format!("{what}{}.", pname.map(|n| format!(" in {n}")).unwrap_or_default()), "Everything is quiet here.");
        let ui2 = ui.clone();
        let btn = w::button("Show all sessions", None, &[], move || {
            ui2.st.borrow_mut().filter = Filter::All;
            app::render_all(&ui2);
        });
        btn.set_halign(gtk::Align::Center);
        b.append(&btn);
        vw.empty.append(&b);
        return;
    }
    vw.head.append(&table_head(ui, View::Monitor, &session_cols("Now", compact), &session_keys(compact)));
    for s in &rows {
        vw.list.append(&session_row(ui, View::Monitor, s, now_cell(s), compact));
    }
    finish(ui, View::Monitor, had_focus);
}

/// A project's git loose ends in plain words (Pango markup).
pub fn git_words(g: &GitTotals) -> String {
    let mut out = Vec::new();
    if g.ahead_total > 0 {
        out.push(amber(&format!("{} not on GitHub", fmt::pl(g.ahead_total, "commit", "commits"))));
    }
    if g.dirty_files > 0 {
        out.push(amber(&format!("{} edited, not committed", fmt::pl(g.dirty_files, "file", "files"))));
    }
    if g.no_upstream > 0 {
        out.push(format!("{} only on this computer", fmt::pl(g.no_upstream, "branch", "branches")));
    }
    if out.is_empty() { dim("all on GitHub") } else { out.join("\n") }
}

/// The projects table's columns and which of them sort; "Open" only while some project has a
/// session open.
fn project_cols(compact: bool, any_open: bool) -> (Vec<Col>, Vec<Option<&'static str>>) {
    let all: [(Col, Option<&'static str>); 5] = [
        (("Project", 220, false, false), Some("project")),
        (("Next step", 220, true, false), None),
        (("Open", 130, false, false), Some("open")),
        (("Loose ends in git", 230, false, false), Some("git")),
        (("Last touched", 96, false, true), Some("last")),
    ];
    all.into_iter().enumerate().filter(|(i, _)| !(compact && matches!(i, 2 | 3)) && (any_open || *i != 2)).map(|(_, c)| c).unzip()
}

fn projects(ui: &Ui) {
    let (list, adding, compact) = {
        let st = ui.st.borrow();
        let Some(ov) = &st.ov else { return };
        let mut list: Vec<ProjectSummary> = ov.projects.iter().filter(|p| st.show_removed || !p.removed).cloned().collect();
        list.sort_by(by_project);
        if let Some((k, rev)) = prefs::sort_of(View::Projects) {
            crate::disk::sort_projects(&mut list, &k, rev, by_project, ov); // disk.rs hook: disk and memory sort
        }
        (list, st.adding, app::compact(&st) || st.lay.tight)
    };
    // Never rebuild under someone typing a folder into "Track a folder".
    let vw = &ui.w.views[&View::Projects];
    if adding && (app::typing_in(ui, &vw.top) || app::typing_in(ui, &vw.list)) {
        return;
    }
    let mut sig_list = list.clone();
    for p in &mut sig_list {
        p.last_at = None;
    }
    let sig = format!("{adding}{compact}{}{}{:?}", serde_json::to_string(&sig_list).unwrap_or_default(), crate::behind::sig(ui), prefs::sort_of(View::Projects));
    let Some(had_focus) = begin(ui, View::Projects, sig) else { return };
    let vw = &ui.w.views[&View::Projects];
    w::clear(&vw.top);
    vw.top.append(&crate::disk::machine_line(ui)); // disk.rs hook: the machine's disk line
    if adding {
        let row = w::hbox(8, &["addrow"]);
        row.append(&w::label("Folder", &["meta"]));
        let entry = gtk::Entry::builder().placeholder_text("~/Projects/something").hexpand(true).build();
        row.append(&entry);
        let err = w::label("", &["err"]);
        let add = {
            let ui = ui.clone();
            let entry = entry.clone();
            let err = err.clone();
            move || {
                let path = entry.text().trim().to_string();
                if path.is_empty() {
                    return;
                }
                let err = err.clone();
                app::bg(
                    &ui,
                    move |src| src.add_project(&path),
                    move |ui, r| match r {
                        Ok(p) => {
                            app::toast(ui, &format!("Tracking {}.", p.name), false);
                            ui.st.borrow_mut().adding = false;
                            app::refresh(ui);
                            ui.st.borrow_mut().drawn.remove(&View::Projects);
                            projects(ui);
                        }
                        Err(e) => err.set_text(&e),
                    },
                );
            }
        };
        let add2 = add.clone();
        entry.connect_activate(move |_| add2());
        row.append(&w::button("Track it", None, &["primary"], add));
        let ui2 = ui.clone();
        row.append(&w::button("Cancel", None, &["ghost"], move || {
            ui2.st.borrow_mut().adding = false;
            ui2.st.borrow_mut().drawn.remove(&View::Projects);
            projects(&ui2);
        }));
        row.append(&err);
        vw.top.append(&row);
        entry.grab_focus();
    }
    if list.is_empty() {
        vw.empty.append(&w::empty("No projects yet.", "Wake finds git folders under ~/Projects on its first run. Track any other folder with the button above."));
        return;
    }
    let any_open = list.iter().any(|p| p.live.waiting + p.live.working + p.live.idle > 0);
    let (mut cols, mut keys) = project_cols(compact, any_open);
    crate::disk::add_cols(&mut cols, &mut keys, compact); // disk.rs hook: DISK and MEMORY columns
    vw.head.append(&table_head(ui, View::Projects, &cols, &keys));
    let cards = app::cards(&ui.st.borrow());
    for p in &list {
        let pj = w::vbox(1, &["pj"]);
        let line = w::hbox(7, &[]);
        line.append(&project_dot(p));
        line.append(&w::clip(&p.name, &["pname"]));
        if let Some(m) = crate::behind::pill(ui, Some(&p.id)) {
            line.append(&m);
        }
        if let Some(pause) = &p.paused {
            let pill = w::label(&format!("paused {}", fmt::short_when(Some(&pause.at))), &["pill", "paused"]);
            pill.set_tooltip_text(Some(&pause.note));
            line.append(&pill);
        }
        pj.append(&line);
        let sub = if p.desc.is_empty() { p.roots.first().map(|r| fmt::folder_hint(r, &p.name)).unwrap_or_default() } else { p.desc.clone() };
        if let Some(l) = crate::behind::row_line(ui, &p.id) {
            pj.append(&l);
        } else if !sub.is_empty() {
            pj.append(&w::clip(&sub, &["desc"]));
        }
        let next: gtk::Widget = if p.removed {
            let ui = ui.clone();
            let id = p.id.clone();
            w::button("Track again", None, &["sm"], move || restore_project(&ui, &id)).upcast()
        } else if !p.next.is_empty() {
            w::clip(&p.next, &["happ"]).upcast()
        } else if let Some(sum) = p.last_summary.as_deref().map(fmt::recap).filter(|s| !s.is_empty()) {
            let l = w::clip(&sum, &["happ"]);
            l.set_markup(&format!("{} {}", dim("last time:"), w::esc(&fmt::clip(&sum, 300))));
            l.upcast()
        } else {
            w::label("", &["happ"]).upcast()
        };
        if cards {
            // Two lines, as a phone's list: the name and its age, then the next step (or the line
            // under the name when there is none).
            let last = w::label(&fmt::ago(p.last_at.as_deref()), &["num"]);
            let id = p.id.clone();
            live_text(ui, View::Projects, &last, move |st| st.ov.as_ref()?.projects.iter().find(|x| x.id == id).map(|p| fmt::ago(p.last_at.as_deref())));
            line.append(&last);
            if !p.next.is_empty() || p.removed {
                w::shrinkable(&next);
                pj.append(&next);
            }
            pj.set_hexpand(true);
            let r = w::hbox(0, &["tr", "cardrow"]);
            pj.add_css_class("card");
            r.append(&pj);
            if p.removed {
                r.add_css_class("gone");
            }
            vw.list.append(&list_row(&p.id, &r));
            continue;
        }
        let mut cells: Vec<(gtk::Widget, i32, bool)> = vec![(pj.upcast(), cols[0].1, false), (next, cols[1].1, true)];
        if !compact {
            // Plain words; amber only for the one that needs you. The row's dot already has the colour.
            let mut open = Vec::new();
            if p.live.waiting > 0 {
                open.push(amber(&format!("{} waiting", p.live.waiting)));
            }
            if p.live.working > 0 {
                open.push(format!("{} working", p.live.working));
            }
            if p.live.idle > 0 {
                open.push(format!("{} idle", p.live.idle));
            }
            let words = if p.git_checked { git_words(&p.git) } else { dim("checking git…") };
            if any_open {
                cells.push((w::markup(&open.join(" · "), &["gitc"]).upcast(), cols[2].1, false));
            }
            cells.push((w::markup(&words, &["gitc"]).upcast(), cols[if any_open { 3 } else { 2 }].1, false));
        }
        let last = w::label(&fmt::ago(p.last_at.as_deref()), &["num"]);
        last.set_xalign(1.0);
        last.set_hexpand(true);
        let id = p.id.clone();
        live_text(ui, View::Projects, &last, move |st| st.ov.as_ref()?.projects.iter().find(|x| x.id == id).map(|p| fmt::ago(p.last_at.as_deref())));
        cells.extend(crate::disk::project_cells(ui, p, compact)); // disk.rs hook
        cells.push((last.upcast(), cols[cols.len() - 1].1, false));
        let r = w::row(cells, &["tr"]);
        if p.removed {
            r.add_css_class("gone");
        }
        vw.list.append(&list_row(&p.id, &r));
    }
    finish(ui, View::Projects, had_focus);
}

pub fn restore_project(ui: &Ui, id: &str) {
    let id = id.to_string();
    app::bg(
        ui,
        move |src| src.remove_project(&id, false),
        |ui, r| match r {
            Ok(p) => {
                app::toast(ui, &format!("Tracking {} again.", p.name), false);
                app::refresh(ui);
            }
            Err(e) => app::toast(ui, &e, true),
        },
    );
}

/// The brief for the ticked ideas, as the agent dialog would start it (one lead line per project).
fn brief_text(ideas: &[Idea], picked: &[String]) -> String {
    let list: Vec<&Idea> = ideas.iter().filter(|i| picked.contains(&i.id)).collect();
    let mut names: Vec<String> = Vec::new();
    for i in &list {
        let n = i.project_name.clone().unwrap_or_else(|| "no project yet".into());
        if !names.contains(&n) {
            names.push(n);
        }
    }
    names
        .iter()
        .map(|n| {
            let texts: Vec<&str> = list.iter().filter(|i| i.project_name.as_deref().unwrap_or("no project yet") == n).map(|i| i.text.as_str()).collect();
            brief::brief_text(n, &texts)
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn ideas(ui: &Ui) {
    let (idea_list, tab, pf, picked, brief, projects) = {
        let st = ui.st.borrow();
        (
            st.ideas.clone(),
            st.idea_tab,
            st.proj_filter.clone(),
            st.picked.clone(),
            st.show_brief,
            st.ov.as_ref().map(|ov| ov.projects.iter().filter(|p| !p.removed).map(|p| (p.id.clone(), p.name.clone())).collect::<Vec<_>>()).unwrap_or_default(),
        )
    };
    let sig = format!("{tab:?}{pf:?}{picked:?}{brief}{}", serde_json::to_string(&idea_list).unwrap_or_default());
    let Some(had_focus) = begin(ui, View::Ideas, sig) else { return };
    let vw = &ui.w.views[&View::Ideas];
    w::clear(&vw.top);
    let Some(all) = idea_list else {
        vw.empty.append(&w::label("Loading ideas…", &["loading"]));
        return;
    };
    if brief && !picked.is_empty() {
        let b = w::vbox(8, &["brief"]);
        b.append(&w::sec("Brief for an agent · check it, then copy"));
        let text = brief_text(&all, &picked);
        let pre = w::wrap(&text, &["pre"]);
        pre.set_selectable(true);
        b.append(&pre);
        let acts = w::hbox(8, &[]);
        let ui2 = ui.clone();
        acts.append(&w::button("Copy brief", None, &["violet"], move || {
            if let Some(d) = gdk::Display::default() {
                d.clipboard().set_text(&text);
            }
            app::toast(&ui2, "Brief copied. Your ideas stay where they are.", false);
        }));
        let ui2 = ui.clone();
        acts.append(&w::button("Close", None, &["ghost"], move || {
            ui2.st.borrow_mut().show_brief = false;
            ideas(&ui2);
        }));
        acts.append(&w::label("the ideas stay where they are", &["meta"]));
        b.append(&acts);
        vw.top.append(&b);
    }
    let list: Vec<&Idea> = all.iter().filter(|i| (pf.is_none() || i.project == pf) && IdeaTab::of(&i.state) == tab).collect();
    if list.is_empty() {
        let e = match tab {
            IdeaTab::Done => w::empty("Nothing marked done yet.", ""),
            IdeaTab::Sent => w::empty("Nothing sent to an agent yet.", "Tick ideas in To sort, then s starts an agent on them."),
            IdeaTab::Sort => w::empty("No ideas to sort.", "Press c anywhere and type. Enter keeps it here."),
        };
        vw.empty.append(&e);
        return;
    }
    let mut names = vec!["Unsorted".to_string()];
    names.extend(projects.iter().map(|p| p.1.clone()));
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    for i in list {
        let card = w::hbox(10, &["idea"]);
        if i.state == "done" {
            card.add_css_class("done");
        }
        if i.state == "sent" {
            card.add_css_class("sent");
        }
        if tab == IdeaTab::Sort {
            let check = gtk::CheckButton::new();
            check.set_active(picked.contains(&i.id));
            check.set_tooltip_text(Some("Tick for a brief (x)"));
            check.update_property(&[gtk::accessible::Property::Label(&format!("Tick for a brief: {}", fmt::clip(&i.text, 60)))]);
            {
                let ui = ui.clone();
                let id = i.id.clone();
                check.connect_toggled(move |_| toggle_pick(&ui, &id));
            }
            card.append(&check);
        }
        let text = w::vbox(2, &[]);
        text.set_hexpand(true);
        text.append(&w::wrap(&i.text, &["tx"]));
        let source = match i.source.as_str() {
            "typed" => "typed",
            "shell" => "from the shell",
            "prompt" => "from a prompt",
            s => s,
        };
        let meta = match (&i.sent_at, i.state.as_str()) {
            (Some(at), "sent") => format!("sent to an agent {} · {} · noted {}", fmt::when(Some(at)), i.project_name.as_deref().unwrap_or("no project"), fmt::when(Some(&i.at))),
            _ => format!("{} · {}", fmt::when(Some(&i.at)), source),
        };
        let meta_l = w::label(&meta, &["mt"]);
        meta_l.set_ellipsize(gtk::pango::EllipsizeMode::End);
        text.append(&meta_l);
        card.append(&text);
        // A narrow list puts the project and the button on a line of their own under the idea.
        let narrow = app::cards(&ui.st.borrow());
        let tools = if narrow { w::hbox(10, &["ideatools"]) } else { card.clone() };
        let dd = gtk::DropDown::from_strings(&refs);
        dd.update_property(&[gtk::accessible::Property::Label(&format!("Project for: {}", fmt::clip(&i.text, 60)))]);
        dd.add_css_class("mini");
        dd.set_valign(gtk::Align::Center);
        let at = i.project.as_ref().and_then(|p| projects.iter().position(|x| &x.0 == p)).map_or(0, |n| n + 1);
        dd.set_selected(at as u32);
        {
            let ui = ui.clone();
            let id = i.id.clone();
            let projects = projects.clone();
            dd.connect_selected_notify(move |dd| {
                let sel = dd.selected() as usize;
                let project = if sel == 0 { None } else { projects.get(sel - 1).map(|p| p.0.clone()) };
                let name = if sel == 0 { "Unsorted".to_string() } else { projects.get(sel - 1).map(|p| p.1.clone()).unwrap_or_default() };
                set_idea(&ui, &id, None, Some(project), format!("Moved to {name}."));
            });
        }
        tools.append(&dd);
        let ui2 = ui.clone();
        let id = i.id.clone();
        let btn = if i.state != "inbox" {
            w::button("Back to sort", None, &["sm"], move || set_idea(&ui2, &id, Some("inbox"), None, "Back in the list to sort.".into()))
        } else {
            // The d key cap shows on the selected row only (style.css).
            w::button("Done", Some("d"), &["sm", "done"], move || set_idea(&ui2, &id, Some("done"), None, "Marked done.".into()))
        };
        btn.set_valign(gtk::Align::Center);
        tools.append(&btn);
        let row = if narrow {
            let both = w::vbox(8, &[]);
            card.set_css_classes(&["ideatop"]);
            tools.set_halign(gtk::Align::End);
            let outer = w::vbox(0, &["idea"]);
            for c in ["done", "sent"] {
                if i.state == c {
                    outer.add_css_class(c);
                }
            }
            both.append(&card);
            both.append(&tools);
            outer.append(&both);
            list_row(&i.id, &outer)
        } else {
            list_row(&i.id, &card)
        };
        row.add_css_class("idea-row");
        vw.list.append(&row);
    }
    finish(ui, View::Ideas, had_focus);
}

fn set_idea(ui: &Ui, id: &str, state: Option<&'static str>, project: Option<Option<String>>, msg: String) {
    let id = id.to_string();
    let id2 = id.clone();
    app::bg(
        ui,
        move |src| src.set_idea(&id, state, project.as_ref().map(|p| p.as_deref())),
        move |ui, r| {
            match r {
                Ok(idea) => {
                    app::toast(ui, &msg, false);
                    let mut st = ui.st.borrow_mut();
                    if let Some(i) = st.ideas.as_mut().and_then(|l| l.iter_mut().find(|i| i.id == id2)) {
                        *i = idea;
                    }
                    if state != Some("inbox") && state.is_some() {
                        st.picked.retain(|p| p != &id2);
                    }
                }
                Err(e) => app::toast(ui, &e, true),
            }
            app::refresh(ui);
            render_chips(ui);
            render_view(ui);
        },
    );
}

pub fn toggle_pick(ui: &Ui, id: &str) {
    {
        let mut st = ui.st.borrow_mut();
        if let Some(at) = st.picked.iter().position(|p| p == id) {
            st.picked.remove(at);
        } else {
            st.picked.push(id.to_string());
        }
        if st.picked.is_empty() {
            st.show_brief = false;
        }
    }
    render_chips(ui);
    // Redrawn once the toggle's own signal has finished.
    let ui = ui.clone();
    glib::idle_add_local_once(move || render_view(&ui));
}

pub fn toggle_done(ui: &Ui, id: &str) {
    let state = ui.st.borrow().ideas.as_ref().and_then(|l| l.iter().find(|i| i.id == id).map(|i| i.state.clone()));
    match state.as_deref() {
        Some("done") | Some("sent") => set_idea(ui, id, Some("inbox"), None, "Back in the list to sort.".into()),
        Some(_) => set_idea(ui, id, Some("done"), None, "Marked done.".into()),
        None => {}
    }
}

pub fn search_changed(ui: &Ui, q: String) {
    let seq = {
        // Fired from inside our own set_text too: never panic on a busy state.
        let Ok(mut st) = ui.st.try_borrow_mut() else { return };
        st.search_q = q.clone();
        st.search_seq += 1;
        st.sel = None;
        if q.trim().is_empty() {
            st.search = None;
        }
        st.search_seq
    };
    if q.trim().is_empty() {
        history(ui);
        return;
    }
    // A short pause so each key doesn't start a search.
    let ui = ui.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(220), move || {
        if ui.st.borrow().search_seq != seq {
            return;
        }
        let q2 = q.clone();
        app::bg(
            &ui,
            move |src| src.search(q2.trim()),
            move |ui, results| {
                if ui.st.borrow().search_seq != seq {
                    return;
                }
                ui.st.borrow_mut().search = Some(results);
                history(ui);
            },
        );
    });
}

/// `text` with each search term marked.
fn highlight(text: &str, q: &str) -> String {
    let terms: Vec<String> = q.split_whitespace().filter(|t| t.len() > 1).map(|t| t.to_lowercase()).collect();
    let text = fmt::clip(text, 400);
    let lower = text.to_lowercase();
    if terms.is_empty() || lower.len() != text.len() {
        return w::esc(&text);
    }
    let mut out = String::new();
    let mut at = 0;
    while at < text.len() {
        let hit = terms.iter().filter_map(|t| lower[at..].find(t.as_str()).map(|i| (at + i, t.len()))).min();
        match hit {
            Some((start, len)) if text.is_char_boundary(start) && text.is_char_boundary(start + len) => {
                out.push_str(&w::esc(&text[at..start]));
                out.push_str(&format!("<span background=\"{}\">{}</span>", theme::hex("hl"), w::esc(&text[start..start + len])));
                at = start + len;
            }
            _ => {
                out.push_str(&w::esc(&text[at..]));
                break;
            }
        }
    }
    out
}

fn history(ui: &Ui) {
    let (q, results, recent, compact) = {
        let st = ui.st.borrow();
        let mut recent: Vec<SessionSummary> = st.ov.as_ref().map(|ov| ov.sessions.clone()).unwrap_or_default();
        recent.sort_by(by_recent_day);
        let mut search = st.search.clone();
        if let Some((k, rev)) = prefs::sort_of(View::History) {
            sort::sessions(&mut recent, &k, rev, by_recent_day);
            if let Some(found) = search.as_mut() {
                // Matches keep their snippet; their order is the column's.
                let mut rows: Vec<SessionSummary> = found.iter().map(|(s, _)| s.clone()).collect();
                sort::sessions(&mut rows, &k, rev, by_recent_day);
                let mut by_id: std::collections::HashMap<String, String> = found.drain(..).map(|(s, m)| (s.id, m)).collect();
                *found = rows
                    .into_iter()
                    .map(|s| {
                        let m = by_id.remove(&s.id).unwrap_or_default();
                        (s, m)
                    })
                    .collect();
            }
        }
        (st.search_q.trim().to_string(), search, recent, app::compact(&st))
    };
    let stable: Vec<String> = recent.iter().map(stable_key).collect();
    let found: Option<Vec<(String, &String)>> = results.as_ref().map(|r| r.iter().map(|(s, m)| (stable_key(s), m)).collect());
    let sig = format!("{q}{compact}{found:?}{stable:?}{:?}{}", prefs::sort_of(View::History), loc_sig(ui));
    let Some(had_focus) = begin(ui, View::History, sig) else { return };
    let vw = &ui.w.views[&View::History];
    if q.is_empty() {
        if recent.is_empty() {
            vw.empty.append(&w::empty("Nothing to search yet.", "Sessions appear here once Wake has read them."));
            return;
        }
        let h = w::vbox(0, &[]);
        let order = match prefs::sort_of(View::History) {
            None => "BY DAY LAST USED".to_string(),
            Some((k, rev)) => format!(
                "BY {}, {}",
                match k.as_str() {
                    "project" => "PROJECT",
                    "state" => "STATE",
                    _ => "LAST USED",
                },
                sort::words(&k, rev).to_uppercase()
            ),
        };
        h.append(&w::label(&format!("{order} · TYPE ABOVE TO SEARCH"), &["isec"]));
        h.append(&table_head(ui, View::History, &session_cols("Last asked / recap", compact), &session_keys(compact)));
        vw.head.append(&h);
        // In Wake's own order the list is by day: each day gets its heading.
        let by_day = prefs::sort_of(View::History).is_none();
        let mut day = String::new();
        for s in &recent {
            if by_day {
                let d = fmt::day(s.last_at.as_deref());
                if d != day {
                    vw.list.append(&group_row(&d, None));
                    day = d;
                }
            }
            vw.list.append(&session_row(ui, View::History, s, now_cell(s), compact));
        }
    } else {
        let Some(results) = results else {
            vw.empty.append(&w::label("Searching…", &["loading"]));
            return;
        };
        if results.is_empty() {
            vw.empty.append(&w::empty(&format!("Nothing matches “{q}”."), "Try a shorter word, a file name, or something you typed to Claude."));
            return;
        }
        let h = w::vbox(0, &[]);
        h.append(&w::label(&format!("{} MATCH", fmt::pl(results.len() as u64, "SESSION", "SESSIONS")), &["isec"]));
        h.append(&table_head(ui, View::History, &session_cols("What matched", compact), &session_keys(compact)));
        vw.head.append(&h);
        for (s, m) in &results {
            let l = w::clip("", &["snip"]);
            l.set_markup(&highlight(m, &q));
            vw.list.append(&session_row(ui, View::History, s, l.upcast(), compact));
        }
    }
    finish(ui, View::History, had_focus);
}

/// Enter on an idea: the keyboard goes to its project list, to sort it.
pub fn focus_idea_project(ui: &Ui, id: &str) {
    let list = &ui.w.views[&View::Ideas].list;
    let mut child = list.first_child();
    while let Some(c) = child {
        if c.widget_name() == id {
            fn find(w: &gtk::Widget) -> Option<gtk::Widget> {
                if w.is::<gtk::DropDown>() {
                    return Some(w.clone());
                }
                let mut c = w.first_child();
                while let Some(x) = c {
                    if let Some(f) = find(&x) {
                        return Some(f);
                    }
                    c = x.next_sibling();
                }
                None
            }
            if let Some(dd) = find(&c) {
                dd.grab_focus();
            }
            return;
        }
        child = c.next_sibling();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(id: &str, state: State, started: &str, last: &str) -> SessionSummary {
        SessionSummary {
            id: id.into(),
            agent: Agent::Claude,
            project: None,
            project_name: None,
            title: id.into(),
            state,
            evidence: String::new(),
            started_at: Some(started.into()),
            last_at: Some(last.into()),
            minutes: 0,
            prompts: 0,
            last_prompt: None,
            away_summary: None,
            question: None,
            subagents: SubagentCounts::default(),
            cwd: None,
            location: None,
            resume_command: None,
            job_id: None,
            pid: None,
            deleted_by_claude: false,
        }
    }

    fn ids(v: &[SessionSummary]) -> Vec<&str> {
        v.iter().map(|s| s.id.as_str()).collect()
    }

    #[test]
    fn monitor_rows_keep_their_place_when_a_session_writes() {
        let mut v = vec![
            s("a", State::Working, "2026-11-20T08:00:00Z", "2026-11-20T09:00:00Z"),
            s("b", State::Working, "2026-11-20T07:00:00Z", "2026-11-20T09:10:00Z"),
            s("w", State::Waiting, "2026-11-19T07:00:00Z", "2026-11-19T07:00:00Z"),
        ];
        v.sort_by(by_session);
        assert_eq!(ids(&v), ["w", "a", "b"]);
        // b writes something: nothing moves.
        v[2].last_at = Some("2026-11-20T09:59:00Z".into());
        v.sort_by(by_session);
        assert_eq!(ids(&v), ["w", "a", "b"]);
    }

    #[test]
    fn lost_track_sorts_with_the_ended() {
        let mut v = vec![s("u", State::Unknown, "2026-11-20T08:00:00Z", "x"), s("e", State::Ended, "2026-11-20T09:00:00Z", "x"), s("i", State::Idle, "2026-11-01T00:00:00Z", "x")];
        v.sort_by(by_session);
        assert_eq!(ids(&v), ["i", "e", "u"]);
    }

    #[test]
    fn history_is_by_day_then_start_and_ignores_the_time_of_day() {
        let mut v = vec![
            s("old", State::Ended, "2026-11-10T08:00:00Z", "2026-11-18T08:00:00Z"),
            s("a", State::Idle, "2026-11-19T08:00:00Z", "2026-11-20T08:00:00Z"),
            s("b", State::Idle, "2026-11-20T06:00:00Z", "2026-11-20T07:00:00Z"),
        ];
        v.sort_by(by_recent_day);
        assert_eq!(ids(&v), ["b", "a", "old"]);
        v[1].last_at = Some("2026-11-20T09:00:00Z".into());
        v.sort_by(by_recent_day);
        assert_eq!(ids(&v), ["b", "a", "old"]);
    }

    fn p(id: &str, waiting: u32, working: u32, ahead: u32, last: Option<&str>) -> ProjectSummary {
        ProjectSummary {
            id: id.into(),
            name: id.into(),
            roots: vec![],
            removed: false,
            desc: String::new(),
            next: String::new(),
            why: String::new(),
            live: LiveCounts { working, waiting, idle: 0 },
            last_at: last.map(String::from),
            sessions_14d: 0,
            ideas: 0,
            git: GitTotals { ahead_total: ahead, no_upstream: 0, dirty_files: 0 },
            paused: None,
            last_summary: None,
            git_checked: true,
            color: None,
        }
    }

    #[test]
    fn projects_need_you_first_then_last_touched() {
        let mut v = [
            p("quiet-new", 0, 0, 0, Some("2026-11-20")),
            p("zeta", 0, 1, 0, Some("2026-11-20")),
            p("alpha", 0, 1, 0, Some("2026-11-01")),
            p("asks", 1, 0, 0, None),
            p("ahead", 0, 0, 3, Some("2026-11-02")),
            p("quiet-old", 0, 0, 0, Some("2026-11-01")),
        ];
        v.sort_by(by_project);
        let names: Vec<&str> = v.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(names, ["asks", "alpha", "zeta", "ahead", "quiet-new", "quiet-old"]);
    }

    #[test]
    fn now_says_what_it_is() {
        let mut x = s("a", State::Waiting, "t", "t");
        x.question = Some("Send it?".into());
        assert_eq!(now_text(&x), ("Asks: Send it?".into(), "Send it?".into(), true));
        let mut y = s("b", State::Idle, "t", "t");
        y.away_summary = Some("Done. (turn off recaps in /config)".into());
        y.last_prompt = Some("do it".into());
        y.evidence = "finished its turn, idle for 2 h".into();
        let (m, tip, q) = now_text(&y);
        assert!(m.starts_with("finished, handed back") && m.contains("recap:") && m.ends_with("Done.") && !q, "{m}");
        assert_eq!(tip, "finished, handed back\nDone.");
        let mut d = s("d", State::Waiting, "t", "t");
        d.question = Some("Next, say whether you want me to send it.".into());
        d.evidence = "its recap asks you to decide 2 min ago".into();
        assert!(now_text(&d).0.starts_with("Wants a decision: Next, say whether"));
        y.state = State::Working;
        assert!(now_text(&y).0.contains("you asked:"));
        assert_eq!(short_loc("Konsole 2 · tab 1"), "K2·1");
        assert_eq!(short_loc("Konsole · tab 3"), "K·3");
        assert_eq!(short_loc("background"), "bg");
    }

    #[test]
    fn highlight_marks_terms_and_survives_case_changes() {
        assert_eq!(highlight("Fix the Scope", "scope"), "Fix the <span background=\"#1A3A4A\">Scope</span>");
        assert_eq!(highlight("İstanbul scope", "scope"), "İstanbul scope");
    }
}
