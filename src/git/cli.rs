//! `GitBackend` implemented by running the `git` command.

use std::collections::{BTreeSet, HashMap};
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use super::leakscan::{LeakScanConfig, LeakScanner, MAX_SCANNED_COMMITS};
use super::parse::{
    self, BRANCH_FORMAT, LOG_FORMAT, REFLOG_FORMAT, REPO_CONFIG_KEYS, STASH_FORMAT,
};
use super::{FetchError, GitBackend, GitError, Repo, SnapshotOpts, classify_fetch_stderr};
use crate::model::{
    CommitDetail, Counts, DetailData, DetailReq, DiffBlock, DiffKind, DiffLine, Head, RepoOp,
    Snapshot, Upstream,
};

/// Largest untracked file whose lines are counted for the `+N` column.
const MAX_COUNTED_FILE: u64 = 1 << 20;
/// How long a stopped git command gets to clean up and exit before it is
/// killed.
const KILL_GRACE: Duration = Duration::from_secs(2);
/// Longest any git command other than fetch may run. Generous, since a cold
/// `git status` in a very large repository can take many seconds.
const GIT_TIMEOUT: Duration = Duration::from_secs(30);
/// Commits listed on each side of a branch detail.
const BRANCH_LOG: usize = 50;
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
            "color.diff=false",
            "-c",
            "diff.suppressBlankEmpty=false",
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
    timeout: Duration,
    leaks: LeakScanner,
}

impl CliBackend {
    pub fn new(repo: Repo) -> Self {
        CliBackend {
            repo,
            timeout: GIT_TIMEOUT,
            leaks: LeakScanner::new(LeakScanConfig::default()),
        }
    }

    /// Sets how long git commands other than fetch may run.
    pub fn with_timeout(self, timeout: Duration) -> Self {
        CliBackend { timeout, ..self }
    }

    /// Sets whether and where to look for possible secrets.
    pub fn with_leak_scan(self, cfg: LeakScanConfig) -> Self {
        CliBackend {
            leaks: LeakScanner::new(cfg),
            ..self
        }
    }

    pub(crate) fn command(&self) -> Command {
        base_command(&self.repo.root)
    }

    /// Runs git and returns stdout; a non-zero exit becomes the first stderr
    /// line. A command still running after the timeout is stopped, so one
    /// wedged call cannot hold up every later snapshot and detail.
    pub(crate) fn git(&self, args: &[&str]) -> Result<Vec<u8>, GitError> {
        let mut cmd = self.command();
        cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // Its own group, so a stop reaches hooks and helpers too.
            cmd.process_group(0);
        }
        let (status, stdout, stderr) = match run_bounded(&mut cmd, self.timeout, None) {
            Ok(Run::Exited {
                status,
                stdout,
                stderr,
            }) => (status, stdout, stderr),
            Ok(_) => {
                let name = args.first().copied().unwrap_or_default();
                return Err(GitError(format!("git {name} timed out")));
            }
            Err(e) => return Err(GitError(format!("git: {e}"))),
        };
        if status.success() {
            Ok(stdout)
        } else {
            let err = String::from_utf8_lossy(&stderr);
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
        let staged = self.git(&[
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--cached",
            "--",
            path,
        ])?;
        let unstaged = self.git(&["diff", "--no-color", "--no-ext-diff", "--", path])?;
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
        // Links, directories and special files are described, not read: a
        // FIFO or a link to /dev/zero would block or never end.
        // `read_regular` checks again on the file it opens.
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            return Vec::new();
        };
        let meta_line = |text: String| {
            vec![DiffLine {
                kind: DiffKind::Meta,
                text,
            }]
        };
        if meta.is_symlink() {
            let target = std::fs::read_link(&path)
                .map(|t| t.display().to_string())
                .unwrap_or_default();
            return meta_line(format!("symlink → {target}"));
        }
        if meta.is_dir() {
            return meta_line("directory".into());
        }
        if !meta.is_file() {
            return meta_line("not a regular file".into());
        }
        let bytes = match read_regular(&path, MAX_COUNTED_FILE) {
            FileRead::Contents(bytes) => bytes,
            FileRead::TooLarge(size) => {
                return meta_line(format!("large file · {}", human_size(size)));
            }
            // It changed after the check above.
            FileRead::NotRegular => return meta_line("not a regular file".into()),
        };
        let size = bytes.len() as u64;
        match String::from_utf8(bytes) {
            Ok(text) if !text.contains('\0') => text
                .lines()
                .map(|l| DiffLine {
                    kind: DiffKind::Add,
                    text: l.replace('\t', "    "),
                })
                .collect(),
            _ => meta_line(format!("binary · {}", human_size(size))),
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

    /// An untracked file's `+N` count, and, with `text`, its contents for
    /// the leak scan when it is small and not binary.
    fn read_untracked(&self, rel: &str, text: bool) -> (Counts, Option<String>) {
        let FileRead::Contents(bytes) = read_regular(&self.repo.root.join(rel), MAX_COUNTED_FILE)
        else {
            return (Counts::Unknown, None);
        };
        if bytes.contains(&0) {
            return (Counts::Binary, None);
        }
        let newlines = bytes.iter().filter(|b| **b == b'\n').count();
        let trailing = !bytes.is_empty() && !bytes.ends_with(b"\n");
        let counts = Counts::lines((newlines + trailing as usize) as u32, 0);
        let text = text.then(|| String::from_utf8_lossy(&bytes).into_owned());
        (counts, text)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum FileRead {
    Contents(Vec<u8>),
    /// Over the cap; the size seen, which may be the cap plus one if the
    /// file grew while being read.
    TooLarge(u64),
    /// Missing, unreadable, a directory, a link or a special file.
    NotRegular,
}

/// Reads a regular file of at most `cap` bytes without following a link or
/// waiting on a FIFO or device. Every check is made on the opened file, so
/// swapping `path` for a link or FIFO after an earlier look cannot block
/// the caller.
fn read_regular(path: &Path, cap: u64) -> FileRead {
    let Ok(file) = open_no_follow(path) else {
        return FileRead::NotRegular;
    };
    match file.metadata() {
        Ok(m) if !m.is_file() => return FileRead::NotRegular,
        Ok(m) if m.len() > cap => return FileRead::TooLarge(m.len()),
        Ok(_) => {}
        Err(_) => return FileRead::NotRegular,
    }
    let mut bytes = Vec::new();
    match file.take(cap + 1).read_to_end(&mut bytes) {
        Ok(n) if n as u64 > cap => FileRead::TooLarge(n as u64),
        Ok(_) => FileRead::Contents(bytes),
        Err(_) => FileRead::NotRegular,
    }
}

#[cfg(unix)]
fn open_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    // Without O_NONBLOCK, opening a FIFO waits for a writer.
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
}

#[cfg(windows)]
fn open_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    // Opens a link itself, which then does not pass as a regular file.
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

fn nonempty_block(title: String, lines: Vec<DiffLine>) -> Vec<DiffBlock> {
    if lines.is_empty() {
        Vec::new()
    } else {
        vec![DiffBlock { title, lines }]
    }
}

pub(crate) fn human_size(bytes: u64) -> String {
    const KB: u64 = 1 << 10;
    const MB: u64 = 1 << 20;
    const GB: u64 = 1 << 30;
    const TB: u64 = 1 << 40;
    let scaled = |unit: u64, name: &str| format!("{:.1} {name}", bytes as f64 / unit as f64);
    match bytes {
        b if b < KB => format!("{b} B"),
        b if b < MB => format!("{} KB", b.div_ceil(KB)),
        b if b < GB => scaled(MB, "MB"),
        b if b < TB => scaled(GB, "GB"),
        _ => scaled(TB, "TB"),
    }
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

impl GitBackend for CliBackend {
    fn repo(&self) -> &Repo {
        &self.repo
    }

    fn snapshot(&self, opts: &SnapshotOpts) -> Result<Snapshot, GitError> {
        // Exits 1 when no key is set.
        let config = parse::parse_repo_config(
            &self
                .git(&["config", "-z", "--get-regexp", REPO_CONFIG_KEYS])
                .unwrap_or_default(),
        );
        let raw = self.git(&[
            "status",
            "--porcelain=v2",
            "-z",
            "--branch",
            "--show-stash",
            config.untracked_mode,
        ])?;
        let (header, mut changes) = parse::parse_status(&raw);

        let head = match (&header.oid, &header.head) {
            (None, Some(b)) => Head::Unborn(b.clone()),
            (Some(oid), None) => Head::Detached(oid.chars().take(7).collect()),
            (_, Some(b)) => Head::Branch(b.clone()),
            (None, None) => Head::Detached(String::new()),
        };
        let unborn = matches!(head, Head::Unborn(_));

        let leak_on = self.leaks.enabled();
        let git = |args: &[&str]| self.git(args);
        let mut leaks = Vec::new();
        // Paths left out stay `Counts::Unknown`, which shows as blank.
        let counted = changes.len() <= opts.numstat_max_files;
        if counted {
            let unstaged = self.git(&["diff", "--no-ext-diff", "--numstat", "-z"]);
            let staged = self.git(&["diff", "--no-ext-diff", "--cached", "--numstat", "-z"]);
            // Half a count would be wrong, so both diffs or neither.
            let mut counts: HashMap<String, Counts> = HashMap::new();
            if let (Ok(unstaged), Ok(staged)) = (unstaged, staged) {
                for (path, n) in parse::parse_numstat(&unstaged)
                    .into_iter()
                    .chain(parse::parse_numstat(&staged))
                {
                    let e = counts.entry(path).or_insert(Counts::lines(0, 0));
                    *e = e.plus(n);
                }
            }
            for c in &mut changes {
                if let Some(n) = counts.get(&c.path) {
                    c.counts = *n;
                } else if c.untracked() {
                    let (n, text) = self.read_untracked(&c.path, leak_on);
                    c.counts = n;
                    if let Some(text) = text {
                        leaks.extend(self.leaks.untracked_file(&c.path, &text));
                    }
                }
            }
        }
        // Past the numstat limit only file names are checked.
        let leak_scan_skipped = (leak_on && !counted).then_some(changes.len());
        if leak_on {
            leaks.extend(self.leaks.worktree(&git, &changes, counted));
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
        // Commits on no remote, newest first; `None` if git could not say.
        let mut unpushed: Option<Vec<String>> = Some(Vec::new());
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
            // Not `@{upstream}..HEAD`: a commit pushed to another branch, or
            // merged in from one, is already out, whatever the upstream says.
            if leak_on && config.has_remote {
                unpushed = self
                    .git(&[
                        "rev-list",
                        &format!("-n{MAX_SCANNED_COMMITS}"),
                        "HEAD",
                        "--not",
                        "--remotes",
                    ])
                    .ok()
                    .map(|out| {
                        String::from_utf8_lossy(&out)
                            .lines()
                            .map(str::to_string)
                            .collect()
                    });
            }
        }
        if leak_on {
            leaks.extend(self.leaks.commits(&git, unpushed.as_deref()));
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
            // FETCH_HEAD is per worktree, like HEAD.
            last_fetch: mtime(&self.repo.git_dir.join("FETCH_HEAD")),
            has_remote: config.has_remote,
            leaks,
            leak_scan_skipped,
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
                    "--no-color",
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
                let n = format!("-n{BRANCH_LOG}");
                let ahead = self.git(&["log", &n, &fmt, &format!("{upstream}..{name}"), "--"])?;
                let behind = self.git(&["log", &n, &fmt, &format!("{name}..{upstream}"), "--"])?;
                let (ahead, behind) = (parse::parse_log(&ahead), parse::parse_log(&behind));
                // Only a list that hit the limit needs counting.
                let (mut ahead_total, mut behind_total) = (ahead.len(), behind.len());
                if ahead_total >= BRANCH_LOG || behind_total >= BRANCH_LOG {
                    let counts = self.git(&[
                        "rev-list",
                        "--left-right",
                        "--count",
                        &format!("{name}...{upstream}"),
                        "--",
                    ])?;
                    let counts = String::from_utf8_lossy(&counts);
                    let mut n = counts.split_whitespace().map(|n| n.parse().ok());
                    ahead_total = n.next().flatten().unwrap_or(ahead_total);
                    behind_total = n.next().flatten().unwrap_or(behind_total);
                }
                Ok(DetailData::Branch {
                    ahead,
                    behind,
                    ahead_total,
                    behind_total,
                })
            }
            DetailReq::Leaks => Err(GitError("possible secrets come with the snapshot".into())),
        }
    }

    fn fetch(&self, prune: bool, timeout: Duration, cancel: &AtomicBool) -> Result<(), FetchError> {
        if cancel.load(Ordering::SeqCst) {
            return Err(FetchError::Cancelled);
        }
        let mut cmd = self.fetch_command(prune);
        detach(&mut cmd);
        match run_bounded(&mut cmd, timeout, Some(cancel)) {
            Ok(Run::Exited { status, .. }) if status.success() => Ok(()),
            Ok(Run::Exited { stderr, .. }) => {
                Err(classify_fetch_stderr(&String::from_utf8_lossy(&stderr)))
            }
            Ok(Run::TimedOut) => Err(FetchError::Timeout),
            Ok(Run::Cancelled) => Err(FetchError::Cancelled),
            Err(e) => Err(FetchError::Other(e.to_string())),
        }
    }
}

/// How a command run by `run_bounded` ended.
enum Run {
    Exited {
        status: ExitStatus,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    TimedOut,
    Cancelled,
}

/// Runs `cmd`, collecting whichever of stdout and stderr are piped, and
/// stops it and everything it started after `timeout` or once `cancel` is
/// set. On Unix `cmd` must start a process group (see `ProcessTree`).
fn run_bounded(
    cmd: &mut Command,
    timeout: Duration,
    cancel: Option<&AtomicBool>,
) -> std::io::Result<Run> {
    /// How often a cancel is noticed while the command runs.
    const TICK: Duration = Duration::from_millis(50);
    let mut child = cmd.spawn()?;
    let tree = ProcessTree::adopt(&child);
    let (closed_tx, closed) = mpsc::channel();
    let stdout = child.stdout.take().map(|p| drain(p, closed_tx.clone()));
    let stderr = child.stderr.take().map(|p| drain(p, closed_tx.clone()));
    drop(closed_tx);
    let mut open = usize::from(stdout.is_some()) + usize::from(stderr.is_some());
    let deadline = Instant::now() + timeout;
    let mut nap = Duration::from_millis(1);
    loop {
        // Pipes close when the command exits, so waiting on them wakes at
        // once; polling for the exit only starts after that.
        if open == 0
            && let Some(status) = child.try_wait()?
        {
            let join =
                |h: Option<JoinHandle<Vec<u8>>>| h.and_then(|h| h.join().ok()).unwrap_or_default();
            return Ok(Run::Exited {
                status,
                stdout: join(stdout),
                stderr: join(stderr),
            });
        }
        let stop = if cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
            Some(Run::Cancelled)
        } else if Instant::now() >= deadline {
            Some(Run::TimedOut)
        } else {
            None
        };
        if let Some(run) = stop {
            tree.terminate(&mut child);
            let _ = child.wait();
            return Ok(run);
        }
        if open > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            match closed.recv_timeout(left.min(TICK)) {
                Ok(()) => open -= 1,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => open = 0,
            }
        } else {
            std::thread::sleep(nap);
            nap = (nap * 2).min(TICK);
        }
    }
}

/// Reads a pipe to its end on its own thread, so neither pipe can fill up
/// and stall the child, then reports the pipe closed.
fn drain(mut pipe: impl Read + Send + 'static, closed: Sender<()>) -> JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        let _ = closed.send(());
        buf
    })
}

/// Cuts a fetch off from the terminal so neither git nor ssh can ever prompt
/// over the TUI.
#[cfg(unix)]
fn detach(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // A new session has no controlling terminal; it also makes git a group
    // leader, so `ProcessTree::terminate` can signal everything it starts.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
}

#[cfg(windows)]
fn detach(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // Without a console there is nothing for git or ssh to prompt on.
    cmd.creation_flags(CREATE_NO_WINDOW);
}

/// A git command and every process it starts: its process group, which
/// the command must lead.
#[cfg(unix)]
struct ProcessTree;

#[cfg(unix)]
impl ProcessTree {
    fn adopt(_child: &Child) -> Self {
        ProcessTree
    }

    /// Stops a timed-out or cancelled command and everything it started.
    fn terminate(&self, child: &mut Child) {
        // SIGTERM first: git removes its ref lock files on SIGTERM but not on
        // SIGKILL, and a leftover lock would break the user's own next fetch.
        let group = -(child.id() as i32);
        unsafe {
            libc::kill(group, libc::SIGTERM);
        }
        wait_grace(child);
        unsafe {
            libc::kill(group, libc::SIGKILL);
        }
    }
}

/// A git command and every process it starts, held in a job object so that
/// a stop reaches ssh, hooks and helpers as well as git.
#[cfg(windows)]
struct ProcessTree {
    job: Option<std::os::windows::io::OwnedHandle>,
}

#[cfg(windows)]
impl ProcessTree {
    fn adopt(child: &Child) -> Self {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW};
        // Anything the child starts before this is outside the job. That
        // window is short (git has barely begun loading), but not closed:
        // std cannot start a process suspended or already in a job.
        let job = unsafe {
            let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if h.is_null() {
                return ProcessTree { job: None };
            }
            let job = OwnedHandle::from_raw_handle(h);
            (AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) != 0)
                .then_some(job)
        };
        ProcessTree { job }
    }

    /// Stops a timed-out or cancelled command and everything it started.
    fn terminate(&self, child: &mut Child) {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        // Git removes its ref lock files in atexit handlers, which
        // TerminateProcess skips, and a leftover lock would break the user's
        // own next fetch. So every process is first asked to exit from
        // inside: on a default install the `git.exe` on PATH is a launcher,
        // and the git doing the work is its child.
        let Some(job) = &self.job else {
            if exit_from_inside(child.as_raw_handle()) {
                wait_grace(child);
            }
            let _ = child.kill();
            return;
        };
        let job = job.as_raw_handle();
        let mut asked = false;
        for pid in job_processes(job) {
            asked |= exit_pid_from_inside(pid);
        }
        if asked {
            let grace = Instant::now();
            while grace.elapsed() < KILL_GRACE && !job_processes(job).is_empty() {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        unsafe {
            TerminateJobObject(job, STOPPED_EXIT_CODE);
        }
    }
}

/// Exit status for a stopped command on Windows, as if by SIGTERM.
#[cfg(windows)]
const STOPPED_EXIT_CODE: u32 = 128 + 15;

/// Ids of the processes still running in `job` (the first 64).
#[cfg(windows)]
fn job_processes(job: std::os::windows::io::RawHandle) -> Vec<u32> {
    use windows_sys::Win32::System::JobObjects::{
        JobObjectBasicProcessIdList, QueryInformationJobObject,
    };
    #[repr(C)]
    struct IdList {
        assigned: u32,
        listed: u32,
        ids: [usize; 64],
    }
    let mut list = IdList {
        assigned: 0,
        listed: 0,
        ids: [0; 64],
    };
    // With more than 64 processes the call fails, but still fills the list.
    unsafe {
        QueryInformationJobObject(
            job,
            JobObjectBasicProcessIdList,
            (&raw mut list).cast(),
            std::mem::size_of::<IdList>() as u32,
            std::ptr::null_mut(),
        );
    }
    let listed = (list.listed as usize).min(list.ids.len());
    list.ids[..listed].iter().map(|id| *id as u32).collect()
}

/// `exit_from_inside` for a process known by its id.
#[cfg(windows)]
fn exit_pid_from_inside(pid: u32) -> bool {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_CREATE_THREAD, PROCESS_QUERY_INFORMATION, PROCESS_VM_OPERATION,
        PROCESS_VM_READ, PROCESS_VM_WRITE,
    };
    let access = PROCESS_CREATE_THREAD
        | PROCESS_QUERY_INFORMATION
        | PROCESS_VM_OPERATION
        | PROCESS_VM_READ
        | PROCESS_VM_WRITE;
    let h = unsafe { OpenProcess(access, 0, pid) };
    if h.is_null() {
        return false;
    }
    let process = unsafe { OwnedHandle::from_raw_handle(h) };
    exit_from_inside(process.as_raw_handle())
}

/// Runs `ExitProcess` on a new thread inside `process`, the way Git for
/// Windows emulates `kill -TERM`, so that its atexit handlers run.
/// Returns whether the thread was started.
#[cfg(windows)]
fn exit_from_inside(process: std::os::windows::io::RawHandle) -> bool {
    use std::ffi::c_void;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    use windows_sys::Win32::System::SystemInformation::{
        IMAGE_FILE_MACHINE_AMD64, IMAGE_FILE_MACHINE_I386,
    };
    use windows_sys::Win32::System::Threading::{
        CreateRemoteThread, GetCurrentProcess, IsWow64Process2, LPTHREAD_START_ROUTINE,
    };
    use windows_sys::core::{s, w};
    unsafe {
        // kernel32 sits at the same address in every process of one
        // architecture, so our `ExitProcess` is valid in the target only if
        // it is built for ours. IsWow64Process2 tells 32-bit x86 processes
        // from native ones, but on an ARM64 machine it cannot tell emulated
        // x64 from native ARM64, so there the stop is left to the kill.
        let (mut ours, mut native, mut theirs, mut unused) = (0, 0, 0, 0);
        if IsWow64Process2(GetCurrentProcess(), &mut ours, &mut native) == 0
            || !matches!(native, IMAGE_FILE_MACHINE_AMD64 | IMAGE_FILE_MACHINE_I386)
            || IsWow64Process2(process, &mut theirs, &mut unused) == 0
            || ours != theirs
        {
            return false;
        }
        let kernel32 = GetModuleHandleW(w!("kernel32.dll"));
        if kernel32.is_null() {
            return false;
        }
        let Some(exit) = GetProcAddress(kernel32, s!("ExitProcess")) else {
            return false;
        };
        // `ExitProcess(u32)` takes its one argument where a thread routine
        // takes its parameter.
        let start: LPTHREAD_START_ROUTINE = Some(std::mem::transmute::<
            unsafe extern "system" fn() -> isize,
            unsafe extern "system" fn(*mut c_void) -> u32,
        >(exit));
        let thread = CreateRemoteThread(
            process,
            std::ptr::null(),
            0,
            start,
            STOPPED_EXIT_CODE as usize as *const c_void,
            0,
            std::ptr::null_mut(),
        );
        if thread.is_null() {
            return false;
        }
        CloseHandle(thread);
        true
    }
}

/// Gives a stopped command `KILL_GRACE` to exit on its own.
fn wait_grace(child: &mut Child) {
    let grace = Instant::now();
    while grace.elapsed() < KILL_GRACE && matches!(child.try_wait(), Ok(None)) {
        std::thread::sleep(Duration::from_millis(20));
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

    /// Runs `read_regular` on another thread so a blocking open fails the
    /// test instead of hanging it.
    fn read_soon(path: &Path, cap: u64) -> FileRead {
        let (tx, rx) = std::sync::mpsc::channel();
        let path = path.to_path_buf();
        std::thread::spawn(move || tx.send(read_regular(&path, cap)));
        rx.recv_timeout(Duration::from_secs(3))
            .unwrap_or_else(|_| panic!("read_regular blocked"))
    }

    #[test]
    fn read_regular_reads_small_files_and_caps_large_ones() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, "abc").unwrap();
        assert_eq!(read_soon(&p, 3), FileRead::Contents(b"abc".to_vec()));
        assert_eq!(read_soon(&p, 2), FileRead::TooLarge(3));
        assert_eq!(read_soon(dir.path(), 10), FileRead::NotRegular);
        assert_eq!(
            read_soon(&dir.path().join("missing"), 10),
            FileRead::NotRegular
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_regular_never_follows_links_or_opens_fifos() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe");
        let made = Command::new("mkfifo").arg(&fifo).status().unwrap();
        assert!(made.success());
        assert_eq!(read_soon(&fifo, 10), FileRead::NotRegular);
        let zero = dir.path().join("zero");
        std::os::unix::fs::symlink("/dev/zero", &zero).unwrap();
        assert_eq!(read_soon(&zero, 10), FileRead::NotRegular);
        let target = dir.path().join("t");
        std::fs::write(&target, "x").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(read_soon(&link, 10), FileRead::NotRegular);
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1500), "2 KB");
        assert_eq!(human_size(5 << 20), "5.0 MB");
        assert_eq!(human_size(3 << 30), "3.0 GB");
        assert_eq!(human_size(2 << 40), "2.0 TB");
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
