//! Behind the remote, in the window. Quiet by default: a project that is behind gets one marker
//! (its row in Projects and Monitor, a block in its panel) with the one-line fix. Loud only when
//! it matters: the person started building in a project that is behind (wake-core's `Watch`
//! decides that), and then a full-width banner sits at the top of Wake until they acknowledge it
//! (`Esc` or its button) for this behind-episode. "Pull now" (`P`) runs `git pull --ff-only` after
//! a confirmation: with Remove worktree, one of the two changes Wake ever makes to a repo. On
//! screen it all reads as plain sentences; the git command is in the tooltips and the dialog.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use gtk4::{self as gtk, gdk, glib, prelude::*};
use wake_core::*;

use crate::{
    app::{self, Drawer, Ui},
    dialogs, fmt, w,
};

const CSS: &str = "
.behind-banner { background-color: #5C0F1A; border-bottom: 3px solid #FF7A8A; padding: 12px 18px; }
.behind-banner .bb-title { color: #FFFFFF; font-weight: 700; font-size: 17px; }
.behind-banner .bb-fix { color: #FFB547; font-weight: 700; font-size: 13.5px; }
.behind-banner .bb-note { color: #FFD9DE; font-size: 12.5px; }
.behind-banner .bb-tag { font-family: \"JetBrains Mono\", monospace; font-weight: 700; font-size: 11px; letter-spacing: 1.5px; color: #2a0409; background-color: #FF7A8A; border-radius: 4px; padding: 3px 8px; }
window.wake .behind-banner button.btn { background-color: rgba(255,255,255,.08); border-color: rgba(255,255,255,.35); color: #FFFFFF; }
window.wake .behind-banner button.btn.bb-pull { background-color: #FFB547; border-color: #FFB547; color: #2a1a02; }
.behind-pill { font-family: \"JetBrains Mono\", monospace; font-weight: 700; font-size: 10px; padding: 2px 6px; border-radius: 4px; background-color: alpha(@amber, .16); color: @amber; }
.behind-pill.alarm { background-color: @rose; color: @on_rose; }
.behind-line { font-family: \"JetBrains Mono\", monospace; font-size: 11px; color: @amber; padding-top: 1px; }
.behind-line.alarm { color: @rose; }
.behind-block { margin: 2px 16px 10px 16px; padding: 10px 12px; border: 1px solid alpha(@amber, .45); border-radius: 8px; background-color: alpha(@amber, .07); }
.behind-block.alarm { border-color: @rose; background-color: alpha(@rose, .10); }
.behind-block .bk-line { color: @amber; font-size: 12.5px; }
";

struct Banner {
    /// The banner slides down into place (and up when acknowledged).
    rev: gtk::Revealer,
    title: gtk::Label,
    fix: gtk::Label,
    note: gtk::Label,
    pull: gtk::Button,
    /// The project shown, and since when.
    showing: Option<(String, Instant)>,
}

thread_local! {
    static BANNER: RefCell<Option<Banner>> = const { RefCell::new(None) };
    /// `--show behind:<project>`: raise this project's alarm on screen (screenshots only).
    static FORCED: RefCell<Option<String>> = const { RefCell::new(None) };
    /// The project panel last seen open, to ask its remote once when it opens.
    static PANEL_SEEN: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Builds the banner (hidden) above everything in the window, and its styles.
pub fn build(ui: &Ui) {
    if let Some(display) = gdk::Display::default() {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(CSS);
        gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1);
    }
    let root = w::hbox(16, &["behind-banner"]);
    root.set_visible(false);
    root.update_property(&[gtk::accessible::Property::Label("Warning: building while behind the remote")]);
    let tag = w::label("BEHIND", &["bb-tag"]);
    tag.set_valign(gtk::Align::Center);
    root.append(&tag);
    let text = w::vbox(3, &[]);
    text.set_hexpand(true);
    let title = w::wrap("", &["bb-title"]);
    let fix = w::label("", &["bb-fix"]);
    fix.set_selectable(true);
    let note = w::wrap("", &["bb-note"]);
    text.append(&title);
    text.append(&fix);
    text.append(&note);
    root.append(&text);
    let ui2 = ui.clone();
    let pull = w::button("Pull now", None, &["bb-pull"], move || {
        if let Some(id) = banner_project() {
            confirm_pull(&ui2, &id);
        }
    });
    pull.set_valign(gtk::Align::Center);
    root.append(&pull);
    let ui2 = ui.clone();
    let ack_b = w::button("I know, carry on", None, &[], move || {
        if let Some(id) = banner_project() {
            ack(&ui2, &id);
        }
    });
    ack_b.set_valign(gtk::Align::Center);
    ack_b.set_tooltip_text(Some("Hide this until the project catches up and falls behind again. The marker on the project stays."));
    root.append(&ack_b);
    root.set_visible(true);
    let rev = crate::motion::revealer(gtk::RevealerTransitionType::SlideDown, &root, false);
    rev.set_visible(false);
    ui.w.column.prepend(&rev);
    BANNER.with(|b| *b.borrow_mut() = Some(Banner { rev, title, fix, note, pull, showing: None }));
}

fn banner_project() -> Option<String> {
    BANNER.with(|b| b.borrow().as_ref().and_then(|b| b.showing.as_ref().map(|s| s.0.clone())))
}

/// The project's entry, when it is behind.
pub fn of(ui: &Ui, project: &str) -> Option<Behind> {
    let st = ui.st.borrow();
    let mut b = st.ov.as_ref()?.behind.as_ref()?.iter().find(|b| b.project == project).cloned()?;
    if FORCED.with(|f| f.borrow().as_deref() == Some(project)) && b.alarm.is_none() {
        b.alarm = Some("a session started working".into());
    }
    Some(b)
}

/// Everything behind, for redraw signatures: a view redraws when this changes.
pub fn sig(ui: &Ui) -> String {
    let forced = FORCED.with(|f| f.borrow().clone());
    let st = ui.st.borrow();
    let list = st.ov.as_ref().and_then(|o| o.behind.as_ref());
    format!("{forced:?}{}", list.map(|l| l.iter().map(|b| format!("{}|{}|{:?}|{}|{}", b.project, b.line, b.alarm, b.can_pull, b.dirty)).collect::<Vec<_>>().join(";")).unwrap_or_default())
}

fn alarmed(ui: &Ui) -> Vec<Behind> {
    let ids: Vec<String> = {
        let st = ui.st.borrow();
        st.ov.as_ref().and_then(|o| o.behind.as_ref()).map(|l| l.iter().map(|b| b.project.clone()).collect()).unwrap_or_default()
    };
    ids.iter().filter_map(|id| of(ui, id)).filter(|b| b.alarm.is_some()).collect()
}

/// After each overview: the banner, and asking a newly opened project panel's remote.
pub fn render(ui: &Ui) {
    let open = match &ui.st.borrow().drawer {
        Some(Drawer::Project(id)) => Some(id.clone()),
        _ => None,
    };
    if PANEL_SEEN.with(|p| p.replace(open.clone())) != open
        && let Some(id) = open
    {
        // On demand: opening a project asks its remote now rather than at the next check, unless
        // Settings turned remote checks off.
        if crate::prefs::get().remote_minutes > 0 {
            app::bg(ui, move |src| src.check_remotes(Some(&id)), |_, _| {});
        }
    }
    let list = alarmed(ui);
    BANNER.with(|cell| {
        let mut cell = cell.borrow_mut();
        let Some(bn) = cell.as_mut() else { return };
        let Some(b) = list.first() else {
            bn.rev.set_reveal_child(false);
            if !crate::motion::on() {
                bn.rev.set_visible(false);
            }
            bn.showing = None;
            return;
        };
        if bn.showing.as_ref().map(|s| &s.0) != Some(&b.project) {
            bn.showing = Some((b.project.clone(), Instant::now()));
            // Screen readers hear it too (not on Broadway, whose missing accessibility backend
            // crashes GTK's announce).
            if std::env::var("GDK_BACKEND").map_or(true, |b| b != "broadway") {
                ui.window.announce(&format!("Warning: {} is behind. Pull before you build.", b.project_name), gtk::AccessibleAnnouncementPriority::High);
            }
        }
        bn.title.set_text(&format!("You're building in {}. {}", b.project_name, news(b)));
        bn.fix.set_text(&next_step(b));
        bn.fix.set_tooltip_text(Some(&command_tip(b)));
        let mut note = Vec::new();
        if list.len() > 1 {
            let others: Vec<&str> = list[1..].iter().map(|x| x.project_name.as_str()).collect();
            note.push(format!("Also behind while building: {}.", others.join(", ")));
        }
        bn.note.set_text(&note.join(" "));
        bn.note.set_visible(!note.is_empty());
        bn.pull.set_visible(b.can_pull);
        bn.rev.set_visible(true);
        bn.rev.set_reveal_child(true);
    });
}

/// The person has seen it: quiet for the rest of this behind-episode.
pub fn ack(ui: &Ui, project: &str) {
    FORCED.with(|f| {
        if f.borrow().as_deref() == Some(project) {
            *f.borrow_mut() = None;
        }
    });
    if let Some(ov) = ui.st.borrow_mut().ov.as_mut()
        && let Some(b) = ov.behind.as_mut().and_then(|l| l.iter_mut().find(|b| b.project == project))
    {
        b.alarm = None;
    }
    let id = project.to_string();
    app::bg(ui, move |src| src.ack_behind(&id), |ui, _| app::refresh(ui));
    app::render_all(ui);
    app::focus_list(ui);
}

fn cant_pull(b: &Behind) -> String {
    format!("{}: {}", b.project_name, next_step(b))
}

/// What happened, in plain words: "Someone pushed 3 commits you don't have yet."
fn news(b: &Behind) -> String {
    match b.count {
        Some(n) => format!("Someone pushed {} you don't have yet.", fmt::pl(n, "commit", "commits")),
        None => "Someone pushed new commits you don't have yet.".to_string(),
    }
}

/// What to do about it, in plain words; `key` adds the key for Pull.
fn next_step(b: &Behind) -> String {
    if b.dirty > 0 {
        let (has, it) = if b.dirty == 1 { ("1 file has".to_string(), "it") } else { (format!("{} files have", b.dirty), "them") };
        format!("{has} changes that aren't committed. Commit {it} (or ask your agent to), then pull.")
    } else if b.ahead > 0 {
        format!(
            "You also have {} that {} on {}, so Wake can't pull for you. Ask your agent to pull and combine them.",
            fmt::pl(b.ahead, "commit", "commits"),
            if b.ahead == 1 { "isn't" } else { "aren't" },
            b.host
        )
    } else if b.can_pull {
        "Pull them before you start.".to_string()
    } else {
        "Wake can't pull this one for you. Ask your agent to pull them in.".to_string()
    }
}

/// The plain sentences together: what happened, then what to do.
fn plain(b: &Behind) -> String {
    format!("{} {}", news(b), next_step(b))
}

/// The git underneath, for a tooltip: the command Pull runs and where.
fn command_tip(b: &Behind) -> String {
    format!("{}\nPull runs {} in {}.", remote::how_far(b), b.fix, fmt::tildify(&b.repo))
}

/// "Pull <project>?" Then `git pull --ff-only`, and its last line of output.
pub fn confirm_pull(ui: &Ui, project: &str) {
    let Some(b) = of(ui, project) else { return };
    if !b.can_pull {
        app::toast(ui, &cant_pull(&b), true);
        return;
    }
    let what = match b.count {
        Some(n) => format!("{} from {}", fmt::pl(n, "commit", "commits"), b.upstream),
        None => format!("the new commits on {}", b.upstream),
    };
    let body = [
        format!("Brings in the new commits, in {}. If they clash with yours, nothing changes and Wake tells you.", fmt::tildify(&b.repo)),
        "Wake changes a repo only when you press Pull or Remove worktree.".to_string(),
    ];
    let id = project.to_string();
    let (name, folder) = (b.project_name.clone(), fmt::tildify(&b.repo));
    let commits = match b.count {
        Some(n) => fmt::pl(n, "new commit", "new commits"),
        None => "the new commits".to_string(),
    };
    dialogs::confirm(ui, &format!("Pull {}?", b.project_name), &body, &[(remote::PULL.to_string(), what)], "Pull", move |ui| {
        app::toast(ui, &format!("Pulling into {}…", name), false);
        let (id2, id) = (id.clone(), id.clone());
        let (name, folder, commits) = (name.clone(), folder.clone(), commits.clone());
        app::bg(
            ui,
            move |src| src.pull(&id2, false),
            move |ui, r| {
                // Which project and folder, then git's own last line ("Fast-forward").
                let said = if r.ok {
                    let rest = r.message.strip_prefix("Pulled.").unwrap_or(&r.message).trim();
                    format!("Pulled {commits} into {name}, in {folder}. {rest}").trim_end().to_string()
                } else {
                    format!("{name}: {}", r.message)
                };
                app::toast(ui, &said, !r.ok);
                app::refresh(ui);
                if ui.st.borrow().drawer == Some(Drawer::Project(id.clone())) {
                    crate::panel::load_project(ui, &id);
                }
            },
        );
    });
}

/// A small marker for a row: "3 to pull", red while building while behind. Tooltip: the fix.
pub fn pill(ui: &Ui, project: Option<&str>) -> Option<gtk::Label> {
    let b = of(ui, project?)?;
    let text = match b.count {
        Some(n) => format!("{n} to pull"),
        None => "to pull".to_string(),
    };
    let l = w::label(&text, &["behind-pill"]);
    if b.alarm.is_some() {
        l.add_css_class("alarm");
    }
    l.set_valign(gtk::Align::Center);
    l.set_hexpand(false);
    l.set_tooltip_text(Some(&format!("{}\n{}", plain(&b), command_tip(&b))));
    Some(l)
}

/// What is waiting, short enough for a project's row (in place of its description): "3 new
/// commits to pull". The whole sentence is its tooltip, and in the panel.
pub fn row_line(ui: &Ui, project: &str) -> Option<gtk::Label> {
    let b = of(ui, project)?;
    let short = match b.count {
        Some(n) => format!("{} to pull", fmt::pl(n, "new commit", "new commits")),
        None => "New commits to pull".to_string(),
    };
    let l = w::clip(&short, &["behind-line"]);
    if b.alarm.is_some() {
        l.add_css_class("alarm");
    }
    l.set_tooltip_text(Some(&format!("{}\n{}", plain(&b), command_tip(&b))));
    Some(l)
}

/// The block at the top of a project's panel: the line, when it was checked, and "Pull now".
pub fn panel_block(ui: &Ui, project: &str) -> Option<gtk::Box> {
    let b = of(ui, project)?;
    let bx = w::vbox(6, &["behind-block"]);
    if b.alarm.is_some() {
        bx.add_css_class("alarm");
    }
    let line = w::wrap(&format!("{} {}", news(&b), next_step(&b)), &["bk-line"]);
    line.set_tooltip_text(Some(&command_tip(&b)));
    bx.append(&line);
    let mut meta = match &b.checked_at {
        Some(at) => format!("Asked {} {}", b.host, fmt::ago(Some(at))),
        None => format!("From the last fetch; {} couldn't be asked just now", b.host),
    };
    if b.others > 0 {
        meta.push_str(&format!(" · {} more {} behind", b.others, if b.others == 1 { "repo" } else { "repos" }));
    }
    if b.others > 0 {
        meta.push_str(&format!(" · {}", fmt::tildify(&b.repo)));
    }
    bx.append(&w::wrap(&meta, &["meta"]));
    if b.can_pull {
        let ui2 = ui.clone();
        let id = project.to_string();
        let btn = w::button("Pull now", None, &["primary", "sm"], move || confirm_pull(&ui2, &id));
        btn.set_widget_name("pull-now");
        btn.set_halign(gtk::Align::Start);
        bx.append(&btn);
    }
    Some(bx)
}

/// `--show behind[:project]` raises a project's alarm on screen (the first one behind if none is
/// named); `--show behind-pull[:project]` opens its "Pull now" confirmation; `--show behind-wait`
/// only waits for the first remote check. On real data each waits up to 30 s for a project to be
/// behind. Never sends anything.
pub fn show(ui: &Ui, what: &str, arg: Option<String>) {
    show_try(ui, what.to_string(), arg, 30);
}

fn show_try(ui: &Ui, what: String, arg: Option<String>, left: u32) {
    let first = {
        let st = ui.st.borrow();
        st.ov.as_ref().and_then(|o| o.behind.as_ref()).and_then(|l| l.iter().find(|b| arg.as_deref().is_none_or(|a| b.project == a || b.project_name == a)).map(|b| b.project.clone()))
    };
    let Some(id) = first else {
        if left > 0 {
            let ui = ui.clone();
            glib::timeout_add_local_once(Duration::from_secs(1), move || {
                app::refresh(&ui);
                show_try(&ui, what, arg, left - 1);
            });
        } else {
            eprintln!("wake: nothing is behind{}", arg.map(|a| format!(" in {a}")).unwrap_or_default());
        }
        return;
    };
    if what == "behind-wait" {
        return;
    }
    if what == "behind-pull" {
        confirm_pull(ui, &id);
    } else {
        FORCED.with(|f| *f.borrow_mut() = Some(id));
        // Shown as if it had just appeared, past the Esc grace.
        app::render_all(ui);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(count: Option<u32>, dirty: u32, ahead: u32) -> Behind {
        Behind { project_name: "Gradebook".into(), host: "GitHub".into(), count, dirty, ahead, can_pull: dirty == 0 && ahead == 0, fix: remote::PULL.into(), ..Default::default() }
    }

    #[test]
    fn behind_reads_as_plain_sentences_with_git_only_in_the_tooltip() {
        assert_eq!(plain(&b(Some(3), 0, 0)), "Someone pushed 3 commits you don't have yet. Pull them before you start.");
        assert_eq!(next_step(&b(Some(1), 1, 0)), "1 file has changes that aren't committed. Commit it (or ask your agent to), then pull.");
        assert_eq!(next_step(&b(None, 2, 0)), "2 files have changes that aren't committed. Commit them (or ask your agent to), then pull.");
        assert_eq!(news(&b(None, 0, 0)), "Someone pushed new commits you don't have yet.");
        assert!(next_step(&b(Some(2), 0, 1)).starts_with("You also have 1 commit that isn't on GitHub"));
        for words in [plain(&b(Some(3), 0, 0)), plain(&b(Some(3), 1, 0)), plain(&b(Some(3), 0, 2)), cant_pull(&b(Some(3), 1, 0))] {
            assert!(!words.contains("git") && !words.contains("stash") && !words.contains("fast-forward"), "{words}");
        }
        assert!(command_tip(&b(Some(3), 0, 0)).contains("git pull --ff-only"));
    }
}
