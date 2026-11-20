//! "Start an agent" on ideas (added 2026-11-20): a dialog showing the brief exactly as it will be
//! sent (editable), which agent, and where it opens; then `Source::start_agent` writes the brief,
//! opens a new Konsole window in the project's folder that runs the command, and brings that
//! window forward. The ideas move to "Sent to an agent".

use std::rc::Rc;

use gtk4::{self as gtk, prelude::*};
use wake_core::{Idea, brief};

use crate::{
    app::{self, IdeaTab, Ui, View},
    dialogs, fmt, views, w,
};

/// Which project the ideas are for, or why they can't go to one agent. Pure, for tests.
pub fn project_of(ideas: &[&Idea]) -> Result<(String, String), String> {
    let Some(first) = ideas.first() else {
        return Err("Tick an idea first (x), or select one.".into());
    };
    if ideas.iter().any(|i| i.project.is_none()) {
        return Err(if ideas.len() == 1 {
            "Pick a project for this idea first: Enter opens its project list.".into()
        } else {
            "Some of these ideas are Unsorted. Pick their project first (Enter on an idea).".into()
        });
    }
    if ideas.iter().any(|i| i.project != first.project) {
        return Err("These ideas are for different projects. Start one agent per project: tick only one project's ideas.".into());
    }
    let id = first.project.clone().unwrap_or_default();
    Ok((id.clone(), first.project_name.clone().unwrap_or(id)))
}

/// The ideas the action works on: `ids` when given (the project panel's), else the ticked ones,
/// else the selected one in To sort.
fn chosen(ui: &Ui, ids: Option<Vec<String>>) -> Vec<Idea> {
    let st = ui.st.borrow();
    // The Ideas list, then any project panel's (the panel may be newer than the list).
    let mut all = st.ideas.clone().unwrap_or_default();
    for d in st.projects.values() {
        all.extend(d.ideas.iter().filter(|i| !all.iter().any(|x| x.id == i.id)).cloned().collect::<Vec<_>>());
    }
    let ids = ids.unwrap_or_else(|| {
        if !st.picked.is_empty() {
            st.picked.clone()
        } else if st.view == View::Ideas && st.idea_tab == IdeaTab::Sort {
            st.sel.clone().into_iter().collect()
        } else {
            Vec::new()
        }
    });
    ids.iter().filter_map(|id| all.iter().find(|i| &i.id == id && i.state == "inbox").cloned()).collect()
}

pub fn open(ui: &Ui, ids: Option<Vec<String>>) {
    let ideas = chosen(ui, ids);
    let refs: Vec<&Idea> = ideas.iter().collect();
    let (project, name) = match project_of(&refs) {
        Ok(p) => p,
        Err(e) => {
            app::toast(ui, &e, true);
            return;
        }
    };
    let root = app::find_project(ui, &project).and_then(|p| p.roots.first().cloned()).map(|r| fmt::tildify(&r)).unwrap_or_else(|| "its folder".into());
    let texts: Vec<&str> = ideas.iter().map(|i| i.text.as_str()).collect();
    let text = brief::brief_text(&name, &texts);
    let id = brief::new_id();
    let choices = ui.src.agent_choices();

    let card = dialogs::card(620);
    card.update_property(&[gtk::accessible::Property::Label(&format!("Start an agent on {name}"))]);
    card.append(&w::wrap(&format!("Start an agent on {name}"), &["h2"]));
    card.append(&w::wrap(&format!("A new Konsole window opens in {root} and starts the agent with this brief."), &["plain"]));
    let a = w::vbox(6, &[]);
    a.append(&w::label("THE BRIEF, AS IT WILL BE SENT", &["flabel"]));
    let area = dialogs::text_area(&text, 120);
    area.update_property(&[gtk::accessible::Property::Label("The brief")]);
    a.append(&dialogs::framed(&area, 150));
    a.append(&w::label("Edit it if you like. Wake saves it in its own folder and hands the file to the agent.", &["hint"]));
    card.append(&a);

    let agent_dd = gtk::DropDown::from_strings(&choices.iter().map(|c| c.label()).collect::<Vec<_>>());
    agent_dd.add_css_class("mini");
    agent_dd.update_property(&[gtk::accessible::Property::Label("Agent")]);
    let cmd_l = w::wrap("", &["mono", "typed"]);
    cmd_l.set_selectable(true);
    let b = w::hbox(14, &["agentrow"]);
    let left = w::vbox(6, &[]);
    left.append(&w::label("AGENT", &["flabel"]));
    if choices.len() > 1 {
        agent_dd.set_halign(gtk::Align::Start);
        left.append(&agent_dd);
    } else {
        left.append(&w::label(choices.first().map_or("Claude", |c| c.label()), &["plain"]));
    }
    b.append(&left);
    let right = w::vbox(6, &[]);
    right.set_hexpand(true);
    right.append(&w::label("RUN IN THE NEW WINDOW", &["flabel"]));
    right.append(&cmd_l);
    b.append(&right);
    card.append(&b);
    let show_cmd = {
        let (ui, id, cmd_l, choices) = (ui.clone(), id.clone(), cmd_l.clone(), choices.clone());
        move |at: u32| {
            let agent = choices.get(at as usize).copied().unwrap_or(brief::Agent::Claude);
            cmd_l.set_text(&ui.src.brief_command(agent, &id).unwrap_or_else(|| format!("{} \"$(cat <the brief>)\"", agent.id())));
        }
    };
    show_cmd(0);
    agent_dd.connect_selected_notify(move |dd| show_cmd(dd.selected()));

    card.append(&w::wrap(if ideas.len() == 1 { "The idea then moves to Sent to an agent." } else { "The ideas then move to Sent to an agent." }, &["hint"]));
    let row = w::hbox(8, &["row"]);
    let err = w::wrap("", &["err"]);
    err.set_hexpand(true);
    row.append(&err);
    let ui2 = ui.clone();
    row.append(&w::button("Cancel", Some("Esc"), &["ghost"], move || dialogs::close(&ui2)));
    let busy = Rc::new(std::cell::Cell::new(false));
    let submit: Rc<dyn Fn()> = {
        let ui = ui.clone();
        let ids: Vec<String> = ideas.iter().map(|i| i.id.clone()).collect();
        let (area, agent_dd, err, busy) = (area.clone(), agent_dd.clone(), err.clone(), busy.clone());
        Rc::new(move || {
            if busy.get() {
                return;
            }
            let b = area.buffer();
            let text = b.text(&b.start_iter(), &b.end_iter(), false).trim().to_string();
            if text.is_empty() {
                err.set_text("The brief is empty.");
                return;
            }
            busy.set(true);
            let agent = choices.get(agent_dd.selected() as usize).copied().unwrap_or(brief::Agent::Claude);
            let req = brief::StartAgent { project: project.clone(), ideas: ids.clone(), brief: text, agent: agent.id().into(), id: id.clone() };
            let (err, busy, ids, name) = (err.clone(), busy.clone(), ids.clone(), name.clone());
            err.set_text("Opening the tab…");
            app::bg(
                &ui,
                move |src| src.start_agent(&req, false),
                move |ui, r| {
                    busy.set(false);
                    if !r.ok {
                        err.set_text(&r.message);
                        return;
                    }
                    dialogs::close(ui);
                    // Which project it is for: the message says the agent and the folder.
                    app::toast(ui, &format!("{name}: {}", r.message), false);
                    {
                        let mut st = ui.st.borrow_mut();
                        st.picked.retain(|p| !ids.contains(p));
                        if st.picked.is_empty() {
                            st.show_brief = false;
                        }
                    }
                    app::refresh_ideas(ui);
                    app::refresh(ui);
                    views::render_chips(ui);
                },
            );
        })
    };
    let s2 = submit.clone();
    let go = w::button("Open the tab", Some("Ctrl+Enter"), &["mint"], move || s2());
    row.append(&go);
    card.append(&row);
    dialogs::show(ui, &card, Some(submit));
    go.grab_focus();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idea(id: &str, project: Option<&str>) -> Idea {
        Idea {
            id: id.into(),
            at: String::new(),
            text: format!("idea {id}"),
            project: project.map(Into::into),
            project_name: project.map(|p| format!("{p} name")),
            source: "typed".into(),
            state: "inbox".into(),
            sent_at: None,
        }
    }

    #[test]
    fn one_agent_per_project_and_unsorted_ideas_need_a_project_first() {
        let (a, b, c, u) = (idea("a", Some("wake")), idea("b", Some("wake")), idea("c", Some("gradebook")), idea("u", None));
        assert_eq!(project_of(&[&a, &b]), Ok(("wake".into(), "wake name".into())));
        assert!(project_of(&[&a, &c]).unwrap_err().contains("different projects"));
        assert!(project_of(&[&u]).unwrap_err().contains("Pick a project"));
        assert!(project_of(&[&a, &u]).unwrap_err().contains("Unsorted"));
        assert!(project_of(&[]).is_err());
    }
}
