mod common;

use common::TestRepo;
use gitst::git::{CliBackend, DiscoverError, GitBackend, Repo, SnapshotOpts};
use gitst::model::*;

fn opts() -> SnapshotOpts {
    SnapshotOpts { max_changes: 1000, numstat_max_files: 500, commits: 50 }
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
    assert!(matches!(s.head, Head::Unborn(ref b) if b == "main"), "{:?}", s.head);
    assert_eq!(s.changes.len(), 1);
    assert_eq!(s.changes[0].added, Some(1));
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
    assert_eq!((a.added, a.removed), (Some(2), Some(1)));
    assert!(s.changes.iter().find(|c| c.path == "b.txt").unwrap().staged());
    assert_eq!(s.commits[0].subject, "first");
    assert!(matches!(s.head, Head::Branch(ref b) if b == "main"));
    assert!(s.oid.is_some());
}

#[test]
fn push_ahead_behind_and_reflog() {
    let r = TestRepo::new();
    r.commit_file("a", "1", "one");
    r.with_bare_remote();
    r.commit_file("a", "2", "two");
    let s = snap(&r);
    assert_eq!(s.upstream.as_ref().map(|u| (u.ahead, u.behind)), Some((1, 0)));
    assert!(s.commits[0].unpushed && !s.commits[1].unpushed);
    r.git(&["push", "-q"]);
    let s = snap(&r);
    assert!(
        s.reflog.iter().any(|e| e.refname == "refs/remotes/origin/main" && e.message == "update by push"),
        "{:?}",
        s.reflog
    );
    assert!(s.has_remote);
    assert!(s.reflog.iter().any(|e| e.refname == "HEAD" && e.message == "commit: two"));
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
    assert!(matches!(s.op, Some(RepoOp::Rebase { step: Some(1), total: Some(1) })), "{:?}", s.op);
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
    assert!(matches!(Repo::discover(d.path()), Err(DiscoverError::NotARepo)));
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
        .snapshot(&SnapshotOpts { max_changes: 3, numstat_max_files: 500, commits: 50 })
        .unwrap();
    assert_eq!((s.changes.len(), s.changes_omitted), (3, 2));
}
