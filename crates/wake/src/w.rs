//! Small widget helpers so the views read as layout, not boilerplate.

use gtk4::{self as gtk, glib, prelude::*};

pub fn esc(s: &str) -> String {
    glib::markup_escape_text(s).to_string()
}

/// A left-aligned label with these CSS classes.
pub fn label(text: &str, classes: &[&str]) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.set_xalign(0.0);
    for c in classes {
        l.add_css_class(c);
    }
    l
}

/// A label from Pango markup (callers escape what they put in).
pub fn markup(m: &str, classes: &[&str]) -> gtk::Label {
    let l = label("", classes);
    l.set_markup(m);
    l
}

/// A one-line label that shrinks with an ellipsis instead of pushing its column wider.
pub fn clip(text: &str, classes: &[&str]) -> gtk::Label {
    let l = label(text, classes);
    l.set_ellipsize(gtk::pango::EllipsizeMode::End);
    l.set_max_width_chars(1);
    l.set_hexpand(true);
    l.set_single_line_mode(true);
    l.set_tooltip_text(Some(text));
    l
}

/// A label that wraps.
pub fn wrap(text: &str, classes: &[&str]) -> gtk::Label {
    let l = label(text, classes);
    l.set_wrap(true);
    l.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    // Natural width no wider than the minimum: a wrapping label never widens its column.
    l.set_natural_wrap_mode(gtk::NaturalWrapMode::Inherit);
    l.set_max_width_chars(1);
    l.set_hexpand(true);
    l
}

pub fn hbox(spacing: i32, classes: &[&str]) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, spacing);
    for c in classes {
        b.add_css_class(c);
    }
    b
}

pub fn vbox(spacing: i32, classes: &[&str]) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Vertical, spacing);
    for c in classes {
        b.add_css_class(c);
    }
    b
}

pub fn spacer() -> gtk::Box {
    let b = hbox(0, &[]);
    b.set_hexpand(true);
    b
}

/// A key cap.
pub fn kbd(text: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.add_css_class("kbd");
    l.set_valign(gtk::Align::Center);
    l
}

/// A button whose child is `text` and, if given, a key cap.
pub fn button(text: &str, key: Option<&str>, classes: &[&str], on_click: impl Fn() + 'static) -> gtk::Button {
    let b = gtk::Button::new();
    let inner = hbox(7, &[]);
    inner.append(&gtk::Label::new(Some(text)));
    if let Some(k) = key {
        for part in k.split('+') {
            inner.append(&kbd(part));
        }
    }
    b.set_child(Some(&inner));
    b.add_css_class("btn");
    for c in classes {
        b.add_css_class(c);
    }
    b.connect_clicked(move |_| on_click());
    b
}

pub fn clear(b: &impl IsA<gtk::Widget>) {
    let w = b.upcast_ref::<gtk::Widget>();
    while let Some(child) = w.first_child() {
        if let Some(bx) = w.downcast_ref::<gtk::Box>() {
            bx.remove(&child);
        } else if let Some(lb) = w.downcast_ref::<gtk::ListBox>() {
            lb.remove(&child);
        } else {
            child.unparent();
        }
    }
}

/// A table column: `child` at `width` when there is room (growing past it when `grow`), down to
/// about half of it when the window is narrow, so the table never sets the window's minimum size.
///
/// Every cell of a column reports the same sizes whatever it holds, so the header and every row
/// share out the width identically and the columns stay lined up.
pub fn col(child: &impl IsA<gtk::Widget>, width: i32, grow: bool) -> gtk::Widget {
    let b = hbox(0, &["cell"]);
    child.set_valign(gtk::Align::Center);
    // The child fills the cell; text in it is expected to ellipsize or wrap rather than widen it.
    child.set_hexpand(true);
    shrinkable(child.upcast_ref());
    b.append(child);
    // The cell's 12 px padding each side is part of its size.
    let (width, min) = if width & FIXED != 0 { (width & !FIXED, width & !FIXED) } else { (width, col_min(width)) };
    let f = Fit::new(&b, min + 24, Some(width + 24));
    f.set_hexpand(grow);
    f.upcast()
}

/// Lets every one-line label in `w` shorten with an ellipsis, so a cell's own minimum is small and
/// its column can narrow without anything being cut off (wrapping labels already narrow).
pub fn shrinkable(w: &gtk::Widget) {
    if let Some(l) = w.downcast_ref::<gtk::Label>() {
        // Column titles stay whole: the table switches to fewer columns, or to two-line rows,
        // before they would have to shorten.
        if !l.wraps() && l.ellipsize() == gtk::pango::EllipsizeMode::None && !l.has_css_class("th") {
            l.set_ellipsize(gtk::pango::EllipsizeMode::End);
        }
        return;
    }
    let mut c = w.first_child();
    while let Some(x) = c {
        shrinkable(&x);
        c = x.next_sibling();
    }
}

/// On a column's width: the column can't narrow (it holds a drawing of a fixed size).
pub const FIXED: i32 = 1 << 20;

/// A column's width without the `FIXED` mark.
pub fn col_width(width: i32) -> i32 {
    width & !FIXED
}

/// How narrow a column may get: small columns (numbers, a button) keep their width.
pub fn col_min(width: i32) -> i32 {
    if width <= 90 { width } else { width * 45 / 100 }
}

/// A one-child wrapper that sizes itself by numbers rather than by its child's natural width: it
/// asks for `min` (or the child's own minimum, if that is more: nothing is ever cut off) and `nat`
/// (or the child's natural width). Used for table cells (so rows line up and the table can shrink:
/// their contents ellipsize, so their own minimum is small) and for columns with a most-wanted
/// width (Settings, Questions, dialogs).
#[derive(Clone)]
pub struct Fit(gtk::Widget);

impl Fit {
    pub fn new(child: &impl IsA<gtk::Widget>, min: i32, nat: Option<i32>) -> Fit {
        let f: fit::FitWidget = glib::Object::new();
        f.imp_set(min, nat.unwrap_or(-1));
        child.set_parent(&f);
        f.set_overflow(gtk::Overflow::Hidden);
        Fit(f.upcast())
    }
    pub fn upcast(self) -> gtk::Widget {
        self.0
    }
    pub fn set_hexpand(&self, e: bool) {
        self.0.set_hexpand(e);
    }
}

mod fit {
    use gtk4::{self as gtk, glib, prelude::*, subclass::prelude::*};
    use std::cell::Cell;

    glib::wrapper! {
        pub struct FitWidget(ObjectSubclass<Imp>) @extends gtk::Widget, @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
    }

    impl FitWidget {
        pub fn imp_set(&self, min: i32, nat: i32) {
            self.imp().min.set(min);
            self.imp().nat.set(nat);
        }
    }

    #[derive(Default)]
    pub struct Imp {
        min: Cell<i32>,
        nat: Cell<i32>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Imp {
        const NAME: &'static str = "WakeFit";
        type Type = FitWidget;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for Imp {
        fn dispose(&self) {
            while let Some(c) = self.obj().first_child() {
                c.unparent();
            }
        }
    }

    impl WidgetImpl for Imp {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            self.obj().first_child().map_or(gtk::SizeRequestMode::ConstantSize, |c| c.request_mode())
        }

        fn measure(&self, o: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let Some(c) = self.obj().first_child() else { return (0, 0, -1, -1) };
            if o == gtk::Orientation::Horizontal {
                let (_, cnat, _, _) = c.measure(o, for_size);
                // Its minimum for any height (a wrapping label measured for a short height asks
                // for a long line; laid out height-for-width, it never gets one).
                let cmin = c.measure(o, -1).0;
                // Never less than the child truly needs: what doesn't fit would be cut off.
                let min = self.min.get().max(cmin).max(0);
                if cmin > self.min.get() && std::env::var_os("WAKE_FIT_DEBUG").is_some() {
                    eprintln!("fit: wants {} but needs {cmin}: {}", self.min.get(), crate::layout::describe(&c));
                }
                let nat = if self.nat.get() >= 0 { self.nat.get() } else { cnat };
                return (min, nat.max(min), -1, -1);
            }
            let cmin_w = c.measure(gtk::Orientation::Horizontal, -1).0;
            // Not filling its slot (centred, say), GTK gives this no more than its natural width,
            // so measure the height at that width: measured wider, wrapped text comes out too
            // short and the bottom is cut off.
            let for_size = if for_size >= 0 && self.nat.get() >= 0 && self.obj().halign() != gtk::Align::Fill { for_size.min(self.nat.get().max(self.min.get())) } else { for_size };
            let w = if for_size < 0 { -1 } else { for_size.max(cmin_w) };
            let (min, nat, _, _) = c.measure(o, w);
            (min, nat, -1, -1)
        }

        fn size_allocate(&self, width: i32, height: i32, _baseline: i32) {
            let Some(c) = self.obj().first_child() else { return };
            let cmin_w = c.measure(gtk::Orientation::Horizontal, -1).0;
            let w = width.max(cmin_w);
            let cmin_h = c.measure(gtk::Orientation::Vertical, w).0;
            c.allocate(w, height.max(cmin_h), -1, None);
        }
    }
}

/// A table row from columns `(widget, width, grow)`.
pub fn row(cells: Vec<(gtk::Widget, i32, bool)>, classes: &[&str]) -> gtk::Box {
    let b = hbox(0, classes);
    for (w, width, grow) in cells {
        b.append(&col(&w, width, grow));
    }
    b
}

/// The coloured dot and word for a session's state.
pub fn state_word(s: wake_core::State) -> (&'static str, &'static str) {
    use wake_core::State::*;
    match s {
        Working => ("working", "working"),
        Waiting => ("waiting", "waiting for you"),
        Idle => ("idle", "idle"),
        Ended => ("ended", "ended"),
        Unknown => ("unknown", "lost track"),
    }
}

pub fn dot(class: &str) -> gtk::Box {
    let d = hbox(0, &["d", class]);
    d.set_size_request(8, 8);
    d.set_valign(gtk::Align::Center);
    d.set_halign(gtk::Align::Center);
    d
}

/// "● working" with its class.
pub fn state_badge(s: wake_core::State) -> gtk::Box {
    let (class, word) = state_word(s);
    let b = hbox(7, &["state", class]);
    b.append(&dot(class));
    let l = clip(word, &[]);
    l.set_tooltip_text(None);
    b.append(&l);
    b
}

pub fn empty(big: &str, small: &str) -> gtk::Box {
    let b = vbox(6, &["empty"]);
    b.set_halign(gtk::Align::Center);
    b.append(&label(big, &["big"]));
    if !small.is_empty() {
        let s = wrap(small, &[]);
        s.set_justify(gtk::Justification::Center);
        s.set_xalign(0.5);
        b.append(&s);
    }
    b
}

pub fn sec(title: &str) -> gtk::Box {
    let b = hbox(8, &["sec"]);
    b.append(&label(&title.to_uppercase(), &[]));
    b
}

/// Keep a scrolled list where the reader left it while its rows are rebuilt.
///
/// Rebuilding empties the list first, so the adjustment's range shrinks and GTK clamps the position
/// to the top. This waits for the new rows to be laid out (the range grows again) and puts the
/// position back, then lets go: after half a second, or as soon as it is back.
pub fn keep_scroll(adj: &gtk::Adjustment, target: f64) {
    use std::cell::RefCell;
    use std::rc::Rc;
    if target <= 0.0 {
        return;
    }
    let id: Rc<RefCell<Option<gtk::glib::SignalHandlerId>>> = Rc::new(RefCell::new(None));
    let put = move |a: &gtk::Adjustment| {
        let max = (a.upper() - a.page_size()).max(0.0);
        a.set_value(target.min(max));
        max >= target
    };
    let id2 = id.clone();
    let handler = adj.connect_changed(move |a| {
        if put(a)
            && let Some(h) = id2.borrow_mut().take()
        {
            a.disconnect(h);
        }
    });
    *id.borrow_mut() = Some(handler);
    let adj2 = adj.clone();
    gtk::glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || {
        if let Some(h) = id.borrow_mut().take() {
            adj2.disconnect(h);
            let max = (adj2.upper() - adj2.page_size()).max(0.0);
            adj2.set_value(target.min(max));
        }
    });
}

/// For screenshots only: a box whose CSS node is named `name` (such as `popover` or `contents`),
/// so a drop-down's open list can be drawn inside the window with the stylesheet's own rules
/// (Broadway, with no monitor, never shows a real popover).
pub fn named_box(name: &str) -> gtk::Widget {
    let b: named::Named = glib::Object::builder().property("css-name", name).build();
    b.set_layout_manager(Some(gtk::BoxLayout::new(gtk::Orientation::Vertical)));
    b.upcast()
}

mod named {
    use gtk4::{self as gtk, glib, prelude::*, subclass::prelude::*};

    glib::wrapper! {
        pub struct Named(ObjectSubclass<Imp>) @extends gtk::Widget, @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
    }

    #[derive(Default)]
    pub struct Imp;

    #[glib::object_subclass]
    impl ObjectSubclass for Imp {
        const NAME: &'static str = "WakeNamed";
        type Type = Named;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for Imp {
        fn dispose(&self) {
            while let Some(c) = self.obj().first_child() {
                c.unparent();
            }
        }
    }

    impl WidgetImpl for Imp {}
}
