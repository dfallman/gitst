//! Types and pure rules for write mode: what a write op is, when a verb is
//! refused, and how git's stderr is turned into a short message.

use crate::git::{FetchError, classify_fetch_stderr};
use crate::model::{DiffLine, Head, Leak, RepoOp, Snapshot};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteOp {
    Stage {
        path: String,
    },
    Unstage {
        path: String,
        unborn: bool,
    },
    StageAll,
    UnstageAll {
        unborn: bool,
    },
    ApplyHunk {
        path: String,
        hunk: ShownHunk,
        reverse: bool,
    },
    PrepareCommit {
        stage_all: bool,
        amend: bool,
    },
    Commit {
        message: String,
        amend: bool,
        tree: String,
        /// HEAD when the commit was prepared, `None` on an unborn branch.
        head: Option<String>,
    },
    PreparePush,
    Push(PushPlan),
    Pull,
    Stash {
        message: Option<String>,
    },
    StashPop {
        index: usize,
        oid: String,
    },
    StashApply {
        index: usize,
        oid: String,
    },
    StashDrop {
        index: usize,
        oid: String,
    },
}

/// A hunk as the detail view shows it, used to find it again in a fresh diff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShownHunk {
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Coverage {
    Full,
    Off,
    Partial(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitPlan {
    pub tree: String,
    /// HEAD's full oid when the tree was pinned, `None` on an unborn branch.
    pub head: Option<String>,
    pub findings: Vec<Leak>,
    pub coverage: Coverage,
    pub amend: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushPlan {
    /// The local branch confirmed, which tracking is set for.
    pub local: String,
    pub remote: String,
    pub branch: String,
    pub oid: String,
    pub count: usize,
    pub set_upstream: bool,
    pub findings: Vec<Leak>,
    pub coverage: Coverage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteOutcome {
    /// Shown in the bottom row for about three seconds.
    Done {
        summary: String,
        warning: Option<String>,
    },
    CommitReady(CommitPlan),
    PushReady(PushPlan),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteError {
    pub summary: String,
    pub output: String,
}

impl WriteError {
    /// The warning band's text for this failure, which is also the key
    /// clearing it remembers.
    pub fn warning(&self) -> String {
        format!("{} · ! for output", self.summary)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verb {
    Commit,
    Push,
    Pull,
    Stash,
    StashAction,
}

pub const REFUSE_NO_REMOTE: &str = "no remote to push to";
pub const REFUSE_DETACHED: &str = "detached HEAD, push from a shell";
pub const REFUSE_UNBORN: &str = "no commits to push";

/// Why `verb` can't run now, or `None`.
pub fn refusal(verb: Verb, snap: &Snapshot) -> Option<String> {
    let blocked_by_op = matches!(
        verb,
        Verb::Commit | Verb::Pull | Verb::Stash | Verb::StashAction
    );
    if blocked_by_op {
        let name = match &snap.op {
            Some(RepoOp::Merge) => Some("merge"),
            Some(RepoOp::Rebase { .. }) => Some("rebase"),
            Some(RepoOp::CherryPick) => Some("cherry-pick"),
            Some(RepoOp::Revert) => Some("revert"),
            Some(RepoOp::Bisect) | None => None,
        };
        if let Some(name) = name {
            return Some(format!(
                "{name} in progress, finish or abort it in your shell"
            ));
        }
    }
    match verb {
        Verb::Push => {
            if !snap.has_remote {
                Some(REFUSE_NO_REMOTE.into())
            } else if matches!(snap.head, Head::Detached(_)) {
                Some(REFUSE_DETACHED.into())
            } else if matches!(snap.head, Head::Unborn(_)) {
                Some(REFUSE_UNBORN.into())
            } else {
                None
            }
        }
        Verb::Pull if snap.upstream.is_none() => Some("no upstream to pull from".into()),
        _ => None,
    }
}

/// The previous message with its first line replaced by `subject`.
pub fn amend_message(previous: &str, subject: &str) -> String {
    match previous.split_once('\n') {
        Some((_, rest)) => format!("{subject}\n{rest}"),
        None => subject.to_string(),
    }
}

fn first_line(stderr: &str) -> String {
    stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("unknown error")
        .trim_start_matches("fatal: ")
        .trim_start_matches("error: ")
        .to_string()
}

/// `push rejected: behind by N, pull first`, `push failed: auth, push from a shell`, ….
pub fn classify_push(stderr: &str, behind: Option<u32>) -> String {
    // A "[remote rejected]" (protected branch, server hook) is not "behind".
    let remote_rejected = stderr.contains("[remote rejected]");
    if !remote_rejected
        && (stderr.contains("[rejected]")
            || stderr.contains("non-fast-forward")
            || stderr.contains("fetch first"))
    {
        return match behind {
            Some(n) => format!("push rejected: behind by {n}, pull first"),
            None => "push rejected: behind, pull first".into(),
        };
    }
    match classify_fetch_stderr(stderr) {
        FetchError::Auth => "push failed: auth, push from a shell".into(),
        FetchError::Offline => "push failed: offline".into(),
        FetchError::Timeout => "push failed: timeout".into(),
        FetchError::Cancelled => "push failed: cancelled".into(),
        FetchError::Other(m) => format!("push failed: {m}"),
    }
}

/// `pull stopped: branches have diverged, merge or rebase from a shell`, ….
pub fn classify_pull(stderr: &str) -> String {
    if stderr.contains("Not possible to fast-forward")
        || stderr.contains("divergent branches")
        || stderr.contains("Need to specify how to reconcile")
    {
        return "pull stopped: branches have diverged, merge or rebase from a shell".into();
    }
    if stderr.contains("would be overwritten") {
        let first = stderr
            .lines()
            .filter(|l| l.starts_with('\t') || l.starts_with("    "))
            .map(str::trim)
            .find(|l| !l.is_empty());
        return match first {
            None => "pull stopped: local changes would be overwritten".into(),
            Some(f) => format!("pull stopped: local changes would be overwritten · {f}"),
        };
    }
    if stderr.contains("CONFLICT") {
        return "pull stopped: conflicts, resolve them in your shell".into();
    }
    match classify_fetch_stderr(stderr) {
        FetchError::Auth => "pull failed: auth, pull from a shell".into(),
        FetchError::Offline => "pull failed: offline".into(),
        FetchError::Timeout => "pull failed: timeout".into(),
        FetchError::Cancelled => "pull failed: cancelled".into(),
        FetchError::Other(m) => format!("pull failed: {m}"),
    }
}

/// `commit failed: pre-commit hook`, `commit failed: commit-msg hook`, or git's first line.
pub fn classify_commit(stderr: &str) -> String {
    // Best-effort substring heuristic: git has no fixed hook message.
    if stderr.contains("pre-commit") {
        "commit failed: pre-commit hook".into()
    } else if stderr.contains("commit-msg") {
        "commit failed: commit-msg hook".into()
    } else {
        format!("commit failed: {}", first_line(stderr))
    }
}

/// Whether stderr says `index.lock` is held.
pub fn lock_held(stderr: &str) -> bool {
    stderr.contains("index.lock") && stderr.contains("File exists")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_during_repo_ops() {
        let mut s = Snapshot {
            op: Some(RepoOp::Merge),
            ..Snapshot::default()
        };
        assert_eq!(
            refusal(Verb::Commit, &s).as_deref(),
            Some("merge in progress, finish or abort it in your shell")
        );
        assert!(refusal(Verb::Pull, &s).is_some() && refusal(Verb::Stash, &s).is_some());
        assert!(refusal(Verb::StashAction, &s).is_some());
        s.has_remote = true;
        s.head = Head::Branch("main".into());
        s.oid = Some("a".into());
        assert!(refusal(Verb::Push, &s).is_none());
        s.op = Some(RepoOp::Bisect);
        assert!(refusal(Verb::Commit, &s).is_none());
    }

    #[test]
    fn push_refusals() {
        let s = Snapshot {
            head: Head::Detached("abc".into()),
            has_remote: true,
            ..Snapshot::default()
        };
        assert!(refusal(Verb::Push, &s).is_some());
        let s = Snapshot {
            head: Head::Branch("main".into()),
            has_remote: false,
            ..Snapshot::default()
        };
        assert!(refusal(Verb::Push, &s).is_some());
        let s = Snapshot {
            has_remote: true,
            ..Snapshot::default()
        };
        assert_eq!(
            refusal(Verb::Push, &s).as_deref(),
            Some("no commits to push")
        );
    }

    #[test]
    fn pull_needs_upstream() {
        let s = Snapshot {
            head: Head::Branch("main".into()),
            has_remote: true,
            ..Snapshot::default()
        };
        assert!(refusal(Verb::Pull, &s).is_some());
    }

    #[test]
    fn amend_keeps_body() {
        assert_eq!(
            amend_message("old\n\nbody line\n# heading\n", "new"),
            "new\n\nbody line\n# heading\n"
        );
        assert_eq!(amend_message("old", "new"), "new");
    }

    #[test]
    fn classify_rejections_and_auth() {
        let rej =
            " ! [rejected]        main -> main (fetch first)\nerror: failed to push some refs";
        assert_eq!(
            classify_push(rej, Some(2)),
            "push rejected: behind by 2, pull first"
        );
        assert_eq!(
            classify_push(rej, None),
            "push rejected: behind, pull first"
        );
        let auth = "fatal: could not read Username for 'https://x': terminal prompts disabled";
        assert_eq!(
            classify_push(auth, None),
            "push failed: auth, push from a shell"
        );
        assert_eq!(
            classify_pull("fatal: Not possible to fast-forward, aborting."),
            "pull stopped: branches have diverged, merge or rebase from a shell"
        );
        assert!(classify_pull(
            "error: Your local changes to the following files would be overwritten by merge:\n\ta.rs\n"
        )
        .starts_with("pull stopped: local changes would be overwritten · a.rs"));
        assert!(lock_held(
            "fatal: Unable to create '/r/.git/index.lock': File exists."
        ));
    }

    #[test]
    fn pull_names_only_the_first_file() {
        assert_eq!(
            classify_pull(
                "error: Your local changes would be overwritten by merge:\n\ta.rs\n\tb.rs\n"
            ),
            "pull stopped: local changes would be overwritten · a.rs"
        );
    }

    #[test]
    fn remote_rejection_is_not_behind() {
        let m = classify_push(
            " ! [remote rejected] main -> main (protected branch hook declined)\nerror: failed to push some refs",
            Some(1),
        );
        assert!(
            m.starts_with("push failed:") && !m.contains("behind"),
            "{m}"
        );
    }

    #[test]
    fn classify_commit_hooks() {
        assert_eq!(
            classify_commit("hint: pre-commit hook failed"),
            "commit failed: pre-commit hook"
        );
        assert_eq!(
            classify_commit("commit-msg hook returned 1"),
            "commit failed: commit-msg hook"
        );
        assert_eq!(
            classify_commit("\nfatal: nothing odd\n"),
            "commit failed: nothing odd"
        );
    }
}
