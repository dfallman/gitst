mod common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use common::{HangingRemote, TestRepo};
use gitst::git::{
    CliBackend, DiscoverError, FetchError, GitBackend, LeakScanConfig, Repo, SnapshotOpts,
};
use gitst::model::*;
use gitst::watch::{self, Relevance};
use gitst::worker::{self, FetchStatus, UiMsg, WorkerConfig, WorkerMsg};

fn opts() -> SnapshotOpts {
    SnapshotOpts {
        max_changes: 1000,
        numstat_max_files: 500,
        commits: 50,
    }
}

fn backend(r: &TestRepo) -> CliBackend {
    CliBackend::new(Repo::discover(&r.path()).ok().unwrap())
}

fn snap(r: &TestRepo) -> Snapshot {
    backend(r).snapshot(&opts()).unwrap()
}

#[test]
fn unborn_repo() {
    let r = TestRepo::new();
    r.write("a.txt", "x\n");
    let s = snap(&r);
    assert!(
        matches!(s.head, Head::Unborn(ref b) if b == "main"),
        "{:?}",
        s.head
    );
    assert_eq!(s.changes.len(), 1);
    assert_eq!(s.changes[0].counts, Counts::lines(1, 0));
    assert!(s.commits.is_empty());
}

#[test]
fn changes_numstat_and_commits() {
    let r = TestRepo::new();
    r.commit_file("a.txt", "1\n2\n", "first");
    r.write("a.txt", "1\n3\n4\n");
    r.write("b.txt", "new\n");
    r.git(&["add", "b.txt"]);
    let s = snap(&r);
    let a = s.changes.iter().find(|c| c.path == "a.txt").unwrap();
    assert_eq!(a.counts, Counts::lines(2, 1));
    assert!(
        s.changes
            .iter()
            .find(|c| c.path == "b.txt")
            .unwrap()
            .staged()
    );
    assert_eq!(s.commits[0].subject, "first");
    assert!(matches!(s.head, Head::Branch(ref b) if b == "main"));
    assert!(s.oid.is_some());
}

fn counts(s: &Snapshot) -> Vec<(&str, Counts)> {
    s.changes
        .iter()
        .map(|c| (c.path.as_str(), c.counts))
        .collect()
}

#[test]
fn binary_and_uncounted_changes_differ() {
    let r = TestRepo::new();
    r.commit_file("img.bin", "a\0b", "one");
    r.write("img.bin", "a\0c");
    r.write("blob.dat", "\0\0");
    r.write("text.txt", "x\ny");
    let s = snap(&r);
    assert_eq!(
        counts(&s),
        vec![
            ("blob.dat", Counts::Binary),
            ("img.bin", Counts::Binary),
            ("text.txt", Counts::lines(2, 0)),
        ]
    );
    // Over the numstat limit nothing is counted, which is not binary.
    let s = backend(&r)
        .snapshot(&SnapshotOpts {
            numstat_max_files: 0,
            ..opts()
        })
        .unwrap();
    assert!(
        s.changes.iter().all(|c| c.counts == Counts::Unknown),
        "{s:?}"
    );
    // An untracked directory is not counted either.
    r.git(&["config", "status.showUntrackedFiles", "normal"]);
    r.write("sub/x.txt", "1\n");
    let s = snap(&r);
    let sub = s.changes.iter().find(|c| c.path == "sub/").unwrap();
    assert_eq!(sub.counts, Counts::Unknown);
}

#[test]
fn push_ahead_behind_and_reflog() {
    let r = TestRepo::new();
    r.commit_file("a", "1", "one");
    r.with_bare_remote();
    r.commit_file("a", "2", "two");
    let s = snap(&r);
    assert_eq!(
        s.upstream.as_ref().map(|u| (u.ahead, u.behind)),
        Some((1, 0))
    );
    assert!(s.commits[0].unpushed && !s.commits[1].unpushed);
    r.git(&["push", "-q"]);
    let s = snap(&r);
    assert!(
        s.reflog
            .iter()
            .any(|e| e.refname == "refs/remotes/origin/main" && e.message == "update by push"),
        "{:?}",
        s.reflog
    );
    assert!(s.has_remote);
    assert!(
        s.reflog
            .iter()
            .any(|e| e.refname == "HEAD" && e.message == "commit: two")
    );
}

#[test]
fn merge_conflict_and_op() {
    let r = TestRepo::new();
    r.commit_file("a", "base\n", "base");
    r.git(&["checkout", "-qb", "c1"]);
    r.commit_file("a", "one\n", "one");
    r.git(&["checkout", "-q", "main"]);
    r.commit_file("a", "two\n", "two");
    let _ = r.try_git(&["merge", "c1"]);
    let s = snap(&r);
    assert!(matches!(s.op, Some(RepoOp::Merge)), "{:?}", s.op);
    assert!(s.changes[0].conflicted());
}

#[test]
fn stopped_rebase() {
    let r = TestRepo::new();
    r.commit_file("a", "base\n", "base");
    r.git(&["checkout", "-qb", "c1"]);
    r.commit_file("a", "one\n", "one");
    r.git(&["checkout", "-q", "main"]);
    r.commit_file("a", "two\n", "two");
    r.git(&["checkout", "-q", "c1"]);
    let _ = r.try_git(&["rebase", "main"]);
    let s = snap(&r);
    assert!(
        matches!(
            s.op,
            Some(RepoOp::Rebase {
                step: Some(1),
                total: Some(1)
            })
        ),
        "{:?}",
        s.op
    );
    assert!(matches!(s.head, Head::Detached(_)));
}

#[test]
fn stash_and_tag() {
    let r = TestRepo::new();
    r.commit_file("a", "1", "one");
    r.git(&["tag", "v0.1"]);
    r.commit_file("a", "2", "two");
    r.write("a", "3");
    r.git(&["stash", "-q"]);
    let s = snap(&r);
    assert_eq!(s.stash_count, 1);
    assert_eq!(s.stashes[0].index, 0);
    let t = s.tag.unwrap();
    assert_eq!((t.name.as_str(), t.distance), ("v0.1", 1));
}

#[test]
fn stale_index_lock_detected() {
    let r = TestRepo::new();
    r.commit_file("a", "1", "one");
    std::fs::write(r.path().join(".git/index.lock"), "").unwrap();
    let s = snap(&r);
    assert!(s.index_lock_age.is_some());
}

#[test]
fn discover_errors() {
    let d = tempfile::tempdir().unwrap();
    assert!(matches!(
        Repo::discover(d.path()),
        Err(DiscoverError::NotARepo)
    ));
    let r = TestRepo::new();
    r.commit_file("a", "1", "one");
    let bare = r.with_bare_remote();
    assert!(matches!(
        Repo::discover(&bare),
        Err(DiscoverError::NoWorkTree)
    ));
    assert!(matches!(
        Repo::discover(&r.path().join(".git")),
        Err(DiscoverError::NoWorkTree)
    ));
}

#[test]
fn discover_from_subdir() {
    let r = TestRepo::new();
    r.commit_file("sub/x", "1", "one");
    let repo = Repo::discover(&r.path().join("sub")).ok().unwrap();
    assert_eq!(repo.root, r.path().canonicalize().unwrap());
}

#[test]
fn max_changes_caps_list() {
    let r = TestRepo::new();
    for i in 0..5 {
        r.write(&format!("f{i}"), "x");
    }
    let s = backend(&r)
        .snapshot(&SnapshotOpts {
            max_changes: 3,
            numstat_max_files: 500,
            commits: 50,
        })
        .unwrap();
    assert_eq!((s.changes.len(), s.changes_omitted), (3, 2));
}

#[test]
fn file_and_commit_details() {
    let r = TestRepo::new();
    r.commit_file("a", "1\n", "one");
    r.write("a", "2\n");
    r.write("u.txt", "hi\n");
    let b = backend(&r);
    let DetailData::File(blocks) = b.detail(&DetailReq::File { path: "a".into() }).unwrap() else {
        panic!()
    };
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].title, "Unstaged");
    assert!(
        blocks[0]
            .lines
            .iter()
            .any(|l| l.kind == DiffKind::Add && l.text == "2")
    );
    let DetailData::File(u) = b
        .detail(&DetailReq::File {
            path: "u.txt".into(),
        })
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(u[0].title, "Untracked");
    assert_eq!(u[0].lines[0].text, "hi");
    let DetailData::Commit(c) = b.detail(&DetailReq::Commit { rev: "HEAD".into() }).unwrap() else {
        panic!()
    };
    assert_eq!(c.message.trim(), "one");
    assert_eq!(c.files[0].0, "a");
    let DetailData::CommitFile(cf) = b
        .detail(&DetailReq::CommitFile {
            rev: "HEAD".into(),
            path: "a".into(),
        })
        .unwrap()
    else {
        panic!()
    };
    assert!(
        cf[0]
            .lines
            .iter()
            .any(|l| l.kind == DiffKind::Add && l.text == "1")
    );
}

#[test]
fn staged_and_unstaged_blocks() {
    let r = TestRepo::new();
    r.commit_file("a", "1\n", "one");
    r.write("a", "2\n");
    r.git(&["add", "a"]);
    r.write("a", "3\n");
    let DetailData::File(blocks) = backend(&r)
        .detail(&DetailReq::File { path: "a".into() })
        .unwrap()
    else {
        panic!()
    };
    let titles: Vec<_> = blocks.iter().map(|b| b.title.as_str()).collect();
    assert_eq!(titles, vec!["Staged", "Unstaged"]);
}

#[test]
fn branch_detail_counts_past_the_listed_commits() {
    let r = TestRepo::new();
    r.commit_file("a", "0", "zero");
    r.with_bare_remote();
    for i in 0..55 {
        r.git(&["commit", "-q", "--allow-empty", "-m", &format!("c{i}")]);
    }
    let DetailData::Branch {
        ahead,
        ahead_total,
        behind_total,
        ..
    } = backend(&r)
        .detail(&DetailReq::Branch {
            name: "main".into(),
            upstream: "origin/main".into(),
        })
        .unwrap()
    else {
        panic!()
    };
    assert_eq!((ahead.len(), ahead_total, behind_total), (50, 55, 0));
}

#[test]
fn branch_detail() {
    let r = TestRepo::new();
    r.commit_file("a", "1", "one");
    r.with_bare_remote();
    r.commit_file("a", "2", "two");
    let DetailData::Branch { ahead, behind, .. } = backend(&r)
        .detail(&DetailReq::Branch {
            name: "main".into(),
            upstream: "origin/main".into(),
        })
        .unwrap()
    else {
        panic!()
    };
    assert_eq!((ahead.len(), behind.len()), (1, 0));
}

#[test]
fn fetch_updates_behind() {
    let r = TestRepo::new();
    r.commit_file("a", "1", "one");
    let remote = r.with_bare_remote();
    let other = TestRepo::clone_from(&remote);
    other.commit_file("a", "2", "two");
    other.git(&["push", "-q"]);
    backend(&r)
        .fetch(false, Duration::from_secs(30), &AtomicBool::new(false))
        .unwrap();
    let s = snap(&r);
    assert_eq!(s.upstream.unwrap().behind, 1);
    assert!(s.last_fetch.is_some());
}

#[test]
fn linked_worktree_reads_its_own_fetch_head() {
    let r = TestRepo::new();
    r.commit_file("a", "1", "one");
    r.with_bare_remote();
    let wt = r.dir.path().join("wt");
    r.git(&["worktree", "add", "-q", wt.to_str().unwrap()]);
    let linked = CliBackend::new(Repo::discover(&wt).ok().unwrap());
    linked
        .fetch(false, Duration::from_secs(30), &AtomicBool::new(false))
        .unwrap();
    assert!(linked.snapshot(&opts()).unwrap().last_fetch.is_some());
    // The main worktree has never fetched.
    assert_eq!(snap(&r).last_fetch, None);
}

#[test]
fn fetch_unreachable_remote_fails_cleanly() {
    let r = TestRepo::new();
    r.commit_file("a", "1", "one");
    r.git(&["remote", "add", "origin", "/nonexistent/remote.git"]);
    let t = Instant::now();
    let err = backend(&r)
        .fetch(false, Duration::from_secs(30), &AtomicBool::new(false))
        .unwrap_err();
    assert!(
        matches!(err, FetchError::Other(ref m) if m.contains("does not appear")),
        "{err:?}"
    );
    assert!(t.elapsed() < Duration::from_secs(10));
}

fn recv_until<T>(rx: &Receiver<UiMsg>, mut f: impl FnMut(UiMsg) -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let msg = rx.recv_timeout(left).expect("timed out waiting for worker");
        if let Some(v) = f(msg) {
            return v;
        }
    }
}

#[test]
fn worker_sends_snapshot_and_detail() {
    let r = TestRepo::new();
    r.commit_file("a", "1", "one");
    let (tx, rx) = channel();
    let cfg = WorkerConfig {
        opts: opts(),
        interval: Duration::ZERO,
        prune: false,
    };
    let w = worker::spawn(Arc::new(backend(&r)), cfg, tx);
    recv_until(&rx, |m| matches!(m, UiMsg::Snapshot { .. }).then_some(()));
    w.send(WorkerMsg::Detail(DetailReq::Commit { rev: "HEAD".into() }))
        .unwrap();
    let d = recv_until(&rx, |m| match m {
        UiMsg::Detail(_, d) => Some(d),
        _ => None,
    });
    assert!(d.is_ok());
    // A refresh after a file edit reports the change as a live event.
    r.write("a", "2");
    w.send(WorkerMsg::Refresh).unwrap();
    let changed = recv_until(&rx, |m| match m {
        UiMsg::Snapshot { changed, .. } => Some(changed),
        _ => None,
    });
    assert_eq!(changed, vec!["a"]);
    w.send(WorkerMsg::Shutdown).unwrap();
}

#[test]
fn worker_manual_fetch_reports_status() {
    let r = TestRepo::new();
    r.commit_file("a", "1", "one");
    r.with_bare_remote();
    let (tx, rx) = channel();
    let cfg = WorkerConfig {
        opts: opts(),
        interval: Duration::ZERO,
        prune: false,
    };
    let w = worker::spawn(Arc::new(backend(&r)), cfg, tx);
    recv_until(&rx, |m| matches!(m, UiMsg::Snapshot { .. }).then_some(()));
    w.send(WorkerMsg::Fetch { manual: true }).unwrap();
    recv_until(&rx, |m| {
        matches!(m, UiMsg::Fetch(FetchStatus { running: true, .. })).then_some(())
    });
    let done = recv_until(&rx, |m| match m {
        UiMsg::Fetch(s) if !s.running => Some(s),
        _ => None,
    });
    assert_eq!(done.last_error, None);
    let live = recv_until(&rx, |m| match m {
        UiMsg::Live(e) => Some(e),
        _ => None,
    });
    assert_eq!(live.text, "fetch · up to date");
    w.send(WorkerMsg::Shutdown).unwrap();
}

#[test]
fn relevance_respects_gitignore() {
    let r = TestRepo::new();
    r.write(".gitignore", "target/\n");
    r.write("sub/.gitignore", "*.log\n!keep.log\n");
    r.commit_file("a", "1", "one");
    let root = r.path().canonicalize().unwrap();
    let mut rel = Relevance::new(&Repo::discover(&root).ok().unwrap());
    assert!(rel.is_relevant(&root.join("src/main.rs")));
    assert!(!rel.is_relevant(&root.join("target/debug/x")));
    assert!(!rel.is_relevant(&root.join("sub/x.log")));
    assert!(rel.is_relevant(&root.join("sub/keep.log")));
    assert!(rel.is_relevant(&root.join(".gitignore")));
    assert!(rel.is_relevant(&root.join(".git/HEAD")));
    assert!(!rel.is_relevant(&root.join(".git/objects/aa/bb")));
    assert!(!rel.is_relevant(Path::new("/somewhere/else")));
}

#[test]
fn watcher_signals_refresh() {
    let r = TestRepo::new();
    r.commit_file("a", "1", "one");
    let (tx, rx) = channel();
    let _h = watch::spawn(&Repo::discover(&r.path()).ok().unwrap(), tx).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    r.write("a", "2");
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(5)),
        Ok(WorkerMsg::Refresh)
    ));
}

#[test]
fn a_busy_ignored_directory_does_not_hold_back_a_refresh() {
    let r = TestRepo::new();
    r.write(".gitignore", "target/\n");
    r.commit_file("a", "1", "one");
    r.write("target/x", "0");
    let (tx, rx) = channel();
    let _h = watch::spawn(&Repo::discover(&r.path()).ok().unwrap(), tx).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let stop = Arc::new(AtomicBool::new(false));
    let build = {
        let (stop, target) = (stop.clone(), r.path().join("target/x"));
        std::thread::spawn(move || {
            let mut i = 0;
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                i += 1;
                let _ = std::fs::write(&target, i.to_string());
                std::thread::sleep(Duration::from_millis(20));
            }
        })
    };
    std::thread::sleep(Duration::from_millis(100));
    let t = Instant::now();
    r.write("a", "2");
    let got = rx.recv_timeout(Duration::from_secs(5));
    let took = t.elapsed();
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    build.join().unwrap();
    assert!(matches!(got, Ok(WorkerMsg::Refresh)));
    // A burst lasts at most a second; the edit needs only the quiet time.
    assert!(took < Duration::from_millis(800), "{took:?}");
}

#[test]
fn relevance_follows_info_exclude_edits_and_nested_repos() {
    let r = TestRepo::new();
    r.commit_file("a", "1", "one");
    let root = r.path().canonicalize().unwrap();
    let mut rel = Relevance::new(&Repo::discover(&root).ok().unwrap());
    assert!(rel.is_relevant(&root.join("scratch/x.tmp")));
    let exclude = root.join(".git/info/exclude");
    std::fs::write(&exclude, "*.tmp\n").unwrap();
    assert!(rel.is_relevant(&exclude));
    assert!(!rel.is_relevant(&root.join("scratch/x.tmp")));
    // A repository nested in the work tree has its own git directory.
    assert!(rel.is_relevant(&root.join("vendor/x/.git/HEAD")));
    assert!(!rel.is_relevant(&root.join("vendor/x/.git/objects/ab/cd")));
    // Unless the nested repository sits in an ignored directory.
    r.write(".gitignore", ".venv/\n");
    assert!(rel.is_relevant(&root.join(".gitignore")));
    assert!(!rel.is_relevant(&root.join(".venv/src/pkg/.git/HEAD")));
}

#[test]
fn submodule_commit_signals_refresh() {
    let r = TestRepo::new();
    r.commit_file("a", "1", "one");
    let lib = TestRepo::new();
    lib.commit_file("l", "1", "lib");
    let lib_path = lib.path();
    r.git(&[
        "-c",
        "protocol.file.allow=always",
        "submodule",
        "add",
        "-q",
        lib_path.to_str().unwrap(),
        "sub",
    ]);
    r.git(&["commit", "-qm", "add sub"]);
    let (tx, rx) = channel();
    let _h = watch::spawn(&Repo::discover(&r.path()).ok().unwrap(), tx).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    r.git(&["-C", "sub", "commit", "-q", "--allow-empty", "-m", "inside"]);
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(5)),
        Ok(WorkerMsg::Refresh)
    ));
}

#[test]
fn relevance_respects_info_exclude() {
    let r = TestRepo::new();
    r.commit_file("a", "1", "one");
    std::fs::write(r.path().join(".git/info/exclude"), "*.tmp\n").unwrap();
    let root = r.path().canonicalize().unwrap();
    let mut rel = Relevance::new(&Repo::discover(&root).ok().unwrap());
    assert!(!rel.is_relevant(&root.join("scratch/x.tmp")));
    assert!(rel.is_relevant(&root.join("scratch/x.rs")));
}

#[test]
fn fetch_times_out_and_returns() {
    let r = TestRepo::new();
    r.commit_file("a", "1", "one");
    let _remote = HangingRemote::add_to(&r);
    let t = Instant::now();
    let err = backend(&r)
        .fetch(false, Duration::from_millis(500), &AtomicBool::new(false))
        .unwrap_err();
    assert_eq!(err, FetchError::Timeout);
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
}

#[test]
fn fetch_stops_when_cancelled() {
    let r = TestRepo::new();
    r.commit_file("a", "1", "one");
    let remote = HangingRemote::add_to(&r);
    let cancel = Arc::new(AtomicBool::new(false));
    let b = backend(&r);
    let flag = cancel.clone();
    let fetch = std::thread::spawn(move || b.fetch(false, Duration::from_secs(30), &flag));
    remote.wait_for_fetch();
    let t = Instant::now();
    cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(fetch.join().unwrap(), Err(FetchError::Cancelled));
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    assert!(remote.closes_within(Duration::from_secs(2)));
}

#[test]
fn worker_shutdown_stops_running_fetch() {
    let r = TestRepo::new();
    r.commit_file("a", "1", "one");
    let remote = HangingRemote::add_to(&r);
    let (tx, rx) = channel();
    let cfg = WorkerConfig {
        opts: opts(),
        interval: Duration::ZERO,
        prune: false,
    };
    let w = worker::spawn(Arc::new(backend(&r)), cfg, tx);
    recv_until(&rx, |m| matches!(m, UiMsg::Snapshot { .. }).then_some(()));
    w.send(WorkerMsg::Fetch { manual: true }).unwrap();
    remote.wait_for_fetch();
    let t = Instant::now();
    w.shutdown();
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    assert!(
        remote.closes_within(Duration::from_secs(2)),
        "fetch transport outlived shutdown"
    );
}

#[test]
fn honours_show_untracked_files_config() {
    let r = TestRepo::new();
    r.commit_file("a", "1", "one");
    r.write("new/deep/file.txt", "x\n");
    r.git(&["config", "status.showUntrackedFiles", "no"]);
    assert!(snap(&r).changes.is_empty());
    r.git(&["config", "status.showUntrackedFiles", "normal"]);
    let paths: Vec<_> = snap(&r).changes.into_iter().map(|c| c.path).collect();
    assert_eq!(paths, vec!["new/"]);
}

#[test]
fn diff_details_survive_colour_and_blank_line_config() {
    let r = TestRepo::new();
    r.commit_file("a", "1\n\n3\n", "one");
    r.write("a", "1\n\n4\n");
    r.git(&["config", "color.diff", "always"]);
    r.git(&["config", "diff.suppressBlankEmpty", "true"]);
    let DetailData::File(blocks) = backend(&r)
        .detail(&DetailReq::File { path: "a".into() })
        .unwrap()
    else {
        panic!()
    };
    let lines = &blocks[0].lines;
    assert!(lines.iter().all(|l| !l.text.contains('\x1b')), "{lines:?}");
    assert!(
        lines
            .iter()
            .any(|l| l.kind == DiffKind::Context && l.text.is_empty()),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.kind == DiffKind::Add && l.text == "4")
    );
}

#[cfg(unix)]
#[test]
fn untracked_fifo_and_symlink_do_not_hang_detail() {
    let r = TestRepo::new();
    r.commit_file("a", "1", "one");
    let fifo = r.path().join("pipe");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    std::os::unix::fs::symlink("/dev/zero", r.path().join("zero")).unwrap();
    let b = Arc::new(backend(&r));
    for path in ["pipe", "zero"] {
        let (tx, rx) = channel();
        let b = b.clone();
        std::thread::spawn(move || {
            let _ = tx.send(b.detail(&DetailReq::File { path: path.into() }));
        });
        let got = rx
            .recv_timeout(Duration::from_secs(3))
            .unwrap_or_else(|_| panic!("{path}: detail hung"));
        let DetailData::File(blocks) = got.unwrap() else {
            panic!()
        };
        assert_eq!(
            blocks[0].lines[0].kind,
            DiffKind::Meta,
            "{path}: {blocks:?}"
        );
    }
    // Snapshotting (which counts untracked lines) must not hang either.
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let _ = tx.send(b.snapshot(&opts()).is_ok());
    });
    assert_eq!(rx.recv_timeout(Duration::from_secs(3)), Ok(true));
}

#[test]
fn has_remote_follows_config() {
    let r = TestRepo::new();
    r.commit_file("a", "1", "one");
    assert!(!snap(&r).has_remote);
    r.with_bare_remote();
    assert!(snap(&r).has_remote);
    r.git(&["remote", "remove", "origin"]);
    assert!(!snap(&r).has_remote);
}

#[test]
fn stash_shows_once_in_activity() {
    let r = TestRepo::new();
    r.commit_file("a", "1", "one");
    r.write("a", "2");
    let (tx, rx) = channel();
    let cfg = WorkerConfig {
        opts: opts(),
        interval: Duration::ZERO,
        prune: false,
    };
    let w = worker::spawn(Arc::new(backend(&r)), cfg, tx);
    recv_until(&rx, |m| matches!(m, UiMsg::Snapshot { .. }).then_some(()));
    r.git(&["stash", "-q"]);
    w.send(WorkerMsg::Refresh).unwrap();
    let (snap, events) = recv_until(&rx, |m| match m {
        UiMsg::Snapshot { snap, events, .. } => Some((snap, events)),
        _ => None,
    });
    let texts: Vec<String> = gitst::activity::merged(&snap.reflog, &events, 10)
        .into_iter()
        .map(|e| e.text)
        .collect();
    let stashes = texts.iter().filter(|t| *t == "stash saved").count();
    assert_eq!(stashes, 1, "{texts:?}");
    assert!(!texts.iter().any(|t| t.starts_with("reset")), "{texts:?}");
    assert!(!texts.iter().any(|t| t.contains("discarded")), "{texts:?}");
}

/// A fake AWS key, assembled so that this file holds none.
fn aws_key() -> String {
    format!("{}{}", "AK", "IAQ7LM2XRT5VBN8KWD")
}

fn leak_list(s: &Snapshot) -> Vec<(&str, &str, &LeakSource)> {
    s.leaks
        .iter()
        .map(|l| (l.rule, l.path.as_str(), &l.source))
        .collect()
}

#[test]
fn leaks_in_untracked_and_staged_files() {
    let r = TestRepo::new();
    r.commit_file("a.txt", "1\n", "one");
    r.write(".env", "TOKEN=1\n");
    r.write(".env.example", "TOKEN=\n");
    r.write("my notes.txt", &format!("x\n{}\n", aws_key()));
    r.write(
        "src/my cfg.rs",
        &format!("a\nb\nlet k = \"{}\";\n", aws_key()),
    );
    r.git(&["add", "src/my cfg.rs"]);
    let s = snap(&r);
    assert_eq!(s.leaks.len(), 3, "{:?}", s.leaks);
    assert!(
        s.leaks
            .iter()
            .any(|l| l.rule == "env-file" && l.path == ".env" && l.source == LeakSource::Untracked)
    );
    assert!(
        s.leaks
            .iter()
            .any(|l| l.path == "my notes.txt" && l.line == Some(2))
    );
    let staged = s
        .leaks
        .iter()
        .find(|l| l.source == LeakSource::Staged)
        .unwrap();
    assert_eq!(
        (staged.path.as_str(), staged.line),
        ("src/my cfg.rs", Some(3))
    );
    assert!(
        !staged.snippet.as_deref().unwrap().contains("Q7LM2XRT"),
        "masked"
    );
    assert_eq!(s.leak_scan_skipped, None);
}

#[test]
fn an_edited_tracked_env_file_is_not_flagged_again() {
    let r = TestRepo::new();
    r.commit_file(".env", "A=1\n", "one");
    r.write(".env", "A=2\n");
    // The commit that added it is, as it is not pushed.
    let s = snap(&r);
    let sources: Vec<&LeakSource> = s.leaks.iter().map(|l| &l.source).collect();
    assert!(
        matches!(sources[..], [LeakSource::Commit(_)]),
        "{:?}",
        s.leaks
    );
}

#[test]
fn leak_scan_ignores_diff_prefix_config() {
    let r = TestRepo::new();
    r.commit_file("a.txt", "1\n", "one");
    r.git(&["config", "diff.noprefix", "true"]);
    r.git(&["config", "diff.mnemonicPrefix", "true"]);
    r.write("a.txt", &format!("1\n{}\n", aws_key()));
    let s = snap(&r);
    let got: Vec<(&str, Option<u32>, &LeakSource)> = s
        .leaks
        .iter()
        .map(|l| (l.path.as_str(), l.line, &l.source))
        .collect();
    assert_eq!(got, vec![("a.txt", Some(2), &LeakSource::Unstaged)]);
}

#[test]
fn leaks_in_unpushed_commits_then_pushed() {
    let r = TestRepo::new();
    r.commit_file("a.txt", "1\n", "one");
    r.with_bare_remote();
    r.commit_file("deploy.txt", &format!("{}\n", aws_key()), "add deploy key");
    // One backend throughout: the pushed list lives in its cache.
    let b = backend(&r);
    let s = b.snapshot(&opts()).unwrap();
    let oid = s.commits[0].oid.clone();
    assert_eq!(
        leak_list(&s),
        vec![(
            "aws-access-key",
            "deploy.txt",
            &LeakSource::Commit(oid.clone())
        )]
    );
    r.git(&["push", "-q"]);
    let s = b.snapshot(&opts()).unwrap();
    assert_eq!(
        leak_list(&s),
        vec![("aws-access-key", "deploy.txt", &LeakSource::Pushed(oid))]
    );
}

#[test]
fn a_leak_amended_away_is_dropped() {
    let r = TestRepo::new();
    r.commit_file("a.txt", "1\n", "one");
    r.with_bare_remote();
    r.write("b.txt", "2\n");
    r.write(".env", "A=1\n");
    r.git(&["add", "b.txt", ".env"]);
    r.git(&["commit", "-q", "-m", "two"]);
    let b = backend(&r);
    let s = b.snapshot(&opts()).unwrap();
    assert!(
        matches!(
            s.leaks[..],
            [Leak {
                rule: "env-file",
                source: LeakSource::Commit(_),
                ..
            }]
        ),
        "{:?}",
        s.leaks
    );
    r.git(&["rm", "-q", "--cached", ".env"]);
    std::fs::remove_file(r.path().join(".env")).unwrap();
    r.git(&["commit", "-q", "--amend", "--no-edit"]);
    assert!(b.snapshot(&opts()).unwrap().leaks.is_empty());
}

#[test]
fn a_pruned_leak_does_not_hold_back_a_pushed_one() {
    // A flagged commit amended away and pruned is an object git no longer
    // knows. The check for the other flagged commits must still run.
    let r = TestRepo::new();
    r.commit_file("a.txt", "1\n", "one");
    r.with_bare_remote();
    r.commit_file("k.txt", &format!("{}\n", aws_key()), "key on main");
    r.git(&["checkout", "-q", "-b", "feat"]);
    r.write(".env", "A=1\n");
    r.git(&["add", ".env"]);
    r.git(&["commit", "-q", "-m", "env"]);
    let b = backend(&r);
    let s = b.snapshot(&opts()).unwrap();
    assert_eq!(s.leaks.len(), 2, "{:?}", s.leaks);
    let key = s.commits[1].oid.clone();
    r.git(&["rm", "-q", "--cached", ".env"]);
    std::fs::remove_file(r.path().join(".env")).unwrap();
    r.git(&["commit", "-q", "--amend", "--no-edit", "--allow-empty"]);
    r.git(&["reflog", "expire", "--expire-unreachable=now", "--all"]);
    r.git(&["gc", "-q", "--prune=now"]);
    r.git(&["checkout", "-q", "main"]);
    r.git(&["push", "-q"]);
    let s = b.snapshot(&opts()).unwrap();
    assert_eq!(
        leak_list(&s),
        vec![("aws-access-key", "k.txt", &LeakSource::Pushed(key))]
    );
    assert_eq!(s.leak_scan_error, None);
}

#[test]
fn leaks_on_a_branch_without_upstream_or_without_a_remote() {
    let r = TestRepo::new();
    r.commit_file("a.txt", "1\n", "one");
    r.git(&["checkout", "-q", "-b", "feat"]);
    r.commit_file("k.txt", &format!("{}\n", aws_key()), "local key");
    let b = backend(&r);
    let s = b.snapshot(&opts()).unwrap();
    let oid = s.commits[0].oid.clone();
    assert_eq!(
        leak_list(&s),
        vec![("aws-access-key", "k.txt", &LeakSource::Commit(oid.clone()))],
        "no remote yet: every commit is unpushed"
    );
    r.git(&["checkout", "-q", "main"]);
    r.with_bare_remote();
    r.git(&["checkout", "-q", "feat"]);
    let s = b.snapshot(&opts()).unwrap();
    assert!(s.upstream.is_none());
    assert_eq!(
        leak_list(&s),
        vec![("aws-access-key", "k.txt", &LeakSource::Commit(oid.clone()))]
    );
    // A remote added and pushed to between two refreshes.
    r.git(&["push", "-q", "origin", "feat"]);
    let s = b.snapshot(&opts()).unwrap();
    assert_eq!(
        leak_list(&s),
        vec![("aws-access-key", "k.txt", &LeakSource::Pushed(oid))]
    );
}

#[test]
fn a_branch_that_is_not_checked_out_is_scanned() {
    let r = TestRepo::new();
    r.commit_file("a.txt", "1\n", "one");
    r.with_bare_remote();
    r.git(&["checkout", "-q", "-b", "other"]);
    r.commit_file("k.txt", &format!("{}\n", aws_key()), "key on other");
    r.git(&["checkout", "-q", "main"]);
    let b = backend(&r);
    let s = b.snapshot(&opts()).unwrap();
    assert_eq!(s.leaks.len(), 1, "{:?}", s.leaks);
    assert!(matches!(s.leaks[0].source, LeakSource::Commit(_)));
    r.git(&["push", "-q", "origin", "other"]);
    let s = b.snapshot(&opts()).unwrap();
    assert!(
        matches!(
            s.leaks[..],
            [Leak {
                source: LeakSource::Pushed(_),
                ..
            }]
        ),
        "{:?}",
        s.leaks
    );
}

#[test]
fn a_commit_pushed_before_gitst_saw_it_is_reported_as_pushed() {
    let r = TestRepo::new();
    r.commit_file("a.txt", "1\n", "one");
    r.with_bare_remote();
    r.commit_file("deploy.txt", &format!("{}\n", aws_key()), "add deploy key");
    r.git(&["push", "-q"]);
    let s = snap(&r);
    let oid = s.commits[0].oid.clone();
    assert_eq!(
        leak_list(&s),
        vec![("aws-access-key", "deploy.txt", &LeakSource::Pushed(oid))]
    );
}

#[test]
fn a_commit_pushed_to_a_ref_that_is_no_upstream_is_reported_as_pushed() {
    let r = TestRepo::new();
    r.commit_file("a.txt", "1\n", "one");
    r.with_bare_remote();
    r.commit_file("deploy.txt", &format!("{}\n", aws_key()), "add deploy key");
    r.git(&["push", "-q", "origin", "HEAD:feature"]);
    let s = snap(&r);
    let oid = s.commits[0].oid.clone();
    assert_eq!(
        leak_list(&s),
        vec![("aws-access-key", "deploy.txt", &LeakSource::Pushed(oid))]
    );
}

#[test]
fn more_than_fifty_unpushed_commits_are_reported() {
    let r = TestRepo::new();
    r.commit_file("a.txt", "1\n", "one");
    r.with_bare_remote();
    for i in 0..52 {
        r.git(&["commit", "-q", "--allow-empty", "-m", &format!("c{i}")]);
    }
    let s = snap(&r);
    assert_eq!(
        s.leak_scan_error.as_deref(),
        Some("only the newest 50 unpushed commits")
    );
}

#[test]
fn a_repository_without_a_remote_is_not_warned_about_the_cap() {
    // Without a remote every commit is unpushed, and the cap is the normal
    // state of a repository with a history, not something to warn about on
    // every refresh.
    let r = TestRepo::new();
    r.commit_file("a.txt", "1\n", "one");
    for i in 0..52 {
        r.git(&["commit", "-q", "--allow-empty", "-m", &format!("c{i}")]);
    }
    let s = snap(&r);
    assert_eq!(s.leak_scan_error, None);
}

#[test]
fn leak_allow_and_markers_suppress() {
    let r = TestRepo::new();
    let repo = Repo::discover(&r.path()).ok().unwrap();
    let allow = gitst::leaks::allow_matcher(&repo.root, &["fixtures/".to_string()]);
    let b = CliBackend::new(repo).with_leak_scan(LeakScanConfig {
        enabled: true,
        allow,
    });
    r.write("fixtures/.env", "A=1\n");
    r.write("fixtures/k.txt", &aws_key());
    r.write("b.txt", &format!("{} # gitst:allow\n", aws_key()));
    assert!(b.snapshot(&opts()).unwrap().leaks.is_empty());
}

#[test]
fn too_many_changes_skip_the_content_scan() {
    let r = TestRepo::new();
    r.write(".env", "A=1\n");
    r.write("k.txt", &aws_key());
    r.write("z.txt", "z\n");
    let opts = SnapshotOpts {
        numstat_max_files: 2,
        ..opts()
    };
    let s = backend(&r).snapshot(&opts).unwrap();
    assert_eq!(s.leak_scan_skipped, Some(3));
    assert_eq!(
        leak_list(&s),
        vec![("env-file", ".env", &LeakSource::Untracked)]
    );
}

#[test]
fn leak_scan_can_be_turned_off() {
    let r = TestRepo::new();
    r.write(".env", "A=1\n");
    let b = backend(&r).with_leak_scan(LeakScanConfig {
        enabled: false,
        ..LeakScanConfig::default()
    });
    let opts = SnapshotOpts {
        numstat_max_files: 0,
        ..opts()
    };
    let s = b.snapshot(&opts).unwrap();
    assert!(s.leaks.is_empty());
    assert_eq!(s.leak_scan_skipped, None);
}

#[test]
fn a_key_pushed_to_a_branch_other_than_the_upstream_counts_as_pushed() {
    let r = TestRepo::new();
    r.commit_file("a.txt", "1\n", "one");
    r.with_bare_remote();
    // Tracks origin/main, but is pushed to origin/feat.
    r.git(&["checkout", "-q", "-b", "feat", "--track", "origin/main"]);
    r.commit_file("deploy.txt", &format!("{}\n", aws_key()), "add deploy key");
    let b = backend(&r);
    let s = b.snapshot(&opts()).unwrap();
    let oid = s.commits[0].oid.clone();
    assert!(s.upstream.is_some());
    assert_eq!(
        leak_list(&s),
        vec![(
            "aws-access-key",
            "deploy.txt",
            &LeakSource::Commit(oid.clone())
        )]
    );
    r.git(&["push", "-q", "origin", "feat"]);
    let s = b.snapshot(&opts()).unwrap();
    assert_eq!(
        leak_list(&s),
        vec![("aws-access-key", "deploy.txt", &LeakSource::Pushed(oid))]
    );
}

/// A shell command that appends `name` to `log`, for config that must
/// never run.
fn tripwire(log: &Path, name: &str) -> String {
    let log = log.display().to_string().replace('\\', "/");
    format!("echo {name} >> '{log}'")
}

/// What the tripwires wrote, clearing it.
fn take_log(log: &Path) -> String {
    let ran = std::fs::read_to_string(log).unwrap_or_default();
    let _ = std::fs::remove_file(log);
    ran
}

#[test]
fn the_repositorys_own_config_runs_nothing_on_a_refresh_or_a_diff() {
    let r = TestRepo::new();
    let log = r.dir.path().join("ran.log");
    r.write(".gitattributes", "*.txt filter=evil diff=evil\n");
    r.git(&["add", ".gitattributes"]);
    r.commit_file("a.txt", "1\n", "one");
    r.git(&["config", "core.fsmonitor", &tripwire(&log, "fsmonitor")]);
    r.git(&["config", "filter.evil.clean", &tripwire(&log, "clean")]);
    r.git(&["config", "filter.evil.smudge", &tripwire(&log, "smudge")]);
    r.git(&["config", "filter.evil.required", "true"]);
    r.git(&["config", "diff.evil.textconv", &tripwire(&log, "textconv")]);
    r.write("a.txt", "2\n");
    // Plain git runs every one of them.
    let _ = r.try_git(&["--no-optional-locks", "status"]);
    let _ = r.try_git(&["show", "HEAD"]);
    let ran = take_log(&log);
    for name in ["fsmonitor", "clean", "smudge", "textconv"] {
        assert!(
            ran.contains(name),
            "{name} did not run in plain git: {ran:?}"
        );
    }
    let b = backend(&r);
    let s = b.snapshot(&opts()).unwrap();
    assert_eq!(counts(&s), vec![("a.txt", Counts::lines(1, 1))]);
    b.detail(&DetailReq::File {
        path: "a.txt".into(),
    })
    .unwrap();
    b.detail(&DetailReq::CommitFile {
        rev: "HEAD".into(),
        path: "a.txt".into(),
    })
    .unwrap();
    assert_eq!(take_log(&log), "");
}

#[test]
fn a_submodules_own_config_runs_nothing_on_a_refresh() {
    let lib = TestRepo::new();
    lib.write(".gitattributes", "* filter=sub\n");
    lib.git(&["add", ".gitattributes"]);
    lib.commit_file("f", "1\n", "lib");
    let r = TestRepo::new();
    r.commit_file("a", "1", "one");
    r.git(&[
        "-c",
        "protocol.file.allow=always",
        "submodule",
        "add",
        "-q",
        lib.path().to_str().unwrap(),
        "lib",
    ]);
    r.git(&["commit", "-qm", "add lib"]);
    let log = r.dir.path().join("ran.log");
    let sub = r.path().join("lib");
    let sub = sub.to_str().unwrap();
    r.git(&[
        "-C",
        sub,
        "config",
        "filter.sub.clean",
        &tripwire(&log, "clean"),
    ]);
    r.git(&["-C", sub, "config", "filter.sub.required", "true"]);
    // Same size, so git must read the file to see whether it changed.
    r.write("lib/f", "2\n");
    let _ = r.try_git(&["--no-optional-locks", "status"]);
    assert!(take_log(&log).contains("clean"), "plain git ran nothing");
    let s = snap(&r);
    assert_eq!(s.changes.len(), 1, "{:?}", s.changes);
    assert_eq!(take_log(&log), "");
}

#[test]
fn the_repositorys_own_config_runs_nothing_on_a_fetch() {
    let r = TestRepo::new();
    r.commit_file("a", "1", "one");
    let remote = r.with_bare_remote();
    let other = TestRepo::clone_from(&remote);
    other.commit_file("a", "2", "two");
    other.git(&["push", "-q"]);
    let log = r.dir.path().join("ran.log");
    let upload = format!("{}; git-upload-pack", tripwire(&log, "uploadpack"));
    r.git(&["config", "remote.origin.uploadpack", &upload]);
    r.git(&["config", "protocol.ext.allow", "always"]);
    let _ = r.try_git(&["ls-remote", "origin"]);
    assert!(
        take_log(&log).contains("uploadpack"),
        "plain git ran nothing"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let hook = r.path().join(".git/hooks/reference-transaction");
        std::fs::write(&hook, format!("#!/bin/sh\n{}\n", tripwire(&log, "hook"))).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    backend(&r)
        .fetch(false, Duration::from_secs(30), &AtomicBool::new(false))
        .unwrap();
    assert_eq!(snap(&r).upstream.unwrap().behind, 1, "fetched");
    assert_eq!(take_log(&log), "");
}

#[test]
fn a_local_ssh_command_does_not_run_on_a_fetch() {
    let r = TestRepo::new();
    r.commit_file("a", "1", "one");
    r.git(&["remote", "add", "origin", "ssh://example.invalid/x.git"]);
    let log = r.dir.path().join("ran.log");
    r.git(&[
        "config",
        "core.sshCommand",
        &format!("{}; false", tripwire(&log, "ssh")),
    ]);
    let _ = r.try_git(&["ls-remote", "origin"]);
    assert!(take_log(&log).contains("ssh"), "plain git ran nothing");
    // Plain ssh runs instead, and finds no such host.
    let _ = backend(&r).fetch(false, Duration::from_secs(20), &AtomicBool::new(false));
    assert_eq!(take_log(&log), "");
}

/// A server that answers every request with 401, so git asks the
/// credential helpers.
fn asks_for_credentials() -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/x.git", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(
                b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"x\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        }
    });
    url
}

#[test]
fn a_local_credential_helper_does_not_run_on_a_fetch() {
    let r = TestRepo::new();
    r.commit_file("a", "1", "one");
    let url = asks_for_credentials();
    r.git(&["remote", "add", "origin", &url]);
    let log = r.dir.path().join("ran.log");
    let helper = format!("!{}", tripwire(&log, "helper"));
    r.git(&["config", "credential.helper", &helper]);
    r.git(&["config", &format!("credential.{url}.helper"), &helper]);
    let plain = std::process::Command::new("git")
        .args(["-C", r.path().to_str().unwrap(), "ls-remote", "origin"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(!plain.status.success());
    assert!(take_log(&log).contains("helper"), "plain git ran nothing");
    let err = backend(&r)
        .fetch(false, Duration::from_secs(20), &AtomicBool::new(false))
        .unwrap_err();
    assert_eq!(err, FetchError::Auth);
    assert_eq!(take_log(&log), "");
}

#[cfg(unix)]
#[test]
fn a_remote_helper_from_the_work_tree_does_not_run_on_a_fetch() {
    use std::os::unix::fs::PermissionsExt;
    let r = TestRepo::new();
    r.commit_file("a", "1", "one");
    r.with_bare_remote();
    let log = r.dir.path().join("ran.log");
    // `git-remote-../x` is `x` in a directory named `git-remote-..`.
    let helper = r.path().join("git-remote-../x");
    r.write(
        "git-remote-../x",
        &format!("#!/bin/sh\n{}\n", tripwire(&log, "helper")),
    );
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
    r.git(&["config", "remote.origin.vcs", "../x"]);
    let _ = r.try_git(&["fetch", "-q"]);
    assert!(take_log(&log).contains("helper"), "plain git ran nothing");
    let err = backend(&r)
        .fetch(false, Duration::from_secs(20), &AtomicBool::new(false))
        .unwrap_err();
    assert!(
        matches!(err, FetchError::Other(ref m) if m.contains("not allowed")),
        "{err:?}"
    );
    assert_eq!(take_log(&log), "");
}
