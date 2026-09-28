#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(unix)]
use std::time::{Duration, Instant};

/// A throwaway repository driven by the real git CLI, isolated from the
/// user's global and system config.
pub struct TestRepo {
    pub dir: tempfile::TempDir,
    name: String,
}

fn git_cmd(cwd: &Path) -> Command {
    let mut c = Command::new("git");
    c.current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
            "-c",
            "advice.detachedHead=false",
        ]);
    c
}

impl TestRepo {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let out = git_cmd(dir.path())
            .args(["init", "-q", "-b", "main", "w"])
            .output()
            .unwrap();
        assert!(out.status.success());
        TestRepo {
            dir,
            name: "w".into(),
        }
    }

    /// Clones `remote` into a sibling working copy.
    pub fn clone_from(remote: &Path) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let out = git_cmd(dir.path())
            .args(["clone", "-q", remote.to_str().unwrap(), "w"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        TestRepo {
            dir,
            name: "w".into(),
        }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.path().join(&self.name)
    }

    pub fn try_git(&self, args: &[&str]) -> Result<String, String> {
        let out = git_cmd(&self.path()).args(args).output().unwrap();
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).into_owned())
        }
    }

    pub fn git(&self, args: &[&str]) -> String {
        self.try_git(args)
            .unwrap_or_else(|e| panic!("git {args:?}: {e}"))
    }

    pub fn write(&self, rel: &str, content: &str) {
        let p = self.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    pub fn commit_file(&self, rel: &str, content: &str, msg: &str) {
        self.write(rel, content);
        self.git(&["add", rel]);
        self.git(&["commit", "-q", "-m", msg]);
    }

    /// Creates `remote.git` next to the working copy, adds it as `origin`
    /// and pushes `main` with upstream tracking.
    pub fn with_bare_remote(&self) -> PathBuf {
        let remote = self.dir.path().join("remote.git");
        let out = git_cmd(self.dir.path())
            .args(["init", "-q", "--bare", "remote.git"])
            .output()
            .unwrap();
        assert!(out.status.success());
        self.git(&["remote", "add", "origin", remote.to_str().unwrap()]);
        self.git(&["push", "-q", "-u", "origin", "main"]);
        remote
    }
}

/// Points `origin` at an ssh transport that writes its pid to the returned
/// file and then hangs.
#[cfg(unix)]
pub fn hanging_remote(r: &TestRepo) -> PathBuf {
    let pid_file = r.dir.path().join("ssh.pid");
    r.git(&["remote", "add", "origin", "ssh://example.invalid/x.git"]);
    let ssh = format!("sh -c 'echo $$ > {}; exec sleep 30' --", pid_file.display());
    r.git(&["config", "core.sshCommand", &ssh]);
    pid_file
}

#[cfg(unix)]
pub fn wait_for_pid(file: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(s) = std::fs::read_to_string(file)
            && s.ends_with('\n')
        {
            return s.trim().to_string();
        }
        assert!(
            Instant::now() < deadline,
            "no pid written to {}",
            file.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
pub fn exits_within(pid: &str, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        let alive = std::process::Command::new("kill")
            .args(["-0", pid])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        if !alive {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
