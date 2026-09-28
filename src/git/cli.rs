//! `GitBackend` implemented by running the `git` command.

use std::collections::{BTreeSet, HashMap};
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use super::parse::{self, BRANCH_FORMAT, LOG_FORMAT, REFLOG_FORMAT, STASH_FORMAT};
use super::{FetchError, GitBackend, GitError, Repo, SnapshotOpts, classify_fetch_stderr};
use crate::model::{
    CommitDetail, Counts, DetailData, DetailReq, DiffBlock, DiffKind, DiffLine, Head, RepoOp,
    Snapshot, Upstream,
};

/// Largest untracked file whose lines are counted for the `+N` column.
const MAX_COUNTED_FILE: u64 = 1 << 20;
/// How long a stopped fetch gets to clean up and exit before it is killed.
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

    /// Untracked-file listing that follows `status.showUntrackedFiles`, but
    /// lists individual files (`all`) when it is not set.
    fn untracked_mode(&self) -> &'static str {
        let value = self
            .git(&["config", "--get", "status.showUntrackedFiles"])
            .unwrap_or_default();
        match String::from_utf8_lossy(&value).trim() {
            "no" | "false" | "off" | "0" => "--untracked-files=no",
            "normal" | "true" | "on" | "1" => "--untracked-files=normal",
            _ => "--untracked-files=all",
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
    fn count_lines(&self, rel: &str) -> Counts {
        let FileRead::Contents(bytes) = read_regular(&self.repo.root.join(rel), MAX_COUNTED_FILE)
        else {
            return Counts::Unknown;
        };
        if bytes.contains(&0) {
            return Counts::Binary;
        }
        let newlines = bytes.iter().filter(|b| **b == b'\n').count();
        let trailing = !bytes.is_empty() && !bytes.ends_with(b"\n");
        Counts::lines((newlines + trailing as usize) as u32, 0)
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
        let raw = self.git(&[
            "status",
            "--porcelain=v2",
            "-z",
            "--branch",
            "--show-stash",
            self.untracked_mode(),
        ])?;
        let (header, mut changes) = parse::parse_status(&raw);

        let head = match (&header.oid, &header.head) {
            (None, Some(b)) => Head::Unborn(b.clone()),
            (Some(oid), None) => Head::Detached(oid.chars().take(7).collect()),
            (_, Some(b)) => Head::Branch(b.clone()),
            (None, None) => Head::Detached(String::new()),
        };
        let unborn = matches!(head, Head::Unborn(_));

        // Paths left out stay `Counts::Unknown`, which shows as blank.
        if changes.len() <= opts.numstat_max_files {
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
                    c.counts = self.count_lines(&c.path);
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
            // FETCH_HEAD is per worktree, like HEAD.
            last_fetch: mtime(&self.repo.git_dir.join("FETCH_HEAD")),
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

    fn fetch(&self, prune: bool, timeout: Duration, cancel: &AtomicBool) -> Result<(), FetchError> {
        if cancel.load(Ordering::SeqCst) {
            return Err(FetchError::Cancelled);
        }
        let mut cmd = self.fetch_command(prune);
        detach(&mut cmd);
        let mut child = cmd.spawn().map_err(|e| FetchError::Other(e.to_string()))?;
        let tree = ProcessTree::adopt(&child);
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
                Ok(None) if cancel.load(Ordering::SeqCst) => {
                    tree.terminate(&mut child);
                    let _ = child.wait();
                    return Err(FetchError::Cancelled);
                }
                Ok(None) if start.elapsed() >= timeout => {
                    tree.terminate(&mut child);
                    let _ = child.wait();
                    return Err(FetchError::Timeout);
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => return Err(FetchError::Other(e.to_string())),
            }
        }
    }
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

/// A fetch and every process it starts.
#[cfg(unix)]
struct ProcessTree;

#[cfg(unix)]
impl ProcessTree {
    fn adopt(_child: &Child) -> Self {
        ProcessTree
    }

    /// Stops a timed-out or cancelled fetch and everything it started.
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

/// A fetch and every process it starts, held in a job object so that a stop
/// reaches ssh and the remote helpers as well as git.
#[cfg(windows)]
struct ProcessTree {
    job: Option<std::os::windows::io::OwnedHandle>,
}

#[cfg(windows)]
impl ProcessTree {
    fn adopt(child: &Child) -> Self {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW};
        // Git has only just started, so it has not yet started anything that
        // would be left outside the job.
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

    /// Stops a timed-out or cancelled fetch and everything it started.
    fn terminate(&self, child: &mut Child) {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        // Git removes its ref lock files in atexit handlers, which
        // TerminateProcess skips, and a leftover lock would break the user's
        // own next fetch. Ask git to exit from inside first.
        if exit_from_inside(child) {
            wait_grace(child);
        }
        match &self.job {
            Some(job) => unsafe {
                TerminateJobObject(job.as_raw_handle(), STOPPED_EXIT_CODE);
            },
            None => {
                let _ = child.kill();
            }
        }
    }
}

/// Exit status for a stopped fetch on Windows, as if by SIGTERM.
#[cfg(windows)]
const STOPPED_EXIT_CODE: u32 = 128 + 15;

/// Runs `ExitProcess` on a new thread inside `child`, the way Git for
/// Windows emulates `kill -TERM`, so that git's atexit handlers run.
/// Returns whether the thread was started.
#[cfg(windows)]
fn exit_from_inside(child: &Child) -> bool {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    use windows_sys::Win32::System::Threading::{
        CreateRemoteThread, GetCurrentProcess, IsWow64Process, LPTHREAD_START_ROUTINE,
    };
    use windows_sys::core::{s, w};
    let process = child.as_raw_handle();
    unsafe {
        // kernel32 sits at the same address in every process of one
        // architecture, so our `ExitProcess` is git's, but only if git is
        // built for the same one.
        let (mut ours, mut theirs) = (0, 0);
        if IsWow64Process(GetCurrentProcess(), &mut ours) == 0
            || IsWow64Process(process, &mut theirs) == 0
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

/// Gives a stopped fetch `KILL_GRACE` to exit on its own.
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
