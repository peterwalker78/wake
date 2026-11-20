//! Plain-words formatting shared by the views: times, durations, sizes, counts.

use gtk4::glib;

/// "1 session", "3 sessions".
pub fn pl(n: impl Into<u64>, one: &str, many: &str) -> String {
    let n = n.into();
    format!("{n} {}", if n == 1 { one } else { many })
}

/// An ISO-8601 time as a GLib time, if it parses.
pub fn parse(iso: &str) -> Option<glib::DateTime> {
    glib::DateTime::from_iso8601(iso, Some(&glib::TimeZone::utc())).ok()
}

/// Seconds since `iso`, never negative.
pub fn secs_since(iso: &str) -> Option<i64> {
    let then = parse(iso)?;
    let now = glib::DateTime::now_utc().ok()?;
    Some((now.difference(&then).as_microseconds() / 1_000_000).max(0))
}

/// One way of saying how long, everywhere: "40 s", "4 min", "2 h", "3 days" (hours up to two days).
pub fn span(s: u64) -> String {
    match s {
        s if s < 60 => format!("{s} s"),
        s if s < 3600 => format!("{} min", s / 60),
        s if s < 2 * 86_400 => format!("{} h", s / 3600),
        s => format!("{} days", s / 86_400),
    }
}

/// `span` as an age: "just now" under a minute (so nothing ticks by the second), else "4 min ago".
pub fn ago_secs(s: u64) -> String {
    if s < 60 { "just now".into() } else { format!("{} ago", span(s)) }
}

/// The age of an ISO-8601 time: "just now", "3 min ago", "2 h ago", "4 days ago".
pub fn ago(iso: Option<&str>) -> String {
    iso.and_then(secs_since).map(|s| ago_secs(s as u64)).unwrap_or_default()
}

/// "today 14:05", "yesterday 09:12", "12 Nov 18:40", in local time.
pub fn when(iso: Option<&str>) -> String {
    let Some(t) = iso.and_then(parse).and_then(|t| t.to_local().ok()) else {
        return String::new();
    };
    let Ok(now) = glib::DateTime::now_local() else {
        return String::new();
    };
    let hm = t.format("%H:%M").map(|s| s.to_string()).unwrap_or_default();
    let day = |d: &glib::DateTime| (d.year(), d.day_of_year());
    if day(&t) == day(&now) {
        return format!("today {hm}");
    }
    if let Ok(y) = now.add_days(-1)
        && day(&t) == day(&y)
    {
        return format!("yesterday {hm}");
    }
    let date = t.format("%-d %b").map(|s| s.to_string()).unwrap_or_default();
    format!("{date} {hm}")
}

/// "14:05", "yesterday", "12 Nov".
pub fn short_when(iso: Option<&str>) -> String {
    let w = when(iso);
    if let Some(rest) = w.strip_prefix("today ") {
        return rest.to_string();
    }
    if w.starts_with("yesterday") {
        return "yesterday".into();
    }
    w.rsplit_once(' ').map(|(d, _)| d.to_string()).unwrap_or(w)
}

/// The local clock, "22:39".
pub fn clock() -> String {
    glib::DateTime::now_local().ok().and_then(|t| t.format("%H:%M").ok()).map(|s| s.to_string()).unwrap_or_default()
}

/// Minutes as a span: "45 min", "1 h".
pub fn minutes(m: u32) -> String {
    span(u64::from(m) * 60)
}

/// Seconds as a span.
pub fn secs(n: u64) -> String {
    span(n)
}

/// An idle time in plain words: "4 min", "7 h", "2 days".
pub fn idle_for(s: u64) -> String {
    span(s)
}

/// Claude's recap without the boilerplate it tacks on ("(disable recaps in /config)").
pub fn recap(s: &str) -> String {
    // "(disable recaps in /config)", "(turn off recaps in /config)", whatever its wording.
    wake_core::live::strip_config_note(s)
}

/// The local day of a time, "2026-11-20", for sorting by day.
pub fn day_key(iso: &str) -> Option<String> {
    parse(iso)?.to_local().ok()?.format("%Y-%m-%d").ok().map(|s| s.to_string())
}

/// The local day of a time, as a heading: "Today", "Yesterday", "Tue 18 Nov".
pub fn day(iso: Option<&str>) -> String {
    let Some(t) = iso.and_then(parse).and_then(|t| t.to_local().ok()) else { return "Earlier".into() };
    let Ok(now) = glib::DateTime::now_local() else { return String::new() };
    let key = |d: &glib::DateTime| (d.year(), d.day_of_year());
    if key(&t) == key(&now) {
        return "Today".into();
    }
    if now.add_days(-1).is_ok_and(|y| key(&t) == key(&y)) {
        return "Yesterday".into();
    }
    t.format("%a %-d %b").map(|s| s.to_string()).unwrap_or_default()
}

/// A project's folder as worth showing under its name: `~/…`, or nothing when it just repeats the name.
pub fn folder_hint(root: &str, name: &str) -> String {
    if base(root).eq_ignore_ascii_case(name) { String::new() } else { tildify(root) }
}

/// Bytes as "640 MB", "4.1 GB" or "86 GB": memory and disk alike.
pub fn bytes(b: u64) -> String {
    let mb = b as f64 / 1_048_576.0;
    let gb = mb / 1024.0;
    if mb < 1000.0 {
        format!("{} MB", mb.round() as u64)
    } else if gb < 10.0 {
        format!("{gb:.1} GB")
    } else {
        format!("{gb:.0} GB")
    }
}

/// `/home/sam/x` or `/home/sam/x` as `~/x`.
pub fn tildify(p: &str) -> String {
    for prefix in ["/var/home/", "/home/"] {
        if let Some(rest) = p.strip_prefix(prefix) {
            return match rest.split_once('/') {
                Some((_, tail)) => format!("~/{tail}"),
                None => "~".into(),
            };
        }
    }
    p.to_string()
}

/// A path relative to `cwd` if inside it, else tildified.
pub fn rel(p: &str, cwd: Option<&str>) -> String {
    if let Some(cwd) = cwd
        && let Some(rest) = p.strip_prefix(&format!("{cwd}/"))
    {
        return rest.to_string();
    }
    tildify(p)
}

/// The last part of a path.
pub fn base(p: &str) -> String {
    p.trim_end_matches('/').rsplit('/').next().unwrap_or(p).to_string()
}

/// One line, at most `n` characters, with an ellipsis if cut.
pub fn clip(s: &str, n: usize) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= n { one } else { format!("{}…", one.chars().take(n.saturating_sub(1)).collect::<String>().trim_end()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_and_paths_read_plainly() {
        assert_eq!(bytes(640 * 1_048_576), "640 MB");
        assert_eq!(bytes(4_402_341_888), "4.1 GB");
        assert_eq!(bytes(86 << 30), "86 GB");
        assert_eq!(tildify("/home/sam/Projects/wake"), "~/Projects/wake");
        assert_eq!(rel("/a/b/c.rs", Some("/a")), "b/c.rs");
        assert_eq!(minutes(76), "1 h");
        assert_eq!(clip("one\ntwo three", 7), "one tw…");
        assert_eq!(pl(1u32, "idea", "ideas"), "1 idea");
    }

    #[test]
    fn one_time_format_everywhere() {
        assert_eq!(span(42), "42 s");
        assert_eq!(span(4 * 60 + 59), "4 min");
        assert_eq!(span(2 * 3600), "2 h");
        assert_eq!(span(35 * 3600), "35 h");
        assert_eq!(span(9 * 86_400), "9 days");
        assert_eq!(ago_secs(40), "just now");
        assert_eq!(ago_secs(90), "1 min ago");
        assert_eq!(ago_secs(3 * 86_400), "3 days ago");
        assert_eq!(idle_for(31 * 3600), "31 h");
    }

    #[test]
    fn recaps_lose_the_boilerplate_and_folders_their_repeat() {
        assert_eq!(recap("All merged. Next, the pack. (disable recaps in /config)"), "All merged. Next, the pack.");
        assert_eq!(recap("plain"), "plain");
        assert_eq!(recap("Next, the pack. (turn off recaps in /config)"), "Next, the pack.");
        assert_eq!(day(None), "Earlier");
        assert_eq!(folder_hint("/home/sam/Projects/wake", "wake"), "");
        assert_eq!(folder_hint("/home/sam/Projects/tideline-desktop", "Slipstream"), "~/Projects/tideline-desktop");
    }
}
