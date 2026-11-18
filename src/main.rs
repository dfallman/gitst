use std::io::stdout;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyCode, KeyModifiers,
};
use crossterm::execute;
use ratatui::DefaultTerminal;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use gitst::app::{App, Cmd};
use gitst::config::{Config, parse_duration};
use gitst::git::{CliBackend, DiscoverError, GitBackend, LeakScanConfig, Repo, SnapshotOpts};
use gitst::ui::{self, theme::Theme};
use gitst::watch;
use gitst::worker::{self, FetchStatus, UiMsg, WorkerConfig, WorkerMsg};
use gitst::write::{WriteError, WriteOutcome};

/// Set while `git commit -e` has the terminal, so that the input thread
/// leaves the keys to the editor.
static EDITOR_RUNNING: AtomicBool = AtomicBool::new(false);

/// How long the input thread waits for an event before it checks the flag.
const INPUT_POLL: Duration = Duration::from_millis(50);

/// Commits loaded for the Commits section.
const COMMITS: usize = 50;

#[derive(Parser)]
#[command(
    version,
    about = "Live, glanceable git status for small terminal panes"
)]
struct Cli {
    /// Directory inside the repository to show (default: current directory)
    path: Option<PathBuf>,
    /// Turn off background fetching (the ↻ button and `f` still fetch);
    /// wins over --interval
    #[arg(long)]
    no_fetch: bool,
    /// Time between background fetches, such as 30s, 5m or 1h
    #[arg(long, value_parser = |s: &str| parse_duration(s).ok_or("expected a duration like 30s, 5m or 1h"))]
    interval: Option<Duration>,
    /// Allow commit, push, pull, stash, and staging from gitst
    #[arg(short = 'w', long)]
    write: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let path = match cli.path {
        Some(p) => p,
        None => std::env::current_dir().context("current directory")?,
    };
    let (cfg, warning) = Config::load();
    let mut interval = cli.interval.unwrap_or_else(|| cfg.fetch_interval());
    if cli.no_fetch {
        interval = Duration::ZERO;
    }
    // Asking the terminal for its colours must happen before raw mode.
    let theme = Theme::detect(cfg.icons == "nerd");

    let mut terminal = ratatui::init();
    // Bracketed paste keeps a pasted newline from arriving as Enter.
    if let Err(e) = execute!(stdout(), EnableMouseCapture, EnableBracketedPaste) {
        let _ = execute!(stdout(), DisableMouseCapture, DisableBracketedPaste);
        ratatui::restore();
        return Err(e).context("enable mouse capture");
    }
    let restore = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(stdout(), DisableMouseCapture, DisableBracketedPaste);
        restore(info);
    }));

    let result = run(
        &mut terminal,
        &path,
        &cfg,
        interval,
        warning,
        &theme,
        cli.write,
    );

    let _ = execute!(stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    result
}

fn run(
    terminal: &mut DefaultTerminal,
    path: &Path,
    cfg: &Config,
    interval: Duration,
    warning: Option<String>,
    theme: &Theme,
    write: bool,
) -> Result<()> {
    let (ui_tx, ui_rx) = mpsc::channel();
    forward_signals(ui_tx.clone())?;
    let input_tx = ui_tx.clone();
    let poll = input_poll(write || cfg.write);
    std::thread::Builder::new()
        .name("gitst-input".into())
        .spawn(move || {
            loop {
                if let Some(wait) = poll {
                    // While an editor runs, its keys are not ours to read.
                    if EDITOR_RUNNING.load(Ordering::SeqCst) {
                        std::thread::sleep(wait);
                        continue;
                    }
                    match crossterm::event::poll(wait) {
                        Ok(true) => {}
                        Ok(false) => continue,
                        Err(_) => break,
                    }
                }
                match crossterm::event::read() {
                    Ok(ev) => {
                        if input_tx.send(UiMsg::Input(ev)).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        })?;

    let Some(repo) = wait_for_repo(terminal, &ui_rx, ui_tx.clone(), path, theme)? else {
        return Ok(());
    };

    let leak_scan = LeakScanConfig {
        enabled: cfg.leak_scan,
        allow: gitst::leaks::allow_matcher(&repo.root, &cfg.leak_allow),
    };
    let backend: Arc<dyn GitBackend> =
        Arc::new(CliBackend::new(repo.clone()).with_leak_scan(leak_scan));
    // Checks the editor path's commit before and after (spec §7.5).
    let pin = CliBackend::new(repo.clone());
    let opts = SnapshotOpts {
        max_changes: cfg.max_changes,
        numstat_max_files: cfg.numstat_max_files,
        commits: COMMITS,
    };
    let worker = worker::spawn(
        backend,
        WorkerConfig {
            opts,
            interval,
            prune: cfg.fetch_prune,
        },
        ui_tx,
    );

    let mut app = App::new(
        cfg,
        FetchStatus {
            enabled: !interval.is_zero(),
            ..FetchStatus::default()
        },
    );
    app.write.enabled |= write;
    app.config_warning = warning;
    app.repo_name = repo
        .root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned());
    // Without a watcher, fall back to refreshing every few seconds.
    let (_watch, poll) = match watch::spawn(&repo, worker.sender()) {
        Ok(h) => (Some(h), None),
        Err(e) => {
            // Added to a config warning, not put in its place.
            let watch = format!("file watching failed ({e}); polling");
            app.config_warning = Some(match app.config_warning.take() {
                Some(w) => format!("{w} · {watch}"),
                None => watch,
            });
            (None, Some(Duration::from_secs(3)))
        }
    };

    // Ages, the spinner and pulses change with time alone, so a redraw is
    // due by this time even if every message until then is one that does
    // not redraw, such as a pointer move.
    // Polling keeps its own time, so redraws for ages, which can come each
    // second, do not refresh any more often.
    let mut due = Instant::now();
    let mut poll_due = poll.map(|p| Instant::now() + p);
    loop {
        if app.take_redraw() {
            app.now = SystemTime::now();
            terminal.draw(|f| ui::draw(f, &mut app, theme))?;
            due = Instant::now() + app.next_wakeup();
        }
        let until = poll_due.map_or(due, |p| p.min(due));
        let first = match ui_rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
            Ok(m) => Some(m),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let now = Instant::now();
        if now >= due {
            app.redraw = true;
        }
        if let (Some(p), Some(at)) = (poll, poll_due)
            && now >= at
        {
            let _ = worker.send(WorkerMsg::Refresh);
            poll_due = Some(now + p);
        }
        let mut quit = false;
        app.now = SystemTime::now();
        for msg in first
            .into_iter()
            .chain(std::iter::from_fn(|| ui_rx.try_recv().ok()))
        {
            for cmd in app.handle(msg) {
                match cmd {
                    Cmd::Quit => quit = true,
                    Cmd::Suspend => suspend(terminal)?,
                    Cmd::Worker(m) => {
                        let _ = worker.send(m);
                    }
                    Cmd::Editor {
                        amend,
                        text,
                        tree,
                        head,
                    } => {
                        let result = editor_commit(&pin, amend, &tree, head.as_deref(), || {
                            run_editor(terminal, &repo.root, amend, &text)
                        })?;
                        app.editor_done(result);
                        let _ = worker.send(WorkerMsg::Refresh);
                    }
                }
            }
        }
        if quit {
            worker.shutdown();
            break;
        }
    }
    Ok(())
}

/// Finds the repository, showing a waiting screen until one appears.
/// Returns `None` if the user quits first.
fn wait_for_repo(
    terminal: &mut DefaultTerminal,
    ui_rx: &Receiver<UiMsg>,
    ui_tx: mpsc::Sender<UiMsg>,
    path: &Path,
    theme: &Theme,
) -> Result<Option<Repo>> {
    let mut _dir_watch = None;
    loop {
        let message = match Repo::discover(path) {
            Ok(repo) => return Ok(Some(repo)),
            Err(DiscoverError::NotARepo) => "not a git repository",
            Err(DiscoverError::NoWorkTree) => "no work tree (bare repository?)",
            Err(DiscoverError::GitMissing) => "git not found on PATH",
            Err(DiscoverError::Other(_)) => "cannot read repository",
        };
        if _dir_watch.is_none() {
            let wake = ui_tx.clone();
            _dir_watch = watch::spawn_dir(path, move || wake.send(UiMsg::Wake).is_ok()).ok();
        }
        let shown = path.display().to_string();
        terminal.draw(|f| {
            let area = f.area();
            let lines = vec![
                Line::from(message)
                    .style(Style::new().add_modifier(Modifier::BOLD))
                    .centered(),
                Line::from(ui::fmt::truncate_left(&shown, area.width as usize))
                    .style(theme.dim)
                    .centered(),
                Line::from("waiting · q quits").style(theme.dim).centered(),
            ];
            let y = area.y + area.height.saturating_sub(3) / 2;
            let r = Rect::new(area.x, y, area.width, 3.min(area.height));
            f.render_widget(Paragraph::new(lines), r);
        })?;
        // Wake on input, on a change in the directory, or every few seconds.
        match ui_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(UiMsg::Input(Event::Key(k)))
                if k.code == KeyCode::Char('q')
                    || (k.code == KeyCode::Char('c')
                        && k.modifiers.contains(KeyModifiers::CONTROL)) =>
            {
                return Ok(None);
            }
            Ok(UiMsg::Input(Event::Key(k)))
                if k.code == KeyCode::Char('z') && k.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                suspend(terminal)?;
            }
            Ok(UiMsg::Suspend) => suspend(terminal)?,
            Ok(UiMsg::Quit) | Err(RecvTimeoutError::Disconnected) => return Ok(None),
            _ => {}
        }
    }
}

/// Turns signals into UI messages, so that a closed terminal or a `kill`
/// quits through the same path as `q`: the fetch is stopped and the
/// terminal restored.
#[cfg(unix)]
fn forward_signals(ui: mpsc::Sender<UiMsg>) -> Result<()> {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGTSTP};
    let mut signals = signal_hook::iterator::Signals::new([SIGHUP, SIGINT, SIGTERM, SIGTSTP])
        .context("install signal handlers")?;
    std::thread::Builder::new()
        .name("gitst-signals".into())
        .spawn(move || {
            for sig in signals.forever() {
                let Some(msg) = signal_msg(sig, EDITOR_RUNNING.load(Ordering::SeqCst)) else {
                    continue;
                };
                if ui.send(msg).is_err() {
                    break;
                }
            }
        })?;
    Ok(())
}

/// What a signal asks of gitst. Ctrl-C and Ctrl-Z while the editor runs
/// are meant for it, or for a hook, so they ask nothing; a closed terminal
/// or a `kill` still quits once it is done.
#[cfg(unix)]
fn signal_msg(sig: i32, editor_running: bool) -> Option<UiMsg> {
    use signal_hook::consts::{SIGINT, SIGTSTP};
    match sig {
        SIGINT | SIGTSTP if editor_running => None,
        SIGTSTP => Some(UiMsg::Suspend),
        _ => Some(UiMsg::Quit),
    }
}

#[cfg(not(unix))]
fn forward_signals(_ui: mpsc::Sender<UiMsg>) -> Result<()> {
    Ok(())
}

/// Hands the terminal back to the shell and stops, as Ctrl-Z does outside
/// raw mode, then takes the terminal again when continued.
#[cfg(unix)]
fn suspend(terminal: &mut DefaultTerminal) -> Result<()> {
    release_terminal(terminal);
    // Stops here until `fg`. A process group without a job-control shell
    // ignores the stop, and this returns at once.
    signal_hook::low_level::emulate_default_handler(signal_hook::consts::SIGTSTP)?;
    retake_terminal(terminal)
}

#[cfg(not(unix))]
fn suspend(_terminal: &mut DefaultTerminal) -> Result<()> {
    Ok(())
}

/// Gives the terminal to the shell: cursor back, mouse and paste capture
/// off, main screen.
fn release_terminal(terminal: &mut DefaultTerminal) {
    let _ = execute!(stdout(), DisableMouseCapture, DisableBracketedPaste);
    let _ = terminal.show_cursor();
    ratatui::restore();
}

/// Takes the terminal back after `release_terminal`.
fn retake_terminal(terminal: &mut DefaultTerminal) -> Result<()> {
    use crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};
    enable_raw_mode()?;
    execute!(
        stdout(),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    // The screen was the shell's meanwhile, so the next draw repaints all.
    terminal.clear()?;
    Ok(())
}

/// How long the input thread waits for an event before it checks whether
/// the editor has the terminal, or `None` to block on the next event: only
/// write mode has an editor path.
fn input_poll(write: bool) -> Option<Duration> {
    write.then_some(INPUT_POLL)
}

/// The arguments of the editor commit, after `git`.
fn editor_args(root: &Path, amend: bool, text: &str) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = vec!["-C".into(), root.into(), "commit".into()];
    if amend {
        args.push("--amend".into());
    }
    args.push("-e".into());
    if !text.is_empty() {
        args.extend(["-m".into(), text.into()]);
    }
    args
}

/// The editor commit (spec §6.3): `launch` runs it only while the index
/// and HEAD are still the `tree` and `head` the confirm pinned, and the
/// commit is checked after like any other (spec §7.5).
fn editor_commit(
    pin: &CliBackend,
    amend: bool,
    tree: &str,
    head: Option<&str>,
    launch: impl FnOnce() -> Result<bool>,
) -> Result<Result<WriteOutcome, WriteError>> {
    if let Err(e) = pin.commit_pin_holds(tree, head) {
        return Ok(Err(e));
    }
    if !launch()? {
        return Ok(Err(WriteError {
            summary: "commit from editor did not complete".into(),
            output: String::new(),
        }));
    }
    Ok(pin.commit_outcome(amend, tree))
}

/// Runs `git commit -e` on the terminal, as the shell would, then takes the
/// terminal back. True when git exits 0.
fn run_editor(
    terminal: &mut DefaultTerminal,
    root: &Path,
    amend: bool,
    text: &str,
) -> Result<bool> {
    EDITOR_RUNNING.store(true, Ordering::SeqCst);
    // Let a poll already under way finish before the editor starts.
    std::thread::sleep(INPUT_POLL * 2);
    release_terminal(terminal);
    let done = std::process::Command::new("git")
        .args(editor_args(root, amend, text))
        .status()
        .is_ok_and(|s| s.success());
    let back = retake_terminal(terminal);
    EDITOR_RUNNING.store(false, Ordering::SeqCst);
    back?;
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A repository with one commit and `a.txt` staged, and the tree and
    /// HEAD a commit confirm would have pinned.
    fn pinned() -> (tempfile::TempDir, CliBackend, String, Option<String>) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q", "-b", "main"]);
        git(p, &["config", "user.name", "T"]);
        git(p, &["config", "user.email", "t@t"]);
        git(p, &["config", "commit.gpgsign", "false"]);
        git(p, &["commit", "-q", "--allow-empty", "-m", "first"]);
        std::fs::write(p.join("a.txt"), "a\n").unwrap();
        git(p, &["add", "a.txt"]);
        let tree = git(p, &["write-tree"]);
        let head = Some(git(p, &["rev-parse", "HEAD"]));
        let backend = CliBackend::new(Repo::discover(p).ok().unwrap());
        (dir, backend, tree, head)
    }

    fn summary(r: Result<WriteOutcome, WriteError>) -> String {
        match r {
            Err(e) => e.summary,
            Ok(o) => panic!("{o:?}"),
        }
    }

    #[test]
    fn editor_does_not_start_when_staged_changes_moved() {
        let (dir, pin, tree, head) = pinned();
        std::fs::write(dir.path().join("b.txt"), "b\n").unwrap();
        git(dir.path(), &["add", "b.txt"]);
        let r = editor_commit(&pin, false, &tree, head.as_deref(), || {
            panic!("the editor started")
        });
        assert_eq!(summary(r.unwrap()), "staged changes moved, review again");
    }

    #[test]
    fn editor_does_not_start_when_head_moved() {
        let (dir, pin, tree, head) = pinned();
        git(
            dir.path(),
            &["commit", "-q", "--allow-empty", "-m", "theirs"],
        );
        let r = editor_commit(&pin, true, &tree, head.as_deref(), || {
            panic!("the editor started")
        });
        assert_eq!(summary(r.unwrap()), "HEAD moved, review again");
    }

    #[test]
    fn editor_commit_is_checked_after() {
        let (dir, pin, tree, head) = pinned();
        let p = dir.path().to_path_buf();
        let r = editor_commit(&pin, false, &tree, head.as_deref(), || {
            // Staged while the editor was open.
            std::fs::write(p.join("b.txt"), "b\n").unwrap();
            git(&p, &["add", "b.txt"]);
            git(&p, &["commit", "-q", "-m", "x"]);
            Ok(true)
        });
        match r.unwrap() {
            Ok(WriteOutcome::Done { summary, warning }) => {
                assert!(summary.starts_with("committed "), "{summary}");
                assert_eq!(
                    warning.as_deref(),
                    Some("commit includes changes staged after review")
                );
            }
            other => panic!("{other:?}"),
        }
        let (_dir, pin, tree, head) = pinned();
        let r = editor_commit(&pin, false, &tree, head.as_deref(), || Ok(false));
        assert_eq!(summary(r.unwrap()), "commit from editor did not complete");
    }

    #[cfg(unix)]
    #[test]
    fn interrupt_and_stop_during_the_editor_are_the_editors() {
        use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGTSTP};
        assert!(signal_msg(SIGINT, true).is_none());
        assert!(signal_msg(SIGTSTP, true).is_none());
        assert!(matches!(signal_msg(SIGTERM, true), Some(UiMsg::Quit)));
        assert!(matches!(signal_msg(SIGHUP, true), Some(UiMsg::Quit)));
        assert!(matches!(signal_msg(SIGINT, false), Some(UiMsg::Quit)));
        assert!(matches!(signal_msg(SIGTSTP, false), Some(UiMsg::Suspend)));
    }

    #[test]
    fn input_is_polled_only_in_write_mode() {
        assert_eq!(input_poll(false), None);
        assert_eq!(input_poll(true), Some(INPUT_POLL));
    }

    #[test]
    fn editor_args_follow_amend_and_text() {
        let args = |amend, text| -> Vec<String> {
            editor_args(Path::new("/r"), amend, text)
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(
            args(false, "msg"),
            ["-C", "/r", "commit", "-e", "-m", "msg"]
        );
        assert_eq!(args(true, ""), ["-C", "/r", "commit", "--amend", "-e"]);
        assert_eq!(
            args(true, "s"),
            ["-C", "/r", "commit", "--amend", "-e", "-m", "s"]
        );
    }
}
