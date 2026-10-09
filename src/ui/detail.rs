//! Full-pane detail views and the help overlay.

use ratatui::Frame;
use ratatui::layout::{Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};

use super::dashboard::{commit_row, meter, row, stats_spans};
use super::fmt::{clock, clusters, rel_age, truncate_left, width};
use super::layout::Density;
use super::theme::Theme;
use super::{now_secs, spans_width};
use crate::app::{App, ScrollTarget, Target};
use crate::model::{
    Commit, CommitDetail, DetailData, DiffBlock, DiffKind, Leak, LeakSource, Snapshot,
};

type DetailLine = (Line<'static>, Option<Target>);

/// Draws the top detail view into `area`; returns the selected line's rect.
pub fn draw(f: &mut Frame, app: &mut App, theme: &Theme, area: Rect, d: Density) -> Option<Rect> {
    app.stack.last()?;
    if area.height == 0 {
        return None;
    }
    let height = area.height as usize;
    let mut text = area;
    let (lines, text_width) = fit(area.width, height, |w| view_lines(app, theme, w, d))?;
    text.width = text_width;
    app.page = height;
    app.detail_targets = lines.iter().map(|(_, t)| t.clone()).collect();
    let view = app.stack.last_mut()?;
    let max_scroll = lines.len().saturating_sub(height);
    if let Some(sel) = view.selected {
        if sel < view.scroll {
            view.scroll = sel;
        } else if sel >= view.scroll + height {
            view.scroll = sel + 1 - height;
        }
    }
    view.scroll = view.scroll.min(max_scroll);
    let (scroll, selected) = (view.scroll, view.selected);

    app.hits.scrolls.push((area, ScrollTarget::Detail));
    let mut selected_rect = None;
    for (i, (line, target)) in lines.iter().enumerate().skip(scroll).take(height) {
        let r = Rect::new(text.x, text.y + (i - scroll) as u16, text.width, 1);
        f.render_widget(Paragraph::new(line.clone()), r);
        if let Some(t) = target {
            app.hits
                .clicks
                .push((r, crate::app::Action::Open(t.clone())));
        }
        if selected == Some(i) {
            f.buffer_mut().set_style(r, theme.select);
            selected_rect = Some(r);
        }
    }
    if max_scroll > 0 {
        let mut state = ScrollbarState::new(max_scroll)
            .position(scroll)
            .viewport_content_length(height);
        let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("│"))
            .track_style(theme.border)
            .thumb_symbol("┃")
            .thumb_style(Style::new().fg(theme.accent));
        f.render_stateful_widget(
            bar,
            area.inner(Margin {
                vertical: 0,
                horizontal: 0,
            }),
            &mut state,
        );
    }
    selected_rect
}

/// Lays the view out for a pane `width` wide and `height` tall, and says
/// how wide the text is: one column less when the lines overflow, which a
/// scrollbar takes. Laid out without that column first: a view that
/// overflows, the heavy case, is then laid out once, and only a view that
/// fits, which is short, is laid out again at the full width.
fn fit(
    width: u16,
    height: usize,
    mut lines_at: impl FnMut(usize) -> Option<Vec<DetailLine>>,
) -> Option<(Vec<DetailLine>, u16)> {
    if width <= 1 {
        return Some((lines_at(width as usize)?, width));
    }
    let narrow = lines_at(width as usize - 1)?;
    if narrow.len() > height {
        return Some((narrow, width - 1));
    }
    Some((lines_at(width as usize)?, width))
}

/// The top detail view's lines, laid out `w` cells wide.
fn view_lines(app: &App, theme: &Theme, w: usize, d: Density) -> Option<Vec<DetailLine>> {
    let view = app.stack.last()?;
    let lines: Vec<DetailLine> = if view.target == Target::Leaks {
        leak_lines(app, theme, w)
    } else {
        match &view.data {
            None => vec![(Line::from(Span::styled(" loading…", theme.dim)), None)],
            Some(Err(e)) => vec![(
                Line::from(Span::styled(format!(" ⚠ {e}"), Style::new().fg(theme.err))),
                None,
            )],
            Some(Ok(DetailData::File(blocks) | DetailData::CommitFile(blocks))) => {
                diff_lines(blocks, theme, w, app.wrap)
            }
            Some(Ok(DetailData::Commit(c))) => commit_lines(c, app, theme, w, d),
            Some(Ok(DetailData::Branch {
                ahead,
                behind,
                ahead_total,
                behind_total,
            })) => {
                let upstream = match &view.target {
                    Target::Branch {
                        upstream: Some(u), ..
                    } => u.clone(),
                    _ => "upstream".into(),
                };
                let sides = [
                    ("ahead of", &ahead[..], *ahead_total),
                    ("behind", &behind[..], *behind_total),
                ];
                branch_lines(sides, &upstream, app, theme, w, d)
            }
        }
    };
    Some(lines)
}

/// Splits `s` into pieces of at most `max` cells, never inside a cluster.
fn wrap_text(s: &str, max: usize) -> Vec<String> {
    let max = max.max(1);
    let mut out = vec![String::new()];
    let mut used = 0;
    for (_, g, w) in clusters(s) {
        if used + w > max && used > 0 {
            out.push(String::new());
            used = 0;
        }
        out.last_mut().expect("non-empty").push_str(g);
        used += w;
    }
    out
}

fn rule(title: &str, w: usize, theme: &Theme) -> Line<'static> {
    let head = format!("── {title} ");
    let fill = "─".repeat(w.saturating_sub(width(&head)));
    Line::from(Span::styled(
        format!("{head}{fill}"),
        theme.border.add_modifier(Modifier::BOLD),
    ))
}

fn diff_lines(blocks: &[DiffBlock], theme: &Theme, w: usize, wrap: bool) -> Vec<DetailLine> {
    if blocks.is_empty() {
        return vec![(
            Line::from(Span::styled(" ✓ no changes", Style::new().fg(theme.add))),
            None,
        )];
    }
    let mut out = Vec::new();
    for block in blocks {
        out.push((rule(&block.title, w, theme), None));
        for l in &block.lines {
            let (marker, style) = match l.kind {
                DiffKind::Add => ("+", Style::new().fg(theme.add)),
                DiffKind::Del => ("-", Style::new().fg(theme.del)),
                DiffKind::Context => (" ", Style::new()),
                DiffKind::Hunk => {
                    let text = format!("{} ", l.text);
                    let fill = "╌".repeat(w.saturating_sub(width(&text)));
                    out.push((
                        Line::from(vec![
                            Span::styled(text, Style::new().fg(theme.accent)),
                            Span::styled(fill, theme.border),
                        ]),
                        None,
                    ));
                    continue;
                }
                DiffKind::Meta => ("", theme.dim.add_modifier(Modifier::ITALIC)),
            };
            let text = format!("{marker}{}", l.text);
            let pieces = if wrap {
                wrap_text(&text, w.saturating_sub(1))
            } else {
                vec![text]
            };
            for (i, piece) in pieces.into_iter().enumerate() {
                let piece = if i == 0 { piece } else { format!(" {piece}") };
                out.push((Line::from(Span::styled(piece, style)), None));
            }
        }
        out.push((Line::default(), None));
    }
    out.pop();
    out
}

fn commit_lines(
    c: &CommitDetail,
    app: &App,
    theme: &Theme,
    w: usize,
    d: Density,
) -> Vec<DetailLine> {
    let mut out: Vec<DetailLine> = Vec::new();
    let plain = |line: Line<'static>| (line, None);
    out.push(plain(Line::from(vec![
        Span::styled(" commit ", theme.dim),
        Span::styled(c.oid.clone(), Style::new().fg(theme.modified)),
    ])));
    let when = format!(
        " · {} ago · {}",
        rel_age(now_secs(app) - c.time),
        clock(c.time)
    );
    out.push(plain(Line::from(vec![
        Span::raw(format!(" {}", c.author)),
        Span::styled(when, theme.dim),
    ])));
    out.push(plain(Line::default()));
    for (i, text) in c.message.lines().enumerate() {
        let style = if i == 0 {
            Style::new().add_modifier(Modifier::BOLD)
        } else {
            Style::new()
        };
        let pieces = if app.wrap {
            wrap_text(text, w.saturating_sub(2))
        } else {
            vec![text.to_string()]
        };
        for piece in pieces {
            out.push(plain(Line::from(Span::styled(format!(" {piece}"), style))));
        }
    }
    out.push(plain(Line::default()));
    out.push((rule(&format!("{} files", c.files.len()), w, theme), None));
    let row_w = w.saturating_sub(2);
    for (path, counts) in &c.files {
        let mut right = Vec::new();
        if d.stats {
            right.extend(stats_spans(*counts, theme));
            right.push(Span::raw(" "));
            right.extend(meter(*counts, theme));
        }
        let path_w = row_w.saturating_sub(spans_width(&right) + 1);
        let line = row(
            Vec::new(),
            vec![Span::raw(truncate_left(path, path_w))],
            right,
            row_w,
        );
        out.push((
            line,
            Some(Target::CommitFile {
                rev: c.oid.clone(),
                path: path.clone(),
            }),
        ));
    }
    out
}

/// Commits ahead of and behind the upstream: `(label, listed, total)` per
/// side, where only the newest are listed.
fn branch_lines(
    sides: [(&str, &[Commit], usize); 2],
    upstream: &str,
    app: &App,
    theme: &Theme,
    w: usize,
    d: Density,
) -> Vec<DetailLine> {
    if sides.iter().all(|(_, commits, _)| commits.is_empty()) {
        let text = format!(" ✓ in sync with {upstream}");
        return vec![(
            Line::from(Span::styled(text, Style::new().fg(theme.add))),
            None,
        )];
    }
    let row_w = w.saturating_sub(2);
    let mut out = Vec::new();
    for (label, commits, total) in sides {
        if commits.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push((Line::default(), None));
        }
        let total = total.max(commits.len());
        out.push((
            rule(&format!("{label} {upstream} ({total})"), w, theme),
            None,
        ));
        for c in commits {
            out.push((
                commit_row(c, app, theme, row_w, d),
                Some(Target::Commit(c.oid.clone())),
            ));
        }
        if total > commits.len() {
            let more = format!(" … {} more", total - commits.len());
            out.push((Line::from(Span::styled(more, theme.dim)), None));
        }
    }
    out
}

/// Possible secrets, grouped by where they are; each opens its diff.
fn leak_lines(app: &App, theme: &Theme, w: usize) -> Vec<DetailLine> {
    let Some(snap) = app.snap.as_ref() else {
        return Vec::new();
    };
    if snap.leaks.is_empty() {
        return vec![(
            Line::from(Span::styled(
                " ✓ no possible secrets",
                Style::new().fg(theme.add),
            )),
            None,
        )];
    }
    let pushed = snap
        .leaks
        .iter()
        .any(|l| matches!(l.source, LeakSource::Pushed(_)));
    let (advice, style): (&[&str], Style) = if pushed {
        (
            &[
                " Already pushed: rotate the key.",
                " Rewriting history is not enough.",
            ],
            Style::new().fg(theme.err).add_modifier(Modifier::BOLD),
        )
    } else {
        (
            &[" Not pushed yet: amend or remove", " it before you push."],
            Style::new(),
        )
    };
    let mut out: Vec<DetailLine> = Vec::new();
    for text in advice {
        out.push((Line::from(Span::styled(*text, style)), None));
    }
    for text in [
        " False alarm? Add gitst:allow to",
        " the line, or a leak_allow glob.",
    ] {
        out.push((Line::from(Span::styled(text, theme.dim)), None));
    }
    let mut leaks: Vec<&Leak> = snap.leaks.iter().collect();
    leaks.sort_by_key(|l| group_rank(&l.source));
    let row_w = w.saturating_sub(2);
    let mut group = String::new();
    for l in leaks {
        let title = group_title(&l.source, snap);
        if title != group {
            out.push((Line::default(), None));
            out.push((rule(&title, w, theme), None));
            group = title;
        }
        let place = match l.line {
            Some(n) => format!("{}:{n}", l.path),
            None => l.path.clone(),
        };
        // The path matters most: the snippet, then the label, give way to it.
        let mut right = vec![Span::styled(l.label, theme.dim)];
        if let Some(s) = &l.snippet {
            right.push(Span::raw(" "));
            right.push(Span::styled(s.clone(), Style::new().fg(theme.err)));
        }
        while !right.is_empty() && width(&place) + 1 + spans_width(&right) > row_w {
            right.truncate(right.len().saturating_sub(2));
        }
        let path_w = row_w.saturating_sub(spans_width(&right) + 1);
        let line = row(
            Vec::new(),
            vec![Span::raw(truncate_left(&place, path_w))],
            right,
            row_w,
        );
        let target = match l.source.oid() {
            Some(rev) => Target::CommitFile {
                rev: rev.to_string(),
                path: l.path.clone(),
            },
            None => Target::File(l.path.clone()),
        };
        out.push((line, Some(target)));
    }
    out
}

/// Pushed first: those need a key rotated.
fn group_rank(s: &LeakSource) -> u8 {
    match s {
        LeakSource::Pushed(_) => 0,
        LeakSource::Untracked => 1,
        LeakSource::Unstaged => 2,
        LeakSource::Staged => 3,
        LeakSource::Commit(_) => 4,
    }
}

fn group_title(s: &LeakSource, snap: &Snapshot) -> String {
    let commit = |oid: &str, mark: &str| {
        let short: String = oid.chars().take(7).collect();
        match snap.commits.iter().find(|c| c.oid == oid) {
            Some(c) => format!("{short} {mark} {}", c.subject),
            None => format!("{short} {mark}"),
        }
    };
    match s {
        LeakSource::Untracked => "Untracked".into(),
        LeakSource::Unstaged => "Unstaged".into(),
        LeakSource::Staged => "Staged".into(),
        LeakSource::Commit(o) => commit(o, "↑"),
        LeakSource::Pushed(o) => commit(o, "pushed"),
    }
}

const DASHBOARD_HELP: &[(&str, &str)] = &[
    ("click", "open · fold · button"),
    ("wheel", "scroll"),
    ("", ""),
    ("j k ↑ ↓", "move"),
    ("enter l", "open · fold"),
    ("esc h", "clear selection"),
    ("tab", "next section"),
    ("space", "fold section"),
    ("g G", "top / bottom"),
    ("s", "possible secrets"),
    ("f", "fetch now"),
    ("?", "close help"),
    ("q", "quit"),
];

const DETAIL_HELP: &[(&str, &str)] = &[
    ("click", "open · button"),
    ("wheel", "scroll"),
    ("right-click", "back"),
    ("", ""),
    ("j k ↑ ↓", "move · scroll"),
    ("enter l", "open"),
    ("esc h", "back"),
    ("space pgdn", "page down"),
    ("pgup", "page up"),
    ("g G", "top / bottom"),
    ("w", "wrap long lines"),
    ("s", "possible secrets"),
    ("f", "fetch now"),
    ("?", "close help"),
    ("q", "quit"),
];

/// The keys that do something in every layout.
const SHORT_HELP: &[(&str, &str)] = &[("q", "quit"), ("f", "fetch now"), ("?", "close help")];

/// Help for the one-line layout, and wherever the full box does not fit:
/// one key per row, or all on one line.
pub fn draw_short_help(f: &mut Frame, theme: &Theme, area: Rect) {
    let key_style = Style::new().fg(theme.accent).add_modifier(Modifier::BOLD);
    let entry = |(k, v): &(&str, &str)| {
        [
            Span::styled(format!(" {k} "), key_style),
            Span::styled(v.to_string(), theme.dim),
        ]
    };
    let lines: Vec<Line> = if area.height as usize >= SHORT_HELP.len() {
        SHORT_HELP
            .iter()
            .map(|e| Line::from_iter(entry(e)))
            .collect()
    } else {
        let mut spans = Vec::new();
        for (i, e) in SHORT_HELP.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" ·", theme.dim));
            }
            spans.extend(entry(e));
        }
        vec![Line::from(spans)]
    };
    let r = Rect::new(area.x, area.y, area.width, lines.len() as u16);
    f.render_widget(Clear, r);
    f.render_widget(Paragraph::new(lines), r);
}

/// Centered help box for the current view; any key or click closes it.
pub fn draw_help(f: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    let help = if app.stack.is_empty() {
        DASHBOARD_HELP
    } else {
        DETAIL_HELP
    };
    let w = area.width.min(38);
    let h = area.height.min(help.len() as u16 + 2);
    if w < 10 || h < 3 {
        draw_short_help(f, theme, area);
        return;
    }
    let r = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    );
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme.accent))
        .title(Span::styled(
            " gitst ",
            Style::new().add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(r);
    f.render_widget(Clear, r);
    f.render_widget(block, r);
    let key_style = Style::new().fg(theme.accent).add_modifier(Modifier::BOLD);
    let lines: Vec<Line> = help
        .iter()
        .map(|(k, v)| {
            Line::from(vec![
                Span::styled(format!(" {k:<12}"), key_style),
                Span::styled(v.to_string(), theme.dim),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_overflowing_view_is_laid_out_once_without_the_scrollbar_column() {
        let lines = |n: usize| -> Option<Vec<DetailLine>> { Some(vec![(Line::raw(""), None); n]) };
        let mut widths = Vec::new();
        let (laid, w) = fit(10, 3, |w| {
            widths.push(w);
            lines(5)
        })
        .unwrap();
        assert_eq!(widths, vec![9], "the heavy case is laid out once");
        assert_eq!((laid.len(), w), (5, 9));
        widths.clear();
        let (laid, w) = fit(10, 3, |w| {
            widths.push(w);
            lines(2)
        })
        .unwrap();
        assert_eq!(
            widths,
            vec![9, 10],
            "a view that fits takes the whole width"
        );
        assert_eq!((laid.len(), w), (2, 10));
        assert_eq!(fit(1, 3, |w| lines(w + 4)).unwrap().1, 1);
    }

    #[test]
    fn wrap_keeps_clusters_whole_and_within_width() {
        let s = "♻\u{fe0f}".repeat(7);
        for piece in wrap_text(&s, 5) {
            assert!(width(&piece) <= 5, "{piece:?}");
            assert!(!piece.starts_with('\u{fe0f}'), "{piece:?}");
        }
    }
}
