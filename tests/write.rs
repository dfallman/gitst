//! Write ops run as plain git against a real repository.

mod common;

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use common::TestRepo;
use gitst::git::{CliBackend, GitBackend, LeakScanConfig, Repo, SnapshotOpts};
use gitst::model::{DetailData, DetailReq, DiffKind};
use gitst::write::{Coverage, ShownHunk, WriteError, WriteOp, WriteOutcome};

/// A repository whose own config holds what a commit needs: the backend
/// under test doesn't get the test helper's `-c` flags.
fn writable() -> TestRepo {
    let r = TestRepo::new();
    r.git(&["config", "user.name", "T"]);
    r.git(&["config", "user.email", "t@t"]);
    r.git(&["config", "commit.gpgsign", "false"]);
    r
}

fn backend(r: &TestRepo) -> CliBackend {
    CliBackend::new(Repo::discover(&r.path()).ok().unwrap())
}

fn run(r: &TestRepo, op: WriteOp) -> Result<WriteOutcome, WriteError> {
    backend(r).write(&op, Duration::from_secs(30), &AtomicBool::new(false))
}

fn staged(r: &TestRepo) -> String {
    r.git(&["diff", "--cached", "--name-only"])
        .trim()
        .to_string()
}

#[test]
fn stage_and_unstage_a_file() {
    let r = writable();
    r.commit_file("a.txt", "one\n", "first");
    r.write("a.txt", "two\n");
    let out = run(
        &r,
        WriteOp::Stage {
            path: "a.txt".into(),
        },
    )
    .unwrap();
    assert_eq!(
        out,
        WriteOutcome::Done {
            summary: "staged a.txt".into(),
            warning: None
        }
    );
    assert_eq!(staged(&r), "a.txt");
    let out = run(
        &r,
        WriteOp::Unstage {
            path: "a.txt".into(),
            unborn: false,
        },
    )
    .unwrap();
    assert_eq!(
        out,
        WriteOutcome::Done {
            summary: "unstaged a.txt".into(),
            warning: None
        }
    );
    assert_eq!(staged(&r), "");
}

#[test]
fn unstage_on_an_unborn_branch() {
    let r = writable();
    r.write("b.txt", "x\n");
    r.git(&["add", "b.txt"]);
    run(
        &r,
        WriteOp::Unstage {
            path: "b.txt".into(),
            unborn: true,
        },
    )
    .unwrap();
    assert_eq!(r.git(&["status", "--porcelain"]).trim(), "?? b.txt");
}

#[test]
fn stage_all_and_unstage_all() {
    let r = writable();
    r.commit_file("a.txt", "1\n", "a");
    r.commit_file("b.txt", "1\n", "b");
    r.write("a.txt", "2\n");
    r.write("b.txt", "2\n");
    r.write("c.txt", "new\n");
    run(&r, WriteOp::StageAll).unwrap();
    assert_eq!(staged(&r).lines().count(), 3);
    let out = run(&r, WriteOp::UnstageAll { unborn: false }).unwrap();
    assert_eq!(
        out,
        WriteOutcome::Done {
            summary: "unstaged all".into(),
            warning: None
        }
    );
    assert_eq!(staged(&r), "");
    assert_eq!(
        std::fs::read_to_string(r.path().join("a.txt")).unwrap(),
        "2\n"
    );
    assert_eq!(
        std::fs::read_to_string(r.path().join("c.txt")).unwrap(),
        "new\n"
    );
}

#[test]
fn odd_paths_are_literal() {
    let r = writable();
    r.write("-x *[a].rs", "1\n");
    r.write("ä b.txt", "1\n");
    r.write("x.rs", "1\n");
    run(
        &r,
        WriteOp::Stage {
            path: "-x *[a].rs".into(),
        },
    )
    .unwrap();
    assert_eq!(staged(&r), "-x *[a].rs");
}

const BUSY: &str = "repository is busy (index.lock held), try again";

#[test]
fn held_lock_is_retried_then_reported() {
    let r = writable();
    r.write("a.txt", "1\n");
    let lock = r.path().join(".git/index.lock");
    std::fs::write(&lock, "").unwrap();
    let err = run(
        &r,
        WriteOp::Stage {
            path: "a.txt".into(),
        },
    )
    .unwrap_err();
    assert_eq!(err.summary, BUSY);
    assert!(lock.exists());
}

#[test]
fn lock_released_during_retry_succeeds() {
    let r = writable();
    r.write("a.txt", "1\n");
    let lock = r.path().join(".git/index.lock");
    std::fs::write(&lock, "").unwrap();
    let l = lock.clone();
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(700));
        std::fs::remove_file(l).unwrap();
    });
    run(
        &r,
        WriteOp::Stage {
            path: "a.txt".into(),
        },
    )
    .unwrap();
    t.join().unwrap();
    assert_eq!(staged(&r), "a.txt");
}

fn aws_key() -> String {
    format!("{}{}", "AK", "IAQ7LM2XRT5VBN8KWD")
}

#[test]
fn staged_now_finds_added_secret() {
    let r = writable();
    r.write("cfg.env", &format!("key = {}\n", aws_key()));
    r.git(&["add", "cfg.env"]);
    let (found, cov) = backend(&r).scan_staged();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].rule, "aws-access-key");
    assert_eq!(cov, Coverage::Full);
}

#[test]
fn staged_now_reports_binary_as_partial() {
    let r = writable();
    r.write("big.dat", &"x".repeat(2 << 20));
    r.git(&["add", "big.dat"]);
    let (_, cov) = backend(&r).scan_staged();
    match cov {
        Coverage::Partial(s) => assert!(s.contains("skipped 1"), "{s}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn staged_now_off_when_disabled() {
    let r = writable();
    r.write("cfg.env", &format!("key = {}\n", aws_key()));
    r.git(&["add", "cfg.env"]);
    let b = backend(&r).with_leak_scan(LeakScanConfig {
        enabled: false,
        ..LeakScanConfig::default()
    });
    assert_eq!(b.scan_staged(), (vec![], Coverage::Off));
}

fn prepare(
    r: &TestRepo,
    stage_all: bool,
    amend: bool,
) -> Result<gitst::write::CommitPlan, WriteError> {
    match run(r, WriteOp::PrepareCommit { stage_all, amend })? {
        WriteOutcome::CommitReady(plan) => Ok(plan),
        other => panic!("{other:?}"),
    }
}

fn commit(
    r: &TestRepo,
    plan: &gitst::write::CommitPlan,
    message: &str,
) -> Result<WriteOutcome, WriteError> {
    run(
        r,
        WriteOp::Commit {
            message: message.into(),
            amend: plan.amend,
            tree: plan.tree.clone(),
            head: plan.head.clone(),
        },
    )
}

fn committed_summary(out: Result<WriteOutcome, WriteError>) -> String {
    match out.unwrap() {
        WriteOutcome::Done { summary, warning } => {
            assert_eq!(warning, None);
            summary
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn commit_staged_only() {
    let r = writable();
    r.commit_file("a.txt", "one\n", "first");
    r.commit_file("b.txt", "one\n", "second");
    r.write("a.txt", "two\n");
    r.write("b.txt", "two\n");
    r.git(&["add", "a.txt"]);
    let plan = prepare(&r, false, false).unwrap();
    let s = committed_summary(commit(&r, &plan, "change a"));
    assert!(s.starts_with("committed "), "{s}");
    assert_eq!(
        r.git(&["show", "--name-only", "--format=", "HEAD"]).trim(),
        "a.txt"
    );
    assert_eq!(r.git(&["status", "--porcelain"]).trim(), "M b.txt");
}

#[test]
fn prepare_with_stage_all() {
    let r = writable();
    r.commit_file("a.txt", "one\n", "first");
    r.write("a.txt", "two\n");
    r.write("b.txt", "new\n");
    let plan = prepare(&r, true, false).unwrap();
    commit(&r, &plan, "both").unwrap();
    assert_eq!(
        r.git(&["show", "--name-only", "--format=", "HEAD"]).trim(),
        "a.txt\nb.txt"
    );
}

#[test]
fn first_commit_on_unborn_branch() {
    let r = writable();
    r.write("a.txt", "one\n");
    r.git(&["add", "a.txt"]);
    let plan = prepare(&r, false, false).unwrap();
    commit(&r, &plan, "first").unwrap();
    assert_eq!(r.git(&["log", "--oneline"]).lines().count(), 1);
}

#[test]
fn hash_subject_is_kept() {
    let r = writable();
    r.write("a.txt", "one\n");
    r.git(&["add", "a.txt"]);
    let plan = prepare(&r, false, false).unwrap();
    commit(&r, &plan, "#123 fix crash").unwrap();
    assert_eq!(
        r.git(&["log", "-1", "--format=%s"]).trim(),
        "#123 fix crash"
    );
}

#[test]
fn amend_keeps_body() {
    let r = writable();
    r.write("a.txt", "one\n");
    r.git(&["add", "a.txt"]);
    let plan = prepare(&r, false, false).unwrap();
    commit(&r, &plan, "old\n\nbody").unwrap();
    let plan = prepare(&r, false, true).unwrap();
    let s = committed_summary(commit(&r, &plan, "new"));
    assert!(s.starts_with("amended "), "{s}");
    assert_eq!(r.git(&["log", "-1", "--format=%B"]), "new\n\nbody\n\n");
}

#[test]
fn message_only_amend() {
    let r = writable();
    r.commit_file("a.txt", "one\n", "old");
    let tree = r.git(&["rev-parse", "HEAD^{tree}"]);
    let plan = prepare(&r, false, true).unwrap();
    commit(&r, &plan, "new").unwrap();
    assert_eq!(r.git(&["rev-parse", "HEAD^{tree}"]), tree);
    assert_eq!(r.git(&["log", "-1", "--format=%s"]).trim(), "new");
    assert_eq!(r.git(&["log", "--oneline"]).lines().count(), 1);
}

#[test]
fn staged_after_prepare_stops_commit() {
    let r = writable();
    r.commit_file("a.txt", "one\n", "first");
    r.write("a.txt", "two\n");
    r.git(&["add", "a.txt"]);
    let plan = prepare(&r, false, false).unwrap();
    let head = r.git(&["rev-parse", "HEAD"]);
    r.write("c.txt", "c\n");
    r.git(&["add", "c.txt"]);
    let e = commit(&r, &plan, "x").unwrap_err();
    assert_eq!(e.summary, "staged changes moved, review again");
    assert_eq!(r.git(&["rev-parse", "HEAD"]), head);
}

#[test]
fn head_moved_after_prepare_stops_commit() {
    let r = writable();
    r.commit_file("a.txt", "one\n", "first");
    let plan = prepare(&r, false, true).unwrap();
    assert_eq!(
        plan.head.as_deref(),
        Some(r.git(&["rev-parse", "HEAD"]).trim())
    );
    // Someone else commits, with the index (and so the tree) unchanged.
    r.git(&["commit", "-q", "--allow-empty", "-m", "theirs"]);
    let head = r.git(&["rev-parse", "HEAD"]);
    let e = commit(&r, &plan, "mine").unwrap_err();
    assert_eq!(e.summary, "HEAD moved, review again");
    assert_eq!(r.git(&["rev-parse", "HEAD"]), head);
    assert_eq!(r.git(&["log", "-1", "--format=%s"]).trim(), "theirs");
}

#[test]
fn unborn_head_is_pinned_as_none() {
    let r = writable();
    r.write("a.txt", "one\n");
    r.git(&["add", "a.txt"]);
    let plan = prepare(&r, false, false).unwrap();
    assert_eq!(plan.head, None);
    r.git(&["commit", "-q", "-m", "theirs"]);
    let e = commit(&r, &plan, "mine").unwrap_err();
    assert_eq!(e.summary, "HEAD moved, review again");
    assert_eq!(r.git(&["log", "--format=%s"]).trim(), "theirs");
}

#[test]
fn secret_staged_after_last_refresh_is_found() {
    let r = writable();
    r.write("cfg.env", &format!("key = {}\n", aws_key()));
    r.git(&["add", "cfg.env"]);
    let plan = prepare(&r, false, false).unwrap();
    assert!(!plan.findings.is_empty());
    assert_eq!(plan.coverage, Coverage::Full);
}

#[test]
fn failing_pre_commit_hook_surfaces_output() {
    use std::os::unix::fs::PermissionsExt;
    let r = writable();
    let hook = r.path().join(".git/hooks/pre-commit");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    std::fs::write(&hook, "#!/bin/sh\necho lint failed >&2\nexit 1\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    r.write("a.txt", "one\n");
    r.git(&["add", "a.txt"]);
    let plan = prepare(&r, false, false).unwrap();
    let e = commit(&r, &plan, "x").unwrap_err();
    assert_eq!(e.summary, "commit failed: pre-commit hook");
    assert!(e.output.contains("lint failed"), "{}", e.output);
}

#[test]
fn commit_timeout_says_commit_failed() {
    use std::os::unix::fs::PermissionsExt;
    let r = writable();
    let hook = r.path().join(".git/hooks/pre-commit");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    std::fs::write(&hook, "#!/bin/sh\nsleep 5\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    r.write("a.txt", "one\n");
    r.git(&["add", "a.txt"]);
    let plan = prepare(&r, false, false).unwrap();
    let e = backend(&r)
        .write(
            &WriteOp::Commit {
                message: "x".into(),
                amend: false,
                tree: plan.tree.clone(),
                head: plan.head.clone(),
            },
            Duration::from_millis(500),
            &AtomicBool::new(false),
        )
        .unwrap_err();
    assert_eq!(e.summary, "commit failed: timeout");
}

#[test]
fn nothing_to_commit() {
    let r = writable();
    r.commit_file("a.txt", "one\n", "first");
    let e = prepare(&r, false, false).unwrap_err();
    assert_eq!(e.summary, "nothing to commit");
    let r = writable();
    let e = prepare(&r, false, false).unwrap_err();
    assert_eq!(e.summary, "nothing to commit");
    let e = prepare(&r, false, true).unwrap_err();
    assert_eq!(e.summary, "no commit to amend");
}

#[test]
fn amend_never_stages_all() {
    let r = writable();
    r.commit_file("a.txt", "one\n", "first");
    let e = prepare(&r, true, true).unwrap_err();
    assert_eq!(e.summary, "amend never stages all");
}

#[test]
fn held_index_lock_shows_busy_summary() {
    let r = writable();
    r.write("a.txt", "one\n");
    r.git(&["add", "a.txt"]);
    let plan = prepare(&r, false, false).unwrap();
    // write-tree only reads, so the lock is taken after the pin is checked.
    std::fs::write(r.path().join(".git/index.lock"), "").unwrap();
    let e = commit(&r, &plan, "x").unwrap_err();
    assert_eq!(e.summary, "repository is busy (index.lock held), try again");
}

#[test]
fn amend_ignores_log_show_signature() {
    let r = writable();
    r.git(&["config", "log.showSignature", "true"]);
    r.write("a.txt", "one\n");
    r.git(&["add", "a.txt"]);
    let plan = prepare(&r, false, false).unwrap();
    commit(&r, &plan, "old\n\nbody").unwrap();
    let plan = prepare(&r, false, true).unwrap();
    commit(&r, &plan, "new").unwrap();
    let raw = r.git(&["cat-file", "commit", "HEAD"]);
    assert_eq!(raw.split_once("\n\n").unwrap().1, "new\n\nbody\n");
}

fn prepare_push(r: &TestRepo) -> Result<gitst::write::PushPlan, WriteError> {
    match run(r, WriteOp::PreparePush)? {
        WriteOutcome::PushReady(p) => {
            assert_eq!(p.outgoing.commit_total, p.count);
            Ok(p)
        }
        other => panic!("{other:?}"),
    }
}

fn push(r: &TestRepo, plan: &gitst::write::PushPlan) -> Result<WriteOutcome, WriteError> {
    run(r, WriteOp::Push(plan.clone()))
}

fn remote_oid(remote: &std::path::Path, branch: &str) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(remote)
        .args(["rev-parse", &format!("refs/heads/{branch}")])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A writable repository with one commit pushed to a bare `origin`.
fn with_remote() -> (TestRepo, std::path::PathBuf) {
    let r = writable();
    r.commit_file("a.txt", "one\n", "first");
    let remote = r.with_bare_remote();
    (r, remote)
}

fn identify(r: &TestRepo) {
    r.git(&["config", "user.name", "T"]);
    r.git(&["config", "user.email", "t@t"]);
    r.git(&["config", "commit.gpgsign", "false"]);
}

#[test]
fn push_ahead_commits() {
    let (r, remote) = with_remote();
    r.commit_file("b.txt", "b\n", "second");
    r.commit_file("c.txt", "c\n", "third");
    let plan = prepare_push(&r).unwrap();
    assert_eq!(plan.count, 2);
    assert_eq!(
        (plan.remote.as_str(), plan.branch.as_str()),
        ("origin", "main")
    );
    assert!(!plan.set_upstream);
    assert!(!plan.creates_branch);
    let head = r.git(&["rev-parse", "HEAD"]).trim().to_string();
    let short = r.git(&["rev-parse", "--short", &head]).trim().to_string();
    let out = push(&r, &plan).unwrap();
    assert_eq!(
        out,
        WriteOutcome::Done {
            summary: format!("pushed {short} to origin/main"),
            warning: None
        }
    );
    assert_eq!(remote_oid(&remote, "main"), head);
}

#[test]
fn push_pins_the_confirmed_commit() {
    let (r, remote) = with_remote();
    r.commit_file("b.txt", "b\n", "second");
    let plan = prepare_push(&r).unwrap();
    r.commit_file("c.txt", "c\n", "third");
    let out = push(&r, &plan).unwrap();
    assert_eq!(remote_oid(&remote, "main"), plan.oid);
    match out {
        WriteOutcome::Done { warning, .. } => {
            assert_eq!(warning.as_deref(), Some("1 newer commit not pushed"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn push_new_branch_sets_upstream() {
    let (r, remote) = with_remote();
    r.git(&["checkout", "-q", "-b", "feat"]);
    r.commit_file("b.txt", "b\n", "second");
    let plan = prepare_push(&r).unwrap();
    assert!(plan.set_upstream);
    assert!(plan.creates_branch);
    assert_eq!(plan.branch, "feat");
    assert_eq!(plan.count, 1);
    push(&r, &plan).unwrap();
    assert_eq!(
        r.git(&["rev-parse", "--abbrev-ref", "feat@{upstream}"])
            .trim(),
        "origin/feat"
    );
    assert_eq!(remote_oid(&remote, "feat"), plan.oid);
}

#[test]
fn push_to_a_deleted_remote_branch_creates_it() {
    // Merged and deleted on the remote, then pruned here: the upstream is
    // still set, but the push would create the branch again.
    let (r, remote) = with_remote();
    r.git(&["checkout", "-q", "-b", "feat"]);
    r.commit_file("b.txt", "b\n", "second");
    let plan = prepare_push(&r).unwrap();
    push(&r, &plan).unwrap();
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(&remote)
        .args(["branch", "-D", "feat"])
        .output()
        .unwrap();
    assert!(out.status.success());
    r.git(&["fetch", "-q", "--prune", "origin"]);
    r.commit_file("c.txt", "c\n", "third");
    let plan = prepare_push(&r).unwrap();
    assert!(!plan.set_upstream, "the upstream is still configured");
    assert!(plan.creates_branch);
}

#[test]
fn first_push_from_unborn_history() {
    let r = writable();
    r.commit_file("a.txt", "one\n", "first");
    let remote = r.dir.path().join("remote.git");
    r.git(&["init", "-q", "--bare", remote.to_str().unwrap()]);
    r.git(&["remote", "add", "origin", remote.to_str().unwrap()]);
    let plan = prepare_push(&r).unwrap();
    assert!(plan.set_upstream);
    assert_eq!(plan.count, 1);
    push(&r, &plan).unwrap();
    assert_eq!(remote_oid(&remote, "main"), plan.oid);
}

#[test]
fn rejected_push_says_pull_first() {
    let (r, remote) = with_remote();
    let other = TestRepo::clone_from(&remote);
    identify(&other);
    other.commit_file("o.txt", "o\n", "theirs");
    other.git(&["push", "-q", "origin", "main"]);
    r.commit_file("b.txt", "b\n", "mine");
    let plan = prepare_push(&r).unwrap();
    let e = push(&r, &plan).unwrap_err();
    assert!(
        e.summary.starts_with("push rejected: behind"),
        "{}",
        e.summary
    );
}

#[test]
fn rejected_push_counts_behind_when_tracking_ref_is_current() {
    let (r, remote) = with_remote();
    let other = TestRepo::clone_from(&remote);
    identify(&other);
    other.commit_file("o.txt", "o\n", "theirs");
    other.git(&["push", "-q", "origin", "main"]);
    r.commit_file("b.txt", "b\n", "mine");
    let plan = prepare_push(&r).unwrap();
    r.git(&["fetch", "-q", "origin"]);
    let e = push(&r, &plan).unwrap_err();
    assert!(
        e.summary.starts_with("push rejected: behind by 1"),
        "{}",
        e.summary
    );
}

#[test]
fn matching_push_default_sends_one_branch() {
    let (r, remote) = with_remote();
    r.git(&["config", "push.default", "matching"]);
    r.git(&["checkout", "-q", "-b", "other"]);
    r.commit_file("o.txt", "o\n", "o1");
    r.git(&["push", "-q", "-u", "origin", "other"]);
    r.commit_file("o2.txt", "o\n", "o2");
    r.git(&["checkout", "-q", "main"]);
    r.commit_file("m.txt", "m\n", "m1");
    let other_before = remote_oid(&remote, "other");
    let plan = prepare_push(&r).unwrap();
    push(&r, &plan).unwrap();
    assert_eq!(remote_oid(&remote, "main"), plan.oid);
    assert_eq!(remote_oid(&remote, "other"), other_before);
}

#[test]
fn push_target_ignores_a_tag_named_like_the_branch() {
    let (r, remote) = with_remote();
    r.git(&["checkout", "-q", "-b", "rel"]);
    r.git(&["tag", "rel"]);
    r.commit_file("b.txt", "b\n", "second");
    let plan = prepare_push(&r).unwrap();
    assert_eq!((plan.local.as_str(), plan.branch.as_str()), ("rel", "rel"));
    push(&r, &plan).unwrap();
    assert_eq!(remote_oid(&remote, "rel"), plan.oid);
}

#[test]
fn nothing_to_push() {
    let (r, _remote) = with_remote();
    let e = prepare_push(&r).unwrap_err();
    assert_eq!(e.summary, "nothing to push");
}

#[test]
fn several_remotes_without_origin() {
    let r = writable();
    r.commit_file("a.txt", "one\n", "first");
    r.git(&["remote", "add", "up", "/nonexistent/a"]);
    r.git(&["remote", "add", "fork", "/nonexistent/b"]);
    let e = prepare_push(&r).unwrap_err();
    assert_eq!(e.summary, "several remotes, push from a shell");
}

#[test]
fn secret_in_range_is_found() {
    let (r, _remote) = with_remote();
    r.commit_file("cfg.env", &format!("key = {}\n", aws_key()), "oops");
    let plan = prepare_push(&r).unwrap();
    assert!(!plan.findings.is_empty());
}

#[test]
fn push_follows_push_remote_config() {
    let (r, _origin) = with_remote();
    let up = r.dir.path().join("up.git");
    r.git(&["init", "-q", "--bare", up.to_str().unwrap()]);
    r.git(&["remote", "add", "up", up.to_str().unwrap()]);
    r.git(&["checkout", "-q", "-b", "newb"]);
    r.git(&["config", "branch.newb.pushRemote", "up"]);
    r.commit_file("b.txt", "b\n", "second");
    let plan = prepare_push(&r).unwrap();
    assert_eq!((plan.remote.as_str(), plan.branch.as_str()), ("up", "newb"));
    assert_eq!(plan.local, "newb");
    push(&r, &plan).unwrap();
    assert_eq!(remote_oid(&up, "newb"), plan.oid);
}

#[test]
fn push_default_upstream_uses_the_differently_named_upstream() {
    let (r, remote) = with_remote();
    r.git(&["config", "push.default", "upstream"]);
    r.git(&["checkout", "-q", "-b", "fix"]);
    r.git(&["push", "-q", "origin", "fix:bugfix"]);
    r.git(&["branch", "-q", "--set-upstream-to", "origin/bugfix", "fix"]);
    r.commit_file("b.txt", "b\n", "second");
    let plan = prepare_push(&r).unwrap();
    assert_eq!(
        (plan.remote.as_str(), plan.branch.as_str()),
        ("origin", "bugfix")
    );
    assert_eq!(plan.count, 1);
    push(&r, &plan).unwrap();
    assert_eq!(remote_oid(&remote, "bugfix"), plan.oid);
}

#[test]
fn push_refuses_a_non_branch_target() {
    let (r, _remote) = with_remote();
    r.git(&["config", "remote.origin.push", "refs/heads/*:refs/for/*"]);
    r.git(&["checkout", "-q", "-b", "fix"]);
    r.commit_file("b.txt", "b\n", "second");
    let e = prepare_push(&r).unwrap_err();
    assert_eq!(e.summary, "push target is not a branch, push from a shell");
}

#[test]
fn push_sets_tracking_for_the_confirmed_branch() {
    let (r, _remote) = with_remote();
    r.git(&["checkout", "-q", "-b", "feat"]);
    r.commit_file("b.txt", "b\n", "second");
    let plan = prepare_push(&r).unwrap();
    r.git(&["checkout", "-q", "main"]);
    push(&r, &plan).unwrap();
    assert_eq!(
        r.git(&["rev-parse", "--abbrev-ref", "feat@{upstream}"])
            .trim(),
        "origin/feat"
    );
    assert!(
        r.try_git(&["rev-parse", "--abbrev-ref", "main@{upstream}"])
            .is_ok()
    );
}

#[test]
fn push_refusals_on_unborn_and_detached() {
    let r = writable();
    assert_eq!(prepare_push(&r).unwrap_err().summary, "no commits to push");
    let (r, _remote) = with_remote();
    r.git(&["checkout", "-q", "--detach"]);
    assert_eq!(
        prepare_push(&r).unwrap_err().summary,
        "detached HEAD, push from a shell"
    );
    let r = writable();
    r.commit_file("a.txt", "one\n", "first");
    assert_eq!(
        prepare_push(&r).unwrap_err().summary,
        "no remote to push to"
    );
}

#[test]
fn push_timeout_says_timeout() {
    let (r, _remote) = with_remote();
    r.commit_file("b.txt", "b\n", "second");
    let plan = prepare_push(&r).unwrap();
    let hook = r.dir.path().join("remote.git/hooks/pre-receive");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    std::fs::write(&hook, "#!/bin/sh\nsleep 5\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let e = backend(&r)
        .write(
            &WriteOp::Push(plan),
            Duration::from_millis(500),
            &AtomicBool::new(false),
        )
        .unwrap_err();
    assert_eq!(e.summary, "push failed: timeout");
}

#[test]
fn push_follows_a_prefixed_push_refspec() {
    let (r, remote) = with_remote();
    r.git(&[
        "config",
        "remote.origin.push",
        "refs/heads/*:refs/heads/team/*",
    ]);
    r.git(&["checkout", "-q", "-b", "fix"]);
    r.commit_file("b.txt", "b\n", "second");
    let plan = prepare_push(&r).unwrap();
    assert_eq!(
        (plan.remote.as_str(), plan.branch.as_str()),
        ("origin", "team/fix")
    );
    push(&r, &plan).unwrap();
    assert_eq!(remote_oid(&remote, "team/fix"), plan.oid);
}

#[test]
fn push_uses_remote_ref_when_tracking_ref_is_nested() {
    let (r, remote) = with_remote();
    r.git(&[
        "config",
        "remote.origin.fetch",
        "+refs/heads/*:refs/remotes/origin/x/*",
    ]);
    r.git(&["fetch", "-q", "origin"]);
    r.git(&["branch", "-q", "--set-upstream-to", "origin/x/main", "main"]);
    r.git(&["config", "remote.origin.push", "refs/heads/*:refs/heads/*"]);
    r.commit_file("b.txt", "b\n", "second");
    let plan = prepare_push(&r).unwrap();
    assert_eq!(plan.branch, "main");
    push(&r, &plan).unwrap();
    assert_eq!(remote_oid(&remote, "main"), plan.oid);
}

fn pull(r: &TestRepo) -> Result<WriteOutcome, WriteError> {
    backend(r).write(
        &WriteOp::Pull,
        Duration::from_secs(120),
        &AtomicBool::new(false),
    )
}

#[test]
fn pull_fast_forwards() {
    let (r, remote) = with_remote();
    let other = TestRepo::clone_from(&remote);
    identify(&other);
    other.commit_file("o.txt", "o\n", "theirs");
    other.git(&["push", "-q", "origin", "main"]);
    let out = pull(&r).unwrap();
    assert_eq!(
        out,
        WriteOutcome::Done {
            summary: "pulled".into(),
            warning: None
        }
    );
    assert_eq!(
        r.git(&["rev-parse", "HEAD"]).trim(),
        remote_oid(&remote, "main")
    );
}

#[test]
fn pull_refuses_divergence() {
    let (r, remote) = with_remote();
    let other = TestRepo::clone_from(&remote);
    identify(&other);
    other.commit_file("o.txt", "o\n", "theirs");
    other.git(&["push", "-q", "origin", "main"]);
    r.commit_file("b.txt", "b\n", "mine");
    let e = pull(&r).unwrap_err();
    assert!(
        e.summary
            .starts_with("pull stopped: branches have diverged"),
        "{}",
        e.summary
    );
}

#[test]
fn pull_never_rebases_even_with_pull_rebase_set() {
    let (r, remote) = with_remote();
    r.git(&["config", "pull.rebase", "true"]);
    let other = TestRepo::clone_from(&remote);
    identify(&other);
    other.commit_file("o.txt", "o\n", "theirs");
    other.git(&["push", "-q", "origin", "main"]);
    r.commit_file("b.txt", "b\n", "mine");
    let mine = r.git(&["rev-parse", "HEAD"]);
    let e = pull(&r).unwrap_err();
    assert!(
        e.summary
            .starts_with("pull stopped: branches have diverged"),
        "{}",
        e.summary
    );
    assert_eq!(r.git(&["rev-parse", "HEAD"]), mine, "not rebased");
    // Recent git refuses the rebase above even without `--no-rebase`, so
    // the fast-forward's reflog entry is where the flag shows.
    r.git(&["reset", "-q", "--hard", "HEAD~1"]);
    pull(&r).unwrap();
    let entry = r.git(&["reflog", "-1", "--format=%gs"]);
    assert!(entry.contains("--no-rebase"), "{entry}");
}

fn stash_op(r: &TestRepo, op: WriteOp) -> Result<WriteOutcome, WriteError> {
    run(r, op)
}

fn stash_oid(r: &TestRepo, n: usize) -> String {
    r.git(&["rev-parse", &format!("stash@{{{n}}}")])
        .trim()
        .to_string()
}

fn done_with(summary: &str) -> WriteOutcome {
    WriteOutcome::Done {
        summary: summary.into(),
        warning: None,
    }
}

#[test]
fn stash_push_includes_untracked() {
    let r = writable();
    r.commit_file("a.txt", "one\n", "first");
    r.write("a.txt", "two\n");
    r.write("new.txt", "n\n");
    let out = stash_op(
        &r,
        WriteOp::Stash {
            message: Some("wip".into()),
        },
    )
    .unwrap();
    assert_eq!(out, done_with("stashed"));
    assert_eq!(r.git(&["status", "--porcelain"]).trim(), "");
    assert!(r.git(&["stash", "list"]).contains("wip"));
    let e = stash_op(&r, WriteOp::Stash { message: None }).unwrap_err();
    assert_eq!(e.summary, "nothing to stash");
}

#[test]
fn stash_pop_and_apply_and_drop() {
    let r = writable();
    r.commit_file("a.txt", "one\n", "first");
    r.write("a.txt", "two\n");
    r.git(&["stash", "push", "-m", "s1"]);
    let oid = stash_oid(&r, 0);
    let out = stash_op(
        &r,
        WriteOp::StashApply {
            index: 0,
            oid: oid.clone(),
        },
    )
    .unwrap();
    assert_eq!(out, done_with("applied stash@{0}"));
    assert_eq!(r.git(&["stash", "list"]).lines().count(), 1);
    r.git(&["checkout", "--", "a.txt"]);
    let out = stash_op(
        &r,
        WriteOp::StashPop {
            index: 0,
            oid: oid.clone(),
        },
    )
    .unwrap();
    assert_eq!(out, done_with("popped stash@{0}"));
    assert_eq!(r.git(&["stash", "list"]).trim(), "");
    assert_eq!(r.git(&["status", "--porcelain"]).trim(), "M a.txt");
    r.git(&["stash", "push", "-m", "s2"]);
    let oid = stash_oid(&r, 0);
    let out = stash_op(&r, WriteOp::StashDrop { index: 0, oid }).unwrap();
    assert_eq!(out, done_with("dropped stash@{0}"));
    assert_eq!(r.git(&["stash", "list"]).trim(), "");
}

#[test]
fn shifted_stash_ref_is_refused() {
    let r = writable();
    r.commit_file("a.txt", "one\n", "first");
    r.write("a.txt", "two\n");
    r.git(&["stash", "push", "-m", "A"]);
    let a = stash_oid(&r, 0);
    r.write("a.txt", "three\n");
    r.git(&["stash", "push", "-m", "B"]);
    let e = stash_op(&r, WriteOp::StashDrop { index: 0, oid: a }).unwrap_err();
    assert_eq!(e.summary, "stashes changed, refreshed");
    assert_eq!(r.git(&["stash", "list"]).lines().count(), 2);
}

#[test]
fn stash_apply_retries_without_index() {
    let r = writable();
    r.commit_file("a.txt", "1\n", "first");
    r.write("a.txt", "2\n");
    r.git(&["add", "a.txt"]);
    r.write("a.txt", "3\n");
    r.git(&["stash", "push", "-m", "s"]);
    let oid = stash_oid(&r, 0);
    // The index already holds the stash's work tree, so its staged patch
    // no longer applies; `apply --index` fails with "Try without --index".
    r.write("a.txt", "3\n");
    r.git(&["add", "a.txt"]);
    assert!(r.try_git(&["stash", "apply", "--index"]).is_err());
    let out = stash_op(&r, WriteOp::StashApply { index: 0, oid }).unwrap();
    assert_eq!(
        out,
        WriteOutcome::Done {
            summary: "applied stash@{0}".into(),
            warning: Some("index not restored".into())
        }
    );
}

#[test]
fn stash_ref_that_no_longer_exists_is_refused() {
    let r = writable();
    r.commit_file("a.txt", "one\n", "first");
    let e = stash_op(
        &r,
        WriteOp::StashDrop {
            index: 0,
            oid: "0".repeat(40),
        },
    )
    .unwrap_err();
    assert_eq!(e.summary, "stashes changed, refreshed");
}

// ---- hunks ----

/// The file view's hunks, as the detail view shows them, with whether each
/// is in the staged block.
fn shown_hunks(r: &TestRepo, path: &str) -> Vec<(bool, ShownHunk)> {
    hunks_from(&backend(r), path)
}

/// The hunks `b` shows for `path`; a backend that has taken a snapshot
/// reads through its guard.
fn hunks_from(b: &CliBackend, path: &str) -> Vec<(bool, ShownHunk)> {
    let req = DetailReq::File { path: path.into() };
    let Ok(DetailData::File { blocks, .. }) = b.detail(&req) else {
        panic!("no file view of {path}");
    };
    let mut out: Vec<(bool, ShownHunk)> = Vec::new();
    for b in &blocks {
        let mut open = false;
        for l in &b.lines {
            if l.kind == DiffKind::Hunk {
                let hunk = ShownHunk {
                    header: l.text.clone(),
                    lines: Vec::new(),
                };
                out.push((b.staged(), hunk));
                open = true;
            } else if open {
                out.last_mut().unwrap().1.lines.push(l.clone());
            }
        }
    }
    out
}

fn apply(
    r: &TestRepo,
    path: &str,
    hunk: ShownHunk,
    reverse: bool,
) -> Result<WriteOutcome, WriteError> {
    run(
        r,
        WriteOp::ApplyHunk {
            path: path.into(),
            hunk,
            reverse,
        },
    )
}

fn hunk_count(diff: &str) -> usize {
    diff.lines().filter(|l| l.starts_with("@@")).count()
}

/// Twenty numbered lines, with lines 2 and 18 changed when `edited`.
fn numbered(edited: bool) -> String {
    (1..=20)
        .map(|i| match (edited, i) {
            (true, 2 | 18) => format!("line {i} changed\n"),
            _ => format!("line {i}\n"),
        })
        .collect()
}

#[test]
fn stage_one_of_two_hunks() {
    let r = writable();
    // The patch is read with `a/` and `b/` whatever these say.
    r.git(&["config", "diff.noprefix", "true"]);
    r.git(&["config", "diff.mnemonicPrefix", "true"]);
    r.commit_file("a.txt", &numbered(false), "first");
    r.write("a.txt", &numbered(true));
    let hunks = shown_hunks(&r, "a.txt");
    assert_eq!(hunks.len(), 2);
    let out = apply(&r, "a.txt", hunks[0].1.clone(), false).unwrap();
    assert_eq!(out, done_with("staged hunk"));
    let cached = r.git(&["diff", "--cached"]);
    assert_eq!(hunk_count(&cached), 1, "{cached}");
    assert!(cached.contains("+line 2 changed"), "{cached}");
    let unstaged = r.git(&["diff"]);
    assert_eq!(hunk_count(&unstaged), 1, "{unstaged}");
    assert!(unstaged.contains("+line 18 changed"), "{unstaged}");
}

#[test]
fn unstage_hunk_round_trip() {
    let r = writable();
    r.commit_file("a.txt", &numbered(false), "first");
    r.write("a.txt", &numbered(true));
    let hunks = shown_hunks(&r, "a.txt");
    apply(&r, "a.txt", hunks[1].1.clone(), false).unwrap();
    let staged: Vec<ShownHunk> = shown_hunks(&r, "a.txt")
        .into_iter()
        .filter(|(staged, _)| *staged)
        .map(|(_, h)| h)
        .collect();
    assert_eq!(staged.len(), 1);
    let out = apply(&r, "a.txt", staged[0].clone(), true).unwrap();
    assert_eq!(out, done_with("unstaged hunk"));
    assert_eq!(
        r.git(&["rev-parse", "HEAD:a.txt"]),
        r.git(&["rev-parse", ":a.txt"]),
        "the index equals HEAD again"
    );
    assert_eq!(hunk_count(&r.git(&["diff"])), 2);
}

#[test]
fn crlf_and_no_newline_hunks_apply() {
    let r = writable();
    let path = r.path().join("w.txt");
    std::fs::write(&path, "one\r\n\ttwo\r\nthree\r\nfour").unwrap();
    r.git(&["add", "w.txt"]);
    r.git(&["commit", "-q", "-m", "first"]);
    std::fs::write(&path, "one\r\n\ttwo changed\r\nthree\r\nfour, last").unwrap();
    let hunks = shown_hunks(&r, "w.txt");
    assert_eq!(hunks.len(), 1);
    assert!(
        hunks[0].1.lines.iter().any(|l| l.text == "    two changed"),
        "{hunks:?}"
    );
    apply(&r, "w.txt", hunks[0].1.clone(), false).unwrap();
    assert_eq!(r.git(&["diff"]), "", "the whole change is staged");
    let staged = r.git(&["show", ":w.txt"]);
    assert_eq!(staged, "one\r\n\ttwo changed\r\nthree\r\nfour, last");
}

#[test]
fn hunk_in_odd_path() {
    let r = writable();
    r.write("-x *[a].rs", &numbered(false));
    r.write("x.rs", "1\n");
    r.git(&["add", "--", "-x *[a].rs", "x.rs"]);
    r.git(&["commit", "-q", "-m", "first"]);
    r.write("-x *[a].rs", &numbered(true));
    r.write("x.rs", "2\n");
    let hunks = shown_hunks(&r, "-x *[a].rs");
    apply(&r, "-x *[a].rs", hunks[0].1.clone(), false).unwrap();
    assert_eq!(staged(&r), "-x *[a].rs");
}

#[test]
fn file_changed_since_view() {
    let r = writable();
    r.commit_file("a.txt", &numbered(false), "first");
    r.write("a.txt", &numbered(true));
    let hunks = shown_hunks(&r, "a.txt");
    r.write(
        "a.txt",
        &numbered(false).replace("line 2\n", "line 2 again\n"),
    );
    let e = apply(&r, "a.txt", hunks[0].1.clone(), false).unwrap_err();
    assert_eq!(e.summary, "file changed, refreshed");
    assert_eq!(staged(&r), "");
    // A staged hunk that is no longer staged.
    r.git(&["add", "a.txt"]);
    let staged_hunk = shown_hunks(&r, "a.txt")[0].1.clone();
    r.git(&["reset", "-q"]);
    let e = apply(&r, "a.txt", staged_hunk, true).unwrap_err();
    assert_eq!(e.summary, "file changed, refreshed");
}

/// A repository whose own config has a clean filter for `n.txt`, as
/// `nbstripout --install` writes one.
fn filtered() -> TestRepo {
    let r = writable();
    r.git(&["config", "filter.strip.clean", "sed s/SECRET/xxx/"]);
    r.git(&["config", "filter.strip.smudge", "cat"]);
    r.write(".gitattributes", "n.txt filter=strip\n");
    r.write("n.txt", &numbered(false));
    r.git(&["add", ".gitattributes", "n.txt"]);
    r.git(&["commit", "-q", "-m", "first"]);
    r.write(
        "n.txt",
        &numbered(false).replace("line 2\n", "line 2 SECRET\n"),
    );
    r
}

#[test]
fn filtered_files_stage_as_a_whole() {
    let r = filtered();
    let req = DetailReq::File {
        path: "n.txt".into(),
    };
    let Ok(DetailData::File { filtered, .. }) = backend(&r).detail(&req) else {
        panic!("no file view");
    };
    assert!(filtered, "hunks of a filtered file can't be staged alone");
    let req = DetailReq::File {
        path: "x.txt".into(),
    };
    r.commit_file("x.txt", "1\n", "x");
    r.write("x.txt", "2\n");
    let Ok(DetailData::File { filtered, .. }) = backend(&r).detail(&req) else {
        panic!("no file view");
    };
    assert!(!filtered);
    run(
        &r,
        WriteOp::Stage {
            path: "n.txt".into(),
        },
    )
    .unwrap();
    assert!(r.git(&["show", ":n.txt"]).contains("line 2 xxx\n"));
}

#[test]
fn a_hunk_is_cut_from_the_diff_git_add_would_stage() {
    let r = filtered();
    // The guard turns the repository's own filter off for reads, so the
    // view shows the file as it is on disk.
    let b = backend(&r);
    b.snapshot(&SnapshotOpts {
        max_changes: 1000,
        numstat_max_files: 500,
        commits: 50,
    })
    .unwrap();
    let hunks = hunks_from(&b, "n.txt");
    assert!(
        hunks[0].1.lines.iter().any(|l| l.text == "line 2 SECRET"),
        "{hunks:?}"
    );
    let e = apply(&r, "n.txt", hunks[0].1.clone(), false).unwrap_err();
    assert_eq!(e.summary, "file changed, refreshed");
    assert_eq!(staged(&r), "");
}

#[test]
fn staging_a_hunk_leaves_the_mode_change() {
    use std::os::unix::fs::PermissionsExt;
    let r = writable();
    r.commit_file("m.sh", &numbered(false), "first");
    r.write("m.sh", &numbered(true));
    let path = r.path().join("m.sh");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(r.git(&["diff"]).contains("new mode 100755"));
    let hunks = shown_hunks(&r, "m.sh");
    apply(&r, "m.sh", hunks[0].1.clone(), false).unwrap();
    assert!(r.git(&["ls-files", "-s", "m.sh"]).starts_with("100644 "));
    let cached = r.git(&["diff", "--cached"]);
    assert!(
        cached.contains("+line 2 changed") && !cached.contains("mode"),
        "{cached}"
    );
    assert!(r.git(&["diff"]).contains("new mode 100755"));
}

fn empty_tree(r: &TestRepo) -> String {
    // The object id of a tree with no entries, in this repository's hash.
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(r.path())
        .args(["hash-object", "-t", "tree", "/dev/null"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn push_plan_lists_outgoing_commits_and_files() {
    let (r, _remote) = with_remote();
    let base = r.git(&["rev-parse", "HEAD"]).trim().to_string();
    r.commit_file("a.txt", "one\ntwo\n", "second");
    r.write("b.bin", "\0\u{1}\0");
    r.write("a.txt", "uno\ntwo\nthree\n");
    r.git(&["add", "."]);
    r.git(&["commit", "-q", "-m", "third"]);
    let o = prepare_push(&r).unwrap().outgoing;
    assert_eq!(o.target, "origin/main");
    assert_eq!(o.base, base);
    assert_eq!(o.oid, r.git(&["rev-parse", "HEAD"]).trim());
    assert_eq!((o.commits.len(), o.commit_total), (2, 2));
    assert_eq!(o.commits[0].subject, "third");
    let files: Vec<_> = o
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.status, f.counts))
        .collect();
    assert_eq!(
        files,
        vec![
            ("a.txt", 'M', gitst::model::Counts::lines(3, 1)),
            ("b.bin", 'A', gitst::model::Counts::Binary),
        ]
    );
    assert_eq!(o.file_total, 2);
    assert_eq!((o.added, o.removed), (3, 1));
}

#[test]
fn new_branch_base_is_parent_of_oldest() {
    let (r, _remote) = with_remote();
    let main_oid = r.git(&["rev-parse", "HEAD"]).trim().to_string();
    r.git(&["checkout", "-q", "-b", "feat"]);
    r.commit_file("f1.txt", "1\n", "f1");
    r.commit_file("f2.txt", "2\n", "f2");
    let o = prepare_push(&r).unwrap().outgoing;
    assert_eq!(o.base, main_oid);
    assert_eq!(o.target, "origin/feat");
    assert_eq!(o.commit_total, 2);
    let paths: Vec<_> = o.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["f1.txt", "f2.txt"]);
}

#[test]
fn new_branch_from_root_uses_empty_tree() {
    let r = writable();
    r.commit_file("a.txt", "one\n", "first");
    let remote = r.dir.path().join("remote.git");
    r.git(&["init", "-q", "--bare", remote.to_str().unwrap()]);
    r.git(&["remote", "add", "origin", remote.to_str().unwrap()]);
    let o = prepare_push(&r).unwrap().outgoing;
    assert_eq!(o.base, empty_tree(&r));
    assert_eq!(o.commit_total, 1);
    assert_eq!(o.files.len(), 1);
    assert_eq!(o.files[0].path, "a.txt");
    assert_eq!(o.files[0].status, 'A');
}

#[test]
fn oldest_outgoing_merge_uses_first_parent() {
    let (r, _remote) = with_remote();
    let main_oid = r.git(&["rev-parse", "HEAD"]).trim().to_string();
    r.git(&["checkout", "-q", "-b", "side"]);
    r.commit_file("s.txt", "s\n", "side");
    r.git(&["push", "-q", "origin", "side"]);
    r.git(&["checkout", "-q", "-b", "feat", "main"]);
    r.git(&["merge", "-q", "--no-ff", "-m", "merge", "side"]);
    let o = prepare_push(&r).unwrap().outgoing;
    assert_eq!(o.base, main_oid);
    assert_eq!(o.commit_total, 1);
    assert_eq!(o.files.len(), 1);
}

#[test]
fn file_list_caps_at_500() {
    let (r, _remote) = with_remote();
    for i in 0..501 {
        r.write(&format!("many/f{i:03}.txt"), "x\n");
    }
    r.git(&["add", "."]);
    r.git(&["commit", "-q", "-m", "many"]);
    let o = prepare_push(&r).unwrap().outgoing;
    assert_eq!((o.files.len(), o.file_total), (500, 501));
    assert_eq!(o.added, 501);
}

#[test]
fn diverged_tracking_lists_only_local_changes() {
    let (r, remote) = with_remote();
    let other = TestRepo::clone_from(&remote);
    identify(&other);
    other.commit_file("r.txt", "r\n", "theirs");
    other.commit_file("r2.txt", "r\n", "theirs 2");
    other.git(&["push", "-q", "origin", "main"]);
    r.git(&["fetch", "-q", "origin"]);
    let fork = r.git(&["rev-parse", "HEAD"]).trim().to_string();
    r.commit_file("m.txt", "m\n", "mine");
    let plan = prepare_push(&r).unwrap();
    assert_eq!(plan.count, 1);
    assert_eq!(plan.outgoing.base, fork);
    let o = plan.outgoing;
    let paths: Vec<_> = o.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["m.txt"]);
    assert_eq!((o.added, o.removed), (1, 0));
}

/// The browse view's data for `r`, as clicking `↑N` loads it.
fn browsed(r: &TestRepo) -> gitst::model::Outgoing {
    match backend(r).detail(&DetailReq::Outgoing).unwrap() {
        DetailData::Outgoing(Some(o)) => o,
        other => panic!("{other:?}"),
    }
}

/// Signs commits in `r` with a fresh SSH key, so `log.showSignature`
/// has a signature to report. `None` where `ssh-keygen` can't run.
fn ssh_signing(r: &TestRepo) -> Option<()> {
    let key = r.dir.path().join("key");
    let made = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "t@t", "-f"])
        .arg(&key)
        .output()
        .ok()?;
    if !made.status.success() {
        return None;
    }
    let public = std::fs::read_to_string(key.with_extension("pub")).unwrap();
    let signers = r.dir.path().join("allowed_signers");
    std::fs::write(&signers, format!("t@t {public}")).unwrap();
    r.git(&["config", "gpg.format", "ssh"]);
    r.git(&["config", "user.signingkey", key.to_str().unwrap()]);
    r.git(&[
        "config",
        "gpg.ssh.allowedSignersFile",
        signers.to_str().unwrap(),
    ]);
    Some(())
}

#[test]
fn push_plan_ignores_log_show_signature() {
    let (r, _remote) = with_remote();
    if ssh_signing(&r).is_none() {
        eprintln!("skipped: ssh-keygen is not available");
        return;
    }
    r.git(&["config", "log.showSignature", "true"]);
    r.write("b.txt", "b\n");
    r.git(&["add", "b.txt"]);
    r.git(&["commit", "-q", "-S", "-m", "signed"]);
    let head = r.git(&["rev-parse", "HEAD"]).trim().to_string();
    let plan = prepare_push(&r).unwrap();
    let c = &plan.outgoing.commits[0];
    assert_eq!(c.oid, head);
    assert_eq!(c.subject, "signed");
}

#[test]
fn browse_and_push_agree_on_the_target() {
    let (r, _remote) = with_remote();
    r.git(&["checkout", "-q", "-b", "feat", "--track", "origin/main"]);
    r.git(&["config", "push.default", "current"]);
    r.commit_file("f.txt", "f\n", "feat");
    let plan = prepare_push(&r).unwrap();
    assert_eq!(plan.outgoing.target, "origin/feat");
    let o = browsed(&r);
    assert_eq!(o.target, plan.outgoing.target);
    assert_eq!(o.base, plan.outgoing.base);
    assert_eq!(o.commit_total, plan.count);
}

#[test]
fn criss_cross_lists_as_many_commits_as_it_counts() {
    let (r, _remote) = with_remote();
    // Two merges that cross: the pushed tip and HEAD share two best
    // merge bases, `x1` and `y1`.
    r.git(&["checkout", "-q", "-b", "x"]);
    r.commit_file("x.txt", "x\n", "x1");
    r.git(&["checkout", "-q", "-b", "y", "main"]);
    r.commit_file("y.txt", "y\n", "y1");
    let y1 = r.git(&["rev-parse", "HEAD"]).trim().to_string();
    r.git(&["merge", "-q", "--no-ff", "-m", "y merges x", "x"]);
    r.git(&["push", "-q", "origin", "y:main"]);
    r.git(&["checkout", "-q", "x"]);
    r.git(&["merge", "-q", "--no-ff", "-m", "x merges y1", &y1]);
    r.git(&["checkout", "-q", "main"]);
    r.git(&["merge", "-q", "--ff-only", "x"]);
    r.git(&["fetch", "-q", "origin"]);
    r.commit_file("m.txt", "m\n", "mine");
    let plan = prepare_push(&r).unwrap();
    assert_eq!(plan.count, 2, "x merges y1, and mine");
    let o = &plan.outgoing;
    assert_eq!((o.commits.len(), o.commit_total), (2, 2));
    let browse = browsed(&r);
    assert_eq!(browse.commit_total, plan.count);
    assert_eq!(browse.commits, o.commits);
}
