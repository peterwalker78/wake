//! Small helpers shared by the core: times, plain-words durations, shell quoting, byte search and the
//! few text patterns the Python version used regular expressions for.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the epoch, now.
pub fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `2026-11-19T10:00:00.000Z` for an epoch time, milliseconds, UTC.
pub fn iso_from_epoch(t: f64) -> String {
    let ms_total = (t * 1000.0).floor() as i64;
    let secs = ms_total.div_euclid(1000);
    let ms = ms_total.rem_euclid(1000);
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (y, mo, d) = civil_from_days(days);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z", y, mo, d, rem / 3600, (rem / 60) % 60, rem % 60, ms)
}

/// A path under `home` written short, `~/x`, also when it is under home's real path (on Fedora
/// Atomic $HOME is /home/sam but paths are often /var/home/sam); any other path as it is.
pub fn tilde(p: impl AsRef<std::path::Path>, home: &std::path::Path) -> String {
    let p = p.as_ref().to_string_lossy();
    let real = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    for pre in [real.to_string_lossy(), home.to_string_lossy()] {
        if p == pre {
            return "~".into();
        }
        if let Some(rest) = p.strip_prefix(pre.as_ref())
            && rest.starts_with('/')
        {
            return format!("~{rest}");
        }
    }
    p.into_owned()
}

pub fn now_iso() -> String {
    iso_from_epoch(now())
}

fn digits(b: &[u8]) -> Option<u32> {
    if b.is_empty() || !b.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut n: u32 = 0;
    for &c in b {
        n = n.checked_mul(10)?.checked_add((c - b'0') as u32)?;
    }
    Some(n)
}

/// ISO-8601 (`YYYY-MM-DD[T ]HH:MM:SS[.frac][Z|±HH:MM]`, or a bare date) to epoch seconds. A time
/// without a zone is taken as UTC, as the Python version did.
pub fn parse_iso(s: &str) -> Option<f64> {
    let b = s.trim().as_bytes();
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y = digits(&b[0..4])? as i64;
    let mo = digits(&b[5..7])?;
    let d = digits(&b[8..10])?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let mut secs = days_from_civil(y, mo, d) as f64 * 86400.0;
    if b.len() == 10 {
        return Some(secs);
    }
    if (b[10] != b'T' && b[10] != b' ') || b.len() < 16 || b[13] != b':' {
        return None;
    }
    let h = digits(&b[11..13])?;
    let mi = digits(&b[14..16])?;
    let mut i = 16;
    let mut sec = 0.0;
    if b.len() >= 19 && b[16] == b':' {
        sec = digits(&b[17..19])? as f64;
        i = 19;
        if i < b.len() && (b[i] == b'.' || b[i] == b',') {
            let start = i + 1;
            let mut j = start;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if j == start {
                return None;
            }
            let frac = std::str::from_utf8(&b[start..j.min(start + 9)]).ok()?;
            sec += format!("0.{frac}").parse::<f64>().ok()?;
            i = j;
        }
    }
    if h > 23 || mi > 59 || sec >= 61.0 {
        return None;
    }
    secs += h as f64 * 3600.0 + mi as f64 * 60.0 + sec;
    let rest = &b[i..];
    match rest {
        [] | [b'Z'] | [b'z'] => Some(secs),
        [sign @ (b'+' | b'-'), r @ ..] => {
            let (oh, om) = match r {
                [a, b2, b':', c, d2] => (digits(&[*a, *b2])?, digits(&[*c, *d2])?),
                [a, b2, c, d2] => (digits(&[*a, *b2])?, digits(&[*c, *d2])?),
                [a, b2] => (digits(&[*a, *b2])?, 0),
                _ => return None,
            };
            let off = (oh * 3600 + om * 60) as f64;
            Some(if *sign == b'+' { secs - off } else { secs + off })
        }
        _ => None,
    }
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

/// Bytes as "640 MB", "4.1 GB" or "15 GB" (whole numbers from 10 GB up), and "under 1 MB" for
/// anything smaller than that but not nothing: memory and disk alike, here and in the app.
pub fn bytes(b: u64) -> String {
    const MB: u64 = 1 << 20;
    let mb = b as f64 / MB as f64;
    let gb = mb / 1024.0;
    if b > 0 && b < MB {
        "under 1 MB".into()
    } else if mb < 1000.0 {
        format!("{} MB", mb.round() as u64)
    } else if gb < 10.0 {
        format!("{gb:.1} GB")
    } else {
        format!("{gb:.0} GB")
    }
}

/// Python's `shlex.quote`.
pub fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "@%+=:,./-_".contains(c)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// Python's `shlex.join`.
pub fn shell_join<S: AsRef<str>>(argv: &[S]) -> String {
    argv.iter().map(|a| shell_quote(a.as_ref())).collect::<Vec<_>>().join(" ")
}

/// A GVariant text-format string literal, as gdbus parses its arguments.
pub fn gvariant_str_quote(text: &str) -> String {
    format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'"))
}

pub fn contains(hay: &[u8], needle: &[u8]) -> bool {
    memchr::memmem::find(hay, needle).is_some()
}

pub fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    memchr::memmem::find(hay, needle)
}

pub fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    memchr::memmem::rfind(hay, needle)
}

/// First `n` characters (not bytes).
pub fn take_chars(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((i, _)) => s[..i].to_string(),
        None => s.to_string(),
    }
}

/// Last `n` characters.
pub fn last_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count <= n {
        return s.to_string();
    }
    s.chars().skip(count - n).collect()
}

/// Collapse runs of whitespace to single spaces and trim (Python's `" ".join(s.split())`).
pub fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// `\bword\b` search over ASCII word characters.
fn has_word(hay: &[u8], word: &[u8], left_boundary: bool) -> bool {
    let mut from = 0;
    while let Some(i) = find(&hay[from..], word) {
        let at = from + i;
        let end = at + word.len();
        let left_ok = !left_boundary || !is_word(word[0]) || at == 0 || !is_word(hay[at - 1]);
        let right_ok = !is_word(word[word.len() - 1]) || end == hay.len() || !is_word(hay[end]);
        if left_ok && right_ok {
            return true;
        }
        from = at + 1;
    }
    false
}

const TEST_WORDS: &[&str] = &[
    "pytest",
    "unittest",
    "rspec",
    "jest",
    "vitest",
    "cargo nextest",
    "cargo test",
    "go test",
    "npm test",
    "npm run test",
    "yarn test",
    "yarn run test",
    "pnpm test",
    "pnpm run test",
    "bun test",
    "bun run test",
    "rails test",
    "make test",
    "make check",
    "ctest",
    "playwright test",
    "mix test",
    "meson test",
    "tox",
    "nox",
];

/// True when a shell command looks like it runs tests (the Python TEST_RE).
pub fn is_test_command(cmd: &str) -> bool {
    let b = cmd.as_bytes();
    TEST_WORDS.iter().any(|w| has_word(b, w.as_bytes(), true)) || has_word(b, b"bin/test", false)
}

/// Replace every `<open…>…</close>` (or to the end when unclosed) with `with`. `open` is the tag
/// start without `>`; when `any_attrs`, anything up to the next `>` may follow it.
pub fn replace_tagged(s: &str, open: &str, close: &str, any_attrs: bool, with: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(open) {
        let after = &rest[i + open.len()..];
        let body_start = if any_attrs {
            match after.find('>') {
                Some(j) => i + open.len() + j + 1,
                None => break,
            }
        } else {
            i + open.len()
        };
        out.push_str(&rest[..i]);
        out.push_str(with);
        match rest[body_start..].find(close) {
            Some(k) => rest = &rest[body_start + k + close.len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Is this a UUID (8-4-4-4-12 lowercase hex)?
pub fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, &c)| match i {
            8 | 13 | 18 | 23 => c == b'-',
            _ => c.is_ascii_digit() || (b'a'..=b'f').contains(&c),
        })
}

/// The UUID that ends `stem` (as in Codex's `rollout-<time>-<uuid>`), if it ends in one. Safe on any
/// text: never slices inside a character.
pub fn uuid_tail(stem: &str) -> Option<&str> {
    let start = stem.len().checked_sub(36)?;
    let tail = stem.get(start..)?;
    is_uuid(tail).then_some(tail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_tails() {
        assert_eq!(uuid_tail("rollout-2026-11-19T11-00-00-0193c2aa-0000-7c93-9c1d-5e2f8a0b4c17"), Some("0193c2aa-0000-7c93-9c1d-5e2f8a0b4c17"));
        assert_eq!(uuid_tail("short"), None);
        // a multi-byte character straddling the cut must not panic
        assert_eq!(uuid_tail("ééééééééééééééééééééééééééééééééééééé"), None);
        assert_eq!(uuid_tail("x€0193c2aa-0000-7c93-9c1d-f744ad8ba0b"), None);
    }

    #[test]
    fn iso_round_trip() {
        let t = parse_iso("2026-11-19T10:00:05.250Z").unwrap();
        assert_eq!(iso_from_epoch(t), "2026-11-19T10:00:05.250Z");
        assert_eq!(parse_iso("2026-11-19T11:00:00+01:00"), parse_iso("2026-11-19T10:00:00Z"));
        assert_eq!(parse_iso("2026-11-19T10:00:00"), parse_iso("2026-11-19T10:00:00.000Z"));
        assert!(parse_iso("nonsense").is_none());
        assert_eq!(iso_from_epoch(0.0), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn one_time_format_everywhere() {
        assert_eq!(span(5), "5 s");
        assert_eq!(span(4 * 60 + 59), "4 min");
        assert_eq!(span(720), "12 min");
        assert_eq!(span(35 * 3600), "35 h");
        assert_eq!(span(5 * 86400), "5 days");
    }

    #[test]
    fn sizes() {
        assert_eq!(bytes(15 << 30), "15 GB");
        assert_eq!(bytes(3 * (1 << 30) + (1 << 29)), "3.5 GB");
        assert_eq!(bytes(4_402_341_888), "4.1 GB");
        assert_eq!(bytes(181 << 20), "181 MB");
        assert_eq!(bytes(300 << 10), "under 1 MB");
        assert_eq!(bytes(0), "0 MB");
    }

    #[test]
    fn home_written_short() {
        let home = std::path::Path::new("/home/sam");
        assert_eq!(tilde("/home/sam/Projects/wake", home), "~/Projects/wake");
        assert_eq!(tilde("/home/sam", home), "~");
        assert_eq!(tilde("/home/samuel/x", home), "/home/samuel/x");
        assert_eq!(tilde("/home/other/x", home), "/home/other/x", "only this person's home");
        let real = crate::testing::scratch("tilde");
        let link = real.0.join("link");
        std::os::unix::fs::symlink(&real.0, &link).unwrap();
        assert_eq!(tilde(real.0.join("a/b"), &link), "~/a/b", "under home's real path");
    }

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("abc/d.e"), "abc/d.e");
        assert_eq!(shell_quote("it's"), "'it'\"'\"'s'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_join(&["a", "b c"]), "a 'b c'");
    }

    #[test]
    fn test_commands() {
        assert!(is_test_command("python3 -m pytest -q"));
        assert!(is_test_command("cd x && cargo test -p a"));
        assert!(is_test_command("npm run test"));
        assert!(is_test_command("./bin/test"));
        assert!(!is_test_command("cargo build"));
        assert!(!is_test_command("pytester"));
        assert!(!is_test_command("cat toxic"));
    }

    #[test]
    fn tags() {
        assert_eq!(replace_tagged("<pasted_content id=1>x</pasted_content> fix", "<pasted_content", "</pasted_content>", true, "[pasted text]"), "[pasted text] fix");
        assert_eq!(replace_tagged("a <system-reminder>zz", "<system-reminder>", "</system-reminder>", false, ""), "a ");
    }
}
