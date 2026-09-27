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
    Rebase { step: Option<u32>, total: Option<u32> },
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
    pub added: Option<u32>,
    pub removed: Option<u32>,
}

impl Change {
    pub fn conflicted(&self) -> bool {
        matches!(
            (self.x, self.y),
            ('D', 'D') | ('A', 'U') | ('U', 'D') | ('U', 'A') | ('D', 'U') | ('A', 'A') | ('U', 'U')
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
