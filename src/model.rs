//! Plain data describing a repository at one point in time.

use std::time::{Duration, SystemTime};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Head {
    Branch(String),
    /// Short object id of the detached commit.
    Detached(String),
    /// Branch name of a repository without commits.
    Unborn(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upstream {
    pub name: String,
    pub ahead: u32,
    pub behind: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepoOp {
    Merge,
    Rebase {
        step: Option<u32>,
        total: Option<u32>,
    },
    CherryPick,
    Revert,
    Bisect,
}

/// One path in `git status`. `x` is the index column, `y` the worktree
/// column, both with git's `.` normalised to a space.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub path: String,
    pub orig_path: Option<String>,
    pub x: char,
    pub y: char,
    pub counts: Counts,
}

/// Lines added and removed by a change, as far as they are known.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Counts {
    Lines {
        added: u32,
        removed: u32,
    },
    /// Numstat's `-`, or an untracked file with NUL bytes.
    Binary,
    /// Not counted: more changes than `numstat_max_files`, an untracked
    /// directory or large file, or git failed.
    #[default]
    Unknown,
}

impl Counts {
    pub fn lines(added: u32, removed: u32) -> Counts {
        Counts::Lines { added, removed }
    }

    /// `(added, removed)` when the lines were counted.
    pub fn known(self) -> Option<(u32, u32)> {
        match self {
            Counts::Lines { added, removed } => Some((added, removed)),
            _ => None,
        }
    }

    /// Counts for one path from two diffs (staged and unstaged). Unknown
    /// wins over binary, and binary over lines.
    pub fn plus(self, other: Counts) -> Counts {
        match (self, other) {
            (Counts::Unknown, _) | (_, Counts::Unknown) => Counts::Unknown,
            (Counts::Binary, _) | (_, Counts::Binary) => Counts::Binary,
            (
                Counts::Lines {
                    added: a,
                    removed: r,
                },
                Counts::Lines {
                    added: b,
                    removed: s,
                },
            ) => Counts::lines(a.saturating_add(b), r.saturating_add(s)),
        }
    }
}

impl Change {
    pub fn conflicted(&self) -> bool {
        matches!(
            (self.x, self.y),
            ('D', 'D')
                | ('A', 'U')
                | ('U', 'D')
                | ('U', 'A')
                | ('D', 'U')
                | ('A', 'A')
                | ('U', 'U')
        )
    }

    pub fn untracked(&self) -> bool {
        self.x == '?'
    }

    pub fn staged(&self) -> bool {
        !self.untracked() && !self.conflicted() && self.x != ' '
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub oid: String,
    pub short: String,
    pub parents: usize,
    pub time: i64,
    pub author: String,
    pub refs: Vec<String>,
    pub subject: String,
    pub unpushed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Branch {
    pub name: String,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub gone: bool,
    pub time: i64,
    pub is_head: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stash {
    pub index: usize,
    pub time: i64,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReflogEntry {
    /// `HEAD` or a full ref such as `refs/remotes/origin/main`.
    pub refname: String,
    pub short: String,
    pub time: i64,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TagInfo {
    pub name: String,
    pub distance: u32,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub head: Head,
    pub oid: Option<String>,
    pub upstream: Option<Upstream>,
    pub op: Option<RepoOp>,
    pub changes: Vec<Change>,
    pub changes_omitted: usize,
    pub commits: Vec<Commit>,
    pub branches: Vec<Branch>,
    pub stashes: Vec<Stash>,
    pub reflog: Vec<ReflogEntry>,
    pub tag: Option<TagInfo>,
    pub stash_count: usize,
    pub index_lock_age: Option<Duration>,
    pub last_fetch: Option<SystemTime>,
    pub has_remote: bool,
}

impl Default for Snapshot {
    fn default() -> Self {
        Snapshot {
            head: Head::Unborn("main".into()),
            oid: None,
            upstream: None,
            op: None,
            changes: Vec::new(),
            changes_omitted: 0,
            commits: Vec::new(),
            branches: Vec::new(),
            stashes: Vec::new(),
            reflog: Vec::new(),
            tag: None,
            stash_count: 0,
            index_lock_age: None,
            last_fetch: None,
            has_remote: false,
        }
    }
}

impl Snapshot {
    pub fn is_dirty(&self) -> bool {
        !self.changes.is_empty()
    }
}

/// Something the UI can ask the worker to load for a detail view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DetailReq {
    /// Staged, unstaged or untracked contents of a working-tree path.
    File {
        path: String,
    },
    Commit {
        rev: String,
    },
    CommitFile {
        rev: String,
        path: String,
    },
    Branch {
        name: String,
        upstream: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffKind {
    Add,
    Del,
    Context,
    Hunk,
    Meta,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffKind,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffBlock {
    pub title: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitDetail {
    pub oid: String,
    pub author: String,
    pub time: i64,
    pub message: String,
    pub files: Vec<(String, Counts)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DetailData {
    File(Vec<DiffBlock>),
    Commit(CommitDetail),
    CommitFile(Vec<DiffBlock>),
    /// The newest commits on each side (at most 50), and how many there
    /// are in all.
    Branch {
        ahead: Vec<Commit>,
        behind: Vec<Commit>,
        ahead_total: usize,
        behind_total: usize,
    },
}
