pub mod dashboard;
pub mod detail;
pub mod fmt;
pub mod layout;
pub mod theme;

use std::time::{Duration, UNIX_EPOCH};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{Action, App, STALE_LOCK, Target};
use crate::model::{Head, Leak, LeakSource, RepoOp, Snapshot};
use fmt::{age_changes_in, rel_age, truncate_right, width, wrap_words};
use layout::Density;
use theme::Theme;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Most rows a long warning wraps onto.
const WARNING_LINES: usize = 3;

/// Draws the whole UI and records the hit map for the next input.
pub fn draw(f: &mut Frame, app: &mut App, theme: &Theme) {
    app.hits.clear();
    app.nav.clear();
    app.row_targets.clear();
    app.detail_targets.clear();
    app.clearable.clear();
    app.age_wake.set(None);
    let area = f.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let density = Density::for_size(area.width, area.height);
    let Some(snap) = app.snap.clone() else {
        let r = Rect::new(area.x, area.y + area.height / 2, area.width, 1);
        f.render_widget(
            Paragraph::new(Line::from(" gitst · loading…").centered()).style(theme.dim),
            r,
        );
        return;
    };
    let in_detail = !app.stack.is_empty();
    // An open view is drawn at any width: it only cuts its lines shorter,
    // and the one-line summary would hide it while keys still went to it.
    if density.tiny && !in_detail {
        draw_tiny(f, area, &snap, theme);
        if app.help {
            detail::draw_short_help(f, theme, area);
        }
        return;
    }

    let mut y = area.y;
    let mut left = area.height;
    let max_warnings = match area.height {
        h if h >= 12 => 3,
        h if h >= 8 => 2,
        h if h >= 5 => 1,
        _ => 0,
    };
    // With no room for the band, the header carries the icon.
    let leak_in_header = !snap.leaks.is_empty() && max_warnings == 0;
    header_line(
        f,
        app,
        &snap,
        theme,
        Rect::new(area.x, y, area.width, 1),
        leak_in_header,
    );
    y += 1;
    left -= 1;
    if !in_detail && area.height >= 14 {
        let line = Line::from(Span::styled(
            truncate_right(&info_text(&snap), area.width as usize),
            theme.dim,
        ));
        f.render_widget(Paragraph::new(line), Rect::new(area.x, y, area.width, 1));
        y += 1;
        left -= 1;
    }
    let hint_rows = u16::from(density.hints && left >= 2);
    let mut band = None;
    let mut error_shown = false;
    let rows = max_warnings.min((left - hint_rows) as usize);
    let mut all = warnings(app, &snap, theme, area.width as usize);
    // A cleared warning that has gone shows again if it comes back.
    app.cleared
        .retain(|k| all.iter().any(|w| w.key.as_ref() == Some(k)));
    all.retain(|w| !w.key.as_ref().is_some_and(|k| app.cleared.contains(k)));
    app.clearable = all.iter().filter_map(|w| w.key.clone()).collect();
    for w in &mut all {
        if let Some(k) = &w.key {
            w.action = Some(Action::Clear(vec![k.clone()]));
        }
    }
    let shown = fit_warnings(all, rows);
    // Rows the warnings leave free let a long one wrap.
    let mut spare = rows - shown.len();
    for w in shown {
        let lines = match &w.text {
            Some((text, style)) => {
                let max = 1 + spare.min(WARNING_LINES - 1);
                let lines = warning_lines(text, *style, area.width as usize, max);
                spare -= lines.len() - 1;
                lines
            }
            None => vec![fit_line(w.line, area.width as usize)],
        };
        let r = Rect::new(area.x, y, area.width, lines.len() as u16);
        f.render_widget(Paragraph::new(lines).style(w.fill), r);
        if let Some(action) = w.action {
            if matches!(action, Action::Open(_)) {
                band = Some(r);
            }
            app.hits.clicks.push((r, action));
        }
        error_shown |= w.error;
        y += r.height;
        left -= r.height;
    }

    let body = Rect::new(area.x, y, area.width, left - hint_rows);
    let selected = if in_detail {
        detail::draw(f, app, theme, body, density)
    } else {
        dashboard::draw(f, app, &snap, theme, body, density)
    };
    // Stale, and saying why; dimmed without the reason, it would only
    // look broken.
    if error_shown {
        f.buffer_mut()
            .set_style(body, Style::new().add_modifier(Modifier::DIM));
    }
    if hint_rows == 1 {
        hint_bar(
            f,
            app,
            theme,
            Rect::new(area.x, body.y + body.height, area.width, 1),
        );
    }
    // The hover tint would hide the band's red.
    if let Some(r) = app
        .hovered()
        .filter(|r| Some(*r) != selected && Some(*r) != band)
    {
        f.buffer_mut().set_style(r, theme.hover);
    }
    if app.help {
        detail::draw_help(f, app, theme, area);
    }
}

/// `rel_age` of a unix time as of `app.now`, noting when it next changes.
pub(crate) fn age_of(app: &App, unix: i64) -> String {
    let since = app.now.duration_since(UNIX_EPOCH).unwrap_or_default();
    age(app, since.as_secs() as i64 - unix, since.subsec_nanos())
}

/// `rel_age` of a measured time, noting when it next changes.
pub(crate) fn age_for(app: &App, d: Duration) -> String {
    age(app, d.as_secs() as i64, d.subsec_nanos())
}

/// `rel_age(secs)`, bringing `app.age_wake` forward to when it next reads
/// differently. The age is `nanos` into its current second.
fn age(app: &App, secs: i64, nanos: u32) -> String {
    // A little after the change, so the redraw does not land just before it.
    let next = Duration::from_secs(age_changes_in(secs) as u64) + Duration::from_millis(10)
        - Duration::from_nanos(nanos.into());
    app.age_wake
        .set(Some(app.age_wake.get().map_or(next, |w| w.min(next))));
    rel_age(secs)
}

/// `1 possible secret`, `3 possible secrets`.
pub(crate) fn secrets_label(n: usize) -> String {
    format!("{n} possible secret{}", if n == 1 { "" } else { "s" })
}

/// The warning glyph in red.
pub(crate) fn leak_icon_span(theme: &Theme) -> Span<'static> {
    Span::styled(
        theme.leak_icon(),
        Style::new().fg(theme.err).add_modifier(Modifier::BOLD),
    )
}

/// Marks a row that holds a possible secret: the glyph and a space.
pub(crate) fn leak_marker(theme: &Theme) -> Vec<Span<'static>> {
    vec![leak_icon_span(theme), Span::raw(" ")]
}

/// ` ⚠ 3 possible secrets · .env, src/config.rs`, fitted to `w` cells: the
/// paths are cut first, then the text shortens to ` ⚠ 3 secrets`.
fn leak_band_text(leaks: &[Leak], icon: &str, w: usize) -> String {
    let n = leaks.len();
    let head = format!(" {icon} {}", secrets_label(n));
    let in_commits = leaks
        .iter()
        .filter(|l| matches!(l.source, LeakSource::Commit(_)))
        .count();
    let suffix = if leaks
        .iter()
        .any(|l| matches!(l.source, LeakSource::Pushed(_)))
    {
        "pushed".to_string()
    } else if in_commits > 0 {
        let s = if in_commits == 1 { "" } else { "s" };
        format!("{in_commits} in unpushed commit{s}")
    } else {
        let mut paths: Vec<&str> = Vec::new();
        for l in leaks {
            if !paths.contains(&l.path.as_str()) {
                paths.push(&l.path);
            }
        }
        paths.join(", ")
    };
    let full = format!("{head} · {suffix}");
    if width(&full) <= w {
        full
    } else if width(&head) + 5 <= w {
        truncate_right(&full, w)
    } else {
        let s = if n == 1 { "" } else { "s" };
        truncate_right(&format!(" {icon} {n} secret{s}"), w)
    }
}

fn branch_spans(snap: &Snapshot, theme: &Theme) -> Vec<Span<'static>> {
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let icon = if theme.nerd { "\u{e0a0} " } else { "" };
    let mut spans = match &snap.head {
        Head::Branch(b) => vec![Span::styled(format!("{icon}{b}"), bold.fg(theme.accent))],
        Head::Detached(s) => vec![Span::styled(format!("@{s}"), bold.fg(theme.warn))],
        Head::Unborn(b) => vec![
            Span::styled(format!("{icon}{b}"), bold.fg(theme.accent)),
            Span::styled(" (no commits)", theme.dim),
        ],
    };
    if let Some(op) = &snap.op {
        let label = match op {
            RepoOp::Rebase {
                step: Some(s),
                total: Some(t),
            } => format!("rebase {s}/{t}"),
            RepoOp::Rebase { .. } => "rebase".into(),
            RepoOp::Merge => "merging".into(),
            RepoOp::CherryPick => "cherry-pick".into(),
            RepoOp::Revert => "reverting".into(),
            RepoOp::Bisect => "bisecting".into(),
        };
        spans.push(Span::raw(" "));
        spans.push(Span::styled(label, bold.fg(theme.warn)));
    }
    if let Some(u) = &snap.upstream {
        let count = |glyph: &str, n: u32, color| {
            let style = if n == 0 {
                theme.dim
            } else {
                Style::new().fg(color)
            };
            Span::styled(format!("{glyph}{n}"), style)
        };
        spans.push(Span::raw(" "));
        spans.push(count("↑", u.ahead, theme.accent));
        spans.push(Span::raw(" "));
        spans.push(count("↓", u.behind, theme.warn));
    }
    if snap.is_dirty() {
        spans.push(Span::styled(" ●", Style::new().fg(theme.modified)));
    }
    spans
}

fn header_line(f: &mut Frame, app: &mut App, snap: &Snapshot, theme: &Theme, r: Rect, leak: bool) {
    let w = r.width as usize;
    let mut right: Vec<Span<'static>> = Vec::new();
    if snap.has_remote {
        let label = if app.fetch.running {
            let millis = app
                .now
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_millis());
            format!("{} fetch", SPINNER[(millis / 100) as usize % SPINNER.len()])
        } else {
            match snap.last_fetch.and_then(|t| app.now.duration_since(t).ok()) {
                Some(age) => format!("↻ {}", age_for(app, age)),
                None if snap.last_fetch.is_some() => "↻ now".into(),
                None => "↻ –".into(),
            }
        };
        right.push(Span::styled(label, Style::new().fg(theme.accent)));
        right.push(Span::raw(" "));
    }
    let mut left: Vec<Span<'static>> = vec![Span::raw(" ")];
    if let Some(view) = app.stack.last() {
        left.push(Span::styled(
            "‹ back",
            Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
        ));
        left.push(Span::raw("  "));
        left.push(Span::styled(
            match view.target {
                Target::Leaks => secrets_label(snap.leaks.len()),
                _ => view.target.title(),
            },
            Style::new().add_modifier(Modifier::BOLD),
        ));
        app.hits
            .clicks
            .push((Rect::new(r.x, r.y, 7.min(r.width), 1), Action::Back));
    } else {
        let branch = branch_spans(snap, theme);
        // The branch matters more, so the name goes first when room is short.
        if let Some(name) = &app.repo_name {
            let needed = 1 + width(name) + 3 + spans_width(&branch) + 1 + spans_width(&right);
            if needed <= w {
                left.push(Span::styled(
                    name.clone(),
                    Style::new().add_modifier(Modifier::BOLD),
                ));
                left.push(Span::styled(" · ", theme.dim));
            }
        }
        left.extend(branch);
    }
    if leak {
        left.push(Span::raw(" "));
        left.push(leak_icon_span(theme));
    }
    let right_w = spans_width(&right);
    if right_w > 0 && right_w + 8 <= w {
        let x = r.x + (w - right_w) as u16;
        app.hits
            .clicks
            .push((Rect::new(x, r.y, right_w as u16, 1), Action::Fetch));
    } else {
        right.clear();
    }
    f.render_widget(Paragraph::new(line_lr(left, right, w)).style(theme.band), r);
}

/// Upstream, nearest tag and stash count: ` origin/main · v0.1.9+4 · stash 1`.
fn info_text(snap: &Snapshot) -> String {
    let mut parts = Vec::new();
    match (&snap.head, &snap.upstream) {
        (_, Some(u)) => parts.push(u.name.clone()),
        (Head::Branch(_), None) => parts.push("no upstream".into()),
        _ => {}
    }
    if let Some(t) = &snap.tag {
        parts.push(if t.distance == 0 {
            t.name.clone()
        } else {
            format!("{}+{}", t.name, t.distance)
        });
    }
    if snap.stash_count > 0 {
        parts.push(format!("stash {}", snap.stash_count));
    }
    format!(" {}", parts.join(" · "))
}

/// A row under the header.
struct Warning {
    line: Line<'static>,
    /// Style for the whole row.
    fill: Style,
    action: Option<Action>,
    /// What a shared row says for it; `None` for the secrets band, which
    /// keeps a row of its own.
    text: Option<(String, Style)>,
    /// The refresh error: the body below is stale.
    error: bool,
    /// What clearing it remembers; `None` for the secrets band and the
    /// refresh error, which cannot be cleared.
    key: Option<String>,
}

/// Fits `warnings` into `rows`: when there are more, the last row carries
/// all that are left, joined, so that none is dropped without a trace.
fn fit_warnings(mut warnings: Vec<Warning>, rows: usize) -> Vec<Warning> {
    if warnings.len() <= rows || rows == 0 {
        warnings.truncate(rows);
        return warnings;
    }
    let rest = warnings.split_off(rows - 1);
    match rest.first() {
        // The band comes first and alone takes the only row.
        Some(w) if w.text.is_none() => warnings.extend(rest.into_iter().take(1)),
        Some(_) => {
            let style = rest[0].text.as_ref().map_or(Style::new(), |t| t.1);
            let joined: Vec<&str> = rest
                .iter()
                .filter_map(|w| w.text.as_ref().map(|t| t.0.as_str()))
                .collect();
            let keys: Vec<String> = rest.iter().filter_map(|w| w.key.clone()).collect();
            warnings.push(Warning {
                line: Line::from(Span::styled(format!(" ⚠ {}", joined.join(" · ")), style)),
                fill: Style::new(),
                action: (!keys.is_empty()).then_some(Action::Clear(keys)),
                text: None,
                error: rest.iter().any(|w| w.error),
                key: None,
            });
        }
        None => {}
    }
    warnings
}

/// `text` after the warning glyph, wrapped onto at most `max` rows of
/// `cols` cells, the later rows lined up under the first.
fn warning_lines(text: &str, style: Style, cols: usize, max: usize) -> Vec<Line<'static>> {
    wrap_words(text, cols.saturating_sub(3), max)
        .into_iter()
        .enumerate()
        .map(|(i, part)| {
            let lead = if i == 0 { " ⚠ " } else { "   " };
            fit_line(
                Line::from(Span::styled(format!("{lead}{part}"), style)),
                cols,
            )
        })
        .collect()
}

fn warnings(app: &App, snap: &Snapshot, theme: &Theme, cols: usize) -> Vec<Warning> {
    let err = Style::new().fg(theme.err).add_modifier(Modifier::BOLD);
    let warn = Style::new().fg(theme.warn);
    let line = |text: String, style: Style| Warning {
        line: Line::from(Span::styled(format!(" ⚠ {text}"), style)),
        fill: Style::new(),
        action: None,
        key: Some(text.clone()),
        text: Some((text, style)),
        error: false,
    };
    let mut out = Vec::new();
    // First, so that no other warning can push it off a small pane.
    if !snap.leaks.is_empty() {
        out.push(Warning {
            line: Line::from(leak_band_text(&snap.leaks, theme.leak_icon(), cols)),
            fill: theme.leak,
            action: Some(Action::Open(Target::Leaks)),
            text: None,
            error: false,
            key: None,
        });
    }
    // Next, so that it is never what a shared row cuts off.
    if let Some(e) = &app.error {
        out.push(Warning {
            error: true,
            key: None,
            ..line(e.clone(), err)
        });
    }
    if let Some(e) = &snap.leak_scan_error {
        out.push(line(format!("secret scan incomplete · {e}"), warn));
    }
    if let Some(e) = &snap.config_error {
        out.push(line(format!("repo config not read · {e}"), warn));
    }
    let conflicts = snap.changes.iter().filter(|c| c.conflicted()).count();
    if conflicts > 0 {
        let s = if conflicts == 1 { "" } else { "s" };
        out.push(line(format!("{conflicts} conflict{s}"), err));
    }
    if let Some(age) = app.lock_age().filter(|a| *a >= STALE_LOCK) {
        // Keyed without the age, which changes each second.
        out.push(Warning {
            key: Some("index.lock".into()),
            ..line(format!("index.lock held {}", age_for(app, age)), warn)
        });
    }
    if let Some(e) = &app.fetch.last_error {
        out.push(line(format!("fetch failed: {}", e.label()), warn));
    }
    if let Some(w) = &app.config_warning {
        out.push(line(w.clone(), warn));
    }
    if let Some(n) = snap.leak_scan_skipped {
        out.push(line(format!("secret scan skipped · {n} changes"), warn));
    }
    out
}

fn hint_bar(f: &mut Frame, app: &mut App, theme: &Theme, r: Rect) {
    let mut items: Vec<(&str, &str, Action)> = if app.stack.is_empty() {
        vec![
            ("?", "help", Action::Help),
            ("f", "fetch", Action::Fetch),
            ("q", "quit", Action::Quit),
        ]
    } else {
        vec![
            ("esc", "back", Action::Back),
            ("w", "wrap", Action::ToggleWrap),
            ("q", "quit", Action::Quit),
        ]
    };
    if !app.clearable.is_empty() {
        items.insert(2, ("c", "clear", Action::Clear(app.clearable.clone())));
    }
    let key_style = Style::new().fg(theme.accent).add_modifier(Modifier::BOLD);
    let mut spans = Vec::new();
    let mut x = 0usize;
    for (key, label, action) in &items {
        let item_w = 1 + width(key) + 1 + width(label) + 1;
        if x + item_w > r.width as usize {
            break;
        }
        spans.push(Span::raw(" "));
        spans.push(Span::styled(key.to_string(), key_style));
        spans.push(Span::styled(format!(" {label} "), theme.dim));
        app.hits.clicks.push((
            Rect::new(r.x + x as u16, r.y, item_w as u16, 1),
            action.clone(),
        ));
        x += item_w;
    }
    f.render_widget(Paragraph::new(Line::from(spans)), r);
}

fn draw_tiny(f: &mut Frame, area: Rect, snap: &Snapshot, theme: &Theme) {
    let mut spans: Vec<Span<'static>> = match &snap.head {
        Head::Branch(b) | Head::Unborn(b) => {
            vec![Span::styled(b.clone(), Style::new().fg(theme.accent))]
        }
        Head::Detached(s) => vec![Span::styled(format!("@{s}"), Style::new().fg(theme.warn))],
    };
    if let Some(u) = &snap.upstream {
        if u.ahead > 0 {
            spans.push(Span::styled(
                format!("↑{}", u.ahead),
                Style::new().fg(theme.accent),
            ));
        }
        if u.behind > 0 {
            spans.push(Span::styled(
                format!("↓{}", u.behind),
                Style::new().fg(theme.warn),
            ));
        }
    }
    if snap.is_dirty() {
        spans.push(Span::styled("●", Style::new().fg(theme.modified)));
    }
    if !snap.leaks.is_empty() {
        spans.push(leak_icon_span(theme));
    }
    let r = Rect::new(area.x, area.y, area.width, 1);
    f.render_widget(
        Paragraph::new(fit_line(Line::from(spans), area.width as usize)),
        r,
    );
}

pub(crate) fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|s| width(&s.content)).sum()
}

/// Cuts spans so their total width is at most `max`, marking the cut with `…`.
pub(crate) fn truncate_spans(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
    if spans_width(&spans) <= max {
        return spans;
    }
    let mut out = Vec::new();
    let mut used = 0;
    for s in spans {
        let w = width(&s.content);
        if used + w <= max.saturating_sub(1) {
            used += w;
            out.push(s);
        } else {
            let text = truncate_right(&s.content, max - used);
            out.push(Span::styled(text, s.style));
            break;
        }
    }
    out
}

pub(crate) fn fit_line(line: Line<'static>, max: usize) -> Line<'static> {
    Line::from(truncate_spans(line.spans, max))
}

/// `left` flush left and `right` flush right within `width` cells; `left`
/// is cut if both do not fit.
pub(crate) fn line_lr(
    left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
    width: usize,
) -> Line<'static> {
    let rw = spans_width(&right);
    let right = if rw >= width { Vec::new() } else { right };
    let rw = spans_width(&right);
    let gap_min = usize::from(rw > 0);
    let mut spans = truncate_spans(left, width.saturating_sub(rw + gap_min));
    let pad = width.saturating_sub(spans_width(&spans) + rw);
    spans.push(Span::raw(" ".repeat(pad)));
    spans.extend(right);
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Leak, LeakSource};

    fn leak(path: &str, source: LeakSource) -> Leak {
        Leak {
            rule: "env-file",
            label: ".env file",
            path: path.into(),
            line: None,
            snippet: None,
            source,
        }
    }

    #[test]
    fn band_text_names_paths_commits_or_pushed() {
        let wt = [
            leak(".env", LeakSource::Untracked),
            leak("src/a.rs", LeakSource::Staged),
            leak(".env", LeakSource::Unstaged),
        ];
        assert_eq!(
            leak_band_text(&wt, "⚠", 80),
            " ⚠ 3 possible secrets · .env, src/a.rs"
        );
        assert_eq!(
            leak_band_text(&wt[..1], "⚠", 80),
            " ⚠ 1 possible secret · .env"
        );
        let c = [
            leak(".env", LeakSource::Untracked),
            leak("k", LeakSource::Commit("c1".into())),
        ];
        assert_eq!(
            leak_band_text(&c, "⚠", 80),
            " ⚠ 2 possible secrets · 1 in unpushed commit"
        );
        let p = [
            leak("k", LeakSource::Commit("c1".into())),
            leak("k", LeakSource::Pushed("c0".into())),
        ];
        assert_eq!(
            leak_band_text(&p, "⚠", 80),
            " ⚠ 2 possible secrets · pushed"
        );
    }

    #[test]
    fn band_text_shortens_to_fit() {
        let wt = [
            leak(".env", LeakSource::Untracked),
            leak("src/a.rs", LeakSource::Staged),
        ];
        let t = leak_band_text(&wt, "⚠", 30);
        assert!(
            t.starts_with(" ⚠ 2 possible secrets · ") && t.ends_with('…') && width(&t) <= 30,
            "{t}"
        );
        assert_eq!(leak_band_text(&wt, "⚠", 20), " ⚠ 2 secrets");
        assert!(width(&leak_band_text(&wt, "⚠", 5)) <= 5);
    }
}
