use std::io::stdout;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers};
use crossterm::execute;
use ratatui::DefaultTerminal;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use gitst::app::{App, Cmd};
use gitst::config::{Config, parse_duration};
use gitst::git::{CliBackend, DiscoverError, GitBackend, Repo, SnapshotOpts};
use gitst::ui::{self, theme::Theme};
use gitst::watch;
use gitst::worker::{self, FetchStatus, UiMsg, WorkerConfig, WorkerMsg};

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
    /// Turn off background fetching (the ↻ button and `f` still fetch)
    #[arg(long)]
    no_fetch: bool,
    /// Time between background fetches, such as 30s, 5m or 1h
    #[arg(long, value_parser = |s: &str| parse_duration(s).ok_or("expected a duration like 30s, 5m or 1h"))]
    interval: Option<Duration>,
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
    execute!(stdout(), EnableMouseCapture)?;
    let restore = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(stdout(), DisableMouseCapture);
        restore(info);
    }));

    let result = run(&mut terminal, &path, &cfg, interval, warning, &theme);

    let _ = execute!(stdout(), DisableMouseCapture);
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
) -> Result<()> {
    let (ui_tx, ui_rx) = mpsc::channel();
    let input_tx = ui_tx.clone();
    std::thread::Builder::new()
        .name("gitst-input".into())
        .spawn(move || {
            while let Ok(ev) = crossterm::event::read() {
                if input_tx.send(UiMsg::Input(ev)).is_err() {
                    break;
                }
            }
        })?;

    let Some(repo) = wait_for_repo(terminal, &ui_rx, path, theme)? else {
        return Ok(());
    };

    let backend: Arc<dyn GitBackend> = Arc::new(CliBackend::new(repo.clone()));
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
    app.config_warning = warning;
    // Without a watcher, fall back to refreshing every few seconds.
    let (_watch, poll) = match watch::spawn(&repo, worker.clone()) {
        Ok(h) => (Some(h), None),
        Err(e) => {
            app.config_warning = Some(format!("file watching failed ({e}); polling"));
            (None, Some(Duration::from_secs(3)))
        }
    };

    loop {
        app.now = SystemTime::now();
        terminal.draw(|f| ui::draw(f, &mut app, theme))?;
        let wait = poll.map_or(app.next_wakeup(), |p| p.min(app.next_wakeup()));
        let first = match ui_rx.recv_timeout(wait) {
            Ok(m) => Some(m),
            Err(RecvTimeoutError::Timeout) => {
                if poll.is_some() {
                    let _ = worker.send(WorkerMsg::Refresh);
                }
                None
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let mut quit = false;
        for msg in first
            .into_iter()
            .chain(std::iter::from_fn(|| ui_rx.try_recv().ok()))
        {
            for cmd in app.handle(msg) {
                match cmd {
                    Cmd::Quit => quit = true,
                    Cmd::Worker(m) => {
                        let _ = worker.send(m);
                    }
                }
            }
        }
        if quit {
            let _ = worker.send(WorkerMsg::Shutdown);
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
    path: &Path,
    theme: &Theme,
) -> Result<Option<Repo>> {
    let (dir_tx, dir_rx) = mpsc::channel();
    let mut _dir_watch = None;
    loop {
        let message = match Repo::discover(path) {
            Ok(repo) => return Ok(Some(repo)),
            Err(DiscoverError::NotARepo) => "not a git repository",
            Err(DiscoverError::GitMissing) => "git not found on PATH",
            Err(DiscoverError::Other(_)) => "cannot read repository",
        };
        if _dir_watch.is_none() {
            _dir_watch = watch::spawn_dir(path, dir_tx.clone()).ok();
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
            Err(RecvTimeoutError::Disconnected) => return Ok(None),
            _ => {}
        }
        while dir_rx.try_recv().is_ok() {}
    }
}
