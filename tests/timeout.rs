//! A git command that hangs is stopped, with everything it started, when it
//! times out and when gitst quits. In a binary of its own: the hang comes
//! from an fsmonitor hook in the user's global config, which only the
//! environment can point to, and every test in a binary shares the
//! environment.
#![cfg(unix)]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::mpsc::channel;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use common::{TestRepo, exits_within, wait_for_pid};
use gitst::git::{CliBackend, GitBackend, Repo, SnapshotOpts};
use gitst::worker::{self, WorkerConfig};

fn opts() -> SnapshotOpts {
    SnapshotOpts {
        max_changes: 1000,
        numstat_max_files: 500,
        commits: 50,
    }
}

/// Makes every `git status` in this process wait on an fsmonitor hook that
/// writes its pid next to the repository, as `hook.pid`, and then hangs.
/// A hook in the repository's own config never runs, so this one is the
/// user's.
fn hang_every_status() {
    static GLOBAL: OnceLock<PathBuf> = OnceLock::new();
    GLOBAL.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap().keep();
        let hook = dir.join("hook.sh");
        // The hook runs in the work tree's root.
        std::fs::write(&hook, "#!/bin/sh\necho $$ > ../hook.pid\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        let global = dir.join("global.gitconfig");
        std::fs::write(
            &global,
            format!("[core]\n\tfsmonitor = {}\n", hook.display()),
        )
        .unwrap();
        // SAFETY: every test calls this first, and `OnceLock` makes the
        // others wait, so no other thread reads the environment meanwhile.
        unsafe { std::env::set_var("GIT_CONFIG_GLOBAL", &global) };
        global
    });
}

fn repo() -> (TestRepo, PathBuf) {
    hang_every_status();
    let r = TestRepo::new();
    r.commit_file("a", "1", "one");
    let pid_file = r.dir.path().join("hook.pid");
    (r, pid_file)
}

#[test]
fn hung_git_command_times_out() {
    let (r, pid_file) = repo();
    let b = CliBackend::new(Repo::discover(&r.path()).ok().unwrap())
        .with_timeout(Duration::from_millis(500));
    let t = Instant::now();
    let err = b.snapshot(&opts()).unwrap_err();
    assert!(err.message.contains("timed out"), "{err}");
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    let pid = wait_for_pid(&pid_file);
    assert!(
        exits_within(&pid, Duration::from_secs(2)),
        "hook {pid} outlived the timeout"
    );
}

#[test]
fn quitting_stops_the_running_snapshot() {
    let (r, pid_file) = repo();
    let b = CliBackend::new(Repo::discover(&r.path()).ok().unwrap());
    let (tx, _rx) = channel();
    let cfg = WorkerConfig {
        opts: opts(),
        interval: Duration::ZERO,
        prune: false,
    };
    let w = worker::spawn(Arc::new(b), cfg, tx);
    let pid = wait_for_pid(&pid_file);
    let t = Instant::now();
    w.shutdown();
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    assert!(
        exits_within(&pid, Duration::from_secs(2)),
        "hook {pid} outlived the quit"
    );
}
