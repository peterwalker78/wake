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
            View::Questions => "Ask",
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
    /// Which tab: Find (sessions) or New idea.
    pub idea: bool,
    /// New idea: the project chosen for it (None: unsorted); None until one is picked (the guess).
    pub proj: Option<Option<String>>,
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
    /// Agents: Claude's own background processes (daemon, terminal hosts, unused spares) shown.
    pub show_background: bool,
    /// The first overview has arrived (the keyboard is put in the list once, then).
    pub started: bool,
    /// Agents asked to stop: their rows stay, dimmed, until the process has gone.
    pub agent_leaving: HashSet<u32>,
    /// Their rows are folding away: the list waits to be redrawn until they have.
    pub agents_folding: bool,
    /// The section of the details shown as a page of its own ("asked", "files"...), else the summary.
    pub panel_page: Option<&'static str>,
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
    pub toast_gen: u64,
    /// Agents: the screensaver's full readouts instead of the small waves (`v`).
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
            panel_page: None,
            show_background: false,
            agent_leaving: HashSet::new(),
            started: false,
            agents_folding: false,
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
            toast_gen: 0,
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

/// The selection block: the one highlight of the selected row, the shape of the row's own box. It
/// stays on the row, placed each time the list is laid out (so a resize or a redraw never leaves it
/// behind), and glides to the next row when the selection moves.
pub struct Glide {
    pub over: gtk::Overlay,
    pub bar: gtk::Box,
    /// The selected row's box, which the block sits on.
    pub target: RefCell<Option<gtk::Widget>>,
    /// While gliding, where the block is now (x, y, width, height); at rest, None (it follows the target).
    pub anim: std::cell::Cell<Option<(f64, f64, f64, f64)>>,
    /// Where the block was last placed, and which row it is on.
    pub at: std::cell::Cell<Option<(f64, f64, f64, f64)>>,
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
    /// Find and New idea, the joined pair at the start of the title bar.
    pub pair: gtk::Box,
    /// Back, at the start of the title bar, while the details are a page of their own.
    pub back_btn: gtk::Button,
    pub counts: crate::layout::Squeeze,
    /// The three places, as tabs in the top bar: Monitor, Projects, History.
    pub tabs: gtk::Box,
    pub indexing: gtk::Label,
    pub clock: gtk::Label,
    /// The view switcher's bottom form, in a phone-narrow window.
    pub tabs_bottom: gtk::Box,
    /// New idea (+) and the main menu in the title bar.
    pub add_btn: gtk::Button,
    pub menu: gtk::MenuButton,
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
    // It fills the rectangle the overlay gives it (an empty box's own size is nothing).
    let bar = w::vbox(0, &["selbar"]);
    bar.set_halign(gtk::Align::Fill);
    bar.set_valign(gtk::Align::Fill);
    bar.set_can_target(false);
    bar.set_visible(false);
    over.add_overlay(&bar);
    let scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&over).build();
    root.append(&top);
    root.append(&head);
    root.append(&scroll);
    let glide = Rc::new(Glide { over, bar, target: Default::default(), anim: Default::default(), at: Default::default(), key: Default::default(), stop: Default::default() });
    {
        // The block's place, worked out whenever the overlay lays out: where the glide has got
        // to, else on its row's box.
        let g = Rc::downgrade(&glide);
        glide.over.connect_get_child_position(move |over, child| {
            let g = g.upgrade()?;
            if child != g.bar.upcast_ref::<gtk::Widget>() {
                return None;
            }
            let rect = g.anim.get().or_else(|| {
                let t = g.target.borrow().clone()?;
                t.parent()?;
                t.compute_bounds(over).map(|b| (f64::from(b.x()), f64::from(b.y()), f64::from(b.width()), f64::from(b.height())))
            });
            let Some((x, y, w, h)) = rect else { return Some(gdk::Rectangle::new(0, 0, 0, 0)) };
            g.at.set(Some((x, y, w, h)));
            Some(gdk::Rectangle::new(x.round() as i32, y.round() as i32, w.round().max(1.0) as i32, h.round().max(1.0) as i32))
        });
    }
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

    // The title bar, laid out as GTK apps lay theirs out: Find and New idea at the start, the view
    // switcher in the middle, Ask, the main menu and the window's buttons at the end. It drags and
    // double-clicks like any title bar.
    // A row (not a CenterBox, whose centring doubles the wider side's minimum): start, the switcher
    // between two equal spacers, end. Start and end share a width (a size group) while there is
    // room, so the switcher sits in the middle.
    let top = w::hbox(0, &["top"]);
    let start = w::hbox(6, &["hstart"]);
    // Back, where GTK apps put it, while the details fill the window (a phone-narrow one).
    let back_btn = gtk::Button::new();
    back_btn.set_child(Some(&w::icon(w::icons::BACK)));
    back_btn.add_css_class("flat");
    back_btn.set_tooltip_text(Some("Back to the list (Esc)"));
    back_btn.update_property(&[gtk::accessible::Property::Label("Back")]);
    back_btn.set_valign(gtk::Align::Center);
    back_btn.set_visible(false);
    start.append(&back_btn);
    // Find and New idea: a pair of buttons, joined, opening the two tabs of the one pop-up. Short of
    // the full width their names make way and the icons stay.
    let pair_btn = |icon: &[&str], name: &str, tip: &str| {
        let b = gtk::Button::new();
        b.add_css_class("pairbtn");
        let inner = w::hbox(7, &[]);
        inner.append(&w::icon(icon));
        inner.append(&w::label(name, &["pairname"]));
        b.set_child(Some(&inner));
        b.set_tooltip_text(Some(tip));
        b.update_property(&[gtk::accessible::Property::Label(name)]);
        b.set_valign(gtk::Align::Center);
        b
    };
    let cmd = pair_btn(w::icons::FIND, "Find", "Find a session (Ctrl+F)");
    let add_btn = pair_btn(w::icons::ADD, "New idea", "New idea (Ctrl+N)");
    let pair = w::hbox(0, &["pair"]);
    pair.set_valign(gtk::Align::Center);
    pair.append(&cmd);
    pair.append(&add_btn);
    start.append(&pair);
    top.append(&start);
    let gap_l = w::spacer();
    top.append(&gap_l);
    // The view switcher: Monitor, Projects, Ideas, History, Agents.
    let tabs = w::hbox(4, &["tabs"]);
    tabs.update_property(&[gtk::accessible::Property::Label("Views")]);
    top.append(&tabs);
    let gap_r = w::spacer();
    top.append(&gap_r);
    let end = w::hbox(6, &["hend"]);
    let indexing = w::label("", &["indexing"]);
    indexing.set_visible(false);
    end.append(&indexing);
    // Ask, with its answers' badge (views.rs fills it).
    let counts = crate::layout::Squeeze::new(6, &["counts"]);
    counts.set_valign(gtk::Align::Center);
    end.append(&counts);
    let menu = gtk::MenuButton::new();
    menu.set_icon_name("open-menu-symbolic");
    menu.add_css_class("flat");
    menu.set_tooltip_text(Some("Main menu"));
    menu.set_valign(gtk::Align::Center);
    end.append(&menu);
    let controls = gtk::WindowControls::new(gtk::PackType::End);
    controls.set_valign(gtk::Align::Center);
    end.append(&controls);
    top.append(&end);
    // The clock isn't in the bar (the desktop has one); it is kept for the code that ticks it.
    let clock = w::label(&crate::fmt::clock(), &["clock"]);
    clock.set_visible(false);
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

        views[&View::History].top.append(&sb);
    }
    main.append(&stack);

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

    // A banner under the title bar for what you should know (demo data, Wake falling behind, tabs it
    // can't see); hidden while all is well.
    let status = w::hbox(10, &["statusbar"]);
    // In a phone-narrow window the view switcher moves to the bottom, as GTK apps do.
    let tabs_bottom = w::hbox(0, &["tabs", "bottomtabs"]);
    tabs_bottom.set_visible(false);

    let column = w::vbox(0, &[]);
    column.append(&handle);
    column.append(&status);
    // The body's minimum width is that of its narrowest layout, whatever is open, so opening the
    // panel never changes the window's minimum size (Slipstream, like any tiling compositor,
    // scales down a window whose minimum outgrows its tile).
    column.append(&body);
    column.append(&tabs_bottom);

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
            pair: pair.clone(),
            counts,
            tabs,
            tabs_bottom,
            add_btn: add_btn.clone(),
            back_btn: back_btn.clone(),
            menu: menu.clone(),
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
    }

    // Signals.
    {
        let ui = ui.clone();
        cmd.connect_clicked(move |_| switcher::open(&ui));
    }
    {
        let ui2 = ui.clone();
        ui.w.back_btn.connect_clicked(move |_| panel::back(&ui2));
    }
    {
        let ui2 = ui.clone();
        ui.w.add_btn.connect_clicked(move |_| open_capture(&ui2));
    }
    {
        // The main menu: Settings, the keys, and the demo board.
        let pop = gtk::Popover::new();
        let items = w::vbox(2, &["moremenu"]);
        let item = |label: &str, f: Box<dyn Fn()>| {
            let b = gtk::Button::with_label(label);
            b.add_css_class("menuitem");
            let pop = pop.clone();
            b.connect_clicked(move |_| {
                pop.popdown();
                f();
            });
            items.append(&b);
        };
        let ui2 = ui.clone();
        item("Settings", Box::new(move || set_view(&ui2, View::Settings)));
        let ui2 = ui.clone();
        item("Keyboard shortcuts", Box::new(move || dialogs::keys(&ui2)));
        let ui2 = ui.clone();
        item("Ask a question", Box::new(move || set_view(&ui2, View::Questions)));
        pop.set_child(Some(&items));
        ui.w.menu.set_popover(Some(&pop));
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
                    st.sel = Some(key.clone());
                }
                follow_sel(&ui2, v, key);
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
                Some("questions") => find_dropdown(ui.w.views[&View::Questions].root.upcast_ref()),
                Some("settings-now") => find_dropdown(ui.w.views[&View::Settings].root.upcast_ref()),
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
            let first = !ui.st.borrow().started;
            ui.st.borrow_mut().started = true;
            render_all(ui);
            panel::tick_procs(ui);
            // On opening, the keyboard starts in the list (not on the title bar's first button).
            if first {
                let ui = ui.clone();
                glib::idle_add_local_once(move || focus_list(&ui));
            }
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
    if ui.st.borrow().drawer.is_some() && !typing_in(ui, &ui.w.drawer) {
        panel::render(ui);
    }
    tick_live(ui);
}

/// Whether the tables show their compact columns: the list is narrow, or the details share the
/// row with it.
pub fn compact(st: &AppState) -> bool {
    // Over the list or a page of its own, the details leave the list its width.
    st.lay.narrow || (st.drawer.is_some() && !st.lay.push && !st.lay.overlay)
}

/// Whether rows are two-line cells rather than columns: the list is narrow, alone or beside the
/// details (columns cut to a few letters say less than two lines).
pub fn cards(st: &AppState) -> bool {
    st.lay.narrow || (st.drawer.is_some() && !st.lay.push && !st.lay.overlay && st.lay.narrow_beside)
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
    // Back shows while the details fill the window.
    ui.w.back_btn.set_visible(lay.push && ui.st.borrow().drawer.is_some());
    // Short of the full width, Find and New idea keep their icons and their names make way, so the
    // view switcher keeps its room.
    let short_top = lay.rail != Rail::Full;
    let mut c = ui.w.pair.first_child();
    while let Some(b) = c {
        if let Some(name) = b.first_child().and_then(|i| i.last_child()) {
            name.set_visible(!short_top);
        }
        c = b.next_sibling();
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

/// With the details open, picking another row shows its details instead (after the click is
/// handled, so a redraw never runs inside it).
pub fn follow_sel(ui: &Ui, v: View, key: String) {
    let open = ui.st.try_borrow().ok().and_then(|st| st.drawer.clone());
    let Some(d) = open else { return };
    let same = match &d {
        Drawer::Session(id) | Drawer::Project(id) => *id == key,
    };
    if same || key.is_empty() {
        return;
    }
    let ui = ui.clone();
    glib::idle_add_local_once(move || {
        if ui.st.borrow().view != v || ui.st.borrow().drawer.is_none() {
            return;
        }
        match v {
            View::Monitor | View::History => panel::open_session(&ui, &key, false),
            View::Projects => panel::open_project(&ui, &key, false),
            // An agent's row: its session's details, when it has a session.
            View::Agents => {
                let sid = agents::selected_proc(&ui.st.borrow()).and_then(|p| p.session_id.clone());
                if let Some(sid) = sid.filter(|s| ui.st.borrow().drawer != Some(Drawer::Session(s.clone()))) {
                    panel::open_session(&ui, &sid, false);
                }
            }
            _ => {}
        }
    });
}

/// A view drawn as grids of cards (Projects, Ideas): its grids, one per section, in order.
pub fn grids_of(ui: &Ui, view: View) -> Vec<gtk::FlowBox> {
    let mut out = Vec::new();
    let mut c = ui.w.views[&view].list.first_child();
    while let Some(x) = c {
        c = x.next_sibling();
        if x.has_css_class("gridrow")
            && let Some(g) = x.downcast::<gtk::ListBoxRow>().ok().and_then(|r| r.child()).and_then(|g| g.downcast::<gtk::FlowBox>().ok())
        {
            out.push(g);
        }
    }
    out
}

/// The first of them (Ideas has one).
pub fn grid_of(ui: &Ui, view: View) -> Option<gtk::FlowBox> {
    grids_of(ui, view).into_iter().next()
}

fn grid_cards(g: &gtk::FlowBox) -> Vec<gtk::FlowBoxChild> {
    (0..).map_while(|i| g.child_at_index(i)).collect()
}

/// One card selected across all the grids: picking one in a grid clears the others.
pub fn grid_picked(ui: &Ui, view: View, g: &gtk::FlowBox) {
    for other in grids_of(ui, view).iter().filter(|o| *o != g) {
        other.unselect_all();
    }
}

/// Moving in the grids. Across (←/→): the next or previous card in reading order, from one grid
/// into the next. Up and down (↑/↓, j/k): the card in the row above or below, nearest in line,
/// from one grid into the next, as in a file manager.
fn grid_step(ui: &Ui, view: View, d: i32, by_row: bool) {
    let grids = grids_of(ui, view);
    let cards: Vec<gtk::FlowBoxChild> = grids.iter().flat_map(grid_cards).collect();
    if cards.is_empty() {
        return;
    }
    let sel = ui.st.borrow().sel.clone();
    let at = cards.iter().position(|c| Some(c.widget_name().as_str()) == sel.as_deref());
    let i = match at {
        None if d > 0 => 0,
        None => cards.len() - 1,
        Some(i) if !by_row => (i as i32 + d).clamp(0, cards.len() as i32 - 1) as usize,
        Some(i) => {
            let list = ui.w.views[&view].list.clone();
            let b = |c: &gtk::FlowBoxChild| c.compute_bounds(&list).map(|r| (f64::from(r.x() + r.width() / 2.0), f64::from(r.y()), f64::from(r.height())));
            // Not laid out yet (just redrawn): a step in reading order instead.
            let Some((cx, cy, ch)) = b(&cards[i]).filter(|r| r.2 > 0.0) else {
                return grid_step(ui, view, d.signum(), false);
            };
            cards
                .iter()
                .enumerate()
                .filter_map(|(n, c)| b(c).map(|(x, y, _)| (n, x, y)))
                .filter(|(_, _, y)| if d > 0 { *y > cy + ch / 2.0 } else { *y < cy - ch / 2.0 })
                .min_by(|a, b| {
                    let (dya, dyb) = ((a.2 - cy).abs(), (b.2 - cy).abs());
                    dya.total_cmp(&dyb).then((a.1 - cx).abs().total_cmp(&(b.1 - cx).abs()))
                })
                .map_or(i, |(n, _, _)| n)
        }
    };
    let key = cards[i].widget_name().to_string();
    ui.st.borrow_mut().sel = Some(key.clone());
    for g in &grids {
        g.unselect_all();
    }
    if let Some(g) = cards[i].parent().and_downcast::<gtk::FlowBox>() {
        g.select_child(&cards[i]);
    }
    cards[i].grab_focus();
    crate::motion::scroll_into_view(&ui.w.views[&view].scroll, &cards[i]);
    let open = matches!(ui.st.borrow().drawer, Some(Drawer::Project(_)));
    if open && view == View::Projects {
        panel::open_project(ui, &key, false);
    }
}

/// ←/→ in a view of cards: the previous or next card. False elsewhere (the arrows are GTK's).
pub fn grid_across(ui: &Ui, d: i32) -> bool {
    let view = ui.st.borrow().view;
    if grids_of(ui, view).is_empty() {
        return false;
    }
    grid_step(ui, view, d, false);
    true
}

pub fn move_sel(ui: &Ui, d: i32) {
    if ui.st.borrow().view == View::Settings {
        setview::keyboard(ui, true);
    }
    let view = ui.st.borrow().view;
    if !grids_of(ui, view).is_empty() {
        grid_step(ui, view, d, true);
        return;
    }
    let rows = rows(ui);
    if rows.is_empty() {
        return;
    }
    let sel = ui.st.borrow().sel.clone();
    // From the selected row, else from the row that has the keyboard.
    let focused = gtk::prelude::GtkWindowExt::focus(&ui.window);
    let i = rows.iter().position(|r| Some(r.widget_name().as_str()) == sel.as_deref()).or_else(|| rows.iter().position(|r| focused.as_ref().is_some_and(|f| f == r.upcast_ref::<gtk::Widget>())));
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
    let grids = grids_of(ui, view);
    if !grids.is_empty() {
        for g in &grids {
            if let Some(c) = grid_cards(g).into_iter().find(|c| c.widget_name() == sel) {
                g.select_child(&c);
                if had_focus {
                    c.grab_focus();
                }
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

/// Puts the selection block on `row`'s box. When the selection moves it glides there from where
/// it was, stretching as it travels and drawing itself in as it lands; then it stays, as the row's
/// highlight. With motion off, or the same row redrawn, it is simply there.
pub fn glide_to(g: &Rc<Glide>, row: Option<&gtk::ListBoxRow>) {
    if let Some(s) = g.stop.borrow_mut().take() {
        s.set(true);
    }
    let Some(row) = row.filter(|r| !r.parent().is_some_and(|l| l.has_css_class("setlist"))) else {
        // Nothing selected (or Settings, whose cards show their own).
        g.anim.set(None);
        *g.target.borrow_mut() = None;
        g.bar.set_visible(false);
        g.key.borrow_mut().clear();
        return;
    };
    // The row's own box (a card, a boxed row) where it has one: the block takes its shape.
    let target: gtk::Widget = row.child().unwrap_or_else(|| row.clone().upcast());
    let key = row.widget_name().to_string();
    let same = *g.key.borrow() == key;
    *g.key.borrow_mut() = key;
    let from = g.at.get().filter(|_| g.bar.is_visible());
    *g.target.borrow_mut() = Some(target.clone());
    g.bar.set_visible(true);
    let Some((fx, fy, fw, fh)) = from.filter(|_| !same && crate::motion::on() && row.is_mapped()) else {
        g.anim.set(None);
        g.over.queue_allocate();
        return;
    };
    g.bar.add_css_class("moving");
    let g2 = g.clone();
    let stop = crate::motion::animate(row, STRETCH_MS, move |eased| {
        let Some(b) = target.compute_bounds(&g2.over) else { return };
        let (tx, ty, tw, th) = (f64::from(b.x()), f64::from(b.y()), f64::from(b.width()), f64::from(b.height()));
        // `animate` hands over eased progress; the two edges want their own timing, so undo it.
        let t = 1.0 - (1.0 - eased).cbrt();
        let (top, bottom) = stretch((fy, fy + fh), (ty, ty + th), t);
        let x = fx + (tx - fx) * eased;
        let w = fw + (tw - fw) * eased;
        if eased >= 1.0 {
            // Arrived: it rests on the row from now on.
            g2.anim.set(None);
            g2.bar.remove_css_class("moving");
        } else {
            g2.anim.set(Some((x, top, w, bottom - top)));
        }
        g2.over.queue_allocate();
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

// ---------- new idea ----------

/// New idea (+ or c): the Find pop-up on its New idea tab.
pub fn open_capture(ui: &Ui) {
    switcher::open_idea(ui);
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
    /// ←/→ among cards (Projects, Ideas): the previous or next card.
    Across(i32),
    /// Home and End: the first or last row (or card).
    Edge(bool),
    /// Page Up and Page Down: a screenful up or down.
    Page(i32),
    Down,
    Up,
    Open,
    /// Agents, Delete: stop the selected agent (asks first).
    Stop,
    Expand,
    Collapse,
    Escape,
}

/// The command for `key` in `view`, with no Ctrl or Alt held and no text field focused.
/// `on_control`: a button or dropdown has the focus, so Enter and Space are its own.
/// Letters ignore Caps Lock; "Stop all stale" needs a real Shift because it stops several agents.
pub fn command_for(key: gdk::Key, shift: bool, view: View, on_control: bool) -> Option<Cmd> {
    use gdk::Key as K;

    if on_control && matches!(key, K::Return | K::KP_Enter | K::space) {
        return None;
    }
    // Only the keys every desktop list has: no letters, so typing never does anything by surprise.
    let _ = shift;
    let agents = view == View::Agents;
    let settings = view == View::Settings;
    let cards = matches!(view, View::Projects | View::Ideas);
    Some(match key {
        K::Down | K::KP_Down => Cmd::Down,
        K::Up | K::KP_Up => Cmd::Up,
        K::Home | K::KP_Home => Cmd::Edge(true),
        K::End | K::KP_End => Cmd::Edge(false),
        K::Page_Down | K::KP_Page_Down => Cmd::Page(1),
        K::Page_Up | K::KP_Page_Up => Cmd::Page(-1),
        K::Return | K::KP_Enter => Cmd::Open,
        K::space if settings => Cmd::Open,
        K::Delete | K::KP_Delete if agents => Cmd::Stop,
        K::Right | K::KP_Right if agents || settings => Cmd::Expand,
        K::Left | K::KP_Left if agents || settings => Cmd::Collapse,
        K::Right | K::KP_Right if cards => Cmd::Across(1),
        K::Left | K::KP_Left if cards => Cmd::Across(-1),
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
    // The shortcuts people bring from other desktop apps (Ctrl+F, Ctrl+N, F5, F10...).
    if let Some(p) = standard_key(ui, key, mods) {
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
    // Details open: Alt+← (anywhere) and Backspace go back a page, then close them, as Esc does.
    let open = ui.st.borrow().drawer.is_some();
    if open && ((key == gdk::Key::BackSpace && !alt && !ctrl) || (alt && !ctrl && key == gdk::Key::Left)) {
        panel::back(ui);
        return Stop;
    }
    if alt || ctrl {
        return Proceed;
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

/// Wake's few shortcuts, as in other desktop apps: Ctrl+F finds, Ctrl+N is a new idea, Ctrl+Tab
/// steps through the views, F1 shows the keys, Ctrl+Q quits, and Shift+F10 or the Menu key opens
/// the selected item's ⋯ menu.
fn standard_key(ui: &Ui, key: gdk::Key, mods: gdk::ModifierType) -> Option<glib::Propagation> {
    use gdk::Key as K;
    let ctrl = mods.contains(gdk::ModifierType::CONTROL_MASK);
    let shift = mods.contains(gdk::ModifierType::SHIFT_MASK);
    let alt = mods.contains(gdk::ModifierType::ALT_MASK);
    if alt {
        return None;
    }
    match key {
        K::f | K::F if ctrl => switcher::open(ui),
        K::n | K::N if ctrl => open_capture(ui),
        K::q | K::Q if ctrl => ui.window.close(),
        K::F1 => dialogs::keys(ui),
        K::F10 if shift => context_menu(ui),
        K::Menu => context_menu(ui),
        K::Tab | K::ISO_Left_Tab if ctrl => step_view(ui, if shift || key == K::ISO_Left_Tab { -1 } else { 1 }),
        _ => return None,
    }
    Some(glib::Propagation::Stop)
}

/// Ctrl+Tab: the next view in the switcher's order (wrapping); back with Shift.
fn step_view(ui: &Ui, d: i32) {
    const ORDER: [View; 5] = [View::Monitor, View::Projects, View::Ideas, View::History, View::Agents];
    let now = ui.st.borrow().view;
    let i = ORDER.iter().position(|v| *v == now).map_or(0, |i| (i as i32 + d).rem_euclid(ORDER.len() as i32) as usize);
    set_view(ui, ORDER[i]);
}

/// Shift+F10 or the Menu key: the selected item's ⋯ menu (its details open first).
fn context_menu(ui: &Ui) {
    // Projects: the selected card's (or row's) own menu, where it is.
    let (view, sel, open) = {
        let st = ui.st.borrow();
        (st.view, st.sel.clone(), st.drawer.is_some())
    };
    if view == View::Projects
        && !open
        && let Some(sel) = sel
    {
        let list: gtk::Widget = ui.w.views[&View::Projects].list.clone().upcast();
        if let Some(item) = panel::find_named(&list, &sel)
            && let Some(mb) = panel::find_named(&item, "card-more").and_downcast::<gtk::MenuButton>()
        {
            mb.popup();
            return;
        }
    }
    if !open {
        open_sel(ui);
    }
    let ui = ui.clone();
    glib::idle_add_local_once(move || {
        let root: gtk::Widget = ui.w.drawer.clone().upcast();
        if let Some(mb) = panel::find_named(&root, "panel-more").or_else(|| panel::find_named(&root, "project-more")).and_downcast::<gtk::MenuButton>() {
            mb.popup();
        }
    });
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
        Cmd::Down => move_sel(ui, 1),
        Cmd::Up => move_sel(ui, -1),
        Cmd::Edge(first) => {
            let view = ui.st.borrow().view;
            if grids_of(ui, view).is_empty() {
                let rs = rows(ui);
                if let Some(r) = if first { rs.first() } else { rs.last() } {
                    select_row(ui, r);
                }
            } else {
                // Cards: from no selection, a step down lands on the first and a step up on the last.
                ui.st.borrow_mut().sel = None;
                move_sel(ui, if first { 1 } else { -1 });
            }
        }
        Cmd::Page(d) => {
            for _ in 0..8 {
                move_sel(ui, d);
            }
        }
        Cmd::Open => open_sel(ui),
        Cmd::Stop => {
            // Only a process's own row: a subagent's or the fold's row never stops anything.
            if view == View::Agents
                && let Some(agents::RowKey::Session(pid)) = sel.as_deref().map(agents::row_key)
            {
                agents::stop_one(ui, pid);
            }
        }
        Cmd::Expand | Cmd::Collapse if view == View::Settings => return setview::step(ui, if cmd == Cmd::Expand { 1 } else { -1 }),
        Cmd::Expand | Cmd::Collapse if view == View::Agents => return agents::key(ui, if cmd == Cmd::Expand { agents::TreeKey::Right } else { agents::TreeKey::Left }),
        Cmd::Expand | Cmd::Collapse => return false,
        Cmd::Across(d) => return grid_across(ui, d),
        Cmd::Escape => {
            let (drawer, brief, pf) = {
                let st = ui.st.borrow();
                (st.drawer.is_some(), st.show_brief, st.proj_filter.is_some())
            };
            if drawer {
                // A section's page goes back to the summary; the summary closes.
                panel::back(ui);
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

/// Focus back on the current view's list, on the selected row if any.
pub fn focus_list(ui: &Ui) {
    let v = ui.st.borrow().view;
    let list = &ui.w.views[&v].list;
    // Cards: the selected one, else the first.
    let grids = grids_of(ui, v);
    if !grids.is_empty() {
        let cards: Vec<gtk::FlowBoxChild> = grids.iter().flat_map(grid_cards).collect();
        let sel = ui.st.borrow().sel.clone();
        if let Some(c) = cards.iter().find(|c| Some(c.widget_name().as_str()) == sel.as_deref()).or(cards.first()) {
            // Focused and selected, together.
            ui.st.borrow_mut().sel = Some(c.widget_name().to_string());
            if let Some(g) = c.parent().and_downcast::<gtk::FlowBox>() {
                g.select_child(c);
            }
            c.grab_focus();
        }
        return;
    }
    // The keyboard and the selection are on the same row, as in any list: the selected row (found
    // again by name after a redraw), else the first, selected.
    let sel = ui.st.borrow().sel.clone();
    let row = list.selected_row().or_else(|| rows(ui).into_iter().find(|r| Some(r.widget_name().as_str()) == sel.as_deref())).or_else(|| rows(ui).into_iter().next());
    if let Some(row) = row {
        ui.st.borrow_mut().sel = Some(row.widget_name().to_string());
        list.select_row(Some(&row));
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
    fn letters_do_nothing_so_typing_never_surprises() {
        assert_eq!(command_for(K::Down, false, View::Monitor, false), Some(Cmd::Down));
        assert_eq!(command_for(K::Up, false, View::Monitor, false), Some(Cmd::Up));
        for k in [K::j, K::k, K::h, K::l, K::g, K::c, K::o, K::p, K::s, K::S, K::x, K::d, K::a, K::_1, K::_2, K::comma, K::slash, K::question] {
            for v in View::ALL {
                assert_eq!(command_for(k, false, v, false), None, "{k:?} in {v:?}");
                assert_eq!(command_for(k, true, v, false), None, "Shift+{k:?} in {v:?}");
            }
        }
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
        assert_eq!(command_for(K::Left, false, View::Settings, false), Some(Cmd::Collapse));
        assert_eq!(command_for(K::space, false, View::Settings, false), Some(Cmd::Open), "Space flips a switch");
        assert_eq!(command_for(K::space, false, View::Monitor, false), None);
        assert_eq!(command_for(K::Right, false, View::Monitor, false), None, "arrows elsewhere are GTK's");
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
    fn delete_stops_an_agent_only_in_agents_and_lists_keep_their_usual_keys() {
        assert_eq!(command_for(K::Delete, false, View::Agents, false), Some(Cmd::Stop));
        assert_eq!(command_for(K::Delete, false, View::Monitor, false), None);
        assert_eq!(command_for(K::Delete, false, View::Ideas, false), None, "Delete never starts anything");
        assert_eq!(command_for(K::Right, false, View::Agents, false), Some(Cmd::Expand));
        // As in any list on Windows or Linux: Home and End, Page Up and Page Down; and ← → among cards.
        assert_eq!(command_for(K::Home, false, View::Monitor, false), Some(Cmd::Edge(true)));
        assert_eq!(command_for(K::End, false, View::History, false), Some(Cmd::Edge(false)));
        assert_eq!(command_for(K::Page_Down, false, View::Agents, false), Some(Cmd::Page(1)));
        assert_eq!(command_for(K::Page_Up, false, View::Projects, false), Some(Cmd::Page(-1)));
        assert_eq!(command_for(K::Right, false, View::Projects, false), Some(Cmd::Across(1)));
        assert_eq!(command_for(K::Left, false, View::Ideas, false), Some(Cmd::Across(-1)));
        assert_eq!(command_for(K::Right, false, View::Monitor, false), None, "a list has no across");
    }

    #[test]
    fn enter_on_a_button_is_the_button_s() {
        assert_eq!(command_for(K::Return, false, View::Agents, true), None);
        assert_eq!(command_for(K::space, false, View::Agents, true), None);
        assert_eq!(command_for(K::Return, false, View::Monitor, false), Some(Cmd::Open));
    }
}
