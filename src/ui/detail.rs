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
use crate::model::{Commit, CommitDetail, DetailData, DiffBlock, DiffKind};

type DetailLine = (Line<'static>, Option<Target>);

/// Draws the top detail view into `area`; returns the selected line's rect.
pub fn draw(f: &mut Frame, app: &mut App, theme: &Theme, area: Rect, d: Density) -> Option<Rect> {
    let view = app.stack.last()?;
    if area.height == 0 {
        return None;
    }
    let w = area.width as usize;
    let lines: Vec<DetailLine> = match &view.data {
        None => vec![(Line::from(Span::styled(" loading…", theme.dim)), None)],
        Some(Err(e)) => vec![(
            Line::from(Span::styled(format!(" ⚠ {e}"), Style::new().fg(theme.err))),
            None,
        )],
        Some(Ok(DetailData::File(blocks) | DetailData::CommitFile(blocks))) => {
            diff_lines(blocks, theme, w, app.wrap)
        }
        Some(Ok(DetailData::Commit(c))) => commit_lines(c, app, theme, w, d),
        Some(Ok(DetailData::Branch { ahead, behind })) => {
            let upstream = match &view.target {
                Target::Branch {
                    upstream: Some(u), ..
                } => u.clone(),
                _ => "upstream".into(),
            };
            branch_lines(ahead, behind, &upstream, app, theme, w, d)
        }
    };

    let height = area.height as usize;
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
        let r = Rect::new(area.x, area.y + (i - scroll) as u16, area.width, 1);
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
    for (path, a, r) in &c.files {
        let mut right = Vec::new();
        if d.stats {
            right.extend(stats_spans(*a, *r, theme));
            right.push(Span::raw(" "));
            right.extend(meter(*a, *r, theme));
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

fn branch_lines(
    ahead: &[Commit],
    behind: &[Commit],
    upstream: &str,
    app: &App,
    theme: &Theme,
    w: usize,
    d: Density,
) -> Vec<DetailLine> {
    if ahead.is_empty() && behind.is_empty() {
        let text = format!(" ✓ in sync with {upstream}");
        return vec![(
            Line::from(Span::styled(text, Style::new().fg(theme.add))),
            None,
        )];
    }
    let row_w = w.saturating_sub(2);
    let mut out = Vec::new();
    for (label, commits) in [("ahead of", ahead), ("behind", behind)] {
        if commits.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push((Line::default(), None));
        }
        out.push((
            rule(&format!("{label} {upstream} ({})", commits.len()), w, theme),
            None,
        ));
        for c in commits {
            out.push((
                commit_row(c, app, theme, row_w, d),
                Some(Target::Commit(c.oid.clone())),
            ));
        }
    }
    out
}

const HELP: &[(&str, &str)] = &[
    ("click", "open · fold · button"),
    ("wheel", "scroll"),
    ("right-click", "back"),
    ("", ""),
    ("j k ↑ ↓", "move"),
    ("enter l", "open"),
    ("esc h", "back"),
    ("tab", "next section"),
    ("space", "fold section"),
    ("g G", "top / bottom"),
    ("f", "fetch now"),
    ("w", "wrap long lines"),
    ("?", "close help"),
    ("q", "quit"),
];

/// Centered help box; any key or click closes it.
pub fn draw_help(f: &mut Frame, _app: &mut App, theme: &Theme, area: Rect) {
    let w = area.width.min(38);
    let h = area.height.min(HELP.len() as u16 + 2);
    if w < 10 || h < 3 {
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
    let lines: Vec<Line> = HELP
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
    fn wrap_keeps_clusters_whole_and_within_width() {
        let s = "♻\u{fe0f}".repeat(7);
        for piece in wrap_text(&s, 5) {
            assert!(width(&piece) <= 5, "{piece:?}");
            assert!(!piece.starts_with('\u{fe0f}'), "{piece:?}");
        }
    }
}
