use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentscope::config::{CliOverrides, Config, Env};
use agentscope::domain::*;
use agentscope::processes::{ProcessInfo, ProcessSnapshot};
use agentscope::providers::{ProviderDelta, ProviderError, ProviderReader, SourceRecord};
use agentscope::worker::{WorkerEvent, WorkerHandle, WorkerRequest};

const WAIT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Script {
    refreshes: VecDeque<Result<ProviderDelta, ProviderError>>,
    pages: VecDeque<Result<TranscriptPage, ProviderError>>,
    refresh_calls: usize,
    page_calls: usize,
    page_requests: Vec<(Option<PageAnchor>, PageDirection)>,
    page_max_events: Vec<usize>,
    count: usize,
    endless: bool,
}

struct Fake {
    provider: Provider,
    script: Arc<Mutex<Script>>,
    /// When set, every `refresh` call announces itself then waits for a permit.
    gate: Option<(Sender<()>, Receiver<()>)>,
    /// When set, every `transcript_page` call announces itself then waits.
    page_gate: Option<(Sender<()>, Receiver<()>)>,
}

fn key(provider: Provider, id: &str) -> SessionKey {
    SessionKey {
        provider,
        provider_session_id: id.into(),
        source: SourceKey::File("a".into()),
    }
}

fn summary(provider: Provider, id: &str, persisted: i64, count: usize) -> SessionSummary {
    SessionSummary {
        key: key(provider, id),
        title: "t".into(),
        title_derived: false,
        project: None,
        cwd: None,
        models: vec![],
        model_details: vec![],
        provider_version: None,
        created_at: None,
        last_event_at: None,
        last_persisted_at: Some(persisted),
        last_completion: None,
        unfinished_work: false,
        displayed_event_count: count,
        hidden_records: 0,
        source_available: true,
        archived: false,
        context_window: None,
        server_tools: None,
        lifecycle: Lifecycle::Unknown,
        process: ProcessAssociation::NotChecked,
        usage: None,
        lineage: Lineage {
            parent: None,
            fork: None,
            sidechain: false,
        },
        warnings: vec![],
    }
}

fn warning(provider: Provider, source: &str, kind: WarningKind, msg: &str) -> Warning {
    Warning {
        key: WarningKey {
            provider,
            source: source.into(),
            kind,
        },
        message: msg.into(),
        count: 1,
    }
}

fn delta(sessions: Vec<SessionSummary>, warnings: Vec<Warning>, done: bool) -> ProviderDelta {
    ProviderDelta {
        scan: ScanProgress {
            provider: sessions.first().map_or(Provider::Codex, |s| s.key.provider),
            scanned: sessions.len(),
            total: None,
            done,
        },
        sessions,
        removed: vec![],
        invalidated: vec![],
        warnings,
    }
}

fn with_invalidated(mut d: ProviderDelta, keys: Vec<SessionKey>) -> ProviderDelta {
    d.invalidated = keys;
    d
}

fn empty_page() -> TranscriptPage {
    TranscriptPage {
        events: vec![],
        diagnostics: vec![],
        next: None,
        has_more: false,
        truncated: false,
    }
}

impl ProviderReader for Fake {
    fn provider(&self) -> Provider {
        self.provider
    }
    fn refresh(
        &mut self,
        _now: Timestamp,
        _budget: RefreshBudget,
    ) -> Result<ProviderDelta, ProviderError> {
        if let Some((entered, gate)) = &self.gate {
            let _ = entered.send(());
            let _ = gate.recv_timeout(WAIT);
        }
        let mut s = self.script.lock().unwrap();
        s.refresh_calls += 1;
        match s.refreshes.pop_front() {
            Some(r) => r,
            None => Ok(delta(vec![], vec![], !s.endless)),
        }
    }
    fn transcript_page(
        &mut self,
        _key: &SessionKey,
        anchor: Option<PageAnchor>,
        direction: PageDirection,
        request: PageRequest,
    ) -> Result<TranscriptPage, ProviderError> {
        if let Some((entered, gate)) = &self.page_gate {
            let _ = entered.send(());
            let _ = gate.recv_timeout(WAIT);
        }
        let mut s = self.script.lock().unwrap();
        s.page_calls += 1;
        s.page_requests.push((anchor, direction));
        s.page_max_events.push(request.max_events);
        s.pages.pop_front().unwrap_or_else(|| Ok(empty_page()))
    }
    fn count_events_after(
        &mut self,
        _key: &SessionKey,
        _anchor: &PageAnchor,
    ) -> Result<usize, ProviderError> {
        Ok(self.script.lock().unwrap().count)
    }
    fn source_record(&mut self, _locator: &SourceLocator) -> Result<SourceRecord, ProviderError> {
        Ok(SourceRecord {
            raw: "raw".into(),
            display: "display".into(),
            unavailable: None,
        })
    }
}

fn config() -> Config {
    let mut c = Config::load_with_env(&CliOverrides::default(), &Env::new()).unwrap();
    c.refresh_ms = 3_600_000;
    c.process_ms = 3_600_000;
    c.process_enabled = false;
    c
}

fn fake(provider: Provider, script: &Arc<Mutex<Script>>) -> Fake {
    Fake {
        provider,
        script: script.clone(),
        gate: None,
        page_gate: None,
    }
}

fn gated(provider: Provider, script: &Arc<Mutex<Script>>) -> (Fake, Receiver<()>, Sender<()>) {
    let (entered_tx, entered_rx) = mpsc::channel();
    let (gate_tx, gate_rx) = mpsc::channel();
    (
        Fake {
            provider,
            script: script.clone(),
            gate: Some((entered_tx, gate_rx)),
            page_gate: None,
        },
        entered_rx,
        gate_tx,
    )
}

fn spawn(fake: Fake) -> WorkerHandle {
    WorkerHandle::spawn(vec![Box::new(fake)], config())
}

fn next(h: &WorkerHandle) -> WorkerEvent {
    h.rx.recv_timeout(WAIT).expect("worker event")
}

/// Collect events until `pred` matches one (inclusive).
fn until(h: &WorkerHandle, pred: impl Fn(&WorkerEvent) -> bool) -> Vec<WorkerEvent> {
    let mut out = Vec::new();
    loop {
        let ev = next(h);
        let hit = pred(&ev);
        out.push(ev);
        if hit {
            return out;
        }
    }
}

fn finished(gen_: u64) -> impl Fn(&WorkerEvent) -> bool {
    move |e| matches!(e, WorkerEvent::RefreshFinished { generation } if *generation == gen_)
}

fn last_roster(events: &[WorkerEvent]) -> (&[SessionSummary], &[Warning]) {
    events
        .iter()
        .rev()
        .find_map(|e| match e {
            WorkerEvent::Roster {
                sessions, warnings, ..
            } => Some((sessions.as_slice(), warnings.as_slice())),
            _ => None,
        })
        .expect("a roster event")
}

#[test]
fn coalesces_refresh_and_drops_stale_transcript_responses() {
    let script = Arc::new(Mutex::new(Script::default()));
    let (f, entered, gate) = gated(Provider::Codex, &script);
    let h = spawn(f);
    // The initial refresh is now running and blocked inside the adapter.
    entered.recv_timeout(WAIT).unwrap();
    let a = key(Provider::Codex, "a");
    let b = key(Provider::Codex, "b");
    for _ in 0..3 {
        h.tx.send(WorkerRequest::Refresh).unwrap();
    }
    h.tx.send(WorkerRequest::Select {
        request_id: 1,
        key: a,
    })
    .unwrap();
    h.tx.send(WorkerRequest::Select {
        request_id: 2,
        key: b.clone(),
    })
    .unwrap();
    h.tx.send(WorkerRequest::LoadPage {
        request_id: 1,
        key: b,
        anchor: None,
        direction: PageDirection::Backward,
    })
    .unwrap();
    gate.send(()).unwrap();
    gate.send(()).unwrap();

    let events = until(&h, finished(2));
    let transcripts: Vec<u64> = events
        .iter()
        .filter_map(|e| match e {
            WorkerEvent::Transcript { request_id, .. } => Some(*request_id),
            _ => None,
        })
        .collect();
    assert_eq!(
        transcripts,
        vec![2],
        "only the latest selection is answered"
    );
    // Three Refresh requests collapsed into exactly one extra pass.
    assert!(matches!(
        h.rx.recv_timeout(Duration::from_millis(400)),
        Err(RecvTimeoutError::Timeout)
    ));
    let s = script.lock().unwrap();
    assert_eq!(s.refresh_calls, 2);
    assert_eq!(s.page_calls, 1);
    drop(s);
    assert!(h.shutdown());
}

#[test]
fn services_page_requests_between_refresh_chunks() {
    let script = Arc::new(Mutex::new(Script::default()));
    {
        let mut s = script.lock().unwrap();
        s.refreshes.push_back(Ok(delta(
            vec![summary(Provider::Codex, "a", 1, 1)],
            vec![],
            false,
        )));
        s.refreshes.push_back(Ok(delta(vec![], vec![], true)));
    }
    let (f, entered, gate) = gated(Provider::Codex, &script);
    let h = spawn(f);
    entered.recv_timeout(WAIT).unwrap(); // chunk 1 running
    h.tx.send(WorkerRequest::Select {
        request_id: 7,
        key: key(Provider::Codex, "a"),
    })
    .unwrap();
    gate.send(()).unwrap(); // chunk 1 returns (not done)
    entered.recv_timeout(WAIT).unwrap(); // chunk 2 is now blocked

    // The page must be answered before chunk 2 is allowed to finish.
    let events = until(&h, |e| matches!(e, WorkerEvent::Transcript { .. }));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, WorkerEvent::RefreshFinished { .. }))
    );
    assert!(events.iter().any(|e| matches!(
        e,
        WorkerEvent::Roster { scan, .. } if !scan.done
    )));
    gate.send(()).unwrap();
    until(&h, finished(1));
    assert!(h.shutdown());
}

#[test]
fn provider_failure_keeps_last_good_sessions_and_replaces_warning() {
    let script = Arc::new(Mutex::new(Script::default()));
    let w1 = warning(Provider::Codex, "root", WarningKind::Parse, "bad line");
    let w2 = warning(
        Provider::Codex,
        "root",
        WarningKind::Unsupported,
        "newer schema",
    );
    {
        let mut s = script.lock().unwrap();
        s.refreshes.push_back(Ok(delta(
            vec![summary(Provider::Codex, "a", 1, 1)],
            vec![w1.clone()],
            true,
        )));
        s.refreshes
            .push_back(Err(ProviderError::Io("disk gone".into())));
        s.refreshes
            .push_back(Ok(delta(vec![], vec![w2.clone()], true)));
        s.refreshes.push_back(Ok(delta(vec![], vec![], true)));
    }
    let h = spawn(fake(Provider::Codex, &script));

    let ev = until(&h, finished(1));
    let (sessions, warnings) = last_roster(&ev);
    assert_eq!(sessions.len(), 1);
    assert_eq!(warnings, std::slice::from_ref(&w1));

    h.tx.send(WorkerRequest::Refresh).unwrap();
    let ev = until(&h, finished(2));
    let (sessions, warnings) = last_roster(&ev);
    assert_eq!(sessions.len(), 1, "last good sessions survive a failure");
    assert!(warnings.contains(&w1));
    assert!(warnings.iter().any(|w| w.key.source == "refresh"
        && w.key.kind == WarningKind::Io
        && w.message.contains("disk gone")));

    h.tx.send(WorkerRequest::Refresh).unwrap();
    let ev = until(&h, finished(3));
    let (sessions, warnings) = last_roster(&ev);
    assert_eq!(sessions.len(), 1);
    assert_eq!(warnings, [w2], "warnings are replaced, not appended");

    h.tx.send(WorkerRequest::Refresh).unwrap();
    let ev = until(&h, finished(4));
    let (_, warnings) = last_roster(&ev);
    assert!(warnings.is_empty(), "recovery clears the key");
    assert!(h.shutdown());
}

#[test]
fn opencode_in_place_update_emits_transcript_invalidated_for_selected_session() {
    let script = Arc::new(Mutex::new(Script::default()));
    {
        let mut s = script.lock().unwrap();
        s.refreshes.push_back(Ok(delta(
            vec![
                summary(Provider::OpenCode, "a", 10, 3),
                summary(Provider::OpenCode, "b", 10, 3),
            ],
            vec![],
            true,
        )));
        // Rows were edited in place; the adapter names the sessions.
        s.refreshes.push_back(Ok(with_invalidated(
            delta(
                vec![
                    summary(Provider::OpenCode, "a", 20, 3),
                    summary(Provider::OpenCode, "b", 20, 3),
                ],
                vec![],
                true,
            ),
            vec![key(Provider::OpenCode, "a"), key(Provider::OpenCode, "b")],
        )));
    }
    let h = spawn(fake(Provider::OpenCode, &script));
    until(&h, finished(1));
    h.tx.send(WorkerRequest::Select {
        request_id: 1,
        key: key(Provider::OpenCode, "a"),
    })
    .unwrap();
    until(&h, |e| matches!(e, WorkerEvent::Transcript { .. }));

    h.tx.send(WorkerRequest::Refresh).unwrap();
    let ev = until(&h, finished(2));
    let invalidated: Vec<&SessionKey> = ev
        .iter()
        .filter_map(|e| match e {
            WorkerEvent::TranscriptInvalidated { key } => Some(key),
            _ => None,
        })
        .collect();
    assert_eq!(invalidated, vec![&key(Provider::OpenCode, "a")]);
    assert!(h.shutdown());
}

#[test]
fn unavailable_page_is_retried_later_without_busy_looping() {
    let script = Arc::new(Mutex::new(Script::default()));
    {
        let mut s = script.lock().unwrap();
        s.pages.push_back(Err(ProviderError::Unavailable(
            "transcript index is building (0 of 9 bytes); retry".into(),
        )));
        s.pages.push_back(Err(ProviderError::Unavailable(
            "transcript index is building (5 of 9 bytes); retry".into(),
        )));
    }
    let h = spawn(fake(Provider::Codex, &script));
    until(&h, finished(1));
    let start = Instant::now();
    h.tx.send(WorkerRequest::Select {
        request_id: 1,
        key: key(Provider::Codex, "a"),
    })
    .unwrap();
    until(&h, |e| {
        matches!(e, WorkerEvent::Transcript { request_id: 1, .. })
    });
    assert!(
        start.elapsed() >= Duration::from_millis(400),
        "two retries must be spaced out, took {:?}",
        start.elapsed()
    );
    assert_eq!(script.lock().unwrap().page_calls, 3);
    assert!(h.shutdown());
}

#[test]
fn processes_are_merged_into_published_roster() {
    let script = Arc::new(Mutex::new(Script::default()));
    script.lock().unwrap().refreshes.push_back(Ok(delta(
        vec![summary(Provider::Codex, "01a1", 1, 1)],
        vec![],
        true,
    )));
    let mut c = config();
    c.process_enabled = true;
    let sampler = Box::new(|now| {
        Ok(ProcessSnapshot {
            sampled_at: now,
            processes: vec![ProcessInfo::new(
                42,
                Some(1_000),
                "codex".into(),
                None,
                Some(vec!["codex".into(), "resume".into(), "01a1".into()]),
            )],
        })
    });
    let h = WorkerHandle::spawn_with_sampler(
        vec![Box::new(fake(Provider::Codex, &script))],
        c,
        sampler,
    );
    let ev = until(&h, finished(1));
    assert!(
        ev.iter()
            .any(|e| matches!(e, WorkerEvent::Processes { .. }))
    );
    let (sessions, _) = last_roster(&ev);
    assert_eq!(
        sessions[0].process,
        ProcessAssociation::Matched {
            pid: 42,
            started_at: Some(1_000)
        }
    );
    assert!(h.shutdown());
}

#[test]
fn shutdown_joins_without_panicking() {
    let script = Arc::new(Mutex::new(Script::default()));
    let h = spawn(fake(Provider::Codex, &script));
    until(&h, finished(1));
    assert!(h.shutdown());

    // A consumer that never reads must not wedge shutdown: the worker keeps
    // producing chunks until its bounded event queue fills.
    let script = Arc::new(Mutex::new(Script {
        endless: true,
        ..Script::default()
    }));
    let h = spawn(fake(Provider::Codex, &script));
    std::thread::sleep(Duration::from_millis(200));
    let start = Instant::now();
    assert!(h.shutdown());
    assert!(start.elapsed() < Duration::from_secs(3));
}

#[test]
fn invalidation_follows_the_adapter_signal_not_summary_changes() {
    let script = Arc::new(Mutex::new(Script::default()));
    {
        let mut s = script.lock().unwrap();
        s.refreshes.push_back(Ok(delta(
            vec![summary(Provider::Claude, "a", 10, 3)],
            vec![],
            true,
        )));
        // Persisted time and count both moved, but nothing was revised.
        s.refreshes.push_back(Ok(delta(
            vec![summary(Provider::Claude, "a", 20, 2)],
            vec![],
            true,
        )));
        // Same persisted time and count, content revised.
        s.refreshes.push_back(Ok(with_invalidated(
            delta(vec![summary(Provider::Claude, "a", 20, 2)], vec![], true),
            vec![key(Provider::Claude, "a")],
        )));
    }
    let h = spawn(fake(Provider::Claude, &script));
    until(&h, finished(1));
    h.tx.send(WorkerRequest::Select {
        request_id: 1,
        key: key(Provider::Claude, "a"),
    })
    .unwrap();
    until(&h, |e| matches!(e, WorkerEvent::Transcript { .. }));
    let count = |ev: &[WorkerEvent]| {
        ev.iter()
            .filter(|e| matches!(e, WorkerEvent::TranscriptInvalidated { .. }))
            .count()
    };
    h.tx.send(WorkerRequest::Refresh).unwrap();
    assert_eq!(count(&until(&h, finished(2))), 0);
    h.tx.send(WorkerRequest::Refresh).unwrap();
    assert_eq!(count(&until(&h, finished(3))), 1);
    assert!(h.shutdown());
}

#[test]
fn process_sampling_is_not_starved_by_faster_refreshes() {
    let script = Arc::new(Mutex::new(Script::default()));
    let mut c = config();
    c.refresh_ms = 40;
    c.process_ms = 200;
    c.process_enabled = true;
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let sampler = Box::new(move |now| {
        counter.fetch_add(1, Ordering::SeqCst);
        Ok(ProcessSnapshot {
            sampled_at: now,
            processes: vec![],
        })
    });
    let h = WorkerHandle::spawn_with_sampler(
        vec![Box::new(fake(Provider::Codex, &script))],
        c,
        sampler,
    );
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(1_100) {
        let _ = h.rx.recv_timeout(Duration::from_millis(20));
    }
    let n = calls.load(Ordering::SeqCst);
    assert!(n >= 4, "expected periodic samples, got {n}");
    assert!(h.shutdown());
}

#[test]
fn selection_arriving_during_a_page_load_is_served_without_more_messages() {
    let script = Arc::new(Mutex::new(Script::default()));
    let (entered_tx, entered) = mpsc::channel();
    let (release, gate_rx) = mpsc::channel();
    let mut f = fake(Provider::Codex, &script);
    f.page_gate = Some((entered_tx, gate_rx));
    let h = spawn(f);
    until(&h, finished(1));
    h.tx.send(WorkerRequest::Select {
        request_id: 1,
        key: key(Provider::Codex, "a"),
    })
    .unwrap();
    entered.recv_timeout(WAIT).unwrap(); // request 1 is inside the adapter
    h.tx.send(WorkerRequest::Select {
        request_id: 2,
        key: key(Provider::Codex, "b"),
    })
    .unwrap();
    release.send(()).unwrap();
    release.send(()).unwrap();
    // No further messages are sent; request 2 must still be answered.
    let events = until(&h, |e| matches!(e, WorkerEvent::Transcript { .. }));
    assert!(matches!(
        events.last(),
        Some(WorkerEvent::Transcript { request_id: 2, .. })
    ));
    assert!(h.shutdown());
}

#[test]
fn initial_selection_requests_the_oldest_page_forward() {
    let script = Arc::new(Mutex::new(Script::default()));
    let h = spawn(fake(Provider::Codex, &script));
    until(&h, finished(1));
    h.tx.send(WorkerRequest::Select {
        request_id: 1,
        key: key(Provider::Codex, "a"),
    })
    .unwrap();
    until(&h, |e| matches!(e, WorkerEvent::Transcript { .. }));
    assert_eq!(
        script.lock().unwrap().page_requests,
        vec![(None, PageDirection::Forward)]
    );
    assert!(h.shutdown());
}

#[test]
fn requests_are_serviced_at_provider_boundaries() {
    let a = Arc::new(Mutex::new(Script::default()));
    let b = Arc::new(Mutex::new(Script::default()));
    let (fa, entered_a, gate_a) = gated(Provider::Codex, &a);
    let (fb, entered_b, gate_b) = gated(Provider::Claude, &b);
    let h = WorkerHandle::spawn(vec![Box::new(fa), Box::new(fb)], config());
    entered_a.recv_timeout(WAIT).unwrap();
    h.tx.send(WorkerRequest::Select {
        request_id: 1,
        key: key(Provider::Codex, "a"),
    })
    .unwrap();
    // Provider A finishes (done) in this single chunk; the page must be
    // answered before provider B's chunk is released.
    gate_a.send(()).unwrap();
    entered_b.recv_timeout(WAIT).unwrap();
    let events = until(&h, |e| matches!(e, WorkerEvent::Transcript { .. }));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, WorkerEvent::RefreshFinished { .. }))
    );
    gate_b.send(()).unwrap();
    until(&h, finished(1));
    assert!(h.shutdown());
}

fn select(h: &WorkerHandle, id: u64, k: &SessionKey) {
    h.tx.send(WorkerRequest::Select {
        request_id: id,
        key: k.clone(),
    })
    .unwrap();
}

fn anchor(n: u64) -> PageAnchor {
    PageAnchor {
        timestamp: Some(n as i64),
        source_order: n,
        stable_id: format!("e{n}"),
    }
}

#[test]
fn count_after_is_answered_with_the_adapter_count() {
    let script = Arc::new(Mutex::new(Script::default()));
    script.lock().unwrap().count = 7;
    let h = spawn(fake(Provider::Codex, &script));
    let k = key(Provider::Codex, "a");
    select(&h, 1, &k);
    until(&h, |e| matches!(e, WorkerEvent::Transcript { .. }));
    h.tx.send(WorkerRequest::CountAfter {
        request_id: 2,
        key: k,
        anchor: anchor(3),
    })
    .unwrap();
    let events = until(&h, |e| matches!(e, WorkerEvent::EventsAfter { .. }));
    assert!(matches!(
        events.last(),
        Some(WorkerEvent::EventsAfter {
            request_id: 2,
            count: 7
        })
    ));
    assert!(h.shutdown());
}

#[test]
fn page_failures_carry_the_request_id() {
    let script = Arc::new(Mutex::new(Script::default()));
    script
        .lock()
        .unwrap()
        .pages
        .push_back(Err(ProviderError::Parse("bad".into())));
    let h = spawn(fake(Provider::Codex, &script));
    select(&h, 5, &key(Provider::Codex, "a"));
    let events = until(&h, |e| matches!(e, WorkerEvent::LoadFailed { .. }));
    assert!(matches!(
        events.last(),
        Some(WorkerEvent::LoadFailed { request_id: 5 })
    ));
    assert!(h.shutdown());
}

#[test]
fn requested_page_size_never_exceeds_the_ui_window() {
    let script = Arc::new(Mutex::new(Script::default()));
    let mut c = config();
    c.max_page_events = 1_000_000;
    let h = WorkerHandle::spawn(vec![Box::new(fake(Provider::Codex, &script))], c);
    select(&h, 1, &key(Provider::Codex, "a"));
    until(&h, |e| matches!(e, WorkerEvent::Transcript { .. }));
    assert_eq!(
        script.lock().unwrap().page_max_events,
        vec![agentscope::app::MAX_LOADED_EVENTS]
    );
    assert!(h.shutdown());
}
