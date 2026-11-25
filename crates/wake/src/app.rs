//! The window: its parts, its state, the background refresh and the keyboard.
//!
//! Layout: a top bar, a rail of views and
//! projects, the main column (chips, the view, the capture bar), a status bar, and over them the
//! side panel, the switcher, dialogs and a toast. Data comes from a `Source` on GLib's blocking
//! pool; the UI thread only ever waits on futures.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use gtk4::{self as gtk, gdk, gio, glib, gsk, prelude::*};
use wake_core::*;

use crate::{agents, dialogs, panel, peek, prefs, scope, setview, switcher, theme, views, w};

pub const APP_ID: &str = "io.github.peterwalker78.Wake";
const POLL: Duration = Duration::from_secs(5);
const PROC_POLL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Default)]
pub struct Opts {
    pub demo: bool,
    pub screenshot: Option<PathBuf>,
    pub view: Option<String>,
    /// For screenshots: `switcher`, `switcher-subs`, `session[:id]`, `project[:id]`, `pause`,
    /// `stale` (the "Stop all stale" confirmation), `stop` (one agent's).
    pub show: Option<String>,
    /// `wake find`: bring Wake forward with Find (the switcher) open, starting it if need be.
    pub find: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum View {
    Monitor,
    Agents,
    Projects,
    Ideas,
    History,
    Questions,
    Settings,
}

impl View {
    pub const ALL: [View; 7] = [View::Monitor, View::Agents, View::Projects, View::Ideas, View::History, View::Questions, View::Settings];

    pub fn name(self) -> &'static str {
        match self {
            View::Monitor => "Monitor",
            View::Agents => "Agents",
            View::Projects => "Projects",
            View::Ideas => "Ideas",
            View::History => "History",
            View::Questions => "Questions",
            View::Settings => "Settings",
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            View::Monitor => "monitor",
            View::Agents => "agents",
            View::Projects => "projects",
            View::Ideas => "ideas",
            View::History => "history",
            View::Questions => "questions",
            View::Settings => "settings",
        }
    }

    /// Its key, as the rail and the keys overlay show it.
    pub fn key(self) -> &'static str {
        match self {
            View::Monitor => "1",
            View::Agents => "2",
            View::Projects => "3",
            View::Ideas => "4",
            View::History => "5",
            View::Questions => "6",
            View::Settings => ",",
        }
    }

    pub fn from_id(id: &str) -> Option<View> {
        View::ALL.into_iter().find(|v| v.id() == id.to_lowercase())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    Live,
    Waiting,
    Working,
    Idle,
    Ended,
    All,
}

/// The Ideas view's lists, one per state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdeaTab {
    Sort,
    Sent,
    Done,
}

impl IdeaTab {
    pub const ALL: [IdeaTab; 3] = [IdeaTab::Sort, IdeaTab::Sent, IdeaTab::Done];

    pub fn label(self) -> &'static str {
        match self {
            IdeaTab::Sort => "To sort",
            IdeaTab::Sent => "Sent to an agent",
            IdeaTab::Done => "Done",
        }
    }

    /// Which list an idea is in (an unknown state reads as to sort).
    pub fn of(state: &str) -> IdeaTab {
        match state {
            "sent" => IdeaTab::Sent,
            "done" => IdeaTab::Done,
            _ => IdeaTab::Sort,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Drawer {
    Session(String),
    Project(String),
}

#[derive(Default)]
pub struct Sw {
    pub open: bool,
    pub q: String,
    pub sel_id: Option<String>,
    pub in_subs: bool,
    pub sub_sel: usize,
    pub expanded: Option<String>,
}

pub struct AppState {
    pub ov: Option<Overview>,
    pub ov_error: Option<String>,
    pub last_ok: Option<i64>,
    pub ideas: Option<Vec<Idea>>,
    pub procs: Option<ProcOverview>,
    pub mem_available: Option<u64>,
    pub view: View,
    pub filter: Filter,
    pub proj_filter: Option<String>,
    pub stale_only: bool,
    /// The selected row's key in the current view.
    pub sel: Option<String>,
    pub drawer: Option<Drawer>,
    pub sessions: HashMap<String, SessionDetail>,
    pub projects: HashMap<String, ProjectDetail>,
    pub subs: HashMap<(String, String), SubagentDetail>,
    pub sub_open: HashSet<String>,
    pub all_prompts: bool,
    pub all_files: bool,
    /// The side panel's sections opened on demand (by name; kept while the panel shows one thing).
    pub panel_open: HashSet<&'static str>,
    pub edit_next: bool,
    pub confirm_remove: bool,
    /// Ideas: which list shows (To sort, Sent to an agent, Done).
    pub idea_tab: IdeaTab,
    pub picked: Vec<String>,
    pub show_brief: bool,
    pub show_removed: bool,
    pub adding: bool,
    pub search_q: String,
    pub search: Option<Vec<(SessionSummary, String)>>,
    pub search_seq: u64,
    pub sw: Sw,
    pub cur: Option<String>,
    pub back: Option<String>,
    pub modal_open: bool,
    pub busy_ov: bool,
    pub busy_procs: bool,
    /// What each view last drew, so a refresh that changes nothing redraws nothing.
    pub drawn: HashMap<View, String>,
    /// Where each view was scrolled when it was last rebuilt, so restoring focus can't move it.
    pub kept_scroll: HashMap<View, f64>,
    pub cap_projects: Vec<Option<String>>,
    pub cap_manual: bool,
    pub toast_gen: u64,
    pub kept_gen: u64,
    /// Agents: the screensaver's full readouts instead of the small waves (`v`).
    pub full_readouts: bool,
    /// Detailed (panel sections open, full readouts) or reduced: Settings' default, `z` for now.
    pub detailed: bool,
    /// Agents: rows opened to show their subagents, and subagents opened to show their steps.
    pub agent_open: HashSet<u32>,
    pub agent_sub_open: HashSet<String>,
    /// Agents: open rows whose finished subagents are unfolded too.
    pub agent_done_open: HashSet<u32>,
    /// The session panel: its finished subagents unfolded.
    pub subs_done_open: bool,
    /// Where each live session was last seen for sure, by session id: shown (quietly) while Wake
    /// re-checks Konsole, instead of "checking…".
    pub known_loc: HashMap<String, String>,
    /// When the overview first said it can't be sure where sessions are (monotonic µs), until it
    /// is sure again: unsure for `GIVE_UP_SECS` is a failure, not a slow first look.
    pub unsure_since: Option<i64>,
    /// What the side panel, top bar, rail and chips last drew, so a refresh that changes nothing
    /// redraws nothing (and focus, hover and scroll stay put).
    pub panel_drawn: Option<String>,
    pub top_drawn: String,
    pub rail_drawn: String,
    pub chips_drawn: String,
    /// The layout for the window's width (layout.rs), as last drawn.
    pub lay: crate::layout::Lay,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            ov: None,
            ov_error: None,
            last_ok: None,
            ideas: None,
            procs: None,
            mem_available: None,
            view: View::Monitor,
            filter: Filter::Live,
            proj_filter: None,
            stale_only: false,
            sel: None,
            drawer: None,
            sessions: HashMap::new(),
            projects: HashMap::new(),
            subs: HashMap::new(),
            sub_open: HashSet::new(),
            all_prompts: false,
            all_files: false,
            panel_open: HashSet::new(),
            edit_next: false,
            confirm_remove: false,
            idea_tab: IdeaTab::Sort,
            picked: Vec::new(),
            show_brief: false,
            show_removed: false,
            adding: false,
            search_q: String::new(),
            search: None,
            search_seq: 0,
            sw: Sw::default(),
            cur: None,
            back: None,
            modal_open: false,
            busy_ov: false,
            busy_procs: false,
            drawn: HashMap::new(),
            kept_scroll: HashMap::new(),
            cap_projects: vec![None],
            cap_manual: false,
            toast_gen: 0,
            kept_gen: 0,
            full_readouts: false,
            detailed: false,
            agent_open: HashSet::new(),
            agent_sub_open: HashSet::new(),
            agent_done_open: HashSet::new(),
            subs_done_open: false,
            known_loc: HashMap::new(),
            unsure_since: None,
            panel_drawn: None,
            top_drawn: String::new(),
            rail_drawn: String::new(),
            chips_drawn: String::new(),
            lay: Default::default(),
        }
    }
}

/// One view's widgets: a top area, a table header, and the scrolling list.
pub struct ViewW {
    pub root: gtk::Box,
    pub top: gtk::Box,
    pub head: gtk::Box,
    pub list: gtk::ListBox,
    pub scroll: gtk::ScrolledWindow,
    pub empty: gtk::Box,
    /// The selection bar that glides from row to row (over the list).
    pub glide: Rc<Glide>,
}

/// The accent bar at the selected row's left edge, gliding to the next row when the selection
/// moves (the row's own bar shows again once it arrives).
pub struct Glide {
    pub over: gtk::Overlay,
    pub bar: gtk::Box,
    /// Where the bar last was (y, height), and which row it was on.
    pub at: std::cell::Cell<Option<(f64, f64)>>,
    pub key: RefCell<String>,
    pub stop: RefCell<Option<Rc<std::cell::Cell<bool>>>>,
}

pub struct Widgets {
    /// Everything under the overlays: made insensitive while a dialog or the switcher is open, so
    /// nothing behind them can be reached.
    pub column: gtk::Box,
    /// Rail, main column and side panel, laid out for the width (layout.rs).
    pub body: crate::layout::Shell,
    pub rail: gtk::Box,
    /// ☰ (the rail as a drawer), and the search box's full and icon forms.
    pub menu_btn: gtk::Button,
    pub cmd_full: gtk::Widget,
    pub cmd_icon: gtk::Button,
    pub counts: crate::layout::Squeeze,
    /// The three places, as tabs in the top bar: Monitor, Projects, History.
    pub tabs: gtk::Box,
    pub indexing: gtk::Label,
    pub clock: gtk::Label,
    /// "DEMO DATA" in the top bar (the status bar says it too).
    pub badge: gtk::Label,
    pub nav: gtk::Box,
    pub plist: gtk::Box,
    /// The slim rail's full form, sliding out over the content (layout.rs), and its lists.
    pub peek_rail: gtk::Box,
    pub peek_nav: gtk::Box,
    pub peek_plist: gtk::Box,
    pub banner: gtk::Label,
    pub chips: crate::layout::Squeeze,
    pub stack: gtk::Stack,
    pub views: HashMap<View, ViewW>,
    pub history_entry: gtk::Entry,
    pub cap_entry: gtk::Entry,
    pub cap_to: gtk::DropDown,
    pub kept: gtk::Label,
    pub status: gtk::Box,
    pub drawer: gtk::Box,
    /// The side panel's slide in and out.
    pub drawer_rev: gtk::Revealer,
    pub switcher: gtk::Box,
    pub sw_entry: gtk::Entry,
    pub sw_list: gtk::Box,
    pub sw_inside: gtk::Box,
    pub sw_scroll: gtk::ScrolledWindow,
    pub modal: gtk::Box,
    pub toast: gtk::Label,
    /// The pointer on the toast: it stays up while it is there.
    pub toast_hover: gtk::EventControllerMotion,
}

pub struct App {
    pub src: Arc<dyn Source>,
    pub demo: bool,
    pub window: gtk::ApplicationWindow,
    pub w: Widgets,
    pub st: RefCell<AppState>,
    pub scope: scope::Shared,
    pub readouts: RefCell<Vec<gtk::DrawingArea>>,
    pub tick: RefCell<Option<gtk::TickCallbackId>>,
    /// Labels each view updates in place (ages, memory, CPU) without rebuilding its rows.
    pub live: RefCell<HashMap<View, Vec<views::Live>>>,
    /// Hold-to-peek: the hold-or-tap state and the preview.
    pub peek: RefCell<peek::Peek>,
}

pub type Ui = Rc<App>;

/// Runs `work` against the source on GLib's blocking pool, then `done` on the UI thread.
pub fn bg<T: Send + 'static>(ui: &Ui, work: impl FnOnce(&dyn Source) -> T + Send + 'static, done: impl FnOnce(&Ui, T) + 'static) {
    let src = ui.src.clone();
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        match gio::spawn_blocking(move || work(src.as_ref())).await {
            Ok(value) => done(&ui, value),
            Err(_) => {
                // The work panicked. Clear the busy flags so the next poll tries again, and say so.
                {
                    let mut st = ui.st.borrow_mut();
                    st.busy_ov = false;
                    st.busy_procs = false;
                    st.ov_error = Some("Wake hit an error reading your sessions; it will keep trying.".into());
                }
                render_all(&ui);
            }
        }
    });
}

pub fn run(opts: Opts) -> glib::ExitCode {
    // A demo or a screenshot never writes the person's settings.
    prefs::init(!opts.demo && opts.screenshot.is_none());
    let app = gtk::Application::builder().application_id(APP_ID).build();
    if opts.screenshot.is_some() || opts.demo {
        // A screenshot or a demo never hands over to (or takes over) the real window.
        app.set_flags(gio::ApplicationFlags::NON_UNIQUE);
    }
    app.connect_startup(|_| theme::load());
    // `wake find` with Wake already open: ask that window to open Find, and go.
    // Running on then just raises that window too, and exits cleanly.
    if opts.find && app.register(gio::Cancellable::NONE).is_ok() && app.is_remote() {
        app.activate_action("find", None);
        return app.run_with_args::<&str>(&[]);
    }
    app.connect_activate(move |app| {
        // A second `wake` raises the first.
        if let Some(window) = app.active_window() {
            window.present();
            return;
        }
        build(app, opts.clone());
    });
    app.run_with_args::<&str>(&[])
}

/// The views whose rows open the side panel (on a double click or Enter; a click only selects).
pub fn opens_panel(view: View) -> bool {
    matches!(view, View::Monitor | View::Agents | View::Projects | View::History)
}

fn view_widgets(view: View) -> ViewW {
    let root = w::vbox(0, &["pane", view.id()]);
    let top = w::vbox(0, &["vtop"]);
    let head = w::hbox(0, &[]);
    let list = gtk::ListBox::new();
    list.add_css_class("table");
    list.set_selection_mode(gtk::SelectionMode::Single);
    // A click selects; a double click (or Enter) opens the details. Ideas and Settings act on one.
    list.set_activate_on_single_click(!opens_panel(view));
    let empty = w::vbox(0, &[]);
    let inner = w::vbox(0, &[]);
    if view == View::Settings {
        // A centred column, 680 px at most, narrowing with the window.
        list.add_css_class("setlist");
        let col = w::Fit::new(&list, 320, Some(680)).upcast();
        col.set_halign(gtk::Align::Center);
        col.set_margin_start(24);
        col.set_margin_end(24);
        inner.append(&col);
    } else {
        inner.append(&list);
    }
    inner.append(&empty);
    let over = gtk::Overlay::new();
    over.set_child(Some(&inner));
    let bar = w::vbox(0, &["selbar"]);
    bar.set_halign(gtk::Align::Start);
    bar.set_valign(gtk::Align::Start);
    bar.set_can_target(false);
    bar.set_visible(false);
    over.add_overlay(&bar);
    let scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&over).build();
    root.append(&top);
    root.append(&head);
    root.append(&scroll);
    let glide = Rc::new(Glide { over, bar, at: Default::default(), key: Default::default(), stop: Default::default() });
    ViewW { root, top, head, list, scroll, empty, glide }
}

fn build(app: &gtk::Application, opts: Opts) {
    let demo = opts.demo || crate::source::is_stand_in();
    let src = if opts.demo { crate::source::demo_source() } else { crate::source::live_source(opts.screenshot.is_some()) };
    // Motion as Settings and the desktop want it, before anything is drawn.
    crate::motion::init();
    // A screenshot's window is the width it asks for from the start (Broadway sizes a window by its
    // default size, clamped to its screen, or by its size request).
    let shot_width: Option<i32> = opts.screenshot.as_ref().map(|_| {
        opts.show.as_deref().and_then(|s| s.strip_prefix("measure:")).and_then(|w| w.parse().ok()).or_else(|| std::env::var("WAKE_SHOT_WIDTH").ok().and_then(|w| w.parse().ok())).unwrap_or(1480)
    });
    let window = gtk::ApplicationWindow::builder().application(app).title("Wake").default_width(shot_width.unwrap_or(1480)).default_height(920).build();
    window.add_css_class("wake");
    // Slipstream doesn't maximise or minimise a window on request (it tiles them itself, and has
    // its own way to put one away), so those two buttons would do nothing there: show only close.
    // Elsewhere the desktop's own layout stands.
    if slipstream_desktop(&std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default())
        && let Some(s) = gtk::Settings::default()
    {
        s.set_gtk_decoration_layout(Some(":close"));
    }

    // Top bar: what doesn't fit drops out, least important first (the badge, the clock, then the
    // counts from the end); the search button narrows down to its keys.
    let top = crate::layout::Squeeze::new(12, &["top"]);
    // ☰: the rail, when the window is too narrow to keep it (a drawer over everything).
    let menu_btn = gtk::Button::with_label("☰");
    menu_btn.add_css_class("iconbtn");
    menu_btn.set_tooltip_text(Some("Views and projects (F6)"));
    menu_btn.update_property(&[gtk::accessible::Property::Label("Views and projects")]);
    menu_btn.set_valign(gtk::Align::Center);
    menu_btn.set_visible(false);
    top.add(&menu_btn, 0);
    top.add(&w::label("WAKE", &["logo"]), 0);
    // The three places you go, as tabs; the tools (Agents, Ideas, Ask, Settings) sit at the right.
    let tabs = w::hbox(2, &["tabs"]);
    tabs.update_property(&[gtk::accessible::Property::Label("Views")]);
    top.add(&tabs, 0);
    let cmd = gtk::Button::new();
    cmd.add_css_class("cmd");
    let cmd_in = w::hbox(8, &[]);
    let cmd_label = w::label("Go to a session…", &[]);
    cmd_label.set_hexpand(true);
    cmd_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    cmd_in.append(&cmd_label);
    cmd_in.append(&w::kbd("Ctrl"));
    cmd_in.append(&w::kbd("K"));
    cmd.set_child(Some(&cmd_in));
    cmd.set_hexpand(true);
    cmd.set_tooltip_text(Some("Go to a session (g or Ctrl+K)"));
    cmd.update_property(&[gtk::accessible::Property::Label("Go to a session")]);
    // 380 px when there is room, down to 150 in a narrow window.
    let cmd_holder = w::hbox(0, &[]);
    cmd_holder.append(&cmd);
    let cmd_full = w::Fit::new(&cmd_holder, 150, Some(300)).upcast();
    top.add(&cmd_full, 0);
    // Narrow, the search box is a 🔍 button (it opens the same switcher over the window).
    let cmd_icon = gtk::Button::new();
    cmd_icon.set_child(Some(&w::label("⌕", &["searchicon"])));
    cmd_icon.add_css_class("iconbtn");
    cmd_icon.set_tooltip_text(Some("Go to a session (g or Ctrl+K)"));
    cmd_icon.update_property(&[gtk::accessible::Property::Label("Go to a session")]);
    cmd_icon.set_valign(gtk::Align::Center);
    cmd_icon.set_visible(false);
    top.add(&cmd_icon, 0);
    top.add(&w::spacer(), 0);
    let indexing = w::label("", &["indexing"]);
    indexing.set_visible(false);
    top.add(&indexing, 5);
    // Each count drops out on its own when there is no room (the first, waiting, last).
    let counts = crate::layout::Squeeze::new(10, &["counts"]);
    counts.set_valign(gtk::Align::Center);
    // The tools that don't fit go under "More ▾".
    counts.with_more();
    top.add(&counts, 0);
    let badge = w::label("DEMO DATA", &["badge"]);
    badge.set_visible(demo);
    badge.set_tooltip_text(Some(if opts.demo { "Made-up data. Nothing you do here is saved or touches this machine." } else { "Wake's core isn't wired in yet, so this is made-up data." }));
    badge.set_valign(gtk::Align::Center);
    top.add(&badge, 4);
    let clock = w::label(&crate::fmt::clock(), &["clock"]);
    top.add(&clock, 3);
    // One bar, not two: Wake's top bar is the title bar. It drags and double-clicks like one, and
    // ends with the desktop's own window buttons (on Slipstream, close only).
    let controls = gtk::WindowControls::new(gtk::PackType::End);
    controls.set_valign(gtk::Align::Center);
    top.add(&controls, 0);
    let handle = gtk::WindowHandle::new();
    handle.set_child(Some(&top));
    // An empty title bar: GTK still gives the window its shadow and resize edges.
    window.set_titlebar(Some(&w::hbox(0, &["notitle"])));

    // Rail, and its full form that slides out over the content while it is slim.
    let rail_box = || {
        let rail = gtk::Box::builder().orientation(gtk::Orientation::Vertical).accessible_role(gtk::AccessibleRole::Navigation).build();
        rail.add_css_class("rail");
        rail.update_property(&[gtk::accessible::Property::Label("Views and projects")]);
        rail.set_size_request(crate::layout::RAIL_W, -1);
        rail.set_hexpand(false);
        rail.append(&w::label("VIEWS", &["grp"]));
        let nav = w::vbox(1, &["nav"]);
        rail.append(&nav);
        rail.append(&w::label("PROJECTS", &["grp"]));
        let plist = w::vbox(1, &["plist"]);
        let rail_scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&plist).build();
        rail.append(&rail_scroll);
        (rail, nav, plist)
    };
    let (rail, nav, plist) = rail_box();
    let (peek_rail, peek_nav, peek_plist) = rail_box();
    peek_rail.set_css_classes(&["rail", "full", "peek-rail"]);

    // Main column.
    let main = w::vbox(0, &["main"]);
    main.set_hexpand(true);
    let banner = w::wrap("", &["banner"]);
    banner.set_visible(false);
    main.append(&banner);
    // Priority+, as a phone's toolbar: the chips that fit, the rest under "More ▾" (the chosen one
    // always shows).
    let chips = crate::layout::Squeeze::with_role(7, &["chips"], gtk::AccessibleRole::Toolbar);
    chips.update_property(&[gtk::accessible::Property::Label("Filters")]);
    chips.with_more();
    main.append(&chips);
    let stack = gtk::Stack::new();
    stack.set_vexpand(true);
    stack.set_transition_type(gtk::StackTransitionType::None);
    // Only the view on show sets the column's size: a wide table in a hidden view mustn't.
    stack.set_hhomogeneous(false);
    stack.set_vhomogeneous(false);
    let mut views = HashMap::new();
    for v in View::ALL {
        let vw = view_widgets(v);
        stack.add_named(&vw.root, Some(v.id()));
        views.insert(v, vw);
    }
    let history_entry = gtk::Entry::builder().placeholder_text("Search everything you asked, every title, every file touched and every recap…").hexpand(true).build();
    history_entry.add_css_class("search");
    {
        let sb = w::hbox(10, &["searchbox"]);
        sb.append(&history_entry);
        sb.append(&w::kbd("/"));
        views[&View::History].top.append(&sb);
    }
    main.append(&stack);

    // Capture bar.
    let cap = gtk::Box::builder().spacing(10).accessible_role(gtk::AccessibleRole::Form).build();
    cap.add_css_class("capture");
    cap.update_property(&[gtk::accessible::Property::Label("Capture a thought")]);
    let cap_label = w::hbox(6, &["lbl"]);
    cap_label.append(&w::label("CAPTURE", &[]));
    cap_label.append(&w::kbd("c"));
    cap.append(&cap_label);
    let cap_entry = gtk::Entry::builder().placeholder_text(crate::capture::PLACEHOLDER).hexpand(true).max_length(2000).build();
    cap_entry.update_property(&[gtk::accessible::Property::Label("Capture a thought")]);
    cap_entry.set_valign(gtk::Align::Center);
    cap.append(&cap_entry);
    let cap_to = gtk::DropDown::from_strings(&["Unsorted"]);
    cap_to.add_css_class("mini");
    cap_to.set_valign(gtk::Align::Center);
    cap_to.set_tooltip_text(Some("Keep it in project"));
    cap_to.update_property(&[gtk::accessible::Property::Label("Project for the thought")]);
    cap.append(&cap_to);
    let kept = w::label("", &["kept"]);
    kept.set_size_request(44, -1);
    cap.append(&kept);
    main.append(&cap);

    // The side panel sits beside the table and pushes it, so the selected row stays readable; when
    // that would leave the table too narrow, it slides over the list instead (layout.rs decides).
    let drawer = gtk::Box::builder().orientation(gtk::Orientation::Vertical).accessible_role(gtk::AccessibleRole::Region).build();
    drawer.add_css_class("drawer");
    drawer.update_property(&[gtk::accessible::Property::Label("Details")]);
    drawer.set_hexpand(false);
    drawer.set_visible(false);
    // The wrapper hides with the panel, so a closed panel takes no room; it slides in and out.
    let drawer_rev = crate::motion::revealer(gtk::RevealerTransitionType::SlideLeft, &drawer, false);
    drawer_rev.set_hexpand(false);
    drawer.bind_property("visible", &drawer_rev, "visible").sync_create().build();
    let body = crate::layout::Shell::new(&rail, &peek_rail, &main, &drawer_rev);
    body.set_vexpand(true);
    body.set_hexpand(true);

    let status = w::hbox(16, &["statusbar"]);

    let column = w::vbox(0, &[]);
    column.append(&handle);
    // The body's minimum width is that of its narrowest layout, whatever is open, so opening the
    // panel never changes the window's minimum size (Slipstream, like any tiling compositor,
    // scales down a window whose minimum outgrows its tile).
    column.append(&body);
    column.append(&status);

    // Overlays: side panel, switcher, dialogs, toast.
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&column));

    let (switcher, sw_entry, sw_list, sw_inside, sw_scroll) = switcher::widgets();
    overlay.add_overlay(&switcher);
    let modal = w::vbox(0, &["scrim"]);
    modal.set_visible(false);
    overlay.add_overlay(&modal);
    let toast = w::label("", &["toast"]);
    toast.set_halign(gtk::Align::Center);
    toast.set_valign(gtk::Align::End);
    toast.set_margin_bottom(92);
    // A long message wraps, centred, rather than running wider than the window.
    toast.set_wrap(true);
    toast.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    toast.set_max_width_chars(72);
    toast.set_justify(gtk::Justification::Center);
    toast.set_margin_start(16);
    toast.set_margin_end(16);
    toast.set_visible(false);
    let toast_hover = gtk::EventControllerMotion::new();
    toast.add_controller(toast_hover.clone());
    overlay.add_overlay(&toast);
    window.set_child(Some(&overlay));

    let ui: Ui = Rc::new(App {
        src,
        demo,
        window: window.clone(),
        w: Widgets {
            column: column.clone(),
            body: body.clone(),
            rail: rail.clone(),
            menu_btn: menu_btn.clone(),
            cmd_full: cmd_full.clone(),
            cmd_icon: cmd_icon.clone(),
            counts,
            tabs,
            badge: badge.clone(),
            indexing,
            clock,
            nav,
            plist,
            peek_rail,
            peek_nav,
            peek_plist,
            banner,
            chips,
            stack,
            views,
            history_entry: history_entry.clone(),
            cap_entry: cap_entry.clone(),
            cap_to: cap_to.clone(),
            kept,
            status,
            drawer,
            drawer_rev,
            switcher,
            sw_entry,
            sw_list,
            sw_inside,
            sw_scroll,
            modal,
            toast,
            toast_hover,
        },
        st: RefCell::new(AppState::default()),
        scope: scope::Model::new(scope::Look::from_settings()),
        readouts: RefCell::new(Vec::new()),
        tick: RefCell::new(None),
        live: RefCell::new(HashMap::new()),
        peek: RefCell::new(peek::Peek::default()),
    });
    {
        let ui2 = ui.clone();
        ui.w.toast_hover.connect_leave(move |_| {
            let ticket = ui2.st.borrow().toast_gen;
            hide_toast_after(&ui2, ticket, 1500);
        });
    }
    if !crate::motion::on() {
        ui.scope.borrow_mut().look.reduced_motion = true;
    }
    {
        // The window's width crossed a breakpoint: draw the rail, tables and bars in their forms for it.
        let ui2 = ui.clone();
        ui.w.body.connect_changed(move |lay| relayout(&ui2, lay));
    }
    {
        let detailed = prefs::detailed();
        let mut st = ui.st.borrow_mut();
        st.detailed = detailed;
        st.full_readouts = detailed;
    }
    let mut core = prefs::get().core();
    // A screenshot never sends a desktop notification, and asks the remotes only for the
    // behind-the-remote shots, which show what they said.
    if opts.screenshot.is_some() {
        core.notify_behind = false;
        core.quiet = true;
        if !opts.show.as_deref().is_some_and(|s| s.starts_with("behind")) {
            core.remote_every = None;
        }
    }
    ui.src.set_options(&core);
    {
        // "Follow the system": the desktop went light or dark.
        let ui = ui.clone();
        theme::watch_system(move || {
            let p = prefs::get();
            if p.theme == "system" && theme::apply(&p.theme, &p.accent) {
                redraw_all(&ui);
            }
        });
    }
    let start = opts.view.clone().or_else(|| Some(prefs::get().start_view).filter(|v| View::from_id(v).is_some_and(|v| v != View::Settings)));
    if let Some(v) = start.as_deref().and_then(View::from_id) {
        ui.st.borrow_mut().view = v;
        ui.w.stack.set_visible_child_name(v.id());
        show_capture(&ui, v);
    }

    // Signals.
    {
        let ui = ui.clone();
        cmd.connect_clicked(move |_| switcher::open(&ui));
    }
    {
        let ui = ui.clone();
        cmd_icon.connect_clicked(move |_| switcher::open(&ui));
    }
    {
        let ui = ui.clone();
        menu_btn.connect_clicked(move |_| set_rail_open(&ui, !ui.w.body.rail_open()));
    }
    {
        // The drawer opened or closed: the keyboard goes into it, and back to the list after.
        let ui2 = ui.clone();
        ui.w.body.connect_rail(move |open| {
            ui2.w.rail.set_css_classes(&if open { vec!["rail", "full", "drawer-rail"] } else { vec!["rail", if ui2.st.borrow().lay.slim() { "slim" } else { "full" }] });
            if open {
                let ui3 = ui2.clone();
                glib::idle_add_local_once(move || {
                    let on = first_on(&ui3.w.nav).or_else(|| ui3.w.nav.first_child());
                    if let Some(b) = on {
                        b.grab_focus();
                    }
                });
            } else if gtk::prelude::GtkWindowExt::focus(&ui2.window).is_none_or(|f| f.is_ancestor(&ui2.w.rail)) {
                focus_list(&ui2);
            }
        });
    }
    {
        // The slim rail's full form went back with the keyboard in it (an item picked): the
        // keyboard goes back to the list, as from the drawer.
        let ui2 = ui.clone();
        ui.w.body.connect_peek(move |open| {
            if !open && gtk::prelude::GtkWindowExt::focus(&ui2.window).is_some_and(|f| f.is_ancestor(&ui2.w.peek_rail)) {
                focus_list(&ui2);
            }
        });
        // The keyboard went into it or left it. Checked once things settle, so a rail redrawn
        // under the keyboard (its counts changed) doesn't close it.
        let ui2 = ui.clone();
        let queued = Rc::new(std::cell::Cell::new(false));
        ui.window.connect_focus_widget_notify(move |_| {
            if queued.replace(true) {
                return;
            }
            let ui3 = ui2.clone();
            let queued = queued.clone();
            glib::idle_add_local_once(move || {
                queued.set(false);
                let inside = gtk::prelude::GtkWindowExt::focus(&ui3.window).is_some_and(|f| f.is_ancestor(&ui3.w.peek_rail));
                ui3.w.body.slide(|s| s.focus(inside));
            });
        });
    }
    // A click on blank space in the view (not a row, not a control) puts the side panel away, as
    // Esc does. It only watches: the click goes on to whatever is there.
    {
        let ui2 = ui.clone();
        let click = gtk::GestureClick::new();
        click.set_propagation_phase(gtk::PropagationPhase::Capture);
        click.connect_released(move |g, n, x, y| {
            if n != 1 || ui2.st.borrow().drawer.is_none() || ui2.st.borrow().modal_open {
                return;
            }
            let Some(area) = g.widget() else { return };
            let picked = area.pick(x, y, gtk::PickFlags::DEFAULT);
            let mut chain = Vec::new();
            let mut c = picked;
            while let Some(w) = c {
                if w == area {
                    break;
                }
                chain.push(hit_of(&w, &ui2.w.drawer));
                c = w.parent();
            }
            if blank_click(&chain) {
                panel::close(&ui2);
            }
        });
        ui.w.stack.add_controller(click);
    }
    for v in View::ALL {
        let ui2 = ui.clone();
        ui.w.views[&v].list.connect_row_activated(move |_, row| {
            let key = row.widget_name().to_string();
            // A subagent's or the fold's row opens on a single click of its own.
            if v == View::Agents && !matches!(agents::row_key(&key), agents::RowKey::Session(_) | agents::RowKey::Other) {
                return;
            }
            ui2.st.borrow_mut().sel = Some(key);
            open_sel(&ui2);
        });
        let ui2 = ui.clone();
        let glide = ui.w.views[&v].glide.clone();
        ui.w.views[&v].list.connect_row_selected(move |_, row| {
            glide_to(&glide, row);
            if let Some(row) = row {
                let key = row.widget_name().to_string();
                if !key.is_empty()
                    && let Ok(mut st) = ui2.st.try_borrow_mut()
                {
                    st.sel = Some(key);
                }
            }
        });
    }
    {
        let ui = ui.clone();
        cap_entry.connect_activate(move |_| capture(&ui));
    }
    {
        let ui = ui.clone();
        cap_to.connect_selected_notify(move |_| {
            if let Ok(mut st) = ui.st.try_borrow_mut() {
                st.cap_manual = true;
            }
        });
    }
    {
        let ui = ui.clone();
        history_entry.connect_changed(move |e| views::search_changed(&ui, e.text().to_string()));
    }
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let ui = ui.clone();
        keys.connect_key_pressed(move |_, key, _, mods| on_key(&ui, key, mods));
    }
    {
        let ui = ui.clone();
        keys.connect_key_released(move |_, key, _, _| peek::on_release(&ui, key));
    }
    window.add_controller(keys);
    {
        // A peek lasts only while Wake has the keyboard.
        let ui = ui.clone();
        window.connect_is_active_notify(move |win| {
            if !win.is_active() {
                peek::focus_lost(&ui);
            }
        });
    }
    peek::learn_kind(&ui);
    crate::questions::build(&ui);
    crate::behind::build(&ui);
    crate::capture::build(&ui);
    crate::keynav::build(&ui);

    render_all(&ui);
    refresh(&ui);
    refresh_procs(&ui);
    refresh_ideas(&ui);
    {
        let ui = ui.clone();
        glib::timeout_add_local(POLL, move || {
            poll(&ui);
            glib::ControlFlow::Continue
        });
    }
    {
        let ui = ui.clone();
        glib::timeout_add_local(PROC_POLL, move || {
            if ui.st.borrow().view == View::Agents && awake(&ui) {
                refresh_procs(&ui);
            }
            glib::ControlFlow::Continue
        });
    }
    {
        let ui = ui.clone();
        glib::timeout_add_local(Duration::from_secs(15), move || {
            ui.w.clock.set_text(&crate::fmt::clock());
            glib::ControlFlow::Continue
        });
    }
    {
        // Back from minimised: catch up at once rather than at the next tick.
        let ui = ui.clone();
        window.connect_suspended_notify(move |win| {
            if !win.is_suspended() {
                poll(&ui);
            }
        });
    }
    {
        // `wake find` from a desktop shortcut lands here when Wake is already open.
        let find = gio::SimpleAction::new("find", None);
        let ui = ui.clone();
        find.connect_activate(move |_, _| {
            ui.window.present();
            switcher::open(&ui);
        });
        app.add_action(&find);
    }
    window.present();
    let first = ui.st.borrow().view;
    match first {
        View::Agents => agents::shown(&ui),
        View::Questions => crate::questions::shown(&ui),
        _ => {}
    }
    // Which AIs Questions can use (and so what it says it is asking).
    setview::shown(&ui);
    if let Some(n) = prefs::note() {
        self::toast(&ui, &n, true);
    }
    if let Some(path) = opts.screenshot.clone() {
        // A fixed size so every screenshot is the same window (Broadway sizes windows to content).
        // Content that needs more than this still widens it: `--show clips` reports that.
        let width = shot_width.unwrap_or(1480);
        ui.window.set_size_request(width, 900);
        // Draw focus as it looks after keyboard use, which Broadway never sends.
        if std::env::var_os("WAKE_SHOT_FOCUS").is_some() {
            ui.window.set_focus_visible(true);
        }
        screenshot(&ui, path, opts.show.clone());
    } else if let Some(show) = opts.show.clone() {
        let ui = ui.clone();
        glib::timeout_add_local_once(Duration::from_millis(600), move || apply_show(&ui, &show));
    }
}

/// Opens what `--show` asks for, once the data is in.
fn apply_show(ui: &Ui, show: &str) {
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
            ui.st.borrow_mut().all_prompts = true;
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
                        let mut c = wd.first_child();
                        while let Some(x) = c {
                            walk(&x, depth + 1);
                            c = x.next_sibling();
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
        "hold" => peek::hook_hold(ui, arg),
        "look" => {
            if let Some(pid) = arg.and_then(|a| a.parse().ok()) {
                peek::hook_look_at(ui, pid);
            }
        }
        "questions" => crate::questions::show(ui, arg),
        "behind" | "behind-pull" | "behind-wait" => crate::behind::show(ui, what, arg),
        "disk" | "disk-machine" | "disk-clear" | "disk-remove" | "set-project" => crate::disk::show(ui, what, arg), // disk.rs hook
        "readouts" => agents::toggle_readouts(ui),
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
        // As if Konsole had not answered for a while: "tab unknown" and the status bar's warning.
        "tabs-unknown" => {
            let since = ui.st.borrow().unsure_since;
            if since.is_some() {
                ui.st.borrow_mut().unsure_since = Some(glib::monotonic_time() - (GIVE_UP_SECS + 1) * 1_000_000);
            }
            render_all(ui);
        }
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
                    for k in ["subs", "asked", "recap", "files", "details", "sessions", "ideas", "git", "pdetails", "colour"] {
                        if open.split(',').any(|x| x == k) {
                            ui.st.borrow_mut().panel_open.insert(k);
                        }
                    }
                    panel::render(ui);
                }
            }
        }
        "project" => {
            let id = arg.or_else(|| ui.st.borrow().ov.as_ref().and_then(|ov| ov.projects.first().map(|p| p.id.clone())));
            if let Some(id) = id {
                panel::open_project(ui, &id, true);
                if let Ok(open) = std::env::var("WAKE_PANEL_OPEN") {
                    for k in ["sessions", "ideas", "git", "pdetails", "colour"] {
                        if open.split(',').any(|x| x == k) {
                            ui.st.borrow_mut().panel_open.insert(k);
                        }
                    }
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
                Some("questions") => find_dropdown(ui.w.views[&View::Questions].root.upcast_ref()),
                Some("settings-now") => find_dropdown(ui.w.views[&View::Settings].root.upcast_ref()),
                _ => Some(ui.w.cap_to.clone().upcast()),
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

fn find_dropdown(w: &gtk::Widget) -> Option<gtk::Widget> {
    if w.is::<gtk::DropDown>() {
        return Some(w.clone());
    }
    let mut c = w.first_child();
    while let Some(x) = c {
        if let Some(f) = find_dropdown(&x) {
            return Some(f);
        }
        c = x.next_sibling();
    }
    None
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

fn screenshot(ui: &Ui, path: PathBuf, show: Option<String>) {
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
        if tries < 6 || (long && tries < 24) || (slow && tries < 45) || (keyed && tries < 10) {
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
        let found = if name.ends_with('*') { find_prefixed(root, name.trim_end_matches('*')) } else { panel::find_named(root, name) };
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
            if let Some(id) = ui.st.borrow().ov.as_ref().and_then(|ov| ov.projects.first().map(|p| p.id.clone())) {
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

fn find_prefixed(w: &gtk::Widget, prefix: &str) -> Option<gtk::Widget> {
    if w.widget_name().starts_with(prefix) && w.is::<gtk::Button>() && w.is_mapped() {
        return Some(w.clone());
    }
    let mut c = w.first_child();
    while let Some(x) = c {
        if let Some(f) = find_prefixed(&x, prefix) {
            return Some(f);
        }
        c = x.next_sibling();
    }
    None
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

// ---------- refresh ----------

/// Shown and not minimised: polling while nobody can see the window only costs memory and CPU.
fn awake(ui: &Ui) -> bool {
    ui.window.is_visible() && !ui.window.is_suspended()
}

fn poll(ui: &Ui) {
    if !awake(ui) {
        return;
    }
    refresh(ui);
    refresh_procs(ui);
    let (view, drawer, sw_open, editing) = {
        let st = ui.st.borrow();
        (st.view, st.drawer.clone(), st.sw.open, st.edit_next)
    };
    if view == View::Ideas {
        refresh_ideas(ui);
    }
    match drawer {
        Some(Drawer::Session(id)) => panel::load_session(ui, &id, true),
        Some(Drawer::Project(id)) if !editing => panel::load_project(ui, &id),
        _ => {}
    }
    if sw_open {
        switcher::render(ui);
    }
}

pub fn refresh(ui: &Ui) {
    {
        let mut st = ui.st.borrow_mut();
        if st.busy_ov {
            return;
        }
        st.busy_ov = true;
    }
    bg(
        ui,
        |src| src.overview(),
        |ui, ov| {
            {
                let mut st = ui.st.borrow_mut();
                if ov.locations_unsure.is_none() {
                    st.known_loc = ov.sessions.iter().filter_map(|s| s.location.as_ref().map(|l| (s.id.clone(), tab_label(l)))).collect();
                    st.unsure_since = None;
                } else if st.unsure_since.is_none() {
                    st.unsure_since = Some(glib::monotonic_time());
                }
                st.busy_ov = false;
                st.ov = Some(ov);
                st.ov_error = None;
                st.last_ok = Some(glib::monotonic_time());
            }
            render_all(ui);
            panel::tick_procs(ui);
        },
    );
}

pub fn refresh_procs(ui: &Ui) {
    {
        let mut st = ui.st.borrow_mut();
        if st.busy_procs {
            return;
        }
        st.busy_procs = true;
    }
    bg(
        ui,
        |src| (src.processes(), mem_available()),
        |ui, (procs, mem)| {
            {
                let mut st = ui.st.borrow_mut();
                st.busy_procs = false;
                st.procs = Some(procs);
                st.mem_available = mem;
            }
            views::render_top(ui);
            views::render_rail(ui);
            panel::tick_procs(ui);
            // Monitor's working rows draw each agent's wave once its note is known.
            if ui.st.borrow().view == View::Monitor {
                views::render_view(ui);
            }
            if ui.st.borrow().view == View::Agents {
                views::render_chips(ui);
                views::render_status(ui);
                agents::render(ui);
                agents::refresh_open(ui);
            }
        },
    );
}

pub fn refresh_ideas(ui: &Ui) {
    bg(
        ui,
        |src| src.ideas(),
        |ui, ideas| {
            ui.st.borrow_mut().ideas = Some(ideas);
            if ui.st.borrow().view == View::Ideas {
                views::render_chips(ui);
                views::render_view(ui);
            }
        },
    );
}

/// MemAvailable from /proc/meminfo, in bytes.
fn mem_available() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = text.lines().find(|l| l.starts_with("MemAvailable:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

pub fn render_all(ui: &Ui) {
    if ui.st.borrow().ov.is_none() {
        views::render_loading(ui);
        return;
    }
    views::render_top(ui);
    views::render_rail(ui);
    views::render_chips(ui);
    views::render_view(ui);
    views::render_status(ui);
    crate::behind::render(ui);
    capture_default(ui);
    if ui.st.borrow().drawer.is_some() && !typing_in(ui, &ui.w.drawer) {
        panel::render(ui);
    }
    tick_live(ui);
}

/// Whether the tables show their compact columns: the list is narrow, or the details share the
/// row with it.
pub fn compact(st: &AppState) -> bool {
    st.lay.narrow || (st.drawer.is_some() && !st.lay.push)
}

/// Whether rows are two-line cells rather than columns: the list is narrow, alone or beside the
/// details (columns cut to a few letters say less than two lines).
pub fn cards(st: &AppState) -> bool {
    st.lay.narrow || (st.drawer.is_some() && !st.lay.push && st.lay.narrow_beside)
}

/// Opens or closes the rail's drawer (a narrow window); does nothing while the rail is shown.
/// Closing also puts the slim rail's slid-out form back.
pub fn set_rail_open(ui: &Ui, open: bool) {
    let hidden = ui.st.borrow().lay.rail == crate::layout::Rail::Hidden;
    ui.w.body.set_rail_open(open && hidden);
    if !open {
        ui.w.body.dismiss_peek();
    }
}

/// The button in `b` marked "on" (the current view in the rail).
fn first_on(b: &gtk::Box) -> Option<gtk::Widget> {
    let mut c = b.first_child();
    while let Some(x) = c {
        if x.has_css_class("on") {
            return Some(x);
        }
        c = x.next_sibling();
    }
    None
}

/// The window's width crossed a breakpoint (layout.rs): the rail goes slim or full, and the
/// tables, top bar and status bar redraw in their forms for the new width.
fn relayout(ui: &Ui, lay: crate::layout::Lay) {
    use crate::layout::Rail;
    let old = {
        let mut st = ui.st.borrow_mut();
        let old = st.lay;
        if old == lay && ui.w.rail.has_css_class(if lay.slim() { "slim" } else { "full" }) {
            return;
        }
        st.lay = lay;
        st.rail_drawn.clear();
        st.top_drawn.clear();
        st.chips_drawn.clear();
        st.panel_drawn = None;
        old
    };
    if lay.rail != Rail::Hidden {
        ui.w.body.set_rail_open(false);
    }
    if lay.rail != Rail::Slim {
        ui.w.body.end_peek();
    }
    // The rail: names, or initials and dots; as a drawer it has its names.
    let slim = lay.slim();
    ui.w.rail.set_css_classes(&["rail", if slim { "slim" } else { "full" }]);
    ui.w.rail.set_size_request(if slim { crate::layout::SLIM_W } else { crate::layout::RAIL_W }, -1);
    // The VIEWS and PROJECTS headings make way for a line between the two.
    let mut c = ui.w.rail.first_child();
    while let Some(x) = c {
        if x.has_css_class("grp") {
            x.set_visible(!slim);
        }
        c = x.next_sibling();
    }
    // The top bar like a phone's: ☰ for the rail, ⌕ for the search box.
    let tiny = lay.rail == Rail::Hidden;
    ui.w.menu_btn.set_visible(tiny);
    ui.w.tabs.set_visible(!tiny);
    // Short of the full width, the search box is its ⌕ button, so the tabs and tools keep room.
    let short_top = lay.rail != Rail::Full;
    ui.w.cmd_full.set_visible(!short_top);
    ui.w.cmd_icon.set_visible(short_top);
    // The clock and the demo badge give way before the tools do (the status bar says "demo data").
    ui.w.clock.set_visible(!short_top);
    ui.w.badge.set_visible(ui.demo && !short_top);
    // The capture bar keeps its entry and project; its label makes way.
    if let Some(cap) = ui.w.cap_entry.parent() {
        let mut c = cap.first_child();
        while let Some(x) = c {
            if x.has_css_class("lbl") {
                x.set_visible(!lay.narrow);
            }
            c = x.next_sibling();
        }
    }
    crate::questions::relayout(lay.narrow);
    // Pushed details that are open when the window narrows: the keyboard follows them.
    let pushed_now = lay.push && !old.push && ui.st.borrow().drawer.is_some();
    if ui.st.borrow().ov.is_none() {
        return;
    }
    views::render_rail(ui);
    views::render_top(ui);
    views::render_chips(ui);
    views::render_view(ui);
    views::render_status(ui);
    if ui.st.borrow().drawer.is_some() {
        panel::render(ui);
        if pushed_now && gtk::prelude::GtkWindowExt::focus(&ui.window).is_none_or(|f| !f.is_ancestor(&ui.w.drawer)) {
            panel::focus_primary(ui);
        }
    }
}

/// Draws everything again from scratch (the theme changed: colours in text are baked in).
pub fn redraw_all(ui: &Ui) {
    {
        let mut st = ui.st.borrow_mut();
        st.drawn.clear();
        st.panel_drawn = None;
        st.top_drawn.clear();
        st.rail_drawn.clear();
        st.chips_drawn.clear();
    }
    render_all(ui);
    crate::questions::redraw(ui);
    for r in ui.readouts.borrow().iter() {
        r.queue_draw();
    }
}

/// The side panel's sections, all of them (Detailed opens them all).
pub const PANEL_FOLDS: [&str; 10] = ["subs", "asked", "recap", "files", "details", "sessions", "ideas", "git", "pdetails", "colour"];

/// Detailed or reduced: panel sections open or folded, full readouts or small waves. `say` tells
/// the person (for `z`, which only lasts until Wake closes).
pub fn set_detailed(ui: &Ui, on: bool, say: bool) {
    {
        let mut st = ui.st.borrow_mut();
        st.detailed = on;
        st.full_readouts = on;
        st.panel_open = if on { PANEL_FOLDS.into_iter().collect() } else { HashSet::new() };
        st.drawn.remove(&View::Agents);
        st.panel_drawn = None;
        st.chips_drawn.clear();
    }
    render_all(ui);
    if ui.st.borrow().view == View::Agents {
        agents::render(ui);
    }
    if say {
        toast(ui, if on { "More detail for now: panel sections open, full readouts. z again for less." } else { "Less detail for now: panel sections folded, small waves. z again for more." }, false);
    }
}

/// Wake's window buttons follow this desktop's: under Slipstream, close only.
pub fn slipstream_desktop(xdg_current_desktop: &str) -> bool {
    xdg_current_desktop.split(':').any(|d| d.eq_ignore_ascii_case("slipstream"))
}

/// Updates the current view's in-place labels (ages, memory, CPU) from the latest data.
pub fn tick_live(ui: &Ui) {
    let (Ok(st), Ok(live)) = (ui.st.try_borrow(), ui.live.try_borrow()) else { return };
    if let Some(list) = live.get(&st.view) {
        for l in list {
            l.apply(&st);
        }
    }
}

/// True while the keyboard is in a text field inside `area`: a refresh must not redraw it then.
pub fn typing_in(ui: &Ui, area: &impl IsA<gtk::Widget>) -> bool {
    gtk::prelude::GtkWindowExt::focus(&ui.window).is_some_and(|f| f.is_ancestor(area) && (f.is::<gtk::Text>() || f.is::<gtk::Entry>() || f.is::<gtk::TextView>()))
}

// ---------- navigation ----------

pub fn set_view(ui: &Ui, v: View) {
    // Picked from the rail's drawer: the drawer goes, the view shows.
    set_rail_open(ui, false);
    let was = ui.st.borrow().view;
    if was == v {
        if v == View::History {
            ui.w.history_entry.grab_focus();
        }
        return;
    }
    {
        let mut st = ui.st.borrow_mut();
        st.view = v;
        st.sel = None;
        st.show_brief = false;
        st.adding = false;
        st.drawn.remove(&v);
    }
    // The waves only move on the view that shows them.
    if matches!(was, View::Agents | View::Monitor) {
        agents::hidden(ui);
    }
    ui.w.stack.set_visible_child_name(v.id());
    show_capture(ui, v);
    render_all(ui);
    match v {
        View::Ideas => refresh_ideas(ui),
        View::Agents => {
            agents::shown(ui);
            refresh_procs(ui);
        }
        View::History => {
            ui.w.history_entry.grab_focus();
        }
        View::Questions => crate::questions::shown(ui),
        View::Settings => setview::shown(ui),
        _ => {}
    }
    let list = &ui.w.views[&v].list;
    list.unselect_all();
    ui.w.views[&v].scroll.vadjustment().set_value(0.0);
}

/// Keys of the selectable rows in the current view, in order.
fn rows(ui: &Ui) -> Vec<gtk::ListBoxRow> {
    let v = ui.st.borrow().view;
    let list = &ui.w.views[&v].list;
    let mut out = Vec::new();
    let mut child = list.first_child();
    while let Some(c) = child {
        if let Ok(row) = c.clone().downcast::<gtk::ListBoxRow>()
            && row.is_selectable()
            && row.is_visible()
        {
            out.push(row);
        }
        child = c.next_sibling();
    }
    out
}

/// A view drawn as a grid of cards (Projects): the grid, in the list's one row.
pub fn grid_of(ui: &Ui, view: View) -> Option<gtk::FlowBox> {
    let mut c = ui.w.views[&view].list.first_child();
    while let Some(x) = c {
        if x.has_css_class("gridrow") {
            return x.downcast::<gtk::ListBoxRow>().ok()?.child()?.downcast::<gtk::FlowBox>().ok();
        }
        c = x.next_sibling();
    }
    None
}

fn grid_cards(g: &gtk::FlowBox) -> Vec<gtk::FlowBoxChild> {
    (0..).map_while(|i| g.child_at_index(i)).collect()
}

/// j/k and the arrows in a grid: the next or previous card, in reading order.
fn grid_step(ui: &Ui, g: &gtk::FlowBox, d: i32) {
    let cards = grid_cards(g);
    if cards.is_empty() {
        return;
    }
    let sel = ui.st.borrow().sel.clone();
    let i = match cards.iter().position(|c| Some(c.widget_name().as_str()) == sel.as_deref()) {
        None if d > 0 => 0,
        None => cards.len() - 1,
        Some(i) => (i as i32 + d).clamp(0, cards.len() as i32 - 1) as usize,
    };
    let key = cards[i].widget_name().to_string();
    ui.st.borrow_mut().sel = Some(key.clone());
    g.select_child(&cards[i]);
    cards[i].grab_focus();
    let open = matches!(ui.st.borrow().drawer, Some(Drawer::Project(_)));
    if open {
        panel::open_project(ui, &key, false);
    }
}

pub fn move_sel(ui: &Ui, d: i32) {
    if ui.st.borrow().view == View::Settings {
        setview::keyboard(ui, true);
    }
    let view = ui.st.borrow().view;
    if let Some(g) = grid_of(ui, view) {
        grid_step(ui, &g, d);
        return;
    }
    let rows = rows(ui);
    if rows.is_empty() {
        return;
    }
    let sel = ui.st.borrow().sel.clone();
    let i = rows.iter().position(|r| Some(r.widget_name().as_str()) == sel.as_deref());
    let i = match i {
        None if d > 0 => 0,
        None => rows.len() - 1,
        Some(i) => (i as i32 + d).clamp(0, rows.len() as i32 - 1) as usize,
    };
    select_row(ui, &rows[i]);
    // An open side panel follows the selection.
    let key = rows[i].widget_name().to_string();
    let drawer = ui.st.borrow().drawer.clone();
    let view = ui.st.borrow().view;
    match drawer {
        Some(Drawer::Session(_)) if matches!(view, View::Monitor | View::History) => panel::open_session(ui, &key, false),
        Some(Drawer::Project(_)) if view == View::Projects => panel::open_project(ui, &key, false),
        _ => {}
    }
}

pub fn select_row(ui: &Ui, row: &gtk::ListBoxRow) {
    let v = ui.st.borrow().view;
    ui.st.borrow_mut().sel = Some(row.widget_name().to_string());
    ui.w.views[&v].list.select_row(Some(row));
    row.grab_focus();
}

/// After a redraw: select (and, if the list had focus, focus) the row that was selected.
pub fn restore_sel(ui: &Ui, view: View, had_focus: bool) {
    let sel = ui.st.borrow().sel.clone();
    let list = &ui.w.views[&view].list;
    let Some(sel) = sel else { return };
    if let Some(g) = grid_of(ui, view) {
        if let Some(c) = grid_cards(&g).into_iter().find(|c| c.widget_name() == sel) {
            g.select_child(&c);
            if had_focus {
                c.grab_focus();
            }
        }
        return;
    }
    let mut child = list.first_child();
    while let Some(c) = child {
        if let Ok(row) = c.clone().downcast::<gtk::ListBoxRow>()
            && row.widget_name() == sel
        {
            list.select_row(Some(&row));
            if had_focus {
                row.grab_focus();
                // Focusing scrolls the row into view; the reader may have scrolled elsewhere.
                let target = ui.st.borrow().kept_scroll.get(&view).copied().unwrap_or(0.0);
                crate::w::keep_scroll(&ui.w.views[&view].scroll.vadjustment(), target);
            }
            return;
        }
        child = c.next_sibling();
    }
}

pub fn open_sel(ui: &Ui) {
    let (view, sel) = {
        let st = ui.st.borrow();
        (st.view, st.sel.clone())
    };
    let Some(sel) = sel else {
        move_sel(ui, 1);
        return;
    };
    match view {
        View::Monitor | View::History => panel::open_session(ui, &sel, true),
        View::Projects => panel::open_project(ui, &sel, true),
        // Enter on an idea is "sort it": the keyboard goes to its project list.
        View::Ideas => views::focus_idea_project(ui, &sel),
        View::Questions => crate::questions::focus_input(ui),
        View::Settings => setview::activate(ui),
        // A session's row opens its details; a subagent's its steps, the fold its finished list.
        View::Agents => {
            agents::key(ui, agents::TreeKey::Enter);
        }
    }
}

pub fn find_session(ui: &Ui, id: &str) -> Option<SessionSummary> {
    let st = ui.st.borrow();
    st.ov.as_ref().and_then(|ov| ov.sessions.iter().find(|s| s.id == id).cloned()).or_else(|| st.sessions.get(id).map(|d| d.summary.clone()))
}

pub fn find_project(ui: &Ui, id: &str) -> Option<ProjectSummary> {
    ui.st.borrow().ov.as_ref().and_then(|ov| ov.projects.iter().find(|p| p.id == id).cloned())
}

pub fn can_reopen(s: &SessionSummary) -> bool {
    s.location.is_none() && s.resume_command.is_some() && matches!(s.state, State::Ended | State::Unknown)
}

pub fn is_live(s: State) -> bool {
    matches!(s, State::Working | State::Waiting | State::Idle | State::Unknown)
}

/// A tab as a person names it; core's label is unique across every Konsole on the desktop.
pub fn tab_label(l: &Location) -> String {
    l.label.clone().unwrap_or_else(|| format!("Konsole {} · tab {}", l.window, l.tab_index + 1))
}

/// How long Wake waits for Konsole to answer before it treats the tabs as unknown.
const GIVE_UP_SECS: i64 = 15;

/// Why Wake can't be sure where live sessions are, while it is so (Konsole hasn't answered).
pub fn unsure_why(ui: &Ui) -> Option<String> {
    ui.st.borrow().ov.as_ref().and_then(|o| o.locations_unsure.clone())
}

/// True while Wake is still finding where live sessions are: not yet `tabs_unknown`.
pub fn unsure(ui: &Ui) -> bool {
    unsure_why(ui).is_some() && tabs_unknown(ui).is_none()
}

/// Why Wake can't see Konsole's tabs, once it has failed to for `GIVE_UP_SECS`. Then a session
/// shows "tab unknown" and Go to attaches or resumes, as it does with no Konsole at all.
pub fn tabs_unknown(ui: &Ui) -> Option<String> {
    let since = ui.st.borrow().unsure_since?;
    if (glib::monotonic_time() - since) / 1_000_000 < GIVE_UP_SECS {
        return None;
    }
    unsure_why(ui)
}

/// Where a session can be seen, as the web's `locOf`: text, class, can go, tooltip. While Wake is
/// `unsure` where sessions are, a live one without a tab says "checking" rather than a confident
/// (and maybe wrong) "background", and can't be gone to: that might attach a second time.
pub fn loc_of(s: &SessionSummary, unsure: bool) -> (String, &'static str, bool, String) {
    if let Some(l) = &s.location {
        return (tab_label(l), "", true, format!("Go to tab \"{}\"", l.tab_title));
    }
    if unsure && is_live(s.state) && s.state != State::Unknown {
        return ("checking…".into(), "none", false, "Wake is still finding which Konsole tab this is in.".into());
    }
    if can_reopen(s) {
        return ("not open".into(), "re", true, "Resume it in a new Konsole window".into());
    }
    if s.deleted_by_claude {
        return ("kept by Wake".into(), "none", false, "Claude deleted its copy; it cannot be reopened".into());
    }
    if is_live(s.state) {
        return ("background".into(), "re", true, "Runs in the background, not in a tab. Go to opens a Konsole tab with claude attach typed, waiting for your Enter".into());
    }
    ("not open".into(), "none", false, String::new())
}

/// `loc_of`, except that while Wake re-checks Konsole a live session shows where it was last seen
/// for sure (quietly, and it can't be gone to until Wake is sure again) instead of "checking…".
/// Once the tabs are `tabs_unknown`, a live session without a tab says "tab unknown" (it may be
/// in one Wake can't see) and Go to attaches to it.
pub fn loc_for(ui: &Ui, s: &SessionSummary) -> (String, &'static str, bool, String) {
    let unsure = unsure(ui);
    let out = loc_of(s, unsure);
    if let Some(why) = tabs_unknown(ui)
        && out.0 == "background"
    {
        return ("tab unknown".into(), out.1, out.2, format!("{why} Go to opens a Konsole tab with claude attach typed, waiting for your Enter."));
    }
    if unsure
        && s.location.is_none()
        && is_live(s.state)
        && let Some(why) = unsure_why(ui)
    {
        if let Some(seen) = ui.st.borrow().known_loc.get(&s.id) {
            return (seen.clone(), "none", false, format!("Last seen here. {why}"));
        }
        if out.0 == "checking…" {
            return ("…".into(), "none", false, why);
        }
    }
    out
}

/// How long the selection bar takes to travel and settle.
const STRETCH_MS: u32 = 260;

/// The selection bar's edges at raw progress `t` (0 to 1), going from the `from` row's (top,
/// bottom) to the `to` row's. The leading edge sets off at once and arrives early; the trailing
/// edge waits a beat and catches up, so the bar stretches as it travels and draws itself back in
/// as it lands. Both ease out, and both end exactly on the target.
pub fn stretch(from: (f64, f64), to: (f64, f64), t: f64) -> (f64, f64) {
    let ease = |x: f64| 1.0 - (1.0 - x.clamp(0.0, 1.0)).powi(3);
    let lead = ease(t / 0.62);
    let trail = ease((t - 0.22) / 0.78);
    let down = to.0 >= from.0;
    let (kt, kb) = if down { (trail, lead) } else { (lead, trail) };
    let top = from.0 + (to.0 - from.0) * kt;
    let bottom = from.1 + (to.1 - from.1) * kb;
    (top.min(bottom - 2.0), bottom)
}

/// Moves the selection bar to `row`: it glides from where it was (150–220 ms, easing out); the
/// row's own bar shows again when it gets there. With motion off, or the same row redrawn, it
/// just is there.
pub fn glide_to(g: &Rc<Glide>, row: Option<&gtk::ListBoxRow>) {
    if let Some(s) = g.stop.borrow_mut().take() {
        s.set(true);
    }
    let mut c = g.over.child().and_then(|i| i.first_child()).and_then(|l| l.first_child());
    while let Some(x) = c {
        x.remove_css_class("glide");
        c = x.next_sibling();
    }
    let Some(row) = row.filter(|r| !r.parent().is_some_and(|l| l.has_css_class("setlist"))) else {
        // Settings' cards show the selected row themselves.
        g.bar.set_visible(false);
        g.key.borrow_mut().clear();
        return;
    };
    let key = row.widget_name().to_string();
    let same = *g.key.borrow() == key;
    *g.key.borrow_mut() = key;
    let from = g.at.get();
    let (g2, r2) = (g.clone(), row.clone());
    let Some((fy, fh)) = from.filter(|_| !same && crate::motion::on() && row.is_mapped()) else {
        g.bar.set_visible(false);
        crate::motion::after_layout(row, move || {
            if let Some(b) = r2.compute_bounds(&g2.over) {
                g2.at.set(Some((f64::from(b.y()), f64::from(b.height()))));
            }
        });
        return;
    };
    row.add_css_class("glide");
    g.bar.set_margin_top(fy as i32);
    g.bar.set_size_request(2, fh as i32);
    g.bar.add_css_class("moving");
    g.bar.set_visible(true);
    let stop = crate::motion::animate(row, STRETCH_MS, move |eased| {
        let Some(b) = r2.compute_bounds(&g2.over) else { return };
        let (ty, th) = (f64::from(b.y()), f64::from(b.height()));
        // `animate` hands over eased progress; the two edges want their own timing, so undo it.
        let t = 1.0 - (1.0 - eased).cbrt();
        let (top, bottom) = stretch((fy, fy + fh), (ty, ty + th), t);
        g2.bar.set_margin_top(top.round() as i32);
        g2.bar.set_size_request(2, (bottom - top).round().max(2.0) as i32);
        g2.at.set(Some((top, bottom - top)));
        if eased >= 1.0 {
            g2.bar.remove_css_class("moving");
            g2.bar.set_visible(false);
            r2.remove_css_class("glide");
        }
    });
    *g.stop.borrow_mut() = Some(stop);
}

/// Go to: `Source::go_to`, then say what happened.
pub fn go_to(ui: &Ui, id: &str) {
    let Some(s) = find_session(ui, id) else { return };
    let (_, _, can, _) = loc_of(&s, unsure(ui));
    if !can {
        let msg = if s.deleted_by_claude {
            "Claude deleted this conversation, so it can't be reopened. Wake kept what it did.".to_string()
        } else if let Some(why) = unsure_why(ui).filter(|_| unsure(ui)) {
            format!("{why} Try again in a moment.")
        } else {
            "Wake can't find a terminal tab for this session.".to_string()
        };
        toast(ui, &msg, true);
        return;
    }
    let id = id.to_string();
    let had_tab = s.location.is_some();
    bg(
        ui,
        {
            let id = id.clone();
            move |src| src.go_to(&id, false)
        },
        move |ui, r| {
            toast(ui, if r.message.is_empty() { if r.ok { "Done." } else { "That didn't work." } } else { &r.message }, !r.ok);
            if r.ok && had_tab {
                let mut st = ui.st.borrow_mut();
                if st.cur.as_deref() != Some(&id) {
                    st.back = st.cur.take();
                    st.cur = Some(id);
                }
            }
        },
    );
}

// ---------- capture ----------

/// The capture bar steps aside in Questions, which has its own input (two text bars would stack),
/// and in Settings.
fn show_capture(ui: &Ui, v: View) {
    if let Some(bar) = ui.w.cap_entry.parent() {
        bar.set_visible(!matches!(v, View::Questions | View::Settings));
    }
}

pub fn capture_default(ui: &Ui) {
    let (ids, names, want, manual) = {
        let st = ui.st.borrow();
        let ps: Vec<&ProjectSummary> = st.ov.as_ref().map(|ov| ov.projects.iter().filter(|p| !p.removed).collect()).unwrap_or_default();
        let mut ids = vec![None];
        let mut names = vec!["Unsorted".to_string()];
        for p in &ps {
            ids.push(Some(p.id.clone()));
            names.push(p.name.clone());
        }
        let want = match &st.drawer {
            Some(Drawer::Project(id)) => Some(id.clone()),
            Some(Drawer::Session(id)) => st.ov.as_ref().and_then(|ov| ov.sessions.iter().find(|s| &s.id == id).and_then(|s| s.project.clone())),
            None => st.proj_filter.clone(),
        };
        (ids, names, want, st.cap_manual)
    };
    let changed = ui.st.borrow().cap_projects != ids;
    let current = ui.st.borrow().cap_projects.get(ui.w.cap_to.selected() as usize).cloned().flatten();
    if changed {
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        ui.st.borrow_mut().cap_projects = ids.clone();
        ui.w.cap_to.set_model(Some(&gtk::StringList::new(&refs)));
        let at = ids.iter().position(|i| *i == current).unwrap_or(0);
        ui.w.cap_to.set_selected(at as u32);
        ui.st.borrow_mut().cap_manual = manual;
    }
    if !manual {
        let at = ids.iter().position(|i| *i == want).unwrap_or(0);
        if ui.w.cap_to.selected() != at as u32 {
            ui.w.cap_to.set_selected(at as u32);
            ui.st.borrow_mut().cap_manual = false;
        }
    }
}

fn capture(ui: &Ui) {
    let text = ui.w.cap_entry.text().trim().to_string();
    if text.is_empty() {
        return;
    }
    let project = ui.st.borrow().cap_projects.get(ui.w.cap_to.selected() as usize).cloned().flatten();
    ui.w.cap_entry.set_sensitive(false);
    bg(
        ui,
        move |src| src.add_idea(&text, project.as_deref()),
        move |ui, r| {
            ui.w.cap_entry.set_sensitive(true);
            match r {
                Ok(idea) => {
                    ui.w.cap_entry.set_text("");
                    // The project stays chosen while the keyboard stays here (several thoughts in a row).
                    crate::capture::kept(idea.project.clone());
                    kept(
                        ui,
                        &match &idea.project_name {
                            Some(n) => format!("kept · {n}"),
                            None => "kept".into(),
                        },
                        false,
                    );
                    if let Some(ideas) = ui.st.borrow_mut().ideas.as_mut() {
                        ideas.insert(0, idea);
                    }
                    refresh(ui);
                    if ui.st.borrow().view == View::Ideas {
                        views::render_chips(ui);
                        views::render_view(ui);
                    }
                }
                Err(e) => kept(ui, &e, true),
            }
            ui.w.cap_entry.grab_focus();
            capture_default(ui);
        },
    );
}

fn kept(ui: &Ui, msg: &str, bad: bool) {
    let k = &ui.w.kept;
    k.set_text(msg);
    if bad {
        k.add_css_class("err");
    } else {
        k.remove_css_class("err");
    }
    let ticket = {
        let mut st = ui.st.borrow_mut();
        st.kept_gen += 1;
        st.kept_gen
    };
    let ui = ui.clone();
    glib::timeout_add_local_once(Duration::from_millis(if bad { 5000 } else { 1600 }), move || {
        if ui.st.borrow().kept_gen == ticket {
            ui.w.kept.set_text("");
        }
    });
}

pub fn toast(ui: &Ui, msg: &str, bad: bool) {
    // Errors go in Wake's log too (what was done is logged by the core already).
    if bad {
        let view = ui.st.borrow().view;
        ui.src.note_error(&format!("Shown in {}", view.name()), msg);
    }
    let t = &ui.w.toast;
    t.set_text(msg);
    if bad {
        t.add_css_class("bad");
    } else {
        t.remove_css_class("bad");
    }
    t.set_visible(true);
    crate::motion::pop(t);
    // Screen readers hear it too (not on Broadway, whose missing accessibility backend crashes
    // GTK's announce, as behind.rs found).
    if std::env::var("GDK_BACKEND").map_or(true, |b| b != "broadway") {
        ui.window.announce(msg, if bad { gtk::AccessibleAnnouncementPriority::High } else { gtk::AccessibleAnnouncementPriority::Medium });
    }
    let ticket = {
        let mut st = ui.st.borrow_mut();
        st.toast_gen += 1;
        st.toast_gen
    };
    // Long enough to read: an error stays 5 s and 50 ms more for each character.
    let ms = if bad { 5000 + 50 * msg.chars().count() as u64 } else { 3200 };
    hide_toast_after(ui, ticket, ms);
}

/// Hides toast `ticket` after `ms`, unless another took its place or the pointer is on it (then
/// leaving it hides it a moment later).
fn hide_toast_after(ui: &Ui, ticket: u64, ms: u64) {
    let ui = ui.clone();
    glib::timeout_add_local_once(Duration::from_millis(ms), move || {
        if ui.st.borrow().toast_gen == ticket && !ui.w.toast_hover.contains_pointer() {
            ui.w.toast.set_visible(false);
        }
    });
}

// ---------- keyboard ----------

fn focus_in_text(ui: &Ui) -> Option<gtk::Widget> {
    let f = gtk::prelude::GtkWindowExt::focus(&ui.window)?;
    (f.is::<gtk::Text>() || f.is::<gtk::TextView>()).then_some(f)
}

/// What a key does, as data: the window's key handler turns a key into one of these, so every
/// shortcut can be tested without a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    Switcher,
    View(View),
    Down,
    Up,
    Open,
    GoTo,
    Capture,
    Search,
    Pause,
    Tick,
    Done,
    Stop,
    StopAllStale,
    Readouts,
    Expand,
    Collapse,
    Escape,
    /// `?`: every key, in a small overlay.
    Keys,
    /// `a`: ask a question (the Questions view, the keyboard in its input).
    Ask,
    /// `z`: detailed or reduced, for now.
    Density,
    /// Ideas, `s`: start an agent on the ticked ideas (or the selected one).
    StartAgent,
}

/// The command for `key` in `view`, with no Ctrl or Alt held and no text field focused.
/// `on_control`: a button or dropdown has the focus, so Enter and Space are its own.
/// Letters ignore Caps Lock; "Stop all stale" needs a real Shift because it stops several agents.
pub fn command_for(key: gdk::Key, shift: bool, view: View, on_control: bool) -> Option<Cmd> {
    use gdk::Key as K;

    if on_control && matches!(key, K::Return | K::KP_Enter | K::space) {
        return None;
    }
    let agents = view == View::Agents;
    let ideas = view == View::Ideas;
    let settings = view == View::Settings;
    if key == K::S && shift {
        return agents.then_some(Cmd::StopAllStale);
    }
    Some(match key.to_lower() {
        K::g => Cmd::Switcher,
        K::question => Cmd::Keys,
        K::_1 | K::KP_1 => Cmd::View(View::Monitor),
        K::_2 | K::KP_2 => Cmd::View(View::Agents),
        K::_3 | K::KP_3 => Cmd::View(View::Projects),
        K::_4 | K::KP_4 => Cmd::View(View::Ideas),
        K::_5 | K::KP_5 => Cmd::View(View::History),
        K::_6 | K::KP_6 => Cmd::View(View::Questions),
        K::comma => Cmd::View(View::Settings),
        K::z => Cmd::Density,
        K::a => Cmd::Ask,
        K::j | K::Down => Cmd::Down,
        K::k | K::Up => Cmd::Up,
        K::Return | K::KP_Enter => Cmd::Open,
        K::space if settings => Cmd::Open,
        K::o => Cmd::GoTo,
        K::c => Cmd::Capture,
        K::slash => Cmd::Search,
        K::p => Cmd::Pause,
        K::x if ideas => Cmd::Tick,
        K::d if ideas => Cmd::Done,
        K::s | K::Delete if agents => Cmd::Stop,
        K::s if ideas => Cmd::StartAgent,
        K::v if agents => Cmd::Readouts,
        K::Right | K::l if agents || settings => Cmd::Expand,
        K::Left | K::h if agents || settings => Cmd::Collapse,
        K::Escape => Cmd::Escape,
        _ => return None,
    })
}

pub fn on_key(ui: &Ui, key: gdk::Key, mods: gdk::ModifierType) -> glib::Propagation {
    use glib::Propagation::{Proceed, Stop};
    // Hold-to-peek first: while a peek is on, every key is its own.
    if let Some(p) = peek::on_press(ui, key, mods) {
        return p;
    }
    let ctrl = mods.contains(gdk::ModifierType::CONTROL_MASK);
    let alt = mods.contains(gdk::ModifierType::ALT_MASK);
    if ctrl && !alt && key.to_lower() == gdk::Key::k {
        if ui.st.borrow().sw.open {
            switcher::close(ui);
        } else {
            switcher::open(ui);
        }
        return Stop;
    }
    if ui.st.borrow().modal_open {
        return dialogs::on_key(ui, key, ctrl);
    }
    if ui.st.borrow().sw.open {
        return switcher::on_key(ui, key);
    }
    // The rail's drawer (a narrow window): Esc puts it away.
    if ui.w.body.rail_open() && key == gdk::Key::Escape {
        set_rail_open(ui, false);
        focus_list(ui);
        return Stop;
    }
    // The capture bar's project picker: arrows, Enter, Esc.
    if let Some(p) = crate::capture::on_key(ui, key, mods) {
        return p;
    }
    // Tab, Shift+Tab and F6 move between the window's regions.
    if let Some(p) = crate::keynav::on_key(ui, key, mods) {
        return p;
    }
    // Questions has its own keys (Enter asks, Esc stops an answer, n, y, r, Delete, /).
    if ui.st.borrow().view == View::Questions
        && let Some(p) = crate::questions::on_key(ui, key, mods)
    {
        return p;
    }
    if let Some(f) = focus_in_text(ui) {
        if key == gdk::Key::Escape {
            if ui.st.borrow().edit_next {
                ui.st.borrow_mut().edit_next = false;
                panel::render(ui);
                return Stop;
            }
            if ui.st.borrow().adding {
                ui.st.borrow_mut().adding = false;
                views::render_view(ui);
                return Stop;
            }
            if f.is_ancestor(&ui.w.history_entry) && !ui.w.history_entry.text().is_empty() {
                ui.w.history_entry.set_text("");
                return Stop;
            }
            focus_list(ui);
            return Stop;
        }
        let in_history = f.is_ancestor(&ui.w.history_entry);
        if in_history && (key == gdk::Key::Down || key == gdk::Key::Up) {
            move_sel(ui, if key == gdk::Key::Down { 1 } else { -1 });
            // Moving the selection must not take the keyboard out of the search box.
            ui.w.history_entry.grab_focus_without_selecting();
            return Stop;
        }
        return Proceed;
    }
    // Details pushed over the list (a narrow window): Backspace or Alt+← go back, as Esc does.
    let pushed = ui.st.borrow().lay.push && ui.st.borrow().drawer.is_some();
    if pushed && ((key == gdk::Key::BackSpace && !alt && !ctrl) || (alt && !ctrl && key == gdk::Key::Left)) {
        panel::close(ui);
        return Stop;
    }
    if alt || ctrl {
        return Proceed;
    }
    // Behind the remote: Esc acknowledges its banner, Shift+P pulls.
    if let Some(p) = crate::disk::on_key(ui, key, mods) {
        return p; // disk.rs hook: m sets a session's project
    }
    if let Some(p) = crate::behind::on_key(ui, key, mods) {
        return p;
    }
    let focus = gtk::prelude::GtkWindowExt::focus(&ui.window);
    // An open drop-down list has every key (arrows, Enter, Esc, typing).
    if focus.as_ref().is_some_and(|f| f.ancestor(gtk::Popover::static_type()).is_some()) {
        return Proceed;
    }
    let on_control = focus.as_ref().is_some_and(|f| f.is::<gtk::Button>() || f.is::<gtk::CheckButton>() || f.is::<gtk::DropDown>() || f.ancestor(gtk::DropDown::static_type()).is_some());
    let view = ui.st.borrow().view;
    let shift = mods.contains(gdk::ModifierType::SHIFT_MASK);
    match command_for(key, shift, view, on_control) {
        Some(cmd) if run_cmd(ui, cmd) => Stop,
        _ => Proceed,
    }
}

/// What a widget under a click is, for telling a click on blank space from one on something.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Hit {
    /// A list row (a click selects it).
    Row,
    /// Something you click to act on: a button, a box to type in, a drop-down, a switch...
    Control,
    /// The side panel itself.
    Panel,
    /// Layout and text: blank as far as a click goes.
    Plain,
}

fn hit_of(w: &gtk::Widget, drawer: &gtk::Box) -> Hit {
    if w == drawer.upcast_ref::<gtk::Widget>() {
        Hit::Panel
    } else if w.is::<gtk::ListBoxRow>() {
        Hit::Row
    } else if w.is::<gtk::Button>()
        || w.is::<gtk::CheckButton>()
        || w.is::<gtk::DropDown>()
        || w.is::<gtk::Entry>()
        || w.is::<gtk::Text>()
        || w.is::<gtk::TextView>()
        || w.is::<gtk::Switch>()
        || w.is::<gtk::Scale>()
        || w.is::<gtk::Scrollbar>()
        || w.is::<gtk::SearchEntry>()
        || w.is::<gtk::Popover>()
    {
        Hit::Control
    } else {
        Hit::Plain
    }
}

/// A click on blank space: nothing between the point and the view is a row, a control or the panel.
pub fn blank_click(chain: &[Hit]) -> bool {
    chain.iter().all(|h| *h == Hit::Plain)
}

/// Does `cmd`; false if it had nothing to act on (so the key goes on to the focused widget).
fn run_cmd(ui: &Ui, cmd: Cmd) -> bool {
    let view = ui.st.borrow().view;
    let sel = ui.st.borrow().sel.clone();
    match cmd {
        Cmd::Switcher => switcher::open(ui),
        Cmd::View(v) => set_view(ui, v),
        Cmd::Keys => dialogs::keys(ui),
        Cmd::Ask => crate::questions::ask_key(ui),
        Cmd::Down => move_sel(ui, 1),
        Cmd::Up => move_sel(ui, -1),
        Cmd::Open => open_sel(ui),
        Cmd::GoTo => match selected_session(ui) {
            Some(id) => go_to(ui, &id),
            None => return false,
        },
        Cmd::Capture => crate::capture::begin(ui),
        Cmd::Search => set_view(ui, View::History),
        Cmd::Pause => {
            let id = {
                let st = ui.st.borrow();
                let project_of = |sid: &str| st.ov.as_ref().and_then(|ov| ov.sessions.iter().find(|s| s.id == sid).and_then(|s| s.project.clone()));
                match &st.drawer {
                    Some(Drawer::Project(id)) => Some(id.clone()),
                    Some(Drawer::Session(id)) => project_of(id),
                    None if view == View::Projects => sel,
                    None if matches!(view, View::Monitor | View::History) => sel.as_deref().and_then(project_of).or(st.proj_filter.clone()),
                    None => st.proj_filter.clone(),
                }
            };
            match id {
                Some(id) => dialogs::pause(ui, &id),
                None => toast(ui, "Pick a project first: select one in Projects, or a session in Monitor.", false),
            }
        }
        Cmd::Tick => {
            if let Some(sel) = sel {
                views::toggle_pick(ui, &sel);
            }
        }
        Cmd::Done => {
            if let Some(sel) = sel {
                views::toggle_done(ui, &sel);
            }
        }
        Cmd::Stop => {
            // Only a process's own row: a subagent's or the fold's row never stops anything.
            if view == View::Agents
                && let Some(agents::RowKey::Session(pid)) = sel.as_deref().map(agents::row_key)
            {
                agents::stop_one(ui, pid);
            }
        }
        Cmd::StopAllStale => agents::stop_all_stale(ui),
        Cmd::StartAgent => crate::start::open(ui, None),
        Cmd::Readouts => agents::toggle_readouts(ui),
        Cmd::Density => {
            let on = !ui.st.borrow().detailed;
            set_detailed(ui, on, true);
        }
        Cmd::Expand | Cmd::Collapse if view == View::Settings => return setview::step(ui, if cmd == Cmd::Expand { 1 } else { -1 }),
        Cmd::Expand | Cmd::Collapse if view == View::Agents => return agents::key(ui, if cmd == Cmd::Expand { agents::TreeKey::Right } else { agents::TreeKey::Left }),
        Cmd::Expand | Cmd::Collapse => return false,
        Cmd::Escape => {
            let (drawer, brief, pf) = {
                let st = ui.st.borrow();
                (st.drawer.is_some(), st.show_brief, st.proj_filter.is_some())
            };
            if drawer {
                panel::close(ui);
            } else if brief {
                ui.st.borrow_mut().show_brief = false;
                views::render_view(ui);
            } else if pf {
                {
                    let mut st = ui.st.borrow_mut();
                    st.proj_filter = None;
                    st.filter = Filter::Live;
                }
                render_all(ui);
            } else {
                return false;
            }
        }
    }
    true
}

/// The session `o` goes to: the open panel's, else the selected row's.
pub fn selected_session(ui: &Ui) -> Option<String> {
    let st = ui.st.borrow();
    match &st.drawer {
        Some(Drawer::Session(id)) => Some(id.clone()),
        _ => match st.view {
            View::Monitor | View::History => st.sel.clone(),
            // A subagent's row (or the fold's) stands for its session.
            View::Agents => agents::selected_proc(&st).and_then(|p| p.session_id.clone()),
            _ => None,
        },
    }
}

/// Focus back on the current view's list, on the selected row if any.
pub fn focus_list(ui: &Ui) {
    let v = ui.st.borrow().view;
    let list = &ui.w.views[&v].list;
    if let Some(row) = list.selected_row() {
        row.grab_focus();
    } else if let Some(row) = rows(ui).first() {
        row.grab_focus();
    } else {
        list.grab_focus();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gdk::Key as K;
    #[test]
    fn only_a_click_on_nothing_but_layout_is_blank() {
        use Hit::*;
        assert!(blank_click(&[Plain, Plain, Plain]), "empty space below the rows");
        assert!(blank_click(&[]), "the view's own background");
        assert!(!blank_click(&[Plain, Row, Plain]), "a label inside a row: the row selects");
        assert!(!blank_click(&[Plain, Control, Row]), "a button's label");
        assert!(!blank_click(&[Plain, Panel]), "anywhere in the side panel");
        assert!(!blank_click(&[Control]), "a chip, an entry, a scrollbar");
    }

    #[test]
    fn rows_that_open_the_panel_open_it_on_a_double_click() {
        assert!(opens_panel(View::Monitor) && opens_panel(View::Agents) && opens_panel(View::Projects) && opens_panel(View::History));
        assert!(!opens_panel(View::Ideas) && !opens_panel(View::Settings) && !opens_panel(View::Questions));
    }

    #[test]
    fn keys_map_to_commands_whatever_caps_lock_says() {
        assert_eq!(command_for(K::j, false, View::Monitor, false), Some(Cmd::Down));
        assert_eq!(command_for(K::J, false, View::Monitor, false), Some(Cmd::Down), "Caps Lock j still moves");
        assert_eq!(command_for(K::_2, false, View::Monitor, false), Some(Cmd::View(View::Agents)));
        assert_eq!(command_for(K::o, false, View::History, false), Some(Cmd::GoTo));
        assert_eq!(command_for(K::g, false, View::Ideas, false), Some(Cmd::Switcher));
        assert_eq!(command_for(K::comma, false, View::Agents, false), Some(Cmd::View(View::Settings)));
        assert_eq!(command_for(K::z, false, View::Monitor, false), Some(Cmd::Density));
        assert_eq!(command_for(K::Z, false, View::Monitor, false), Some(Cmd::Density), "Caps Lock z too");
    }

    #[test]
    fn settings_arrows_change_the_selected_setting_and_free_keys_stay_free() {
        assert_eq!(command_for(K::Right, false, View::Settings, false), Some(Cmd::Expand));
    }

    #[test]
    fn the_selection_bar_stretches_while_travelling_and_lands_exactly() {
        let (a, b) = ((100.0, 140.0), (300.0, 360.0));
        // Starts on the first row, ends exactly on the second, either way.
        assert_eq!(stretch(a, b, 0.0), a);
        assert_eq!(stretch(a, b, 1.0), b);
        assert_eq!(stretch(b, a, 1.0), a);
        // Mid-way it is longer than either row: the leading edge is ahead of the trailing one.
        let (top, bottom) = stretch(a, b, 0.4);
        assert!(bottom - top > 60.0, "{top}..{bottom}");
        let (top, bottom) = stretch(b, a, 0.4);
        assert!(bottom - top > 60.0, "going up too: {top}..{bottom}");
        // Never inverted or thinner than the bar.
        for i in 0..=20 {
            let (t, bt) = stretch(a, b, f64::from(i) / 20.0);
            assert!(bt - t >= 2.0);
        }
        assert_eq!(command_for(K::h, false, View::Settings, false), Some(Cmd::Collapse));
        assert_eq!(command_for(K::space, false, View::Settings, false), Some(Cmd::Open), "Space flips a switch");
        assert_eq!(command_for(K::space, false, View::Monitor, false), None);
        assert_eq!(command_for(K::Right, false, View::Monitor, false), None, "arrows elsewhere are GTK's");
        // z was free: no view uses it for anything else.
        for v in View::ALL {
            assert_eq!(command_for(K::z, false, v, false), Some(Cmd::Density));
        }
    }

    #[test]
    fn only_slipstream_gets_the_close_only_title_bar() {
        assert!(slipstream_desktop("Slipstream"));
        assert!(slipstream_desktop("slipstream:GNOME"));
        assert!(!slipstream_desktop("KDE"));
        assert!(!slipstream_desktop(""));
        assert!(!slipstream_desktop("NotSlipstream"));
    }

    #[test]
    fn stopping_keys_only_in_agents_and_stop_all_needs_a_real_shift() {
        assert_eq!(command_for(K::s, false, View::Agents, false), Some(Cmd::Stop));
        assert_eq!(command_for(K::S, false, View::Agents, false), Some(Cmd::Stop), "Caps Lock S is one stop, not all");
        assert_eq!(command_for(K::S, true, View::Agents, false), Some(Cmd::StopAllStale));
        assert_eq!(command_for(K::S, true, View::Monitor, false), None);
        assert_eq!(command_for(K::s, false, View::Monitor, false), None);
        assert_eq!(command_for(K::Right, false, View::Agents, false), Some(Cmd::Expand));
        assert_eq!(command_for(K::v, false, View::Agents, false), Some(Cmd::Readouts));
    }

    #[test]
    fn enter_on_a_button_is_the_button_s() {
        assert_eq!(command_for(K::Return, false, View::Agents, true), None);
        assert_eq!(command_for(K::space, false, View::Agents, true), None);
        assert_eq!(command_for(K::Return, false, View::Monitor, false), Some(Cmd::Open));
        assert_eq!(command_for(K::x, false, View::Ideas, false), Some(Cmd::Tick));
        assert_eq!(command_for(K::x, false, View::Monitor, false), None);
    }

    #[test]
    fn s_starts_an_agent_in_ideas_and_stops_one_only_in_agents() {
        assert_eq!(command_for(K::s, false, View::Ideas, false), Some(Cmd::StartAgent));
        assert_eq!(command_for(K::s, false, View::Agents, false), Some(Cmd::Stop));
        assert_eq!(command_for(K::s, false, View::Projects, false), None);
        assert_eq!(command_for(K::Delete, false, View::Ideas, false), None, "Delete never starts anything");
    }
}
