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
use ratatui::style::Color;

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
        counts: Counts::lines(a, r),
        modified: None,
    }
}

/// `c`, last written `secs_ago`.
fn edited(c: Change, secs_ago: i64) -> Change {
    Change {
        modified: Some(UNIX_EPOCH + Duration::from_secs(at(secs_ago) as u64)),
        ..c
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
            edited(ch("notes.md", '?', '?', 3, 0), 45),
            ch("old.rs", ' ', 'D', 0, 2),
            edited(ch("src/app.rs", ' ', 'M', 42, 7), 120),
            edited(ch("src/git.rs", ' ', 'M', 9, 3), 6 * 86_400),
            edited(ch("src/watch.rs", 'A', ' ', 10, 0), 3600),
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
            oid: "0".repeat(40),
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
        leaks: Vec::new(),
        leak_scan_skipped: None,
        leak_scan_error: None,
        config_error: None,
    }
}

fn leak(
    path: &str,
    rule: &'static str,
    label: &'static str,
    line: Option<u32>,
    snippet: Option<&str>,
    source: LeakSource,
) -> Leak {
    Leak {
        rule,
        label,
        path: path.into(),
        line,
        snippet: snippet.map(Into::into),
        source,
    }
}

/// The fixture with an untracked `.env`, a key in `src/app.rs` and a key
/// file in the newest (unpushed) commit.
fn leaky() -> Snapshot {
    let mut s = fixture();
    s.changes.insert(0, ch(".env", '?', '?', 2, 0));
    let oid = s.commits[0].oid.clone();
    s.leaks = vec![
        leak(
            ".env",
            "env-file",
            ".env file",
            None,
            None,
            LeakSource::Untracked,
        ),
        leak(
            "src/app.rs",
            "aws-access-key",
            "AWS access key",
            Some(12),
            Some("AKIA••••••••••••WXYZ"),
            LeakSource::Unstaged,
        ),
        leak(
            "deploy/id_rsa",
            "ssh-private-key",
            "SSH private key",
            None,
            None,
            LeakSource::Commit(oid),
        ),
    ];
    s
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
    app.repo_name = Some("spritz".into());
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

/// `out` with the app's version, which changes each release, masked at
/// the same width so the snapshot stays put.
fn unversioned(out: &str) -> String {
    mask_version(out, env!("CARGO_PKG_VERSION"))
}

/// `out` with the help box's ` gitst <version> ` title redrawn as
/// ` gitst VERSION `, centered in the same run of `─`, so a snapshot does
/// not change with the version or with its length.
fn mask_version(out: &str, version: &str) -> String {
    let title = format!(" gitst {version} ");
    let mask = " gitst VERSION ";
    out.lines()
        .map(|line| {
            let Some(at) = line.find(&title) else {
                return line.to_string();
            };
            let (before, rest) = line.split_at(at);
            let after = &rest[title.len()..];
            let head = before.trim_end_matches('─');
            let tail = after.trim_start_matches('─');
            let run = before[head.len()..].chars().count()
                + title.chars().count()
                + after[..after.len() - tail.len()].chars().count();
            let left = (run - mask.chars().count()) / 2;
            let right = run - mask.chars().count() - left;
            format!(
                "{head}{}{mask}{}{tail}",
                "─".repeat(left),
                "─".repeat(right)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
        + if out.ends_with('\n') { "\n" } else { "" }
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
fn an_open_view_is_drawn_at_a_tiny_width() {
    let mut app = new_app(leaky());
    app.handle(key('s'));
    let out = draw(&mut app, 20, 10);
    assert!(out.contains("‹ back"), "{out}");
    assert!(out.contains(".env"), "the findings, not the summary: {out}");
    let out = draw(&mut app, 20, 1);
    assert!(out.starts_with(" ‹ back"), "{out}");
    open(&mut app, Target::File("a".into()));
    app.handle(UiMsg::Detail(
        DetailReq::File { path: "a".into() },
        Ok(file_data()),
    ));
    for (w, h) in [(1, 1), (5, 3), (12, 2), (23, 40)] {
        draw(&mut app, w, h);
    }
    let out = draw(&mut app, 20, 12);
    assert!(out.contains("Unstaged"), "{out}");
    app.handle(UiMsg::Input(Event::Key(KeyEvent::new(
        KeyCode::Esc,
        KeyModifiers::NONE,
    ))));
    app.handle(UiMsg::Input(Event::Key(KeyEvent::new(
        KeyCode::Esc,
        KeyModifiers::NONE,
    ))));
    let out = draw(&mut app, 20, 6);
    assert!(out.lines().skip(1).all(|l| l.trim().is_empty()), "{out}");
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
    assert!(out.contains("▶ Changes"), "{out}");
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
fn warnings_past_the_last_row_share_it() {
    let mut s = leaky();
    s.changes.insert(0, ch("conflict.rs", 'U', 'U', 3, 1));
    let mut app = new_app(s);
    app.handle(UiMsg::RefreshError("index file corrupt".into()));
    app.handle(UiMsg::Fetch(FetchStatus {
        last_error: Some(gitst::git::FetchError::Offline),
        ..FetchStatus::default()
    }));
    // Room for two warning rows.
    let out = draw(&mut app, 80, 10);
    let rows: Vec<&str> = out.lines().skip(1).take(2).collect();
    assert!(rows[0].contains("3 possible secrets"), "{out}");
    assert!(
        rows[1].contains("index file corrupt · 1 conflict · fetch failed: offline"),
        "{out}"
    );
}

const LONG_WARNING: &str = "config.toml: ignored colour, fetch_interval, \
    pulse_secs, collapsed \"branchess\", leak_allow \"a{b\" (invalid), \
    icons (invalid), show_ages, theme";

#[test]
fn a_long_warning_wraps_onto_spare_rows() {
    let mut app = new_app(fixture());
    app.config_warning = Some(LONG_WARNING.into());
    let out = draw(&mut app, 44, 20);
    let rows: Vec<&str> = out.lines().skip(2).take(4).collect();
    assert!(
        rows[0].starts_with(" ⚠ config.toml: ignored colour,"),
        "{out}"
    );
    assert!(
        rows[1].starts_with("   fetch_interval, pulse_secs,"),
        "{out}"
    );
    assert!(rows[2].trim_end().ends_with('…'), "{out}");
    assert!(rows[3].starts_with('╭'), "{out}");
    // With one row, it is cut there.
    let out = draw(&mut app, 44, 6);
    let row = out.lines().nth(1).unwrap();
    let next = out.lines().nth(2).unwrap();
    assert!(
        row.trim_end().ends_with('…') && !next.starts_with("   "),
        "{out}"
    );
}

#[test]
fn a_wrapped_warning_leaves_rows_for_the_others() {
    let mut s = fixture();
    s.changes.insert(0, ch("conflict.rs", 'U', 'U', 3, 1));
    let mut app = new_app(s);
    app.config_warning = Some(LONG_WARNING.into());
    let out = draw(&mut app, 44, 20);
    let rows: Vec<&str> = out.lines().skip(2).take(3).collect();
    assert!(rows[0].contains("1 conflict"), "{out}");
    assert!(rows[1].starts_with(" ⚠ config.toml"), "{out}");
    assert!(
        rows[2].starts_with("   fetch_interval") && rows[2].trim_end().ends_with('…'),
        "{out}"
    );
}

#[test]
fn c_clears_warnings_until_they_change() {
    let mut s = leaky();
    s.changes.insert(0, ch("conflict.rs", 'U', 'U', 3, 1));
    let mut app = new_app(s);
    app.config_warning = Some("config.toml: ignored colour".into());
    let out = draw(&mut app, 80, 20);
    assert!(
        out.contains("1 conflict") && out.contains("c clear"),
        "{out}"
    );
    app.handle(key('c'));
    let out = draw(&mut app, 80, 20);
    assert!(
        !out.contains("1 conflict") && !out.contains("ignored colour"),
        "{out}"
    );
    assert!(!out.contains("c clear"), "{out}");
    // The secrets band stays.
    assert!(out.contains("3 possible secrets"), "{out}");

    // A failed fetch is new, so it shows; once fetching works again and
    // then fails, it shows again after being cleared.
    let offline = || {
        UiMsg::Fetch(FetchStatus {
            last_error: Some(gitst::git::FetchError::Offline),
            ..FetchStatus::default()
        })
    };
    app.handle(offline());
    assert!(draw(&mut app, 80, 20).contains("fetch failed: offline"));
    app.handle(key('c'));
    assert!(!draw(&mut app, 80, 20).contains("fetch failed"));
    app.handle(offline());
    assert!(!draw(&mut app, 80, 20).contains("fetch failed"));
    app.handle(UiMsg::Fetch(FetchStatus::default()));
    draw(&mut app, 80, 20);
    app.handle(offline());
    assert!(draw(&mut app, 80, 20).contains("fetch failed: offline"));
}

#[test]
fn clicking_a_warning_clears_only_that_one() {
    let mut s = fixture();
    s.changes.insert(0, ch("conflict.rs", 'U', 'U', 3, 1));
    let mut app = new_app(s);
    app.config_warning = Some("config.toml: ignored colour".into());
    app.handle(UiMsg::RefreshError("index file corrupt".into()));
    let out = draw(&mut app, 80, 20);
    let rows: Vec<&str> = out.lines().skip(2).take(2).collect();
    assert!(rows[0].contains("index file corrupt"), "{out}");
    assert!(rows[1].contains("1 conflict"), "{out}");
    // The refresh error cannot be cleared.
    app.handle(click(5, 2));
    app.handle(click(5, 3));
    let out = draw(&mut app, 80, 20);
    assert!(
        out.contains("index file corrupt") && !out.contains("1 conflict"),
        "{out}"
    );
    assert!(out.contains("ignored colour"), "{out}");
}

/// How many cells of a `w`×`h` draw are dimmed.
fn dimmed(app: &mut App, w: u16, h: u16) -> usize {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| gitst::ui::draw(f, app, &Theme::ansi())).unwrap();
    let buf = t.backend().buffer();
    buf.content()
        .iter()
        .filter(|c| c.modifier.contains(ratatui::style::Modifier::DIM))
        .count()
}

#[test]
fn the_body_dims_only_while_the_error_says_why() {
    let mut app = new_app(leaky());
    let clean = (dimmed(&mut app, 44, 10), dimmed(&mut app, 44, 6));
    app.handle(UiMsg::RefreshError("index file corrupt".into()));
    // Room for two warning rows: the band, then the error.
    assert!(draw(&mut app, 44, 10).contains("index file corrupt"));
    assert!(dimmed(&mut app, 44, 10) > clean.0);
    // One row, which the band takes.
    let out = draw(&mut app, 44, 6);
    assert!(!out.contains("index file corrupt"), "{out}");
    assert_eq!(dimmed(&mut app, 44, 6), clean.1, "{out}");
}

#[test]
fn loading_before_first_snapshot() {
    utc();
    let mut app = App::new(&Config::default(), FetchStatus::default());
    app.now = SystemTime::now();
    let out = draw(&mut app, 44, 10);
    assert!(out.contains("loading"), "{out}");
}

fn open(app: &mut App, target: Target) {
    app.perform(gitst::app::Action::Open(target));
}

fn diff_block(title: &str, lines: &[(DiffKind, &str)]) -> DiffBlock {
    DiffBlock {
        title: title.into(),
        lines: lines
            .iter()
            .map(|(k, t)| DiffLine {
                kind: *k,
                text: t.to_string(),
            })
            .collect(),
    }
}

/// A working-tree file's diff, unfiltered.
fn file(blocks: Vec<DiffBlock>) -> DetailData {
    DetailData::File {
        blocks,
        filtered: false,
    }
}

fn file_data() -> DetailData {
    file(vec![
        diff_block(
            "Staged",
            &[
                (DiffKind::Hunk, "@@ -1 +1 @@"),
                (DiffKind::Del, "use a;"),
                (DiffKind::Add, "use b;"),
            ],
        ),
        diff_block(
            "Unstaged",
            &[
                (DiffKind::Hunk, "@@ -10,3 +10,3 @@ fn main() {"),
                (DiffKind::Context, "    let x = 1;"),
                (DiffKind::Del, "    old();"),
                (DiffKind::Add, "    new();"),
                (DiffKind::Meta, "\\ No newline at end of file"),
            ],
        ),
    ])
}

#[test]
fn file_diff_view() {
    let mut app = new_app(fixture());
    open(&mut app, Target::File("src/app.rs".into()));
    assert!(draw(&mut app, 44, 14).contains("loading"));
    app.handle(UiMsg::Detail(
        DetailReq::File {
            path: "src/app.rs".into(),
        },
        Ok(file_data()),
    ));
    insta::assert_snapshot!(draw(&mut app, 44, 14));
}

#[test]
fn clean_file_shows_no_changes() {
    let mut app = new_app(fixture());
    open(&mut app, Target::File("a".into()));
    app.handle(UiMsg::Detail(
        DetailReq::File { path: "a".into() },
        Ok(file(vec![])),
    ));
    assert!(draw(&mut app, 44, 10).contains("✓ no changes"));
}

#[test]
fn detail_error_is_shown() {
    let mut app = new_app(fixture());
    open(&mut app, Target::File("a".into()));
    app.handle(UiMsg::Detail(
        DetailReq::File { path: "a".into() },
        Err("bad revision".into()),
    ));
    assert!(draw(&mut app, 44, 10).contains("bad revision"));
}

#[test]
fn commit_view_and_file_click() {
    let mut app = new_app(fixture());
    let oid = "4de1e3c0000000000000000000000000000000000".to_string();
    open(&mut app, Target::Commit(oid.clone()));
    let detail = CommitDetail {
        oid: oid.clone(),
        author: "Dan".into(),
        time: at(120),
        message: "icons: jug on cream\n\nAll ten sizes, soft shadow.".into(),
        files: vec![
            ("Assets/icon.png".into(), Counts::Binary),
            ("project.yml".into(), Counts::lines(4, 1)),
        ],
    };
    app.handle(UiMsg::Detail(
        DetailReq::Commit { rev: oid.clone() },
        Ok(DetailData::Commit(detail)),
    ));
    let out = draw(&mut app, 44, 16);
    insta::assert_snapshot!(out);
    let y = out.lines().position(|l| l.contains("project.yml")).unwrap() as u16;
    app.handle(click(5, y));
    assert_eq!(
        app.stack.last().unwrap().target,
        Target::CommitFile {
            rev: oid,
            path: "project.yml".into()
        }
    );
}

#[test]
fn branch_view() {
    let mut app = new_app(fixture());
    open(
        &mut app,
        Target::Branch {
            name: "main".into(),
            upstream: Some("origin/main".into()),
        },
    );
    let snap = fixture();
    let data = DetailData::Branch {
        ahead: snap.commits[..2].to_vec(),
        behind: vec![],
        ahead_total: 2,
        behind_total: 0,
    };
    app.handle(UiMsg::Detail(
        DetailReq::Branch {
            name: "main".into(),
            upstream: "origin/main".into(),
        },
        Ok(data),
    ));
    insta::assert_snapshot!(draw(&mut app, 44, 12));
}

#[test]
fn the_scrollbar_has_a_column_of_its_own() {
    let mut app = new_app(fixture());
    open(&mut app, Target::File("a".into()));
    // Wrapped at 30 columns, the second piece fills the row, so its last
    // character would sit under the bar.
    let line = format!("{}{}Z", "x".repeat(28), "y".repeat(28));
    let lines: Vec<(DiffKind, &str)> = (0..30).map(|_| (DiffKind::Add, line.as_str())).collect();
    app.handle(UiMsg::Detail(
        DetailReq::File { path: "a".into() },
        Ok(file(vec![diff_block("Unstaged", &lines)])),
    ));
    app.handle(key('w'));
    let out = draw(&mut app, 30, 12);
    assert!(out.contains('Z'), "{out}");
    for row in out.lines().filter(|l| l.contains(['x', 'y', 'Z'])) {
        assert!("│┃".contains(row.chars().last().unwrap()), "{out}");
    }
}

#[test]
fn bidi_controls_in_paths_never_reach_the_terminal() {
    let mut s = leaky();
    let path = "evil\u{202E}txt.exe\u{2066}\u{200F}";
    s.changes.insert(0, ch(path, '?', '?', 1, 0));
    s.leaks[0].path = path.into();
    let mut app = new_app(s);
    app.repo_name = Some("re\u{202E}po".into());
    let out = draw(&mut app, 60, 20);
    assert!(out.contains("eviltxt.exe"), "{out}");
    assert!(out.contains("repo · main"), "{out}");
    app.handle(key('s'));
    let out = draw(&mut app, 60, 20);
    assert!(out.contains("eviltxt.exe"), "{out}");
    for c in ['\u{202E}', '\u{2066}', '\u{200F}'] {
        assert!(!out.contains(c), "{c:?} drawn");
    }
}

#[test]
fn help_overlay() {
    let mut app = new_app(fixture());
    app.handle(key('?'));
    let out = draw(&mut app, 44, 22);
    assert!(
        out.contains(&format!(" gitst {} ", env!("CARGO_PKG_VERSION"))),
        "{out}"
    );
    insta::assert_snapshot!(unversioned(&out));
    app.handle(click(1, 1));
    assert!(!app.help);
}

#[test]
fn wrap_toggle_changes_output() {
    let mut app = new_app(fixture());
    open(&mut app, Target::File("a".into()));
    let long = "x".repeat(70);
    let data = file(vec![diff_block("Unstaged", &[(DiffKind::Add, &long)])]);
    app.handle(UiMsg::Detail(
        DetailReq::File { path: "a".into() },
        Ok(data),
    ));
    let clipped = draw(&mut app, 30, 10);
    app.handle(key('w'));
    let wrapped = draw(&mut app, 30, 10);
    assert_ne!(clipped, wrapped);
    assert!(wrapped.matches("xxxxxxxxxx").count() > clipped.matches("xxxxxxxxxx").count());
}

#[test]
fn detail_scroll_is_clamped() {
    let mut app = new_app(fixture());
    open(&mut app, Target::File("a".into()));
    app.handle(UiMsg::Detail(
        DetailReq::File { path: "a".into() },
        Ok(file_data()),
    ));
    app.handle(key('G'));
    let out = draw(&mut app, 44, 8);
    assert!(app.stack[0].scroll <= 10, "{}", app.stack[0].scroll);
    assert!(out.contains("new();"), "{out}");
}

#[test]
fn stale_lock_warning_appears_without_a_new_snapshot() {
    let mut s = fixture();
    s.index_lock_age = Some(Duration::from_secs(2));
    let mut app = new_app(s);
    assert!(!draw(&mut app, 44, 20).contains("index.lock"));
    let wake = app.next_wakeup();
    assert!(wake <= Duration::from_secs(8), "{wake:?}");
    app.now += Duration::from_secs(9);
    assert!(draw(&mut app, 44, 20).contains("index.lock held 11s"));
}

#[test]
fn a_config_that_could_not_be_read_is_warned_about() {
    let mut s = fixture();
    s.config_error = Some("unknown option `show-scope'".into());
    let out = render(60, 20, s);
    assert!(
        out.contains("repo config not read · unknown option `show-scope'"),
        "{out}"
    );
}

#[test]
fn activity_rows_show_clock_and_age() {
    let out = render(44, 28, fixture());
    let row = out
        .lines()
        .find(|l| l.contains("04:43 commit"))
        .expect("activity row");
    assert!(row.trim_end().ends_with("2m │"), "{row}");
    let push = out
        .lines()
        .find(|l| l.contains("pushed origin/main"))
        .unwrap();
    assert!(push.trim_end().ends_with("1h │"), "{push}");
}

fn wheel_down(x: u16, y: u16) -> UiMsg {
    UiMsg::Input(Event::Mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    }))
}

#[test]
fn wheel_still_scrolls_after_keyboard_selection() {
    let mut app = new_app(fixture());
    let out = draw(&mut app, 44, 16);
    let y = out.lines().position(|l| l.contains("notes.md")).unwrap() as u16;
    app.handle(key('j'));
    app.handle(key('j'));
    draw(&mut app, 44, 16);
    app.handle(wheel_down(5, y));
    draw(&mut app, 44, 16);
    assert!(app.scroll[&gitst::ui::layout::SectionId::Changes] > 0);

    open(&mut app, Target::File("a".into()));
    let long: Vec<(DiffKind, &str)> = (0..40).map(|_| (DiffKind::Add, "line")).collect();
    app.handle(UiMsg::Detail(
        DetailReq::File { path: "a".into() },
        Ok(file(vec![diff_block("Unstaged", &long)])),
    ));
    draw(&mut app, 44, 16);
    app.handle(key('j'));
    draw(&mut app, 44, 16);
    app.handle(wheel_down(5, 5));
    draw(&mut app, 44, 16);
    assert!(app.stack[0].scroll > 0);
}

fn many_files_commit(app: &mut App) {
    let oid = "4de1e3c0000000000000000000000000000000000".to_string();
    open(app, Target::Commit(oid.clone()));
    let detail = CommitDetail {
        oid: oid.clone(),
        author: "Dan".into(),
        time: at(120),
        message: "many files".into(),
        files: (0..30)
            .map(|i| (format!("file{i:02}.rs"), Counts::lines(1, 0)))
            .collect(),
    };
    app.handle(UiMsg::Detail(
        DetailReq::Commit { rev: oid },
        Ok(DetailData::Commit(detail)),
    ));
}

fn special(code: KeyCode) -> UiMsg {
    UiMsg::Input(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
}

#[test]
fn home_and_end_win_over_a_selected_row() {
    for (bottom, top) in [('G', 'g'), ('\0', '\0')] {
        let (to_bottom, to_top) = if bottom == '\0' {
            (special(KeyCode::End), special(KeyCode::Home))
        } else {
            (key(bottom), key(top))
        };
        let mut app = new_app(fixture());
        many_files_commit(&mut app);
        draw(&mut app, 44, 12);
        app.handle(key('j'));
        draw(&mut app, 44, 12);
        app.handle(to_bottom);
        let out = draw(&mut app, 44, 12);
        assert!(out.contains("file29.rs"), "{out}");
        app.handle(to_top);
        let out = draw(&mut app, 44, 12);
        assert!(out.contains("many files"), "{out}");
        assert_eq!(app.stack[0].scroll, 0);
    }
}

#[test]
fn help_shows_in_the_one_line_layout() {
    let mut app = new_app(fixture());
    app.handle(key('?'));
    insta::assert_snapshot!(draw(&mut app, 20, 8));
    let out = draw(&mut app, 20, 1);
    assert!(out.contains("quit"), "{out}");
}

#[test]
fn help_describes_the_detail_view() {
    let mut app = new_app(fixture());
    open(&mut app, Target::File("a".into()));
    app.handle(UiMsg::Detail(
        DetailReq::File { path: "a".into() },
        Ok(file_data()),
    ));
    app.handle(key('?'));
    let out = draw(&mut app, 44, 22);
    insta::assert_snapshot!(unversioned(&out));
    assert!(out.contains("page down"), "{out}");
    assert!(!out.contains("fold section"), "{out}");
    assert!(!out.contains("next section"), "{out}");
}

#[test]
fn uncounted_changes_are_blank_not_binary() {
    let mut s = fixture();
    s.changes[2].counts = Counts::Unknown;
    s.changes[3].counts = Counts::Binary;
    let out = render(44, 20, s.clone());
    let line = |p: &str| out.lines().find(|l| l.contains(p)).unwrap().to_string();
    assert!(!line("src/app.rs").contains("bin"), "{out}");
    assert!(line("src/git.rs").contains("bin"), "{out}");
    // Totals cover what was counted: 3 + 10 added, 2 removed.
    assert!(line("Changes").contains("+13 −2"), "{out}");

    for c in &mut s.changes {
        c.counts = Counts::Unknown;
    }
    let out = render(44, 20, s);
    assert!(!out.contains("bin"), "{out}");
    let title = out.lines().find(|l| l.contains("Changes")).unwrap();
    assert!(!title.contains('+'), "{out}");
}

#[test]
fn single_file_activity_row_opens_the_file() {
    let mut app = new_app(fixture());
    app.handle(UiMsg::Live(gitst::activity::ActivityEvent {
        time: at(30),
        kind: gitst::activity::ActivityKind::Files,
        text: "src/app.rs +42 −7".into(),
        rev: None,
        path: Some("src/app.rs".into()),
    }));
    let out = draw(&mut app, 44, 28);
    let y = out
        .lines()
        .position(|l| l.contains("src/app.rs +42"))
        .unwrap_or_else(|| panic!("{out}")) as u16;
    app.handle(click(10, y));
    assert_eq!(
        app.stack.last().map(|v| v.target.clone()),
        Some(Target::File("src/app.rs".into()))
    );
}

#[test]
fn branch_view_titles_use_the_full_counts() {
    let mut app = new_app(fixture());
    let target = Target::Branch {
        name: "main".into(),
        upstream: Some("origin/main".into()),
    };
    open(&mut app, target);
    app.handle(UiMsg::Detail(
        DetailReq::Branch {
            name: "main".into(),
            upstream: "origin/main".into(),
        },
        Ok(DetailData::Branch {
            ahead: fixture().commits[..2].to_vec(),
            behind: vec![],
            ahead_total: 80,
            behind_total: 0,
        }),
    ));
    let out = draw(&mut app, 44, 12);
    assert!(out.contains("ahead of origin/main (80)"), "{out}");
    assert!(out.contains("78 more"), "{out}");
}

fn enter() -> UiMsg {
    UiMsg::Input(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )))
}

fn opened(app: &mut App) -> Option<Target> {
    app.handle(enter());
    app.stack.pop().map(|v| v.target)
}

#[test]
fn selection_stays_on_its_row_when_rows_move() {
    let mut app = new_app(fixture());
    draw(&mut app, 44, 28);
    // Changes title, notes.md, old.rs, src/app.rs, src/git.rs.
    for _ in 0..5 {
        app.handle(key('j'));
    }
    draw(&mut app, 44, 28);
    assert_eq!(opened(&mut app), Some(Target::File("src/git.rs".into())));

    // A new path sorts above the selected one.
    let mut s = fixture();
    s.changes.insert(0, ch("a_first.rs", '?', '?', 1, 0));
    app.handle(UiMsg::Snapshot {
        snap: Arc::new(s.clone()),
        events: vec![],
        changed: vec![],
    });
    draw(&mut app, 44, 28);
    assert_eq!(opened(&mut app), Some(Target::File("src/git.rs".into())));

    // Once the selected path is gone, the row that took its place is.
    s.changes.retain(|c| c.path != "src/git.rs");
    app.handle(UiMsg::Snapshot {
        snap: Arc::new(s),
        events: vec![],
        changed: vec![],
    });
    draw(&mut app, 44, 28);
    assert_eq!(opened(&mut app), Some(Target::File("src/watch.rs".into())));
}

#[test]
fn leaks_view_groups_and_opens_findings() {
    let mut app = new_app(leaky());
    app.handle(key('s'));
    let wide = draw(&mut app, 80, 20);
    assert!(
        wide.contains("src/app.rs:12") && wide.contains("AKIA••••"),
        "{wide}"
    );
    let out = draw(&mut app, 44, 20);
    insta::assert_snapshot!(out);
    assert!(
        out.lines().next().unwrap().contains("3 possible secrets"),
        "{out}"
    );
    assert!(out.contains("Not pushed yet"), "{out}");
    let y = out
        .lines()
        .position(|l| l.contains("src/app.rs:12"))
        .unwrap() as u16;
    app.handle(click(5, y));
    assert_eq!(
        app.stack.last().unwrap().target,
        Target::File("src/app.rs".into())
    );
}

#[test]
fn leaks_view_after_a_push_and_once_clean() {
    let mut s = leaky();
    let oid = s.commits[0].oid.clone();
    s.leaks[2].source = LeakSource::Pushed(oid.clone());
    let mut app = new_app(s);
    app.handle(key('s'));
    let out = draw(&mut app, 44, 20);
    assert!(out.contains("rotate the key"), "{out}");
    app.handle(UiMsg::Snapshot {
        snap: Arc::new(fixture()),
        events: vec![],
        changed: vec![],
    });
    assert!(draw(&mut app, 44, 20).contains("no possible secrets"));
    app.handle(UiMsg::Snapshot {
        snap: Arc::new(leaky()),
        events: vec![],
        changed: vec![],
    });
    let out = draw(&mut app, 44, 20);
    let y = out
        .lines()
        .position(|l| l.contains("deploy/id_rsa"))
        .unwrap() as u16;
    app.handle(click(5, y));
    assert_eq!(
        app.stack.last().unwrap().target,
        Target::CommitFile {
            rev: oid,
            path: "deploy/id_rsa".into()
        }
    );
}

#[test]
fn leak_band_and_row_markers() {
    let out = render(44, 20, leaky());
    insta::assert_snapshot!(out);
    assert!(
        out.lines()
            .nth(2)
            .unwrap()
            .contains("⚠ 3 possible secrets · 1 in unpushed commit"),
        "{out}"
    );
    let row = |needle: &str| {
        out.lines()
            .find(|l| l.contains(needle))
            .unwrap()
            .to_string()
    };
    assert!(row("?? .env").contains('⚠'), "{out}");
    assert!(row("4de1e3c ↑ ⚠").contains("icons"), "{out}");
    let mut app = new_app(leaky());
    let mut t = Terminal::new(TestBackend::new(44, 20)).unwrap();
    t.draw(|f| gitst::ui::draw(f, &mut app, &Theme::ansi()))
        .unwrap();
    let buf = t.backend().buffer();
    assert_eq!(buf[(43, 2)].bg, Color::Red, "the band fills its row");
    assert_eq!(buf[(1, 2)].fg, Color::White);
}

#[test]
fn leak_warning_survives_small_panes() {
    let narrow = render(24, 12, leaky());
    assert!(narrow.contains("⚠ 3 secrets"), "{narrow}");
    let tiny = render(20, 6, leaky());
    assert!(tiny.lines().next().unwrap().contains('⚠'), "{tiny}");
    let short = render(44, 4, leaky());
    assert!(short.lines().next().unwrap().contains('⚠'), "{short}");
}

#[test]
fn clicking_the_band_opens_the_leaks_view() {
    let mut app = new_app(leaky());
    draw(&mut app, 44, 20);
    app.handle(click(5, 2));
    assert_eq!(app.stack.last().unwrap().target, Target::Leaks);
}

#[test]
fn skipped_scan_note_comes_last() {
    let mut s = fixture();
    s.index_lock_age = Some(Duration::from_secs(42));
    s.leak_scan_skipped = Some(812);
    let out = render(44, 20, s);
    let at = |needle: &str| out.lines().position(|l| l.contains(needle)).unwrap();
    assert!(
        at("index.lock held") < at("⚠ secret scan skipped · 812 changes"),
        "{out}"
    );
}

#[test]
fn ages_wake_the_ui_when_they_next_read_differently() {
    let mut app = new_app(fixture());
    // notes.md is 45s old, so it next changes in a second.
    draw(&mut app, 120, 40);
    let wake = app.next_wakeup();
    assert!(
        wake > Duration::from_millis(900) && wake <= Duration::from_millis(1010),
        "{wake:?}"
    );

    // Half a second on, the redraw still lands just after the change.
    app.now += Duration::from_millis(500);
    draw(&mut app, 120, 40);
    let wake = app.next_wakeup();
    assert!(wake <= Duration::from_millis(510), "{wake:?}");

    // With everything old, ages wait no longer than the idle redraw.
    let mut s = fixture();
    s.changes.clear();
    s.commits.clear();
    s.reflog.clear();
    s.last_fetch = None;
    let mut app = new_app(s);
    draw(&mut app, 120, 40);
    assert_eq!(app.next_wakeup(), Duration::from_secs(30));
}

// ---- write mode ----

fn write_app(snap: Snapshot) -> App {
    utc();
    let mut app = App::new(
        &Config {
            write: true,
            ..Config::default()
        },
        FetchStatus {
            enabled: true,
            ..FetchStatus::default()
        },
    );
    app.now = UNIX_EPOCH + Duration::from_secs(NOW as u64);
    app.repo_name = Some("spritz".into());
    app.handle(UiMsg::Snapshot {
        snap: Arc::new(snap),
        events: vec![],
        changed: vec![],
    });
    app
}

fn first_line(out: &str) -> &str {
    out.lines().next().unwrap()
}

/// The screen column `word` starts at in `line`.
fn column(line: &str, word: &str) -> u16 {
    line[..line.find(word).unwrap()].chars().count() as u16
}

fn last_line(out: &str) -> &str {
    out.lines().last().unwrap()
}

/// The row under the header, where a prompt, confirm, or result shows.
fn prompt_line(out: &str) -> &str {
    out.lines().nth(1).unwrap()
}

#[test]
fn read_only_header_unchanged() {
    let out = render(80, 20, fixture());
    assert!(!first_line(&out).contains("Commit"), "{out}");
    let out = draw(&mut new_app(fixture()), 80, 20);
    assert!(!first_line(&out).contains("Push"), "{out}");
}

#[test]
fn a_dimmed_label_still_takes_the_click() {
    let mut s = fixture();
    s.upstream = None;
    let mut app = write_app(s);
    let out = draw(&mut app, 80, 20);
    app.handle(click(column(first_line(&out), "Write"), 0));
    let out = draw(&mut app, 80, 20);
    let (y, row) = line_with(&out, "Pull");
    app.handle(click(column(row, "Pull"), y));
    assert_eq!(
        app.write.result.as_ref().map(|r| r.0.as_str()),
        Some("no upstream to pull from")
    );
}

#[test]
fn commit_prompt_under_the_header() {
    let mut app = write_app(fixture());
    app.handle(key('C'));
    for c in "fix it".chars() {
        app.handle(key(c));
    }
    let mut t = Terminal::new(TestBackend::new(80, 20)).unwrap();
    t.draw(|f| gitst::ui::draw(f, &mut app, &Theme::ansi()))
        .unwrap();
    let out = text(&t);
    assert!(prompt_line(&out).starts_with(" commit: fix it"), "{out}");
    assert!(
        prompt_line(&out)
            .trim_end()
            .ends_with("⏎ commit · ⇥ amend · esc")
    );
    assert!(
        last_line(&out).contains("quit"),
        "the hint row stays: {out}"
    );
    let pos = t.get_cursor_position().unwrap();
    assert_eq!((pos.x, pos.y), (" commit: fix it".len() as u16, 1));
    let row = t.backend().buffer().content()[80..160].to_vec();
    assert!(
        row.iter()
            .all(|c| c.bg == ratatui::style::Color::Yellow && c.fg == ratatui::style::Color::Black),
        "black on yellow"
    );
    insta::assert_snapshot!(out);
}

#[test]
fn stash_prompt_hint() {
    let mut app = write_app(fixture());
    app.handle(key('S'));
    let out = draw(&mut app, 80, 20);
    assert!(prompt_line(&out).starts_with(" stash: "), "{out}");
    assert!(
        prompt_line(&out).trim_end().ends_with("⏎ stash · esc"),
        "{out}"
    );
}

fn confirm(question: &str, y_only: bool) -> gitst::app::Prompt {
    gitst::app::Prompt::Confirm {
        question: question.into(),
        y_only,
        on_yes: gitst::write::WriteOp::StashDrop {
            index: 0,
            oid: "0".repeat(40),
        },
    }
}

#[test]
fn y_only_confirm_shows_y() {
    let mut app = write_app(fixture());
    app.write.prompt = Some(confirm("Drop stash@{0}?", true));
    let out = draw(&mut app, 80, 20);
    let last = prompt_line(&out);
    assert!(last.starts_with(" Drop stash@{0}?"), "{out}");
    assert!(
        last.trim_end().ends_with(" y") && !last.contains("y / n"),
        "{out}"
    );
    app.write.prompt = Some(confirm("Push 3 commits to origin/main?", false));
    let out = draw(&mut app, 80, 20);
    assert!(prompt_line(&out).trim_end().ends_with("y / n"), "{out}");
    insta::assert_snapshot!(out);
}

#[test]
fn the_prompt_takes_the_row_under_the_header() {
    let mut app = write_app(fixture());
    app.write.prompt = Some(confirm("Push?", false));
    let out = draw(&mut app, 40, 6);
    assert!(prompt_line(&out).starts_with(" Push?"), "{out}");
    // On the dashboard it stands in for the info line.
    let out = draw(&mut app, 80, 20);
    assert!(prompt_line(&out).starts_with(" Push?"), "{out}");
    assert!(!out.contains("origin/main ·"), "{out}");
}

#[test]
fn finishing_while_quitting() {
    let mut app = write_app(fixture());
    app.write.running = Some(gitst::write::Verb::Commit);
    app.write.quitting = true;
    let out = draw(&mut app, 80, 20);
    assert!(prompt_line(&out).contains("finishing commit…"), "{out}");
    app.write.running = None;
    let out = draw(&mut app, 80, 20);
    assert!(prompt_line(&out).contains("finishing…"), "{out}");
}

#[test]
fn a_result_shows_under_the_header() {
    let mut app = write_app(fixture());
    app.write.result = Some(("committed a1b2c3d".into(), std::time::Instant::now()));
    let out = draw(&mut app, 80, 20);
    assert!(prompt_line(&out).starts_with(" committed a1b2c3d"), "{out}");
    app.write.result = Some((
        "expired result".into(),
        std::time::Instant::now() - Duration::from_secs(4),
    ));
    let out = draw(&mut app, 80, 20);
    assert!(!out.contains("expired result"), "{out}");
}

fn failed(app: &mut App) {
    app.write.failure = Some(gitst::write::WriteError {
        summary: "pre-commit hook failed".into(),
        output: "hook: lint failed\n  src/a.rs:3: bad\n".into(),
    });
}

#[test]
fn failure_warning_names_bang() {
    let mut app = write_app(fixture());
    failed(&mut app);
    let out = draw(&mut app, 80, 20);
    assert!(
        out.contains("pre-commit hook failed · ! for output"),
        "{out}"
    );
    let y = out
        .lines()
        .position(|l| l.contains("! for output"))
        .unwrap() as u16;
    app.handle(click(5, y));
    assert_eq!(
        app.stack.last().map(|v| v.target.clone()),
        Some(Target::Output)
    );
    // And `c` clears it.
    let mut app = write_app(fixture());
    failed(&mut app);
    draw(&mut app, 80, 20);
    app.handle(key('c'));
    let out = draw(&mut app, 80, 20);
    assert!(!out.contains("! for output"), "{out}");
}

#[test]
fn a_failure_shows_again_after_clearing_and_goes_with_the_next_action() {
    use gitst::write::{WriteError, WriteOp, WriteOutcome};
    let failed = || {
        UiMsg::Write(
            WriteOp::Pull,
            Err(WriteError {
                summary: "pull failed: auth, pull from a shell".into(),
                output: "fatal: could not read Username".into(),
            }),
        )
    };
    let mut app = write_app(fixture());
    app.handle(key('U'));
    app.handle(failed());
    assert!(draw(&mut app, 80, 20).contains("! for output"));
    app.handle(key('c'));
    assert!(!draw(&mut app, 80, 20).contains("! for output"));
    // The same failure again is news, though its text is the same.
    app.handle(key('U'));
    draw(&mut app, 80, 20);
    app.handle(failed());
    let out = draw(&mut app, 80, 20);
    assert!(out.contains("pull failed: auth"), "{out}");
    // A success after it takes it away.
    app.handle(key('U'));
    app.handle(UiMsg::Write(
        WriteOp::Pull,
        Ok(WriteOutcome::Done {
            summary: "pulled".into(),
            warning: None,
        }),
    ));
    let out = draw(&mut app, 80, 20);
    assert!(!out.contains("! for output"), "{out}");
}

#[test]
fn output_view_shows_hook_output() {
    let mut app = write_app(fixture());
    failed(&mut app);
    app.handle(key('!'));
    let out = draw(&mut app, 80, 20);
    assert!(out.contains("‹ back") && out.contains("output"), "{out}");
    assert!(out.contains("hook: lint failed"), "{out}");
    assert!(out.contains("src/a.rs:3: bad"), "{out}");
    insta::assert_snapshot!(out);
}

#[test]
fn help_lists_write_keys_only_in_write_mode() {
    let mut app = write_app(fixture());
    app.handle(key('?'));
    let out = draw(&mut app, 80, 30);
    assert!(
        out.contains("last failure") && out.contains("stage all"),
        "{out}"
    );
    assert!(out.contains("pop · apply · drop"), "{out}");
    let mut app = new_app(fixture());
    app.handle(key('?'));
    let out = draw(&mut app, 80, 30);
    assert!(
        !out.contains("pop · apply · drop") && !out.contains("last failure"),
        "{out}"
    );
}

#[test]
fn detail_headers_carry_stash_and_file_labels() {
    let mut app = write_app(fixture());
    open(&mut app, Target::Stash(0));
    let out = draw(&mut app, 80, 20);
    assert!(first_line(&out).contains("pop · apply · drop"), "{out}");
    let x = column(first_line(&out), "apply");
    // Nothing happens until the view has loaded the stash it acts on.
    app.handle(click(x, 0));
    assert_eq!(app.write.running, None);
    app.handle(UiMsg::Detail(
        DetailReq::Commit {
            rev: "stash@{0}".into(),
        },
        Ok(DetailData::Commit(CommitDetail {
            oid: "s0".into(),
            author: "a".into(),
            time: 0,
            message: "WIP on main: x".into(),
            files: vec![],
        })),
    ));
    app.handle(click(x, 0));
    // The stash op is on its way.
    assert_eq!(app.write.running, Some(gitst::write::Verb::StashAction));

    let mut app = write_app(fixture());
    open(&mut app, Target::File("src/app.rs".into()));
    let out = draw(&mut app, 80, 20);
    assert!(first_line(&out).contains("+ stage file"), "{out}");
    let mut app = write_app(fixture());
    open(&mut app, Target::File("src/watch.rs".into()));
    let out = draw(&mut app, 80, 20);
    assert!(first_line(&out).contains("− unstage file"), "{out}");
    let mut app = new_app(fixture());
    open(&mut app, Target::File("src/watch.rs".into()));
    let out = draw(&mut app, 80, 20);
    assert!(!first_line(&out).contains("unstage"), "{out}");
}

// ---- fix round 1 ----

#[test]
fn the_header_keeps_the_band_style() {
    use ratatui::style::{Modifier, Style};
    utc();
    let mut theme = Theme::ansi();
    theme.band = Style::new().bg(Color::Blue).add_modifier(Modifier::BOLD);
    for write in [false, true] {
        let mut app = if write {
            write_app(fixture())
        } else {
            new_app(fixture())
        };
        let mut t = Terminal::new(TestBackend::new(80, 20)).unwrap();
        t.draw(|f| gitst::ui::draw(f, &mut app, &theme)).unwrap();
        let cell = &t.backend().buffer()[(70, 0)];
        assert_eq!(cell.bg, Color::Blue, "write={write}");
        assert!(cell.modifier.contains(Modifier::BOLD));
    }
}

fn after_write_key(w: u16, h: u16) -> (App, String) {
    let mut app = write_app(fixture());
    draw(&mut app, w, h);
    app.handle(key('C'));
    let out = draw(&mut app, w, h);
    (app, out)
}

#[test]
fn pane_too_small_shows_in_the_header() {
    // No row below the header, and the one-line layout.
    for (w, h) in [(40, 2), (20, 10), (20, 1)] {
        let (_, out) = after_write_key(w, h);
        assert!(
            first_line(&out).contains("pane too small"),
            "{w}x{h}\n{out}"
        );
    }
    // Too narrow for a prompt but tall enough for a row under the header.
    let (_, out) = after_write_key(29, 10);
    assert!(prompt_line(&out).contains("pane too small"), "{out}");
}

#[test]
fn an_open_prompt_shows_in_the_header_when_no_row_fits() {
    let mut app = write_app(fixture());
    app.write.prompt = Some(confirm("Push?", false));
    let out = draw(&mut app, 40, 2);
    assert!(first_line(&out).starts_with(" Push?"), "{out}");
    assert!(first_line(&out).trim_end().ends_with("y / n"), "{out}");
    app.write.prompt = Some(gitst::app::Prompt::Input {
        purpose: gitst::app::InputPurpose::Commit,
        input: gitst::prompt::LineInput::new("hi"),
        amend: false,
        saved: String::new(),
    });
    let mut t = Terminal::new(TestBackend::new(40, 2)).unwrap();
    t.draw(|f| gitst::ui::draw(f, &mut app, &Theme::ansi()))
        .unwrap();
    assert!(first_line(&text(&t)).starts_with(" commit: hi"));
    let pos = t.get_cursor_position().unwrap();
    assert_eq!((pos.x, pos.y), (" commit: hi".len() as u16, 0));
}

#[test]
fn a_failure_in_the_shared_warning_row_still_opens_the_output() {
    let mut s = fixture();
    s.config_error = Some("bad".into());
    s.leak_scan_error = Some("x".into());
    let mut app = write_app(s);
    failed(&mut app);
    let out = draw(&mut app, 80, 10);
    let y = out
        .lines()
        .position(|l| l.contains("! for output"))
        .expect(&out) as u16;
    assert_eq!(y, 2, "in the last of two warning rows\n{out}");
    app.handle(click(5, y));
    assert_eq!(
        app.stack.last().map(|v| v.target.clone()),
        Some(Target::Output)
    );
}

// ---- hunks ----

fn file_loaded(app: &mut App) {
    open(app, Target::File("src/app.rs".into()));
    app.handle(UiMsg::Detail(
        DetailReq::File {
            path: "src/app.rs".into(),
        },
        Ok(file_data()),
    ));
}

fn line_with<'a>(out: &'a str, text: &str) -> (u16, &'a str) {
    let (y, l) = out
        .lines()
        .enumerate()
        .find(|(_, l)| l.contains(text))
        .unwrap_or_else(|| panic!("no {text:?} in\n{out}"));
    (y as u16, l)
}

#[test]
fn hunk_labels_only_in_write_mode() {
    let mut app = write_app(fixture());
    file_loaded(&mut app);
    let out = draw(&mut app, 60, 16);
    let (_, staged) = line_with(&out, "@@ -1 +1 @@");
    assert!(staged.starts_with("▌@@ -1 +1 @@"), "{out}");
    assert!(staged.trim_end().ends_with("− unstage"), "{out}");
    let (y, unstaged) = line_with(&out, "@@ -10,3");
    assert!(unstaged.starts_with("@@"), "{out}");
    assert!(unstaged.trim_end().ends_with("+ stage"), "{out}");
    insta::assert_snapshot!(out);
    let x = column(unstaged, "+ stage");
    let cmds = app.handle(click(x, y));
    let op = cmds.iter().find_map(|c| match c {
        gitst::app::Cmd::Worker(gitst::worker::WorkerMsg::Write(op)) => Some(&**op),
        _ => None,
    });
    assert!(
        matches!(
            op,
            Some(gitst::write::WriteOp::ApplyHunk { reverse: false, hunk, .. })
                if hunk.header == "@@ -10,3 +10,3 @@ fn main() {"
        ),
        "{op:?}"
    );

    // Read-only: the cursor, and no labels.
    let mut app = new_app(fixture());
    file_loaded(&mut app);
    let out = draw(&mut app, 60, 16);
    assert!(line_with(&out, "@@ -1 +1 @@").1.starts_with("▌@@"), "{out}");
    assert!(
        !out.contains("+ stage") && !out.contains("− unstage"),
        "{out}"
    );
    app.handle(key(']'));
    let out = draw(&mut app, 60, 16);
    assert!(line_with(&out, "@@ -1 +1 @@").1.starts_with("@@"), "{out}");
    assert!(line_with(&out, "@@ -10,3").1.starts_with("▌@@"), "{out}");
}

#[test]
fn renamed_files_have_no_hunk_labels() {
    let mut s = fixture();
    s.changes[2].orig_path = Some("src/old_app.rs".into());
    let mut app = write_app(s);
    file_loaded(&mut app);
    let out = draw(&mut app, 60, 16);
    assert!(
        !line_with(&out, "@@ -1 +1 @@").1.contains("unstage"),
        "{out}"
    );
    assert!(first_line(&out).contains("+ stage file"), "{out}");
}

#[test]
fn the_hunk_cursor_scrolls_into_view() {
    let mut app = new_app(fixture());
    file_loaded(&mut app);
    let out = draw(&mut app, 60, 6);
    assert!(!out.contains("@@ -10,3"), "{out}");
    app.handle(key(']'));
    let out = draw(&mut app, 60, 6);
    assert!(line_with(&out, "@@ -10,3").1.starts_with("▌@@"), "{out}");
}

#[test]
fn filtered_files_have_no_hunk_labels() {
    let mut app = write_app(fixture());
    open(&mut app, Target::File("src/app.rs".into()));
    let DetailData::File { blocks, .. } = file_data() else {
        unreachable!()
    };
    app.handle(UiMsg::Detail(
        DetailReq::File {
            path: "src/app.rs".into(),
        },
        Ok(DetailData::File {
            blocks,
            filtered: true,
        }),
    ));
    let out = draw(&mut app, 60, 16);
    assert!(line_with(&out, "@@ -1 +1 @@").1.starts_with("▌@@"), "{out}");
    assert!(!line_with(&out, "@@ -10,3").1.contains("stage"), "{out}");
    assert!(!out.contains("− unstage"), "{out}");
    assert!(first_line(&out).contains("+ stage file"), "{out}");
}

// ---- outgoing view ----

fn outgoing_commit(short: &str, subject: &str) -> Commit {
    commit(short, subject, 300, true, &[])
}

fn outgoing_file(path: &str, status: char, counts: Counts) -> OutgoingFile {
    OutgoingFile {
        path: path.into(),
        status,
        counts,
    }
}

/// Four commits, two shown; five files, four shown; +33 −11.
fn outgoing_fixture() -> Outgoing {
    Outgoing {
        target: "origin/main".into(),
        base: "b".repeat(40),
        oid: "c".repeat(40),
        commits: vec![
            outgoing_commit("4de1e3c", "icons: jug on cream"),
            outgoing_commit("ebf9d60", "pin engine to v0.1.9"),
        ],
        commit_total: 4,
        files: vec![
            outgoing_file("Assets/icon.png", 'A', Counts::Binary),
            outgoing_file("old.rs", 'D', Counts::lines(0, 11)),
            outgoing_file("src/app.rs", 'M', Counts::lines(30, 0)),
            outgoing_file(
                "src/some/very/deep/folder/watch.rs",
                'M',
                Counts::lines(3, 0),
            ),
        ],
        file_total: 5,
        added: 33,
        removed: 11,
    }
}

fn open_outgoing(app: &mut App, o: Option<Outgoing>) {
    open(app, Target::Outgoing { pinned: false });
    app.handle(UiMsg::Detail(
        DetailReq::Outgoing,
        Ok(DetailData::Outgoing(o)),
    ));
}

#[test]
fn outgoing_view_lists_commits_and_files() {
    let mut app = new_app(fixture());
    open_outgoing(&mut app, Some(outgoing_fixture()));
    insta::assert_snapshot!(draw(&mut app, 44, 14));
}

#[test]
fn outgoing_view_rows_open_what_they_show() {
    let o = outgoing_fixture();
    let mut app = new_app(fixture());
    open_outgoing(&mut app, Some(o.clone()));
    let out = draw(&mut app, 44, 14);
    let targets: Vec<Option<Target>> = app.detail_targets.clone();
    let range = |path: &str| Target::RangeFile {
        base: o.base.clone(),
        oid: o.oid.clone(),
        path: path.into(),
    };
    for (i, line) in out.lines().skip(1).enumerate() {
        let expected = if line.contains("icons: jug") {
            Some(Target::Commit(o.commits[0].oid.clone()))
        } else if line.contains("pin engine") {
            Some(Target::Commit(o.commits[1].oid.clone()))
        } else if line.contains("Assets/icon.png") {
            Some(range("Assets/icon.png"))
        } else if line.contains("old.rs") {
            Some(range("old.rs"))
        } else if line.contains("src/app.rs") {
            Some(range("src/app.rs"))
        } else if line.contains("watch.rs") {
            Some(range("src/some/very/deep/folder/watch.rs"))
        } else {
            // Rules, "… N more", and empty rows have no target.
            None
        };
        assert_eq!(
            targets.get(i).cloned().flatten(),
            expected,
            "row {i}: {line}\n{out}"
        );
    }
    // A file row opens its range diff on click.
    let y = out.lines().position(|l| l.contains("src/app.rs")).unwrap() as u16;
    app.handle(click(5, y));
    assert_eq!(app.stack.last().unwrap().target, range("src/app.rs"));
}

#[test]
fn range_file_view_shows_its_diff() {
    let mut app = new_app(fixture());
    let o = outgoing_fixture();
    let target = Target::RangeFile {
        base: o.base,
        oid: o.oid,
        path: "src/app.rs".into(),
    };
    open(&mut app, target.clone());
    let DetailData::File { blocks, .. } = file_data() else {
        unreachable!()
    };
    app.handle(UiMsg::Detail(
        DetailReq::RangeFile {
            base: "b".repeat(40),
            oid: "c".repeat(40),
            path: "src/app.rs".into(),
        },
        Ok(DetailData::RangeFile(blocks)),
    ));
    let out = draw(&mut app, 44, 14);
    assert!(first_line(&out).contains("src/app.rs"), "{out}");
    assert!(out.contains("@@"), "{out}");
}

#[test]
fn outgoing_view_when_nothing_to_push() {
    let mut app = new_app(fixture());
    let o = Outgoing {
        commit_total: 0,
        file_total: 0,
        commits: vec![],
        files: vec![],
        ..outgoing_fixture()
    };
    open_outgoing(&mut app, Some(o.clone()));
    let out = draw(&mut app, 44, 8);
    assert!(out.contains(" ✓ nothing to push to origin/main"), "{out}");
    let mut app = new_app(fixture());
    open_outgoing(
        &mut app,
        Some(Outgoing {
            target: NO_UPSTREAM.into(),
            ..o
        }),
    );
    let out = draw(&mut app, 44, 8);
    assert!(
        out.contains(" ✓ nothing to push\n") || out.contains(" ✓ nothing to push "),
        "{out}"
    );
    assert!(!out.contains("(no upstream)"), "{out}");
}

#[test]
fn outgoing_view_without_a_remote() {
    let mut app = new_app(fixture());
    open_outgoing(&mut app, None);
    assert!(draw(&mut app, 44, 8).contains(" no remote to push to"));
}

#[test]
fn outgoing_title_names_the_target() {
    let mut app = new_app(fixture());
    open_outgoing(&mut app, Some(outgoing_fixture()));
    assert!(first_line(&draw(&mut app, 60, 8)).contains("Outgoing → origin/main"));
    let mut app = new_app(fixture());
    open_outgoing(
        &mut app,
        Some(Outgoing {
            target: NO_UPSTREAM.into(),
            ..outgoing_fixture()
        }),
    );
    assert!(first_line(&draw(&mut app, 60, 8)).contains("Outgoing → (no upstream)"));
}

fn outgoing_on_top(app: &App) -> bool {
    app.stack.last().map(|v| v.target.clone()) == Some(Target::Outgoing { pinned: false })
}

#[test]
fn header_ahead_count_opens_outgoing() {
    // Read-only, with the count at 2.
    let mut app = new_app(fixture());
    let out = draw(&mut app, 60, 20);
    let x = column(first_line(&out), "↑2");
    app.handle(click(x, 0));
    assert!(outgoing_on_top(&app));
    // The same in write mode.
    let mut app = write_app(fixture());
    let out = draw(&mut app, 80, 20);
    app.handle(click(column(first_line(&out), "↑2"), 0));
    assert!(outgoing_on_top(&app));
}

#[test]
fn header_ahead_count_is_not_a_link_at_zero() {
    let mut snap = fixture();
    snap.upstream.as_mut().unwrap().ahead = 0;
    let mut app = new_app(snap);
    let out = draw(&mut app, 60, 20);
    app.handle(click(column(first_line(&out), "↑0"), 0));
    assert!(app.stack.is_empty());
}

#[test]
fn commits_border_ahead_opens_outgoing() {
    let mut app = new_app(fixture());
    let out = draw(&mut app, 60, 28);
    let (y, line) = out
        .lines()
        .enumerate()
        .find(|(_, l)| l.contains("Commits") && l.contains("↑2"))
        .expect("commits border");
    app.handle(click(column(line, "↑2"), y as u16));
    assert!(outgoing_on_top(&app), "{out}");
    // A click elsewhere on the border still folds the section.
    let mut app = new_app(fixture());
    draw(&mut app, 60, 28);
    app.handle(click(column(line, "Commits"), y as u16));
    let out = draw(&mut app, 60, 28);
    assert!(out.contains("▶ Commits"), "{out}");
}

#[test]
fn a_folded_commits_title_ahead_opens_outgoing() {
    let mut app = new_app(fixture());
    let out = draw(&mut app, 60, 28);
    let y = out.lines().position(|l| l.contains("Commits")).unwrap() as u16;
    app.handle(click(5, y));
    let out = draw(&mut app, 60, 28);
    let (y, line) = out
        .lines()
        .enumerate()
        .find(|(_, l)| l.contains("▶ Commits"))
        .unwrap();
    app.handle(click(column(line, "↑2"), y as u16));
    assert!(outgoing_on_top(&app), "{out}");
}

#[test]
fn sized_push_confirm_over_outgoing_view() {
    use gitst::write::{Coverage, PushPlan, WriteOp, WriteOutcome};
    let mut o = outgoing_fixture();
    o.commits.truncate(1);
    o.commit_total = 1;
    o.files = vec![
        outgoing_file("src/a.rs", 'M', Counts::lines(2, 1)),
        outgoing_file("src/b.rs", 'A', Counts::lines(1, 0)),
    ];
    o.file_total = 2;
    o.added = 3;
    o.removed = 1;
    let plan = PushPlan {
        local: "main".into(),
        remote: "origin".into(),
        branch: "main".into(),
        oid: o.oid.clone(),
        count: 1,
        set_upstream: true,
        creates_branch: true,
        findings: Vec::new(),
        coverage: Coverage::Full,
        outgoing: Box::new(o),
    };
    let mut app = write_app(fixture());
    app.handle(key('P'));
    app.handle(UiMsg::Write(
        WriteOp::PreparePush,
        Ok(WriteOutcome::PushReady(plan)),
    ));
    let out = draw(&mut app, 72, 12);
    let last = prompt_line(&out);
    assert!(
        last.contains("Push 1 commit, 2 files (+3 −1) to origin/main and track it?"),
        "{out}"
    );
    assert!(last.trim_end().ends_with("y / n"), "{out}");
    assert!(first_line(&out).contains("Push now"), "{out}");
    assert!(!first_line(&out).contains("Commit"), "{out}");
    insta::assert_snapshot!(out);
    // A click on `Push now` answers the confirm, as `y` does.
    let x = column(first_line(&out), "Push now");
    let cmds = app.handle(click(x, 0));
    let op = cmds.iter().find_map(|c| match c {
        gitst::app::Cmd::Worker(gitst::worker::WorkerMsg::Write(op)) => Some(&**op),
        _ => None,
    });
    assert!(matches!(op, Some(WriteOp::Push(_))), "{op:?}");
    assert!(app.write.prompt.is_none());
}

#[test]
fn a_clean_tree_offers_amend() {
    let mut s = fixture();
    s.changes.clear();
    let mut app = write_app(s);
    let out = draw(&mut app, 80, 20);
    assert!(first_line(&out).contains("Push · Write ▾"), "{out}");
    app.handle(key('C'));
    let out = draw(&mut app, 80, 20);
    assert!(prompt_line(&out).starts_with(" amend: "), "{out}");
    assert!(
        prompt_line(&out)
            .trim_end()
            .ends_with("⏎ amend · ⇥ new commit · esc"),
        "{out}"
    );
}

#[test]
fn help_mentions_outgoing_clicks_and_push() {
    let mut app = write_app(fixture());
    app.handle(key('?'));
    let out = draw(&mut app, 80, 40);
    assert!(out.contains("click ↑N"), "{out}");
    let (_, row) = line_with(&out, " P ");
    assert!(row.contains("push") && !row.contains("review"), "{out}");
}

fn written(cmds: &[gitst::app::Cmd]) -> Option<&gitst::write::WriteOp> {
    cmds.iter().find_map(|c| match c {
        gitst::app::Cmd::Worker(gitst::worker::WorkerMsg::Write(op)) => Some(&**op),
        _ => None,
    })
}

#[test]
fn a_change_rows_button_stages_or_unstages_it() {
    use gitst::write::WriteOp;
    let mut app = write_app(fixture());
    let out = draw(&mut app, 80, 20);
    let (y, row) = line_with(&out, "src/app.rs");
    assert!(row.trim_end().ends_with("+ │"), "{out}");
    let x = row.chars().count() as u16 - 3;
    let cmds = app.handle(click(x, y));
    assert!(
        matches!(written(&cmds), Some(WriteOp::Stage { path }) if path == "src/app.rs"),
        "the button stages, not opens"
    );
    assert!(app.stack.is_empty());

    let mut app = write_app(fixture());
    let out = draw(&mut app, 80, 20);
    let (y, row) = line_with(&out, "src/watch.rs");
    assert!(row.trim_end().ends_with("− │"), "staged only: {out}");
    let cmds = app.handle(click(row.chars().count() as u16 - 3, y));
    assert!(matches!(written(&cmds), Some(WriteOp::Unstage { .. })));

    // Read-only, the row has no button.
    let out = draw(&mut new_app(fixture()), 80, 20);
    let (_, row) = line_with(&out, "src/app.rs");
    assert!(!row.trim_end().ends_with("+ │"), "{out}");
}

#[test]
fn a_hovered_header_label_lights_up_as_a_button() {
    let theme = Theme::from_palette((255, 255, 255), (0, 0, 0));
    let mut app = write_app(fixture());
    let out = draw(&mut app, 80, 20);
    let x = column(first_line(&out), "Push");
    app.handle(UiMsg::Input(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column: x + 1,
        row: 0,
        modifiers: KeyModifiers::NONE,
    })));
    let mut t = Terminal::new(TestBackend::new(80, 20)).unwrap();
    t.draw(|f| gitst::ui::draw(f, &mut app, &theme)).unwrap();
    let buf = t.backend().buffer();
    let bg = |x: u16| buf[(x, 0)].bg;
    let lit = theme.band_hover.bg.unwrap();
    // The word and a cell either side, and no further.
    for cx in x - 1..x + 5 {
        assert_eq!(bg(cx), lit, "column {cx}");
    }
    assert_ne!(bg(x - 2), lit);
    assert_ne!(bg(x + 5), lit);
}

/// Draws `app` at `w`×`h`, clicks `Write ▾`, and draws it again.
fn open_menu(app: &mut App, w: u16, h: u16) -> String {
    let out = draw(app, w, h);
    app.handle(click(column(first_line(&out), "Write"), 0));
    let out = draw(app, w, h);
    assert!(app.write.menu, "{out}");
    out
}

#[test]
fn header_shows_push_and_write_menu() {
    let out = draw(&mut write_app(fixture()), 80, 20);
    let head = first_line(&out);
    assert!(
        head.contains("Push · Write ▾") && head.contains('↻'),
        "{out}"
    );
    for gone in ["Stage all", "Commit", "Pull", "Stash"] {
        assert!(!head.contains(gone), "{out}");
    }
    insta::assert_snapshot!(out);
}

#[test]
fn push_drops_before_write_and_moves_into_the_menu() {
    let mut app = write_app(fixture());
    let out = draw(&mut app, 40, 20);
    let head = first_line(&out);
    assert!(head.contains("Write ▾") && !head.contains("Push"), "{out}");
    let out = open_menu(&mut app, 40, 20);
    let (_, row) = line_with(&out, "Push");
    assert!(row.contains(" P "), "{out}");
    // With room for both, Push stays out of the menu.
    let mut app = write_app(fixture());
    let out = open_menu(&mut app, 60, 20);
    assert_eq!(
        out.lines().filter(|l| l.contains("Push")).count(),
        1,
        "{out}"
    );
}

#[test]
fn the_menu_lists_actions_with_keys() {
    let mut app = write_app(fixture());
    let out = open_menu(&mut app, 50, 20);
    for (word, k) in [
        ("Stage all", 'A'),
        ("Commit", 'C'),
        ("Pull", 'U'),
        ("Stash", 'S'),
    ] {
        let (_, row) = line_with(&out, word);
        assert!(row.contains(&format!(" {k} ")), "{out}");
    }
    insta::assert_snapshot!(out);
}

#[test]
fn the_menu_names_unstage_all_and_amend() {
    let mut s = fixture();
    s.changes.retain(|c| c.x != ' ' && c.y == ' ');
    let out = open_menu(&mut write_app(s), 50, 20);
    assert!(out.contains("Unstage all"), "{out}");
    let mut s = fixture();
    s.changes.clear();
    let out = open_menu(&mut write_app(s), 50, 20);
    assert!(out.contains("Amend") && !out.contains("tage all"), "{out}");
}

#[test]
fn clicking_a_menu_item_runs_it_and_closes_the_menu() {
    use gitst::write::WriteOp;
    let mut app = write_app(fixture());
    let out = open_menu(&mut app, 50, 20);
    let (y, row) = line_with(&out, "Stage all");
    let cmds = app.handle(click(column(row, "Stage all"), y));
    assert_eq!(written(&cmds), Some(&WriteOp::StageAll));
    assert!(!app.write.menu);

    // An item that opens a prompt leaves the prompt row in charge.
    let mut app = write_app(fixture());
    let out = open_menu(&mut app, 50, 20);
    let (y, row) = line_with(&out, "Commit");
    app.handle(click(column(row, "Commit"), y));
    assert!(!app.write.menu);
    assert!(app.write.prompt.is_some());
}

#[test]
fn a_click_outside_the_menu_only_closes_it() {
    let mut app = write_app(fixture());
    let out = open_menu(&mut app, 50, 28);
    let (y, _) = line_with(&out, "src/app.rs");
    app.handle(click(3, y));
    assert!(!app.write.menu);
    assert!(app.stack.is_empty(), "the row under the click did not open");
    // So does a second click on `Write ▾`.
    let out = open_menu(&mut app, 50, 28);
    app.handle(click(column(first_line(&out), "Write"), 0));
    assert!(!app.write.menu);
}

#[test]
fn esc_closes_the_menu_and_does_nothing_else() {
    let mut app = write_app(fixture());
    open_menu(&mut app, 50, 28);
    app.handle(key('j'));
    open_menu(&mut app, 50, 28);
    let selected = app.selected;
    app.handle(UiMsg::Input(Event::Key(KeyEvent::new(
        KeyCode::Esc,
        KeyModifiers::NONE,
    ))));
    assert!(!app.write.menu);
    assert_eq!(app.selected, selected, "Esc did not clear the selection");
}

#[test]
fn a_key_closes_the_menu_and_still_acts() {
    let mut app = write_app(fixture());
    open_menu(&mut app, 50, 20);
    app.handle(key('?'));
    assert!(!app.write.menu);
    assert!(app.help);
}

#[test]
fn the_menu_stays_inside_a_narrow_pane() {
    let mut app = write_app(fixture());
    open_menu(&mut app, 50, 20);
    let out = draw(&mut app, 30, 20);
    assert!(out.lines().all(|l| l.chars().count() <= 30), "{out}");
    assert!(out.contains("Commit"), "the menu is still drawn\n{out}");
    let (y, row) = line_with(&out, "Commit");
    app.handle(click(column(row, "Commit"), y));
    assert!(app.write.prompt.is_some(), "its rows match what is drawn");

    // In the one-line layout there is no header to hang it from: nothing
    // is drawn, and a click only closes it.
    let mut app = write_app(fixture());
    open_menu(&mut app, 50, 20);
    let out = draw(&mut app, 22, 20);
    assert!(!out.contains("Commit"), "{out}");
    app.handle(click(5, 0));
    assert!(!app.write.menu);
    assert!(app.stack.is_empty() && app.write.prompt.is_none());
}

#[test]
fn read_only_has_no_write_menu() {
    let out = draw(&mut new_app(fixture()), 80, 20);
    assert!(!first_line(&out).contains("Write"), "{out}");
}

#[test]
fn write_spins_for_other_verbs_and_push_for_push() {
    let mut app = write_app(fixture());
    app.write.running = Some(gitst::write::Verb::Stash);
    let out = draw(&mut app, 80, 20);
    assert!(first_line(&out).contains("Push · ⠋ Write ▾"), "{out}");
    app.write.running = Some(gitst::write::Verb::Push);
    let out = draw(&mut app, 80, 20);
    assert!(first_line(&out).contains("⠋ Push · Write ▾"), "{out}");
}

#[test]
fn write_menu_keeps_its_place_before_the_repo_name() {
    let out = draw(&mut write_app(fixture()), 30, 20);
    let head = first_line(&out);
    assert!(
        head.contains("Write ▾") && !head.contains("spritz"),
        "{out}"
    );
    // Read-only, the name still shows at that width.
    let out = draw(&mut new_app(fixture()), 30, 20);
    assert!(first_line(&out).contains("spritz"), "{out}");
}

#[test]
fn a_confirm_that_arrives_later_closes_the_menu() {
    // The menu was opened while a commit's scan ran; its confirm arrives.
    let mut app = write_app(fixture());
    open_menu(&mut app, 80, 20);
    app.write.prompt = Some(confirm(
        "Possible AWS access key in config/dev.env. Commit anyway?",
        true,
    ));
    let out = draw(&mut app, 80, 20);
    assert!(!app.write.menu);
    assert!(
        prompt_line(&out).contains("config/dev.env. Commit anyway?"),
        "{out}"
    );
    assert!(
        !prompt_line(&out).contains('╭'),
        "the box does not cover it\n{out}"
    );
}

#[test]
fn a_view_without_the_menu_label_closes_the_menu() {
    use gitst::write::{Coverage, PushPlan, WriteOp, WriteOutcome};
    let plan = PushPlan {
        local: "main".into(),
        remote: "origin".into(),
        branch: "main".into(),
        oid: "c1".into(),
        count: 1,
        set_upstream: true,
        creates_branch: true,
        findings: Vec::new(),
        coverage: Coverage::Full,
        outgoing: Default::default(),
    };
    let mut app = write_app(fixture());
    app.handle(key('P'));
    open_menu(&mut app, 50, 28);
    app.handle(UiMsg::Write(
        WriteOp::PreparePush,
        Ok(WriteOutcome::PushReady(plan)),
    ));
    draw(&mut app, 50, 28);
    app.handle(key('n'));
    let out = draw(&mut app, 50, 28);
    assert!(!app.write.menu, "{out}");
    let (y, _) = line_with(&out, "src/app.rs");
    app.handle(click(5, y));
    assert_eq!(
        app.stack.last().map(|v| v.target.clone()),
        Some(Target::File("src/app.rs".into())),
        "the click is not swallowed"
    );
}

#[test]
fn a_closed_menus_items_take_no_clicks_before_a_redraw() {
    let mut app = write_app(fixture());
    let out = open_menu(&mut app, 50, 28);
    let (y, row) = line_with(&out, "Stage all");
    let x = column(row, "Stage all");
    // A second click on `Write ▾` closes it, and a quick click follows
    // where `Stage all` was, before anything is drawn again.
    app.handle(click(column(first_line(&out), "Write"), 0));
    let cmds = app.handle(click(x, y));
    assert!(written(&cmds).is_none(), "nothing was staged");
}

#[test]
fn r_opens_the_outgoing_view_from_anywhere() {
    let outgoing = Some(Target::Outgoing { pinned: false });
    for mut app in [new_app(fixture()), write_app(fixture())] {
        app.handle(key('r'));
        assert_eq!(app.stack.last().map(|v| v.target.clone()), outgoing);
        app.handle(key('r'));
        assert_eq!(app.stack.len(), 1, "not opened twice");
    }
    // From an open view, on top of it.
    let mut app = new_app(fixture());
    let out = draw(&mut app, 44, 28);
    let (y, _) = line_with(&out, "src/app.rs");
    app.handle(click(10, y));
    app.handle(key('r'));
    assert_eq!(app.stack.len(), 2);
    assert_eq!(app.stack.last().map(|v| v.target.clone()), outgoing);
}

#[test]
fn help_lists_r_for_outgoing() {
    let mut app = new_app(fixture());
    app.handle(key('?'));
    let out = draw(&mut app, 80, 40);
    assert!(out.contains("click ↑N"), "{out}");
    let (_, row) = line_with(&out, " r  ");
    assert!(row.contains("outgoing commits"), "{out}");
}

#[test]
fn the_version_mask_ignores_how_long_the_version_is() {
    // A centered title moves by a cell when the version's length changes.
    let old = "x╭─────────── gitst 0.1.16 ───────────╮y";
    let new = "x╭──────────── gitst 0.2.0 ───────────╮y";
    assert_eq!(old.chars().count(), new.chars().count());
    assert_eq!(mask_version(old, "0.1.16"), mask_version(new, "0.2.0"));
    assert!(
        mask_version(old, "0.1.16").starts_with("x╭"),
        "the box stays"
    );
    assert!(!mask_version(old, "0.1.16").contains("0.1.16"));
}
