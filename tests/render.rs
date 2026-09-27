//! Renders the UI into an in-memory terminal and snapshots the text.

use std::sync::{Arc, Once};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use gitst::app::{App, Target};
use gitst::config::Config;
use gitst::model::*;
use gitst::ui::theme::Theme;
use gitst::worker::{FetchStatus, UiMsg};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

const NOW: i64 = 1_790_484_333;

fn utc() {
    static ONCE: Once = Once::new();
    // SAFETY: set once, before any test reads the local time zone.
    ONCE.call_once(|| unsafe { std::env::set_var("TZ", "UTC") });
}

fn at(secs_ago: i64) -> i64 {
    NOW - secs_ago
}

fn ch(path: &str, x: char, y: char, a: u32, r: u32) -> Change {
    Change {
        path: path.into(),
        orig_path: None,
        x,
        y,
        added: Some(a),
        removed: Some(r),
    }
}

fn commit(short: &str, subject: &str, ago: i64, unpushed: bool, refs: &[&str]) -> Commit {
    Commit {
        oid: format!("{short}0000000000000000000000000000000000"),
        short: short.into(),
        parents: 1,
        time: at(ago),
        author: "Dan".into(),
        refs: refs.iter().map(|r| r.to_string()).collect(),
        subject: subject.into(),
        unpushed,
    }
}

fn branch(
    name: &str,
    upstream: Option<&str>,
    ahead: u32,
    behind: u32,
    gone: bool,
    head: bool,
) -> Branch {
    Branch {
        name: name.into(),
        upstream: upstream.map(Into::into),
        ahead,
        behind,
        gone,
        time: at(600),
        is_head: head,
    }
}

fn reflog(refname: &str, short: &str, ago: i64, message: &str) -> ReflogEntry {
    ReflogEntry {
        refname: refname.into(),
        short: short.into(),
        time: at(ago),
        message: message.into(),
    }
}

fn fixture() -> Snapshot {
    Snapshot {
        head: Head::Branch("main".into()),
        oid: Some("4de1e3c".into()),
        upstream: Some(Upstream {
            name: "origin/main".into(),
            ahead: 2,
            behind: 0,
        }),
        op: None,
        changes: vec![
            ch("notes.md", '?', '?', 3, 0),
            ch("old.rs", ' ', 'D', 0, 2),
            ch("src/app.rs", ' ', 'M', 42, 7),
            ch("src/git.rs", ' ', 'M', 9, 3),
            ch("src/watch.rs", 'A', ' ', 10, 0),
        ],
        changes_omitted: 0,
        commits: vec![
            commit("4de1e3c", "icons: jug on cream", 120, true, &["main"]),
            commit("ebf9d60", "pin engine to v0.1.9", 3600, true, &[]),
            commit(
                "59eb9c3",
                "xcode settings in project.yml",
                4000,
                false,
                &["origin/main"],
            ),
            commit("8e6bbde", "release v0.1.9", 9000, false, &["tag: v0.1.9"]),
        ],
        branches: vec![
            branch("main", Some("origin/main"), 2, 0, false, true),
            branch(
                "feat/spritz-connect",
                Some("origin/feat/spritz-connect"),
                0,
                3,
                false,
                false,
            ),
            branch("old", Some("origin/old"), 0, 0, true, false),
            branch("scratch", None, 0, 0, false, false),
        ],
        stashes: vec![Stash {
            index: 0,
            time: at(7200),
            message: "WIP on main: 4de1e3c icons".into(),
        }],
        reflog: vec![
            reflog("HEAD", "4de1e3c", 120, "commit: icons: jug on cream"),
            reflog("HEAD", "ebf9d60", 3600, "commit: pin engine to v0.1.9"),
            reflog(
                "refs/remotes/origin/main",
                "59eb9c3",
                3900,
                "update by push",
            ),
            reflog(
                "HEAD",
                "59eb9c3",
                5000,
                "checkout: moving from feat/spritz-connect to main",
            ),
        ],
        tag: Some(TagInfo {
            name: "v0.1.9".into(),
            distance: 4,
        }),
        stash_count: 1,
        index_lock_age: None,
        last_fetch: Some(UNIX_EPOCH + Duration::from_secs(at(120) as u64)),
        has_remote: true,
    }
}

fn new_app(snap: Snapshot) -> App {
    utc();
    let mut app = App::new(
        &Config::default(),
        FetchStatus {
            enabled: true,
            ..FetchStatus::default()
        },
    );
    app.now = UNIX_EPOCH + Duration::from_secs(NOW as u64);
    app.handle(UiMsg::Snapshot {
        snap: Arc::new(snap),
        events: vec![],
        changed: vec![],
    });
    app
}

fn text(t: &Terminal<TestBackend>) -> String {
    let buf = t.backend().buffer();
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

fn draw(app: &mut App, w: u16, h: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| gitst::ui::draw(f, app, &Theme::ansi())).unwrap();
    text(&t)
}

fn render(w: u16, h: u16, snap: Snapshot) -> String {
    draw(&mut new_app(snap), w, h)
}

fn click(x: u16, y: u16) -> UiMsg {
    UiMsg::Input(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    }))
}

fn key(c: char) -> UiMsg {
    UiMsg::Input(Event::Key(KeyEvent::new(
        KeyCode::Char(c),
        KeyModifiers::NONE,
    )))
}

#[test]
fn dashboard_44x28() {
    insta::assert_snapshot!(render(44, 28, fixture()));
}

#[test]
fn dashboard_30x12() {
    insta::assert_snapshot!(render(30, 12, fixture()));
}

#[test]
fn dashboard_120x40() {
    insta::assert_snapshot!(render(120, 40, fixture()));
}

#[test]
fn dashboard_24x8() {
    insta::assert_snapshot!(render(24, 8, fixture()));
}

#[test]
fn tiny_width_is_one_line() {
    let out = render(20, 6, fixture());
    let first = out.lines().next().unwrap();
    assert!(
        first.contains("main") && first.contains("↑2") && first.contains('●'),
        "{out}"
    );
    assert!(out.lines().skip(1).all(|l| l.trim().is_empty()));
}

#[test]
fn clean_tree() {
    let mut s = fixture();
    s.changes.clear();
    insta::assert_snapshot!(render(44, 20, s));
}

#[test]
fn warnings_conflict_rebase_lock() {
    let mut s = fixture();
    s.changes.insert(0, ch("conflict.rs", 'U', 'U', 3, 1));
    s.op = Some(RepoOp::Rebase {
        step: Some(3),
        total: Some(7),
    });
    s.head = Head::Detached("4de1e3c".into());
    s.index_lock_age = Some(Duration::from_secs(42));
    insta::assert_snapshot!(render(44, 20, s));
}

#[test]
fn unborn_and_no_remote() {
    let s = Snapshot {
        changes: vec![ch("a.txt", '?', '?', 1, 0)],
        ..Snapshot::default()
    };
    let out = render(44, 14, s);
    assert!(out.contains("main (no commits)"), "{out}");
    assert!(!out.contains('↻'), "{out}");
}

#[test]
fn tiny_sizes_do_not_panic() {
    for (w, h) in [
        (1, 1),
        (10, 3),
        (23, 40),
        (80, 1),
        (80, 2),
        (40, 4),
        (0, 0),
        (300, 3),
    ] {
        render(w, h, fixture());
    }
    let long = Snapshot {
        changes: vec![ch(
            "文档/日本語のファイル名がとても長い/🎉🎉🎉.md",
            '?',
            '?',
            1,
            0,
        )],
        ..fixture()
    };
    for w in 24..60 {
        render(w, 20, long.clone());
    }
}

#[test]
fn click_on_change_row_opens_diff() {
    let mut app = new_app(fixture());
    let out = draw(&mut app, 44, 28);
    let y = out
        .lines()
        .position(|l| l.contains("src/app.rs"))
        .expect("row drawn") as u16;
    app.handle(click(10, y));
    assert_eq!(
        app.stack.last().map(|v| v.target.clone()),
        Some(Target::File("src/app.rs".into()))
    );
}

#[test]
fn click_on_title_folds_section() {
    let mut app = new_app(fixture());
    let out = draw(&mut app, 44, 28);
    let y = out.lines().position(|l| l.contains("Changes")).unwrap() as u16;
    app.handle(click(5, y));
    let out = draw(&mut app, 44, 28);
    assert!(out.contains("▸ Changes"), "{out}");
    assert!(!out.contains("src/app.rs"), "{out}");
}

#[test]
fn click_fetch_control_requests_fetch() {
    let mut app = new_app(fixture());
    let out = draw(&mut app, 44, 28);
    let first = out.lines().next().unwrap();
    let x = first.chars().position(|c| c == '↻').expect("fetch control") as u16;
    let cmds = app.handle(click(x, 0));
    assert!(matches!(
        cmds[..],
        [gitst::app::Cmd::Worker(gitst::worker::WorkerMsg::Fetch {
            manual: true
        })]
    ));
}

#[test]
fn keyboard_selection_is_drawn_and_scrolls_into_view() {
    let mut app = new_app(fixture());
    draw(&mut app, 44, 16);
    for _ in 0..40 {
        app.handle(key('j'));
        draw(&mut app, 44, 16);
    }
    assert!(app.selected.is_some());
}

#[test]
fn stale_snapshot_error_is_shown() {
    let mut app = new_app(fixture());
    app.handle(UiMsg::RefreshError("index file corrupt".into()));
    let out = draw(&mut app, 44, 20);
    assert!(out.contains("index file corrupt"), "{out}");
}

#[test]
fn loading_before_first_snapshot() {
    utc();
    let mut app = App::new(&Config::default(), FetchStatus::default());
    app.now = SystemTime::now();
    let out = draw(&mut app, 44, 10);
    assert!(out.contains("loading"), "{out}");
}
