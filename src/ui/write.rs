//! What write mode puts on screen: the action labels on the header row,
//! the prompt, confirm, and result in the bottom row, and the failure's
//! output.

use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::fmt::{truncate_right, width};
use super::line_lr;
use super::theme::Theme;
use crate::app::{Action, App, InputPurpose, Prompt, Target, WriteAction, staged_only};
use crate::model::Snapshot;
use crate::write::{Verb, refusal};

/// A clickable word on the header row.
pub(super) struct Label {
    text: String,
    dimmed: bool,
    action: Action,
}

/// Where a label sits in its group: offset from the group's start, width,
/// and what a click runs.
pub(super) type LabelHit = (usize, usize, Action);

const SEP: &str = " · ";

fn label(text: &str, dimmed: bool, action: WriteAction) -> Label {
    Label {
        text: text.into(),
        dimmed,
        action: Action::Write(action),
    }
}

/// `Commit · Push · Pull · Stash`, in the order they drop out from the
/// right. The running one shows the spinner.
pub(super) fn action_labels(app: &App, snap: &Snapshot) -> Vec<Label> {
    let no_work = snap.changes.is_empty() && snap.commits.is_empty();
    let nothing_ahead = snap.upstream.as_ref().is_some_and(|u| u.ahead == 0);
    let mut labels = vec![
        (
            Verb::Commit,
            label(
                "Commit",
                refusal(Verb::Commit, snap).is_some() || no_work,
                WriteAction::Commit,
            ),
        ),
        (
            Verb::Push,
            label(
                "Push",
                refusal(Verb::Push, snap).is_some() || nothing_ahead,
                WriteAction::Push,
            ),
        ),
        (
            Verb::Pull,
            label(
                "Pull",
                refusal(Verb::Pull, snap).is_some(),
                WriteAction::Pull,
            ),
        ),
        (
            Verb::Stash,
            label(
                "Stash",
                refusal(Verb::Stash, snap).is_some(),
                WriteAction::Stash,
            ),
        ),
    ];
    for (verb, l) in &mut labels {
        if app.write.running == Some(*verb) {
            l.text = format!("{} {}", super::spinner(app), l.text);
            l.dimmed = false;
        }
    }
    labels.into_iter().map(|(_, l)| l).collect()
}

/// The labels of the open detail view: stage a file, or act on a stash.
pub(super) fn context_labels(app: &App, snap: &Snapshot) -> Vec<Label> {
    match app.stack.last().map(|v| &v.target) {
        Some(Target::File(path)) => {
            let Some(c) = snap.changes.iter().find(|c| &c.path == path) else {
                return Vec::new();
            };
            let text = if staged_only(c) {
                "− unstage file"
            } else {
                "+ stage file"
            };
            vec![label(text, false, WriteAction::ToggleFile(path.clone()))]
        }
        Some(Target::Stash(i)) => vec![
            label("pop", false, WriteAction::StashPop(*i)),
            label("apply", false, WriteAction::StashApply(*i)),
            label("drop", false, WriteAction::StashDrop(*i)),
        ],
        _ => Vec::new(),
    }
}

/// Cells the first `n` labels take, with their separators.
pub(super) fn group_width(labels: &[Label], n: usize) -> usize {
    let words: usize = labels.iter().take(n).map(|l| width(&l.text)).sum();
    words + width(SEP) * n.saturating_sub(1)
}

/// The first `n` labels as spans, and where each can be clicked.
pub(super) fn group_spans(
    labels: &[Label],
    n: usize,
    theme: &Theme,
) -> (Vec<Span<'static>>, Vec<LabelHit>) {
    let mut spans = Vec::new();
    let mut hits = Vec::new();
    let mut x = 0;
    for (i, l) in labels.iter().take(n).enumerate() {
        if i > 0 {
            spans.push(Span::styled(SEP, theme.dim));
            x += width(SEP);
        }
        let style = if l.dimmed {
            theme.dim
        } else {
            Style::new().fg(theme.accent)
        };
        let w = width(&l.text);
        spans.push(Span::styled(l.text.clone(), style));
        hits.push((x, w, l.action.clone()));
        x += w;
    }
    (spans, hits)
}

/// The lowercase word for what an op is doing.
fn verb_word(verb: Verb) -> &'static str {
    match verb {
        Verb::Commit => "commit",
        Verb::Push => "push",
        Verb::Pull => "pull",
        Verb::Stash | Verb::StashAction => "stash",
    }
}

/// Whether the bottom row has something to say: a prompt or confirm, the
/// wait for an op at quit, or a result still on show.
pub(super) fn bottom_row_active(app: &App) -> bool {
    app.write.prompt.is_some() || app.write.quitting || app.write.live_result().is_some()
}

/// Draws the bottom row into `r`, which is one row, and records it.
pub(super) fn bottom_row(f: &mut Frame, app: &mut App, theme: &Theme, r: Rect) {
    let w = r.width as usize;
    let key_style = Style::new().add_modifier(Modifier::BOLD);
    if let Some(prompt) = &app.write.prompt {
        app.write.prompt_rect = Some(r);
        match prompt {
            Prompt::Input { purpose, input, .. } => {
                let (word, hint) = match purpose {
                    InputPurpose::Commit => ("commit", "⏎ commit · ⇥ amend · esc"),
                    InputPurpose::Stash => ("stash", "⏎ stash · esc"),
                };
                let lead = format!(" {word}: ");
                let hint_w = width(hint);
                // The hint goes first when room is short.
                let hint_shown = width(&lead) + hint_w + 8 <= w;
                let reserved = if hint_shown { hint_w + 2 } else { 0 };
                let cells = w.saturating_sub(width(&lead) + reserved).max(1);
                let (view, offset) = input.view(cells);
                let right = if hint_shown {
                    vec![Span::styled(hint, theme.dim), Span::raw(" ")]
                } else {
                    Vec::new()
                };
                let left = vec![
                    Span::styled(lead.clone(), Style::new().fg(theme.accent)),
                    Span::raw(view),
                ];
                f.render_widget(Paragraph::new(line_lr(left, right, w)), r);
                let x = (width(&lead) + offset).min(w.saturating_sub(1));
                f.set_cursor_position(Position::new(r.x + x as u16, r.y));
            }
            Prompt::Confirm {
                question, y_only, ..
            } => {
                let keys = if *y_only { "y" } else { "y / n" };
                let right = vec![Span::styled(keys, key_style), Span::raw(" ")];
                let room = w.saturating_sub(width(keys) + 3);
                let left = vec![Span::raw(format!(" {}", truncate_right(question, room)))];
                f.render_widget(Paragraph::new(line_lr(left, right, w)), r);
            }
        }
        return;
    }
    let text = if app.write.quitting {
        match app.write.running {
            Some(v) => format!(" finishing {}…", verb_word(v)),
            None => " finishing…".to_string(),
        }
    } else {
        format!(" {}", app.write.live_result().unwrap_or_default())
    };
    let line = Line::from(Span::raw(truncate_right(&text, w)));
    f.render_widget(Paragraph::new(line), r);
}
