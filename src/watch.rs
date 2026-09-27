//! Filesystem watching: turns relevant file events into refresh signals.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::{EventKind, RecursiveMode, Watcher};

use crate::git::Repo;
use crate::worker::WorkerMsg;

/// Keeps the underlying watcher alive; dropping it stops watching.
pub struct WatchHandle {
    _watcher: notify::RecommendedWatcher,
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
}

/// Quiet period that ends a burst of file events.
const DEBOUNCE: Duration = Duration::from_millis(150);
/// A continuous burst still refreshes at least this often.
const MAX_BURST: Duration = Duration::from_secs(1);

impl Relevance {
    pub fn new(repo: &Repo) -> Self {
        // Both matchers are rooted at the worktree so its paths can be matched.
        let (global, _) = GitignoreBuilder::new(&repo.root).build_global();
        let mut exclude = GitignoreBuilder::new(&repo.root);
        exclude.add(repo.common_dir.join("info/exclude"));
        let base = vec![
            global,
            exclude.build().unwrap_or_else(|_| Gitignore::empty()),
        ];
        Relevance {
            root: repo.root.clone(),
            git_dir: repo.git_dir.clone(),
            common_dir: repo.common_dir.clone(),
            ignores: HashMap::new(),
            base,
        }
    }

    pub fn is_relevant(&mut self, path: &Path) -> bool {
        for dir in [&self.git_dir, &self.common_dir] {
            if let Ok(rel) = path.strip_prefix(dir) {
                return git_internal_relevant(rel);
            }
        }
        let Ok(rel) = path.strip_prefix(&self.root) else {
            return false;
        };
        if rel.components().any(|c| c.as_os_str() == ".git") {
            return false;
        }
        if path.file_name().is_some_and(|n| n == ".gitignore") {
            self.ignores.clear();
            return true;
        }
        !self.ignored(path)
    }

    /// Applies ignore rules from lowest to highest precedence; the last
    /// matching rule wins, so a deeper `!pattern` can re-include a path.
    fn ignored(&mut self, path: &Path) -> bool {
        let is_dir = path.is_dir();
        let mut ignored = false;
        let mut apply = |m: ignore::Match<&ignore::gitignore::Glob>| {
            if m.is_ignore() {
                ignored = true;
            } else if m.is_whitelist() {
                ignored = false;
            }
        };
        for g in &self.base {
            apply(g.matched_path_or_any_parents(path, is_dir));
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
                apply(m.matched_path_or_any_parents(path, is_dir));
            }
        }
        ignored
    }
}

/// Whether a path inside the git directory reflects a change in repo state.
pub fn git_internal_relevant(rel: &Path) -> bool {
    let s = rel.to_string_lossy();
    matches!(
        s.as_ref(),
        "HEAD" | "index" | "index.lock" | "packed-refs" | "BISECT_LOG"
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

fn watcher(
    tx: Sender<notify::Result<notify::Event>>,
) -> notify::Result<notify::RecommendedWatcher> {
    notify::recommended_watcher(tx)
}

/// Watches the repository and sends one `Refresh` per burst of relevant events.
pub fn spawn(repo: &Repo, out: Sender<WorkerMsg>) -> notify::Result<WatchHandle> {
    let (tx, rx) = mpsc::channel();
    let mut w = watcher(tx)?;
    w.watch(&repo.root, RecursiveMode::Recursive)?;
    if !repo.git_dir.starts_with(&repo.root) {
        w.watch(&repo.git_dir, RecursiveMode::Recursive)?;
    }
    if !repo.common_dir.starts_with(&repo.root) && !repo.common_dir.starts_with(&repo.git_dir) {
        w.watch(&repo.common_dir, RecursiveMode::Recursive)?;
    }
    let mut relevance = Relevance::new(repo);
    std::thread::Builder::new()
        .name("gitst-watch".into())
        .spawn(move || {
            let mut relevant = |ev: &notify::Result<notify::Event>| match ev {
                Ok(ev) => {
                    !matches!(ev.kind, EventKind::Access(_))
                        && ev.paths.iter().any(|p| relevance.is_relevant(p))
                }
                Err(_) => true,
            };
            while let Ok(ev) = rx.recv() {
                if !relevant(&ev) {
                    continue;
                }
                let start = Instant::now();
                while start.elapsed() < MAX_BURST {
                    match rx.recv_timeout(DEBOUNCE) {
                        Ok(_) => {}
                        Err(RecvTimeoutError::Timeout) => break,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
                if out.send(WorkerMsg::Refresh).is_err() {
                    return;
                }
            }
        })
        .map_err(|e| notify::Error::generic(&e.to_string()))?;
    Ok(WatchHandle { _watcher: w })
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
    Ok(WatchHandle { _watcher: w })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        ] {
            assert!(git_internal_relevant(Path::new(p)), "{p}");
        }
        for p in [
            "objects/ab/cdef",
            "description",
            "hooks/pre-commit",
            "COMMIT_EDITMSG",
        ] {
            assert!(!git_internal_relevant(Path::new(p)), "{p}");
        }
    }
}
