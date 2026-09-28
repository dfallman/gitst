//! Filesystem watching: turns relevant file events into refresh signals.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};

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

/// How the work tree is watched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// One recursive watch. FSEvents and ReadDirectoryChangesW watch a whole
    /// tree with one handle, so ignored directories cost nothing to watch,
    /// and `Relevance` drops their events cheaply.
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
    // A submodule's git directory is `modules/<name>/`, and the name may
    // contain slashes, so any tail of the path may be the part that counts.
    if let Ok(sub) = rel.strip_prefix("modules") {
        let comps: Vec<_> = sub.components().collect();
        return (1..comps.len()).any(|i| {
            let tail: PathBuf = comps[i..].iter().collect();
            own_git_path_relevant(&tail)
        });
    }
    own_git_path_relevant(rel)
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
            "refs/",
            "logs/",
            "rebase-merge",
            "rebase-apply",
            "sequencer",
        ]
        .iter()
        .any(|p| s.starts_with(p))
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
    let root = repo.root.clone();
    let mut relevance = Relevance::new(repo);
    std::thread::Builder::new()
        .name("gitst-watch".into())
        .spawn(move || {
            // Every event is looked at, even inside a burst, so that no new
            // directory goes unwatched. Returns whether the event is
            // relevant, and whether `.gitignore` changed.
            let mut on_event = |ev: notify::Result<notify::Event>| -> (bool, bool) {
                let Ok(ev) = ev else {
                    return (true, false);
                };
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
                    follow_new_dirs(&weak, &root, &ev, &relevant);
                }
                (!relevant.is_empty(), gitignore)
            };
            while let Ok(ev) = rx.recv() {
                let (relevant, mut rewalk) = on_event(ev);
                if !relevant {
                    continue;
                }
                let start = Instant::now();
                while start.elapsed() < MAX_BURST {
                    match rx.recv_timeout(DEBOUNCE) {
                        Ok(ev) => rewalk |= on_event(ev).1,
                        Err(RecvTimeoutError::Timeout) => break,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
                // After `.gitignore` edits, once per burst: directories no
                // longer ignored need watches. (Newly ignored ones keep
                // theirs until restart; their events are dropped.)
                if mode == Mode::PerDirectory
                    && rewalk
                    && let Some(w) = weak.upgrade()
                {
                    let mut w = w.lock().unwrap_or_else(PoisonError::into_inner);
                    let _ = watch_unignored(&mut w, &root);
                }
                if out.send(WorkerMsg::Refresh).is_err() {
                    return;
                }
            }
        })
        .map_err(|e| notify::Error::generic(&e.to_string()))?;
    Ok(WatchHandle { _watcher: watcher })
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

/// Watches one directory (not recursively) and signals on any change. Used
/// while waiting for a repository to appear.
pub fn spawn_dir(dir: &Path, out: Sender<()>) -> notify::Result<WatchHandle> {
    let (tx, rx) = mpsc::channel::<notify::Result<notify::Event>>();
    let mut w = watcher(tx)?;
    w.watch(dir, RecursiveMode::NonRecursive)?;
    std::thread::spawn(move || {
        while rx.recv().is_ok() {
            while rx.recv_timeout(DEBOUNCE).is_ok() {}
            if out.send(()).is_err() {
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
            "HEAD",
            "index",
            "index.lock",
            "refs/heads/main",
            "logs/HEAD",
            "MERGE_HEAD",
            "FETCH_HEAD",
            "rebase-merge/msgnum",
            "packed-refs",
            "BISECT_LOG",
            "config",
            "info/exclude",
            // Submodules' git directories, whose names may contain slashes.
            "modules/sub/HEAD",
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
            r"info\exclude",
            r"rebase-merge\msgnum",
            r"modules\libs\foo\refs\heads\main",
        ] {
            assert!(git_internal_relevant(Path::new(p)), "{p}");
        }
        for p in [
            "objects/ab/cdef",
            "modules/sub/objects/ab/cdef",
            "description",
            "hooks/pre-commit",
            "COMMIT_EDITMSG",
        ] {
            assert!(!git_internal_relevant(Path::new(p)), "{p}");
        }
    }
}
