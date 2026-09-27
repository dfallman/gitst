//! Text fitting and time formatting, all measured in terminal cells.

use chrono::{Local, TimeZone};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub fn width(s: &str) -> usize {
    s.width()
}

/// Shortens a path from the left, keeping as many trailing components as
/// fit (always the file name when possible): `…/src/app.rs`.
pub fn truncate_left(path: &str, max: usize) -> String {
    if width(path) <= max {
        return path.to_string();
    }
    let parts: Vec<&str> = path.split('/').collect();
    let mut best: Option<String> = None;
    for start in (1..parts.len()).rev() {
        let candidate = format!("…/{}", parts[start..].join("/"));
        if width(&candidate) > max {
            break;
        }
        best = Some(candidate);
    }
    best.unwrap_or_else(|| tail(path, max))
}

/// `…` followed by as much of the end of `s` as fits in `max` cells.
fn tail(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let mut used = 1;
    let mut start = s.len();
    for (i, c) in s.char_indices().rev() {
        let w = c.width().unwrap_or(0);
        if used + w > max {
            break;
        }
        used += w;
        start = i;
    }
    format!("…{}", &s[start..])
}

/// Shortens text from the right: `subject…`.
pub fn truncate_right(s: &str, max: usize) -> String {
    if width(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 1;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > max {
            break;
        }
        used += w;
        out.push(c);
    }
    out.push('…');
    out
}

/// Compact age: `now`, `45s`, `12m`, `3h`, `2d`, `5w`, `3mo`, `2y`.
pub fn rel_age(secs: i64) -> String {
    const DAY: i64 = 86_400;
    match secs {
        s if s < 10 => "now".into(),
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < DAY => format!("{}h", s / 3600),
        s if s < 7 * DAY => format!("{}d", s / DAY),
        s if s < 60 * DAY => format!("{}w", s / (7 * DAY)),
        s if s < 365 * DAY => format!("{}mo", s / (30 * DAY)),
        s => format!("{}y", s / (365 * DAY)),
    }
}

/// Local wall-clock `HH:MM` for a unix time.
pub fn clock(unix: i64) -> String {
    match Local.timestamp_opt(unix, 0) {
        chrono::LocalResult::Single(t) | chrono::LocalResult::Ambiguous(t, _) => {
            t.format("%H:%M").to_string()
        }
        chrono::LocalResult::None => "--:--".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_left_keeps_filename_and_width() {
        assert_eq!(truncate_left("src/app.rs", 20), "src/app.rs");
        assert_eq!(
            truncate_left("very/long/dir/src/app.rs", 14),
            "…/src/app.rs"
        );
        assert_eq!(truncate_left("very/long/dir/src/app.rs", 10), "…/app.rs");
        assert_eq!(truncate_left("a_really_long_filename.rs", 8), "…name.rs");
        assert_eq!(truncate_left("x", 0), "");
        let t = truncate_left("文档/日本語のファイル.md", 10);
        assert!(width(&t) <= 10, "{t}");
        assert!(t.ends_with(".md"));
        assert!(width(&truncate_left("🎉🎉🎉/🎉🎉🎉🎉.md", 5)) <= 5);
        assert!(width(&truncate_left("日本語", 1)) <= 1);
    }

    #[test]
    fn truncate_right_fits() {
        assert_eq!(truncate_right("hello world", 6), "hello…");
        assert_eq!(truncate_right("hello", 5), "hello");
        assert_eq!(truncate_right("hello", 0), "");
        assert!(width(&truncate_right("日本語のテキスト", 5)) <= 5);
    }

    #[test]
    fn ages() {
        assert_eq!(rel_age(-5), "now");
        assert_eq!(rel_age(3), "now");
        assert_eq!(rel_age(45), "45s");
        assert_eq!(rel_age(720), "12m");
        assert_eq!(rel_age(3 * 3600), "3h");
        assert_eq!(rel_age(3 * 86400), "3d");
        assert_eq!(rel_age(21 * 86400), "3w");
        assert_eq!(rel_age(90 * 86400), "3mo");
        assert_eq!(rel_age(800 * 86400), "2y");
    }

    #[test]
    fn clock_is_hh_mm() {
        let c = clock(1_790_484_333);
        assert_eq!(c.len(), 5);
        assert_eq!(&c[2..3], ":");
    }
}
