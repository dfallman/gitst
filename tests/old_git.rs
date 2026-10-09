//! With a git too old for `git config --show-scope` (before 2.26), the
//! config cannot be read, and the snapshot says so instead of acting as if
//! it were empty. In a binary of its own: the old git is a wrapper found
//! through `PATH`, which every test in a binary shares.
#![cfg(unix)]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::OnceLock;

use common::TestRepo;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use gitst::git::{CliBackend, FetchError, GitBackend, Repo, SnapshotOpts};

/// Puts a `git` first on `PATH` that refuses `--show-scope` the way git
/// 2.25 does, and passes everything else on to the real one.
fn old_git_on_path() {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let real = std::env::split_paths(&path)
            .map(|d| d.join("git"))
            .find(|g| g.is_file())
            .expect("a real git on PATH");
        let dir = tempfile::tempdir().unwrap().keep();
        let fake = dir.join("git");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do\n  if [ \"$a\" = --show-scope ]; then\n    echo \"error: unknown option \\`show-scope'\" >&2\n    exit 129\n  fi\ndone\nexec {} \"$@\"\n",
                real.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut dirs = vec![dir.clone()];
        dirs.extend(std::env::split_paths(&path));
        // SAFETY: every test calls this first, and `OnceLock` makes the
        // others wait, so no other thread reads the environment meanwhile.
        unsafe { std::env::set_var("PATH", std::env::join_paths(dirs).unwrap()) };
        dir
    });
}

#[test]
fn an_unreadable_config_is_reported_and_the_rest_still_works() {
    old_git_on_path();
    let r = TestRepo::new();
    r.commit_file("a", "1\n", "one");
    r.write("b", "2\n");
    let b = CliBackend::new(Repo::discover(&r.path()).ok().unwrap());
    let s = b
        .snapshot(&SnapshotOpts {
            max_changes: 1000,
            numstat_max_files: 500,
            commits: 50,
        })
        .unwrap();
    assert_eq!(s.changes.len(), 1, "{:?}", s.changes);
    assert_eq!(s.commits.len(), 1);
    assert!(
        s.config_error
            .as_deref()
            .is_some_and(|e| e.contains("show-scope")),
        "{:?}",
        s.config_error
    );
}

#[test]
fn a_fetch_does_not_run_with_an_unreadable_config() {
    // With no config, the guard would take away the user's own credential
    // helpers and ssh command; better not to fetch.
    old_git_on_path();
    let r = TestRepo::new();
    r.commit_file("a", "1\n", "one");
    r.with_bare_remote();
    let b = CliBackend::new(Repo::discover(&r.path()).ok().unwrap());
    let err = b
        .fetch(false, Duration::from_secs(30), &AtomicBool::new(false))
        .unwrap_err();
    assert!(
        matches!(&err, FetchError::Other(m) if m.contains("repo config not read")),
        "{err:?}"
    );
}
