//! UI state and input handling. Drawing fills in the hit map and navigation
//! list; input is resolved against whatever was drawn last.

use std::cell::Cell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};

use crate::activity::{ActivityEvent, ActivityKind, files_event};
use crate::config::Config;
use crate::model::{DetailData, DetailReq, DiffBlock, DiffKind, Snapshot};
use crate::ui::layout::SectionId;
use crate::worker::{FetchStatus, UiMsg, WorkerMsg};
use crate::write::ShownHunk;

mod write_flow;
pub use write_flow::{InputPurpose, Prompt, WriteAction, WriteState};
pub(crate) use write_flow::{count, staged_only};

/// An `index.lock` older than this is reported.
pub const STALE_LOCK: Duration = Duration::from_secs(10);
/// File events this close together (seconds) merge into one Activity row.
const BURST_SECS: i64 = 60;
/// Most live events kept for the Activity section.
const MAX_LIVE: usize = 200;
const SCROLL_STEP: usize = 3;

/// Something a row can open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    File(String),
    Commit(String),
    CommitFile {
        rev: String,
        path: String,
    },
    Branch {
        name: String,
        upstream: Option<String>,
    },
    Stash(usize),
    /// Possible secrets, drawn from the current snapshot.
    Leaks,
    /// The last failed write action's output, drawn from the failure.
    Output,
    /// What a push would send. Pinned: drawn from the push plan being
    /// confirmed, and never reloaded.
    Outgoing {
        pinned: bool,
    },
    /// One file's diff across an outgoing range.
    RangeFile {
        base: String,
        oid: String,
        path: String,
    },
}

impl Target {
    pub fn request(&self) -> DetailReq {
        match self {
            Target::File(path) => DetailReq::File { path: path.clone() },
            Target::Commit(rev) => DetailReq::Commit { rev: rev.clone() },
            Target::CommitFile { rev, path } => DetailReq::CommitFile {
                rev: rev.clone(),
                path: path.clone(),
            },
            Target::Branch {
                name,
                upstream: Some(u),
            } => DetailReq::Branch {
                name: name.clone(),
                upstream: u.clone(),
            },
            Target::Branch {
                name,
                upstream: None,
            } => DetailReq::Commit { rev: name.clone() },
            Target::Stash(i) => DetailReq::Commit {
                rev: format!("stash@{{{i}}}"),
            },
            Target::Leaks => DetailReq::Leaks,
            Target::Output => DetailReq::Output,
            Target::Outgoing { .. } => DetailReq::Outgoing,
            Target::RangeFile { base, oid, path } => DetailReq::RangeFile {
                base: base.clone(),
                oid: oid.clone(),
                path: path.clone(),
            },
        }
    }

    pub fn title(&self) -> String {
        match self {
            Target::File(p) => p.clone(),
            Target::Commit(rev) => format!("commit {}", rev.chars().take(7).collect::<String>()),
            Target::CommitFile { rev, path } => {
                format!("{} @ {}", path, rev.chars().take(7).collect::<String>())
            }
            Target::Branch { name, .. } => format!("branch {name}"),
            Target::Stash(i) => format!("stash@{{{i}}}"),
            Target::Leaks => "possible secrets".into(),
            Target::Output => "output".into(),
            Target::Outgoing { .. } => "outgoing".into(),
            Target::RangeFile { path, .. } => path.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Toggle(SectionId),
    Open(Target),
    Back,
    Fetch,
    Help,
    Quit,
    ToggleWrap,
    /// Hides these warnings until they change.
    Clear(Vec<String>),
    Write(WriteAction),
    /// Opens the last failed write action's output.
    ShowOutput,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollTarget {
    Section(SectionId),
    Detail,
}

/// Screen areas recorded during the last draw.
#[derive(Default, Debug)]
pub struct HitMap {
    pub clicks: Vec<(Rect, Action)>,
    pub scrolls: Vec<(Rect, ScrollTarget)>,
}

impl HitMap {
    pub fn clear(&mut self) {
        self.clicks.clear();
        self.scrolls.clear();
    }

    pub fn click_at(&self, x: u16, y: u16) -> Option<&Action> {
        self.clicks
            .iter()
            .find(|(r, _)| r.contains(Position::new(x, y)))
            .map(|(_, a)| a)
    }

    pub fn scroll_at(&self, x: u16, y: u16) -> Option<ScrollTarget> {
        self.scrolls
            .iter()
            .find(|(r, _)| r.contains(Position::new(x, y)))
            .map(|(_, t)| *t)
    }
}

/// A dashboard row the keyboard can select: a section title (`row: None`)
/// or a row with a target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NavItem {
    pub section: SectionId,
    pub row: Option<usize>,
}

pub struct DetailView {
    pub target: Target,
    pub req: DetailReq,
    pub data: Option<Result<DetailData, String>>,
    pub scroll: usize,
    pub selected: Option<usize>,
    /// The hunk cursor of a file diff: an index into `hunks`.
    pub hunk: Option<usize>,
    /// The hunk just staged or unstaged, as its block (staged or not) and
    /// its index there: the reload selects the hunk nearest to it.
    pub reselect: Option<(bool, usize)>,
}

impl DetailView {
    /// The diff blocks a file or commit-file view shows, once loaded.
    pub fn blocks(&self) -> Option<&[DiffBlock]> {
        match &self.data {
            Some(Ok(DetailData::File { blocks: b, .. } | DetailData::CommitFile(b))) => Some(b),
            _ => None,
        }
    }

    /// Whether the view shows a push plan's outgoing range: it keeps the
    /// data it opened with.
    pub fn pinned(&self) -> bool {
        self.target == Target::Outgoing { pinned: true }
    }

    /// Whether the file has a `filter` attribute.
    pub fn filtered(&self) -> bool {
        matches!(self.data, Some(Ok(DetailData::File { filtered: true, .. })))
    }

    /// The hunk to select once the view has (re)loaded: the first when it
    /// opens, the same index, clamped, when it reloads, and after a hunk
    /// was applied, the nearest to it in the block it left.
    fn place_cursor(&mut self) {
        let blocks = self.blocks().map(hunk_blocks).unwrap_or_default();
        let n = blocks.len();
        let reselect = self.reselect.take();
        if n == 0 {
            self.hunk = None;
            return;
        }
        self.hunk = Some(match reselect {
            Some((staged, k)) => {
                let same: Vec<usize> = (0..n).filter(|i| blocks[*i] == staged).collect();
                match same.get(k.min(same.len().saturating_sub(1))) {
                    Some(i) => *i,
                    // The block is gone: the staged block comes first.
                    None if staged => 0,
                    None => n - 1,
                }
            }
            None => self.hunk.unwrap_or(0).min(n - 1),
        });
    }
}

/// For each hunk `data` shows, in order, whether it is in the staged block.
fn hunk_blocks(data: &[DiffBlock]) -> Vec<bool> {
    data.iter()
        .flat_map(|b| b.lines.iter().map(move |l| (b.staged(), l)))
        .filter(|(_, l)| l.kind == DiffKind::Hunk)
        .map(|(staged, _)| staged)
        .collect()
}

/// Hunks of the open file view, in display order, with whether each is in the staged block.
fn hunks(data: &[DiffBlock]) -> Vec<(bool, ShownHunk)> {
    let mut out: Vec<(bool, ShownHunk)> = Vec::new();
    for block in data {
        let staged = block.staged();
        let mut in_hunk = false;
        for line in &block.lines {
            if line.kind == DiffKind::Hunk {
                let hunk = ShownHunk {
                    header: line.text.clone(),
                    lines: Vec::new(),
                };
                out.push((staged, hunk));
                in_hunk = true;
            } else if in_hunk && let Some((_, hunk)) = out.last_mut() {
                hunk.lines.push(line.clone());
            }
        }
    }
    out
}

pub enum Cmd {
    Quit,
    /// Hand the terminal back to the shell and stop until `fg`.
    Suspend,
    Worker(WorkerMsg),
    /// Commit through `git commit` and the user's editor in the terminal,
    /// starting from `text`, unless the index or HEAD moved from the
    /// confirmed `tree` and `head` (spec §7.5).
    Editor {
        amend: bool,
        text: String,
        tree: String,
        head: Option<String>,
    },
}

pub struct App {
    pub snap: Option<Arc<Snapshot>>,
    pub error: Option<String>,
    pub config_warning: Option<String>,
    /// Warnings cleared from view, by key. A key is dropped once its
    /// warning is gone, so the warning shows again if it comes back.
    pub cleared: HashSet<String>,
    /// Filled by draw: keys of the warnings that `c` clears.
    pub clearable: Vec<String>,
    /// The work tree's folder name, shown before the branch.
    pub repo_name: Option<String>,
    pub fetch: FetchStatus,
    pub write: WriteState,
    pub live: Vec<ActivityEvent>,
    pub pulses: HashMap<String, Instant>,
    /// Paths edited in the current burst of file events.
    burst: BTreeSet<String>,
    pub folded: HashSet<SectionId>,
    pub scroll: HashMap<SectionId, usize>,
    pub selected: Option<NavItem>,
    /// What the selected row opens, so the selection can follow that row
    /// when rows above it come or go.
    pub selected_target: Option<Target>,
    pub hover: Option<(u16, u16)>,
    pub stack: Vec<DetailView>,
    pub help: bool,
    pub wrap: bool,
    pub pulse_secs: u64,
    /// Wall-clock time used for ages; set before each draw.
    pub now: SystemTime,
    /// `now` when the current snapshot arrived.
    pub snap_at: SystemTime,
    /// Filled by draw.
    pub hits: HitMap,
    /// Filled by draw: selectable dashboard items in screen order.
    pub nav: Vec<NavItem>,
    /// Filled by draw: what each dashboard row opens.
    pub row_targets: HashMap<(SectionId, usize), Target>,
    /// Filled by draw: targets of the top detail view's lines.
    pub detail_targets: Vec<Option<Target>>,
    /// Filled by draw: the line of each hunk's `@@` in the top detail view.
    pub hunk_lines: Vec<usize>,
    /// Filled by draw: rows of the section or detail body, for paging.
    pub page: usize,
    /// Filled by draw: how long until an age on screen reads differently.
    pub age_wake: Cell<Option<Duration>>,
    /// Whether something on screen may have changed since the last draw.
    pub redraw: bool,
    /// Filled by draw: the size of the pane, as `(width, height)`. `None`
    /// before the first draw.
    pub drawn_size: Option<(u16, u16)>,
}

/// Whether an open view of `req` may be out of date once `next` replaces
/// `prev`, with `changed` the paths whose status, counts, or contents moved.
/// Commits, commit files, stashes, and range diffs never change. A pinned
/// Outgoing view is never reloaded, whatever this says.
fn stale(req: &DetailReq, prev: Option<&Snapshot>, next: &Snapshot, changed: &[String]) -> bool {
    let Some(prev) = prev else {
        return matches!(
            req,
            DetailReq::File { .. } | DetailReq::Branch { .. } | DetailReq::Outgoing
        );
    };
    match req {
        DetailReq::File { path } => {
            let listed = |s: &Snapshot| s.changes.iter().any(|c| &c.path == path);
            prev.oid != next.oid || changed.contains(path) || listed(prev) != listed(next)
        }
        DetailReq::Branch { .. } => {
            prev.oid != next.oid || prev.branches != next.branches || prev.reflog != next.reflog
        }
        DetailReq::Outgoing => {
            prev.oid != next.oid || prev.head != next.head || prev.upstream != next.upstream
        }
        _ => false,
    }
}

impl App {
    pub fn new(cfg: &Config, fetch: FetchStatus) -> App {
        let folded = SectionId::ALL
            .into_iter()
            .filter(|id| cfg.collapsed.iter().any(|c| c == id.key()))
            .collect();
        App {
            snap: None,
            error: None,
            config_warning: None,
            cleared: HashSet::new(),
            clearable: Vec::new(),
            repo_name: None,
            fetch,
            write: WriteState::new(cfg.write),
            live: Vec::new(),
            pulses: HashMap::new(),
            burst: BTreeSet::new(),
            folded,
            scroll: HashMap::new(),
            selected: None,
            selected_target: None,
            hover: None,
            stack: Vec::new(),
            help: false,
            wrap: false,
            pulse_secs: cfg.pulse_seconds,
            now: SystemTime::now(),
            snap_at: SystemTime::now(),
            hits: HitMap::default(),
            nav: Vec::new(),
            row_targets: HashMap::new(),
            detail_targets: Vec::new(),
            hunk_lines: Vec::new(),
            page: 10,
            age_wake: Cell::new(None),
            redraw: true,
            drawn_size: None,
        }
    }

    /// Whether to draw, clearing the request.
    pub fn take_redraw(&mut self) -> bool {
        std::mem::take(&mut self.redraw)
    }

    /// Selects a dashboard item, remembering what a row opens.
    fn select(&mut self, item: Option<NavItem>) {
        self.selected_target = item
            .and_then(|n| Some((n.section, n.row?)))
            .and_then(|key| self.row_targets.get(&key).cloned());
        self.selected = item;
    }

    /// The clickable area under the mouse pointer.
    pub(crate) fn hovered(&self) -> Option<Rect> {
        let (x, y) = self.hover?;
        self.hits
            .clicks
            .iter()
            .map(|(r, _)| *r)
            .find(|r| r.contains(Position::new(x, y)))
    }

    pub fn handle(&mut self, msg: UiMsg) -> Vec<Cmd> {
        // A pointer move redraws only when it changes what is highlighted.
        let moved = matches!(
            &msg,
            UiMsg::Input(Event::Mouse(m))
                if matches!(m.kind, MouseEventKind::Moved | MouseEventKind::Drag(_))
        );
        if !moved {
            self.redraw = true;
        }
        match msg {
            UiMsg::Input(Event::Key(k)) if k.kind != KeyEventKind::Release => self.key(k),
            UiMsg::Input(Event::Mouse(m)) => self.mouse(m),
            UiMsg::Input(Event::Paste(text)) => self.paste(&text),
            UiMsg::Input(_) => Vec::new(),
            UiMsg::Snapshot {
                snap,
                events,
                changed,
            } => {
                let reload = self
                    .stack
                    .last()
                    .filter(|v| !v.pinned())
                    .filter(|v| stale(&v.req, self.snap.as_deref(), &snap, &changed))
                    .map(|v| v.req.clone());
                self.snap = Some(snap);
                self.snap_at = self.now;
                self.error = None;
                for e in events {
                    self.add_live(e, &changed);
                }
                let excess = self.live.len().saturating_sub(MAX_LIVE);
                self.live.drain(..excess);
                let now = Instant::now();
                let ttl = Duration::from_secs(self.pulse_secs);
                self.pulses.retain(|_, t| now.duration_since(*t) < ttl);
                for path in changed {
                    self.pulses.insert(path, now);
                }
                reload
                    .map(|req| Cmd::Worker(WorkerMsg::Detail(req)))
                    .into_iter()
                    .collect()
            }
            UiMsg::RefreshError(e) => {
                self.error = Some(e);
                Vec::new()
            }
            UiMsg::Fetch(status) => {
                self.fetch = status;
                Vec::new()
            }
            UiMsg::Live(e) => {
                self.live.push(e);
                Vec::new()
            }
            UiMsg::Detail(req, data) => {
                // A pinned view keeps the plan's data, even if a browse
                // load of the same request lands while it is open.
                for v in self
                    .stack
                    .iter_mut()
                    .filter(|v| v.req == req && !v.pinned())
                {
                    v.data = Some(data.clone());
                    v.place_cursor();
                }
                Vec::new()
            }
            UiMsg::Suspend => vec![Cmd::Suspend],
            UiMsg::Quit => vec![Cmd::Quit],
            UiMsg::Wake => Vec::new(),
            UiMsg::Write(op, result) => self.write_done(&op, result),
        }
    }

    /// How long the UI may sleep before something on screen changes by itself.
    pub fn next_wakeup(&self) -> Duration {
        let ttl = Duration::from_secs(self.pulse_secs);
        // Wake when a held index.lock becomes old enough to report.
        let lock = self
            .lock_age()
            .filter(|a| *a < STALE_LOCK)
            .map(|a| (STALE_LOCK - a).max(Duration::from_millis(200)));
        let wake = self.base_wakeup(ttl);
        let wake = self.age_wake.get().map_or(wake, |a| a.min(wake));
        let wake = self.write_wakeup().map_or(wake, |w| w.min(wake));
        lock.map_or(wake, |l| l.min(wake))
    }

    /// Appends a live event. File events close together in time merge into
    /// one row, so an agent editing for a minute leaves one line, not dozens.
    fn add_live(&mut self, e: ActivityEvent, changed: &[String]) {
        if e.kind != ActivityKind::Files {
            self.live.push(e);
            return;
        }
        let joins = self.live.last().is_some_and(|last| {
            last.kind == ActivityKind::Files && e.time - last.time <= BURST_SECS
        });
        if !joins {
            self.burst.clear();
        }
        self.burst.extend(changed.iter().cloned());
        // Paths that are no longer changed drop out, so undoing most of a
        // burst leaves the row describing what remains.
        if let Some(snap) = &self.snap {
            self.burst
                .retain(|p| snap.changes.iter().any(|c| &c.path == p));
        }
        let paths: Vec<String> = self.burst.iter().cloned().collect();
        let e = match &self.snap {
            Some(snap) if !paths.is_empty() => files_event(&paths, snap, e.time),
            _ => e,
        };
        match self.live.last_mut() {
            Some(last) if joins => *last = e,
            _ => self.live.push(e),
        }
    }

    /// How long `index.lock` has existed, as of `now`.
    pub fn lock_age(&self) -> Option<Duration> {
        let since = self.now.duration_since(self.snap_at).unwrap_or_default();
        self.snap.as_ref()?.index_lock_age.map(|a| a + since)
    }

    fn base_wakeup(&self, ttl: Duration) -> Duration {
        if self.fetch.running {
            Duration::from_millis(100)
        } else if self.pulses.values().any(|t| t.elapsed() < ttl) {
            Duration::from_millis(500)
        } else {
            Duration::from_secs(30)
        }
    }

    pub fn perform(&mut self, action: Action) -> Vec<Cmd> {
        match action {
            Action::Toggle(id) => {
                if !self.folded.remove(&id) {
                    self.folded.insert(id);
                }
                Vec::new()
            }
            // Gated on write mode, and filled from the failure.
            Action::Open(Target::Output) => self.show_output(),
            Action::Open(target) => {
                let req = target.request();
                // The leaks view draws from the snapshot; nothing to load.
                let load = req != DetailReq::Leaks;
                self.push_view(target, req.clone(), None);
                if load {
                    vec![Cmd::Worker(WorkerMsg::Detail(req))]
                } else {
                    Vec::new()
                }
            }
            Action::Back => {
                if self.help {
                    self.help = false;
                } else {
                    self.stack.pop();
                    self.detail_targets.clear();
                    self.hunk_lines.clear();
                }
                Vec::new()
            }
            Action::Fetch => vec![Cmd::Worker(WorkerMsg::Fetch { manual: true })],
            Action::Help => {
                self.help = !self.help;
                Vec::new()
            }
            Action::Quit => vec![Cmd::Quit],
            Action::ToggleWrap => {
                self.wrap = !self.wrap;
                Vec::new()
            }
            Action::Clear(keys) => {
                self.cleared.extend(keys);
                Vec::new()
            }
            Action::Write(action) => self.write_action(action),
            Action::ShowOutput => self.show_output(),
        }
    }

    /// Opens a detail view on top, with `data` when it needs no load. The
    /// rows the last draw recorded belong to the view below.
    fn push_view(
        &mut self,
        target: Target,
        req: DetailReq,
        data: Option<Result<DetailData, String>>,
    ) {
        self.stack.push(DetailView {
            target,
            req,
            data,
            scroll: 0,
            selected: None,
            hunk: None,
            reselect: None,
        });
        self.detail_targets.clear();
        self.hunk_lines.clear();
    }

    /// Opens the possible-secrets view, unless there are none or it is
    /// already on top.
    fn open_leaks(&mut self) -> Vec<Cmd> {
        let any = self.snap.as_ref().is_some_and(|s| !s.leaks.is_empty());
        let on_top = self.stack.last().is_some_and(|v| v.target == Target::Leaks);
        if any && !on_top {
            self.perform(Action::Open(Target::Leaks))
        } else {
            Vec::new()
        }
    }

    fn key(&mut self, k: KeyEvent) -> Vec<Cmd> {
        if k.modifiers.contains(KeyModifiers::CONTROL) {
            // Raw mode turns these keys into input instead of signals.
            match k.code {
                KeyCode::Char('c') => return self.quit(),
                KeyCode::Char('z') => return vec![Cmd::Suspend],
                _ => {}
            }
        }
        if self.write.prompt.is_some() {
            return self.prompt_key(k);
        }
        if self.help {
            self.help = false;
            // The help box lists `q` as quit, so it quits from there too.
            return if k.code == KeyCode::Char('q') {
                self.quit()
            } else {
                Vec::new()
            };
        }
        match k.code {
            KeyCode::Char('q') => return self.quit(),
            KeyCode::Char('f') => return self.perform(Action::Fetch),
            KeyCode::Char('?') => return self.perform(Action::Help),
            KeyCode::Char('s') => return self.open_leaks(),
            KeyCode::Char('c') => return self.perform(Action::Clear(self.clearable.clone())),
            _ => {}
        }
        if let Some(cmds) = self.write_key(k) {
            return cmds;
        }
        if self.stack.is_empty() {
            self.dashboard_key(k.code)
        } else {
            self.detail_key(k.code)
        }
    }

    fn dashboard_key(&mut self, code: KeyCode) -> Vec<Cmd> {
        let pos = self
            .selected
            .and_then(|s| self.nav.iter().position(|n| *n == s));
        let last = self.nav.len().saturating_sub(1);
        let select = |app: &mut App, i: usize| app.select(app.nav.get(i).copied());
        match code {
            KeyCode::Char('j') | KeyCode::Down => {
                select(self, pos.map_or(0, |p| (p + 1).min(last)))
            }
            KeyCode::Char('k') | KeyCode::Up => {
                select(self, pos.map_or(0, |p| p.saturating_sub(1)))
            }
            KeyCode::Char('g') | KeyCode::Home => select(self, 0),
            KeyCode::Char('G') | KeyCode::End => select(self, last),
            KeyCode::Tab | KeyCode::BackTab => {
                let titles: Vec<usize> = (0..self.nav.len())
                    .filter(|i| self.nav[*i].row.is_none())
                    .collect();
                let cur = pos.unwrap_or(0);
                let next = if code == KeyCode::Tab {
                    titles
                        .iter()
                        .copied()
                        .find(|i| *i > cur)
                        .or(titles.first().copied())
                } else {
                    titles
                        .iter()
                        .rev()
                        .copied()
                        .find(|i| *i < cur)
                        .or(titles.last().copied())
                };
                if let Some(i) = next {
                    select(self, i);
                }
            }
            KeyCode::Char(' ') => {
                if let Some(s) = self.selected {
                    self.select(Some(NavItem {
                        section: s.section,
                        row: None,
                    }));
                    return self.perform(Action::Toggle(s.section));
                }
            }
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => match self.selected {
                Some(NavItem { section, row: None }) => {
                    return self.perform(Action::Toggle(section));
                }
                Some(NavItem {
                    section,
                    row: Some(r),
                }) => {
                    if let Some(t) = self.row_targets.get(&(section, r)).cloned() {
                        return self.perform(Action::Open(t));
                    }
                }
                None => {}
            },
            KeyCode::Esc | KeyCode::Char('h') | KeyCode::Left | KeyCode::Backspace => {
                self.select(None)
            }
            _ => {}
        }
        Vec::new()
    }

    fn detail_key(&mut self, code: KeyCode) -> Vec<Cmd> {
        let selectable: Vec<usize> = (0..self.detail_targets.len())
            .filter(|i| self.detail_targets[*i].is_some())
            .collect();
        let page = self.page.max(1);
        let hunk_lines = &self.hunk_lines;
        let Some(view) = self.stack.last_mut() else {
            return Vec::new();
        };
        let step = |view: &mut DetailView, down: bool, n: usize| {
            if selectable.is_empty() {
                view.scroll = if down {
                    view.scroll + n
                } else {
                    view.scroll.saturating_sub(n)
                };
                return;
            }
            let cur = view
                .selected
                .and_then(|s| selectable.iter().position(|i| *i == s));
            let idx = match (cur, down) {
                (None, _) => 0,
                (Some(c), true) => (c + n).min(selectable.len() - 1),
                (Some(c), false) => c.saturating_sub(n),
            };
            view.selected = Some(selectable[idx]);
        };
        match code {
            KeyCode::Esc | KeyCode::Char('h') | KeyCode::Backspace | KeyCode::Left => {
                return self.perform(Action::Back);
            }
            KeyCode::Char('j') | KeyCode::Down => step(view, true, 1),
            KeyCode::Char('k') | KeyCode::Up => step(view, false, 1),
            KeyCode::PageDown | KeyCode::Char(' ') => {
                view.selected = None;
                view.scroll += page;
            }
            KeyCode::PageUp => {
                view.selected = None;
                view.scroll = view.scroll.saturating_sub(page);
            }
            // A selected row would pull the view back to it on the next
            // draw, so jumping drops the selection, as paging does.
            KeyCode::Char('g') | KeyCode::Home => {
                view.selected = None;
                view.scroll = 0;
            }
            KeyCode::Char('G') | KeyCode::End => {
                view.selected = None;
                view.scroll = usize::MAX / 2;
            }
            KeyCode::Char(']') | KeyCode::Char('[') => {
                let n = view.blocks().map_or(0, |b| hunk_blocks(b).len());
                let Some(cur) = view.hunk.filter(|_| n > 0) else {
                    return Vec::new();
                };
                let next = if code == KeyCode::Char(']') {
                    (cur + 1).min(n - 1)
                } else {
                    cur.saturating_sub(1)
                };
                view.hunk = Some(next);
                // Out of view, its `@@` line goes to the top.
                if let Some(&line) = hunk_lines.get(next)
                    && (line < view.scroll || line >= view.scroll + page)
                {
                    view.scroll = line;
                }
            }
            KeyCode::Char('w') => return self.perform(Action::ToggleWrap),
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                let target = view
                    .selected
                    .and_then(|i| self.detail_targets.get(i).cloned().flatten());
                if let Some(t) = target {
                    return self.perform(Action::Open(t));
                }
            }
            _ => {}
        }
        Vec::new()
    }

    fn mouse(&mut self, m: MouseEvent) -> Vec<Cmd> {
        let (x, y) = (m.column, m.row);
        if matches!(m.kind, MouseEventKind::Down(_)) && self.write.prompt.is_some() {
            // A click on the prompt row is its own; elsewhere it cancels,
            // but for the header's `‹ back` above the push confirm's view,
            // and its `Push now`.
            if !self.in_prompt_row(x, y) {
                if m.kind == MouseEventKind::Down(MouseButton::Left) {
                    return self.click_while_prompting(x, y);
                } else {
                    self.cancel_prompt();
                }
            }
            return Vec::new();
        }
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if self.help {
                    self.help = false;
                    return Vec::new();
                }
                match self.hits.click_at(x, y).cloned() {
                    Some(action) => self.perform(action),
                    None => Vec::new(),
                }
            }
            MouseEventKind::Down(MouseButton::Right) => self.perform(Action::Back),
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let down = m.kind == MouseEventKind::ScrollDown;
                let apply = |v: &mut usize| {
                    *v = if down {
                        *v + SCROLL_STEP
                    } else {
                        v.saturating_sub(SCROLL_STEP)
                    }
                };
                match self.hits.scroll_at(x, y) {
                    // Scrolling moves the view away from a selected row, so the
                    // selection steps back to the section title (or is dropped)
                    // instead of pulling the view back on the next draw.
                    Some(ScrollTarget::Section(id)) => {
                        if self.selected.is_some_and(|s| s.section == id) {
                            self.select(Some(NavItem {
                                section: id,
                                row: None,
                            }));
                        }
                        apply(self.scroll.entry(id).or_insert(0));
                    }
                    Some(ScrollTarget::Detail) => {
                        if let Some(v) = self.stack.last_mut() {
                            v.selected = None;
                            apply(&mut v.scroll);
                        }
                    }
                    None => {}
                }
                Vec::new()
            }
            MouseEventKind::Moved | MouseEventKind::Drag(_) => {
                let before = self.hovered();
                self.hover = Some((x, y));
                self.redraw |= self.hovered() != before;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_mode_follows_config() {
        let cfg = Config {
            write: true,
            ..Config::default()
        };
        assert!(App::new(&cfg, FetchStatus::default()).write.enabled);
        assert!(!app().write.enabled);
    }

    fn app() -> App {
        App::new(&Config::default(), FetchStatus::default())
    }

    fn key(code: KeyCode) -> UiMsg {
        UiMsg::Input(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn ch(c: char) -> UiMsg {
        key(KeyCode::Char(c))
    }

    fn mouse(kind: MouseEventKind, x: u16, y: u16) -> UiMsg {
        UiMsg::Input(Event::Mouse(MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        }))
    }

    fn click(x: u16, y: u16) -> UiMsg {
        mouse(MouseEventKind::Down(MouseButton::Left), x, y)
    }

    fn detail_req(cmds: &[Cmd]) -> Option<&DetailReq> {
        cmds.iter().find_map(|c| match c {
            Cmd::Worker(WorkerMsg::Detail(r)) => Some(r),
            _ => None,
        })
    }

    #[test]
    fn quit_and_fetch_keys() {
        let mut a = app();
        assert!(matches!(
            a.handle(ch('f'))[..],
            [Cmd::Worker(WorkerMsg::Fetch { manual: true })]
        ));
        assert!(matches!(a.handle(ch('q'))[..], [Cmd::Quit]));
        let ctrl_c = UiMsg::Input(Event::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )));
        assert!(matches!(a.handle(ctrl_c)[..], [Cmd::Quit]));
        let ctrl_z = UiMsg::Input(Event::Key(KeyEvent::new(
            KeyCode::Char('z'),
            KeyModifiers::CONTROL,
        )));
        assert!(matches!(a.handle(ctrl_z)[..], [Cmd::Suspend]));
        assert!(matches!(a.handle(UiMsg::Quit)[..], [Cmd::Quit]));
        assert!(matches!(a.handle(UiMsg::Suspend)[..], [Cmd::Suspend]));
    }

    #[test]
    fn default_folds_from_config() {
        let a = app();
        assert!(a.folded.contains(&SectionId::Branches) && a.folded.contains(&SectionId::Stashes));
        assert!(!a.folded.contains(&SectionId::Changes));
    }

    #[test]
    fn click_opens_detail_and_back_returns() {
        let mut a = app();
        a.hits.clicks.push((
            Rect::new(0, 5, 20, 1),
            Action::Open(Target::File("a.rs".into())),
        ));
        let cmds = a.handle(click(3, 5));
        assert_eq!(
            detail_req(&cmds),
            Some(&DetailReq::File {
                path: "a.rs".into()
            })
        );
        assert_eq!(a.stack.len(), 1);
        a.handle(ch('h'));
        assert!(a.stack.is_empty());
        a.handle(click(3, 5));
        a.handle(mouse(MouseEventKind::Down(MouseButton::Right), 0, 0));
        assert!(a.stack.is_empty());
    }

    #[test]
    fn click_outside_hits_does_nothing() {
        let mut a = app();
        a.hits.clicks.push((Rect::new(0, 5, 20, 1), Action::Quit));
        assert!(a.handle(click(3, 6)).is_empty());
    }

    #[test]
    fn toggle_section_by_click() {
        let mut a = app();
        a.hits
            .clicks
            .push((Rect::new(0, 2, 30, 1), Action::Toggle(SectionId::Changes)));
        a.handle(click(0, 2));
        assert!(a.folded.contains(&SectionId::Changes));
        a.handle(click(0, 2));
        assert!(!a.folded.contains(&SectionId::Changes));
    }

    #[test]
    fn stash_and_upstreamless_branch_map_to_commit_requests() {
        assert_eq!(
            Target::Stash(2).request(),
            DetailReq::Commit {
                rev: "stash@{2}".into()
            }
        );
        assert_eq!(
            Target::Branch {
                name: "feat".into(),
                upstream: None
            }
            .request(),
            DetailReq::Commit { rev: "feat".into() }
        );
    }

    #[test]
    fn detail_data_attaches_to_matching_view() {
        let mut a = app();
        a.hits.clicks.push((
            Rect::new(0, 0, 10, 1),
            Action::Open(Target::Commit("abc".into())),
        ));
        a.handle(click(0, 0));
        a.handle(UiMsg::Detail(
            DetailReq::Commit {
                rev: "other".into(),
            },
            Err("x".into()),
        ));
        assert!(a.stack[0].data.is_none());
        a.handle(UiMsg::Detail(
            DetailReq::Commit { rev: "abc".into() },
            Err("boom".into()),
        ));
        assert_eq!(a.stack[0].data, Some(Err("boom".into())));
    }

    #[test]
    fn snapshot_sets_pulses_and_refreshes_open_file() {
        let mut a = app();
        a.hits.clicks.push((
            Rect::new(0, 0, 10, 1),
            Action::Open(Target::File("a".into())),
        ));
        a.handle(click(0, 0));
        let cmds = a.handle(UiMsg::Snapshot {
            snap: Arc::new(Snapshot::default()),
            events: vec![],
            changed: vec!["a".into()],
        });
        assert!(a.pulses.contains_key("a"));
        assert_eq!(
            detail_req(&cmds),
            Some(&DetailReq::File { path: "a".into() })
        );
        assert!(a.snap.is_some());
        assert!(a.next_wakeup() <= Duration::from_secs(1));
    }

    #[test]
    fn an_open_file_reloads_only_when_it_may_have_changed() {
        let mut a = app();
        let change = |path: &str, added| crate::model::Change {
            path: path.into(),
            orig_path: None,
            x: ' ',
            y: 'M',
            counts: crate::model::Counts::lines(added, 0),
            modified: None,
        };
        let snap = |changes| UiMsg::Snapshot {
            snap: Arc::new(Snapshot {
                changes,
                oid: Some("c1".into()),
                ..Snapshot::default()
            }),
            events: vec![],
            changed: vec![],
        };
        a.handle(snap(vec![change("a", 1), change("b", 1)]));
        a.perform(Action::Open(Target::File("a".into())));
        assert!(detail_req(&a.handle(snap(vec![change("a", 1), change("b", 2)]))).is_none());
        let edited = UiMsg::Snapshot {
            snap: Arc::new(Snapshot {
                changes: vec![change("a", 1)],
                oid: Some("c1".into()),
                ..Snapshot::default()
            }),
            events: vec![],
            changed: vec!["a".into()],
        };
        assert!(detail_req(&a.handle(edited)).is_some(), "edited");
        let committed = UiMsg::Snapshot {
            snap: Arc::new(Snapshot {
                oid: Some("c2".into()),
                ..Snapshot::default()
            }),
            events: vec![],
            changed: vec![],
        };
        assert!(detail_req(&a.handle(committed)).is_some(), "committed");
    }

    /// A staged hunk, then two unstaged ones.
    fn three_hunks() -> DetailData {
        use crate::model::{DiffBlock, DiffKind, DiffLine};
        let block = |title: &str, headers: &[&str]| DiffBlock {
            title: title.into(),
            lines: headers
                .iter()
                .flat_map(|h| {
                    [
                        DiffLine {
                            kind: DiffKind::Hunk,
                            text: h.to_string(),
                        },
                        DiffLine {
                            kind: DiffKind::Add,
                            text: "x".into(),
                        },
                    ]
                })
                .collect(),
        };
        file(vec![
            block(DiffBlock::STAGED, &["@@ -1 +1 @@"]),
            block(DiffBlock::UNSTAGED, &["@@ -5 +5 @@", "@@ -9 +9 @@"]),
        ])
    }

    fn file(blocks: Vec<crate::model::DiffBlock>) -> DetailData {
        DetailData::File {
            blocks,
            filtered: false,
        }
    }

    #[test]
    fn bracket_keys_move_hunk_in_read_only() {
        let mut a = app();
        let req = DetailReq::File { path: "a".into() };
        a.perform(Action::Open(Target::File("a".into())));
        assert!(a.handle(ch(']')).is_empty(), "nothing loaded yet");
        assert_eq!(a.stack[0].hunk, None);
        a.handle(UiMsg::Detail(req.clone(), Ok(three_hunks())));
        assert_eq!(a.stack[0].hunk, Some(0), "opening selects the first hunk");
        for (k, want) in [(']', 1), (']', 2), (']', 2), ('[', 1), ('[', 0), ('[', 0)] {
            assert!(a.handle(ch(k)).is_empty(), "{k}");
            assert_eq!(a.stack[0].hunk, Some(want), "{k}");
        }
        assert!(a.handle(ch('a')).is_empty(), "no write in read-only mode");
        // Moving scrolls the `@@` line into view, as the last draw laid it out.
        a.hunk_lines = vec![1, 30, 60];
        a.page = 10;
        a.handle(ch(']'));
        assert_eq!(a.stack[0].scroll, 30);
        a.handle(ch('['));
        assert_eq!(a.stack[0].scroll, 1);
        // A reload keeps the index, clamped to the hunks there are now.
        a.stack[0].hunk = Some(2);
        let one = match three_hunks() {
            DetailData::File { mut blocks, .. } => {
                blocks.truncate(1);
                file(blocks)
            }
            _ => unreachable!(),
        };
        a.handle(UiMsg::Detail(req.clone(), Ok(one)));
        assert_eq!(a.stack[0].hunk, Some(0));
        a.handle(UiMsg::Detail(req, Ok(file(vec![]))));
        assert_eq!(a.stack[0].hunk, None);
        assert!(a.handle(ch(']')).is_empty());
        assert_eq!(a.stack[0].hunk, None, "no hunks: the keys do nothing");
    }

    #[test]
    fn keyboard_navigation_over_nav_items() {
        let mut a = app();
        a.nav = vec![
            NavItem {
                section: SectionId::Changes,
                row: None,
            },
            NavItem {
                section: SectionId::Changes,
                row: Some(0),
            },
            NavItem {
                section: SectionId::Commits,
                row: None,
            },
        ];
        a.handle(ch('j'));
        assert_eq!(a.selected, Some(a.nav[0]));
        a.handle(key(KeyCode::Down));
        assert_eq!(a.selected, Some(a.nav[1]));
        a.handle(key(KeyCode::Tab));
        assert_eq!(a.selected, Some(a.nav[2]));
        a.handle(ch('j'));
        assert_eq!(a.selected, Some(a.nav[2]), "stays on the last item");
        a.handle(ch(' '));
        assert!(a.folded.contains(&SectionId::Commits));
        a.handle(key(KeyCode::BackTab));
        assert_eq!(a.selected, Some(a.nav[0]));
    }

    #[test]
    fn enter_opens_selected_row_target() {
        let mut a = app();
        a.nav = vec![NavItem {
            section: SectionId::Changes,
            row: Some(0),
        }];
        a.selected = Some(a.nav[0]);
        a.hits.clicks.push((
            Rect::new(0, 3, 10, 1),
            Action::Open(Target::File("x".into())),
        ));
        a.row_targets
            .insert((SectionId::Changes, 0), Target::File("x".into()));
        let cmds = a.handle(key(KeyCode::Enter));
        assert_eq!(
            detail_req(&cmds),
            Some(&DetailReq::File { path: "x".into() })
        );
    }

    #[test]
    fn wheel_scrolls_region_under_pointer() {
        let mut a = app();
        a.hits.scrolls.push((
            Rect::new(0, 4, 30, 5),
            ScrollTarget::Section(SectionId::Commits),
        ));
        a.handle(mouse(MouseEventKind::ScrollDown, 2, 5));
        assert_eq!(a.scroll.get(&SectionId::Commits), Some(&3));
        a.handle(mouse(MouseEventKind::ScrollUp, 2, 5));
        a.handle(mouse(MouseEventKind::ScrollUp, 2, 5));
        assert_eq!(a.scroll.get(&SectionId::Commits), Some(&0));
    }

    #[test]
    fn dashboard_back_keys_clear_selection() {
        let mut a = app();
        a.nav = vec![NavItem {
            section: SectionId::Changes,
            row: None,
        }];
        for code in [
            KeyCode::Esc,
            KeyCode::Char('h'),
            KeyCode::Left,
            KeyCode::Backspace,
        ] {
            a.selected = Some(a.nav[0]);
            a.handle(key(code));
            assert_eq!(a.selected, None, "{code:?}");
        }
    }

    #[test]
    fn q_quits_while_help_is_open() {
        let mut a = app();
        a.handle(ch('?'));
        assert!(matches!(a.handle(ch('q'))[..], [Cmd::Quit]));
    }

    #[test]
    fn hover_redraws_only_when_its_target_changes() {
        let mut a = app();
        a.hits
            .clicks
            .push((Rect::new(0, 2, 10, 1), Action::Toggle(SectionId::Changes)));
        a.hits
            .clicks
            .push((Rect::new(0, 3, 10, 1), Action::Toggle(SectionId::Commits)));
        let moved = |x, y| mouse(MouseEventKind::Moved, x, y);
        a.take_redraw();
        a.handle(moved(1, 2));
        assert!(a.take_redraw(), "onto a target");
        a.handle(moved(5, 2));
        assert!(!a.take_redraw(), "within the same target");
        a.handle(moved(5, 3));
        assert!(a.take_redraw(), "onto another target");
        a.handle(moved(5, 9));
        assert!(a.take_redraw(), "off every target");
        a.handle(moved(6, 9));
        assert!(!a.take_redraw(), "still off every target");
        a.handle(ch('j'));
        assert!(a.take_redraw(), "any other input");
    }

    #[test]
    fn help_toggles_and_any_key_closes() {
        let mut a = app();
        a.handle(ch('?'));
        assert!(a.help);
        assert!(a.handle(ch('x')).is_empty());
        assert!(!a.help);
    }

    #[test]
    fn detail_keys_scroll_and_wrap() {
        let mut a = app();
        a.hits.clicks.push((
            Rect::new(0, 0, 10, 1),
            Action::Open(Target::File("a".into())),
        ));
        a.handle(click(0, 0));
        a.handle(ch('j'));
        a.handle(ch('j'));
        assert_eq!(a.stack[0].scroll, 2);
        a.handle(ch('k'));
        assert_eq!(a.stack[0].scroll, 1);
        a.handle(ch('w'));
        assert!(a.wrap);
        a.handle(key(KeyCode::Esc));
        assert!(a.stack.is_empty());
    }

    #[test]
    fn live_and_fetch_and_errors_are_stored() {
        let mut a = app();
        a.handle(UiMsg::Live(ActivityEvent {
            time: 1,
            kind: ActivityKind::Fetch,
            text: "x".into(),
            rev: None,
            path: None,
        }));
        assert_eq!(a.live.len(), 1);
        a.handle(UiMsg::Fetch(FetchStatus {
            running: true,
            ..FetchStatus::default()
        }));
        assert!(a.fetch.running);
        assert!(a.next_wakeup() <= Duration::from_millis(200));
        a.handle(UiMsg::RefreshError("bad".into()));
        assert_eq!(a.error.as_deref(), Some("bad"));
    }

    fn files_snapshot(
        a: &mut App,
        changes: &[(&str, u32)],
        time: i64,
        text: &str,
        changed: &[&str],
    ) {
        let changes = changes
            .iter()
            .map(|(p, n)| crate::model::Change {
                path: p.to_string(),
                orig_path: None,
                x: ' ',
                y: 'M',
                counts: crate::model::Counts::lines(*n, 0),
                modified: None,
            })
            .collect();
        let event = ActivityEvent {
            time,
            kind: ActivityKind::Files,
            text: text.into(),
            rev: None,
            path: None,
        };
        a.handle(UiMsg::Snapshot {
            snap: Arc::new(Snapshot {
                changes,
                ..Snapshot::default()
            }),
            events: vec![event],
            changed: changed.iter().map(|s| s.to_string()).collect(),
        });
    }

    #[test]
    fn file_edits_in_a_burst_coalesce() {
        let mut a = app();
        files_snapshot(&mut a, &[("a.rs", 1)], 100, "a.rs +1", &["a.rs"]);
        files_snapshot(&mut a, &[("a.rs", 3)], 110, "a.rs +3", &["a.rs"]);
        assert_eq!(a.live.len(), 1);
        assert_eq!(a.live[0].text, "a.rs +3");
        files_snapshot(
            &mut a,
            &[("a.rs", 3), ("b.rs", 1)],
            120,
            "b.rs +1",
            &["b.rs"],
        );
        assert_eq!(a.live.len(), 1);
        assert_eq!(
            (a.live[0].text.as_str(), a.live[0].time),
            ("2 files changed", 120)
        );
        files_snapshot(&mut a, &[("c.rs", 1)], 400, "c.rs +1", &["c.rs"]);
        assert_eq!(a.live.len(), 2);
    }

    #[test]
    fn a_burst_forgets_reverted_paths() {
        let mut a = app();
        let all = [("a.rs", 1), ("b.rs", 1), ("c.rs", 1)];
        files_snapshot(
            &mut a,
            &all,
            100,
            "3 files changed",
            &["a.rs", "b.rs", "c.rs"],
        );
        assert_eq!(a.live[0].text, "3 files changed");
        files_snapshot(
            &mut a,
            &all[..1],
            110,
            "2 files discarded",
            &["b.rs", "c.rs"],
        );
        assert_eq!(a.live.len(), 1);
        assert_eq!(a.live[0].text, "a.rs +1");
        assert_eq!(a.live[0].path.as_deref(), Some("a.rs"));
        files_snapshot(&mut a, &[], 120, "a.rs discarded", &["a.rs"]);
        assert_eq!(a.live[0].text, "a.rs discarded");
        assert_eq!(a.live[0].path, None);
    }

    fn with_leak(a: &mut App) {
        let leak = crate::model::Leak {
            rule: "env-file",
            label: ".env file",
            path: ".env".into(),
            line: None,
            snippet: None,
            source: crate::model::LeakSource::Untracked,
        };
        a.handle(UiMsg::Snapshot {
            snap: Arc::new(Snapshot {
                leaks: vec![leak],
                ..Snapshot::default()
            }),
            events: vec![],
            changed: vec![],
        });
    }

    #[test]
    fn s_opens_the_leaks_view_only_when_there_are_leaks() {
        let mut a = app();
        assert!(a.handle(ch('s')).is_empty());
        assert!(a.stack.is_empty(), "nothing to show");
        with_leak(&mut a);
        assert!(
            a.handle(ch('s')).is_empty(),
            "drawn from the snapshot, not loaded"
        );
        assert_eq!(a.stack.len(), 1);
        assert_eq!(a.stack[0].target, Target::Leaks);
        a.handle(ch('s'));
        assert_eq!(a.stack.len(), 1, "not opened twice");
    }

    #[test]
    fn browse_outgoing_works_read_only_and_never_confirms() {
        let mut a = app();
        assert!(!a.write.enabled);
        let cmds = a.perform(Action::Open(Target::Outgoing { pinned: false }));
        assert_eq!(detail_req(&cmds), Some(&DetailReq::Outgoing));
        assert_eq!(
            a.stack.last().unwrap().target,
            Target::Outgoing { pinned: false }
        );
        assert!(a.write.prompt.is_none());
        for k in [ch('y'), key(KeyCode::Enter), ch('P')] {
            assert!(a.handle(k).is_empty());
            assert!(a.write.prompt.is_none());
        }
    }

    #[test]
    fn an_open_outgoing_view_reloads_when_head_branch_or_upstream_moves() {
        let mut a = app();
        let on = |branch: &str, oid: &str, ahead| UiMsg::Snapshot {
            snap: Arc::new(Snapshot {
                head: crate::model::Head::Branch(branch.into()),
                oid: Some(oid.into()),
                upstream: Some(crate::model::Upstream {
                    name: "origin/main".into(),
                    ahead,
                    behind: 0,
                }),
                ..Snapshot::default()
            }),
            events: vec![],
            changed: vec!["a.rs".into()],
        };
        a.handle(on("main", "c1", 1));
        a.perform(Action::Open(Target::Outgoing { pinned: false }));
        let edit = on("main", "c1", 1);
        assert!(detail_req(&a.handle(edit)).is_none(), "an edit");
        let commit = on("main", "c2", 2);
        assert!(detail_req(&a.handle(commit)).is_some(), "a commit");
        let push = on("main", "c2", 0);
        assert!(detail_req(&a.handle(push)).is_some(), "a push");
        // A branch at the same commit, with the same counts.
        let switch = on("feat", "c2", 0);
        assert!(detail_req(&a.handle(switch)).is_some(), "a switch");
    }
}
