#![allow(dead_code)]

use std::io::Read;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

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

/// A remote that takes a fetch's connection and never answers, so the
/// fetch hangs until it is stopped. Seeing the connection close shows that
/// the process holding it, git's HTTP transport, has exited.
pub struct HangingRemote {
    events: Receiver<Conn>,
}

#[derive(Debug, PartialEq, Eq)]
enum Conn {
    Opened,
    Closed,
}

impl HangingRemote {
    /// Starts the remote and adds it to `r` as `origin`.
    pub fn add_to(r: &TestRepo) -> HangingRemote {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/x.git", listener.local_addr().unwrap());
        r.git(&["remote", "add", "origin", &url]);
        let (tx, events) = channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                if tx.send(Conn::Opened).is_err() {
                    break;
                }
                let tx = tx.clone();
                std::thread::spawn(move || {
                    let mut buf = [0u8; 4096];
                    while matches!(stream.read(&mut buf), Ok(n) if n > 0) {}
                    let _ = tx.send(Conn::Closed);
                });
            }
        });
        HangingRemote { events }
    }

    /// Waits for a fetch to connect.
    pub fn wait_for_fetch(&self) {
        let got = self.events.recv_timeout(Duration::from_secs(10));
        assert_eq!(got, Ok(Conn::Opened), "no fetch connected");
    }

    /// Whether the connection closes within `limit`.
    pub fn closes_within(&self, limit: Duration) -> bool {
        self.events.recv_timeout(limit) == Ok(Conn::Closed)
    }
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
