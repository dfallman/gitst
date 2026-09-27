pub mod cli;
pub mod parse;

use std::path::{Path, PathBuf};

use crate::model::{DetailData, DetailReq, Snapshot};

pub use cli::CliBackend;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repo {
    pub root: PathBuf,
    pub git_dir: PathBuf,
    pub common_dir: PathBuf,
}

#[derive(Debug)]
pub enum DiscoverError {
    NotARepo,
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
            if err.contains("not a git repository") || err.contains("must be run in a work tree") {
                return Err(DiscoverError::NotARepo);
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
}
