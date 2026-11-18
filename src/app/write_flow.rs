//! Write mode in the app: keys, prompts, and confirms turned into write
//! ops, and their results turned into the next confirm or a short result.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Position, Rect};

use super::{Action, App, Cmd, DetailView, NavItem, Target, hunks};
use crate::model::{Change, DetailData, DetailReq, DiffKind, Head, Snapshot};
use crate::prompt::{Edit, LineInput};
use crate::ui::layout::SectionId;
use crate::worker::WorkerMsg;
use crate::write::{Coverage, PushPlan, Verb, WriteError, WriteOp, WriteOutcome, refusal};

/// How long the prompt row shows a result.
pub const RESULT_TTL: Duration = Duration::from_secs(3);

const CONFLICTED: &str = "resolve conflicts in your editor or shell";

/// Failures that answer the action rather than report an error: they go to
/// the prompt row, not the warning band.
const SOFT: &[&str] = &[
    "nothing to commit",
    "nothing to push",
    "nothing to stash",
    "stashes changed, refreshed",
    "no commit to amend",
    "amend never stages all",
    "file changed, refreshed",
];

#[derive(Default)]
pub struct WriteState {
    pub enabled: bool,
    /// The verb whose op is on its way or running: its label spins.
    pub running: Option<Verb>,
    pub prompt: Option<Prompt>,
    /// The last success or soft failure, for the prompt row, and when.
    pub result: Option<(String, Instant)>,
    /// The last failed action, for the warning band; `!` opens its output.
    pub failure: Option<WriteError>,
    /// `q` was pressed while an op ran; gitst quits when it reports.
    pub quitting: bool,
    /// Filled by draw: the prompt row while a prompt is open, so that a
    /// click on it doesn't cancel it.
    pub prompt_rect: Option<Rect>,
    /// The `Write ▾` menu is open.
    pub menu: bool,
    /// Filled by draw: the open menu's box, the `Write ▾` label it hangs
    /// from, and whether `Push` went into the menu for want of room.
    pub menu_rect: Option<Rect>,
    pub menu_anchor: Option<Rect>,
    pub push_in_menu: bool,
    /// An op is on its way or running, with a verb or not (staging has
    /// none). Write keys wait for it.
    busy: bool,
    pending: Option<Pending>,
    /// The commit in flight was started with Ctrl-E: once confirmed, it
    /// goes to the editor instead of the worker.
    editor: bool,
}

impl WriteState {
    /// The result, while it is still on show.
    pub fn live_result(&self) -> Option<&str> {
        let (text, at) = self.result.as_ref()?;
        (at.elapsed() < RESULT_TTL).then_some(text.as_str())
    }

    pub fn new(enabled: bool) -> WriteState {
        WriteState {
            enabled,
            ..WriteState::default()
        }
    }
}

/// What the prompt row asks for while it takes every key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Prompt {
    Input {
        purpose: InputPurpose,
        input: LineInput,
        amend: bool,
        /// The text typed before amend replaced it, restored when amend
        /// is toggled off.
        saved: String,
    },
    Confirm {
        /// Without the trailing `y / n`, which the prompt row adds.
        question: String,
        /// Only `y` accepts, not `Enter`.
        y_only: bool,
        on_yes: WriteOp,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputPurpose {
    Commit,
    Stash,
}

/// A write action from a key or a click.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteAction {
    Commit,
    Push,
    Pull,
    Stash,
    /// Stage or unstage a Changes file.
    ToggleFile(String),
    ToggleAll,
    /// Stage or unstage a hunk of the open file.
    ToggleHunk(usize),
    StashPop(usize),
    StashApply(usize),
    StashDrop(usize),
}

/// A flow between its steps.
#[derive(Debug)]
enum Pending {
    /// The commit message, waiting for `CommitReady`.
    Commit(String),
    /// Confirms still to accept, as `(question, y_only)`, before the open
    /// confirm's op is sent.
    Confirms(Vec<(String, bool)>),
}

/// The verb an op runs under, or `None` for staging.
fn verb(op: &WriteOp) -> Option<Verb> {
    match op {
        WriteOp::PrepareCommit { .. } | WriteOp::Commit { .. } => Some(Verb::Commit),
        WriteOp::PreparePush | WriteOp::Push(_) => Some(Verb::Push),
        WriteOp::Pull => Some(Verb::Pull),
        WriteOp::Stash { .. } => Some(Verb::Stash),
        WriteOp::StashPop { .. } | WriteOp::StashApply { .. } | WriteOp::StashDrop { .. } => {
            Some(Verb::StashAction)
        }
        WriteOp::Stage { .. }
        | WriteOp::Unstage { .. }
        | WriteOp::StageAll
        | WriteOp::UnstageAll { .. }
        | WriteOp::ApplyHunk { .. } => None,
    }
}

/// `1 file`, `2 files`.
pub(crate) fn count(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// A push that needs no look first: no possible secrets, a scan that
/// covered everything (or was turned off), and a branch the remote has
/// already. Anything else opens the Outgoing view with the confirm.
fn routine(plan: &PushPlan) -> bool {
    plan.findings.is_empty()
        && !matches!(plan.coverage, Coverage::Partial(_))
        && !plan.creates_branch
}

/// Staged, with nothing unstaged: `a` unstages such a file.
pub(crate) fn staged_only(c: &Change) -> bool {
    c.x != ' ' && c.y == ' '
}

impl App {
    /// Quits, or, while an op runs, quits once it reports. A second press
    /// quits at once.
    pub(super) fn quit(&mut self) -> Vec<Cmd> {
        if self.write.busy && !self.write.quitting {
            self.write.quitting = true;
            Vec::new()
        } else {
            vec![Cmd::Quit]
        }
    }

    fn show_result(&mut self, text: impl Into<String>) {
        self.write.result = Some((text.into(), Instant::now()));
    }

    /// Whether a click at `(x, y)` is on the open prompt's row.
    pub(super) fn in_prompt_row(&self, x: u16, y: u16) -> bool {
        self.write
            .prompt_rect
            .is_some_and(|r| r.contains(Position::new(x, y)))
    }

    /// How long until the screen changes for write mode's sake: a spinner
    /// frame while an op runs, or the result going away.
    pub(super) fn write_wakeup(&self) -> Option<Duration> {
        let spin = self.write.running.map(|_| Duration::from_millis(100));
        let result = self.write.result.as_ref().and_then(|(_, at)| {
            // A little after the expiry, so the redraw does not land just before it.
            RESULT_TTL
                .checked_sub(at.elapsed())
                .map(|left| left + Duration::from_millis(10))
        });
        match (spin, result) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Runs the write action a key stands for here, or `None` if it stands
    /// for none. With Ctrl or Alt held it stands for none, so that a
    /// shortcut meant for something else can't change the repository.
    pub(super) fn write_key(&mut self, k: KeyEvent) -> Option<Vec<Cmd>> {
        if !self.write.enabled
            || k.modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        let action = match k.code {
            KeyCode::Char('!') => return Some(self.perform(Action::ShowOutput)),
            KeyCode::Char('C') => WriteAction::Commit,
            KeyCode::Char('P') => WriteAction::Push,
            KeyCode::Char('U') => WriteAction::Pull,
            KeyCode::Char('S') => WriteAction::Stash,
            KeyCode::Char('A') if self.stack.is_empty() => WriteAction::ToggleAll,
            KeyCode::Char('a') if self.stack.is_empty() => {
                WriteAction::ToggleFile(self.selected_change()?)
            }
            KeyCode::Char('a') => self.detail_toggle()?,
            KeyCode::Char('p') => WriteAction::StashPop(self.selected_stash()?),
            KeyCode::Char('o') => WriteAction::StashApply(self.selected_stash()?),
            KeyCode::Char('D') => WriteAction::StashDrop(self.selected_stash()?),
            _ => return None,
        };
        Some(self.perform(Action::Write(action)))
    }

    /// The path of the selected Changes row.
    fn selected_change(&self) -> Option<String> {
        let NavItem {
            section: SectionId::Changes,
            row: Some(r),
        } = self.selected?
        else {
            return None;
        };
        match self.row_targets.get(&(SectionId::Changes, r))? {
            Target::File(path) => Some(path.clone()),
            _ => None,
        }
    }

    /// What `a` does in the open file view, once it has loaded: the
    /// selected hunk, or the whole file where hunks don't apply.
    fn detail_toggle(&self) -> Option<WriteAction> {
        let view = self.stack.last()?;
        let Target::File(path) = &view.target else {
            return None;
        };
        view.blocks()?;
        if self.whole_file_only(view) {
            return Some(WriteAction::ToggleFile(path.clone()));
        }
        Some(WriteAction::ToggleHunk(view.hunk?))
    }

    /// Whether the file `view` shows can only be staged as a whole
    /// (spec §6.2): untracked, renamed or copied, a type change,
    /// conflicted, or filtered, or a diff with no hunks (binary, mode
    /// only), a submodule's, or one too large to show. True for a view
    /// that isn't a loaded working-tree file.
    pub(crate) fn whole_file_only(&self, view: &DetailView) -> bool {
        let (Target::File(path), Some(blocks)) = (&view.target, view.blocks()) else {
            return true;
        };
        if view.filtered() {
            return true;
        }
        let row = self
            .snap
            .as_ref()
            .and_then(|s| s.changes.iter().find(|c| &c.path == path));
        if row.is_some_and(|c| {
            c.untracked() || c.orig_path.is_some() || c.conflicted() || c.x == 'T' || c.y == 'T'
        }) {
            return true;
        }
        let mut lines = blocks.iter().flat_map(|b| &b.lines);
        let mut any_hunk = false;
        let odd = lines.any(|l| match l.kind {
            DiffKind::Hunk => {
                any_hunk = true;
                // A combined diff: the file is conflicted.
                l.text.starts_with("@@@")
            }
            DiffKind::Add | DiffKind::Del => l.text.starts_with("Subproject commit "),
            DiffKind::Meta => l.text.starts_with("diff too large to show"),
            DiffKind::Context => false,
        });
        odd || !any_hunk
    }

    /// The stash of the open stash view, or of the selected Stashes row.
    fn selected_stash(&self) -> Option<usize> {
        let target = match self.stack.last() {
            Some(view) => &view.target,
            None => {
                let NavItem {
                    section: SectionId::Stashes,
                    row: Some(r),
                } = self.selected?
                else {
                    return None;
                };
                self.row_targets.get(&(SectionId::Stashes, r))?
            }
        };
        match target {
            Target::Stash(i) => Some(*i),
            _ => None,
        }
    }

    /// Stash `i` as the user sees it, as `(oid, message)`. In its open
    /// view, from the view's own data: that is never reloaded, so a stash
    /// pushed meanwhile, which takes over `stash@{i}` in the snapshot,
    /// can't take its place; `None` until the view has loaded. On a
    /// Stashes row, from the snapshot the row shows.
    fn shown_stash(&self, i: usize, snap: &Snapshot) -> Option<(String, String)> {
        let Some(view) = self.stack.last() else {
            let stash = snap.stashes.iter().find(|s| s.index == i)?;
            return Some((stash.oid.clone(), stash.message.clone()));
        };
        if view.target != Target::Stash(i) {
            return None;
        }
        let Some(Ok(DetailData::Commit(detail))) = &view.data else {
            return None;
        };
        let subject = detail.message.lines().next().unwrap_or_default();
        Some((detail.oid.clone(), subject.to_string()))
    }

    /// Shows why `verb` can't run now, if it can't.
    fn refused(&mut self, verb: Verb, snap: &Snapshot) -> bool {
        match refusal(verb, snap) {
            Some(why) => {
                self.show_result(why);
                true
            }
            None => false,
        }
    }

    pub(super) fn write_action(&mut self, action: WriteAction) -> Vec<Cmd> {
        let w = &self.write;
        if !w.enabled || w.busy || w.prompt.is_some() {
            return Vec::new();
        }
        if self.drawn_size.is_some_and(|(w, h)| h < 3 || w < 30) {
            self.show_result("pane too small");
            return Vec::new();
        }
        let Some(snap) = self.snap.clone() else {
            return Vec::new();
        };
        let unborn = matches!(snap.head, Head::Unborn(_));
        match action {
            WriteAction::Commit => {
                if self.refused(Verb::Commit, &snap) {
                    return Vec::new();
                }
                if !snap.changes.is_empty() {
                    self.open_input(InputPurpose::Commit);
                } else if let Some(subject) = self.amend_subject() {
                    // Nothing to commit, so the header says Amend.
                    self.open_input(InputPurpose::Commit);
                    if let Some(Prompt::Input { input, amend, .. }) = &mut self.write.prompt {
                        *input = LineInput::new(&subject);
                        *amend = true;
                    }
                } else {
                    self.show_result("nothing to commit");
                }
                Vec::new()
            }
            WriteAction::Push => self.send_write(WriteOp::PreparePush),
            WriteAction::Pull => self.send_write(WriteOp::Pull),
            WriteAction::Stash => {
                if self.refused(Verb::Stash, &snap) {
                    return Vec::new();
                }
                if snap.changes.is_empty() {
                    self.show_result("nothing to stash");
                } else {
                    self.open_input(InputPurpose::Stash);
                }
                Vec::new()
            }
            WriteAction::ToggleFile(path) => {
                let Some(c) = snap.changes.iter().find(|c| c.path == path) else {
                    return Vec::new();
                };
                if c.conflicted() {
                    self.show_result(CONFLICTED);
                    return Vec::new();
                }
                let op = if staged_only(c) {
                    WriteOp::Unstage { path, unborn }
                } else {
                    WriteOp::Stage { path }
                };
                self.send_write(op)
            }
            WriteAction::ToggleAll => {
                if snap.changes.is_empty() {
                    return Vec::new();
                }
                // `add -A` would mark them resolved.
                if snap.changes.iter().any(Change::conflicted) {
                    self.show_result(CONFLICTED);
                    return Vec::new();
                }
                let all_staged = snap.changes_omitted == 0 && snap.changes.iter().all(staged_only);
                self.send_write(if all_staged {
                    WriteOp::UnstageAll { unborn }
                } else {
                    WriteOp::StageAll
                })
            }
            WriteAction::ToggleHunk(i) => {
                let Some(view) = self.stack.last() else {
                    return Vec::new();
                };
                let (Target::File(path), Some(blocks)) = (&view.target, view.blocks()) else {
                    return Vec::new();
                };
                if self.whole_file_only(view) {
                    let path = path.clone();
                    return self.write_action(WriteAction::ToggleFile(path));
                }
                let Some((staged, hunk)) = hunks(blocks).into_iter().nth(i) else {
                    return Vec::new();
                };
                let path = path.clone();
                self.send_write(WriteOp::ApplyHunk {
                    path,
                    hunk,
                    reverse: staged,
                })
            }
            WriteAction::StashPop(i) | WriteAction::StashApply(i) | WriteAction::StashDrop(i) => {
                let Some((oid, message)) = self.shown_stash(i, &snap) else {
                    return Vec::new();
                };
                let index = i;
                match action {
                    WriteAction::StashPop(_) => self.send_write(WriteOp::StashPop { index, oid }),
                    WriteAction::StashApply(_) => {
                        self.send_write(WriteOp::StashApply { index, oid })
                    }
                    _ => {
                        if self.refused(Verb::StashAction, &snap) {
                            return Vec::new();
                        }
                        let question = format!("Drop stash@{{{i}}} \"{message}\"?");
                        self.ask(question, true, WriteOp::StashDrop { index, oid })
                    }
                }
            }
        }
    }

    fn open_input(&mut self, purpose: InputPurpose) {
        // The key that would close the help box must reach the prompt.
        self.help = false;
        self.write.prompt = Some(Prompt::Input {
            purpose,
            input: LineInput::default(),
            amend: false,
            saved: String::new(),
        });
    }

    fn ask(&mut self, question: String, y_only: bool, on_yes: WriteOp) -> Vec<Cmd> {
        // The key that would close the help box must reach the confirm.
        self.help = false;
        self.write.prompt = Some(Prompt::Confirm {
            question,
            y_only,
            on_yes,
        });
        Vec::new()
    }

    /// Asks each of `steps` in turn, then sends `op`.
    fn confirm_steps(&mut self, mut steps: Vec<(String, bool)>, op: WriteOp) -> Vec<Cmd> {
        if steps.is_empty() {
            return self.send_write(op);
        }
        let (question, y_only) = steps.remove(0);
        self.write.pending = Some(Pending::Confirms(steps));
        self.ask(question, y_only, op)
    }

    /// Sends `op` to the worker, unless its verb is refused now.
    fn send_write(&mut self, op: WriteOp) -> Vec<Cmd> {
        let verb = verb(&op);
        if let (Some(v), Some(snap)) = (verb, self.snap.clone())
            && self.refused(v, &snap)
        {
            self.write.pending = None;
            return Vec::new();
        }
        // The last failure is old news once something else runs.
        self.write.failure = None;
        if let WriteOp::Commit {
            message,
            amend,
            tree,
            head,
        } = &op
            && std::mem::take(&mut self.write.editor)
        {
            return vec![Cmd::Editor {
                amend: *amend,
                text: message.clone(),
                tree: tree.clone(),
                head: head.clone(),
            }];
        }
        self.write.busy = true;
        self.write.running = verb;
        vec![Cmd::Worker(WorkerMsg::Write(Box::new(op)))]
    }

    /// A key while a prompt or confirm is open: it takes every key.
    pub(super) fn prompt_key(&mut self, k: KeyEvent) -> Vec<Cmd> {
        let Some(prompt) = self.write.prompt.take() else {
            return Vec::new();
        };
        match prompt {
            Prompt::Confirm {
                question,
                y_only,
                on_yes,
            } if self.pinned_at().is_some() => self.pinned_confirm_key(k, question, y_only, on_yes),
            Prompt::Confirm {
                question,
                y_only,
                on_yes,
            } => match k.code {
                KeyCode::Char('y') => self.accept(on_yes),
                KeyCode::Enter if !y_only => self.accept(on_yes),
                KeyCode::Char('n') | KeyCode::Esc => {
                    self.write.pending = None;
                    Vec::new()
                }
                _ => self.ask(question, y_only, on_yes),
            },
            Prompt::Input {
                purpose,
                mut input,
                mut amend,
                mut saved,
            } => {
                let ctrl_e = k.code == KeyCode::Char('e')
                    && k.modifiers.contains(KeyModifiers::CONTROL)
                    && purpose == InputPurpose::Commit;
                if ctrl_e {
                    return self.submit(purpose, input, amend, saved, true);
                }
                if k.code == KeyCode::Tab {
                    if let (InputPurpose::Commit, Some(subject)) = (purpose, self.amend_subject()) {
                        if amend {
                            input = LineInput::new(&saved);
                        } else {
                            saved = std::mem::take(&mut input.text);
                            input = LineInput::new(&subject);
                        }
                        amend = !amend;
                    }
                } else {
                    match input.key(k) {
                        Edit::Submit => return self.submit(purpose, input, amend, saved, false),
                        Edit::Cancel => {
                            self.write.pending = None;
                            return Vec::new();
                        }
                        Edit::Changed | Edit::Ignored => {}
                    }
                }
                self.write.prompt = Some(Prompt::Input {
                    purpose,
                    input,
                    amend,
                    saved,
                });
                Vec::new()
            }
        }
    }

    /// A key while the push confirm sits over its Outgoing view: `y`
    /// answers every step of it, and `n`, or leaving the Outgoing view,
    /// cancels it. Every other key, `Enter` included, browses the views
    /// from it upward, and the confirm stays open: paging drops the
    /// selection, so an `Enter` meant to open a row could otherwise push.
    fn pinned_confirm_key(
        &mut self,
        k: KeyEvent,
        question: String,
        y_only: bool,
        on_yes: WriteOp,
    ) -> Vec<Cmd> {
        let at_outgoing = self.stack.last().is_some_and(DetailView::pinned);
        let cmds = match k.code {
            KeyCode::Char('y') => return self.accept_push(on_yes),
            KeyCode::Char('n') => {
                self.cancel_push();
                return Vec::new();
            }
            KeyCode::Esc | KeyCode::Char('h') | KeyCode::Left | KeyCode::Backspace => {
                if at_outgoing {
                    self.cancel_push();
                    return Vec::new();
                }
                self.perform(Action::Back)
            }
            // The detail keys only move, open, and wrap: none writes.
            code => self.detail_key(code),
        };
        self.ask(question, y_only, on_yes);
        cmds
    }

    /// Accepts a step of the push confirm. A push refused as it is sent
    /// closes its Outgoing view, as one that reports does.
    fn accept_push(&mut self, on_yes: WriteOp) -> Vec<Cmd> {
        let cmds = self.accept(on_yes);
        if self.write.prompt.is_none() && !self.write.busy {
            self.close_outgoing();
        }
        cmds
    }

    /// Drops the rest of the push confirm, with its Outgoing view.
    fn cancel_push(&mut self) {
        self.write.pending = None;
        self.close_outgoing();
    }

    /// Where the push plan's Outgoing view is in the stack, if it is open.
    fn pinned_at(&self) -> Option<usize> {
        self.stack.iter().position(DetailView::pinned)
    }

    /// Closes the push plan's Outgoing view and every view above it.
    fn close_outgoing(&mut self) {
        if let Some(at) = self.pinned_at() {
            self.stack.truncate(at);
            self.detail_targets.clear();
            self.hunk_lines.clear();
        }
    }

    /// HEAD's subject, unless there is no commit to amend.
    fn amend_subject(&self) -> Option<String> {
        let snap = self.snap.as_ref()?;
        if matches!(snap.head, Head::Unborn(_)) {
            return None;
        }
        snap.commits.first().map(|c| c.subject.clone())
    }

    fn accept(&mut self, on_yes: WriteOp) -> Vec<Cmd> {
        if let Some(Pending::Confirms(steps)) = &mut self.write.pending {
            if steps.is_empty() {
                self.write.pending = None;
            } else {
                let (question, y_only) = steps.remove(0);
                return self.ask(question, y_only, on_yes);
            }
        }
        self.send_write(on_yes)
    }

    fn submit(
        &mut self,
        purpose: InputPurpose,
        input: LineInput,
        amend: bool,
        saved: String,
        editor: bool,
    ) -> Vec<Cmd> {
        self.write.editor = editor;
        let text = input.text.trim();
        if purpose == InputPurpose::Stash {
            let message = (!text.is_empty()).then(|| text.to_string());
            return self.send_write(WriteOp::Stash { message });
        }
        // The editor takes an empty message: git opens it blank, or with
        // HEAD's message when amending.
        if text.is_empty() && !editor {
            self.write.prompt = Some(Prompt::Input {
                purpose,
                input,
                amend,
                saved,
            });
            return Vec::new();
        }
        let Some(snap) = self.snap.clone() else {
            return Vec::new();
        };
        let message = input.text;
        if amend {
            self.write.pending = Some(Pending::Commit(message));
            let op = WriteOp::PrepareCommit {
                stage_all: false,
                amend: true,
            };
            let pushed =
                snap.upstream.is_some() && snap.commits.first().is_some_and(|c| !c.unpushed);
            return if pushed {
                let question = "HEAD is pushed; amending will need a force-push. Amend anyway?";
                self.ask(question.into(), false, op)
            } else {
                self.send_write(op)
            };
        }
        if snap.changes.iter().any(Change::staged) {
            self.write.pending = Some(Pending::Commit(message));
            return self.send_write(WriteOp::PrepareCommit {
                stage_all: false,
                amend: false,
            });
        }
        if snap.changes.is_empty() {
            self.show_result("nothing to commit");
            return Vec::new();
        }
        // `add -A` would mark them resolved.
        if snap.changes.iter().any(Change::conflicted) {
            self.show_result(CONFLICTED);
            return Vec::new();
        }
        let files = count(snap.changes.len() + snap.changes_omitted, "file");
        self.write.pending = Some(Pending::Commit(message));
        let op = WriteOp::PrepareCommit {
            stage_all: true,
            amend: false,
        };
        self.ask(
            format!("Nothing staged. Stage all {files} and commit?"),
            false,
            op,
        )
    }

    /// An op reported: show its result, or ask the next confirm of its flow.
    pub(super) fn write_done(
        &mut self,
        op: &WriteOp,
        result: Result<WriteOutcome, WriteError>,
    ) -> Vec<Cmd> {
        self.write.busy = false;
        self.write.running = None;
        if self.write.quitting {
            return vec![Cmd::Quit];
        }
        let pending = self.write.pending.take();
        // A file view reloads after a hunk, whether it applied or the view
        // was stale: a hunk staged in a file that stays `MM` changes nothing
        // the next snapshot would notice. Staging a whole file always moves
        // its status, so the snapshot reloads the view then.
        self.mark_applied(op);
        // Pushed or not, the range it showed is done with.
        if matches!(op, WriteOp::Push(_)) {
            self.close_outgoing();
        }
        let hunk = matches!(op, WriteOp::ApplyHunk { .. });
        let reload = self
            .stack
            .last()
            .filter(|v| hunk && matches!(v.target, Target::File(_)))
            .map(|v| Cmd::Worker(WorkerMsg::Detail(v.req.clone())));
        let mut cmds = self.write_result(pending, op, result);
        cmds.extend(reload);
        cmds
    }

    /// Remembers where the hunk `op` applied sits in the open file view
    /// (its block, and its index there), so the reload can select the
    /// hunk nearest to it. It is looked up by content: the cursor may have
    /// moved while the op ran.
    fn mark_applied(&mut self, op: &WriteOp) {
        let WriteOp::ApplyHunk {
            path,
            hunk,
            reverse,
        } = op
        else {
            return;
        };
        let Some(view) = self.stack.last_mut() else {
            return;
        };
        if view.target != Target::File(path.clone()) {
            return;
        }
        let Some(blocks) = view.blocks() else {
            return;
        };
        let shown = hunks(blocks);
        if let Some(at) = shown.iter().position(|(s, h)| s == reverse && h == hunk) {
            let k = shown[..at].iter().filter(|(s, _)| s == reverse).count();
            view.reselect = Some((*reverse, k));
        }
    }

    /// The result of `op`, shown or turned into the next step.
    fn write_result(
        &mut self,
        pending: Option<Pending>,
        op: &WriteOp,
        result: Result<WriteOutcome, WriteError>,
    ) -> Vec<Cmd> {
        match result {
            Ok(WriteOutcome::Done { summary, warning }) => {
                self.show_done(summary, warning);
                self.close_gone_stash(op);
                Vec::new()
            }
            Ok(WriteOutcome::CommitReady(plan)) => {
                let Some(Pending::Commit(message)) = pending else {
                    return Vec::new();
                };
                let mut steps = Vec::new();
                if let Some(first) = plan.findings.first() {
                    let more = match plan.findings.len() - 1 {
                        0 => String::new(),
                        n => format!(" (+{n} more)"),
                    };
                    let question = format!(
                        "Possible {} in {}{more}. Commit anyway?",
                        first.label, first.path
                    );
                    steps.push((question, true));
                }
                if let Coverage::Partial(why) = &plan.coverage {
                    steps.push((format!("{why}. Commit anyway?"), false));
                }
                let op = WriteOp::Commit {
                    message,
                    amend: plan.amend,
                    tree: plan.tree,
                    head: plan.head,
                };
                self.confirm_steps(steps, op)
            }
            Ok(WriteOutcome::PushReady(plan)) => {
                if routine(&plan) {
                    return self.send_write(WriteOp::Push(plan));
                }
                self.open_pinned_outgoing(&plan);
                let target = format!("{}/{}", plan.remote, plan.branch);
                let track = if plan.set_upstream {
                    " and track it"
                } else {
                    ""
                };
                let commits = count(plan.count, "commit");
                let o = &plan.outgoing;
                let files = count(o.file_total, "file");
                let size = format!("{files} (+{} −{})", o.added, o.removed);
                let question = format!("Push {commits}, {size} to {target}{track}?");
                let mut steps = vec![(question, false)];
                if !plan.findings.is_empty() {
                    let secrets = count(plan.findings.len(), "possible secret");
                    let question =
                        format!("{secrets} in these commits will reach {target}. Push anyway?");
                    steps.push((question, true));
                }
                if let Coverage::Partial(why) = &plan.coverage {
                    steps.push((format!("{why}. Push anyway?"), false));
                }
                self.confirm_steps(steps, WriteOp::Push(plan))
            }
            Err(e) => {
                self.show_error(e);
                Vec::new()
            }
        }
    }

    /// Opens the Outgoing view of `plan`, drawn from the plan itself: what
    /// it shows is what is counted, scanned, and pushed.
    fn open_pinned_outgoing(&mut self, plan: &PushPlan) {
        let data = DetailData::Outgoing(Some((*plan.outgoing).clone()));
        self.push_view(
            Target::Outgoing { pinned: true },
            DetailReq::Outgoing,
            Some(Ok(data)),
        );
    }

    fn show_done(&mut self, summary: String, warning: Option<String>) {
        self.show_result(match warning {
            Some(w) => format!("{summary} · {w}"),
            None => summary,
        });
    }

    /// A refusal, or an outcome that answers the action, goes to the bottom
    /// row. Anything else failed, and stays in the warning band until it is
    /// cleared, whether or not git said anything (a timeout, or a hook that
    /// failed silently, says nothing).
    fn show_error(&mut self, e: WriteError) {
        if e.is_refusal() || SOFT.contains(&e.summary.as_str()) {
            self.show_result(e.summary);
        } else {
            // A failure is news even when an earlier one with the same
            // text was cleared.
            self.cleared.remove(&e.warning());
            self.write.failure = Some(e);
        }
    }

    /// The outcome of a commit made in the editor (spec §6.3), shown as
    /// the worker's would be.
    pub fn editor_done(&mut self, result: Result<WriteOutcome, WriteError>) {
        match result {
            Ok(WriteOutcome::Done { summary, warning }) => self.show_done(summary, warning),
            Ok(_) => {}
            Err(e) => self.show_error(e),
        }
    }

    /// Closes the open view of a stash that `op` popped or dropped: under
    /// its old index it would show the next stash. File and branch views
    /// reload with the snapshot that follows, and commit views can't change.
    fn close_gone_stash(&mut self, op: &WriteOp) {
        let (WriteOp::StashPop { index, .. } | WriteOp::StashDrop { index, .. }) = op else {
            return;
        };
        if self
            .stack
            .last()
            .is_some_and(|v| v.target == Target::Stash(*index))
        {
            self.stack.pop();
            self.detail_targets.clear();
            self.hunk_lines.clear();
        }
    }

    /// Opens the last failure's output, drawn from it with nothing to load.
    pub(super) fn show_output(&mut self) -> Vec<Cmd> {
        if !self.write.enabled {
            return Vec::new();
        }
        let Some(failure) = self.write.failure.as_ref().filter(|f| f.has_output()) else {
            return Vec::new();
        };
        let data = Some(Ok(DetailData::Output(failure.output.clone())));
        match self.stack.last_mut() {
            Some(view) if view.target == Target::Output => view.data = data,
            _ => self.push_view(Target::Output, DetailReq::Output, data),
        }
        Vec::new()
    }

    /// Pasted text goes to an open input, and nowhere else.
    pub(super) fn paste(&mut self, text: &str) -> Vec<Cmd> {
        if let Some(Prompt::Input { input, .. }) = &mut self.write.prompt {
            input.paste(text);
        }
        Vec::new()
    }

    /// A click at `(x, y)`, outside the prompt row, while a prompt is
    /// open. The header's `‹ back` above the push confirm's Outgoing view
    /// goes back and keeps the confirm, and `Push now` at that view
    /// answers it; any other click cancels the prompt (with that view), as
    /// `‹ back` at the view itself does.
    pub(super) fn click_while_prompting(&mut self, x: u16, y: u16) -> Vec<Cmd> {
        let hit = self.hits.click_at(x, y);
        let back = hit == Some(&Action::Back);
        let push_now = hit == Some(&Action::Write(WriteAction::Push));
        let above_outgoing = self.pinned_at().is_some_and(|at| at + 1 < self.stack.len());
        let at_outgoing = self.stack.last().is_some_and(DetailView::pinned);
        if push_now && at_outgoing {
            // `Push now` over the push confirm answers it, as `y` does.
            if let Some(Prompt::Confirm { on_yes, .. }) = self.write.prompt.take() {
                return self.accept_push(on_yes);
            }
        }
        if back && above_outgoing {
            return self.perform(Action::Back);
        }
        self.cancel_prompt();
        Vec::new()
    }

    /// A click while a prompt is open cancels it, and does nothing else.
    /// A cancelled push confirm closes its Outgoing view.
    pub(super) fn cancel_prompt(&mut self) -> bool {
        let open = self.write.prompt.take().is_some();
        if open {
            self.write.pending = None;
            self.close_outgoing();
        }
        open
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use crossterm::event::{Event, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::layout::Rect;

    use super::*;
    use crate::config::Config;
    use crate::model::{
        Commit, CommitDetail, Counts, DiffBlock, DiffKind, DiffLine, Leak, LeakSource, Outgoing,
        OutgoingFile, RepoOp, Stash, Upstream,
    };
    use crate::worker::{FetchStatus, UiMsg};
    use crate::write::{CommitPlan, PushPlan, ShownHunk};

    fn change(path: &str, x: char, y: char) -> Change {
        Change {
            path: path.into(),
            orig_path: None,
            x,
            y,
            counts: Counts::lines(1, 0),
            modified: None,
        }
    }

    fn snap() -> Snapshot {
        Snapshot {
            head: Head::Branch("main".into()),
            oid: Some("c1".into()),
            has_remote: true,
            upstream: Some(Upstream {
                name: "origin/main".into(),
                ahead: 1,
                behind: 0,
            }),
            commits: vec![Commit {
                oid: "c1".into(),
                short: "c1".into(),
                parents: 1,
                time: 0,
                author: "a".into(),
                refs: vec![],
                subject: "first commit".into(),
                unpushed: true,
            }],
            changes: vec![change("a.rs", 'M', ' ')],
            stashes: vec![Stash {
                index: 0,
                time: 0,
                message: "WIP on main: x".into(),
                oid: "s0".into(),
            }],
            stash_count: 1,
            ..Snapshot::default()
        }
    }

    fn app_with(write: bool, s: Snapshot) -> App {
        let cfg = Config {
            write,
            ..Config::default()
        };
        let mut a = App::new(&cfg, FetchStatus::default());
        a.snap = Some(Arc::new(s));
        a
    }

    fn wapp(s: Snapshot) -> App {
        app_with(true, s)
    }

    fn key(code: KeyCode) -> UiMsg {
        UiMsg::Input(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn ch(c: char) -> UiMsg {
        key(KeyCode::Char(c))
    }

    fn typed(a: &mut App, text: &str) {
        for c in text.chars() {
            assert!(a.handle(ch(c)).is_empty());
        }
    }

    fn sent(cmds: &[Cmd]) -> Option<&WriteOp> {
        cmds.iter().find_map(|c| match c {
            Cmd::Worker(WorkerMsg::Write(op)) => Some(&**op),
            _ => None,
        })
    }

    fn reply(a: &mut App, op: WriteOp, result: Result<WriteOutcome, WriteError>) -> Vec<Cmd> {
        a.handle(UiMsg::Write(op, result))
    }

    fn done(summary: &str) -> Result<WriteOutcome, WriteError> {
        Ok(WriteOutcome::Done {
            summary: summary.into(),
            warning: None,
        })
    }

    fn fail(summary: &str, output: &str) -> Result<WriteOutcome, WriteError> {
        Err(WriteError {
            summary: summary.into(),
            output: output.into(),
        })
    }

    /// The open confirm's question and whether it accepts `y` only.
    fn confirm(a: &App) -> Option<(&str, bool)> {
        match &a.write.prompt {
            Some(Prompt::Confirm {
                question, y_only, ..
            }) => Some((question.as_str(), *y_only)),
            _ => None,
        }
    }

    fn input(a: &App) -> Option<(&str, bool)> {
        match &a.write.prompt {
            Some(Prompt::Input { input, amend, .. }) => Some((input.text.as_str(), *amend)),
            _ => None,
        }
    }

    fn select_row(a: &mut App, section: SectionId, target: Target) {
        a.nav = vec![NavItem {
            section,
            row: Some(0),
        }];
        a.row_targets.insert((section, 0), target);
        a.selected = Some(a.nav[0]);
    }

    fn leak(path: &str) -> Leak {
        Leak {
            rule: "aws",
            label: "AWS access key",
            path: path.into(),
            line: Some(1),
            snippet: None,
            source: LeakSource::Staged,
        }
    }

    fn plan() -> PushPlan {
        PushPlan {
            local: "main".into(),
            remote: "origin".into(),
            branch: "main".into(),
            oid: "c1".into(),
            count: 3,
            set_upstream: false,
            creates_branch: false,
            findings: vec![],
            coverage: Coverage::Full,
            outgoing: Default::default(),
        }
    }

    /// A plan that needs the confirm: it creates the branch on the remote.
    fn risky() -> PushPlan {
        PushPlan {
            set_upstream: true,
            creates_branch: true,
            ..plan()
        }
    }

    #[test]
    fn write_keys_inert_in_read_only() {
        let mut a = app_with(false, snap());
        a.write.failure = Some(WriteError {
            summary: "x".into(),
            output: "y".into(),
        });
        select_row(&mut a, SectionId::Changes, Target::File("a.rs".into()));
        for c in ['a', 'A', 'C', 'P', 'U', 'S', '!'] {
            assert!(a.handle(ch(c)).is_empty(), "{c}");
            assert!(a.write.prompt.is_none(), "{c}");
        }
        select_row(&mut a, SectionId::Stashes, Target::Stash(0));
        for c in ['p', 'o', 'D'] {
            assert!(a.handle(ch(c)).is_empty(), "{c}");
            assert!(a.write.prompt.is_none(), "{c}");
        }
        assert!(a.stack.is_empty());
        assert!(a.write.result.is_none());
        assert!(
            a.perform(Action::Write(WriteAction::Push)).is_empty(),
            "clicks too"
        );
    }

    #[test]
    fn commit_flow_with_secret_needs_y() {
        let mut a = wapp(snap());
        assert!(a.handle(ch('C')).is_empty());
        assert_eq!(input(&a), Some(("", false)));
        typed(&mut a, "msg");
        let cmds = a.handle(key(KeyCode::Enter));
        let prepare = WriteOp::PrepareCommit {
            stage_all: false,
            amend: false,
        };
        assert_eq!(sent(&cmds), Some(&prepare));
        assert_eq!(a.write.running, Some(Verb::Commit));
        assert!(a.write.prompt.is_none());
        let ready = CommitPlan {
            tree: "t1".into(),
            head: Some("h1".into()),
            findings: vec![leak("config/dev.env")],
            coverage: Coverage::Full,
            amend: false,
        };
        assert!(reply(&mut a, prepare, Ok(WriteOutcome::CommitReady(ready))).is_empty());
        assert_eq!(a.write.running, None);
        assert_eq!(
            confirm(&a),
            Some((
                "Possible AWS access key in config/dev.env. Commit anyway?",
                true
            ))
        );
        assert!(a.handle(key(KeyCode::Enter)).is_empty(), "Enter can't pass");
        assert!(confirm(&a).is_some());
        let cmds = a.handle(ch('y'));
        assert_eq!(
            sent(&cmds),
            Some(&WriteOp::Commit {
                message: "msg".into(),
                amend: false,
                tree: "t1".into(),
                head: Some("h1".into()),
            })
        );
        assert!(a.write.prompt.is_none());
    }

    #[test]
    fn ctrl_e_after_ready_returns_editor_cmd() {
        let mut a = wapp(snap());
        a.handle(ch('C'));
        typed(&mut a, "msg");
        let ctrl_e = UiMsg::Input(Event::Key(KeyEvent::new(
            KeyCode::Char('e'),
            KeyModifiers::CONTROL,
        )));
        let cmds = a.handle(ctrl_e);
        let prepare = WriteOp::PrepareCommit {
            stage_all: false,
            amend: false,
        };
        assert_eq!(sent(&cmds), Some(&prepare));
        let ready = CommitPlan {
            tree: "t1".into(),
            head: Some("h1".into()),
            findings: Vec::new(),
            coverage: Coverage::Full,
            amend: false,
        };
        let cmds = reply(&mut a, prepare, Ok(WriteOutcome::CommitReady(ready)));
        assert!(matches!(
            &cmds[..],
            [Cmd::Editor { amend: false, text, tree, head }]
                if text == "msg" && tree == "t1" && head.as_deref() == Some("h1")
        ));
        assert!(!a.write.editor);
    }

    #[test]
    fn commit_confirms_count_more_findings_and_partial_coverage() {
        let mut a = wapp(snap());
        a.handle(ch('C'));
        typed(&mut a, "m");
        let cmds = a.handle(key(KeyCode::Enter));
        let op = sent(&cmds).unwrap().clone();
        let ready = CommitPlan {
            tree: "t".into(),
            head: Some("h1".into()),
            findings: vec![leak("a"), leak("b"), leak("c")],
            coverage: Coverage::Partial("2 binary files not scanned".into()),
            amend: false,
        };
        reply(&mut a, op, Ok(WriteOutcome::CommitReady(ready)));
        assert_eq!(
            confirm(&a),
            Some((
                "Possible AWS access key in a (+2 more). Commit anyway?",
                true
            ))
        );
        assert!(a.handle(ch('y')).is_empty());
        assert_eq!(
            confirm(&a),
            Some(("2 binary files not scanned. Commit anyway?", false))
        );
        let cmds = a.handle(key(KeyCode::Enter));
        assert!(matches!(sent(&cmds), Some(WriteOp::Commit { .. })));
    }

    #[test]
    fn declining_a_confirm_sends_nothing() {
        let mut a = wapp(snap());
        a.handle(ch('P'));
        reply(
            &mut a,
            WriteOp::PreparePush,
            Ok(WriteOutcome::PushReady(plan())),
        );
        assert!(a.handle(ch('n')).is_empty());
        assert!(a.write.prompt.is_none());
        a.handle(ch('P'));
        reply(
            &mut a,
            WriteOp::PreparePush,
            Ok(WriteOutcome::PushReady(plan())),
        );
        assert!(a.handle(key(KeyCode::Esc)).is_empty());
        assert!(a.write.prompt.is_none());
    }

    #[test]
    fn nothing_staged_asks_to_stage_all() {
        let mut s = snap();
        s.changes = vec![change("a.rs", ' ', 'M'), change("b.rs", '?', '?')];
        let mut a = wapp(s);
        a.handle(ch('C'));
        typed(&mut a, "x");
        assert!(a.handle(key(KeyCode::Enter)).is_empty());
        assert_eq!(
            confirm(&a),
            Some(("Nothing staged. Stage all 2 files and commit?", false))
        );
        let cmds = a.handle(key(KeyCode::Enter));
        let prepare = WriteOp::PrepareCommit {
            stage_all: true,
            amend: false,
        };
        assert_eq!(sent(&cmds), Some(&prepare));
        let ready = CommitPlan {
            tree: "t".into(),
            head: Some("h1".into()),
            findings: vec![],
            coverage: Coverage::Off,
            amend: false,
        };
        let cmds = reply(&mut a, prepare, Ok(WriteOutcome::CommitReady(ready)));
        assert_eq!(
            sent(&cmds),
            Some(&WriteOp::Commit {
                message: "x".into(),
                amend: false,
                tree: "t".into(),
                head: Some("h1".into()),
            }),
            "no findings, no confirm"
        );
    }

    #[test]
    fn commit_prompt_edge_cases() {
        let mut empty = snap();
        empty.changes.clear();
        let mut a = wapp(empty.clone());
        a.handle(ch('C'));
        assert_eq!(
            input(&a),
            Some(("first commit", true)),
            "a clean tree opens to amend HEAD"
        );
        a.handle(key(KeyCode::Tab));
        assert_eq!(input(&a), Some(("", false)), "⇥ goes to a new commit");
        assert!(a.handle(key(KeyCode::Enter)).is_empty(), "empty text");
        assert!(input(&a).is_some());
        typed(&mut a, "x");
        assert!(a.handle(key(KeyCode::Enter)).is_empty());
        assert!(a.write.prompt.is_none());
        assert_eq!(a.write.result.as_ref().unwrap().0, "nothing to commit");

        empty.head = Head::Unborn("main".into());
        empty.commits.clear();
        let mut a = wapp(empty);
        assert!(a.handle(ch('C')).is_empty());
        assert!(a.write.prompt.is_none());
        assert_eq!(a.write.result.as_ref().unwrap().0, "nothing to commit");
    }

    #[test]
    fn second_press_while_running_is_ignored() {
        let mut a = wapp(snap());
        let cmds = a.handle(ch('P'));
        assert_eq!(sent(&cmds), Some(&WriteOp::PreparePush));
        assert_eq!(a.write.running, Some(Verb::Push));
        assert!(a.handle(ch('P')).is_empty());
        assert!(a.handle(ch('C')).is_empty());
        assert!(a.write.prompt.is_none());
        reply(
            &mut a,
            WriteOp::PreparePush,
            Ok(WriteOutcome::PushReady(risky())),
        );
        assert_eq!(
            confirm(&a),
            Some((
                "Push 3 commits, 0 files (+0 −0) to origin/main and track it?",
                false
            ))
        );
        assert!(a.handle(ch('U')).is_empty(), "confirm takes the key");
        let cmds = a.handle(ch('y'));
        assert_eq!(sent(&cmds), Some(&WriteOp::Push(risky())));
        assert_eq!(a.write.running, Some(Verb::Push));
    }

    #[test]
    fn push_confirms_target_then_secrets_then_coverage() {
        let mut a = wapp(snap());
        a.handle(ch('P'));
        let p = PushPlan {
            set_upstream: true,
            creates_branch: true,
            findings: vec![leak("a"), leak("b")],
            coverage: Coverage::Partial("scan skipped".into()),
            ..plan()
        };
        reply(
            &mut a,
            WriteOp::PreparePush,
            Ok(WriteOutcome::PushReady(p.clone())),
        );
        assert_eq!(
            confirm(&a),
            Some((
                "Push 3 commits, 0 files (+0 −0) to origin/main and track it?",
                false
            ))
        );
        assert!(a.handle(ch('y')).is_empty());
        assert_eq!(
            confirm(&a),
            Some((
                "2 possible secrets in these commits will reach origin/main. Push anyway?",
                true
            ))
        );
        assert!(a.handle(key(KeyCode::Enter)).is_empty());
        assert!(a.handle(ch('y')).is_empty());
        assert_eq!(confirm(&a), Some(("scan skipped. Push anyway?", false)));
        let cmds = a.handle(ch('y'));
        assert_eq!(sent(&cmds), Some(&WriteOp::Push(p)));
    }

    #[test]
    fn prompt_swallows_keys() {
        let mut a = wapp(snap());
        a.handle(ch('C'));
        assert!(a.handle(ch('q')).is_empty());
        assert_eq!(input(&a), Some(("q", false)));
        assert!(a.handle(key(KeyCode::Esc)).is_empty());
        assert!(a.write.prompt.is_none());
    }

    #[test]
    fn click_outside_the_prompt_cancels_it() {
        let mut a = wapp(snap());
        a.hits.clicks.push((Rect::new(0, 0, 10, 1), Action::Quit));
        a.handle(ch('C'));
        let click = UiMsg::Input(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 1,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }));
        assert!(a.handle(click).is_empty(), "the click only cancels");
        assert!(a.write.prompt.is_none());
    }

    #[test]
    fn paste_into_prompt_does_not_submit() {
        let mut a = wapp(snap());
        let paste = |s: &str| UiMsg::Input(Event::Paste(s.into()));
        assert!(a.handle(paste("x")).is_empty(), "ignored without a prompt");
        a.handle(ch('C'));
        assert!(a.handle(paste("a\nb")).is_empty());
        assert_eq!(input(&a), Some(("a b", false)));
    }

    #[test]
    fn amend_toggle_restores_typed_text() {
        let mut a = wapp(snap());
        a.handle(ch('C'));
        typed(&mut a, "wip");
        a.handle(key(KeyCode::Tab));
        assert_eq!(input(&a), Some(("first commit", true)));
        a.handle(key(KeyCode::Tab));
        assert_eq!(input(&a), Some(("wip", false)));
        a.handle(key(KeyCode::Tab));
        let cmds = a.handle(key(KeyCode::Enter));
        assert_eq!(
            sent(&cmds),
            Some(&WriteOp::PrepareCommit {
                stage_all: false,
                amend: true,
            }),
            "HEAD is unpushed: no confirm"
        );

        let mut unborn = snap();
        unborn.head = Head::Unborn("main".into());
        unborn.commits.clear();
        let mut a = wapp(unborn);
        a.handle(ch('C'));
        a.handle(key(KeyCode::Tab));
        assert_eq!(input(&a), Some(("", false)), "nothing to amend");
    }

    #[test]
    fn amending_a_pushed_head_confirms_first() {
        let mut s = snap();
        s.commits[0].unpushed = false;
        s.changes.clear();
        let mut a = wapp(s);
        a.handle(ch('C'));
        assert_eq!(input(&a), Some(("first commit", true)), "already amending");
        assert!(a.handle(key(KeyCode::Enter)).is_empty());
        assert_eq!(
            confirm(&a),
            Some((
                "HEAD is pushed; amending will need a force-push. Amend anyway?",
                false
            ))
        );
        let cmds = a.handle(key(KeyCode::Enter));
        let prepare = WriteOp::PrepareCommit {
            stage_all: false,
            amend: true,
        };
        assert_eq!(sent(&cmds), Some(&prepare));
        let ready = CommitPlan {
            tree: "t".into(),
            head: Some("h1".into()),
            findings: vec![],
            coverage: Coverage::Full,
            amend: true,
        };
        let cmds = reply(&mut a, prepare, Ok(WriteOutcome::CommitReady(ready)));
        assert_eq!(
            sent(&cmds),
            Some(&WriteOp::Commit {
                message: "first commit".into(),
                amend: true,
                tree: "t".into(),
                head: Some("h1".into()),
            })
        );
    }

    #[test]
    fn refusal_shows_in_result() {
        let mut s = snap();
        s.op = Some(RepoOp::Merge);
        let mut a = wapp(s);
        for c in ['C', 'U', 'S'] {
            a.write.result = None;
            assert!(a.handle(ch(c)).is_empty(), "{c}");
            assert!(a.write.prompt.is_none(), "{c}");
            assert!(
                a.write
                    .result
                    .as_ref()
                    .unwrap()
                    .0
                    .contains("merge in progress"),
                "{c}"
            );
        }
        let mut s = snap();
        s.has_remote = false;
        let mut a = wapp(s);
        assert!(a.handle(ch('P')).is_empty());
        assert_eq!(
            a.write.result.as_ref().unwrap().0,
            crate::write::REFUSE_NO_REMOTE
        );
    }

    #[test]
    fn quit_waits_for_running_write() {
        let mut a = wapp(snap());
        a.handle(ch('P'));
        assert!(a.handle(ch('q')).is_empty());
        assert!(a.write.quitting);
        let cmds = reply(
            &mut a,
            WriteOp::PreparePush,
            Ok(WriteOutcome::PushReady(plan())),
        );
        assert!(matches!(cmds[..], [Cmd::Quit]));
        assert!(a.write.prompt.is_none(), "no confirm on the way out");

        let mut a = wapp(snap());
        a.handle(ch('U'));
        assert!(a.handle(ch('q')).is_empty());
        assert!(matches!(a.handle(ch('q'))[..], [Cmd::Quit]), "second q");
        let mut a = wapp(snap());
        a.handle(ch('U'));
        a.handle(ch('q'));
        let ctrl_c = UiMsg::Input(Event::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )));
        assert!(matches!(a.handle(ctrl_c)[..], [Cmd::Quit]), "Ctrl-C");
    }

    #[test]
    fn failure_and_bang_open_output() {
        let mut a = wapp(snap());
        assert!(a.handle(ch('!')).is_empty());
        assert!(a.stack.is_empty(), "no failure yet");
        a.handle(ch('U'));
        reply(
            &mut a,
            WriteOp::Pull,
            fail("pull failed: auth, pull from a shell", "fatal: no creds"),
        );
        assert_eq!(a.write.running, None);
        assert_eq!(
            a.write.failure.as_ref().map(|f| f.summary.as_str()),
            Some("pull failed: auth, pull from a shell")
        );
        assert!(a.handle(ch('!')).is_empty(), "no worker load");
        let top = a.stack.last().unwrap();
        assert_eq!(top.target, Target::Output);
        assert_eq!(
            top.data,
            Some(Ok(DetailData::Output("fatal: no creds".into())))
        );
        a.handle(ch('!'));
        assert_eq!(a.stack.len(), 1, "not opened twice");
    }

    #[test]
    fn soft_errors_and_successes_go_to_the_result() {
        let mut a = wapp(snap());
        for soft in [
            "nothing to commit",
            "nothing to push",
            "nothing to stash",
            "stashes changed, refreshed",
        ] {
            a.handle(ch('P'));
            reply(&mut a, WriteOp::PreparePush, fail(soft, ""));
            assert_eq!(a.write.result.as_ref().unwrap().0, soft);
            assert!(a.write.failure.is_none(), "{soft}");
        }
        a.handle(ch('U'));
        reply(
            &mut a,
            WriteOp::Pull,
            Ok(WriteOutcome::Done {
                summary: "popped stash@{0}".into(),
                warning: Some("index not restored".into()),
            }),
        );
        assert_eq!(
            a.write.result.as_ref().unwrap().0,
            "popped stash@{0} · index not restored"
        );
    }

    #[test]
    fn worker_refusals_without_output_go_to_the_result() {
        let mut a = wapp(snap());
        for refusal in [
            "several remotes, push from a shell",
            "no push target configured, push from a shell",
            "push target is not a branch, push from a shell",
            crate::write::REFUSE_NO_REMOTE,
            crate::write::REFUSE_DETACHED,
            crate::write::REFUSE_UNBORN,
            "HEAD moved, review again",
            "staged changes moved, review again",
        ] {
            a.handle(ch('P'));
            reply(&mut a, WriteOp::PreparePush, fail(refusal, ""));
            assert_eq!(a.write.result.as_ref().unwrap().0, refusal);
            assert!(a.write.failure.is_none(), "{refusal}");
        }
        // Git's own failure, with output to read, stays in the band.
        a.handle(ch('P'));
        reply(
            &mut a,
            WriteOp::PreparePush,
            fail("push failed: offline", "fatal: unable to access"),
        );
        assert!(a.write.failure.is_some());
    }

    #[test]
    fn failures_without_output_stay_in_the_band() {
        for summary in [
            "push failed: timeout",
            "pull failed: timeout",
            "commit failed: timeout",
            "timed out",
            "cancelled",
            "too much output",
            "git: No such file or directory (os error 2)",
            "git failed",
            "commit from editor did not complete",
        ] {
            let mut a = wapp(snap());
            a.handle(ch('P'));
            reply(&mut a, WriteOp::PreparePush, fail(summary, ""));
            let failure = a
                .write
                .failure
                .as_ref()
                .unwrap_or_else(|| panic!("{summary}"));
            assert_eq!(
                failure.warning(),
                summary,
                "no `! for output` without output"
            );
            assert!(a.write.result.is_none(), "{summary}");
            // `!` has nothing to open.
            let cmds = a.handle(ch('!'));
            assert!(cmds.is_empty());
            assert!(a.stack.is_empty(), "{summary}");
        }
    }

    #[test]
    fn a_failure_with_output_offers_it() {
        let mut a = wapp(snap());
        a.handle(ch('P'));
        reply(
            &mut a,
            WriteOp::PreparePush,
            fail("push failed: offline", "fatal: unable to access"),
        );
        assert_eq!(
            a.write.failure.as_ref().unwrap().warning(),
            "push failed: offline · ! for output"
        );
        a.handle(ch('!'));
        assert_eq!(a.stack.last().unwrap().target, Target::Output);
    }

    #[test]
    fn stage_toggle_by_file_state() {
        let cases = [
            (
                ('M', ' '),
                Some(WriteOp::Unstage {
                    path: "a.rs".into(),
                    unborn: false,
                }),
            ),
            (
                (' ', 'M'),
                Some(WriteOp::Stage {
                    path: "a.rs".into(),
                }),
            ),
            (
                ('M', 'M'),
                Some(WriteOp::Stage {
                    path: "a.rs".into(),
                }),
            ),
            (
                ('?', '?'),
                Some(WriteOp::Stage {
                    path: "a.rs".into(),
                }),
            ),
            (('U', 'U'), None),
        ];
        for ((x, y), want) in cases {
            let mut s = snap();
            s.changes = vec![change("a.rs", x, y)];
            let mut a = wapp(s);
            select_row(&mut a, SectionId::Changes, Target::File("a.rs".into()));
            let cmds = a.handle(ch('a'));
            assert_eq!(sent(&cmds), want.as_ref(), "{x}{y}");
            if want.is_none() {
                assert_eq!(
                    a.write.result.as_ref().unwrap().0,
                    "resolve conflicts in your editor or shell"
                );
            }
        }
        let mut s = snap();
        s.head = Head::Unborn("main".into());
        let mut a = wapp(s);
        select_row(&mut a, SectionId::Changes, Target::File("a.rs".into()));
        assert_eq!(
            sent(&a.handle(ch('a'))),
            Some(&WriteOp::Unstage {
                path: "a.rs".into(),
                unborn: true,
            })
        );
        assert_eq!(a.write.running, None, "staging has no label");
        assert!(a.handle(ch('a')).is_empty(), "one write at a time");
        reply(
            &mut a,
            WriteOp::Unstage {
                path: "a.rs".into(),
                unborn: true,
            },
            done("unstaged a.rs"),
        );
        assert_eq!(a.write.result.as_ref().unwrap().0, "unstaged a.rs");
    }

    fn hunk_line(text: &str) -> DiffLine {
        DiffLine {
            kind: DiffKind::Hunk,
            text: text.into(),
        }
    }

    fn add_line(text: &str) -> DiffLine {
        DiffLine {
            kind: DiffKind::Add,
            text: text.into(),
        }
    }

    fn block(title: &str, lines: Vec<DiffLine>) -> DiffBlock {
        DiffBlock {
            title: title.into(),
            lines,
        }
    }

    /// Opens `path`'s file view and loads `blocks` into it.
    fn file_view(a: &mut App, path: &str, blocks: Vec<DiffBlock>) {
        a.perform(Action::Open(Target::File(path.into())));
        reload(a, path, blocks);
    }

    fn detail_req(cmds: &[Cmd]) -> Option<&DetailReq> {
        cmds.iter().find_map(|c| match c {
            Cmd::Worker(WorkerMsg::Detail(r)) => Some(r),
            _ => None,
        })
    }

    fn staged_and_unstaged() -> Vec<DiffBlock> {
        vec![
            block(
                DiffBlock::STAGED,
                vec![hunk_line("@@ -1 +1 @@"), add_line("s")],
            ),
            block(
                DiffBlock::UNSTAGED,
                vec![
                    hunk_line("@@ -5 +5 @@"),
                    add_line("u"),
                    hunk_line("@@ -9 +9 @@"),
                    add_line("v"),
                ],
            ),
        ]
    }

    #[test]
    fn a_stages_or_unstages_the_selected_hunk() {
        let mut s = snap();
        s.changes = vec![change("a.rs", 'M', 'M')];
        let mut a = wapp(s);
        file_view(&mut a, "a.rs", staged_and_unstaged());
        let op = WriteOp::ApplyHunk {
            path: "a.rs".into(),
            hunk: ShownHunk {
                header: "@@ -1 +1 @@".into(),
                lines: vec![add_line("s")],
            },
            reverse: true,
        };
        assert_eq!(sent(&a.handle(ch('a'))), Some(&op), "from the staged block");
        assert_eq!(a.write.running, None, "staging has no label");
        let cmds = reply(&mut a, op, done("unstaged hunk"));
        assert_eq!(
            detail_req(&cmds),
            Some(&DetailReq::File {
                path: "a.rs".into()
            }),
            "the view reloads"
        );
        assert_eq!(a.write.result.as_ref().unwrap().0, "unstaged hunk");
        a.handle(ch(']'));
        a.handle(ch(']'));
        let cmds = a.handle(ch('a'));
        assert_eq!(
            sent(&cmds),
            Some(&WriteOp::ApplyHunk {
                path: "a.rs".into(),
                hunk: ShownHunk {
                    header: "@@ -9 +9 @@".into(),
                    lines: vec![add_line("v")],
                },
                reverse: false,
            })
        );
        let op = sent(&cmds).unwrap().clone();
        let cmds = reply(&mut a, op, fail("file changed, refreshed", ""));
        assert!(detail_req(&cmds).is_some(), "a stale view reloads too");
        assert_eq!(
            a.write.result.as_ref().unwrap().0,
            "file changed, refreshed"
        );
        // It reloads with one unstaged hunk: the nearest in its block.
        let mut fewer = staged_and_unstaged();
        fewer[1].lines.truncate(2);
        reload(&mut a, "a.rs", fewer);
        assert_eq!(a.stack[0].hunk, Some(1));
    }

    fn reload(a: &mut App, path: &str, blocks: Vec<DiffBlock>) {
        let req = DetailReq::File { path: path.into() };
        let data = DetailData::File {
            blocks,
            filtered: false,
        };
        a.handle(UiMsg::Detail(req, Ok(data)));
    }

    /// Unstaged hunks with these headers, one added line each.
    fn unstaged(headers: &[&str]) -> DiffBlock {
        let lines = headers
            .iter()
            .flat_map(|h| [hunk_line(h), add_line(h)])
            .collect();
        block(DiffBlock::UNSTAGED, lines)
    }

    fn staged(headers: &[&str]) -> DiffBlock {
        DiffBlock {
            title: DiffBlock::STAGED.into(),
            ..unstaged(headers)
        }
    }

    fn applied(cmds: &[Cmd]) -> (String, bool) {
        match sent(cmds) {
            Some(WriteOp::ApplyHunk { hunk, reverse, .. }) => (hunk.header.clone(), *reverse),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_walks_down_the_unstaged_hunks() {
        let mut s = snap();
        s.changes = vec![change("a.rs", ' ', 'M')];
        let mut a = wapp(s);
        file_view(&mut a, "a.rs", vec![unstaged(&["h0", "h1", "h2"])]);
        let cmds = a.handle(ch('a'));
        assert_eq!(applied(&cmds), ("h0".into(), false));
        reply(&mut a, sent(&cmds).unwrap().clone(), done("staged hunk"));
        reload(
            &mut a,
            "a.rs",
            vec![staged(&["h0"]), unstaged(&["h1", "h2"])],
        );
        let cmds = a.handle(ch('a'));
        assert_eq!(applied(&cmds), ("h1".into(), false), "not h0 back again");
        reply(&mut a, sent(&cmds).unwrap().clone(), done("staged hunk"));
        reload(
            &mut a,
            "a.rs",
            vec![staged(&["h0", "h1"]), unstaged(&["h2"])],
        );
        assert_eq!(applied(&a.handle(ch('a'))), ("h2".into(), false));
    }

    #[test]
    fn when_its_block_is_gone_the_nearest_hunk_is_selected() {
        let mut s = snap();
        s.changes = vec![change("a.rs", 'M', 'M')];
        let mut a = wapp(s);
        file_view(
            &mut a,
            "a.rs",
            vec![staged(&["s0"]), unstaged(&["u0", "u1"])],
        );
        let cmds = a.handle(ch('a'));
        assert_eq!(applied(&cmds), ("s0".into(), true));
        reply(&mut a, sent(&cmds).unwrap().clone(), done("unstaged hunk"));
        reload(&mut a, "a.rs", vec![unstaged(&["s0", "u0", "u1"])]);
        assert_eq!(a.stack[0].hunk, Some(0), "the first, next to where it was");
        a.handle(ch(']'));
        a.handle(ch(']'));
        let cmds = a.handle(ch('a'));
        reply(&mut a, sent(&cmds).unwrap().clone(), done("staged hunk"));
        reload(&mut a, "a.rs", vec![staged(&["s0", "u0", "u1"])]);
        assert_eq!(a.stack[0].hunk, Some(2), "the last, next to where it was");
    }

    #[test]
    fn a_filtered_file_stages_as_a_whole() {
        let mut s = snap();
        s.changes = vec![change("a.rs", ' ', 'M')];
        let mut a = wapp(s);
        a.perform(Action::Open(Target::File("a.rs".into())));
        let data = DetailData::File {
            blocks: vec![unstaged(&["h0"])],
            filtered: true,
        };
        let req = DetailReq::File {
            path: "a.rs".into(),
        };
        a.handle(UiMsg::Detail(req, Ok(data)));
        assert_eq!(
            sent(&a.handle(ch('a'))),
            Some(&WriteOp::Stage {
                path: "a.rs".into()
            })
        );
    }

    #[test]
    fn a_click_on_a_hunk_label_applies_it() {
        let mut s = snap();
        s.changes = vec![change("a.rs", 'M', 'M')];
        let mut a = wapp(s);
        file_view(&mut a, "a.rs", staged_and_unstaged());
        let cmds = a.perform(Action::Write(WriteAction::ToggleHunk(1)));
        assert!(matches!(
            sent(&cmds),
            Some(WriteOp::ApplyHunk { reverse: false, hunk, .. }) if hunk.header == "@@ -5 +5 @@"
        ));
    }

    #[test]
    fn a_on_untracked_detail_stages_whole_file() {
        let mut s = snap();
        s.changes = vec![change("n.txt", '?', '?')];
        let mut a = wapp(s);
        file_view(
            &mut a,
            "n.txt",
            vec![block(DiffBlock::UNTRACKED, vec![add_line("hello")])],
        );
        assert_eq!(
            sent(&a.handle(ch('a'))),
            Some(&WriteOp::Stage {
                path: "n.txt".into()
            })
        );
    }

    #[test]
    fn whole_file_where_hunks_do_not_apply() {
        let staged_rename = Change {
            orig_path: Some("old.rs".into()),
            ..change("a.rs", 'R', ' ')
        };
        let submodule = vec![block(
            DiffBlock::UNSTAGED,
            vec![
                hunk_line("@@ -1 +1 @@"),
                DiffLine {
                    kind: DiffKind::Del,
                    text: "Subproject commit 1111111".into(),
                },
                add_line("Subproject commit 2222222"),
            ],
        )];
        let mode_only = vec![];
        let binary = vec![block(
            DiffBlock::UNSTAGED,
            vec![DiffLine {
                kind: DiffKind::Meta,
                text: "binary file changed · 1 KB".into(),
            }],
        )];
        let cases = [
            (staged_rename, staged_and_unstaged(), "unstage"),
            (change("a.rs", ' ', 'M'), submodule, "stage"),
            (change("a.rs", ' ', 'M'), mode_only, "stage"),
            (change("a.rs", ' ', 'M'), binary, "stage"),
        ];
        for (row, blocks, want) in cases {
            let mut s = snap();
            s.changes = vec![row];
            let mut a = wapp(s);
            file_view(&mut a, "a.rs", blocks);
            let got = sent(&a.handle(ch('a'))).cloned();
            let want = if want == "stage" {
                WriteOp::Stage {
                    path: "a.rs".into(),
                }
            } else {
                WriteOp::Unstage {
                    path: "a.rs".into(),
                    unborn: false,
                }
            };
            assert_eq!(got, Some(want));
        }
        // Conflicted: the combined diff has hunks, but nothing is staged.
        let mut s = snap();
        s.changes = vec![change("a.rs", 'U', 'U')];
        let mut a = wapp(s);
        file_view(
            &mut a,
            "a.rs",
            vec![block(
                DiffBlock::UNSTAGED,
                vec![hunk_line("@@@ -1 -1 +1 @@@")],
            )],
        );
        assert!(a.handle(ch('a')).is_empty());
        assert_eq!(
            a.write.result.as_ref().unwrap().0,
            "resolve conflicts in your editor or shell"
        );
    }

    #[test]
    fn a_waits_for_the_file_view_to_load() {
        let mut a = wapp(snap());
        a.perform(Action::Open(Target::File("a.rs".into())));
        assert!(a.handle(ch('a')).is_empty());
    }

    #[test]
    fn stage_all_unless_all_staged() {
        let mut s = snap();
        s.changes = vec![change("a", 'M', ' '), change("b", 'A', ' ')];
        let mut a = wapp(s.clone());
        assert_eq!(
            sent(&a.handle(ch('A'))),
            Some(&WriteOp::UnstageAll { unborn: false })
        );
        s.changes.push(change("c", ' ', 'M'));
        let mut a = wapp(s.clone());
        assert_eq!(sent(&a.handle(ch('A'))), Some(&WriteOp::StageAll));
        s.changes.push(change("d", 'U', 'U'));
        let mut a = wapp(s);
        assert!(
            a.handle(ch('A')).is_empty(),
            "would mark conflicts resolved"
        );
        assert_eq!(
            a.write.result.as_ref().unwrap().0,
            "resolve conflicts in your editor or shell"
        );
    }

    #[test]
    fn stash_keys_on_a_row_and_in_its_view() {
        let mut a = wapp(snap());
        select_row(&mut a, SectionId::Stashes, Target::Stash(0));
        assert!(a.handle(ch('D')).is_empty());
        assert_eq!(
            confirm(&a),
            Some(("Drop stash@{0} \"WIP on main: x\"?", true))
        );
        assert!(a.handle(key(KeyCode::Enter)).is_empty());
        let drop = WriteOp::StashDrop {
            index: 0,
            oid: "s0".into(),
        };
        assert_eq!(sent(&a.handle(ch('y'))), Some(&drop));
        assert_eq!(a.write.running, Some(Verb::StashAction));
        reply(&mut a, drop, done("dropped stash@{0}"));

        let mut a = wapp(snap());
        open_stash(&mut a, 0, "s0", "WIP on main: x");
        assert_eq!(
            sent(&a.handle(ch('p'))),
            Some(&WriteOp::StashPop {
                index: 0,
                oid: "s0".into(),
            })
        );
        let mut a = wapp(snap());
        open_stash(&mut a, 0, "s0", "WIP on main: x");
        assert_eq!(
            sent(&a.handle(ch('o'))),
            Some(&WriteOp::StashApply {
                index: 0,
                oid: "s0".into(),
            })
        );
    }

    /// Opens stash@{index}'s view, loaded with the stash `oid`.
    fn open_stash(a: &mut App, index: usize, oid: &str, message: &str) {
        a.perform(Action::Open(Target::Stash(index)));
        a.stack.last_mut().unwrap().data = Some(Ok(DetailData::Commit(CommitDetail {
            oid: oid.into(),
            author: "a".into(),
            time: 0,
            message: format!("{message}\n\nbody"),
            files: vec![],
        })));
    }

    #[test]
    fn a_stash_view_acts_on_the_stash_it_shows() {
        // An agent stashed while stash@{0} was open: the snapshot's
        // stash@{0} is the agent's now, and the view's is stash@{1}.
        let mut s = snap();
        s.stashes.insert(
            0,
            Stash {
                index: 0,
                time: 0,
                message: "On main: agent".into(),
                oid: "s9".into(),
            },
        );
        s.stashes[1].index = 1;
        let mut a = wapp(s.clone());
        open_stash(&mut a, 0, "s0", "WIP on main: x");
        assert!(a.handle(ch('D')).is_empty());
        assert_eq!(
            confirm(&a),
            Some(("Drop stash@{0} \"WIP on main: x\"?", true))
        );
        assert_eq!(
            sent(&a.handle(ch('y'))),
            Some(&WriteOp::StashDrop {
                index: 0,
                oid: "s0".into(),
            }),
            "the backend refuses it: stashes changed"
        );
        for (c, want) in [
            (
                'p',
                WriteOp::StashPop {
                    index: 0,
                    oid: "s0".into(),
                },
            ),
            (
                'o',
                WriteOp::StashApply {
                    index: 0,
                    oid: "s0".into(),
                },
            ),
        ] {
            let mut a = wapp(s.clone());
            open_stash(&mut a, 0, "s0", "WIP on main: x");
            assert_eq!(sent(&a.handle(ch(c))), Some(&want), "{c}");
        }
        // On a Stashes row, the snapshot is what the row shows.
        let mut a = wapp(s);
        select_row(&mut a, SectionId::Stashes, Target::Stash(0));
        a.handle(ch('D'));
        assert_eq!(
            sent(&a.handle(ch('y'))),
            Some(&WriteOp::StashDrop {
                index: 0,
                oid: "s9".into(),
            })
        );
    }

    #[test]
    fn a_stash_view_does_nothing_until_it_has_loaded() {
        let mut a = wapp(snap());
        a.perform(Action::Open(Target::Stash(0)));
        for c in ['p', 'o', 'D'] {
            assert!(a.handle(ch(c)).is_empty(), "{c}");
            assert!(a.write.prompt.is_none(), "{c}");
        }
        assert!(
            a.perform(Action::Write(WriteAction::StashPop(0)))
                .is_empty(),
            "clicks too"
        );
    }

    #[test]
    fn stash_prompt() {
        let mut a = wapp(snap());
        a.handle(ch('S'));
        assert_eq!(input(&a), Some(("", false)));
        a.handle(key(KeyCode::Tab));
        assert_eq!(input(&a), Some(("", false)), "no amend for a stash");
        assert_eq!(
            sent(&a.handle(key(KeyCode::Enter))),
            Some(&WriteOp::Stash { message: None })
        );
        let mut a = wapp(snap());
        a.handle(ch('S'));
        typed(&mut a, "later");
        assert_eq!(
            sent(&a.handle(key(KeyCode::Enter))),
            Some(&WriteOp::Stash {
                message: Some("later".into())
            })
        );
        let mut s = snap();
        s.changes.clear();
        let mut a = wapp(s);
        a.handle(ch('S'));
        assert!(a.write.prompt.is_none());
        assert_eq!(a.write.result.as_ref().unwrap().0, "nothing to stash");
    }
    #[test]
    fn modifier_keys_do_not_write() {
        let with =
            |c: char, m: KeyModifiers| UiMsg::Input(Event::Key(KeyEvent::new(KeyCode::Char(c), m)));
        let mut a = wapp(snap());
        select_row(&mut a, SectionId::Stashes, Target::Stash(0));
        for c in ['p', 'o', 'D'] {
            assert!(
                a.handle(with(c, KeyModifiers::CONTROL)).is_empty(),
                "Ctrl-{c}"
            );
            assert!(a.handle(with(c, KeyModifiers::ALT)).is_empty(), "Alt-{c}");
            assert!(a.write.prompt.is_none(), "{c}");
        }
        let mut a = wapp(snap());
        select_row(&mut a, SectionId::Changes, Target::File("a.rs".into()));
        assert!(a.handle(with('a', KeyModifiers::ALT)).is_empty(), "Alt-a");
        assert!(
            a.handle(with('a', KeyModifiers::CONTROL)).is_empty(),
            "Ctrl-a"
        );
        assert!(
            a.handle(with('A', KeyModifiers::ALT | KeyModifiers::SHIFT))
                .is_empty()
        );
        assert_eq!(
            sent(&a.handle(with('P', KeyModifiers::SHIFT))),
            Some(&WriteOp::PreparePush),
            "Shift is how capitals arrive"
        );
    }

    #[test]
    fn a_prompt_or_confirm_closes_the_help_box() {
        let mut a = wapp(snap());
        a.handle(ch('P'));
        a.handle(ch('?'));
        assert!(a.help);
        reply(
            &mut a,
            WriteOp::PreparePush,
            Ok(WriteOutcome::PushReady(risky())),
        );
        assert!(!a.help, "the confirm is not behind the help box");
        assert_eq!(
            sent(&a.handle(ch('y'))),
            Some(&WriteOp::Push(risky())),
            "the key answers the confirm"
        );

        let mut a = wapp(snap());
        a.handle(ch('?'));
        a.perform(Action::Write(WriteAction::Commit));
        assert!(!a.help);
        assert!(input(&a).is_some());
    }

    #[test]
    fn popping_a_stash_closes_its_view() {
        let pop = WriteOp::StashPop {
            index: 0,
            oid: "s0".into(),
        };
        let mut a = wapp(snap());
        open_stash(&mut a, 0, "s0", "WIP on main: x");
        assert_eq!(sent(&a.handle(ch('p'))), Some(&pop));
        assert!(reply(&mut a, pop.clone(), done("popped stash@{0}")).is_empty());
        assert!(a.stack.is_empty(), "stash@{{0}} is now another stash");

        let apply = WriteOp::StashApply {
            index: 0,
            oid: "s0".into(),
        };
        let mut a = wapp(snap());
        open_stash(&mut a, 0, "s0", "WIP on main: x");
        a.handle(ch('o'));
        assert!(reply(&mut a, apply, done("applied stash@{0}")).is_empty());
        assert_eq!(a.stack.len(), 1, "an applied stash stays");

        let mut a = wapp(snap());
        a.perform(Action::Open(Target::Stash(1)));
        a.handle(UiMsg::Write(pop, done("popped stash@{0}")));
        assert_eq!(a.stack.len(), 1, "another stash's view stays");

        let mut a = wapp(snap());
        a.perform(Action::Open(Target::File("a.rs".into())));
        let stage = WriteOp::Stage {
            path: "a.rs".into(),
        };
        assert!(
            reply(&mut a, stage, done("staged a.rs")).is_empty(),
            "the snapshot reloads a file view"
        );
    }

    #[test]
    fn opening_output_is_the_same_as_show_output() {
        let mut a = app_with(false, snap());
        a.write.failure = Some(WriteError {
            summary: "x".into(),
            output: "out".into(),
        });
        assert!(a.perform(Action::Open(Target::Output)).is_empty());
        assert!(a.stack.is_empty(), "read-only");

        let mut a = wapp(snap());
        assert!(a.perform(Action::Open(Target::Output)).is_empty());
        assert!(a.stack.is_empty(), "no failure");
        a.write.failure = Some(WriteError {
            summary: "x".into(),
            output: "out".into(),
        });
        assert!(
            a.perform(Action::Open(Target::Output)).is_empty(),
            "no load"
        );
        assert_eq!(
            a.stack.last().unwrap().data,
            Some(Ok(DetailData::Output("out".into())))
        );
    }

    #[test]
    fn write_keys_need_a_pane_of_3_by_30() {
        for (w, h) in [(29, 10), (30, 2)] {
            let mut a = wapp(snap());
            a.drawn_size = Some((w, h));
            assert!(a.handle(ch('C')).is_empty());
            assert!(a.write.prompt.is_none(), "{w}x{h}");
            assert_eq!(a.write.result.as_ref().unwrap().0, "pane too small");
        }
        let mut a = wapp(snap());
        a.drawn_size = Some((30, 3));
        a.handle(ch('C'));
        assert!(a.write.prompt.is_some());
    }

    #[test]
    fn a_click_inside_the_prompt_row_keeps_it() {
        let mut a = wapp(snap());
        a.handle(ch('C'));
        a.write.prompt_rect = Some(Rect::new(0, 5, 20, 1));
        let click = |y| {
            UiMsg::Input(Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 3,
                row: y,
                modifiers: KeyModifiers::NONE,
            }))
        };
        assert!(a.handle(click(5)).is_empty());
        assert!(a.write.prompt.is_some());
        a.handle(click(4));
        assert!(a.write.prompt.is_none());
    }

    #[test]
    fn wakeups_follow_the_result_and_the_running_op() {
        let mut a = wapp(snap());
        assert!(a.next_wakeup() > Duration::from_secs(3));
        a.write.result = Some(("done".into(), Instant::now()));
        assert!(a.next_wakeup() <= Duration::from_secs(4));
        assert!(a.next_wakeup() > Duration::from_secs(2));
        a.write.result = None;
        a.write.running = Some(Verb::Push);
        assert!(a.next_wakeup() <= Duration::from_millis(100));
    }

    fn commit(oid: &str) -> Commit {
        Commit {
            oid: oid.into(),
            short: oid.into(),
            parents: 1,
            time: 0,
            author: "a".into(),
            refs: vec![],
            subject: format!("commit {oid}"),
            unpushed: true,
        }
    }

    fn outgoing_file(path: &str, added: u32, removed: u32) -> OutgoingFile {
        OutgoingFile {
            path: path.into(),
            status: 'M',
            counts: Counts::lines(added, removed),
        }
    }

    /// Two commits and three files, +10 −2, from `b0` to `c1`, to a new
    /// remote branch, so it confirms.
    fn sized_plan() -> PushPlan {
        PushPlan {
            count: 2,
            set_upstream: true,
            creates_branch: true,
            outgoing: Box::new(Outgoing {
                target: "origin/main".into(),
                base: "b0".into(),
                oid: "c1".into(),
                commits: vec![commit("c1"), commit("c0")],
                commit_total: 2,
                files: vec![
                    outgoing_file("src/a.rs", 6, 1),
                    outgoing_file("src/b.rs", 3, 1),
                    outgoing_file("src/c.rs", 1, 0),
                ],
                file_total: 3,
                added: 10,
                removed: 2,
            }),
            ..plan()
        }
    }

    /// Presses `P` and answers with `p`, as the worker would.
    fn push_ready(a: &mut App, p: &PushPlan) -> Vec<Cmd> {
        a.handle(ch('P'));
        reply(
            a,
            WriteOp::PreparePush,
            Ok(WriteOutcome::PushReady(p.clone())),
        )
    }

    /// The row targets drawing the Outgoing view registers: a rule, the
    /// commits, a rule, and the files.
    fn draw_outgoing(a: &mut App, o: &Outgoing) {
        let mut rows = vec![None];
        rows.extend(
            o.commits
                .iter()
                .map(|c| Some(Target::Commit(c.oid.clone()))),
        );
        rows.push(None);
        rows.extend(o.files.iter().map(|f| {
            Some(Target::RangeFile {
                base: o.base.clone(),
                oid: o.oid.clone(),
                path: f.path.clone(),
            })
        }));
        a.detail_targets = rows;
    }

    fn pinned(a: &App) -> bool {
        a.stack
            .iter()
            .any(|v| v.target == Target::Outgoing { pinned: true })
    }

    #[test]
    fn push_ready_opens_pinned_outgoing_view_and_sized_confirm() {
        let mut a = wapp(snap());
        let p = sized_plan();
        let cmds = push_ready(&mut a, &p);
        assert!(
            detail_req(&cmds).is_none(),
            "drawn from the plan, not loaded"
        );
        let top = a.stack.last().unwrap();
        assert_eq!(top.target, Target::Outgoing { pinned: true });
        assert_eq!(
            top.data,
            Some(Ok(DetailData::Outgoing(Some((*p.outgoing).clone()))))
        );
        assert_eq!(
            confirm(&a),
            Some((
                "Push 2 commits, 3 files (+10 −2) to origin/main and track it?",
                false
            ))
        );

        let mut one = sized_plan();
        one.count = 1;
        one.set_upstream = true;
        one.outgoing.file_total = 1;
        let mut a = wapp(snap());
        push_ready(&mut a, &one);
        assert_eq!(
            confirm(&a),
            Some((
                "Push 1 commit, 1 file (+10 −2) to origin/main and track it?",
                false
            ))
        );
    }

    #[test]
    fn navigation_keys_pass_through_while_confirming() {
        let mut a = wapp(snap());
        let p = sized_plan();
        push_ready(&mut a, &p);
        draw_outgoing(&mut a, &p.outgoing);
        let question = confirm(&a).unwrap().0.to_string();
        let selected = |a: &App| a.stack.last().unwrap().selected;
        for (code, want) in [
            (KeyCode::Char('j'), Some(1)),
            (KeyCode::Down, Some(2)),
            (KeyCode::Char('k'), Some(1)),
            (KeyCode::Up, Some(1)),
        ] {
            assert!(a.handle(key(code)).is_empty(), "{code:?}");
            assert_eq!(selected(&a), want, "{code:?}");
            assert_eq!(confirm(&a).map(|c| c.0), Some(question.as_str()));
        }
        a.page = 5;
        a.handle(key(KeyCode::PageDown));
        assert_eq!(a.stack.last().unwrap().scroll, 5);
        a.handle(key(KeyCode::PageUp));
        a.handle(ch('G'));
        assert!(a.stack.last().unwrap().scroll > 5);
        a.handle(ch('g'));
        assert_eq!(a.stack.last().unwrap().scroll, 0);
        a.handle(key(KeyCode::End));
        assert!(a.stack.last().unwrap().scroll > 5, "End as G");
        assert!(a.handle(ch('U')).is_empty(), "write keys are not");
        assert!(a.write.prompt.is_some() && a.stack.len() == 1);
        a.handle(ch('j'));
        let cmds = a.handle(key(KeyCode::Right));
        assert_eq!(
            detail_req(&cmds),
            Some(&DetailReq::Commit { rev: "c1".into() }),
            "→ as l"
        );
        assert_eq!(confirm(&a).map(|c| c.0), Some(question.as_str()));
    }

    #[test]
    fn enter_after_esc_before_a_redraw_does_not_push() {
        let mut a = wapp(snap());
        let p = sized_plan();
        push_ready(&mut a, &p);
        draw_outgoing(&mut a, &p.outgoing);
        a.handle(ch('j'));
        a.handle(key(KeyCode::Enter));
        a.handle(key(KeyCode::Esc));
        assert!(a.detail_targets.is_empty(), "not drawn yet");
        let cmds = a.handle(key(KeyCode::Enter));
        assert!(sent(&cmds).is_none(), "a row is selected");
        assert!(a.write.prompt.is_some());
        assert_eq!(
            a.stack.last().unwrap().target,
            Target::Outgoing { pinned: true }
        );
    }

    #[test]
    fn back_keys_pop_above_the_outgoing_view_and_cancel_at_it() {
        for code in [KeyCode::Char('h'), KeyCode::Left, KeyCode::Backspace] {
            let mut a = wapp(snap());
            push_ready(&mut a, &sized_plan());
            a.perform(Action::Open(Target::Commit("c1".into())));
            assert!(a.handle(key(code)).is_empty(), "{code:?}");
            assert!(a.write.prompt.is_some(), "{code:?}");
            assert_eq!(a.stack.len(), 1, "{code:?}");
            assert!(a.handle(key(code)).is_empty(), "{code:?}");
            assert!(a.write.prompt.is_none(), "{code:?}");
            assert!(a.stack.is_empty(), "{code:?}");
        }
    }

    #[test]
    fn esc_from_commit_inside_outgoing_keeps_the_confirm() {
        let mut a = wapp(snap());
        let p = sized_plan();
        push_ready(&mut a, &p);
        draw_outgoing(&mut a, &p.outgoing);
        a.handle(ch('j'));
        let cmds = a.handle(key(KeyCode::Enter));
        assert_eq!(
            detail_req(&cmds),
            Some(&DetailReq::Commit { rev: "c1".into() })
        );
        assert_eq!(a.stack.last().unwrap().target, Target::Commit("c1".into()));
        assert!(a.write.prompt.is_some());
        a.handle(key(KeyCode::Esc));
        assert_eq!(
            a.stack.last().unwrap().target,
            Target::Outgoing { pinned: true }
        );
        assert!(a.write.prompt.is_some());
        assert!(a.handle(key(KeyCode::Esc)).is_empty());
        assert!(a.write.prompt.is_none());
        assert!(a.stack.is_empty());

        // `n`, or a click outside the prompt row, cancels from any depth.
        let mut a = wapp(snap());
        push_ready(&mut a, &p);
        a.perform(Action::Open(Target::Commit("c1".into())));
        assert!(a.handle(ch('n')).is_empty());
        assert!(a.write.prompt.is_none() && a.stack.is_empty());
        let mut a = wapp(snap());
        push_ready(&mut a, &p);
        let click = UiMsg::Input(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 1,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }));
        a.handle(click);
        assert!(a.write.prompt.is_none() && a.stack.is_empty());
    }

    #[test]
    fn enter_on_the_outgoing_view_opens_a_row_and_never_accepts() {
        let mut a = wapp(snap());
        let p = PushPlan {
            findings: vec![leak("a")],
            ..sized_plan()
        };
        push_ready(&mut a, &p);
        let size = confirm(&a).unwrap().0.to_string();
        assert!(a.handle(key(KeyCode::Enter)).is_empty(), "no selection");
        assert_eq!(
            confirm(&a).map(|c| c.0),
            Some(size.as_str()),
            "not accepted"
        );
        assert!(a.handle(ch('y')).is_empty(), "y accepts the size step");
        assert_eq!(confirm(&a).map(|c| c.1), Some(true), "the secret step");
        assert!(a.handle(key(KeyCode::Enter)).is_empty(), "y only");
        assert_eq!(confirm(&a).map(|c| c.1), Some(true));
        // A view above the Outgoing view takes Enter as usual.
        a.perform(Action::Open(Target::Commit("c1".into())));
        a.detail_targets = vec![Some(Target::CommitFile {
            rev: "c1".into(),
            path: "src/a.rs".into(),
        })];
        a.handle(ch('j'));
        let cmds = a.handle(key(KeyCode::Enter));
        assert!(matches!(
            detail_req(&cmds),
            Some(DetailReq::CommitFile { .. })
        ));
        assert_eq!(a.stack.len(), 3);
        let cmds = a.handle(ch('y'));
        assert_eq!(sent(&cmds), Some(&WriteOp::Push(p)), "y from any view");
    }

    #[test]
    fn enter_after_paging_the_outgoing_view_does_not_push() {
        let mut a = wapp(snap());
        let p = sized_plan();
        push_ready(&mut a, &p);
        draw_outgoing(&mut a, &p.outgoing);
        a.page = 5;
        let question = confirm(&a).unwrap().0.to_string();
        // Paging drops the selection, so Enter has no row to open.
        for code in [KeyCode::Char('j'), KeyCode::Char(' '), KeyCode::Enter] {
            assert!(sent(&a.handle(key(code))).is_none(), "{code:?}");
        }
        assert_eq!(confirm(&a).map(|c| c.0), Some(question.as_str()));
        assert_eq!(a.stack.len(), 1);
        assert!(pinned(&a));
    }

    #[test]
    fn back_click_pops_above_the_outgoing_view_and_cancels_at_it() {
        let click = |column: u16, row: u16| {
            UiMsg::Input(Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                modifiers: KeyModifiers::NONE,
            }))
        };
        let mut a = wapp(snap());
        push_ready(&mut a, &sized_plan());
        a.perform(Action::Open(Target::Commit("c1".into())));
        // The header's `‹ back`, as drawing registers it.
        a.hits.clicks.push((Rect::new(0, 0, 7, 1), Action::Back));
        assert!(a.handle(click(1, 0)).is_empty());
        assert!(a.write.prompt.is_some(), "the confirm stays");
        assert_eq!(a.stack.len(), 1);
        assert!(pinned(&a));
        assert!(a.handle(click(1, 0)).is_empty());
        assert!(a.write.prompt.is_none(), "at the Outgoing view: cancels");
        assert!(a.stack.is_empty());

        // Any other click cancels, from any depth.
        let mut a = wapp(snap());
        push_ready(&mut a, &sized_plan());
        a.perform(Action::Open(Target::Commit("c1".into())));
        a.hits.clicks.push((Rect::new(0, 0, 7, 1), Action::Back));
        a.hits.clicks.push((Rect::new(10, 0, 5, 1), Action::Quit));
        assert!(a.handle(click(11, 0)).is_empty());
        assert!(a.write.prompt.is_none() && a.stack.is_empty());
    }

    #[test]
    fn y_pushes_and_closes_outgoing_after_result() {
        for result in [
            done("pushed to origin/main"),
            fail("push failed", "rejected"),
        ] {
            let mut a = wapp(snap());
            a.perform(Action::Open(Target::File("a.rs".into())));
            let p = sized_plan();
            push_ready(&mut a, &p);
            a.perform(Action::Open(Target::Commit("c1".into())));
            let cmds = a.handle(ch('y'));
            assert_eq!(sent(&cmds), Some(&WriteOp::Push(p.clone())));
            assert!(pinned(&a), "open while the push runs");
            reply(&mut a, WriteOp::Push(p), result);
            assert!(!pinned(&a));
            assert_eq!(a.stack.len(), 1, "views below it stay");
            assert_eq!(a.stack[0].target, Target::File("a.rs".into()));
        }

        // A push refused when it is sent closes the view at once.
        let mut a = wapp(snap());
        push_ready(&mut a, &sized_plan());
        let mut s = snap();
        s.head = Head::Detached("c1".into());
        a.snap = Some(Arc::new(s));
        assert!(a.handle(ch('y')).is_empty());
        assert!(!pinned(&a));
    }

    #[test]
    fn pinned_outgoing_is_not_reloaded_by_snapshots() {
        let mut a = wapp(snap());
        let p = sized_plan();
        push_ready(&mut a, &p);
        let moved = Snapshot {
            oid: Some("c9".into()),
            upstream: None,
            ..snap()
        };
        let cmds = a.handle(UiMsg::Snapshot {
            snap: Arc::new(moved),
            events: vec![],
            changed: vec![],
        });
        assert!(detail_req(&cmds).is_none());
        // A browse load arriving late doesn't replace the pinned data.
        a.handle(UiMsg::Detail(
            DetailReq::Outgoing,
            Ok(DetailData::Outgoing(None)),
        ));
        assert_eq!(
            a.stack.last().unwrap().data,
            Some(Ok(DetailData::Outgoing(Some((*p.outgoing).clone()))))
        );
    }

    #[test]
    fn file_row_opens_range_diff() {
        let mut a = wapp(snap());
        let p = sized_plan();
        push_ready(&mut a, &p);
        draw_outgoing(&mut a, &p.outgoing);
        for _ in 0..4 {
            a.handle(ch('j'));
        }
        let cmds = a.handle(key(KeyCode::Enter));
        let want = DetailReq::RangeFile {
            base: "b0".into(),
            oid: "c1".into(),
            path: "src/b.rs".into(),
        };
        assert_eq!(detail_req(&cmds), Some(&want));
        assert!(a.write.prompt.is_some());
        let cmds = a.handle(UiMsg::Snapshot {
            snap: Arc::new(Snapshot {
                oid: Some("c9".into()),
                ..snap()
            }),
            events: vec![],
            changed: vec!["src/b.rs".into()],
        });
        assert!(detail_req(&cmds).is_none(), "a range diff never changes");
    }

    #[test]
    fn a_routine_push_sends_without_asking() {
        let mut a = wapp(snap());
        let cmds = push_ready(&mut a, &plan());
        assert_eq!(sent(&cmds), Some(&WriteOp::Push(plan())));
        assert!(a.write.prompt.is_none());
        assert!(!pinned(&a), "no Outgoing view");
        assert_eq!(a.write.running, Some(Verb::Push));
    }

    #[test]
    fn a_risky_push_still_confirms() {
        for p in [
            PushPlan {
                findings: vec![leak("a")],
                ..plan()
            },
            PushPlan {
                coverage: Coverage::Partial("scan skipped".into()),
                ..plan()
            },
            PushPlan {
                creates_branch: true,
                ..plan()
            },
        ] {
            let mut a = wapp(snap());
            let cmds = push_ready(&mut a, &p);
            assert!(sent(&cmds).is_none(), "{p:?}");
            assert!(confirm(&a).is_some(), "{p:?}");
            assert!(pinned(&a), "{p:?}");
        }
    }

    #[test]
    fn setting_an_upstream_alone_is_routine() {
        // The remote has the branch: the push only records tracking.
        let p = PushPlan {
            set_upstream: true,
            ..plan()
        };
        let mut a = wapp(snap());
        let cmds = push_ready(&mut a, &p);
        assert_eq!(sent(&cmds), Some(&WriteOp::Push(p)));
        assert!(a.write.prompt.is_none());
    }

    #[test]
    fn push_now_from_unpinned_outgoing_sends_at_once() {
        let mut a = wapp(snap());
        a.perform(Action::Open(Target::Outgoing { pinned: false }));
        a.perform(Action::Write(WriteAction::Push));
        let cmds = reply(
            &mut a,
            WriteOp::PreparePush,
            Ok(WriteOutcome::PushReady(plan())),
        );
        assert_eq!(sent(&cmds), Some(&WriteOp::Push(plan())));
        assert_eq!(a.stack.len(), 1, "only the view the user opened");
        assert!(!pinned(&a));
    }
}
