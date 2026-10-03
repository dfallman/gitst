pub mod cli;
pub mod leakscan;
pub mod parse;

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use crate::model::{DetailData, DetailReq, Snapshot};

pub use cli::CliBackend;
pub use leakscan::LeakScanConfig;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repo {
    pub root: PathBuf,
    pub git_dir: PathBuf,
    pub common_dir: PathBuf,
}

#[derive(Debug)]
pub enum DiscoverError {
    NotARepo,
    /// A bare repository, or inside a `.git` directory.
    NoWorkTree,
    GitMissing,
    Other(String),
}

impl Repo {
    /// Finds the repository containing `path`.
    pub fn discover(path: &Path) -> Result<Repo, DiscoverError> {
        let out = cli::base_command(path)
            .args([
                "rev-parse",
                "--path-format=absolute",
                "--show-toplevel",
                "--git-dir",
                "--git-common-dir",
            ])
            .output()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => DiscoverError::GitMissing,
                _ => DiscoverError::Other(e.to_string()),
            })?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            if err.contains("not a git repository") {
                return Err(DiscoverError::NotARepo);
            }
            if err.contains("must be run in a work tree") {
                return Err(DiscoverError::NoWorkTree);
            }
            return Err(DiscoverError::Other(
                err.lines().next().unwrap_or("").to_string(),
            ));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut lines = text.lines().map(|l| {
            let p = PathBuf::from(l);
            p.canonicalize().unwrap_or(p)
        });
        match (lines.next(), lines.next(), lines.next()) {
            (Some(root), Some(git_dir), Some(common_dir)) => Ok(Repo {
                root,
                git_dir,
                common_dir,
            }),
            _ => Err(DiscoverError::Other("unexpected rev-parse output".into())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitError(pub String);

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    Auth,
    Offline,
    Timeout,
    /// Stopped because gitst is quitting.
    Cancelled,
    Other(String),
}

impl FetchError {
    /// Short reason shown on the warning line.
    pub fn label(&self) -> String {
        match self {
            FetchError::Auth => "auth".into(),
            FetchError::Offline => "offline".into(),
            FetchError::Timeout => "timeout".into(),
            FetchError::Cancelled => "cancelled".into(),
            FetchError::Other(m) => m.clone(),
        }
    }
}

/// Classifies `git fetch` stderr (produced under `LC_ALL=C`).
pub fn classify_fetch_stderr(stderr: &str) -> FetchError {
    const AUTH: &[&str] = &[
        "Authentication failed",
        "Permission denied",
        "could not read Username",
        "could not read Password",
        "terminal prompts disabled",
        "Host key verification failed",
    ];
    // Checked before OFFLINE: curl reports a timeout as
    // "Failed to connect … Operation timed out".
    const TIMEOUT: &[&str] = &["Connection timed out", "Operation timed out"];
    const OFFLINE: &[&str] = &[
        "Could not resolve host",
        "Network is unreachable",
        "Connection refused",
        "Failed to connect",
    ];
    if AUTH.iter().any(|p| stderr.contains(p)) {
        FetchError::Auth
    } else if TIMEOUT.iter().any(|p| stderr.contains(p)) {
        FetchError::Timeout
    } else if OFFLINE.iter().any(|p| stderr.contains(p)) {
        FetchError::Offline
    } else {
        let line = stderr
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("fetch failed");
        FetchError::Other(
            line.trim_start_matches("fatal: ")
                .trim_start_matches("error: ")
                .to_string(),
        )
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SnapshotOpts {
    pub max_changes: usize,
    pub numstat_max_files: usize,
    pub commits: usize,
}

/// Everything gitst reads from a repository goes through this trait.
pub trait GitBackend: Send + Sync {
    fn repo(&self) -> &Repo;
    fn snapshot(&self, opts: &SnapshotOpts) -> Result<Snapshot, GitError>;
    fn detail(&self, req: &DetailReq) -> Result<DetailData, GitError>;
    /// Runs `git fetch`, stopping it after `timeout` or once `cancel` is set.
    fn fetch(&self, prune: bool, timeout: Duration, cancel: &AtomicBool) -> Result<(), FetchError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_stderr() {
        assert_eq!(
            classify_fetch_stderr(
                "fatal: could not read Username for 'https://github.com': terminal prompts disabled\n"
            ),
            FetchError::Auth
        );
        assert_eq!(
            classify_fetch_stderr(
                "git@github.com: Permission denied (publickey).\nfatal: Could not read from remote repository.\n"
            ),
            FetchError::Auth
        );
        assert_eq!(
            classify_fetch_stderr(
                "ssh: Could not resolve hostname x: nodename nor servname provided\n"
            ),
            FetchError::Offline
        );
        assert_eq!(
            classify_fetch_stderr(
                "fatal: unable to access 'https://x/': Could not resolve host: x\n"
            ),
            FetchError::Offline
        );
        assert_eq!(
            classify_fetch_stderr(
                "ssh: connect to host example.com port 22: Connection timed out\nfatal: Could not read from remote repository.\n"
            ),
            FetchError::Timeout
        );
        assert_eq!(
            classify_fetch_stderr(
                "fatal: unable to access 'https://x/': Failed to connect to x port 443 after 75003 ms: Operation timed out\n"
            ),
            FetchError::Timeout
        );
        assert_eq!(
            classify_fetch_stderr(
                "fatal: unable to access 'https://x/': Failed to connect to x port 443 after 3 ms: Couldn't connect to server\n"
            ),
            FetchError::Offline
        );
        assert_eq!(
            classify_fetch_stderr(
                "fatal: '/nope' does not appear to be a git repository\nfatal: Could not read from remote repository.\n"
            ),
            FetchError::Other("'/nope' does not appear to be a git repository".into())
        );
    }
}
