//! What write mode puts on screen: the action labels on the header row,
//! the prompt, confirm, and result in the row under it, and the failure's
//! output.

use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use super::fmt::{truncate_right, width};
use super::line_lr;
use super::theme::Theme;
use crate::app::{Action, App, InputPurpose, Prompt, Target, WriteAction, staged_only};
use crate::model::{Head, Snapshot};
use crate::write::{Verb, refusal};

/// A clickable word on the header row.
pub(super) struct Label {
    text: String,
    dimmed: bool,
    /// Bold, for the one thing the view is there for.
    strong: bool,
    /// Drawn first in its group, wherever it comes in the drop order.
    lead: bool,
    action: Action,
}

impl Label {
    /// Whether this is `Write ▾`.
    pub(super) fn opens_menu(&self) -> bool {
        self.action == Action::WriteMenu
    }
}

/// Where a label sits in its group: offset from the group's start, width,
/// and what a click runs.
pub(super) type LabelHit = (usize, usize, Action);

const SEP: &str = " · ";

fn label(text: &str, dimmed: bool, action: WriteAction) -> Label {
    Label {
        text: text.into(),
        dimmed,
        strong: false,
        lead: false,
        action: Action::Write(action),
    }
}

/// Puts the spinner before `l` while `verb` runs.
fn spin(app: &App, l: &mut Label, verb: Verb) {
    if app.write.running == Some(verb) {
        l.text = format!("{} {}", super::spinner(app), l.text);
        l.dimmed = false;
    }
}

/// The `Push` label, dimmed when there is nothing it can send.
fn push_label(app: &App, snap: &Snapshot, text: &str) -> Label {
    let nothing_ahead = snap.upstream.as_ref().is_some_and(|u| u.ahead == 0);
    let mut l = label(
        text,
        refusal(Verb::Push, snap).is_some() || nothing_ahead,
        WriteAction::Push,
    );
    spin(app, &mut l, Verb::Push);
    l
}

/// `Push · Write ▾`, in the reverse of the order they drop out in: `Push`
/// is drawn first but goes first, into the menu, which keeps every action
/// in reach.
pub(super) fn action_labels(app: &App, snap: &Snapshot) -> Vec<Label> {
    // The Outgoing view shows what a push sends, so pushing is all its
    // header offers. Over the push confirm, a click answers it.
    if let Some(Target::Outgoing { .. }) = app.stack.last().map(|v| &v.target) {
        let mut l = push_label(app, snap, "Push now");
        l.strong = !l.dimmed;
        return vec![l];
    }
    let other = app.write.running.filter(|v| *v != Verb::Push);
    let menu = Label {
        text: match other {
            Some(_) => format!("{} Write ▾", super::spinner(app)),
            None => "Write ▾".into(),
        },
        dimmed: false,
        strong: false,
        lead: false,
        action: Action::WriteMenu,
    };
    let mut push = push_label(app, snap, "Push");
    push.lead = true;
    vec![menu, push]
}

/// The menu's items, each with the key that does the same: `Stage all`
/// on the dashboard when there are changes, `Commit`, `Pull`, `Stash`, and
/// `Push` when the header had no room for it.
pub(super) fn menu_items(app: &App, snap: &Snapshot) -> Vec<(Label, &'static str)> {
    let unborn = matches!(snap.head, Head::Unborn(_));
    let clean = snap.changes.is_empty();
    // A clean tree has nothing to commit, but HEAD can still be amended.
    let can_amend = clean && !unborn && !snap.commits.is_empty();
    let no_work = clean && !can_amend;
    let mut items = Vec::new();
    if app.stack.is_empty() && !clean {
        let all_staged = snap.changes_omitted == 0 && snap.changes.iter().all(staged_only);
        let text = if all_staged {
            "Unstage all"
        } else {
            "Stage all"
        };
        items.push((label(text, false, WriteAction::ToggleAll), "A"));
    }
    let verbs = [
        (
            Verb::Commit,
            if can_amend { "Amend" } else { "Commit" },
            no_work,
            WriteAction::Commit,
            "C",
        ),
        (Verb::Pull, "Pull", false, WriteAction::Pull, "U"),
        (Verb::Stash, "Stash", false, WriteAction::Stash, "S"),
    ];
    for (verb, text, idle, action, key) in verbs {
        let mut l = label(text, refusal(verb, snap).is_some() || idle, action);
        spin(app, &mut l, verb);
        items.push((l, key));
    }
    if app.write.push_in_menu {
        items.push((push_label(app, snap, "Push"), "P"));
    }
    items
}

/// Draws the open `Write ▾` menu under its label, right-aligned to it and
/// kept inside `area`, and registers its items over everything else.
pub(super) fn draw_menu(f: &mut Frame, app: &mut App, snap: &Snapshot, theme: &Theme, area: Rect) {
    if !app.write.menu {
        return;
    }
    // A prompt or confirm that arrived while it was open takes the row it
    // would cover, and a view whose header has no `Write ▾` has nothing to
    // hang it from: either way it closes, rather than stay open unseen.
    let anchor = match app.write.menu_anchor {
        Some(a) if app.write.prompt.is_none() => a,
        _ => {
            app.close_menu();
            return;
        }
    };
    let items = menu_items(app, snap);
    // A cell of padding, the word, two cells, the key, and a cell.
    let text_w = items.iter().map(|(l, _)| width(&l.text)).max().unwrap_or(0);
    let row_w = 1 + text_w + 2 + 1 + 1;
    let w = (row_w as u16 + 2).min(area.width);
    let h = (items.len() as u16 + 2).min(area.height.saturating_sub(1));
    if w < 5 || h < 3 {
        app.close_menu();
        return;
    }
    let right = (anchor.x + anchor.width).clamp(area.x + w, area.x + area.width);
    let r = Rect::new(right - w, area.y + 1, w, h);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme.accent));
    let inner = block.inner(r);
    f.render_widget(Clear, r);
    f.render_widget(block, r);
    for (i, (l, key)) in items.into_iter().take(inner.height as usize).enumerate() {
        let style = if l.dimmed {
            theme.dim
        } else {
            Style::new().fg(theme.accent)
        };
        let row = Rect::new(inner.x, inner.y + i as u16, inner.width, 1);
        let left = vec![Span::raw(" "), Span::styled(l.text, style)];
        let right = vec![Span::styled(key, theme.dim), Span::raw(" ")];
        f.render_widget(
            Paragraph::new(line_lr(left, right, inner.width as usize)),
            row,
        );
        app.hits.overlay.push((row, l.action));
    }
    app.write.menu_rect = Some(r);
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
    let taken = &labels[..n.min(labels.len())];
    let ordered = taken
        .iter()
        .filter(|l| l.lead)
        .chain(taken.iter().filter(|l| !l.lead));
    for (i, l) in ordered.enumerate() {
        if i > 0 {
            spans.push(Span::styled(SEP, theme.dim));
            x += width(SEP);
        }
        let style = if l.dimmed {
            theme.dim
        } else if l.strong {
            Style::new().fg(theme.accent).add_modifier(Modifier::BOLD)
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

/// Whether the prompt row has something to say: a prompt or confirm, the
/// wait for an op at quit, or a result still on show.
pub(super) fn prompt_row_active(app: &App) -> bool {
    app.write.prompt.is_some() || app.write.quitting || app.write.live_result().is_some()
}

/// Draws the prompt row into `r`, which is one row, and records it.
pub(super) fn prompt_row(f: &mut Frame, app: &mut App, theme: &Theme, r: Rect) {
    let w = r.width as usize;
    let key_style = Style::new().add_modifier(Modifier::BOLD);
    if let Some(prompt) = &app.write.prompt {
        app.write.prompt_rect = Some(r);
        match prompt {
            Prompt::Input {
                purpose,
                input,
                amend,
                ..
            } => {
                let (word, hint) = match purpose {
                    InputPurpose::Commit if *amend => ("amend", "⏎ amend · ⇥ new commit · esc"),
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
                let left = vec![Span::styled(lead.clone(), key_style), Span::raw(view)];
                f.render_widget(
                    Paragraph::new(line_lr(left, right, w)).style(theme.prompt),
                    r,
                );
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
                f.render_widget(
                    Paragraph::new(line_lr(left, right, w)).style(theme.prompt),
                    r,
                );
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
    f.render_widget(Paragraph::new(line).style(theme.prompt), r);
}
