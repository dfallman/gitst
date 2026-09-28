pub mod dashboard;
pub mod detail;
pub mod fmt;
pub mod layout;
pub mod theme;

use std::time::UNIX_EPOCH;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{Action, App, STALE_LOCK};
use crate::model::{Head, RepoOp, Snapshot};
use fmt::{rel_age, truncate_right, width};
use layout::Density;
use theme::Theme;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Draws the whole UI and records the hit map for the next input.
pub fn draw(f: &mut Frame, app: &mut App, theme: &Theme) {
    app.hits.clear();
    app.nav.clear();
    app.row_targets.clear();
    app.detail_targets.clear();
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
    if density.tiny {
        draw_tiny(f, area, &snap, theme);
        if app.help {
            detail::draw_short_help(f, theme, area);
        }
        return;
    }

    let in_detail = !app.stack.is_empty();
    let mut y = area.y;
    let mut left = area.height;
    header_line(f, app, &snap, theme, Rect::new(area.x, y, area.width, 1));
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
    let max_warnings = match area.height {
        h if h >= 8 => 2,
        h if h >= 5 => 1,
        _ => 0,
    };
    for line in warnings(app, &snap, theme)
        .into_iter()
        .take(max_warnings.min((left - hint_rows) as usize))
    {
        f.render_widget(
            Paragraph::new(fit_line(line, area.width as usize)),
            Rect::new(area.x, y, area.width, 1),
        );
        y += 1;
        left -= 1;
    }

    let body = Rect::new(area.x, y, area.width, left - hint_rows);
    let selected = if in_detail {
        detail::draw(f, app, theme, body, density)
    } else {
        dashboard::draw(f, app, &snap, theme, body, density)
    };
    if app.error.is_some() {
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
    if let Some((hx, hy)) = app.hover {
        let hovered = app
            .hits
            .clicks
            .iter()
            .map(|(r, _)| *r)
            .find(|r| r.contains((hx, hy).into()));
        if let Some(r) = hovered.filter(|r| Some(*r) != selected) {
            f.buffer_mut().set_style(r, theme.hover);
        }
    }
    if app.help {
        detail::draw_help(f, app, theme, area);
    }
}

fn now_secs(app: &App) -> i64 {
    app.now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
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

fn header_line(f: &mut Frame, app: &mut App, snap: &Snapshot, theme: &Theme, r: Rect) {
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
                Some(age) => format!("↻ {}", rel_age(age.as_secs() as i64)),
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
            view.target.title(),
            Style::new().add_modifier(Modifier::BOLD),
        ));
        app.hits
            .clicks
            .push((Rect::new(r.x, r.y, 7.min(r.width), 1), Action::Back));
    } else {
        left.extend(branch_spans(snap, theme));
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

fn warnings(app: &App, snap: &Snapshot, theme: &Theme) -> Vec<Line<'static>> {
    let err = Style::new().fg(theme.err).add_modifier(Modifier::BOLD);
    let warn = Style::new().fg(theme.warn);
    let line = |text: String, style: Style| Line::from(Span::styled(format!(" ⚠ {text}"), style));
    let mut out = Vec::new();
    if let Some(e) = &app.error {
        out.push(line(e.clone(), err));
    }
    let conflicts = snap.changes.iter().filter(|c| c.conflicted()).count();
    if conflicts > 0 {
        let s = if conflicts == 1 { "" } else { "s" };
        out.push(line(format!("{conflicts} conflict{s}"), err));
    }
    if let Some(age) = app.lock_age().filter(|a| *a >= STALE_LOCK) {
        out.push(line(
            format!("index.lock held {}", rel_age(age.as_secs() as i64)),
            warn,
        ));
    }
    if let Some(e) = &app.fetch.last_error {
        out.push(line(format!("fetch failed: {}", e.label()), warn));
    }
    if let Some(w) = &app.config_warning {
        out.push(line(w.clone(), warn));
    }
    out
}

fn hint_bar(f: &mut Frame, app: &mut App, theme: &Theme, r: Rect) {
    let items: &[(&str, &str, Action)] = if app.stack.is_empty() {
        &[
            ("?", "help", Action::Help),
            ("f", "fetch", Action::Fetch),
            ("q", "quit", Action::Quit),
        ]
    } else {
        &[
            ("esc", "back", Action::Back),
            ("w", "wrap", Action::ToggleWrap),
            ("q", "quit", Action::Quit),
        ]
    };
    let key_style = Style::new().fg(theme.accent).add_modifier(Modifier::BOLD);
    let mut spans = Vec::new();
    let mut x = 0usize;
    for (key, label, action) in items {
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
