//! The stacked, collapsible dashboard.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};

use super::fmt::{clock, rel_age, truncate_left};
use super::layout::{Density, SectionId, SectionReq, Slot, allocate};
use super::theme::Theme;
use super::{leak_marker, line_lr, now_secs, spans_width, truncate_spans};
use crate::activity::{ActivityKind, merged};
use crate::app::{Action, App, NavItem, ScrollTarget, Target};
use crate::model::{Change, Commit, Counts, Snapshot};

/// Most activity rows kept in the section.
const ACTIVITY_ROWS: usize = 100;
const METER_CELLS: usize = 5;

struct Row {
    line: Line<'static>,
    target: Option<Target>,
}

struct Section {
    id: SectionId,
    count: Option<usize>,
    summary: Vec<Span<'static>>,
    rows: Vec<Row>,
}

/// Draws the sections into `area`; returns the rect of the selected item.
pub fn draw(
    f: &mut Frame,
    app: &mut App,
    snap: &Snapshot,
    theme: &Theme,
    area: Rect,
    d: Density,
) -> Option<Rect> {
    if area.height == 0 {
        return None;
    }
    let inner_w = if d.borders {
        area.width.saturating_sub(2)
    } else {
        area.width
    } as usize;
    let sections: Vec<Section> = SectionId::ALL
        .into_iter()
        .filter(|id| match id {
            SectionId::Branches => !snap.branches.is_empty(),
            SectionId::Stashes => !snap.stashes.is_empty(),
            _ => true,
        })
        .map(|id| build(id, snap, app, theme, inner_w, d))
        .collect();
    let reqs: Vec<SectionReq> = sections
        .iter()
        .map(|s| SectionReq {
            id: s.id,
            folded: app.folded.contains(&s.id),
            content: s.rows.len(),
        })
        .collect();
    let overhead = if d.borders { 2 } else { 1 };
    let slots = allocate(area.height as usize, overhead, &reqs);

    let mut y = area.y;
    let mut selected_rect = None;
    for (section, slot) in sections.into_iter().zip(slots) {
        let expanded_rows = match slot {
            Slot::Hidden => continue,
            Slot::Collapsed => None,
            Slot::Expanded { rows } => Some(rows),
        };
        let id = section.id;
        let title_item = NavItem {
            section: id,
            row: None,
        };
        app.nav.push(title_item);
        let Some(rows) = expanded_rows else {
            let r = Rect::new(area.x, y, area.width, 1);
            let lead = if d.borders { "  " } else { " " };
            let mut left = vec![Span::raw(lead)];
            left.extend(title_spans(&section, false, theme));
            let line = line_lr(left, pad_right(section.summary), area.width as usize);
            f.render_widget(Paragraph::new(line), r);
            app.hits.clicks.push((r, Action::Toggle(id)));
            if app.selected == Some(title_item) {
                f.buffer_mut().set_style(r, theme.select);
                selected_rect = Some(r);
            }
            y += 1;
            continue;
        };

        let (title_rect, body) = if d.borders {
            let outer = Rect::new(area.x, y, area.width, rows as u16 + 2);
            let mut title = vec![Span::raw(" ")];
            title.extend(title_spans(&section, true, theme));
            title.push(Span::raw(" "));
            let mut block = Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(theme.border)
                .title(Line::from(title));
            if !section.summary.is_empty() {
                let mut summary = vec![Span::raw(" ")];
                summary.extend(section.summary.clone());
                summary.push(Span::raw(" "));
                block = block.title_top(Line::from(summary).right_aligned());
            }
            let body = block.inner(outer);
            f.render_widget(block, outer);
            y += rows as u16 + 2;
            (Rect::new(area.x, outer.y, area.width, 1), body)
        } else {
            let r = Rect::new(area.x, y, area.width, 1);
            let mut left = vec![Span::raw(" ")];
            left.extend(title_spans(&section, true, theme));
            left.push(Span::raw(" "));
            let summary = pad_right(section.summary.clone());
            let rule = (area.width as usize)
                .saturating_sub(spans_width(&left) + spans_width(&summary) + 1);
            left.push(Span::styled("─".repeat(rule), theme.border));
            left.push(Span::raw(" "));
            f.render_widget(
                Paragraph::new(line_lr(left, summary, area.width as usize)),
                r,
            );
            y += 1 + rows as u16;
            (r, Rect::new(area.x, r.y + 1, area.width, rows as u16))
        };
        app.hits.clicks.push((title_rect, Action::Toggle(id)));
        if app.selected == Some(title_item) {
            f.buffer_mut().set_style(title_rect, theme.select);
            selected_rect = Some(title_rect);
        }
        app.hits.scrolls.push((body, ScrollTarget::Section(id)));

        for (i, row) in section.rows.iter().enumerate() {
            if let Some(t) = &row.target {
                app.nav.push(NavItem {
                    section: id,
                    row: Some(i),
                });
                app.row_targets.insert((id, i), t.clone());
            }
        }
        if let Some(NavItem {
            section: s,
            row: Some(r),
        }) = app.selected
            && s == id
        {
            let row = follow_selection(&section.rows, r, app.selected_target.as_ref());
            app.selected = Some(NavItem { section: id, row });
            app.selected_target = row.and_then(|i| section.rows[i].target.clone());
        }
        let selected_row = match app.selected {
            Some(NavItem {
                section,
                row: Some(r),
            }) if section == id => Some(r),
            _ => None,
        };
        let scroll = app.scroll.entry(id).or_insert(0);
        let (start, shown, more) = visible_window(section.rows.len(), rows, scroll, selected_row);
        for (i, row) in section.rows.iter().enumerate().skip(start).take(shown) {
            let r = Rect::new(body.x, body.y + (i - start) as u16, body.width, 1);
            f.render_widget(Paragraph::new(row.line.clone()), r);
            if let Some(t) = &row.target {
                app.hits.clicks.push((r, Action::Open(t.clone())));
            }
            if selected_row == Some(i) {
                f.buffer_mut().set_style(r, theme.select);
                selected_rect = Some(r);
            }
        }
        if more > 0 {
            let r = Rect::new(body.x, body.y + shown as u16, body.width, 1);
            f.render_widget(
                Paragraph::new(Span::styled(format!(" +{more} more"), theme.dim)),
                r,
            );
        }
    }
    selected_rect
}

/// Where a selected row is after the rows changed: the row that opens the
/// same thing; else, once that is gone, the row now in its place (or the
/// nearest selectable one above it); else the section title (`None`).
fn follow_selection(rows: &[Row], old: usize, target: Option<&Target>) -> Option<usize> {
    if let Some(t) = target
        && let Some(i) = rows.iter().position(|r| r.target.as_ref() == Some(t))
    {
        return Some(i);
    }
    let last = rows.len().checked_sub(1)?;
    (0..=old.min(last))
        .rev()
        .find(|i| rows[*i].target.is_some())
}

/// Which rows of a section show: `(first, count, hidden_below)`. When rows
/// remain below, the last slot shows `+N more` instead. Keeps `selected` in
/// view and clamps `scroll`.
fn visible_window(
    total: usize,
    slots: usize,
    scroll: &mut usize,
    selected: Option<usize>,
) -> (usize, usize, usize) {
    if total <= slots {
        *scroll = 0;
        return (0, total, 0);
    }
    let max_scroll = total - slots;
    *scroll = (*scroll).min(max_scroll);
    let capacity = |s: usize| {
        if s < max_scroll && slots >= 2 {
            slots - 1
        } else {
            slots
        }
    };
    if let Some(sel) = selected {
        if sel < *scroll {
            *scroll = sel;
        } else if sel >= *scroll + capacity(*scroll) {
            *scroll = (sel + 1).saturating_sub(slots - 1).min(max_scroll);
            if sel >= *scroll + capacity(*scroll) {
                *scroll = max_scroll;
            }
        }
    }
    let shown = capacity(*scroll);
    (*scroll, shown, total - *scroll - shown)
}

fn pad_right(mut spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    if !spans.is_empty() {
        spans.push(Span::raw(" "));
    }
    spans
}

fn title_spans(s: &Section, expanded: bool, theme: &Theme) -> Vec<Span<'static>> {
    let mut spans = vec![
        Span::styled(if expanded { "▾ " } else { "▸ " }, theme.dim),
        Span::styled(s.id.title(), Style::new().add_modifier(Modifier::BOLD)),
    ];
    if let Some(n) = s.count {
        spans.push(Span::styled(format!(" ({n})"), theme.dim));
    }
    spans
}

fn build(
    id: SectionId,
    snap: &Snapshot,
    app: &App,
    theme: &Theme,
    inner_w: usize,
    d: Density,
) -> Section {
    // Rows start with one space and keep one free cell at the right edge.
    let w = inner_w.saturating_sub(2);
    match id {
        SectionId::Changes => changes(snap, app, theme, w, d),
        SectionId::Activity => Section {
            id,
            count: None,
            summary: Vec::new(),
            rows: activity(snap, app, theme, w, d),
        },
        SectionId::Commits => {
            let unpushed = snap.commits.iter().filter(|c| c.unpushed).count();
            let summary = if unpushed > 0 {
                vec![Span::styled(
                    format!("↑{unpushed}"),
                    Style::new().fg(theme.accent),
                )]
            } else {
                Vec::new()
            };
            Section {
                id,
                count: None,
                summary,
                rows: commits(snap, app, theme, w, d),
            }
        }
        SectionId::Branches => Section {
            id,
            count: Some(snap.branches.len()),
            summary: Vec::new(),
            rows: branches(snap, app, theme, w, d),
        },
        SectionId::Stashes => Section {
            id,
            count: Some(snap.stashes.len()),
            summary: Vec::new(),
            rows: stashes(snap, app, theme, w, d),
        },
    }
}

/// A row of `left`, a flexible `middle` cut to fit, and `right` flush right,
/// all within `w` cells after a one-space indent.
pub(crate) fn row(
    left: Vec<Span<'static>>,
    middle: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
    w: usize,
) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    let fixed = spans_width(&left) + spans_width(&right) + usize::from(!right.is_empty());
    spans.extend(left);
    let middle = truncate_spans(middle, w.saturating_sub(fixed));
    let pad = w.saturating_sub(fixed + spans_width(&middle)) + usize::from(!right.is_empty());
    spans.extend(middle);
    spans.push(Span::raw(" ".repeat(pad)));
    spans.extend(right);
    Line::from(truncate_spans(spans, w + 1))
}

fn status_spans(c: &Change, theme: &Theme) -> Vec<Span<'static>> {
    if c.conflicted() {
        let s = Style::new().fg(theme.err).add_modifier(Modifier::BOLD);
        return vec![Span::styled(format!("{}{}", c.x, c.y), s)];
    }
    if c.untracked() {
        return vec![Span::styled("??", Style::new().fg(theme.untracked))];
    }
    let y_color = if c.y == 'D' {
        theme.del
    } else {
        theme.modified
    };
    vec![
        Span::styled(c.x.to_string(), Style::new().fg(theme.add)),
        Span::styled(c.y.to_string(), Style::new().fg(y_color)),
    ]
}

/// `+a −r`, `bin`, or nothing when the lines were not counted.
pub(crate) fn stats_spans(counts: Counts, theme: &Theme) -> Vec<Span<'static>> {
    match counts {
        Counts::Lines {
            added: a,
            removed: r,
        } => {
            let mut v = Vec::new();
            if a > 0 {
                v.push(Span::styled(format!("+{a}"), Style::new().fg(theme.add)));
            }
            if r > 0 {
                if a > 0 {
                    v.push(Span::raw(" "));
                }
                v.push(Span::styled(format!("−{r}"), Style::new().fg(theme.del)));
            }
            v
        }
        Counts::Binary => vec![Span::styled("bin", theme.dim)],
        Counts::Unknown => Vec::new(),
    }
}

/// Up to five cells, more for bigger changes, split green/red by ratio.
pub(crate) fn meter(counts: Counts, theme: &Theme) -> Vec<Span<'static>> {
    let (a, r) = counts.known().unwrap_or((0, 0));
    let (a, r) = (a as f64, r as f64);
    let total = a + r;
    let cells = if total == 0.0 {
        0
    } else {
        ((total + 1.0).log2().ceil() as usize).clamp(1, METER_CELLS)
    };
    let adds = if total == 0.0 {
        0
    } else {
        ((cells as f64) * a / total).round() as usize
    };
    vec![
        Span::styled("■".repeat(adds), Style::new().fg(theme.add)),
        Span::styled("■".repeat(cells - adds), Style::new().fg(theme.del)),
        Span::raw(" ".repeat(METER_CELLS - cells)),
    ]
}

fn changes(snap: &Snapshot, app: &App, theme: &Theme, w: usize, d: Density) -> Section {
    let id = SectionId::Changes;
    // Totals cover the changes whose lines were counted, if any were.
    let total = snap
        .changes
        .iter()
        .filter_map(|c| c.counts.known())
        .reduce(|(a, r), (b, s)| (a.saturating_add(b), r.saturating_add(s)));
    let summary = match total {
        Some((a, r)) if d.stats => stats_spans(Counts::lines(a, r), theme),
        _ => Vec::new(),
    };
    let count = Some(snap.changes.len() + snap.changes_omitted).filter(|n| *n > 0);
    if snap.changes.is_empty() {
        let line = Line::from(Span::styled(" ✓ clean", Style::new().fg(theme.add)));
        return Section {
            id,
            count,
            summary,
            rows: vec![Row { line, target: None }],
        };
    }

    let stats_w = if d.stats {
        snap.changes
            .iter()
            .map(|c| spans_width(&stats_spans(c.counts, theme)))
            .max()
            .unwrap_or(0)
    } else {
        0
    };
    let flagged: HashSet<&str> = snap
        .leaks
        .iter()
        .filter(|l| l.source.oid().is_none())
        .map(|l| l.path.as_str())
        .collect();
    let ttl = Duration::from_secs(app.pulse_secs);
    let now = Instant::now();
    let mut rows: Vec<Row> = snap
        .changes
        .iter()
        .map(|c| {
            let mut left = status_spans(c, theme);
            left.push(Span::raw(" "));
            let mut right = Vec::new();
            let flag = flagged.contains(c.path.as_str());
            if d.stats {
                let stats = stats_spans(c.counts, theme);
                right.push(Span::raw(" ".repeat(stats_w - spans_width(&stats))));
                if flag {
                    right.extend(leak_marker(theme));
                }
                right.extend(stats);
                right.push(Span::raw(" "));
                right.extend(meter(c.counts, theme));
            } else if flag {
                right.extend(leak_marker(theme));
            }
            let pulse = match app.pulses.get(&c.path).map(|t| now.duration_since(*t)) {
                Some(age) if age < ttl / 2 => Span::styled(
                    "•",
                    Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
                ),
                Some(age) if age < ttl => Span::styled(
                    "•",
                    Style::new().fg(theme.accent).add_modifier(Modifier::DIM),
                ),
                _ => Span::raw(" "),
            };
            right.push(Span::raw(" "));
            right.push(pulse);
            let display = match &c.orig_path {
                Some(orig) => format!("{orig} → {}", c.path),
                None => c.path.clone(),
            };
            let path_w = w.saturating_sub(spans_width(&left) + spans_width(&right) + 1);
            let middle = vec![Span::raw(truncate_left(&display, path_w))];
            Row {
                line: row(left, middle, right, w),
                target: Some(Target::File(c.path.clone())),
            }
        })
        .collect();
    if snap.changes_omitted > 0 {
        let line = Line::from(Span::styled(
            format!(" … {} more files", snap.changes_omitted),
            theme.dim,
        ));
        rows.push(Row { line, target: None });
    }
    Section {
        id,
        count,
        summary,
        rows,
    }
}

fn activity(snap: &Snapshot, app: &App, theme: &Theme, w: usize, d: Density) -> Vec<Row> {
    let events = merged(&snap.reflog, &app.live, ACTIVITY_ROWS);
    if events.is_empty() {
        return vec![Row {
            line: Line::from(Span::styled(" no activity yet", theme.dim)),
            target: None,
        }];
    }
    events
        .into_iter()
        .map(|e| {
            let color = match e.kind {
                ActivityKind::Commit | ActivityKind::Amend | ActivityKind::Stage => Some(theme.add),
                ActivityKind::Push | ActivityKind::Fetch | ActivityKind::Pull => Some(theme.accent),
                ActivityKind::Checkout => Some(theme.untracked),
                ActivityKind::Merge
                | ActivityKind::Rebase
                | ActivityKind::CherryPick
                | ActivityKind::Stash => Some(theme.modified),
                ActivityKind::Reset => Some(theme.warn),
                ActivityKind::Files | ActivityKind::Other => None,
            };
            let (verb, rest) = e.text.split_once(' ').unwrap_or((e.text.as_str(), ""));
            let verb_style = match color {
                Some(c) => Style::new().fg(c),
                None => Style::new(),
            };
            let left = vec![Span::styled(clock(e.time), theme.dim), Span::raw(" ")];
            let middle = vec![
                Span::styled(verb.to_string(), verb_style),
                Span::raw(format!(" {rest}")),
            ];
            Row {
                line: row(left, middle, age_span(e.time, app, theme, d), w),
                target: e.rev.map(Target::Commit).or_else(|| {
                    e.path
                        .filter(|p| snap.changes.iter().any(|c| &c.path == p))
                        .map(Target::File)
                }),
            }
        })
        .collect()
}

fn age_span(time: i64, app: &App, theme: &Theme, d: Density) -> Vec<Span<'static>> {
    if d.ages {
        vec![Span::styled(rel_age(now_secs(app) - time), theme.dim)]
    } else {
        Vec::new()
    }
}

fn commits(snap: &Snapshot, app: &App, theme: &Theme, w: usize, d: Density) -> Vec<Row> {
    if snap.commits.is_empty() {
        return vec![Row {
            line: Line::from(Span::styled(" no commits yet", theme.dim)),
            target: None,
        }];
    }
    snap.commits
        .iter()
        .map(|c| Row {
            line: commit_row(c, app, theme, w, d),
            target: Some(Target::Commit(c.oid.clone())),
        })
        .collect()
}

/// `4de1e3c ↑ subject [tag]   2m`
pub(crate) fn commit_row(
    c: &Commit,
    app: &App,
    theme: &Theme,
    w: usize,
    d: Density,
) -> Line<'static> {
    let marker = if c.unpushed {
        Span::styled("↑", Style::new().fg(theme.accent))
    } else if c.parents > 1 {
        Span::styled("◇", theme.dim)
    } else {
        Span::raw(" ")
    };
    let mut left = vec![
        Span::styled(c.short.clone(), Style::new().fg(theme.modified)),
        Span::raw(" "),
        marker,
        Span::raw(" "),
    ];
    let flagged = app.snap.as_ref().is_some_and(|s| {
        s.leaks
            .iter()
            .any(|l| l.source.oid() == Some(c.oid.as_str()))
    });
    if flagged {
        left.extend(leak_marker(theme));
    }
    let mut middle = vec![Span::raw(c.subject.clone())];
    if w >= 48 {
        for tag in c.refs.iter().filter_map(|r| r.strip_prefix("tag: ")) {
            middle.push(Span::styled(
                format!(" [{tag}]"),
                Style::new().fg(theme.accent),
            ));
        }
    }
    row(left, middle, age_span(c.time, app, theme, d), w)
}

fn branches(snap: &Snapshot, app: &App, theme: &Theme, w: usize, d: Density) -> Vec<Row> {
    snap.branches
        .iter()
        .map(|b| {
            let (dot, name_style) = if b.is_head {
                (
                    Span::styled("● ", Style::new().fg(theme.accent)),
                    Style::new().add_modifier(Modifier::BOLD),
                )
            } else {
                (Span::raw("  "), Style::new())
            };
            let mut right = Vec::new();
            if b.gone {
                right.push(Span::styled("gone", theme.dim));
            } else if b.upstream.is_none() {
                right.push(Span::styled("local", theme.dim));
            } else {
                if b.ahead > 0 {
                    right.push(Span::styled(
                        format!("↑{}", b.ahead),
                        Style::new().fg(theme.accent),
                    ));
                }
                if b.behind > 0 {
                    if !right.is_empty() {
                        right.push(Span::raw(" "));
                    }
                    right.push(Span::styled(
                        format!("↓{}", b.behind),
                        Style::new().fg(theme.warn),
                    ));
                }
            }
            let age = age_span(b.time, app, theme, d);
            if !age.is_empty() {
                if !right.is_empty() {
                    right.push(Span::raw(" "));
                }
                right.extend(age);
            }
            let middle = vec![Span::styled(b.name.clone(), name_style)];
            let upstream = if b.gone { None } else { b.upstream.clone() };
            Row {
                line: row(vec![dot], middle, right, w),
                target: Some(Target::Branch {
                    name: b.name.clone(),
                    upstream,
                }),
            }
        })
        .collect()
}

fn stashes(snap: &Snapshot, app: &App, theme: &Theme, w: usize, d: Density) -> Vec<Row> {
    snap.stashes
        .iter()
        .map(|s| {
            let left = vec![
                Span::styled(format!("{{{}}}", s.index), Style::new().fg(theme.accent)),
                Span::raw(" "),
            ];
            let middle = vec![Span::raw(s.message.clone())];
            Row {
                line: row(left, middle, age_span(s.time, app, theme, d), w),
                target: Some(Target::Stash(s.index)),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_fits_without_scrolling() {
        let mut s = 5;
        assert_eq!(visible_window(3, 4, &mut s, None), (0, 3, 0));
        assert_eq!(s, 0);
    }

    #[test]
    fn window_shows_more_indicator_until_bottom() {
        let mut s = 0;
        assert_eq!(visible_window(10, 4, &mut s, None), (0, 3, 7));
        s = 6;
        assert_eq!(visible_window(10, 4, &mut s, None), (6, 4, 0));
        s = 99;
        assert_eq!(visible_window(10, 4, &mut s, None), (6, 4, 0));
    }

    #[test]
    fn window_follows_selection() {
        let mut s = 0;
        let (start, shown, _) = visible_window(10, 4, &mut s, Some(5));
        assert!(start <= 5 && 5 < start + shown, "{start} {shown}");
        let (start, shown, _) = visible_window(10, 4, &mut s, Some(9));
        assert!(start <= 9 && 9 < start + shown);
        let (start, _, _) = visible_window(10, 4, &mut s, Some(1));
        assert_eq!(start, 1);
    }

    #[test]
    fn meter_scales_and_splits() {
        let t = Theme::ansi();
        let cells = |a, r| {
            meter(Counts::lines(a, r), &t)
                .iter()
                .map(|s| s.content.chars().filter(|c| *c == '■').count())
                .sum::<usize>()
        };
        assert_eq!(cells(0, 0), 0);
        assert_eq!(cells(1, 0), 1);
        assert_eq!(cells(42, 7), 5);
        assert_eq!(spans_width(&meter(Counts::lines(3, 1), &t)), METER_CELLS);
    }
}
