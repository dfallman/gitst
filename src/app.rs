//! UI state and input handling. Drawing fills in the hit map and navigation
//! list; input is resolved against whatever was drawn last.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};

use crate::activity::ActivityEvent;
use crate::config::Config;
use crate::model::{DetailData, DetailReq, Snapshot};
use crate::ui::layout::SectionId;
use crate::worker::{FetchStatus, UiMsg, WorkerMsg};

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
}

pub enum Cmd {
    Quit,
    Worker(WorkerMsg),
}

pub struct App {
    pub snap: Option<Arc<Snapshot>>,
    pub error: Option<String>,
    pub config_warning: Option<String>,
    pub fetch: FetchStatus,
    pub live: Vec<ActivityEvent>,
    pub pulses: HashMap<String, Instant>,
    pub folded: HashSet<SectionId>,
    pub scroll: HashMap<SectionId, usize>,
    pub selected: Option<NavItem>,
    pub hover: Option<(u16, u16)>,
    pub stack: Vec<DetailView>,
    pub help: bool,
    pub wrap: bool,
    pub pulse_secs: u64,
    /// Wall-clock time used for ages; set before each draw.
    pub now: SystemTime,
    /// Filled by draw.
    pub hits: HitMap,
    /// Filled by draw: selectable dashboard items in screen order.
    pub nav: Vec<NavItem>,
    /// Filled by draw: what each dashboard row opens.
    pub row_targets: HashMap<(SectionId, usize), Target>,
    /// Filled by draw: targets of the top detail view's lines.
    pub detail_targets: Vec<Option<Target>>,
    /// Filled by draw: rows of the section or detail body, for paging.
    pub page: usize,
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
            fetch,
            live: Vec::new(),
            pulses: HashMap::new(),
            folded,
            scroll: HashMap::new(),
            selected: None,
            hover: None,
            stack: Vec::new(),
            help: false,
            wrap: false,
            pulse_secs: cfg.pulse_seconds,
            now: SystemTime::now(),
            hits: HitMap::default(),
            nav: Vec::new(),
            row_targets: HashMap::new(),
            detail_targets: Vec::new(),
            page: 10,
        }
    }

    pub fn handle(&mut self, msg: UiMsg) -> Vec<Cmd> {
        match msg {
            UiMsg::Input(Event::Key(k)) if k.kind != KeyEventKind::Release => self.key(k),
            UiMsg::Input(Event::Mouse(m)) => self.mouse(m),
            UiMsg::Input(_) => Vec::new(),
            UiMsg::Snapshot {
                snap,
                events,
                changed,
            } => {
                self.snap = Some(snap);
                self.error = None;
                self.live.extend(events);
                let excess = self.live.len().saturating_sub(MAX_LIVE);
                self.live.drain(..excess);
                let now = Instant::now();
                let ttl = Duration::from_secs(self.pulse_secs);
                self.pulses.retain(|_, t| now.duration_since(*t) < ttl);
                for path in changed {
                    self.pulses.insert(path, now);
                }
                match self.stack.last() {
                    Some(v)
                        if matches!(v.req, DetailReq::File { .. } | DetailReq::Branch { .. }) =>
                    {
                        vec![Cmd::Worker(WorkerMsg::Detail(v.req.clone()))]
                    }
                    _ => Vec::new(),
                }
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
                for v in self.stack.iter_mut().filter(|v| v.req == req) {
                    v.data = Some(data.clone());
                }
                Vec::new()
            }
        }
    }

    /// How long the UI may sleep before something on screen changes by itself.
    pub fn next_wakeup(&self) -> Duration {
        let ttl = Duration::from_secs(self.pulse_secs);
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
            Action::Open(target) => {
                let req = target.request();
                self.stack.push(DetailView {
                    target,
                    req: req.clone(),
                    data: None,
                    scroll: 0,
                    selected: None,
                });
                self.detail_targets.clear();
                vec![Cmd::Worker(WorkerMsg::Detail(req))]
            }
            Action::Back => {
                if self.help {
                    self.help = false;
                } else {
                    self.stack.pop();
                    self.detail_targets.clear();
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
        }
    }

    fn key(&mut self, k: KeyEvent) -> Vec<Cmd> {
        if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
            return vec![Cmd::Quit];
        }
        if self.help {
            self.help = false;
            return Vec::new();
        }
        match k.code {
            KeyCode::Char('q') => return vec![Cmd::Quit],
            KeyCode::Char('f') => return self.perform(Action::Fetch),
            KeyCode::Char('?') => return self.perform(Action::Help),
            _ => {}
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
        let select = |app: &mut App, i: usize| app.selected = app.nav.get(i).copied();
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
                    self.selected = Some(NavItem {
                        section: s.section,
                        row: None,
                    });
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
            KeyCode::Esc => self.selected = None,
            _ => {}
        }
        Vec::new()
    }

    fn detail_key(&mut self, code: KeyCode) -> Vec<Cmd> {
        let selectable: Vec<usize> = (0..self.detail_targets.len())
            .filter(|i| self.detail_targets[*i].is_some())
            .collect();
        let page = self.page.max(1);
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
            KeyCode::PageDown | KeyCode::Char(' ') => view.scroll += page,
            KeyCode::PageUp => view.scroll = view.scroll.saturating_sub(page),
            KeyCode::Char('g') | KeyCode::Home => view.scroll = 0,
            KeyCode::Char('G') | KeyCode::End => view.scroll = usize::MAX / 2,
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
                    Some(ScrollTarget::Section(id)) => apply(self.scroll.entry(id).or_insert(0)),
                    Some(ScrollTarget::Detail) => {
                        if let Some(v) = self.stack.last_mut() {
                            apply(&mut v.scroll);
                        }
                    }
                    None => {}
                }
                Vec::new()
            }
            MouseEventKind::Moved | MouseEventKind::Drag(_) => {
                self.hover = Some((x, y));
                Vec::new()
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            kind: crate::activity::ActivityKind::Fetch,
            text: "x".into(),
            rev: None,
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
}
