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
    name.map_or_else(|| "no project".to_string(), str::to_string)
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
    // The view switcher: each place with its icon and name, and a count where something needs you.
    // Short of the full width it keeps its icons; phone-narrow, it moves to the bottom of the window.
    let lay = ui.st.borrow().lay;
    let phone = lay.rail == crate::layout::Rail::Hidden;
    let icons_only = lay.rail == crate::layout::Rail::Slim;
    let (view, n_ideas) = (ui.st.borrow().view, ideas);
    let n_stale = stale.map_or(0, |(n, _)| n);
    let qbadge = crate::questions::rail_badge();
    let sig = format!("{}/{}/{n_ideas}/{n_stale}/{phone}{icons_only}/{view:?}/{qbadge:?}", c.working, c.waiting);
    if ui.st.borrow().top_drawn == sig {
        return;
    }
    ui.st.borrow_mut().top_drawn = sig;
    ui.w.tabs.set_visible(!phone);
    ui.w.tabs_bottom.set_visible(phone);
    let places: [(View, &[&str], u32, &str, String); 5] = [
        (View::Monitor, w::icons::MONITOR, c.waiting, "attention", format!("{} waiting for you, {} working", c.waiting, c.working)),
        (View::Projects, w::icons::PROJECTS, 0, "", "Where you left each project".into()),
        (View::Ideas, w::icons::IDEAS, n_ideas, "", fmt::pl(n_ideas, "idea to sort", "ideas to sort")),
        (View::History, w::icons::HISTORY, 0, "", "Every session, by day; search it".into()),
        (View::Agents, w::icons::AGENTS, n_stale, "attention", if n_stale > 0 { format!("{n_stale} look stale") } else { "Agent processes and their memory".into() }),
    ];
    for (bar, bottom) in [(&ui.w.tabs, false), (&ui.w.tabs_bottom, true)] {
        w::clear(bar);
        for (v, icon, n, badge_class, tip) in &places {
            let b = gtk::Button::new();
            b.add_css_class("tab");
            if view == *v {
                b.add_css_class("on");
            }
            let inner = if bottom { w::vbox(2, &[]) } else { w::hbox(8, &[]) };
            inner.set_halign(gtk::Align::Center);
            // The icon, and beside it its count when there is one.
            let head = w::hbox(4, &[]);
            head.set_halign(gtk::Align::Center);
            head.append(&w::icon(icon));
            let badge = (*n > 0).then(|| {
                let badge = w::label(&n.to_string(), &["tabn"]);
                if !badge_class.is_empty() {
                    badge.add_css_class(badge_class);
                }
                badge.set_valign(gtk::Align::Center);
                badge
            });
            if (icons_only || bottom)
                && let Some(b) = &badge
            {
                head.append(b);
            }
            inner.append(&head);
            if !icons_only || bottom {
                // A name that can shorten, so the bar never holds the window wider than it is
                // (the layout swaps to icons a moment after a resize).
                let name = w::label(v.name(), &["tablabel"]);
                name.set_ellipsize(gtk::pango::EllipsizeMode::End);
                inner.append(&name);
            }
            if !(icons_only || bottom)
                && let Some(b) = badge
            {
                inner.append(&b);
            }
            b.set_child(Some(&inner));
            b.set_tooltip_text(Some(&format!("{} (key {}): {tip}", v.name(), v.key())));
            b.update_property(&[gtk::accessible::Property::Label(v.name())]);
            if bottom {
                b.set_hexpand(true);
            }
            let ui2 = ui.clone();
            let v = *v;
            b.connect_clicked(move |_| app::set_view(&ui2, v));
            bar.append(&b);
        }
    }
    // Ask, at the end of the bar: lit while an answer comes in, with a count of new ones.
    ui.w.counts.clear();
    let ask = gtk::Button::new();
    ask.add_css_class("flat");
    ask.add_css_class("askbtn");
    let inner = w::hbox(6, &[]);
    inner.append(&w::icon(w::icons::ASK));
    let tip = match qbadge {
        Some((_, true)) => {
            ask.add_css_class("live");
            "Ask (6): answering now".to_string()
        }
        Some((n, false)) if n > 0 => {
            inner.append(&w::label(&n.to_string(), &["tabn"]));
            format!("Ask (6): {}", fmt::pl(n, "new answer", "new answers"))
        }
        _ => "Ask your own AI a question, read-only (6)".to_string(),
    };
    ask.set_child(Some(&inner));
    ask.set_tooltip_text(Some(&tip));
    ask.update_property(&[gtk::accessible::Property::Label("Ask")]);
    if view == View::Questions {
        ask.add_css_class("on");
    }
    let ui2 = ui.clone();
    ask.connect_clicked(move |_| app::set_view(&ui2, View::Questions));
    ui.w.counts.add(&ask, 0);
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
                    // The rows "Open now" shows, lost track included.
                    View::Monitor => Some((ov.sessions.iter().filter(|s| matches(s, Filter::Live)).count() as u32, c.waiting > 0)),
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
            if v == View::Monitor {
                b.set_tooltip_text(Some(&format!("Monitor (key 1): {}", fmt::pl(n, "open session", "open sessions"))));
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
    // A banner under the title bar, only when there is something to know: made-up data, Wake
    // falling behind or failing to read, or Konsole's tabs out of sight while a session is open.
    let (err, last_ok, procs_at) = {
        let st = ui.st.borrow();
        if st.ov.is_none() {
            return;
        }
        (st.ov_error.clone(), st.last_ok, st.procs.as_ref().map(|p| p.generated_at.clone()))
    };
    let any_live = ui.st.borrow().ov.as_ref().is_some_and(|o| o.sessions.iter().any(|s| app::is_live(s.state)));
    let tabs_unknown = app::tabs_unknown(ui).filter(|_| any_live);
    let secs = last_ok.map(|t| (glib::monotonic_time() - t) / 1_000_000).unwrap_or(0);
    let (icon, words, class, tip) = if ui.demo {
        (w::icons::INFO, "You're looking at made-up data. Nothing here is real.".to_string(), "info", "Demo mode: made-up sessions and agents.".to_string())
    } else if let Some(err) = &err {
        (w::icons::WARNING, err.clone(), "bad", err.clone())
    } else if secs > 15 {
        (w::icons::WARNING, format!("Wake last checked {} ago.", fmt::span(secs as u64)), "late", "Wake reads your sessions every few seconds; it hasn't managed to lately.".to_string())
    } else if let Some(why) = tabs_unknown {
        (w::icons::WARNING, "Wake can't see Konsole's tabs, so Go to may open a new window.".to_string(), "late", format!("{why} Run wake doctor in a terminal to see why."))
    } else {
        (w::icons::INFO, String::new(), "", String::new())
    };
    let s = &ui.w.status;
    let sig = format!("{words}|{class}");
    if s.widget_name() == sig.as_str() {
        return;
    }
    s.set_widget_name(&sig);
    w::clear(s);
    s.set_visible(!words.is_empty());
    if words.is_empty() {
        return;
    }
    for c in ["info", "bad", "late"] {
        s.remove_css_class(c);
    }
    s.add_css_class(class);
    s.append(&w::icon(icon));
    let l = w::wrap(&words, &[]);
    l.set_xalign(0.0);
    s.append(&l);
    let fresh = procs_at.as_deref().map(|t| format!("\nAgent processes read {}.", fmt::ago(Some(t)))).unwrap_or_default();
    s.set_tooltip_text(Some(&format!("{tip}{fresh}")));
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
    format!("{:?}{:?}{:?}{}{:?}{:?}{}{}{counts:?}", st.view, st.filter, st.proj_filter, st.stale_only, st.idea_tab, st.picked, st.show_removed, ov.projects.len())
}

pub fn render_chips(ui: &Ui) {
    // Settings has its own title, in its column; Monitor's headline says what it is, and its
    // board needs no filters (the row comes back to show a project filter, to clear it).
    let hide = {
        let st = ui.st.borrow();
        // History's search box says what it searches.
        // Agents has its two actions on its title line.
        st.view == View::Settings || st.view == View::History || st.view == View::Agents || (st.view == View::Monitor && st.proj_filter.is_none())
    };
    ui.w.chips.set_visible(!hide);
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
            chips.add_next(&w::label("Monitor", &["ttl"]));
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
            } else {
                chips.add(&w::label(PURPOSE_MONITOR, &["meta"]), PURPOSE_PRIO);
            }
        }
        // Agents' row is hidden: its actions are on its title line (agents.rs).
        View::Agents => {}
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
                // The keys (x, s) are in the status bar.
                chips.add(&w::label(PURPOSE_IDEAS, &["meta"]), PURPOSE_PRIO);
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
            // The tab names the place; the row keeps its tools.
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
            chips.add(&w::label(PURPOSE_PROJECTS, &["meta"]), PURPOSE_PRIO);
            chips.add_next(&crate::disk::details_button(ui));
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

/// Opens a second Wake on demo data, and reaps it when it closes.
fn open_demo() {
    let Ok(exe) = std::env::current_exe() else { return };
    std::thread::spawn(move || {
        if let Ok(mut child) = std::process::Command::new(exe).arg("--demo").spawn() {
            let _ = child.wait();
        }
    });
}

/// Each screen says what it is for, in one line beside its title (History and Questions say what
/// they search and who answers, in the same place).
const PURPOSE_MONITOR: &str = "what needs you, and the way back to it";
const PURPOSE_PROJECTS: &str = "where you left each project, and what's next";
const PURPOSE_IDEAS: &str = "what you noticed, to sort into projects or hand to an agent";
/// The purpose line is the first thing a narrow header lets go of, before any button.
const PURPOSE_PRIO: i32 = 1000;

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
pub fn group_row(title: &str, n: usize, sub: Option<&str>) -> gtk::ListBoxRow {
    let r = gtk::ListBoxRow::new();
    r.set_selectable(false);
    r.set_activatable(false);
    r.set_focusable(false);
    r.add_css_class("grouprow");
    // The same heading as every section: its name, how many, and what the group is.
    let b = w::vbox(2, &["pgrp"]);
    let head = w::hbox(8, &[]);
    head.append(&w::label(title, &["heading"]));
    if n > 0 {
        head.append(&w::label(&n.to_string(), &["dim", "count"]));
    }
    b.append(&head);
    if let Some(sub) = sub {
        let l = w::wrap(sub, &["dim", "caption"]);
        l.set_xalign(0.0);
        b.append(&l);
    }
    r.set_child(Some(&b));
    r
}

pub fn list_row(key: &str, child: &impl IsA<gtk::Widget>) -> gtk::ListBoxRow {
    let r = gtk::ListBoxRow::new();
    r.set_widget_name(key);
    r.set_child(Some(child));
    r
}

pub type Col = (&'static str, i32, bool, bool);

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
    // Why it stopped comes first: "finished · recap: …". Only the why: the state column already
    // says ended or lost track, and Last says when.
    match crate::why::stop_reason(s).and_then(|r| row_reason(&r.words).map(|w| (w.to_string(), r.words.clone()))) {
        Some((words, full)) => (format!("{} {} {m}", e(&words), dim("·")), format!("{full}\n{tip}"), false),
        None => (m, tip, false),
    }
}

/// The part of a stop reason a row needs beside its state and age: "finished, handed back" and
/// "stopped mid-task" (without "(last ran 01:30)"); nothing for "lost track: …" or "ended: last
/// ran …", which the state column and Last already say.
fn row_reason(words: &str) -> Option<&str> {
    let w = words.strip_prefix("ended: ").unwrap_or(words);
    if w.starts_with("lost track") || w.starts_with("last ran") {
        return None;
    }
    Some(w.split(" (last ran").next().unwrap_or(w))
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
    let tally = monitor_tally(ui);
    let waves: Vec<u32> = rows.iter().filter_map(|s| wave_pid(ui, s)).collect();
    let sig = format!("{filter:?}{pf:?}{compact}{stable:?}{}{colours:?}{}{tally:?}{waves:?}", crate::behind::sig(ui), loc_sig(ui));
    let Some(had_focus) = begin(ui, View::Monitor, sig) else { return };
    let vw = &ui.w.views[&View::Monitor];
    w::clear(&vw.top);
    // An empty board also says what it is for, for someone who hasn't seen it full.
    let empty = |big: &str, small: &str, about: bool| {
        if !about {
            return w::empty(big, small);
        }
        let b = w::empty(big, "");
        let l = w::wrap(&format!("{small}\n\nWake lists every Claude Code and Codex session on this computer: which is working, which is waiting for you, and where it runs."), &[]);
        l.set_xalign(0.0);
        // A block about as wide as a paragraph, centred like the other empty states.
        b.append(&w::Fit::new(&l, 0, Some(520)).upcast());
        b
    };
    if total == 0 {
        let b = empty("No sessions in the last 14 days.", "Start Claude in any project. It shows up here within a few seconds.", true);
        // Seeing it full is the quickest way to learn what it is for: a second Wake on made-up data.
        let btn = w::button("See a full board, with made-up sessions", None, &["sm"], open_demo);
        btn.set_tooltip_text(Some("Opens a second Wake window on demo data (wake --demo). Nothing in it is real."));
        btn.set_halign(gtk::Align::Center);
        btn.set_margin_top(14);
        b.append(&btn);
        vw.empty.append(&b);
        return;
    }
    if rows.is_empty() {
        let what = match filter {
            Filter::Live => "Nothing is open right now".to_string(),
            Filter::All => "No sessions".into(),
            f => format!("No {} sessions", CHIPS.iter().find(|c| c.0 == f).map_or("", |c| c.1).to_lowercase()),
        };
        let b = empty(&format!("{what}{}.", pname.map(|n| format!(" in {n}")).unwrap_or_default()), "Everything is quiet here.", filter == Filter::Live);
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
    // The headline scrolls away with the board, so a long board keeps the room.
    let head = gtk::ListBoxRow::new();
    head.set_selectable(false);
    head.set_activatable(false);
    head.set_focusable(false);
    head.add_css_class("grouprow");
    head.add_css_class("herorow");
    head.set_child(Some(&hero(tally)));
    vw.list.append(&head);
    agents::follow_notes(ui);
    ui.readouts.borrow_mut().clear();
    board(ui, &rows, compact);
    finish(ui, View::Monitor, had_focus);
    agents::ensure_ticking(ui);
}

// ---------- Monitor: the board ----------
//
// Built from the same few parts as every screen: a page title, section headings, cards for what
// needs you, and boxed lists of session rows (the same rows History uses).

/// Waiting, working and idle, among the sessions the project filter lets through.
fn monitor_tally(ui: &Ui) -> (usize, usize, usize) {
    let st = ui.st.borrow();
    let Some(ov) = &st.ov else { return (0, 0, 0) };
    let mine: Vec<&SessionSummary> = ov.sessions.iter().filter(|s| st.proj_filter.is_none() || s.project == st.proj_filter).collect();
    let n = |f: &dyn Fn(&SessionSummary) -> bool| mine.iter().filter(|s| f(s)).count();
    (n(&|s| s.state == State::Waiting), n(&|s| s.state == State::Working), n(&|s| s.state == State::Idle))
}

/// The page title: how many need you, and the rest in a line under it.
fn hero((waiting, working, idle): (usize, usize, usize)) -> gtk::Box {
    let big = match waiting {
        0 => "Nothing needs you right now".to_string(),
        1 => "1 session needs you".to_string(),
        n => format!("{n} sessions need you"),
    };
    let mut rest = Vec::new();
    if working > 0 {
        rest.push(format!("{working} working"));
    }
    if idle > 0 {
        rest.push(format!("{idle} idle"));
    }
    let sub = if rest.is_empty() { "No agent is busy.".to_string() } else { format!("{}.", rest.join(" · ")) };
    page_title(&big, &sub)
}

/// A page's title and the line under it, the same on every screen that has one.
pub fn page_title(title: &str, sub: &str) -> gtk::Box {
    let b = w::vbox(4, &["pagetitle"]);
    let h = w::wrap(title, &["title-1"]);
    h.set_xalign(0.0);
    b.append(&h);
    if !sub.is_empty() {
        let l = w::wrap(sub, &["dim"]);
        l.set_xalign(0.0);
        b.append(&l);
    }
    b
}

/// The board's rows, in three groups; a group with nothing in it isn't shown.
fn board(ui: &Ui, rows: &[SessionSummary], compact: bool) {
    let vw = &ui.w.views[&View::Monitor];
    let waiting: Vec<&SessionSummary> = rows.iter().filter(|s| s.state == State::Waiting).collect();
    let working: Vec<&SessionSummary> = rows.iter().filter(|s| s.state == State::Working).collect();
    let rest: Vec<&SessionSummary> = rows.iter().filter(|s| !matches!(s.state, State::Waiting | State::Working)).collect();
    let narrow = compact || app::cards(&ui.st.borrow());
    if !waiting.is_empty() {
        vw.list.append(&section_row("Needs you", waiting.len()));
        for s in waiting {
            vw.list.append(&ask_card(ui, s));
        }
    }
    if !working.is_empty() {
        vw.list.append(&section_row("Working", working.len()));
        let n = working.len();
        for (i, s) in working.into_iter().enumerate() {
            let wave = (!narrow).then(|| wave_cell(ui, s)).flatten();
            vw.list.append(&session_item(ui, s, now_cell(s), wave, (i == 0, i + 1 == n), !narrow));
        }
    }
    if !rest.is_empty() {
        vw.list.append(&section_row("Earlier", rest.len()));
        let n = rest.len();
        for (i, s) in rest.into_iter().enumerate() {
            vw.list.append(&session_item(ui, s, now_cell(s), None, (i == 0, i + 1 == n), !narrow));
        }
    }
}

/// A section's heading and how many are in it, never selected.
pub fn section_row(title: &str, n: usize) -> gtk::ListBoxRow {
    let r = gtk::ListBoxRow::new();
    r.set_selectable(false);
    r.set_activatable(false);
    r.set_focusable(false);
    r.add_css_class("grouprow");
    r.add_css_class("sectrow");
    let b = w::hbox(8, &["sect"]);
    b.append(&w::label(title, &["heading"]));
    if n > 0 {
        b.append(&w::label(&n.to_string(), &["dim", "count"]));
    }
    r.set_child(Some(&b));
    r
}

/// The project and session on one line: the project's colour, its name, the session's title.
fn who_line(ui: &Ui, s: &SessionSummary) -> gtk::Box {
    let top = w::hbox(8, &[]);
    if let Some(tag) = project_tag(ui, s.project.as_deref()) {
        top.append(&tag);
    }
    let name = w::label(&crate::disk::project_words(ui, s), &["pname"]);
    top.append(&name);
    let t = w::label(&s.title, &["dim"]);
    t.set_ellipsize(gtk::pango::EllipsizeMode::End);
    t.set_tooltip_text(Some(&s.title));
    top.append(&t);
    if let Some(m) = crate::behind::pill(ui, s.project.as_deref()) {
        top.append(&m);
    }
    top
}

/// "just now", "2 h ago", kept current in place.
fn age_label(ui: &Ui, s: &SessionSummary) -> gtk::Label {
    let last = w::label(&fmt::ago(s.last_at.as_deref()), &["num", "dim"]);
    last.set_xalign(1.0);
    let id = s.id.clone();
    live_text(ui, View::Monitor, &last, move |st| session_in(st, &id).map(|s| fmt::ago(s.last_at.as_deref())));
    last
}

/// A session waiting for you, as a card: who, its status, the question, and the way there.
fn ask_card(ui: &Ui, s: &SessionSummary) -> gtk::ListBoxRow {
    let card = w::vbox(10, &["card", "askcard"]);
    let top = w::hbox(10, &[]);
    let who = who_line(ui, s);
    who.set_hexpand(true);
    top.append(&who);
    top.append(&age_label(ui, s));
    card.append(&top);
    let q = s.question.clone().or_else(|| s.away_summary.as_deref().map(fmt::recap)).unwrap_or_else(|| "It is waiting for you.".into());
    let ql = w::wrap(&fmt::clip(&q, 600), &["title-3"]);
    ql.set_xalign(0.0);
    ql.set_lines(4);
    ql.set_ellipsize(gtk::pango::EllipsizeMode::End);
    ql.set_tooltip_text(Some(&q));
    card.append(&ql);
    let acts = w::hbox(8, &[]);
    let (loc, _, can_go, tip) = app::loc_for(ui, s);
    let id = s.id.clone();
    let ui2 = ui.clone();
    let go = w::button(&panel::go_words(s, &loc, can_go), Some("o"), &["suggested"], move || app::go_to(&ui2, &id));
    go.set_sensitive(can_go);
    if !tip.is_empty() {
        go.set_tooltip_text(Some(&format!("{tip} (o)")));
    }
    acts.append(&go);
    let id = s.id.clone();
    let ui2 = ui.clone();
    acts.append(&w::button("Details", Some("Enter"), &[], move || panel::open_session(&ui2, &id, true)));
    acts.append(&w::spacer());
    let asks = crate::why::asks_question(s);
    acts.append(&w::state_tag(if asks { "Asks you" } else { "Wants a decision" }, "waiting"));
    card.append(&acts);
    let r = list_row(&s.id, &card);
    r.add_css_class("cardrow");
    r
}

/// The process whose note draws a working session's wave, once the processes are read.
fn wave_pid(ui: &Ui, s: &SessionSummary) -> Option<u32> {
    if s.state != State::Working {
        return None;
    }
    let st = ui.st.borrow();
    st.procs.as_ref()?.procs.iter().find(|a| a.session_id.as_deref() == Some(&s.id) && a.note.is_some()).map(|a| a.pid)
}

/// A working session's live wave, from agent-notify's note, when it has one.
fn wave_cell(ui: &Ui, s: &SessionSummary) -> Option<gtk::Widget> {
    let pid = wave_pid(ui, s)?;
    let area = crate::scope::mini(&ui.scope, pid, &format!("{}, working", s.title));
    ui.readouts.borrow_mut().push(area.clone());
    area.set_valign(gtk::Align::Center);
    Some(area.upcast())
}

/// One session as a row of a boxed list, the same on Monitor and History: an optional lead (the
/// wave), who, a second line, and at the end its status and age. `edge` is (first, last) in its list.
pub fn session_item(ui: &Ui, s: &SessionSummary, second: gtk::Widget, lead: Option<gtk::Widget>, edge: (bool, bool), wide: bool) -> gtk::ListBoxRow {
    let row = w::hbox(14, &["item"]);
    if let Some(l) = lead {
        row.append(&l);
    }
    let mid = w::vbox(4, &[]);
    mid.set_hexpand(true);
    mid.append(&who_line(ui, s));
    w::shrinkable(&second);
    mid.append(&second);
    row.append(&mid);
    let end = w::vbox(6, &[]);
    end.set_valign(gtk::Align::Center);
    let (_, word) = w::state_word(s.state);
    let tag = w::state_badge(s.state);
    tag.set_halign(gtk::Align::End);
    tag.set_tooltip_text(Some(&format!("{word}: {}", s.evidence)));
    if wide {
        end.append(&tag);
    }
    let age = age_label(ui, s);
    age.set_halign(gtk::Align::End);
    end.append(&age);
    row.append(&end);
    let r = list_row(&s.id, &row);
    r.add_css_class("itemrow");
    if edge.0 {
        r.add_css_class("first");
    }
    if edge.1 {
        r.add_css_class("last");
    }
    r
}

/// A project's git loose ends in plain words (Pango markup). "not on GitHub" only when its remote
/// is GitHub; a repo with no remote at all is a way of working, said once and quietly; and "all
/// pushed" isn't claimed when Wake couldn't ask the remote.
pub fn git_words(p: &ProjectSummary) -> String {
    let g = &p.git;
    let unpushed = if g.github { "not on GitHub" } else { "not pushed" };
    let mut out = Vec::new();
    if g.ahead_total > 0 {
        out.push(amber(&format!("{} {unpushed}", fmt::pl(g.ahead_total, "commit", "commits"))));
    }
    if g.dirty_files > 0 {
        out.push(amber(&format!("{} edited, not committed", fmt::pl(g.dirty_files, "file", "files"))));
    }
    if g.no_upstream > 0 {
        out.push(format!("{} {unpushed}", fmt::pl(g.no_upstream, "branch", "branches")));
    }
    if g.no_remote > 0 {
        out.push(dim("only on this computer (no remote)"));
    }
    match (out.is_empty(), &p.remote_problem) {
        // On two lines, like the other loose ends, so the narrow column shows all of it.
        (true, Some(why)) => dim(&glib::markup_escape_text(&why.replacen(" (", "\n(", 1))),
        (true, None) => dim("all pushed"),
        _ => out.join("\n"),
    }
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
    if adding {
        let row = w::hbox(8, &["addrow"]);
        row.append(&w::label("Folder", &["meta"]));
        let entry = gtk::Entry::builder().placeholder_text("~/Projects/something").hexpand(true).build();
        row.append(&entry);
        // Or pick it in the desktop's folder chooser: it fills the box, and Track it adds it.
        let (ui2, entry2) = (ui.clone(), entry.clone());
        row.append(&w::button("Choose…", None, &["ghost"], move || {
            let d = gtk::FileDialog::builder().title("Choose a folder to track").modal(true).build();
            let entry = entry2.clone();
            d.select_folder(Some(&ui2.window), gtk::gio::Cancellable::NONE, move |r| {
                if let Some(path) = r.ok().and_then(|f| f.path()) {
                    entry.set_text(&path.to_string_lossy());
                    entry.grab_focus();
                }
            });
        }));
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
        vw.empty.append(&w::empty("No projects yet.", "Wake finds git folders under ~/Projects and the folders your sessions ran in. Track any other folder with the button above."));
        return;
    }
    // A grid of cards, one per project, in its colour: the next step first, then what is open and
    // what is loose in git. Arrows, Enter, p and the panel work on it as on any list.
    let grid = card_grid(ui, "pgrid", 4);
    for p in &list {
        grid.insert(&project_card(ui, p), -1);
    }
    vw.list.append(&grid_row(&grid));
    finish(ui, View::Projects, had_focus);
}

/// A grid of cards that keeps the list's ways: selecting one is selecting it (`sel`), and a double
/// click or Enter does what Enter does. Each card is a `FlowBoxChild` named by its key.
fn card_grid(ui: &Ui, class: &str, max: u32) -> gtk::FlowBox {
    let grid = gtk::FlowBox::new();
    grid.add_css_class(class);
    grid.set_homogeneous(true);
    grid.set_min_children_per_line(1);
    grid.set_max_children_per_line(max);
    grid.set_column_spacing(16);
    grid.set_row_spacing(16);
    grid.set_selection_mode(gtk::SelectionMode::Single);
    grid.set_activate_on_single_click(false);
    grid.set_valign(gtk::Align::Start);
    {
        let ui = ui.clone();
        grid.connect_selected_children_changed(move |g| {
            if let Some(c) = g.selected_children().first() {
                let key = c.widget_name().to_string();
                let view = match ui.st.try_borrow_mut() {
                    Ok(mut st) => {
                        st.sel = Some(key.clone());
                        st.view
                    }
                    Err(_) => return,
                };
                app::follow_sel(&ui, view, key);
            }
        });
    }
    {
        let ui = ui.clone();
        grid.connect_child_activated(move |_, c| {
            ui.st.borrow_mut().sel = Some(c.widget_name().to_string());
            app::open_sel(&ui);
        });
    }
    grid
}

/// The list's one row holding a grid (app.rs finds it by its class to move through the cards).
fn grid_row(grid: &gtk::FlowBox) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();
    row.set_selectable(false);
    row.set_activatable(false);
    row.set_focusable(false);
    row.add_css_class("grouprow");
    row.add_css_class("gridrow");
    row.set_child(Some(grid));
    row
}

/// One project as a card: its colour along the top, its name, the next step in large type (or
/// where it was left), its open sessions, and its loose ends in git.
fn project_card(ui: &Ui, p: &ProjectSummary) -> gtk::FlowBoxChild {
    let colour = crate::setview::colour_of(p);
    let card = w::vbox(0, &["card", "pcard"]);
    let body = w::vbox(8, &["pbody"]);
    let head = w::hbox(8, &[]);
    let dot = w::dot(&format!("pc-{colour}"));
    dot.add_css_class("tag");
    head.append(&dot);
    let name = w::clip(&p.name, &["title-3"]);
    head.append(&name);
    if let Some(pause) = &p.paused {
        let pill = w::label(&format!("paused {}", fmt::short_when(Some(&pause.at))), &["pill", "paused"]);
        pill.set_tooltip_text(Some(&pause.note));
        pill.set_valign(gtk::Align::Center);
        head.append(&pill);
    }
    let last = w::label(&fmt::ago(p.last_at.as_deref()), &["num", "dim"]);
    last.set_valign(gtk::Align::Center);
    let id = p.id.clone();
    live_text(ui, View::Projects, &last, move |st| st.ov.as_ref()?.projects.iter().find(|x| x.id == id).map(|p| fmt::ago(p.last_at.as_deref())));
    head.append(&last);
    body.append(&head);
    let sub = if p.desc.is_empty() { p.roots.first().map(|r| fmt::folder_hint(r, &p.name)).unwrap_or_default() } else { p.desc.clone() };
    if !sub.is_empty() {
        body.append(&w::clip(&sub, &["dim"]));
    }
    // The next step, or where it was left.
    let (lead, text, class) = if !p.next.is_empty() {
        ("Next step", p.next.clone(), "body-large")
    } else if let Some(pause) = &p.paused {
        ("Paused with a note", pause.note.clone(), "body-large")
    } else if let Some(sum) = p.last_summary.as_deref().map(fmt::recap).filter(|s| !s.is_empty()) {
        ("Last time", sum, "dim")
    } else {
        ("Next step", "None written yet. Open it (Enter) to write one.".to_string(), "dim")
    };
    let l = w::label(lead, &["caption-heading"]);
    l.set_xalign(0.0);
    l.set_margin_top(6);
    body.append(&l);
    let t = w::wrap(&fmt::clip(&text, 400), &[class]);
    t.set_xalign(0.0);
    t.set_lines(3);
    t.set_ellipsize(gtk::pango::EllipsizeMode::End);
    t.set_tooltip_text(Some(&text));
    t.set_vexpand(true);
    t.set_valign(gtk::Align::Start);
    body.append(&t);
    // What is open, and what has come in.
    let chips = w::hbox(6, &[]);
    chips.set_margin_top(6);
    let chip = |n: u32, word: &str, class: &str| w::state_tag(&format!("{n} {word}"), class);
    if p.live.waiting > 0 {
        chips.append(&chip(p.live.waiting, "waiting", "waiting"));
    }
    if p.live.working > 0 {
        chips.append(&chip(p.live.working, "working", "working"));
    }
    if p.live.idle > 0 {
        chips.append(&chip(p.live.idle, "idle", "idle"));
    }
    if let Some(l) = crate::behind::row_line(ui, &p.id) {
        // As a tag like the others, in its whole words.
        let t = w::state_tag(&l.text(), "behind");
        t.set_tooltip_text(l.tooltip_text().as_deref());
        chips.append(&t);
    }
    if chips.first_child().is_some() {
        body.append(&chips);
    }
    if p.removed {
        let ui = ui.clone();
        let id = p.id.clone();
        body.append(&w::button("Track again", None, &["sm"], move || restore_project(&ui, &id)));
    } else {
        let words = if p.git_checked { git_words(p) } else { dim("checking git…") };
        let g = w::markup(&words.replace("\n(", " (").replace('\n', " · "), &["caption"]);
        g.set_wrap(true);
        g.set_xalign(0.0);
        g.set_max_width_chars(1);
        g.set_hexpand(true);
        body.append(&g);
    }
    card.append(&body);
    let c = gtk::FlowBoxChild::new();
    c.set_widget_name(&p.id);
    c.set_child(Some(&card));
    c.set_tooltip_text(Some("Double-click for details · Enter"));
    if p.removed {
        c.add_css_class("gone");
    }
    c
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
    // The projects too: each note's colour and project list come from them, and they may arrive
    // after the ideas.
    let colours: Vec<String> = ui.st.borrow().ov.as_ref().map(|o| o.projects.iter().map(crate::setview::colour_of).collect()).unwrap_or_default();
    let sig = format!("{tab:?}{pf:?}{picked:?}{brief}{}{projects:?}{colours:?}", serde_json::to_string(&idea_list).unwrap_or_default());
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
    // Sticky notes, each in its project's colour (violet while unsorted).
    let grid = card_grid(ui, "notegrid", 4);
    for i in list {
        let note = w::vbox(10, &["card", "note"]);
        let colour = i.project.as_deref().and_then(|id| colour_by_id(ui, id));
        if i.state == "done" {
            note.add_css_class("done");
        }
        if i.state == "sent" {
            note.add_css_class("sent");
        }
        // The top: tick it for a brief, and its project.
        let top = w::hbox(8, &[]);
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
            top.append(&check);
        }
        let dd = gtk::DropDown::from_strings(&refs);
        dd.update_property(&[gtk::accessible::Property::Label(&format!("Project for: {}", fmt::clip(&i.text, 60)))]);
        dd.add_css_class("mini");
        dd.add_css_class("notedd");
        // Its project's colour, as everywhere: a dot by the name.
        let dot = w::dot(&colour.map_or_else(|| "quiet".to_string(), |c| format!("pc-{c}")));
        dot.add_css_class("tag");
        dot.set_halign(gtk::Align::End);
        top.append(&w::spacer());
        top.append(&dot);
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
        top.append(&dd);
        note.append(&top);
        // The idea itself, large.
        let tx = w::wrap(&i.text, &["body-large", "notetx"]);
        tx.set_xalign(0.0);
        tx.set_vexpand(true);
        tx.set_valign(gtk::Align::Start);
        note.append(&tx);
        // When and how it came, and what to do with it.
        let source = match i.source.as_str() {
            "typed" => "typed",
            "shell" => "from the shell",
            "prompt" => "from a prompt",
            s => s,
        };
        let meta = match (&i.sent_at, i.state.as_str()) {
            (Some(at), "sent") => format!("sent {} · noted {}", fmt::short_when(Some(at)), fmt::short_when(Some(&i.at))),
            _ => format!("{} · {}", fmt::short_when(Some(&i.at)), source),
        };
        let foot = w::hbox(8, &[]);
        let meta_l = w::clip(&meta, &["caption", "dim"]);
        foot.append(&meta_l);
        if let Some(brief) = i.brief.clone().filter(|_| i.state == "sent") {
            // The brief it was sent in, opened in the desktop's default app.
            let b = gtk::Button::with_label("brief");
            b.add_css_class("link");
            b.set_widget_name("open-brief");
            b.set_tooltip_text(Some("Open the brief Wake sent the agent"));
            let ui2 = ui.clone();
            b.connect_clicked(move |_| open_brief(&ui2, &brief));
            foot.append(&b);
        }
        let ui2 = ui.clone();
        let id = i.id.clone();
        let btn = if i.state != "inbox" {
            w::button("Back to sort", None, &["sm", "ghost"], move || set_idea(&ui2, &id, Some("inbox"), None, "Back in the list to sort.".into()))
        } else {
            w::button("Done", Some("d"), &["sm", "ghost", "done"], move || set_idea(&ui2, &id, Some("done"), None, "Marked done.".into()))
        };
        btn.set_valign(gtk::Align::Center);
        foot.append(&btn);
        note.append(&foot);
        let c = gtk::FlowBoxChild::new();
        c.set_widget_name(&i.id);
        c.set_child(Some(&note));
        grid.insert(&c, -1);
    }
    vw.list.append(&grid_row(&grid));
    finish(ui, View::Ideas, had_focus);
}

/// Open `<data>/briefs/<id>.md` with the desktop's default app for it.
fn open_brief(ui: &Ui, id: &str) {
    let path = wake_core::store::Paths::from_env().data.join("briefs").join(format!("{id}.md"));
    if ui.demo {
        app::toast(ui, &format!("Demo: this would open the brief, {}.", fmt::tildify(&path.to_string_lossy())), false);
        return;
    }
    if !path.exists() {
        app::toast(ui, "That brief isn't there any more.", true);
        return;
    }
    let uri = gtk::gio::File::for_path(&path).uri();
    if let Err(e) = gtk::gio::AppInfo::launch_default_for_uri(&uri, None::<&gtk::gio::AppLaunchContext>) {
        app::toast(ui, &format!("Couldn't open the brief: {e}"), true);
    }
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
    // A timeline: by the day each session was last used, newest first. Search shows the matches
    // the same way, each with what matched.
    let (q, results, recent, compact) = {
        let st = ui.st.borrow();
        let mut recent: Vec<SessionSummary> = st.ov.as_ref().map(|ov| ov.sessions.clone()).unwrap_or_default();
        recent.sort_by(by_recent_day);
        (st.search_q.trim().to_string(), st.search.clone(), recent, app::compact(&st))
    };
    let stable: Vec<String> = recent.iter().map(stable_key).collect();
    let found: Option<Vec<(String, &String)>> = results.as_ref().map(|r| r.iter().map(|(s, m)| (stable_key(s), m)).collect());
    let colours: Vec<String> = ui.st.borrow().ov.as_ref().map(|o| o.projects.iter().map(crate::setview::colour_of).collect()).unwrap_or_default();
    let sig = format!("{q}{compact}{found:?}{stable:?}{colours:?}{}", loc_sig(ui));
    let Some(had_focus) = begin(ui, View::History, sig) else { return };
    let vw = &ui.w.views[&View::History];
    if q.is_empty() {
        if recent.is_empty() {
            vw.empty.append(&w::empty("Nothing to search yet.", "Sessions appear here once Wake has read them."));
            return;
        }
        // Each day a section of the same session rows Monitor uses.
        let mut days: Vec<(String, Vec<&SessionSummary>)> = Vec::new();
        for s in &recent {
            let d = fmt::day(s.last_at.as_deref());
            match days.last_mut() {
                Some((day, v)) if *day == d => v.push(s),
                _ => days.push((d, vec![s])),
            }
        }
        for (day, ss) in days {
            vw.list.append(&section_row(&day, 0));
            let n = ss.len();
            for (i, s) in ss.into_iter().enumerate() {
                vw.list.append(&session_item(ui, s, now_cell(s), None, (i == 0, i + 1 == n), !compact));
            }
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
        vw.list.append(&section_row(&fmt::pl(results.len() as u64, "session matches", "sessions match"), 0));
        let n = results.len();
        for (i, (s, m)) in results.iter().enumerate() {
            let l = w::clip("", &["snip"]);
            l.set_markup(&highlight(m, &q));
            vw.list.append(&session_item(ui, s, l.upcast(), None, (i == 0, i + 1 == n), !compact));
        }
    }
    finish(ui, View::History, had_focus);
}

/// Enter on an idea: the keyboard goes to its project list, to sort it.
pub fn focus_idea_project(ui: &Ui, id: &str) {
    let list: gtk::Widget = match app::grid_of(ui, View::Ideas) {
        Some(g) => g.upcast(),
        None => ui.w.views[&View::Ideas].list.clone().upcast(),
    };
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
            git: GitTotals { ahead_total: ahead, ..Default::default() },
            paused: None,
            last_summary: None,
            git_checked: true,
            color: None,
            remote_problem: None,
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
        assert_eq!(row_reason("stopped mid-task (last ran 01:30)"), Some("stopped mid-task"));
        assert_eq!(row_reason("ended: finished, handed back"), Some("finished, handed back"));
        assert_eq!(row_reason("ended: last ran 09:52"), None);
        assert_eq!(row_reason("lost track: says busy, but nothing has changed for 6 h"), None);
    }

    #[test]
    fn highlight_marks_terms_and_survives_case_changes() {
        assert_eq!(highlight("Fix the Scope", "scope"), "Fix the <span background=\"#1A3A4A\">Scope</span>");
        assert_eq!(highlight("İstanbul scope", "scope"), "İstanbul scope");
    }
}
