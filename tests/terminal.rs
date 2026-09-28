//! Runs the gitst binary in a pseudo-terminal and checks how it leaves the
//! terminal and its fetch when stopped by keys and signals.
#![cfg(unix)]

mod common;

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{TestRepo, exits_within, hanging_remote, wait_for_pid};

const ENTER_ALT: &str = "\x1b[?1049h";
const LEAVE_ALT: &str = "\x1b[?1049l";

/// gitst running on the slave side of a pty, with everything it writes
/// collected from the master side.
struct Tui {
    child: Child,
    master: File,
    output: Arc<Mutex<Vec<u8>>>,
    _home: tempfile::TempDir,
}

impl Tui {
    fn start(cwd: &Path) -> Tui {
        let (mut master, mut slave) = (0, 0);
        let mut size = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let made = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &raw mut size,
            )
        };
        assert_eq!(made, 0, "openpty");
        let master = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { OwnedFd::from_raw_fd(slave) };
        // A home without a gitst config, so the defaults apply.
        let home = tempfile::tempdir().unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_gitst"));
        cmd.current_dir(cwd)
            .env("HOME", home.path())
            .env_remove("XDG_CONFIG_HOME")
            .env("TERM", "xterm-256color")
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave);
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                libc::ioctl(0, libc::TIOCSCTTY as _, 0);
                Ok(())
            });
        }
        let child = cmd.spawn().unwrap();
        let output = Arc::new(Mutex::new(Vec::new()));
        let sink = output.clone();
        let mut reader = master.try_clone().unwrap();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        let tui = Tui {
            child,
            master,
            output,
            _home: home,
        };
        tui.wait_for(|out| out.contains(ENTER_ALT), "the TUI to start");
        tui
    }

    fn output(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }

    fn wait_for(&self, done: impl Fn(&str) -> bool, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done(&self.output()) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn signal(&self, sig: libc::c_int) {
        unsafe {
            libc::kill(self.child.id() as i32, sig);
        }
    }

    fn type_bytes(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).unwrap();
    }

    fn exits_within(&mut self, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if self.child.try_wait().unwrap().is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// Whether the terminal was left in the normal screen, which is where
    /// the TUI returns it on exit.
    fn left_normal_screen(&self) -> bool {
        let out = self.output();
        out.rfind(LEAVE_ALT) > out.rfind(ENTER_ALT)
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn repo() -> TestRepo {
    let r = TestRepo::new();
    r.commit_file("a", "1", "one");
    r
}

#[test]
fn hangup_stops_the_fetch_and_exits() {
    let r = repo();
    // With no FETCH_HEAD, the first background fetch starts at once.
    let pid_file = hanging_remote(&r);
    let mut tui = Tui::start(&r.path());
    let pid = wait_for_pid(&pid_file);
    tui.signal(libc::SIGHUP);
    assert!(
        tui.exits_within(Duration::from_secs(5)),
        "gitst ignored SIGHUP"
    );
    assert!(
        exits_within(&pid, Duration::from_secs(2)),
        "fetch transport {pid} outlived gitst"
    );
}

#[test]
fn term_and_int_restore_the_terminal() {
    for sig in [libc::SIGTERM, libc::SIGINT] {
        let mut tui = Tui::start(&repo().path());
        tui.signal(sig);
        assert!(tui.exits_within(Duration::from_secs(5)), "signal {sig}");
        assert!(tui.left_normal_screen(), "signal {sig}: {:?}", tui.output());
    }
}

#[test]
fn ctrl_z_hands_back_the_terminal_and_resumes() {
    let r = repo();
    let mut tui = Tui::start(&r.path());
    let entered = tui.output().matches(ENTER_ALT).count();
    tui.type_bytes(b"\x1a");
    tui.wait_for(|out| out.matches(LEAVE_ALT).count() >= 1, "Ctrl-Z to leave");
    // The test is not a job-control shell, so the stop may be ignored or
    // may need a SIGCONT, as `fg` would send.
    tui.signal(libc::SIGCONT);
    tui.wait_for(
        |out| out.matches(ENTER_ALT).count() > entered,
        "gitst to take the terminal back",
    );
    tui.type_bytes(b"q");
    assert!(tui.exits_within(Duration::from_secs(5)));
    assert!(tui.left_normal_screen());
}

#[test]
fn the_spinner_keeps_turning_while_the_mouse_moves() {
    let r = repo();
    // A background fetch that never finishes keeps the spinner up.
    let pid_file = hanging_remote(&r);
    let mut tui = Tui::start(&r.path());
    wait_for_pid(&pid_file);
    std::thread::sleep(Duration::from_millis(300));
    let before = tui.output().len();
    let start = Instant::now();
    let mut x = 10;
    while start.elapsed() < Duration::from_millis(1500) {
        // Pointer moves (SGR mouse reports) faster than the spinner turns.
        x = if x == 10 { 11 } else { 10 };
        tui.type_bytes(format!("\x1b[<35;{x};20M").as_bytes());
        std::thread::sleep(Duration::from_millis(20));
    }
    let frames = tui.output()[before..]
        .chars()
        .filter(|c| SPINNER.contains(*c))
        .count();
    assert!(frames >= 5, "the spinner turned {frames} times in 1.5 s");
}

const SPINNER: &str = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏";
