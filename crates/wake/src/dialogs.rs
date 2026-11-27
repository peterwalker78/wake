//! In-window dialogs: pausing a project, and confirming before stopping an agent.

use std::rc::Rc;

use gtk4::{self as gtk, gdk, glib, prelude::*};
use wake_core::*;

use crate::{
    app::{self, Ui, View},
    w,
};

/// A dialog card: screen readers are told it is a dialog.
pub fn card(width: i32) -> gtk::Box {
    let card = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).accessible_role(gtk::AccessibleRole::Dialog).build();
    card.add_css_class("modal");
    card.set_size_request(width, -1);
    card
}

/// Puts a dialog up. Everything behind it is made insensitive, so Tab stays inside the dialog and
/// nothing behind it (a row's Stop button, say) can be reached or activated.
pub fn show(ui: &Ui, card: &gtk::Box, submit: Option<Rc<dyn Fn()>>) {
    let modal = &ui.w.modal;
    w::clear(modal);
    // The width the card asked for when there is room; in a narrow window, the window's width less
    // a margin each side (its text wraps), but never less than its controls need.
    let want = card.width_request();
    card.set_size_request(-1, -1);
    let fit = w::Fit::new(card, 0, (want > 0).then_some(want)).upcast();
    fit.set_vexpand(true);
    // A narrow window shows it as a phone does: a sheet up from the bottom, the window's width.
    let sheet = ui.st.borrow().lay.narrow;
    if sheet {
        card.add_css_class("sheet");
    }
    fit.set_halign(if sheet { gtk::Align::Fill } else { gtk::Align::Center });
    fit.set_valign(if sheet { gtk::Align::End } else { gtk::Align::Center });
    let m = if sheet { 8 } else { 16 };
    fit.set_margin_start(m);
    fit.set_margin_end(m);
    fit.set_margin_top(16);
    fit.set_margin_bottom(if sheet { 0 } else { 16 });
    // In a scroller: the window's overlay gives the scrim at least its minimum height, which for
    // a tall card (Keys) is the card's height at its narrowest, far taller than the window, and
    // the card was centred out of sight. Scrolled, a card taller than the window scrolls instead.
    let scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&fit).build();
    modal.append(&scroll);
    modal.set_visible(true);
    crate::motion::pop(modal);
    crate::motion::pop(card);
    let back = gtk::prelude::GtkWindowExt::focus(&ui.window);
    {
        let mut st = ui.st.borrow_mut();
        st.modal_open = true;
        st.modal_return = back;
        st.modal_submit = submit;
        st.modal_esc = None;
        st.modal_closed = None;
    }
    ui.w.column.set_sensitive(false);
}

pub fn close(ui: &Ui) {
    ui.w.modal.set_visible(false);
    w::clear(&ui.w.modal);
    ui.st.borrow_mut().modal_open = false;
    ui.w.column.set_sensitive(true);
    ui.st.borrow_mut().modal_submit = None;
    ui.st.borrow_mut().modal_esc = None;
    let on_closed = ui.st.borrow_mut().modal_closed.take();
    if let Some(f) = on_closed {
        f();
    }
    let back = ui.st.borrow_mut().modal_return.take();
    match back {
        Some(b) if b.is_mapped() => {
            b.grab_focus();
        }
        _ => app::focus_list(ui),
    }
}

pub fn on_key(ui: &Ui, key: gdk::Key, ctrl: bool) -> glib::Propagation {
    if key == gdk::Key::Escape {
        // A dialog may ask first (New project, with an answer typed).
        let esc = ui.st.borrow().modal_esc.clone();
        if !esc.is_some_and(|f| f()) {
            close(ui);
        }
        return glib::Propagation::Stop;
    }
    if ctrl && matches!(key, gdk::Key::Return | gdk::Key::KP_Enter) {
        let submit = ui.st.borrow().modal_submit.clone();
        if let Some(f) = submit {
            f();
        }
        return glib::Propagation::Stop;
    }
    glib::Propagation::Proceed
}

pub fn text_area(text: &str, min_height: i32) -> gtk::TextView {
    let t = gtk::TextView::new();
    t.set_wrap_mode(gtk::WrapMode::WordChar);
    t.buffer().set_text(text);
    t.set_size_request(-1, min_height);
    t.add_css_class("area");
    t.set_accepts_tab(false);
    t
}

/// A text area in a scrolling frame of a fixed height, so a long note scrolls instead of growing
/// the dialog.
pub fn framed(t: &gtk::TextView, height: i32) -> gtk::ScrolledWindow {
    t.set_size_request(-1, -1);
    let s = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).min_content_height(height).max_content_height(height).child(t).build();
    s.add_css_class("areaframe");
    s
}

fn buffer_text(t: &gtk::TextView) -> String {
    let b = t.buffer();
    b.text(&b.start_iter(), &b.end_iter(), false).trim().to_string()
}

/// "Pausing <project>": a note for future-you and the next step, drafted from where things stand.
pub fn pause(ui: &Ui, id: &str) {
    let Some(p) = app::find_project(ui, id) else { return };
    if p.removed {
        return;
    }
    let detail = ui.st.borrow().projects.get(id).cloned();
    let Some(d) = detail else {
        // Fetch what's needed to draft the note, then open.
        let id2 = id.to_string();
        app::bg(
            ui,
            move |src| src.project(&id2),
            |ui, d| {
                if let Some(d) = d {
                    let id = d.summary.id.clone();
                    ui.st.borrow_mut().projects.insert(id.clone(), d);
                    pause(ui, &id);
                }
            },
        );
        return;
    };
    let waiting = d.sessions.iter().find(|s| s.state == State::Waiting && s.question.is_some());
    let note = match waiting {
        Some(s) => format!("Open question: {}", s.question.as_deref().unwrap_or("")),
        None => d.last_pause.as_ref().map(|c| c.note.clone()).unwrap_or_default(),
    };
    let card = card(540);
    card.append(&w::label(&format!("Pausing {}", p.name), &["h2"]));
    let a = w::vbox(6, &[]);
    a.append(&w::label("WHAT SHOULD FUTURE-YOU KNOW?", &["flabel"]));
    let note_t = text_area(&note, 70);
    a.append(&framed(&note_t, 84));
    a.append(&w::label("Drafted from where things stand. Edit it or keep it.", &["hint"]));
    card.append(&a);
    let b = w::vbox(6, &[]);
    b.append(&w::label("NEXT TIME, START WITH", &["flabel"]));
    let next_t = text_area(&p.next, 46);
    b.append(&framed(&next_t, 52));
    card.append(&b);
    card.append(&w::wrap("Wake saves this with the branch and commit of each repo and the open sessions. Agents keep running; pausing never stops them.", &["hint"]));
    let row = w::hbox(8, &["row"]);
    let err = w::label("", &["err"]);
    err.set_hexpand(true);
    row.append(&err);
    let ui2 = ui.clone();
    row.append(&w::button("Cancel", None, &["ghost"], move || close(&ui2)));
    let submit: Rc<dyn Fn()> = {
        let ui = ui.clone();
        let id = id.to_string();
        let name = p.name.clone();
        let (note_t, next_t, err) = (note_t.clone(), next_t.clone(), err.clone());
        Rc::new(move || {
            let (note, next) = (buffer_text(&note_t), buffer_text(&next_t));
            let (id2, name, err) = (id.clone(), name.clone(), err.clone());
            app::bg(
                &ui,
                move |src| src.pause(&id2, &note, &next),
                move |ui, r| match r {
                    Ok(c) => {
                        close(ui);
                        app::toast(ui, &format!("Paused {name}. Your place is saved."), false);
                        crate::panel::load_project(ui, &c.project);
                        app::refresh(ui);
                    }
                    Err(e) => err.set_text(&e),
                },
            );
        })
    };
    let s2 = submit.clone();
    row.append(&w::button("Save pause", Some("Ctrl+Enter"), &["mint"], move || s2()));
    card.append(&row);
    show(ui, &card, Some(submit));
    note_t.grab_focus();
    let buf = note_t.buffer();
    buf.place_cursor(&buf.end_iter());
}

/// A confirmation: a title, what it will do, an optional list of `(what, small note on the right)`,
/// and the button that does it. The keyboard starts on Cancel, and there is no shortcut that
/// confirms: stopping something takes a deliberate Tab and Enter, or a click.
pub fn confirm(ui: &Ui, title: &str, body: &[String], list: &[(String, String)], label: &str, act: impl Fn(&Ui) + 'static) {
    let card = card(560);
    card.append(&w::wrap(title, &["h2"]));
    for line in body.iter().filter(|l| !l.is_empty()) {
        card.append(&w::wrap(line, &["plain"]));
    }
    if !list.is_empty() {
        let l = w::vbox(5, &["confirm-list"]);
        for (item, note) in list {
            let r = w::hbox(10, &[]);
            r.append(&w::wrap(item, &["mono"]));
            if !note.is_empty() {
                let n = w::label(note, &["meta"]);
                n.set_valign(gtk::Align::Start);
                r.append(&n);
            }
            l.append(&r);
        }
        card.append(&l);
    }
    let row = w::hbox(8, &["row"]);
    row.append(&w::spacer());
    let ui2 = ui.clone();
    let cancel = w::button("Cancel", Some("Esc"), &["ghost"], move || close(&ui2));
    row.append(&cancel);
    let ui2 = ui.clone();
    let go = w::button(label, None, &["danger-solid"], move || {
        close(&ui2);
        act(&ui2);
    });
    row.append(&go);
    card.append(&row);
    show(ui, &card, None);
    cancel.grab_focus();
}

/// `?`: every key, grouped, so the status bar only needs the few that matter where you are.
pub fn keys(ui: &Ui) {
    let c = card(760);
    let top = w::hbox(8, &[]);
    top.append(&w::label("Keys", &["h2"]));
    top.append(&w::spacer());
    top.append(&w::label("Esc closes", &["hint"]));
    c.append(&top);
    let peek = if crate::peek::can_fly(ui) { "Peek: zoom to the session's window, or preview it if no window shows it" } else { "Peek: preview the session" };
    let groups: [(&str, Vec<(&str, String)>); 2] = [
        (
            "Moving around",
            vec![
                ("Tab  Shift+Tab", "the next or previous area: the top, the list, the details".into()),
                ("↑ ↓ ← →", "move within the area you're in".into()),
                ("Home  End", "the first or last".into()),
                ("Enter", "open the details, or press the button".into()),
                ("Esc", "go back, or close".into()),
                ("hold Space", format!("{peek}; let go to come back. \"Peek available\" shows at the top right when you can")),
            ],
        ),
        (
            "Shortcuts",
            vec![
                ("Ctrl+F", "find a session".into()),
                ("Ctrl+N", "a new idea".into()),
                ("Ctrl+1 to 6", View::TABS.map(View::name).join(", ")),
                ("Ctrl+,", "Settings".into()),
                ("Ctrl+Tab", "the next view (Shift: back)".into()),
                ("Menu  Shift+F10", "the selected item's menu".into()),
                ("Delete", "in Agents: stop the agent (asks first)".into()),
                ("F1", "these keys".into()),
                ("Ctrl+Q", "quit".into()),
            ],
        ),
    ];
    // Two columns side by side.
    let grid = gtk::Grid::builder().column_spacing(36).row_spacing(16).build();
    for (i, (title, rows)) in groups.into_iter().enumerate() {
        let g = w::vbox(5, &["keygrp"]);
        g.set_valign(gtk::Align::Start);
        g.append(&w::sec(title));
        for (k, what) in rows {
            let r = w::hbox(12, &[]);
            let kl = w::label(k, &["keyname"]);
            kl.set_size_request(130, -1);
            kl.set_xalign(0.0);
            r.append(&kl);
            r.append(&w::wrap(&what, &["plain"]));
            g.append(&r);
        }
        grid.attach(&g, i as i32, 0, 1, 1);
    }
    c.append(&grid);
    let ui2 = ui.clone();
    let close_b = w::button("Close", Some("Esc"), &["ghost"], move || close(&ui2));
    close_b.set_halign(gtk::Align::End);
    c.append(&close_b);
    show(ui, &c, None);
    close_b.grab_focus();
}
