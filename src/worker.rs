//! Background worker: owns the provider adapters and the process sampler, and
//! speaks a small request/event protocol so the UI thread never touches disk.
//!
//! Scheduling rules (spec §5.4, §13):
//! - `Refresh` requests are coalesced into one flag; a request that arrives
//!   during a pass means "run one more pass afterwards".
//! - A pass is chunked: each `ProviderReader::refresh` call is bounded by a
//!   [`RefreshBudget`], and queued page/source requests are serviced between
//!   chunks.
//! - Only the latest selection matters. Page responses carry the request ID of
//!   the request that produced them and are dropped if a newer selection (or
//!   any later `Select`) arrived while they were being computed.
//! - A provider failure keeps its last good sessions and replaces its warnings
//!   by [`WarningKey`] per generation; recovery clears the key.
//! - `Unavailable` (for example "transcript index is building") is retryable:
//!   the request stays queued and is retried after [`RETRY_DELAY`], never in a
//!   tight loop.

use std::collections::{HashMap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::domain::{
    PageAnchor, PageDirection, PageRequest, Provider, RefreshBudget, ScanProgress, SessionKey,
    SessionSummary, SourceLocator, Timestamp, TranscriptPage, Warning, WarningKey, WarningKind,
};
use crate::processes::{self, ProcessError, ProcessSnapshot};
use crate::providers::{ProviderError, ProviderReader, SourceRecord};

/// Capacity of the worker → UI event queue. The worker blocks (politely, see
/// `emit`) when the UI falls behind.
const EVENT_CAPACITY: usize = 256;
/// Maximum queued page/source loads; the oldest are dropped first.
const MAX_PENDING_LOADS: usize = 16;
/// Delay before a retryable (`Unavailable`) request is attempted again.
const RETRY_DELAY: Duration = Duration::from_millis(250);
/// Poll interval while the event queue is full.
const BACKPRESSURE_POLL: Duration = Duration::from_millis(2);
/// Per-chunk refresh budget handed to each adapter.
const CHUNK_BUDGET: RefreshBudget = RefreshBudget {
    max_records: 256,
    max_bytes: 8 * 1024 * 1024,
    max_millis: 100,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkerRequest {
    Refresh,
    Select {
        request_id: u64,
        key: SessionKey,
    },
    LoadPage {
        request_id: u64,
        key: SessionKey,
        anchor: Option<PageAnchor>,
        direction: PageDirection,
    },
    LoadSource {
        request_id: u64,
        locator: SourceLocator,
    },
    Shutdown,
}

#[derive(Clone, Debug)]
pub enum WorkerEvent {
    Roster {
        generation: u64,
        sessions: Vec<SessionSummary>,
        warnings: Vec<Warning>,
        scan: ScanProgress,
    },
    Transcript {
        request_id: u64,
        page: TranscriptPage,
    },
    /// The selected session changed in place; the visible page must be reloaded.
    /// This is not a new event for new-event counting.
    TranscriptInvalidated {
        key: SessionKey,
    },
    Source {
        request_id: u64,
        record: SourceRecord,
    },
    Processes {
        snapshot: ProcessSnapshot,
    },
    Warning {
        warning: Warning,
    },
    RefreshFinished {
        generation: u64,
    },
}

type Sampler = Box<dyn FnMut(Timestamp) -> Result<ProcessSnapshot, ProcessError> + Send>;

/// Builds provider readers; runs on the worker thread.
pub type ReaderFactory = Box<dyn FnOnce() -> Vec<Box<dyn ProviderReader>> + Send>;

pub struct WorkerHandle {
    pub tx: Sender<WorkerRequest>,
    pub rx: Receiver<WorkerEvent>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl WorkerHandle {
    /// Start a worker that samples real processes (when enabled in `config`).
    pub fn spawn(readers: Vec<Box<dyn ProviderReader>>, config: Config) -> Self {
        Self::spawn_with_sampler(readers, config, Box::new(processes::observe_processes))
    }

    /// Start a worker with an injected process sampler.
    pub fn spawn_with_sampler(
        readers: Vec<Box<dyn ProviderReader>>,
        config: Config,
        sampler: Sampler,
    ) -> Self {
        Self::spawn_inner(Box::new(move || readers), config, sampler)
    }

    /// Start a worker that builds its readers on the worker thread, so slow
    /// provider setup (e.g. opening SQLite) never blocks the caller.
    pub fn spawn_lazy(factory: ReaderFactory, config: Config) -> Self {
        Self::spawn_inner(factory, config, Box::new(processes::observe_processes))
    }

    fn spawn_inner(factory: ReaderFactory, config: Config, sampler: Sampler) -> Self {
        let (tx, req_rx) = mpsc::channel();
        let (ev_tx, rx) = mpsc::sync_channel(EVENT_CAPACITY);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let join = thread::Builder::new()
            .name("agentscope-worker".into())
            .spawn(move || {
                let worker = Worker::new(factory(), config, sampler, req_rx, ev_tx, worker_stop);
                worker.run()
            })
            .expect("spawn worker thread");
        Self {
            tx,
            rx,
            stop,
            join: Some(join),
        }
    }

    /// Ask the worker to stop and wait for it. Returns `false` if the worker
    /// thread had panicked.
    pub fn shutdown(mut self) -> bool {
        self.close()
    }

    fn close(&mut self) -> bool {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.tx.send(WorkerRequest::Shutdown);
        match self.join.take() {
            Some(join) => join.join().is_ok(),
            None => true,
        }
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        self.close();
    }
}

struct PendingLoad {
    kind: LoadKind,
    retry_at: Option<Instant>,
}

enum LoadKind {
    Page {
        request_id: u64,
        key: SessionKey,
        anchor: Option<PageAnchor>,
        direction: PageDirection,
    },
    Source {
        request_id: u64,
        locator: SourceLocator,
    },
}

#[derive(Default)]
struct ProviderState {
    sessions: HashMap<SessionKey, SessionSummary>,
    /// Warnings published for the last completed generation.
    warnings: HashMap<WarningKey, Warning>,
    /// Warnings accumulated during the generation in progress.
    pending: HashMap<WarningKey, Warning>,
    failure: Option<Warning>,
}

struct Worker {
    readers: Vec<Box<dyn ProviderReader>>,
    states: Vec<ProviderState>,
    config: Config,
    sampler: Sampler,
    requests: Receiver<WorkerRequest>,
    events: SyncSender<WorkerEvent>,
    stop: Arc<AtomicBool>,
    shutting_down: bool,
    generation: u64,
    refresh_requested: bool,
    pending: VecDeque<PendingLoad>,
    selected: Option<(u64, SessionKey)>,
    observation: Option<Result<ProcessSnapshot, ProcessError>>,
    last_scan: Option<ScanProgress>,
    process_every: Duration,
    /// Advanced only when a sample is actually taken, never by refresh passes.
    next_process: Instant,
}

fn now_ms() -> Timestamp {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as Timestamp)
}

/// Run an adapter call, converting a panic into an internal error so a faulty
/// adapter cannot take the worker down.
fn guard<T>(f: impl FnOnce() -> Result<T, ProviderError>) -> Result<T, ProviderError> {
    catch_unwind(AssertUnwindSafe(f))
        .unwrap_or_else(|_| Err(ProviderError::Internal("provider adapter panicked".into())))
}

fn warning_kind(err: &ProviderError) -> WarningKind {
    match err {
        ProviderError::Io(_) => WarningKind::Io,
        ProviderError::Parse(_) => WarningKind::Parse,
        ProviderError::Unsupported(_) => WarningKind::Unsupported,
        ProviderError::Unavailable(_) | ProviderError::Internal(_) => WarningKind::Unavailable,
    }
}

fn failure_warning(provider: Provider, source: &str, err: &ProviderError) -> Warning {
    Warning {
        key: WarningKey {
            provider,
            source: source.into(),
            kind: warning_kind(err),
        },
        message: err.to_string(),
        count: 1,
    }
}

impl Worker {
    fn new(
        readers: Vec<Box<dyn ProviderReader>>,
        config: Config,
        sampler: Sampler,
        requests: Receiver<WorkerRequest>,
        events: SyncSender<WorkerEvent>,
        stop: Arc<AtomicBool>,
    ) -> Self {
        let states = readers.iter().map(|_| ProviderState::default()).collect();
        let process_every = Duration::from_millis(config.process_ms.max(1));
        Self {
            readers,
            states,
            config,
            sampler,
            requests,
            events,
            stop,
            shutting_down: false,
            generation: 0,
            refresh_requested: true, // initial refresh
            pending: VecDeque::new(),
            selected: None,
            observation: None,
            last_scan: None,
            process_every,
            next_process: Instant::now() + process_every,
        }
    }

    fn run(mut self) {
        let refresh_every = Duration::from_millis(self.config.refresh_ms.max(1));
        let mut next_refresh = Instant::now() + refresh_every;
        while !self.shutting_down {
            self.drain_requests();
            if self.shutting_down {
                break;
            }
            self.service_pending();
            if self.shutting_down {
                break;
            }
            if self.has_due_load() {
                // Loads queued while servicing (e.g. a selection that arrived
                // during an adapter call) are answered now, not at the next tick.
                continue;
            }
            let now = Instant::now();
            if self.config.process_enabled && now >= self.next_process {
                self.sample_processes();
                self.publish_after_sample();
                continue;
            }
            if !self.refresh_requested && now >= next_refresh {
                self.refresh_requested = true;
            }
            if self.refresh_requested {
                self.run_refresh();
                next_refresh = Instant::now() + refresh_every;
                continue;
            }
            let mut deadline = next_refresh;
            if self.config.process_enabled {
                deadline = deadline.min(self.next_process);
            }
            if let Some(retry) = self.pending.iter().filter_map(|p| p.retry_at).min() {
                deadline = deadline.min(retry);
            }
            let wait = deadline.saturating_duration_since(Instant::now());
            match self.requests.recv_timeout(wait) {
                Ok(req) => self.handle(req),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => self.shutting_down = true,
            }
        }
    }

    /// A queued load that can run now. Servicing always removes it or pushes
    /// its `retry_at` into the future, so this cannot spin.
    fn has_due_load(&self) -> bool {
        let now = Instant::now();
        self.pending
            .iter()
            .any(|p| p.retry_at.is_none_or(|t| t <= now))
    }

    // ---- requests -------------------------------------------------------

    fn drain_requests(&mut self) {
        while !self.shutting_down {
            match self.requests.try_recv() {
                Ok(req) => self.handle(req),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => self.shutting_down = true,
            }
        }
    }

    fn handle(&mut self, req: WorkerRequest) {
        match req {
            WorkerRequest::Refresh => self.refresh_requested = true,
            WorkerRequest::Shutdown => self.shutting_down = true,
            WorkerRequest::Select { request_id, key } => {
                if self
                    .selected
                    .as_ref()
                    .is_some_and(|(id, _)| *id > request_id)
                {
                    return; // older than the current selection
                }
                self.selected = Some((request_id, key.clone()));
                self.pending
                    .retain(|p| !matches!(p.kind, LoadKind::Page { .. }));
                self.push_load(LoadKind::Page {
                    request_id,
                    key,
                    anchor: None,
                    // Transcripts open oldest-first; End/G requests Backward explicitly.
                    direction: PageDirection::Forward,
                });
            }
            WorkerRequest::LoadPage {
                request_id,
                key,
                anchor,
                direction,
            } => {
                if self.page_is_stale(request_id, &key) {
                    return;
                }
                self.push_load(LoadKind::Page {
                    request_id,
                    key,
                    anchor,
                    direction,
                });
            }
            WorkerRequest::LoadSource {
                request_id,
                locator,
            } => self.push_load(LoadKind::Source {
                request_id,
                locator,
            }),
        }
    }

    fn push_load(&mut self, kind: LoadKind) {
        if self.pending.len() >= MAX_PENDING_LOADS {
            self.pending.pop_front();
        }
        self.pending.push_back(PendingLoad {
            kind,
            retry_at: None,
        });
    }

    /// A page load is stale unless its key is the selected session and it is
    /// not older than the latest selection.
    fn page_is_stale(&self, request_id: u64, key: &SessionKey) -> bool {
        match &self.selected {
            Some((sel_id, sel_key)) => request_id < *sel_id || sel_key != key,
            None => true,
        }
    }

    /// Service every due queued load once. Retryable failures are re-queued
    /// with a delay.
    fn service_pending(&mut self) {
        let now = Instant::now();
        let queue = std::mem::take(&mut self.pending);
        let mut keep = VecDeque::new();
        let mut iter = queue.into_iter();
        for load in iter.by_ref() {
            if self.shutting_down {
                keep.push_back(load);
                break;
            }
            if load.retry_at.is_some_and(|t| t > now) {
                keep.push_back(load);
                continue;
            }
            if let Some(retry) = self.service_one(load.kind) {
                keep.push_back(retry);
            }
        }
        keep.extend(iter);
        // Requests handled while servicing were pushed to `self.pending`.
        keep.extend(std::mem::take(&mut self.pending));
        while keep.len() > MAX_PENDING_LOADS {
            keep.pop_front();
        }
        self.pending = keep;
    }

    fn service_one(&mut self, kind: LoadKind) -> Option<PendingLoad> {
        match kind {
            LoadKind::Page {
                request_id,
                key,
                anchor,
                direction,
            } => {
                if self.page_is_stale(request_id, &key) {
                    return None;
                }
                let request = PageRequest {
                    max_events: self.config.max_page_events,
                    max_bytes: self.config.max_page_bytes,
                };
                let result = match self.reader_index(key.provider) {
                    Some(i) => guard(|| {
                        self.readers[i].transcript_page(&key, anchor.clone(), direction, request)
                    }),
                    None => Err(ProviderError::Unsupported("no adapter for provider".into())),
                };
                // Newer requests may have arrived while the adapter ran.
                self.drain_requests();
                if self.shutting_down || self.page_is_stale(request_id, &key) {
                    return None;
                }
                match result {
                    Ok(page) => {
                        self.emit(WorkerEvent::Transcript { request_id, page });
                        None
                    }
                    Err(ProviderError::Unavailable(_)) => Some(PendingLoad {
                        kind: LoadKind::Page {
                            request_id,
                            key,
                            anchor,
                            direction,
                        },
                        retry_at: Some(Instant::now() + RETRY_DELAY),
                    }),
                    Err(err) => {
                        let warning = failure_warning(key.provider, "transcript", &err);
                        self.emit(WorkerEvent::Warning { warning });
                        None
                    }
                }
            }
            LoadKind::Source {
                request_id,
                locator,
            } => {
                let provider = locator.session.provider;
                let result = match self.reader_index(provider) {
                    Some(i) => guard(|| self.readers[i].source_record(&locator)),
                    None => Err(ProviderError::Unsupported("no adapter for provider".into())),
                };
                if self.shutting_down {
                    return None;
                }
                match result {
                    Ok(record) => {
                        self.emit(WorkerEvent::Source { request_id, record });
                        None
                    }
                    Err(ProviderError::Unavailable(_)) => Some(PendingLoad {
                        kind: LoadKind::Source {
                            request_id,
                            locator,
                        },
                        retry_at: Some(Instant::now() + RETRY_DELAY),
                    }),
                    Err(err) => {
                        let warning = failure_warning(provider, "source", &err);
                        self.emit(WorkerEvent::Warning { warning });
                        None
                    }
                }
            }
        }
    }

    fn reader_index(&self, provider: Provider) -> Option<usize> {
        self.readers.iter().position(|r| r.provider() == provider)
    }

    // ---- refresh --------------------------------------------------------

    fn run_refresh(&mut self) {
        self.refresh_requested = false;
        self.generation += 1;
        let generation = self.generation;
        if self.config.process_enabled && self.observation.is_none() {
            self.sample_processes();
        }
        for i in 0..self.readers.len() {
            self.states[i].pending.clear();
            loop {
                if self.shutting_down {
                    return;
                }
                let now = now_ms();
                let provider = self.readers[i].provider();
                let result = guard(|| self.readers[i].refresh(now, CHUNK_BUDGET));
                let scan = match result {
                    Ok(delta) => {
                        let scan = delta.scan;
                        self.apply_delta(i, delta);
                        scan
                    }
                    Err(err) => {
                        let state = &mut self.states[i];
                        // Keep last-good sessions and the previously published
                        // warnings; only the failure warning is (re)set.
                        state.failure = Some(failure_warning(provider, "refresh", &err));
                        state.pending.clear();
                        ScanProgress {
                            provider,
                            scanned: 0,
                            total: None,
                            done: true,
                        }
                    }
                };
                let state = &mut self.states[i];
                if scan.done && state.failure.is_none() {
                    state.warnings = std::mem::take(&mut state.pending);
                }
                self.last_scan = Some(scan);
                self.publish(generation, scan);
                // Checkpoint after every chunk, including a provider's last or
                // failed one, so page/source/shutdown requests never wait for
                // the remaining providers.
                self.checkpoint();
                if scan.done {
                    break;
                }
            }
        }
        self.emit(WorkerEvent::RefreshFinished { generation });
    }

    /// Drain requests and answer every actionable load.
    fn checkpoint(&mut self) {
        self.drain_requests();
        self.service_pending();
        while !self.shutting_down && self.has_due_load() {
            self.service_pending();
        }
    }

    fn apply_delta(&mut self, i: usize, delta: crate::providers::ProviderDelta) {
        let selected = self.selected.as_ref().map(|(_, k)| k.clone());
        let state = &mut self.states[i];
        state.failure = None;
        for key in &delta.removed {
            state.sessions.remove(key);
        }
        for session in delta.sessions {
            state.sessions.insert(session.key.clone(), session);
        }
        for warning in delta.warnings {
            state.pending.insert(warning.key.clone(), warning);
        }
        if let Some(key) = selected
            && delta.invalidated.contains(&key)
        {
            self.emit(WorkerEvent::TranscriptInvalidated { key });
        }
    }

    // ---- processes ------------------------------------------------------

    fn sample_processes(&mut self) {
        let observation = (self.sampler)(now_ms());
        if let Ok(snapshot) = &observation {
            let snapshot = snapshot.clone();
            self.emit(WorkerEvent::Processes { snapshot });
        }
        self.observation = Some(observation);
        self.next_process = Instant::now() + self.process_every;
    }

    fn publish_after_sample(&mut self) {
        if let Some(scan) = self.last_scan {
            self.publish(self.generation, scan);
        }
    }

    // ---- publishing -----------------------------------------------------

    fn publish(&mut self, generation: u64, scan: ScanProgress) {
        let mut sessions: Vec<SessionSummary> = Vec::new();
        let mut warnings: Vec<Warning> = Vec::new();
        for state in &self.states {
            sessions.extend(state.sessions.values().cloned());
            let mut ws: Vec<Warning> = state.warnings.values().cloned().collect();
            ws.extend(state.failure.clone());
            ws.sort_by(|a, b| {
                a.key
                    .source
                    .cmp(&b.key.source)
                    .then(a.message.cmp(&b.message))
            });
            warnings.extend(ws);
        }
        if self.config.process_enabled
            && let Some(observation) = &self.observation
        {
            for s in &mut sessions {
                s.process = processes::association_from_observation(s, observation);
            }
        }
        sessions.sort_by(|a, b| {
            b.last_persisted_at
                .cmp(&a.last_persisted_at)
                .then_with(|| a.key.provider.cmp(&b.key.provider))
                .then_with(|| a.key.provider_session_id.cmp(&b.key.provider_session_id))
        });
        self.emit(WorkerEvent::Roster {
            generation,
            sessions,
            warnings,
            scan,
        });
    }

    /// Send with backpressure: wait for queue space but give up on shutdown or
    /// when the UI side is gone, so a stalled consumer can never wedge `join`.
    fn emit(&mut self, mut event: WorkerEvent) {
        loop {
            if self.stop.load(Ordering::SeqCst) {
                self.shutting_down = true;
                return;
            }
            match self.events.try_send(event) {
                Ok(()) => return,
                Err(TrySendError::Disconnected(_)) => {
                    self.shutting_down = true;
                    return;
                }
                Err(TrySendError::Full(e)) => {
                    event = e;
                    thread::sleep(BACKPRESSURE_POLL);
                }
            }
        }
    }
}
