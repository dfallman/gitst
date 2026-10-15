//! Text fitting and time formatting, all measured in terminal cells.

use chrono::{Local, TimeZone};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

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

/// User-perceived characters with their display widths, so emoji
/// sequences are measured and cut as a whole.
pub fn clusters(s: &str) -> impl DoubleEndedIterator<Item = (usize, &str, usize)> {
    s.grapheme_indices(true).map(|(i, g)| (i, g, g.width()))
}

/// `…` followed by as much of the end of `s` as fits in `max` cells.
fn tail(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let mut used = 1;
    let mut start = s.len();
    for (i, _, w) in clusters(s).rev() {
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
    let mut used = 1;
    let mut end = 0;
    for (i, g, w) in clusters(s) {
        if used + w > max {
            break;
        }
        used += w;
        end = i + g.len();
    }
    format!("{}…", &s[..end])
}

/// Compact age: `now`, `45s`, `12m`, `3h`, `2d`, `5w`, `3mo`, `2y`. A time
/// more than a minute ahead, after the clock was set back, is `in 3h`;
/// less is clock skew, and `now`.
pub fn rel_age(secs: i64) -> String {
    const DAY: i64 = 86_400;
    match secs {
        s if s < -60 => format!("in {}", rel_age(s.saturating_neg())),
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

/// Seconds until `rel_age(secs)` reads differently, as `secs` grows by one
/// a second.
pub fn age_changes_in(secs: i64) -> i64 {
    const DAY: i64 = 86_400;
    if secs < -60 {
        // `in …` counts down: it changes as the age drops below the unit it
        // shows, and becomes `now` once less than a minute ahead.
        let ahead = secs.saturating_neg();
        let unit = unit_of(ahead);
        return (ahead % unit + 1).min(ahead - 60);
    }
    if secs < 10 {
        return 10 - secs;
    }
    // A unit's count changes each unit, and the unit itself at the end of
    // its range, which need not fall on a whole number of units.
    let end = match secs {
        s if s < 60 => 60,
        s if s < 3600 => 3600,
        s if s < DAY => DAY,
        s if s < 7 * DAY => 7 * DAY,
        s if s < 60 * DAY => 60 * DAY,
        s if s < 365 * DAY => 365 * DAY,
        _ => return 365 * DAY - secs % (365 * DAY),
    };
    let unit = unit_of(secs);
    (unit - secs % unit).min(end - secs)
}

/// The unit `rel_age` counts a non-negative age in.
fn unit_of(secs: i64) -> i64 {
    const DAY: i64 = 86_400;
    match secs {
        s if s < 60 => 1,
        s if s < 3600 => 60,
        s if s < DAY => 3600,
        s if s < 7 * DAY => DAY,
        s if s < 60 * DAY => 7 * DAY,
        s if s < 365 * DAY => 30 * DAY,
        _ => 365 * DAY,
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
    fn emoji_sequences_are_measured_and_cut_whole() {
        let heart = "❤\u{fe0f}"; // emoji presentation: 2 cells
        let s = format!("{heart}{heart}{heart}{heart} fix");
        let r = truncate_right(&s, 8);
        assert!(width(&r) <= 8, "{r:?} is {} wide", width(&r));
        assert!(!r.contains("❤…"), "cut inside a cluster: {r:?}");
        let l = truncate_left(&s, 8);
        assert!(width(&l) <= 8, "{l:?}");
        assert!(!l.starts_with("…\u{fe0f}"), "{l:?}");
        let family = "👨\u{200d}👩\u{200d}👧 kids.md";
        let f = truncate_left(family, 9);
        assert!(width(&f) <= 9 && !f.contains('\u{200d}'), "{f:?}");
    }

    #[test]
    fn ages() {
        assert_eq!(rel_age(-5), "now");
        assert_eq!(rel_age(-3 * 3600), "in 3h");
        assert_eq!(rel_age(i64::MIN), format!("in {}", rel_age(i64::MAX)));
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
    fn age_changes_exactly_when_its_label_does() {
        const DAY: i64 = 86_400;
        let starts = [
            -400 * DAY,
            -61 * DAY,
            -8 * DAY,
            -DAY - 5,
            -3601,
            -125,
            -61,
            -60,
            0,
            9,
            10,
            59,
            60,
            3599,
            DAY - 1,
            7 * DAY - 1,
            60 * DAY - 1,
            365 * DAY - 1,
            400 * DAY,
        ];
        for s in starts {
            let n = age_changes_in(s);
            assert!(n >= 1, "{s}: {n}");
            // Checking each second is too slow for the longest units.
            if n <= 2 * DAY {
                for k in 1..n {
                    assert_eq!(rel_age(s + k), rel_age(s), "{s}+{k}");
                }
            }
            assert_ne!(rel_age(s + n), rel_age(s), "{s}+{n}");
        }
        assert_eq!(age_changes_in(59 * DAY), DAY, "8w becomes 2mo");
        assert!(age_changes_in(i64::MAX) >= 1);
        assert!(age_changes_in(i64::MIN) >= 1);
    }

    #[test]
    fn clock_is_hh_mm() {
        let c = clock(1_790_484_333);
        assert_eq!(c.len(), 5);
        assert_eq!(&c[2..3], ":");
    }
}
