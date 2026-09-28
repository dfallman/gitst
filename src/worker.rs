//! The git worker thread: refreshes snapshots, loads details, schedules fetches.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SendError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::activity::{ActivityEvent, ActivityKind, diff_snapshots};
use crate::git::{FetchError, GitBackend, SnapshotOpts};
use crate::model::{DetailData, DetailReq, Snapshot};

pub enum WorkerMsg {
    Refresh,
    Fetch {
        manual: bool,
    },
    FetchDone {
        manual: bool,
        result: Result<(), FetchError>,
    },
    Detail(DetailReq),
    Shutdown,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct FetchStatus {
    pub running: bool,
    pub last_error: Option<FetchError>,
    pub failures: u32,
    /// Whether the fetch timer is on.
    pub enabled: bool,
}

pub enum UiMsg {
    Input(crossterm::event::Event),
    Snapshot {
        snap: Arc<Snapshot>,
        events: Vec<ActivityEvent>,
        changed: Vec<String>,
    },
    RefreshError(String),
    Fetch(FetchStatus),
    Detail(DetailReq, Result<DetailData, String>),
    Live(ActivityEvent),
}

#[derive(Clone, Copy, Debug)]
pub struct WorkerConfig {
    pub opts: SnapshotOpts,
    pub interval: Duration,
    pub prune: bool,
}

/// Longest delay between timed fetches, however many have failed.
const MAX_BACKOFF: Duration = Duration::from_secs(60 * 60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);

/// Delay before the next timed fetch after `failures` consecutive failures:
/// the interval doubles twice, then jumps to the cap (5 → 10 → 20 → 60 min).
pub fn backoff(interval: Duration, failures: u32) -> Duration {
    let cap = MAX_BACKOFF.max(interval);
    match failures {
        0..=2 => interval.saturating_mul(1 << failures).min(cap),
        _ => cap,
    }
}

/// The fetch in flight, shared with the handle so that shutdown can stop it
/// even while the worker thread is busy.
#[derive(Default)]
struct FetchSlot {
    cancel: AtomicBool,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl FetchSlot {
    fn thread(&self) -> MutexGuard<'_, Option<JoinHandle<()>>> {
        self.thread.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The running worker: its inbox, and the means to stop it. Dropping it
/// shuts the worker down.
pub struct WorkerHandle {
    tx: Sender<WorkerMsg>,
    fetch: Arc<FetchSlot>,
}

impl WorkerHandle {
    pub fn send(&self, msg: WorkerMsg) -> Result<(), SendError<WorkerMsg>> {
        self.tx.send(msg)
    }

    /// A second sender to the worker's inbox.
    pub fn sender(&self) -> Sender<WorkerMsg> {
        self.tx.clone()
    }

    /// Stops the worker and waits for any running fetch to exit. Fetches
    /// run in their own session, so nothing else would stop them, and a
    /// fetch left running can hold ref locks long after gitst is gone.
    pub fn shutdown(&self) {
        let _ = self.tx.send(WorkerMsg::Shutdown);
        let thread = {
            let mut slot = self.fetch.thread();
            self.fetch.cancel.store(true, Ordering::SeqCst);
            slot.take()
        };
        if let Some(t) = thread {
            let _ = t.join();
        }
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Starts the worker thread.
pub fn spawn(backend: Arc<dyn GitBackend>, cfg: WorkerConfig, ui: Sender<UiMsg>) -> WorkerHandle {
    let (tx, rx) = mpsc::channel();
    let inbox = tx.clone();
    let slot = Arc::new(FetchSlot::default());
    let fetch_slot = slot.clone();
    std::thread::Builder::new()
        .name("gitst-worker".into())
        .spawn(move || {
            let fetch = FetchStatus {
                enabled: !cfg.interval.is_zero(),
                ..FetchStatus::default()
            };
            Worker {
                backend,
                cfg,
                ui,
                inbox,
                slot: fetch_slot,
                prev: None,
                fetch,
                next_fetch: None,
                scheduled: false,
                manual_pending: false,
            }
            .run(rx)
        })
        .expect("spawn worker thread");
    WorkerHandle { tx, fetch: slot }
}

struct Worker {
    backend: Arc<dyn GitBackend>,
    cfg: WorkerConfig,
    ui: Sender<UiMsg>,
    inbox: Sender<WorkerMsg>,
    slot: Arc<FetchSlot>,
    prev: Option<Arc<Snapshot>>,
    fetch: FetchStatus,
    next_fetch: Option<Instant>,
    /// Whether the first timed fetch has been scheduled.
    scheduled: bool,
    /// A manual fetch succeeded and the next refresh should say whether it changed anything.
    manual_pending: bool,
}

impl Worker {
    fn run(mut self, rx: Receiver<WorkerMsg>) {
        self.refresh();
        loop {
            let first = match self.next_fetch {
                Some(due) => match rx.recv_timeout(due.saturating_duration_since(Instant::now())) {
                    Ok(m) => m,
                    Err(RecvTimeoutError::Timeout) => WorkerMsg::Fetch { manual: false },
                    Err(RecvTimeoutError::Disconnected) => return,
                },
                None => match rx.recv() {
                    Ok(m) => m,
                    Err(_) => return,
                },
            };
            let mut refresh = false;
            for msg in std::iter::once(first).chain(std::iter::from_fn(|| rx.try_recv().ok())) {
                match msg {
                    WorkerMsg::Refresh => refresh = true,
                    WorkerMsg::Fetch { manual } => self.start_fetch(manual),
                    WorkerMsg::FetchDone { manual, result } => {
                        self.finish_fetch(manual, result);
                        refresh = true;
                    }
                    WorkerMsg::Detail(req) => {
                        let data = self.backend.detail(&req).map_err(|e| e.0);
                        if self.ui.send(UiMsg::Detail(req, data)).is_err() {
                            return;
                        }
                    }
                    WorkerMsg::Shutdown => return,
                }
            }
            if refresh {
                self.refresh();
            }
        }
    }

    fn refresh(&mut self) {
        match self.backend.snapshot(&self.cfg.opts) {
            Ok(snap) => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs() as i64);
                let (events, changed) = match &self.prev {
                    Some(prev) => diff_snapshots(prev, &snap, now),
                    None => (Vec::new(), Vec::new()),
                };
                if std::mem::take(&mut self.manual_pending)
                    && self.prev.as_ref().is_some_and(|p| p.reflog == snap.reflog)
                {
                    let text = "fetch · up to date".to_string();
                    let _ = self.ui.send(UiMsg::Live(ActivityEvent {
                        time: now,
                        kind: ActivityKind::Fetch,
                        text,
                        rev: None,
                    }));
                }
                self.schedule_first_fetch(&snap);
                let snap = Arc::new(snap);
                self.prev = Some(snap.clone());
                let _ = self.ui.send(UiMsg::Snapshot {
                    snap,
                    events,
                    changed,
                });
            }
            Err(e) => {
                let _ = self.ui.send(UiMsg::RefreshError(e.0));
            }
        }
    }

    /// The first timed fetch is due one interval after the last fetch
    /// (by anyone), or immediately if that is already past.
    fn schedule_first_fetch(&mut self, snap: &Snapshot) {
        if self.scheduled || self.cfg.interval.is_zero() || !snap.has_remote {
            return;
        }
        self.scheduled = true;
        let since = snap
            .last_fetch
            .and_then(|t| t.elapsed().ok())
            .unwrap_or(Duration::MAX);
        self.next_fetch = Some(Instant::now() + self.cfg.interval.saturating_sub(since));
    }

    fn start_fetch(&mut self, manual: bool) {
        let has_remote = self.prev.as_ref().is_some_and(|p| p.has_remote);
        if self.fetch.running || !has_remote {
            if !self.fetch.running {
                self.next_fetch = None;
            }
            return;
        }
        // Holding the slot keeps shutdown from missing a fetch that starts
        // while it runs.
        let mut thread = self.slot.thread();
        if self.slot.cancel.load(Ordering::SeqCst) {
            return;
        }
        self.fetch.running = true;
        self.next_fetch = None;
        let _ = self.ui.send(UiMsg::Fetch(self.fetch.clone()));
        let backend = self.backend.clone();
        let inbox = self.inbox.clone();
        let slot = self.slot.clone();
        let prune = self.cfg.prune;
        *thread = Some(std::thread::spawn(move || {
            let result = backend.fetch(prune, FETCH_TIMEOUT, &slot.cancel);
            let _ = inbox.send(WorkerMsg::FetchDone { manual, result });
        }));
    }

    fn finish_fetch(&mut self, manual: bool, result: Result<(), FetchError>) {
        self.fetch.running = false;
        match result {
            Ok(()) => {
                self.fetch.failures = 0;
                self.fetch.last_error = None;
                self.manual_pending = manual;
            }
            Err(e) => {
                self.fetch.failures += 1;
                self.fetch.last_error = Some(e);
            }
        }
        if !self.cfg.interval.is_zero() {
            self.next_fetch =
                Some(Instant::now() + backoff(self.cfg.interval, self.fetch.failures));
        }
        let _ = self.ui.send(UiMsg::Fetch(self.fetch.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        let m = Duration::from_secs(60);
        assert_eq!(backoff(5 * m, 0), 5 * m);
        assert_eq!(backoff(5 * m, 1), 10 * m);
        assert_eq!(backoff(5 * m, 2), 20 * m);
        assert_eq!(backoff(5 * m, 3), 60 * m);
        assert_eq!(backoff(5 * m, 40), 60 * m);
        assert_eq!(backoff(90 * m, 0), 90 * m);
    }
}
