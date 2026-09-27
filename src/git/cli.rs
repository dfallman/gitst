//! `GitBackend` implemented by running the `git` command.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::SystemTime;

use super::parse::{self, BRANCH_FORMAT, LOG_FORMAT, REFLOG_FORMAT, STASH_FORMAT};
use super::{GitBackend, GitError, Repo, SnapshotOpts};
use crate::model::{Head, RepoOp, Snapshot, Upstream};

/// Largest untracked file whose lines are counted for the `+N` column.
const MAX_COUNTED_FILE: u64 = 1 << 20;
/// Upper bound on remote-tracking reflogs read per refresh.
const MAX_REMOTE_REFLOGS: usize = 10;

/// A `git` command that can never take optional locks, prompt, or colour
/// its output, whatever the user's configuration says.
pub(crate) fn base_command(cwd: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("--no-optional-locks")
        .arg("-C")
        .arg(cwd)
        .args([
            "-c", "color.ui=false",
            "-c", "core.quotePath=false",
            "-c", "log.showSignature=false",
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null());
    c
}

pub struct CliBackend {
    repo: Repo,
}

impl CliBackend {
    pub fn new(repo: Repo) -> Self {
        CliBackend { repo }
    }

    pub(crate) fn command(&self) -> Command {
        base_command(&self.repo.root)
    }

    /// Runs git and returns stdout; a non-zero exit becomes the first stderr line.
    pub(crate) fn git(&self, args: &[&str]) -> Result<Vec<u8>, GitError> {
        let out = self.command().args(args).output().map_err(|e| GitError(format!("git: {e}")))?;
        if out.status.success() {
            Ok(out.stdout)
        } else {
            let err = String::from_utf8_lossy(&out.stderr);
            let line = err.lines().find(|l| !l.trim().is_empty()).unwrap_or("git failed");
            Err(GitError(line.trim_start_matches("fatal: ").to_string()))
        }
    }

    fn op_state(&self) -> Option<RepoOp> {
        let g = &self.repo.git_dir;
        let read_num = |p: &str| -> Option<u32> { std::fs::read_to_string(g.join(p)).ok()?.trim().parse().ok() };
        if g.join("rebase-merge").is_dir() {
            Some(RepoOp::Rebase { step: read_num("rebase-merge/msgnum"), total: read_num("rebase-merge/end") })
        } else if g.join("rebase-apply").is_dir() {
            Some(RepoOp::Rebase { step: read_num("rebase-apply/next"), total: read_num("rebase-apply/last") })
        } else if g.join("MERGE_HEAD").exists() {
            Some(RepoOp::Merge)
        } else if g.join("CHERRY_PICK_HEAD").exists() {
            Some(RepoOp::CherryPick)
        } else if g.join("REVERT_HEAD").exists() {
            Some(RepoOp::Revert)
        } else if g.join("BISECT_LOG").exists() {
            Some(RepoOp::Bisect)
        } else {
            None
        }
    }

    /// Line count of a small text file, for untracked files' `+N`.
    fn count_lines(&self, rel: &str) -> Option<u32> {
        let path = self.repo.root.join(rel);
        let meta = std::fs::metadata(&path).ok()?;
        if !meta.is_file() || meta.len() > MAX_COUNTED_FILE {
            return None;
        }
        let bytes = std::fs::read(&path).ok()?;
        if bytes.contains(&0) {
            return None;
        }
        let newlines = bytes.iter().filter(|b| **b == b'\n').count();
        let trailing = !bytes.is_empty() && !bytes.ends_with(b"\n");
        Some((newlines + trailing as usize) as u32)
    }
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// Adds two numstat counts; `None` (binary) wins.
fn add_counts(a: Option<u32>, b: Option<u32>) -> Option<u32> {
    Some(a? + b?)
}

impl GitBackend for CliBackend {
    fn repo(&self) -> &Repo {
        &self.repo
    }

    fn snapshot(&self, opts: &SnapshotOpts) -> Result<Snapshot, GitError> {
        let raw = self.git(&[
            "status",
            "--porcelain=v2",
            "-z",
            "--branch",
            "--show-stash",
            "--untracked-files=all",
        ])?;
        let (header, mut changes) = parse::parse_status(&raw);

        let head = match (&header.oid, &header.head) {
            (None, Some(b)) => Head::Unborn(b.clone()),
            (Some(oid), None) => Head::Detached(oid.chars().take(7).collect()),
            (_, Some(b)) => Head::Branch(b.clone()),
            (None, None) => Head::Detached(String::new()),
        };
        let unborn = matches!(head, Head::Unborn(_));

        if changes.len() <= opts.numstat_max_files {
            let mut counts: HashMap<String, (Option<u32>, Option<u32>)> = HashMap::new();
            for args in [&["diff", "--no-ext-diff", "--numstat", "-z"][..], &["diff", "--no-ext-diff", "--cached", "--numstat", "-z"][..]] {
                if let Ok(out) = self.git(args) {
                    for (path, a, r) in parse::parse_numstat(&out) {
                        let e = counts.entry(path).or_insert((Some(0), Some(0)));
                        *e = (add_counts(e.0, a), add_counts(e.1, r));
                    }
                }
            }
            for c in &mut changes {
                if let Some((a, r)) = counts.get(&c.path) {
                    (c.added, c.removed) = (*a, *r);
                } else if c.untracked() {
                    c.added = self.count_lines(&c.path);
                    c.removed = c.added.map(|_| 0);
                }
            }
        }

        changes.sort_by(|a, b| b.conflicted().cmp(&a.conflicted()).then_with(|| a.path.cmp(&b.path)));
        let changes_omitted = changes.len().saturating_sub(opts.max_changes);
        changes.truncate(opts.max_changes);

        let upstream = header.upstream.clone().map(|name| Upstream { name, ahead: header.ahead, behind: header.behind });

        let mut commits = Vec::new();
        if !unborn {
            let n = format!("-n{}", opts.commits);
            let fmt = format!("--format={LOG_FORMAT}");
            commits = parse::parse_log(&self.git(&["log", &n, &fmt, "HEAD"])?);
            if upstream.is_some() {
                if let Ok(out) = self.git(&["rev-list", "-n500", "@{upstream}..HEAD"]) {
                    let ahead: BTreeSet<String> =
                        String::from_utf8_lossy(&out).lines().map(str::to_string).collect();
                    for c in &mut commits {
                        c.unpushed = ahead.contains(&c.oid);
                    }
                }
            }
        }

        let branches = self
            .git(&["for-each-ref", "--sort=-committerdate", &format!("--format={BRANCH_FORMAT}"), "refs/heads"])
            .map(|o| parse::parse_branches(&o))
            .unwrap_or_default();

        let stashes = if header.stash > 0 {
            self.git(&["stash", "list", &format!("--format={STASH_FORMAT}")])
                .map(|o| parse::parse_stashes(&o))
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let reflog_fmt = format!("--format={REFLOG_FORMAT}");
        let mut reflog = Vec::new();
        if !unborn {
            if let Ok(out) = self.git(&["log", "-g", "--date=unix", "-n50", &reflog_fmt, "HEAD"]) {
                reflog.extend(parse::parse_reflog("HEAD", &out));
            }
        }
        let mut upstreams: Vec<&str> = Vec::new();
        if let Some(u) = &upstream {
            upstreams.push(&u.name);
        }
        for b in &branches {
            if let Some(u) = &b.upstream {
                if !upstreams.contains(&u.as_str()) {
                    upstreams.push(u);
                }
            }
        }
        for u in upstreams.into_iter().take(MAX_REMOTE_REFLOGS) {
            let refname = format!("refs/remotes/{u}");
            if let Ok(out) = self.git(&["log", "-g", "--date=unix", "-n20", &reflog_fmt, &refname]) {
                reflog.extend(parse::parse_reflog(&refname, &out));
            }
        }

        let tag = if unborn {
            None
        } else {
            self.git(&["describe", "--tags", "--long"])
                .ok()
                .and_then(|o| parse::parse_describe(&String::from_utf8_lossy(&o)))
        };

        let has_remote = self.git(&["remote"]).map(|o| !o.trim_ascii().is_empty()).unwrap_or(false);
        let index_lock_age =
            mtime(&self.repo.git_dir.join("index.lock")).map(|t| t.elapsed().unwrap_or_default());

        Ok(Snapshot {
            head,
            oid: header.oid,
            upstream,
            op: self.op_state(),
            changes,
            changes_omitted,
            commits,
            branches,
            stashes,
            reflog,
            tag,
            stash_count: header.stash,
            index_lock_age,
            last_fetch: mtime(&self.repo.common_dir.join("FETCH_HEAD")),
            has_remote,
        })
    }
}
