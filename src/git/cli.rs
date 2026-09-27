//! `GitBackend` implemented by running the `git` command.

use std::collections::{BTreeSet, HashMap};
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use super::parse::{self, BRANCH_FORMAT, LOG_FORMAT, REFLOG_FORMAT, STASH_FORMAT};
use super::{FetchError, GitBackend, GitError, Repo, SnapshotOpts, classify_fetch_stderr};
use crate::model::{
    CommitDetail, DetailData, DetailReq, DiffBlock, DiffKind, DiffLine, Head, RepoOp, Snapshot,
    Upstream,
};

/// Largest untracked file whose lines are counted for the `+N` column.
const MAX_COUNTED_FILE: u64 = 1 << 20;
/// How long a timed-out fetch gets to exit after SIGTERM.
const KILL_GRACE: Duration = Duration::from_secs(2);
/// Upper bound on remote-tracking reflogs read per refresh.
const MAX_REMOTE_REFLOGS: usize = 10;

/// A `git` command that can never take optional locks, prompt, or colour
/// its output, whatever the user's configuration says.
pub(crate) fn base_command(cwd: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("--no-optional-locks")
        .arg("--literal-pathspecs")
        .arg("-C")
        .arg(cwd)
        .args([
            "-c",
            "color.ui=false",
            "-c",
            "core.quotePath=false",
            "-c",
            "log.showSignature=false",
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
        let out = self
            .command()
            .args(args)
            .output()
            .map_err(|e| GitError(format!("git: {e}")))?;
        if out.status.success() {
            Ok(out.stdout)
        } else {
            let err = String::from_utf8_lossy(&out.stderr);
            let line = err
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("git failed");
            Err(GitError(line.trim_start_matches("fatal: ").to_string()))
        }
    }

    fn op_state(&self) -> Option<RepoOp> {
        let g = &self.repo.git_dir;
        let read_num = |p: &str| -> Option<u32> {
            std::fs::read_to_string(g.join(p)).ok()?.trim().parse().ok()
        };
        if g.join("rebase-merge").is_dir() {
            Some(RepoOp::Rebase {
                step: read_num("rebase-merge/msgnum"),
                total: read_num("rebase-merge/end"),
            })
        } else if g.join("rebase-apply").is_dir() {
            Some(RepoOp::Rebase {
                step: read_num("rebase-apply/next"),
                total: read_num("rebase-apply/last"),
            })
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

    /// Staged and unstaged diffs of one path, or its contents when untracked.
    fn file_detail(&self, path: &str) -> Result<Vec<DiffBlock>, GitError> {
        let staged = self.git(&["diff", "--no-ext-diff", "--cached", "--", path])?;
        let unstaged = self.git(&["diff", "--no-ext-diff", "--", path])?;
        let mut blocks = nonempty_block("Staged".into(), parse::parse_diff(&staged));
        blocks.extend(nonempty_block(
            "Unstaged".into(),
            parse::parse_diff(&unstaged),
        ));
        let size = std::fs::metadata(self.repo.root.join(path))
            .map(|m| m.len())
            .ok();
        for line in blocks.iter_mut().flat_map(|b| b.lines.iter_mut()) {
            if line.kind == DiffKind::Meta && line.text.starts_with("Binary files") {
                line.text = match size {
                    Some(s) => format!("binary file changed · {}", human_size(s)),
                    None => "binary file changed".into(),
                };
            }
        }
        if blocks.is_empty() && self.git(&["ls-files", "-z", "--", path])?.is_empty() {
            blocks = nonempty_block("Untracked".into(), self.untracked_lines(path));
        }
        Ok(blocks)
    }

    fn untracked_lines(&self, rel: &str) -> Vec<DiffLine> {
        let path = self.repo.root.join(rel);
        let Ok(meta) = std::fs::metadata(&path) else {
            return Vec::new();
        };
        let meta_line = |text: String| {
            vec![DiffLine {
                kind: DiffKind::Meta,
                text,
            }]
        };
        if meta.is_dir() {
            return meta_line("directory".into());
        }
        if meta.len() > MAX_COUNTED_FILE {
            return meta_line(format!("large file · {}", human_size(meta.len())));
        }
        let bytes = std::fs::read(&path).unwrap_or_default();
        match String::from_utf8(bytes) {
            Ok(text) if !text.contains('\0') => text
                .lines()
                .map(|l| DiffLine {
                    kind: DiffKind::Add,
                    text: l.replace('\t', "    "),
                })
                .collect(),
            _ => meta_line(format!("binary · {}", human_size(meta.len()))),
        }
    }

    /// `git fetch` as run in the background.
    pub(crate) fn fetch_command(&self, prune: bool) -> Command {
        let mut cmd = self.command();
        // No askpass helper, GUI credential manager or ssh passphrase dialog
        // may pop up for a fetch the user did not start.
        cmd.args(["-c", "credential.interactive=false", "fetch", "--quiet"])
            .env("GIT_ASKPASS", "")
            .env("SSH_ASKPASS", "")
            .env("SSH_ASKPASS_REQUIRE", "never")
            .env("GCM_INTERACTIVE", "never");
        if prune {
            cmd.arg("--prune");
        }
        cmd.stdout(Stdio::null()).stderr(Stdio::piped());
        cmd
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

fn nonempty_block(title: String, lines: Vec<DiffLine>) -> Vec<DiffBlock> {
    if lines.is_empty() {
        Vec::new()
    } else {
        vec![DiffBlock { title, lines }]
    }
}

pub(crate) fn human_size(bytes: u64) -> String {
    match bytes {
        b if b < 1024 => format!("{b} B"),
        b if b < 1024 * 1024 => format!("{} KB", b.div_ceil(1024)),
        b => format!("{:.1} MB", b as f64 / (1024.0 * 1024.0)),
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
            for args in [
                &["diff", "--no-ext-diff", "--numstat", "-z"][..],
                &["diff", "--no-ext-diff", "--cached", "--numstat", "-z"][..],
            ] {
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

        changes.sort_by(|a, b| {
            b.conflicted()
                .cmp(&a.conflicted())
                .then_with(|| a.path.cmp(&b.path))
        });
        let changes_omitted = changes.len().saturating_sub(opts.max_changes);
        changes.truncate(opts.max_changes);

        let upstream = header.upstream.clone().map(|name| Upstream {
            name,
            ahead: header.ahead,
            behind: header.behind,
        });

        let mut commits = Vec::new();
        if !unborn {
            let n = format!("-n{}", opts.commits);
            let fmt = format!("--format={LOG_FORMAT}");
            commits = parse::parse_log(&self.git(&["log", &n, &fmt, "HEAD"])?);
            if upstream.is_some()
                && let Ok(out) = self.git(&["rev-list", "-n500", "@{upstream}..HEAD"])
            {
                let ahead: BTreeSet<String> = String::from_utf8_lossy(&out)
                    .lines()
                    .map(str::to_string)
                    .collect();
                for c in &mut commits {
                    c.unpushed = ahead.contains(&c.oid);
                }
            }
        }

        let branches = self
            .git(&[
                "for-each-ref",
                "--sort=-committerdate",
                &format!("--format={BRANCH_FORMAT}"),
                "refs/heads",
            ])
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
        if !unborn
            && let Ok(out) = self.git(&["log", "-g", "--date=unix", "-n50", &reflog_fmt, "HEAD"])
        {
            reflog.extend(parse::parse_reflog("HEAD", &out));
        }
        let mut upstreams: Vec<&str> = Vec::new();
        if let Some(u) = &upstream {
            upstreams.push(&u.name);
        }
        for b in &branches {
            if let Some(u) = &b.upstream
                && !upstreams.contains(&u.as_str())
            {
                upstreams.push(u);
            }
        }
        for u in upstreams.into_iter().take(MAX_REMOTE_REFLOGS) {
            let refname = format!("refs/remotes/{u}");
            if let Ok(out) = self.git(&["log", "-g", "--date=unix", "-n20", &reflog_fmt, &refname])
            {
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

        let has_remote = self
            .git(&["remote"])
            .map(|o| !o.trim_ascii().is_empty())
            .unwrap_or(false);
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

    fn detail(&self, req: &DetailReq) -> Result<DetailData, GitError> {
        match req {
            DetailReq::File { path } => self.file_detail(path).map(DetailData::File),
            DetailReq::Commit { rev } => {
                let meta =
                    self.git(&["show", "-s", "--format=%H%x1f%an%x1f%at%x1f%B", rev, "--"])?;
                let meta = String::from_utf8_lossy(&meta);
                let mut f = meta.splitn(4, '\x1f');
                let (oid, author, time, message) = (f.next(), f.next(), f.next(), f.next());
                let stat = self.git(&[
                    "show",
                    "--format=",
                    "--numstat",
                    "-z",
                    "--diff-merges=first-parent",
                    rev,
                    "--",
                ])?;
                Ok(DetailData::Commit(CommitDetail {
                    oid: oid.unwrap_or_default().to_string(),
                    author: author.unwrap_or_default().to_string(),
                    time: time.and_then(|t| t.parse().ok()).unwrap_or(0),
                    message: message.unwrap_or_default().trim_end().to_string(),
                    files: parse::parse_numstat(&stat),
                }))
            }
            DetailReq::CommitFile { rev, path } => {
                let out = self.git(&[
                    "show",
                    "--format=",
                    "--no-ext-diff",
                    "--diff-merges=first-parent",
                    rev,
                    "--",
                    path,
                ])?;
                let title: String = rev.chars().take(12).collect();
                Ok(DetailData::CommitFile(nonempty_block(
                    title,
                    parse::parse_diff(&out),
                )))
            }
            DetailReq::Branch { name, upstream } => {
                let fmt = format!("--format={LOG_FORMAT}");
                let ahead =
                    self.git(&["log", "-n50", &fmt, &format!("{upstream}..{name}"), "--"])?;
                let behind =
                    self.git(&["log", "-n50", &fmt, &format!("{name}..{upstream}"), "--"])?;
                Ok(DetailData::Branch {
                    ahead: parse::parse_log(&ahead),
                    behind: parse::parse_log(&behind),
                })
            }
        }
    }

    fn fetch(&self, prune: bool, timeout: Duration) -> Result<(), FetchError> {
        let mut cmd = self.fetch_command(prune);
        // A new session has no controlling terminal, so neither git nor ssh
        // can ever prompt over the TUI; it also lets us kill the whole group.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut child = cmd.spawn().map_err(|e| FetchError::Other(e.to_string()))?;
        let mut stderr = child.stderr.take().expect("stderr is piped");
        let reader = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = stderr.read_to_string(&mut s);
            s
        });
        let start = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let err = reader.join().unwrap_or_default();
                    return if status.success() {
                        Ok(())
                    } else {
                        Err(classify_fetch_stderr(&err))
                    };
                }
                Ok(None) if start.elapsed() >= timeout => {
                    // SIGTERM first: git removes its ref lock files on
                    // SIGTERM but not on SIGKILL, and a leftover lock would
                    // break the user's own next fetch.
                    let group = -(child.id() as i32);
                    unsafe {
                        libc::kill(group, libc::SIGTERM);
                    }
                    let grace = Instant::now();
                    while grace.elapsed() < KILL_GRACE && matches!(child.try_wait(), Ok(None)) {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    unsafe {
                        libc::kill(group, libc::SIGKILL);
                    }
                    let _ = child.wait();
                    return Err(FetchError::Timeout);
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => return Err(FetchError::Other(e.to_string())),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;

    fn backend() -> CliBackend {
        let p = std::path::PathBuf::from("/tmp");
        CliBackend::new(Repo {
            root: p.clone(),
            git_dir: p.clone(),
            common_dir: p,
        })
    }

    #[test]
    fn fetch_disables_every_prompt() {
        let cmd = backend().fetch_command(false);
        let envs: HashMap<&OsStr, Option<&OsStr>> = cmd.get_envs().collect();
        let env = |k: &str| {
            envs.get(OsStr::new(k))
                .copied()
                .flatten()
                .and_then(OsStr::to_str)
        };
        assert_eq!(env("GIT_TERMINAL_PROMPT"), Some("0"));
        assert_eq!(
            env("GIT_ASKPASS"),
            Some(""),
            "empty disables core.askPass and SSH_ASKPASS in git"
        );
        assert_eq!(env("SSH_ASKPASS"), Some(""));
        assert_eq!(env("SSH_ASKPASS_REQUIRE"), Some("never"));
        assert_eq!(env("GCM_INTERACTIVE"), Some("never"));
        let args: Vec<_> = cmd.get_args().filter_map(OsStr::to_str).collect();
        assert!(args.contains(&"credential.interactive=false"), "{args:?}");
    }
}
