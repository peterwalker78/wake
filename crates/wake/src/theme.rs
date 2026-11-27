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
/// The first choice in Settings, "desktop", is not here: it follows the desktop's own accent.
pub const ACCENTS: [(&str, &str, &str); 4] = [("cyan", "#38CEFF", "#005F88"), ("mint", "#42E6A4", "#076A42"), ("violet", "#B79CFF", "#6B47C9"), ("blue", "#6EA8FF", "#2B5FC7")];

/// "desktop" when the desktop shares no accent colour: Slipstream's ice cyan in the dark theme and
/// its deep teal in the light one (and as the tertiary colour in both).
const DESKTOP_DARK: &str = "#42D3FF";
const DESKTOP_DEEP: &str = "#005A75";

/// Text on the accent: whichever of these reads better on it.
const ON_DARK_TEXT: &str = "#04131A";
const ON_WHITE_TEXT: &str = "#FFFFFF";

/// The colours that follow the accent: the accent itself, text on it, and the tertiary colour (a
/// deep shade of the accent's hue, the same in both themes).
#[derive(Debug, Clone, PartialEq)]
pub struct AccentSet {
    pub accent: String,
    pub on_accent: String,
    pub tertiary: String,
}

/// The accent colours for the choice `accent` ("desktop" or a preset) in the light or dark theme,
/// with `desktop` the desktop's accent colour if it shares one (sRGB, each channel 0 to 1).
pub fn accent_set(accent: &str, light: bool, desktop: Option<[f64; 3]>) -> AccentSet {
    let (accent, tertiary) = match ACCENTS.iter().find(|a| a.0 == accent) {
        Some((_, d, l)) => ((*if light { l } else { d }).to_string(), colour::hex(colour::deep(colour::rgb(d)))),
        None => match desktop {
            Some(c) => {
                let deep = colour::hex(colour::deep(c));
                (if light { deep.clone() } else { colour::hex(colour::readable_on(c, &DARK_GROUNDS.map(colour::rgb), 4.5)) }, deep)
            }
            None => ((if light { DESKTOP_DEEP } else { DESKTOP_DARK }).to_string(), DESKTOP_DEEP.to_string()),
        },
    };
    let on_accent = if colour::contrast(colour::rgb(ON_DARK_TEXT), colour::rgb(&accent)) >= colour::contrast(colour::rgb(ON_WHITE_TEXT), colour::rgb(&accent)) { ON_DARK_TEXT } else { ON_WHITE_TEXT };
    AccentSet { accent, on_accent: on_accent.to_string(), tertiary }
}

/// The dark theme's grounds that text in the accent colour sits on (as the presets are checked).
const DARK_GROUNDS: [&str; 4] = ["#0A0D12", "#0E131A", "#111822", "#15202B"];

/// Colour maths for the desktop's accent: the same OKLCH rule Slipstream's settings use
/// (`slipstream-settings/src/harmony.rs`) for the tertiary colour, a deep shade of the primary's
/// hue, so that Wake on Slipstream shows Slipstream's own three colours.
mod colour {
    /// The deep shade's lightness, and how vivid it is as a share of the colour's vividness.
    const DEEP: f64 = 0.44;
    const DEEP_SHARE: f64 = 0.63;
    /// Below this a colour is grey enough that its hue says nothing: the deep shade is then as vivid
    /// as Slipstream's colour wheel allows.
    const GREY: f64 = 0.04;
    const MOST_CHROMA: f64 = 0.20;

    /// "#rrggbb" as channels 0 to 1 (grey for anything else).
    pub fn rgb(hex: &str) -> [f64; 3] {
        let h = hex.trim_start_matches('#');
        let ch = |i: usize| h.get(i..i + 2).and_then(|x| u8::from_str_radix(x, 16).ok()).map_or(0.5, |v| f64::from(v) / 255.0);
        [ch(0), ch(2), ch(4)]
    }

    pub fn hex(rgb: [f64; 3]) -> String {
        let [r, g, b] = rgb.map(|c| (c.clamp(0.0, 1.0) * 255.0).round() as u8);
        format!("#{r:02X}{g:02X}{b:02X}")
    }

    fn linear(c: f64) -> f64 {
        if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    }

    fn encoded(c: f64) -> f64 {
        if c <= 0.0031308 { 12.92 * c } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 }
    }

    /// WCAG contrast between two colours.
    pub fn contrast(a: [f64; 3], b: [f64; 3]) -> f64 {
        let lum = |c: [f64; 3]| {
            let [r, g, b] = c.map(linear);
            0.2126 * r + 0.7152 * g + 0.0722 * b
        };
        let (x, y) = (lum(a), lum(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    /// Lightness, chroma and hue in degrees.
    pub fn oklch(rgb: [f64; 3]) -> (f64, f64, f64) {
        let [r, g, b] = rgb.map(linear);
        let l = (0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b).cbrt();
        let m = (0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b).cbrt();
        let s = (0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b).cbrt();
        let lightness = 0.2104542553 * l + 0.7936177850 * m - 0.0040720468 * s;
        let a = 1.9779984951 * l - 2.4285922050 * m + 0.4505937099 * s;
        let b = 0.0259040371 * l + 0.7827717662 * m - 0.8086757660 * s;
        (lightness, a.hypot(b), b.atan2(a).to_degrees().rem_euclid(360.0))
    }

    /// The OKLCH colour as linear red, green and blue, which may fall outside 0 to 1.
    fn linear_rgb(lightness: f64, chroma: f64, hue: f64) -> [f64; 3] {
        let (sin, cos) = hue.to_radians().sin_cos();
        let (a, b) = (chroma * cos, chroma * sin);
        let l = (lightness + 0.3963377774 * a + 0.2158037573 * b).powi(3);
        let m = (lightness - 0.1055613458 * a - 0.0638541728 * b).powi(3);
        let s = (lightness - 0.0894841775 * a - 1.2914855480 * b).powi(3);
        [4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s, -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s, -0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s]
    }

    /// How vivid `hue` can be at `lightness`, up to `chroma`, and still be shown on a screen.
    fn most_chroma(lightness: f64, hue: f64, chroma: f64) -> f64 {
        let shown = |chroma| linear_rgb(lightness, chroma, hue).iter().all(|c| (-1e-4..=1.0 + 1e-4).contains(c));
        if shown(chroma) {
            return chroma;
        }
        let (mut low, mut high) = (0.0, chroma);
        for _ in 0..18 {
            let middle = (low + high) / 2.0;
            if shown(middle) {
                low = middle;
            } else {
                high = middle;
            }
        }
        low
    }

    fn at(lightness: f64, chroma: f64, hue: f64) -> [f64; 3] {
        let chroma = most_chroma(lightness, hue, chroma);
        linear_rgb(lightness, chroma, hue).map(|c| encoded(c.clamp(0.0, 1.0)))
    }

    /// The deep shade of `rgb`: its hue at lightness 0.44, at 0.63 of its vividness.
    pub fn deep(rgb: [f64; 3]) -> [f64; 3] {
        let (_, chroma, hue) = oklch(rgb);
        let chroma = if chroma < GREY { MOST_CHROMA } else { chroma };
        at(DEEP, chroma * DEEP_SHARE, hue)
    }

    /// `rgb` as it is when it has `ratio` contrast with every one of `grounds`, else moved away from
    /// them in OKLCH lightness (lighter on dark grounds, darker on light ones), keeping its hue and
    /// as much of its chroma as the screen can show, just far enough to reach it.
    pub fn readable_on(rgb: [f64; 3], grounds: &[[f64; 3]], ratio: f64) -> [f64; 3] {
        // Checked as it will be shown: rounded to whole channels.
        let shown = |c: [f64; 3]| self::rgb(&hex(c));
        let reads = |c: [f64; 3]| grounds.iter().all(|g| contrast(c, *g) >= ratio);
        if reads(shown(rgb)) {
            return rgb;
        }
        let (lightness, chroma, hue) = oklch(rgb);
        // Away from the grounds first: up when they are darker than the colour, else down; the other
        // way only when that can't get there (black on a dark ground).
        let ground = grounds.iter().map(|g| oklch(*g).0).sum::<f64>() / grounds.len().max(1) as f64;
        let away = if ground <= lightness { 0.005 } else { -0.005 };
        for step in [away, -away] {
            let mut l = lightness;
            while (0.0..=1.0).contains(&(l + step)) {
                l += step;
                let c = shown(at(l, chroma, hue));
                if reads(c) {
                    return c;
                }
            }
        }
        if ground < 0.5 { [1.0; 3] } else { [0.0; 3] }
    }
}

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
    /// The accent colours `apply` last showed.
    static SHOWING: RefCell<Option<AccentSet>> = const { RefCell::new(None) };
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

/// The palette's CSS: every named colour the stylesheet uses, for `light` or dark, with `accent`
/// ("desktop" or a preset) and the desktop's accent colour if it shares one.
pub fn palette_css(light: bool, accent: &str, desktop: Option<[f64; 3]>) -> String {
    let set = if light { LIGHT } else { DARK };
    let mut out = String::new();
    for (k, v) in set {
        out.push_str(&format!("@define-color {k} {v};\n"));
    }
    let a = accent_set(accent, light, desktop);
    out.push_str(&format!("@define-color accent {};\n", a.accent));
    out.push_str(&format!("@define-color on_accent {};\n", a.on_accent));
    out.push_str(&format!("@define-color tertiary {};\n", a.tertiary));
    // Settings' swatches: the desktop's colour as it shows here, then the presets.
    out.push_str(&format!("@define-color acc_desktop {};\n", accent_set("desktop", light, desktop).accent));
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

/// The accent colours showing now (as `apply` last set them).
fn accent_now() -> AccentSet {
    SHOWING.with(|s| s.borrow().clone()).unwrap_or_else(|| accent_set(&crate::prefs::get().accent, LIGHT_NOW.with(Cell::get), desktop_accent()))
}

/// The accent's colour now, for Pango markup.
pub fn accent_hex() -> String {
    accent_now().accent
}

/// The tertiary colour now (a deep shade of the accent's hue), for drawing.
pub fn tertiary_hex() -> String {
    accent_now().tertiary
}

/// "#RRGGBB" as channels 0 to 1.
pub fn colour_rgb(hex: &str) -> [f64; 3] {
    colour::rgb(hex)
}

/// `colour` made to stand out from `background` by `ratio` (WCAG contrast; 3:1 is what WCAG 1.4.11
/// asks of marks that aren't text): unchanged when it already does, else its OKLCH lightness moved
/// away from the background just far enough, hue and chroma kept. Lighter on a dark background,
/// darker on a light one, never toward the background.
pub fn readable_on(colour: &str, background: &str, ratio: f64) -> String {
    colour::hex(colour::readable_on(colour::rgb(colour), &[colour::rgb(background)], ratio))
}

/// The colour of `top` drawn at `alpha` over `under`, as "#RRGGBB".
pub fn over(top: &str, alpha: f64, under: &str) -> String {
    let (t, u) = (colour::rgb(top), colour::rgb(under));
    colour::hex(std::array::from_fn(|i| t[i] * alpha + u[i] * (1.0 - alpha)))
}

pub fn is_light() -> bool {
    LIGHT_NOW.with(Cell::get)
}

thread_local! {
    /// The desktop's preference as last read from the portal: Some(true) light, Some(false) dark.
    static SYSTEM_LIGHT: Cell<Option<Option<bool>>> = const { Cell::new(None) };
    /// The desktop's accent colour as last read from the portal (None inside: it shares none).
    static SYSTEM_ACCENT: Cell<Option<Option<[f64; 3]>>> = const { Cell::new(None) };
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

/// The portal's `accent-color`: sRGB `(ddd)`, each channel 0 to 1; anything out of that range
/// means the desktop has no accent colour.
fn accent_colour(v: &glib::Variant) -> Option<[f64; 3]> {
    // ReadOne answers (v); SettingChanged carries the value itself. Unwrap variants until the triple.
    let mut v = v.clone();
    for _ in 0..8 {
        if let Some((r, g, b)) = v.get::<(f64, f64, f64)>() {
            let c = [r, g, b];
            return c.iter().all(|x| (0.0..=1.0).contains(x)).then_some(c);
        }
        if v.is_container() && v.n_children() == 1 {
            v = v.child_value(0);
        } else if v.is_type(glib::VariantTy::VARIANT) {
            v = v.as_variant()?;
        } else {
            return None;
        }
    }
    None
}

/// One `org.freedesktop.appearance` key from the settings portal.
fn read_appearance(key: &str) -> Option<glib::Variant> {
    let bus = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).ok()?;
    let args = ("org.freedesktop.appearance", key).to_variant();
    bus.call_sync(Some(PORTAL), PORTAL_PATH, PORTAL_SETTINGS, "ReadOne", Some(&args), None, gio::DBusCallFlags::NONE, 700, gio::Cancellable::NONE).ok()
}

/// The desktop's accent colour from the settings portal (Slipstream answers with its primary
/// colour; GNOME and KDE answer too). None when there is no portal, the desktop sets none, or it
/// is out of range. For a screenshot, `WAKE_DESKTOP_ACCENT` ("#rrggbb", or "none") stands in.
pub fn desktop_accent() -> Option<[f64; 3]> {
    if let Some(v) = SYSTEM_ACCENT.with(Cell::get) {
        return v;
    }
    let v = match std::env::var("WAKE_DESKTOP_ACCENT") {
        Ok(hex) if hex.len() == 7 && hex.starts_with('#') && hex[1..].chars().all(|c| c.is_ascii_hexdigit()) => Some(colour::rgb(&hex)),
        Ok(_) => None,
        // Tests never ask the desktop: they see Wake's own colours.
        Err(_) if cfg!(test) => None,
        Err(_) => read_appearance("accent-color").and_then(|r| accent_colour(&r)),
    };
    SYSTEM_ACCENT.with(|c| c.set(Some(v)));
    v
}

/// The desktop's light/dark preference, read from the settings portal, never from GTK's own
/// colour-scheme setting: Wake sets that one itself, so reading it back would only echo Wake's
/// own choice (and following the echo looped forever).
fn system_prefers_light() -> Option<bool> {
    if let Some(v) = SYSTEM_LIGHT.with(Cell::get) {
        return v;
    }
    let v = read_appearance("color-scheme").and_then(|r| scheme_light(&r));
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

/// Shows `theme` with `accent`. True if light or dark, or the accent's colours, changed (markup
/// colours need a redraw then).
pub fn apply(theme: &str, accent: &str) -> bool {
    let Some(display) = gdk::Display::default() else { return false };
    let light = wants_light(theme);
    let desktop = desktop_accent();
    let now = accent_set(accent, light, desktop);
    let changed = (LIGHT_NOW.with(|l| l.replace(light)) != light) | (SHOWING.with(|s| s.replace(Some(now.clone()))) != Some(now));
    let settings = gtk::Settings::for_display(&display);
    if settings.is_gtk_application_prefer_dark_theme() != !light {
        settings.set_gtk_application_prefer_dark_theme(!light);
    }
    let css = palette_css(light, accent, desktop);
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

/// Calls `on_change` whenever the desktop's light/dark preference or its accent colour changes (set
/// up once), from the portal's SettingChanged signal. Only a real change calls it.
pub fn watch_system(on_change: impl Fn() + 'static) {
    if WATCHING.with(|w| w.replace(true)) {
        return;
    }
    let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else { return };
    // The subscription lives as long as the app; Wake never unwatches.
    #[expect(deprecated, reason = "subscribe_to_signal unsubscribes when its handle is dropped; this subscription lasts as long as the app")]
    let _ = bus.signal_subscribe(
        Some(PORTAL),
        Some(PORTAL_SETTINGS),
        Some("SettingChanged"),
        Some(PORTAL_PATH),
        Some("org.freedesktop.appearance"),
        gio::DBusSignalFlags::NONE,
        move |_, _, _, _, _, params| {
            if params.n_children() < 3 {
                return;
            }
            let changed = match params.child_value(1).get::<String>().as_deref() {
                Some("color-scheme") => {
                    let v = scheme_light(&params.child_value(2));
                    SYSTEM_LIGHT.with(|c| c.replace(Some(v))) != Some(v)
                }
                // A screenshot's stand-in colour stays put.
                Some("accent-color") if std::env::var_os("WAKE_DESKTOP_ACCENT").is_none() => {
                    let v = accent_colour(&params.child_value(2));
                    SYSTEM_ACCENT.with(|c| c.replace(Some(v))) != Some(v)
                }
                _ => false,
            };
            if changed {
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
    fn the_portal_accent_reads_through_its_variant_wrapping_and_out_of_range_is_none() {
        let c = (0.26f64, 0.83f64, 1.0f64).to_variant();
        assert_eq!(accent_colour(&glib::Variant::from_variant(&glib::Variant::from_variant(&c))), Some([0.26, 0.83, 1.0]));
        assert_eq!(accent_colour(&(glib::Variant::from_variant(&c),).to_variant()), Some([0.26, 0.83, 1.0]));
        assert_eq!(accent_colour(&(-1.0f64, -1.0f64, -1.0f64).to_variant()), None, "unset");
        assert_eq!(accent_colour(&(0.5f64, 1.2f64, 0.0f64).to_variant()), None);
        assert_eq!(accent_colour(&2u32.to_variant()), None);
    }

    /// Slipstream's colours: the deep rule (OKLCH hue kept, lightness 0.44, chroma x 0.63) gives
    /// its deep teal from its ice cyan, and Wake's colours with no desktop accent are those two.
    #[test]
    fn the_deep_rule_matches_slipstreams_and_the_fallbacks_are_its_colours() {
        let deep = colour::deep(colour::rgb("#42d3ff"));
        assert!(delta_e(&colour::hex(deep), "#005a75") < 3.0, "{}", colour::hex(deep));
        let (l, c, h) = colour::oklch(deep);
        let (_, c0, h0) = colour::oklch(colour::rgb("#42d3ff"));
        assert!((l - 0.44).abs() < 0.01 && (h - h0).abs() < 2.0 && (c - c0 * 0.63).abs() < 0.01, "{l} {c} {h}");
        let none = |light| accent_set("desktop", light, None);
        assert_eq!((none(false).accent.as_str(), none(true).accent.as_str()), ("#42D3FF", "#005A75"));
        assert_eq!((none(false).tertiary.as_str(), none(true).tertiary.as_str()), ("#005A75", "#005A75"));
        let ice = Some(colour::rgb("#42d3ff"));
        assert_eq!(accent_set("desktop", false, ice).accent, "#42D3FF", "the dark theme's accent is the colour itself");
        assert_eq!(accent_set("desktop", true, ice).accent, accent_set("desktop", true, ice).tertiary, "the light theme's is its deep shade");
        // A preset keeps its own colours whatever the desktop says.
        assert_eq!(accent_set("mint", false, ice).accent, "#42E6A4");
        assert_eq!(accent_set("mint", true, ice).accent, "#076A42");
    }

    /// Any colour the desktop might share: the accent reads on every ground of its theme, the
    /// text on the accent button reads on it, and the tertiary colour is the same in both themes.
    #[test]
    fn any_desktop_colour_gives_a_readable_accent_and_readable_text_on_it() {
        let mut colours: Vec<[f64; 3]> = vec![[0.0; 3], [1.0; 3], [0.5; 3], [1.0, 1.0, 0.0], [0.0, 0.0, 1.0], [0.05, 0.1, 0.4]];
        // GNOME's accents and a spread of hues and strengths.
        for hex in ["#3584E4", "#2190A4", "#3A944A", "#C88800", "#ED5B00", "#E62D42", "#D56199", "#9141AC", "#6F8396"] {
            colours.push(colour::rgb(hex));
        }
        for i in 0..36 {
            let t = f64::from(i) / 36.0 * std::f64::consts::TAU;
            for k in [0.25, 0.6, 1.0] {
                colours.push([0.5 + 0.5 * t.cos() * k, 0.5 + 0.5 * (t + 2.1).cos() * k, 0.5 + 0.5 * (t + 4.2).cos() * k]);
            }
        }
        for c in colours {
            let (d, l) = (accent_set("desktop", false, Some(c)), accent_set("desktop", true, Some(c)));
            for bg in ["bg", "panel", "panel2", "rowhi"] {
                assert!(contrast(&d.accent, get(DARK, bg)) >= 4.5, "dark accent {} on {bg} (from {})", d.accent, colour::hex(c));
                assert!(contrast(&l.accent, get(LIGHT, bg)) >= 4.5, "light accent {} on {bg} (from {})", l.accent, colour::hex(c));
            }
            for a in [&d, &l] {
                assert!(contrast(&a.on_accent, &a.accent) >= 4.5, "text {} on the accent {} (from {})", a.on_accent, a.accent, colour::hex(c));
            }
            assert_eq!(d.tertiary, l.tertiary);
        }
    }

    /// Task waves on a trace: the tertiary colour, lightness moved just far enough for 3:1 against
    /// the trace's background, hue kept; a colour that already stands out is left alone.
    #[test]
    fn a_mark_reaches_three_to_one_on_its_background_keeping_its_hue() {
        let ground = over("#0B0E13", f64::from(0xb8u8) / 255.0, get(DARK, "panel"));
        let t = readable_on("#005A75", &ground, 3.0);
        assert!(contrast(&t, &ground) >= 3.0, "{t} on {ground}");
        let (l0, _, h0) = colour::oklch(colour::rgb("#005A75"));
        let (l, _, h) = colour::oklch(colour::rgb(&t));
        assert!(l > l0 && (h - h0).abs() < 3.0, "lighter, same hue: {t}");
        assert!(contrast(&t, &ground) < 3.3, "just far enough: {t}");
        assert_eq!(readable_on("#42D3FF", &ground, 3.0), "#42D3FF", "already stands out");
        // On a light background it goes darker instead.
        let pale = readable_on("#7FB8CC", "#F6F8FB", 3.0);
        assert!(contrast(&pale, "#F6F8FB") >= 3.0 && colour::oklch(colour::rgb(&pale)).0 < colour::oklch(colour::rgb("#7FB8CC")).0, "{pale}");
        // Any accent's tertiary works on the trace in either theme.
        for c in ["#3584E4", "#E62D42", "#C88800", "#6F8396", "#42D3FF", "#FFFFFF", "#000000"] {
            let tert = accent_set("desktop", false, Some(colour::rgb(c))).tertiary;
            for under in [get(DARK, "panel"), get(LIGHT, "panel")] {
                let g = over("#0B0E13", f64::from(0xb8u8) / 255.0, under);
                assert!(contrast(&readable_on(&tert, &g, 3.0), &g) >= 3.0, "{tert} on {g}");
            }
        }
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
        let defined = palette_css(false, "cyan", None);
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
