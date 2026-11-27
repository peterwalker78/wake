//! Wake's look: Slipstream's fonts handed to Pango from their files, the stylesheet, and its two
//! palettes. style.css only ever names its colours (`@bg`, `@ink`, `@amber`, ...); this file
//! defines them, dark or light, in a second provider that is swapped when Settings changes the
//! theme or the accent (or, on "Follow the system", when the desktop's preference changes).

use std::cell::{Cell, RefCell};
use std::path::PathBuf;

use gtk4::{self as gtk, gdk, gio, glib, pango::prelude::*, prelude::*};

const FONTS: [&str; 5] = ["AtkinsonHyperlegible-Regular.ttf", "AtkinsonHyperlegible-Bold.ttf", "ChakraPetch-Bold.ttf", "JetBrainsMono-Regular.ttf", "JetBrainsMono-Bold.ttf"];

/// The dark set: Slipstream's console colours (what Wake always looked like).
const DARK: &[(&str, &str)] = &[
    ("bg", "#0A0D12"),
    ("panel", "#0E131A"),
    ("panel2", "#111822"),
    ("row", "#0F151D"),
    ("rowhi", "#15202B"),
    ("rowfocus", "#121A23"),
    ("float", "#0C1117"),
    ("codebg", "#0E141C"),
    ("line", "#1E2732"),
    ("line2", "#28333F"),
    ("line3", "#34404E"),
    ("ink", "#E7ECF3"),
    ("soft", "#C9D3DF"),
    ("muted", "#8995A6"),
    ("dim", "#808B9C"),
    ("faint", "#5C6676"),
    ("cyan", "#38CEFF"),
    ("mint", "#42E6A4"),
    ("amber", "#FFB547"),
    ("rose", "#FF7A8A"),
    ("violet", "#B79CFF"),
    ("err", "#FF9AA6"),
    ("on_mint", "#042016"),
    ("on_violet", "#160A33"),
    ("on_rose", "#2A0409"),
    ("scrim", "rgba(4,6,9,.62)"),
    ("shadow", "#000000"),
    ("hl", "#1A3A4A"),
    ("codespan", "#18212C"),
    ("wavebg", "transparent"),
    ("knob", "#E7ECF3"),
];

/// The light set: the same roles on a pale ground. Every text colour is at least 4.5:1 on every
/// background here (checked on bg, panel, panel2, row, rowhi and float).
const LIGHT: &[(&str, &str)] = &[
    ("bg", "#F6F8FB"),
    ("panel", "#EDF1F6"),
    ("panel2", "#E4E9F0"),
    ("row", "#E9EEF4"),
    ("rowhi", "#DCE6F2"),
    ("rowfocus", "#E3EAF3"),
    ("float", "#FFFFFF"),
    ("codebg", "#E9EEF4"),
    ("line", "#D6DDE6"),
    ("line2", "#C2CBD6"),
    ("line3", "#AEB8C5"),
    ("ink", "#0F141B"),
    ("soft", "#1E2731"),
    ("muted", "#475262"),
    ("dim", "#556070"),
    ("faint", "#8A95A4"),
    ("cyan", "#005F88"),
    ("mint", "#076A42"),
    ("amber", "#8A5200"),
    ("rose", "#B3263C"),
    ("violet", "#6B47C9"),
    ("err", "#B01E35"),
    ("on_mint", "#FFFFFF"),
    ("on_violet", "#FFFFFF"),
    ("on_rose", "#FFFFFF"),
    ("scrim", "rgba(20,28,40,.28)"),
    ("shadow", "rgba(20,28,40,.35)"),
    ("hl", "#FFE7A8"),
    ("codespan", "#E1E7EF"),
    ("wavebg", "#0B0E13"),
    ("knob", "#FFFFFF"),
];

/// Accent presets: (name, dark, light). The accent is focus, selection, links and the main button.
pub const ACCENTS: [(&str, &str, &str); 4] = [("cyan", "#38CEFF", "#005F88"), ("mint", "#42E6A4", "#076A42"), ("violet", "#B79CFF", "#6B47C9"), ("blue", "#6EA8FF", "#2B5FC7")];

/// Project dot colours: (name, dark, light), in `store::COLOUR_NAMES` order. None of them amber,
/// which means "waiting for you": orange is a deep red-orange, kept clear of amber in both themes.
pub const PROJECT_COLOURS: [(&str, &str, &str); 14] = [
    ("red", "#FF796D", "#C00515"),
    ("orange", "#CB6705", "#BD6006"),
    ("wine", "#D05A6E", "#8C1737"),
    ("pink", "#FFA9D2", "#C3498B"),
    ("magenta", "#C845C7", "#7F0280"),
    ("purple", "#D7B6FF", "#955DCD"),
    ("indigo", "#6178F7", "#2F37B2"),
    ("blue", "#5AADFE", "#0168B4"),
    ("sky", "#0ABADE", "#077288"),
    ("teal", "#099B8F", "#005A52"),
    ("mint", "#10EB9C", "#05794F"),
    ("green", "#06A141", "#035E23"),
    ("lime", "#B2DA09", "#708904"),
    ("slate", "#9CA9B7", "#5C6774"),
];

thread_local! {
    static PALETTE: RefCell<Option<gtk::CssProvider>> = const { RefCell::new(None) };
    static LIGHT_NOW: Cell<bool> = const { Cell::new(false) };
    static WATCHING: Cell<bool> = const { Cell::new(false) };
}

/// Where Slipstream's fonts are: installed under the data folder or beside Slipstream's programs.
/// Without them the stylesheet's fallbacks apply.
fn fonts_dir() -> Option<PathBuf> {
    let data = wake_core::store::Paths::from_env().data_home;
    [data.join("slipstream/fonts"), PathBuf::from("/usr/local/share/slipstream/fonts"), PathBuf::from("/usr/share/slipstream/fonts")].into_iter().find(|d| d.join(FONTS[0]).is_file())
}

fn fonts() {
    let Some(dir) = fonts_dir() else { return };
    let Some(map) = gtk::Label::new(None).pango_context().font_map() else { return };
    for name in FONTS {
        let path = dir.join(name);
        if let Err(e) = map.add_font_file(&path) {
            eprintln!("wake: can't load {}: {e}", path.display());
        }
    }
}

/// The palette's CSS: every named colour the stylesheet uses, for `light` or dark, with `accent`.
pub fn palette_css(light: bool, accent: &str) -> String {
    let set = if light { LIGHT } else { DARK };
    let mut out = String::new();
    for (k, v) in set {
        out.push_str(&format!("@define-color {k} {v};\n"));
    }
    let (_, ad, al) = ACCENTS.iter().find(|a| a.0 == accent).unwrap_or(&ACCENTS[0]);
    out.push_str(&format!("@define-color accent {};\n", if light { al } else { ad }));
    // Text on the accent: dark on the bright dark-theme accents, white on the deep light ones.
    out.push_str(&format!("@define-color on_accent {};\n", if light { "#FFFFFF" } else { "#04131A" }));
    for (name, d, l) in ACCENTS {
        out.push_str(&format!("@define-color acc_{name} {};\n", if light { l } else { d }));
    }
    for (name, d, l) in PROJECT_COLOURS {
        out.push_str(&format!("@define-color pc_{name} {};\n", if light { l } else { d }));
    }
    out
}

/// A colour by its stylesheet name, for Pango markup (which can't use CSS names), in the theme now
/// showing.
pub fn hex(name: &str) -> &'static str {
    let set = if LIGHT_NOW.with(Cell::get) { LIGHT } else { DARK };
    set.iter().find(|(k, _)| *k == name).map_or("#808B9C", |(_, v)| v)
}

/// "#RRGGBB" as Cairo's 0-1 channels (for drawn lines).
pub fn rgb(hex: &str) -> (f64, f64, f64) {
    let ch = |i: usize| u8::from_str_radix(hex.get(i..i + 2).unwrap_or("80"), 16).map_or(0.5, |v| f64::from(v) / 255.0);
    (ch(1), ch(3), ch(5))
}

/// The accent's colour now, for Pango markup.
pub fn accent_hex() -> &'static str {
    let light = LIGHT_NOW.with(Cell::get);
    let a = crate::prefs::get().accent;
    let (_, d, l) = ACCENTS.iter().find(|x| x.0 == a).unwrap_or(&ACCENTS[0]);
    if light { l } else { d }
}

pub fn is_light() -> bool {
    LIGHT_NOW.with(Cell::get)
}

thread_local! {
    /// The desktop's preference as last read from the portal: Some(true) light, Some(false) dark.
    static SYSTEM_LIGHT: Cell<Option<Option<bool>>> = const { Cell::new(None) };
}

const PORTAL: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const PORTAL_SETTINGS: &str = "org.freedesktop.portal.Settings";

/// The portal's `color-scheme`: 1 prefers dark, 2 prefers light, 0 no preference.
fn scheme_light(v: &glib::Variant) -> Option<bool> {
    // ReadOne answers (v); SettingChanged carries the value itself. Unwrap variants until a u32.
    let mut v = v.clone();
    loop {
        if let Some(n) = v.get::<u32>() {
            return match n {
                1 => Some(false),
                2 => Some(true),
                _ => None,
            };
        }
        if v.is_container() && v.n_children() == 1 {
            v = v.child_value(0);
        } else {
            v = v.as_variant()?;
        }
    }
}

/// The desktop's light/dark preference, read from the settings portal, never from GTK's own
/// colour-scheme setting: Wake sets that one itself, so reading it back would only echo Wake's
/// own choice (and following the echo looped forever).
fn system_prefers_light() -> Option<bool> {
    if let Some(v) = SYSTEM_LIGHT.with(Cell::get) {
        return v;
    }
    let v = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).ok().and_then(|bus| {
        let args = ("org.freedesktop.appearance", "color-scheme").to_variant();
        bus.call_sync(Some(PORTAL), PORTAL_PATH, PORTAL_SETTINGS, "ReadOne", Some(&args), None, gio::DBusCallFlags::NONE, 700, gio::Cancellable::NONE).ok().and_then(|r| scheme_light(&r))
    });
    SYSTEM_LIGHT.with(|c| c.set(Some(v)));
    v
}

/// Whether `theme` ("dark", "light", "system") shows light now. Following the system with no
/// preference to follow, Wake stays dark, as it always was.
pub fn wants_light(theme: &str) -> bool {
    match theme {
        "light" => true,
        "system" => system_prefers_light().unwrap_or(false),
        _ => false,
    }
}

pub fn load() {
    fonts();
    let Some(display) = gdk::Display::default() else { return };
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("style.css"));
    gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    let p = crate::prefs::get();
    apply(&p.theme, &p.accent);
}

/// Shows `theme` with `accent`. True if light or dark changed (markup colours need a redraw then).
pub fn apply(theme: &str, accent: &str) -> bool {
    let Some(display) = gdk::Display::default() else { return false };
    let light = wants_light(theme);
    let changed = LIGHT_NOW.with(|l| l.replace(light)) != light;
    let settings = gtk::Settings::for_display(&display);
    if settings.is_gtk_application_prefer_dark_theme() != !light {
        settings.set_gtk_application_prefer_dark_theme(!light);
    }
    let css = palette_css(light, accent);
    PALETTE.with(|cell| {
        let mut cell = cell.borrow_mut();
        let provider = cell.get_or_insert_with(|| {
            let p = gtk::CssProvider::new();
            gtk::style_context_add_provider_for_display(&display, &p, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
            p
        });
        provider.load_from_string(&css);
    });
    changed
}

/// Calls `on_change` whenever the desktop's light/dark preference changes (set up once), from the
/// portal's SettingChanged signal. Only a real change of the preference calls it.
pub fn watch_system(on_change: impl Fn() + 'static) {
    if WATCHING.with(|w| w.replace(true)) {
        return;
    }
    let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else { return };
    // The subscription lives as long as the app; Wake never unwatches.
    #[allow(deprecated)]
    let _ = bus.signal_subscribe(
        Some(PORTAL),
        Some(PORTAL_SETTINGS),
        Some("SettingChanged"),
        Some(PORTAL_PATH),
        Some("org.freedesktop.appearance"),
        gio::DBusSignalFlags::NONE,
        move |_, _, _, _, _, params| {
            if params.n_children() < 3 || params.child_value(1).get::<String>().as_deref() != Some("color-scheme") {
                return;
            }
            let v = scheme_light(&params.child_value(2));
            if SYSTEM_LIGHT.with(|c| c.replace(Some(v))) != Some(v) {
                on_change();
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lum(hex: &str) -> f64 {
        let h = hex.trim_start_matches('#');
        let c = |i: usize| {
            let v = f64::from(u8::from_str_radix(&h[i..i + 2], 16).unwrap()) / 255.0;
            if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * c(0) + 0.7152 * c(2) + 0.0722 * c(4)
    }

    fn contrast(a: &str, b: &str) -> f64 {
        let (x, y) = (lum(a), lum(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    fn get<'a>(set: &'a [(&str, &str)], k: &str) -> &'a str {
        set.iter().find(|(n, _)| *n == k).unwrap().1
    }

    #[test]
    fn the_portal_scheme_reads_through_its_variant_wrapping() {
        let inner = 2u32.to_variant();
        assert_eq!(scheme_light(&glib::Variant::from_variant(&glib::Variant::from_variant(&inner))), Some(true));
        assert_eq!(scheme_light(&(glib::Variant::from_variant(&1u32.to_variant()),).to_variant()), Some(false));
        assert_eq!(scheme_light(&0u32.to_variant()), None);
    }

    #[test]
    fn text_is_readable_on_every_background_in_both_themes() {
        for (set, name) in [(DARK, "dark"), (LIGHT, "light")] {
            for bg in ["bg", "panel", "panel2", "row", "rowhi", "float"] {
                for fg in ["ink", "soft", "muted", "dim", "cyan", "mint", "amber", "rose", "violet", "err"] {
                    let c = contrast(get(set, fg), get(set, bg));
                    assert!(c >= 4.5, "{name}: {fg} on {bg} is {c:.2}:1");
                }
            }
        }
        for (_, d, l) in ACCENTS {
            for bg in ["bg", "panel", "panel2", "rowhi"] {
                assert!(contrast(d, get(DARK, bg)) >= 4.5 && contrast(l, get(LIGHT, bg)) >= 4.5, "accent {d}/{l} on {bg}");
            }
            assert!(contrast("#FFFFFF", l) >= 4.5 && contrast("#04131A", d) >= 4.5, "text on the accent button");
        }
        // Dots are not text: 3:1 against the panels is enough to see them.
        for (n, d, l) in PROJECT_COLOURS {
            assert!(contrast(d, get(DARK, "panel")) >= 3.0 && contrast(l, get(LIGHT, "panel")) >= 3.0, "{n}");
        }
    }

    /// CIELAB (D65), for telling colours apart as eyes do.
    fn lab(hex: &str) -> [f64; 3] {
        let v = u32::from_str_radix(hex.trim_start_matches('#'), 16).unwrap();
        let lin = |c: u32| {
            let c = f64::from(c & 0xff) / 255.0;
            if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
        };
        let (r, g, b) = (lin(v >> 16), lin(v >> 8), lin(v));
        let x = (r * 0.4124 + g * 0.3576 + b * 0.1805) / 0.95047;
        let y = r * 0.2126 + g * 0.7152 + b * 0.0722;
        let z = (r * 0.0193 + g * 0.1192 + b * 0.9505) / 1.08883;
        let t = |x: f64| if x > 0.008856 { x.cbrt() } else { 7.787 * x + 16.0 / 116.0 };
        [116.0 * t(y) - 16.0, 500.0 * (t(x) - t(y)), 200.0 * (t(y) - t(z))]
    }

    fn delta_e(a: &str, b: &str) -> f64 {
        let (a, b) = (lab(a), lab(b));
        ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
    }

    #[test]
    fn project_colours_stay_clear_of_amber_and_of_each_other_in_both_themes() {
        for (set, i) in [(DARK, 1), (LIGHT, 2)] {
            let amber = get(set, "amber");
            for (k, c) in PROJECT_COLOURS.iter().enumerate() {
                let own = if i == 1 { c.1 } else { c.2 };
                assert!(delta_e(own, amber) >= 20.0, "{} is too close to amber ({:.1})", c.0, delta_e(own, amber));
                for o in &PROJECT_COLOURS[k + 1..] {
                    let other = if i == 1 { o.1 } else { o.2 };
                    assert!(delta_e(own, other) >= 15.0, "{} and {} look alike ({:.1})", c.0, o.0, delta_e(own, other));
                }
            }
        }
    }

    #[test]
    fn every_named_colour_is_in_both_sets_and_the_stylesheet_names_only_those() {
        let names: Vec<&str> = DARK.iter().map(|(k, _)| *k).collect();
        assert_eq!(names, LIGHT.iter().map(|(k, _)| *k).collect::<Vec<_>>());
        let css = include_str!("style.css");
        let defined = palette_css(false, "cyan");
        let mut at = 0;
        while let Some(i) = css[at..].find('@') {
            let start = at + i + 1;
            let name: String = css[start..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
            at = start;
            // At-rules (the motion's @keyframes) are not colours.
            if name == "keyframes" {
                continue;
            }
            assert!(defined.contains(&format!("@define-color {name} ")), "style.css uses @{name}, which no palette defines");
        }
        let names: Vec<&str> = PROJECT_COLOURS.iter().map(|c| c.0).collect();
        assert_eq!(names, wake_core::store::COLOUR_NAMES);
    }
}
