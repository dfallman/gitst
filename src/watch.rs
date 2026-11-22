//! Filesystem watching: turns relevant file events into refresh signals.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant, SystemTime};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::event::ModifyKind;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::git::Repo;
use crate::worker::WorkerMsg;

/// Keeps the underlying watcher alive; dropping it stops watching.
pub struct WatchHandle {
    _watcher: Arc<Mutex<RecommendedWatcher>>,
}

/// Decides whether a changed path could change what gitst shows.
pub struct Relevance {
    root: PathBuf,
    git_dir: PathBuf,
    common_dir: PathBuf,
    /// Per-directory `.gitignore` matchers, `None` when the directory has none.
    ignores: HashMap<PathBuf, Option<Gitignore>>,
    /// Global excludes and `.git/info/exclude`.
    base: Vec<Gitignore>,
    /// Whether each directory seen so far is itself ignored.
    ignored_dirs: HashMap<PathBuf, bool>,
}

/// Quiet period that ends a burst of file events.
const DEBOUNCE: Duration = Duration::from_millis(150);
/// A continuous burst still refreshes at least this often.
const MAX_BURST: Duration = Duration::from_secs(1);
/// How often `.git` state is checked directly so a dropped watcher event
/// never leaves the view stale until the next timed fetch.
const SAFETY_POLL: Duration = Duration::from_secs(2);
/// Upper bound on a small ref or reftable-list file read by `GitStamp`.
const MAX_STAMP_FILE: u64 = 4096;
/// Most remotes inspected per safety-net check under `refs/remotes/`.
const MAX_REMOTES: usize = 16;

/// How the work tree is watched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// One recursive watch. FSEvents and ReadDirectoryChangesW watch a whole
    /// tree with one handle, so ignored directories cost nothing to watch.
    /// Their events still arrive; `Relevance` drops them cheaply, and they
    /// neither start nor lengthen a burst.
    Recursive,
    /// One watch per directory that is not ignored, added as directories
    /// appear. inotify and kqueue need a watch per directory, and a
    /// recursive watch would spend them on `target/` or `node_modules/`
    /// until they run out.
    PerDirectory,
}

const MODE: Mode = if cfg!(any(target_os = "macos", target_os = "windows")) {
    Mode::Recursive
} else {
    Mode::PerDirectory
};

/// Global excludes and `info/exclude`, rooted at the work tree so its
/// paths can be matched.
fn base_ignores(repo_root: &Path, common_dir: &Path) -> Vec<Gitignore> {
    let (global, _) = GitignoreBuilder::new(repo_root).build_global();
    let mut exclude = GitignoreBuilder::new(repo_root);
    exclude.add(common_dir.join("info/exclude"));
    vec![
        global,
        exclude.build().unwrap_or_else(|_| Gitignore::empty()),
    ]
}

impl Relevance {
    pub fn new(repo: &Repo) -> Self {
        Relevance {
            root: repo.root.clone(),
            git_dir: repo.git_dir.clone(),
            common_dir: repo.common_dir.clone(),
            ignores: HashMap::new(),
            base: base_ignores(&repo.root, &repo.common_dir),
            ignored_dirs: HashMap::new(),
        }
    }

    pub fn is_relevant(&mut self, path: &Path) -> bool {
        for dir in [&self.git_dir, &self.common_dir] {
            if let Ok(rel) = path.strip_prefix(dir) {
                if rel == Path::new("info/exclude") {
                    self.base = base_ignores(&self.root, &self.common_dir);
                    self.ignored_dirs.clear();
                }
                return git_internal_relevant(rel);
            }
        }
        let Ok(rel) = path.strip_prefix(&self.root) else {
            return false;
        };
        // Inside a nested repository's git directory, such as an older
        // submodule's `sub/.git/`, unless that repository is itself in an
        // ignored directory (a virtualenv or a build's checkout).
        let comps: Vec<_> = rel.components().collect();
        if let Some(i) = comps.iter().rposition(|c| c.as_os_str() == ".git") {
            let nested: PathBuf = comps[..i].iter().collect();
            if i > 0 && self.ignored_as(&self.root.join(nested), true) {
                return false;
            }
            let inner: PathBuf = comps[i + 1..].iter().collect();
            return git_internal_relevant(&inner);
        }
        if path.file_name().is_some_and(|n| n == ".gitignore") {
            self.ignores.clear();
            self.ignored_dirs.clear();
            return true;
        }
        !self.ignored(path)
    }

    /// Whether `path` is ignored. As in git, nothing inside an ignored
    /// directory can be re-included, so the first ignored ancestor settles
    /// it; those answers are cached, which keeps a build writing thousands
    /// of files under `target/` cheap to filter.
    fn ignored(&mut self, path: &Path) -> bool {
        self.ignored_as(path, path.is_dir())
    }

    fn ignored_as(&mut self, path: &Path, is_dir: bool) -> bool {
        let dirs: Vec<PathBuf> = path
            .ancestors()
            .skip(1)
            .take_while(|d| d.starts_with(&self.root) && *d != self.root)
            .map(Path::to_path_buf)
            .collect();
        for dir in dirs.into_iter().rev() {
            let ignored = match self.ignored_dirs.get(&dir) {
                Some(i) => *i,
                None => {
                    let i = self.matches(&dir, true);
                    self.ignored_dirs.insert(dir, i);
                    i
                }
            };
            if ignored {
                return true;
            }
        }
        self.matches(path, is_dir)
    }

    /// Applies ignore rules from lowest to highest precedence to `path`
    /// alone; the last matching rule wins, so a deeper `!pattern` can
    /// re-include it.
    fn matches(&mut self, path: &Path, is_dir: bool) -> bool {
        let mut ignored = false;
        let mut apply = |m: ignore::Match<&ignore::gitignore::Glob>| {
            if m.is_ignore() {
                ignored = true;
            } else if m.is_whitelist() {
                ignored = false;
            }
        };
        for g in &self.base {
            apply(g.matched(path, is_dir));
        }
        let dirs: Vec<PathBuf> = path
            .ancestors()
            .skip(1)
            .take_while(|d| d.starts_with(&self.root))
            .map(Path::to_path_buf)
            .collect();
        for dir in dirs.into_iter().rev() {
            let matcher = self.ignores.entry(dir.clone()).or_insert_with(|| {
                let file = dir.join(".gitignore");
                file.is_file().then(|| Gitignore::new(file).0)
            });
            if let Some(m) = matcher {
                apply(m.matched(path, is_dir));
            }
        }
        ignored
    }
}

/// Whether a path inside a git directory reflects a change in repo state.
pub fn git_internal_relevant(rel: &Path) -> bool {
    // A coalesced or directory-level event on `.git` itself may cover any
    // ref, HEAD, or index change inside it.
    if rel.as_os_str().is_empty() {
        return true;
    }
    // A submodule's git directory is `modules/<name>/`, and the name may
    // contain slashes, so any tail of the path may be the part that counts.
    if let Ok(sub) = rel.strip_prefix("modules") {
        let comps: Vec<_> = sub.components().collect();
        if comps.is_empty() {
            return true;
        }
        return (1..comps.len()).any(|i| {
            let tail: PathBuf = comps[i..].iter().collect();
            own_git_path_relevant(&tail)
        });
    }
    own_git_path_relevant(rel)
}

/// Whether `s` is `dir` itself or a path beneath `dir/`.
fn under(s: &str, dir: &str) -> bool {
    s == dir
        || s.strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn own_git_path_relevant(rel: &Path) -> bool {
    // Compared as git writes them, with `/`, whatever the platform uses.
    let parts: Vec<_> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect();
    let s = parts.join("/");
    matches!(
        s.as_ref(),
        "HEAD"
            | "index"
            | "index.lock"
            | "packed-refs"
            | "BISECT_LOG"
            | "config"
            | "config.worktree"
            | "info/exclude"
    ) || (s.ends_with("_HEAD") && !s.contains('/'))
        || [
            "refs",
            "logs",
            "reftable",
            "rebase-merge",
            "rebase-apply",
            "sequencer",
        ]
        .iter()
        .any(|dir| under(&s, dir))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileStamp {
    is_dir: bool,
    len: u64,
    modified: Option<SystemTime>,
}

/// Cheap fingerprint of a repository's `.git` state, taken without spawning
/// git, so the watcher thread can catch external commits, pushes, checkouts,
/// staging, or ref updates even if the OS watcher drops an event.
#[derive(Clone, Debug, PartialEq, Eq)]
struct GitStamp {
    head: Option<Vec<u8>>,
    head_ref: Option<Vec<u8>>,
    head_ref_log: Option<FileStamp>,
    remote_refs: Vec<(String, Option<FileStamp>, Option<Vec<u8>>)>,
    stash: Option<Vec<u8>>,
    reftable: Option<Vec<u8>>,
    stats: [Option<FileStamp>; 17],
}

fn stat_file(path: &Path) -> Option<FileStamp> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    Some(FileStamp {
        is_dir: meta.is_dir(),
        len: meta.len(),
        modified: meta.modified().ok(),
    })
}

/// Reads a small regular file (never a symlink, directory, or FIFO), capped
/// at `MAX_STAMP_FILE` bytes.
fn read_small(path: &Path) -> Option<Vec<u8>> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_STAMP_FILE {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let mut buf = Vec::with_capacity(meta.len() as usize);
    file.take(MAX_STAMP_FILE + 1).read_to_end(&mut buf).ok()?;
    (buf.len() as u64 <= MAX_STAMP_FILE).then_some(buf)
}

/// The target of `HEAD` when it is a safe relative `ref: <path>` line.
fn head_symref(head: &[u8]) -> Option<&str> {
    let target = std::str::from_utf8(head)
        .ok()?
        .lines()
        .next()?
        .strip_prefix("ref:")?
        .trim();
    if target.is_empty() {
        return None;
    }
    let safe = Path::new(target)
        .components()
        .all(|c| matches!(c, Component::Normal(_)));
    safe.then_some(target)
}

fn remote_branch_refs(
    common_dir: &Path,
    branch: &str,
) -> Vec<(String, Option<FileStamp>, Option<Vec<u8>>)> {
    let remotes_dir = common_dir.join("refs/remotes");
    let Ok(entries) = std::fs::read_dir(&remotes_dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            e.file_type()
                .ok()?
                .is_dir()
                .then(|| e.file_name().to_string_lossy().into_owned())
        })
        .collect();
    names.sort();
    names.truncate(MAX_REMOTES);
    names
        .into_iter()
        .map(|remote| {
            let dir = remotes_dir.join(&remote);
            let dir_stamp = stat_file(&dir);
            let ref_bytes = read_small(&dir.join(branch));
            (remote, dir_stamp, ref_bytes)
        })
        .collect()
}

impl GitStamp {
    fn take(git_dir: &Path, common_dir: &Path) -> Self {
        let head = read_small(&git_dir.join("HEAD"));
        let (head_ref, head_ref_log, remote_refs) = match head.as_deref().and_then(head_symref) {
            Some(target) => {
                let hr = read_small(&common_dir.join(target));
                let log = stat_file(&common_dir.join("logs").join(target));
                let remotes = target
                    .strip_prefix("refs/heads/")
                    .map(|branch| remote_branch_refs(common_dir, branch))
                    .unwrap_or_default();
                (hr, log, remotes)
            }
            None => (None, None, Vec::new()),
        };
        GitStamp {
            head,
            head_ref,
            head_ref_log,
            remote_refs,
            stash: read_small(&common_dir.join("refs/stash")),
            reftable: read_small(&common_dir.join("reftable/tables.list")),
            stats: [
                stat_file(&git_dir.join("index")),
                stat_file(&git_dir.join("index.lock")),
                stat_file(&git_dir.join("FETCH_HEAD")),
                stat_file(&git_dir.join("logs/HEAD")),
                stat_file(&git_dir.join("MERGE_HEAD")),
                stat_file(&git_dir.join("CHERRY_PICK_HEAD")),
                stat_file(&git_dir.join("REVERT_HEAD")),
                stat_file(&git_dir.join("BISECT_LOG")),
                stat_file(&git_dir.join("rebase-merge")),
                stat_file(&git_dir.join("rebase-merge/msgnum")),
                stat_file(&git_dir.join("rebase-apply")),
                stat_file(&git_dir.join("rebase-apply/next")),
                stat_file(&git_dir.join("sequencer")),
                stat_file(&git_dir.join("config.worktree")),
                stat_file(&common_dir.join("packed-refs")),
                stat_file(&common_dir.join("config")),
                stat_file(&common_dir.join("info/exclude")),
            ],
        }
    }
}

fn watcher(tx: Sender<notify::Result<notify::Event>>) -> notify::Result<RecommendedWatcher> {
    notify::recommended_watcher(tx)
}

/// Directories under `dir` (and `dir` itself) that are not ignored, as
/// `git status` would descend into them. Git directories are skipped.
fn unignored_dirs(dir: &Path) -> impl Iterator<Item = PathBuf> {
    ignore::WalkBuilder::new(dir)
        .hidden(false)
        // Only git's own ignore files, not ripgrep's `.ignore`.
        .ignore(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_some_and(|t| t.is_dir()))
        .map(ignore::DirEntry::into_path)
}

/// Watches every directory under `dir` that is not ignored, one by one.
fn watch_unignored(w: &mut RecommendedWatcher, dir: &Path) -> notify::Result<()> {
    for d in unignored_dirs(dir) {
        match w.watch(&d, RecursiveMode::NonRecursive) {
            // Gone again before it could be watched.
            Err(notify::Error {
                kind: notify::ErrorKind::PathNotFound,
                ..
            }) => {}
            r => r?,
        }
    }
    Ok(())
}

/// Watches the repository and sends one `Refresh` per burst of relevant events.
pub fn spawn(repo: &Repo, out: Sender<WorkerMsg>) -> notify::Result<WatchHandle> {
    spawn_with(repo, out, MODE)
}

fn spawn_with(repo: &Repo, out: Sender<WorkerMsg>, mode: Mode) -> notify::Result<WatchHandle> {
    let (tx, rx) = mpsc::channel();
    let mut w = watcher(tx)?;
    match mode {
        Mode::Recursive => w.watch(&repo.root, RecursiveMode::Recursive)?,
        Mode::PerDirectory => watch_unignored(&mut w, &repo.root)?,
    }
    let inside = |d: &Path| mode == Mode::Recursive && d.starts_with(&repo.root);
    if !inside(&repo.git_dir) {
        w.watch(&repo.git_dir, RecursiveMode::Recursive)?;
    }
    if !inside(&repo.common_dir) && !repo.common_dir.starts_with(&repo.git_dir) {
        w.watch(&repo.common_dir, RecursiveMode::Recursive)?;
    }
    let watcher = Arc::new(Mutex::new(w));
    // The thread must not keep the watcher alive, or dropping the handle
    // would never end it.
    let weak = Arc::downgrade(&watcher);
    let repo = repo.clone();
    std::thread::Builder::new()
        .name("gitst-watch".into())
        .spawn(move || watch_loop(repo, rx, out, weak, mode, SAFETY_POLL))
        .map_err(|e| notify::Error::generic(&e.to_string()))?;
    Ok(WatchHandle { _watcher: watcher })
}

fn watch_loop(
    repo: Repo,
    rx: Receiver<notify::Result<notify::Event>>,
    out: Sender<WorkerMsg>,
    weak: Weak<Mutex<RecommendedWatcher>>,
    mode: Mode,
    safety_poll: Duration,
) {
    let mut relevance = Relevance::new(&repo);
    let mut last_stamp = GitStamp::take(&repo.git_dir, &repo.common_dir);
    let mut next_check = Instant::now() + safety_poll;
    // Every event is looked at, even inside a burst, so that no new
    // directory goes unwatched. Returns whether the event is
    // relevant, and whether `.gitignore` changed (or a rescan happened).
    let mut on_event = |ev: notify::Result<notify::Event>| -> (bool, bool) {
        let Ok(ev) = ev else {
            return (true, false);
        };
        if ev.need_rescan() {
            return (true, true);
        }
        if matches!(ev.kind, EventKind::Access(_)) {
            return (false, false);
        }
        let relevant: Vec<&PathBuf> = ev
            .paths
            .iter()
            .filter(|p| relevance.is_relevant(p))
            .collect();
        let gitignore = relevant
            .iter()
            .any(|p| p.file_name().is_some_and(|n| n == ".gitignore"));
        if mode == Mode::PerDirectory && !gitignore {
            follow_new_dirs(&weak, &repo.root, &ev, &relevant);
        }
        (!relevant.is_empty(), gitignore)
    };
    loop {
        let wait = next_check.saturating_duration_since(Instant::now());
        let first = match rx.recv_timeout(wait) {
            Ok(ev) => Some(ev),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        if let Some(ev) = first {
            let (relevant, mut rewalk) = on_event(ev);
            if !relevant {
                if Instant::now() >= next_check {
                    next_check = Instant::now() + safety_poll;
                    let stamp = GitStamp::take(&repo.git_dir, &repo.common_dir);
                    if stamp != last_stamp {
                        last_stamp = stamp;
                        if out.send(WorkerMsg::Refresh).is_err() {
                            return;
                        }
                    }
                }
                continue;
            }
            // The burst ends after a quiet period without relevant
            // events: a build writing to `target/` must not hold back
            // the refresh for an edit next to it.
            let end = Instant::now() + MAX_BURST;
            let mut quiet = Instant::now() + DEBOUNCE;
            loop {
                let wait = quiet.min(end).saturating_duration_since(Instant::now());
                if wait.is_zero() {
                    break;
                }
                match rx.recv_timeout(wait) {
                    Ok(ev) => {
                        let (relevant, gitignore) = on_event(ev);
                        rewalk |= gitignore;
                        if relevant {
                            quiet = Instant::now() + DEBOUNCE;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            // After `.gitignore` edits or a rescan, once per burst:
            // directories no longer ignored need watches. (Newly ignored
            // ones keep theirs until restart; their events are dropped.)
            if mode == Mode::PerDirectory
                && rewalk
                && let Some(w) = weak.upgrade()
            {
                let mut w = w.lock().unwrap_or_else(PoisonError::into_inner);
                let _ = watch_unignored(&mut w, &repo.root);
            }
            last_stamp = GitStamp::take(&repo.git_dir, &repo.common_dir);
            next_check = Instant::now() + safety_poll;
            if out.send(WorkerMsg::Refresh).is_err() {
                return;
            }
        } else {
            next_check = Instant::now() + safety_poll;
            let stamp = GitStamp::take(&repo.git_dir, &repo.common_dir);
            if stamp != last_stamp {
                last_stamp = stamp;
                if out.send(WorkerMsg::Refresh).is_err() {
                    return;
                }
            }
        }
    }
}

/// Adds watches for directories that appeared (created or moved in).
fn follow_new_dirs(
    watcher: &Weak<Mutex<RecommendedWatcher>>,
    root: &Path,
    ev: &notify::Event,
    relevant: &[&PathBuf],
) {
    let appeared = matches!(
        ev.kind,
        EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(_))
    );
    if !appeared {
        return;
    }
    let dirs: Vec<&Path> = relevant
        .iter()
        .filter(|p| p.starts_with(root) && p.is_dir())
        .map(|p| p.as_path())
        .collect();
    if dirs.is_empty() {
        return;
    }
    let Some(w) = watcher.upgrade() else {
        return;
    };
    let mut w = w.lock().unwrap_or_else(PoisonError::into_inner);
    for d in dirs {
        // Anything missed here is still found by the refresh this event
        // starts; only later edits inside would go unseen.
        let _ = watch_unignored(&mut w, d);
    }
}

/// Watches one directory (not recursively) and calls `wake` once per burst
/// of changes, until it returns false. Used while waiting for a repository
/// to appear.
pub fn spawn_dir(
    dir: &Path,
    wake: impl Fn() -> bool + Send + 'static,
) -> notify::Result<WatchHandle> {
    let (tx, rx) = mpsc::channel::<notify::Result<notify::Event>>();
    let mut w = watcher(tx)?;
    w.watch(dir, RecursiveMode::NonRecursive)?;
    std::thread::spawn(move || {
        while rx.recv().is_ok() {
            while rx.recv_timeout(DEBOUNCE).is_ok() {}
            if !wake() {
                return;
            }
        }
    });
    Ok(WatchHandle {
        _watcher: Arc::new(Mutex::new(w)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git_repo() -> (tempfile::TempDir, Repo) {
        let d = tempfile::tempdir().unwrap();
        let ok = Command::new("git")
            .args(["init", "-q"])
            .current_dir(d.path())
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let repo = Repo::discover(d.path()).ok().unwrap();
        (d, repo)
    }

    fn write(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    #[test]
    fn unignored_dirs_skip_ignored_and_git() {
        let (_d, repo) = git_repo();
        let root = &repo.root;
        write(root, ".gitignore", "target/\n");
        write(root, "src/deep/a.rs", "");
        write(root, "target/debug/x", "");
        let mut dirs: Vec<PathBuf> = unignored_dirs(root)
            .map(|d| d.strip_prefix(root).unwrap().to_path_buf())
            .collect();
        dirs.sort();
        let want: Vec<PathBuf> = ["", "src", "src/deep"].iter().map(PathBuf::from).collect();
        assert_eq!(dirs, want);
    }

    #[test]
    fn per_directory_watch_follows_new_directories() {
        let (_d, repo) = git_repo();
        let (tx, rx) = mpsc::channel();
        let _h = spawn_with(&repo, tx, Mode::PerDirectory).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let refresh = |what: &str| {
            assert!(
                matches!(
                    rx.recv_timeout(Duration::from_secs(5)),
                    Ok(WorkerMsg::Refresh)
                ),
                "no refresh for {what}"
            );
            // Let the burst end.
            while rx.recv_timeout(Duration::from_millis(400)).is_ok() {}
        };
        write(&repo.root, "new/deep/a.txt", "1");
        refresh("a new directory");
        write(&repo.root, "new/deep/b.txt", "1");
        refresh("a file in a directory created after the start");
    }

    #[test]
    fn per_directory_watch_follows_gitignore_edits() {
        let (_d, repo) = git_repo();
        write(&repo.root, ".gitignore", "gen/\n");
        write(&repo.root, "gen/sub/a.txt", "1");
        let (tx, rx) = mpsc::channel();
        let _h = spawn_with(&repo, tx, Mode::PerDirectory).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let refresh = |what: &str| {
            assert!(
                matches!(
                    rx.recv_timeout(Duration::from_secs(5)),
                    Ok(WorkerMsg::Refresh)
                ),
                "no refresh for {what}"
            );
            while rx.recv_timeout(Duration::from_millis(400)).is_ok() {}
        };
        write(&repo.root, ".gitignore", "");
        refresh("the .gitignore edit");
        write(&repo.root, "gen/sub/b.txt", "1");
        refresh("a file in a directory no longer ignored");
    }

    #[test]
    fn internal_paths() {
        for p in [
            "",
            "HEAD",
            "index",
            "index.lock",
            "refs",
            "refs/heads/main",
            "logs",
            "logs/HEAD",
            "reftable",
            "reftable/tables.list",
            "MERGE_HEAD",
            "FETCH_HEAD",
            "rebase-merge",
            "rebase-merge/msgnum",
            "packed-refs",
            "BISECT_LOG",
            "config",
            "info/exclude",
            "modules",
            // Submodules' git directories, whose names may contain slashes.
            "modules/sub/HEAD",
            "modules/sub/refs",
            "modules/sub/reftable/tables.list",
            "modules/libs/foo/refs/heads/main",
            "modules/a/modules/b/index",
        ] {
            assert!(git_internal_relevant(Path::new(p)), "{p}");
        }
        // Event paths on Windows use backslashes.
        #[cfg(windows)]
        for p in [
            r"refs\heads\main",
            r"logs\HEAD",
            r"reftable\tables.list",
            r"info\exclude",
            r"rebase-merge\msgnum",
            r"modules\libs\foo\refs\heads\main",
        ] {
            assert!(git_internal_relevant(Path::new(p)), "{p}");
        }
        for p in [
            "objects/ab/cdef",
            "modules/sub/objects/ab/cdef",
            "refs-extra",
            "logs-extra",
            "reftable-extra",
            "rebase-merge-extra",
            "description",
            "hooks/pre-commit",
            "COMMIT_EDITMSG",
        ] {
            assert!(!git_internal_relevant(Path::new(p)), "{p}");
        }
    }

    fn run_git(root: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    #[test]
    fn head_symref_rejects_traversal() {
        assert_eq!(
            head_symref(b"ref: refs/heads/main\n"),
            Some("refs/heads/main")
        );
        assert_eq!(
            head_symref(b"ref: refs/heads/feat/one\n"),
            Some("refs/heads/feat/one")
        );
        assert_eq!(head_symref(b"ref: ../outside\n"), None);
        assert_eq!(head_symref(b"ref: /etc/passwd\n"), None);
        assert_eq!(head_symref(b"ref: \n"), None);
        assert_eq!(head_symref(b"0123456789abcdef\n"), None);
    }

    #[test]
    fn git_stamp_detects_commits_amends_pushes_and_reftable() {
        let (_d, repo) = git_repo();
        let take = || GitStamp::take(&repo.git_dir, &repo.common_dir);

        let s0 = take();
        run_git(&repo.root, &["commit", "-q", "--allow-empty", "-m", "one"]);
        let s1 = take();
        assert_ne!(s0, s1, "commit should change stamp");

        // Same-second amend rewrites the 41-byte loose ref with a new oid.
        run_git(
            &repo.root,
            &["commit", "-q", "--amend", "--allow-empty", "-m", "one-b"],
        );
        let s2 = take();
        assert_ne!(s1, s2, "amend should change stamp");

        // Push to a bare remote updates refs/remotes/origin/<branch>.
        let bare_dir = tempfile::tempdir().unwrap();
        run_git(bare_dir.path(), &["init", "-q", "--bare"]);
        run_git(
            &repo.root,
            &["remote", "add", "origin", bare_dir.path().to_str().unwrap()],
        );
        let s3 = take();
        assert_ne!(s2, s3, "adding remote changes config");
        run_git(&repo.root, &["push", "-q", "-u", "origin", "HEAD"]);
        let s4 = take();
        assert_ne!(s3, s4, "push updates remote-tracking ref");

        // Reftable table list updates also change the stamp.
        write(&repo.common_dir, "reftable/tables.list", "0001.ref\n");
        let s5 = take();
        assert_ne!(s4, s5, "reftable update should change stamp");
        write(&repo.common_dir, "reftable/tables.list", "0002.ref\n");
        let s6 = take();
        assert_ne!(s5, s6, "reftable content change should change stamp");
    }

    #[test]
    fn safety_poll_catches_missed_commit_without_fs_events() {
        let (_d, repo) = git_repo();
        run_git(&repo.root, &["commit", "-q", "--allow-empty", "-m", "one"]);
        let (ev_tx, ev_rx) = mpsc::channel();
        let (out_tx, out_rx) = mpsc::channel();
        let repo_thread = repo.clone();
        let handle = std::thread::spawn(move || {
            watch_loop(
                repo_thread,
                ev_rx,
                out_tx,
                Weak::new(),
                Mode::Recursive,
                Duration::from_millis(50),
            )
        });

        // Quiet repo: safety poll sends no spurious refreshes.
        assert!(out_rx.recv_timeout(Duration::from_millis(180)).is_err());

        // External commit with zero notify events delivered.
        run_git(&repo.root, &["commit", "-q", "--allow-empty", "-m", "two"]);
        assert!(matches!(
            out_rx.recv_timeout(Duration::from_secs(2)),
            Ok(WorkerMsg::Refresh)
        ));
        // Only one refresh for the commit.
        assert!(out_rx.recv_timeout(Duration::from_millis(180)).is_err());

        drop(ev_tx);
        handle.join().unwrap();
    }

    #[test]
    fn safety_poll_not_starved_by_ignored_events() {
        let (_d, repo) = git_repo();
        write(&repo.root, ".gitignore", "target/\n");
        run_git(&repo.root, &["add", ".gitignore"]);
        run_git(&repo.root, &["commit", "-q", "-m", "init"]);

        let (ev_tx, ev_rx) = mpsc::channel();
        let (out_tx, out_rx) = mpsc::channel();
        let repo_thread = repo.clone();
        let handle = std::thread::spawn(move || {
            watch_loop(
                repo_thread,
                ev_rx,
                out_tx,
                Weak::new(),
                Mode::Recursive,
                Duration::from_millis(80),
            )
        });

        run_git(&repo.root, &["commit", "-q", "--allow-empty", "-m", "two"]);
        let ignored = repo.root.join("target/out.o");
        let start = Instant::now();
        let mut refreshed = false;
        while start.elapsed() < Duration::from_secs(2) {
            let ev =
                notify::Event::new(EventKind::Modify(ModifyKind::Any)).add_path(ignored.clone());
            let _ = ev_tx.send(Ok(ev));
            if matches!(
                out_rx.recv_timeout(Duration::from_millis(10)),
                Ok(WorkerMsg::Refresh)
            ) {
                refreshed = true;
                break;
            }
        }
        assert!(refreshed, "ignored event stream starved safety poll");

        drop(ev_tx);
        handle.join().unwrap();
    }

    #[test]
    fn rescan_event_triggers_refresh() {
        let (_d, repo) = git_repo();
        let (ev_tx, ev_rx) = mpsc::channel();
        let (out_tx, out_rx) = mpsc::channel();
        let repo_thread = repo.clone();
        let handle = std::thread::spawn(move || {
            watch_loop(
                repo_thread,
                ev_rx,
                out_tx,
                Weak::new(),
                Mode::Recursive,
                Duration::from_secs(60),
            )
        });

        let ev = notify::Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan);
        ev_tx.send(Ok(ev)).unwrap();
        assert!(matches!(
            out_rx.recv_timeout(Duration::from_secs(2)),
            Ok(WorkerMsg::Refresh)
        ));

        drop(ev_tx);
        handle.join().unwrap();
    }
}
