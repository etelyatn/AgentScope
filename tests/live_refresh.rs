//! Live updates: worker events applied to the app without moving the reader.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentscope::app::{App, Panel, Screen};
use agentscope::config::{CliOverrides, Config, Env};
use agentscope::domain::*;
use agentscope::processes::{ProcessInfo, ProcessSnapshot};
use agentscope::providers::{ProviderDelta, ProviderError, ProviderReader, SourceRecord};
use agentscope::ui::{
    KeyAction, RosterContext, apply_worker_event, drain_worker, flush_requests, handle_key, render,
    reset_for_new_worker, worker_stopped,
};
use agentscope::worker::{WorkerEvent, WorkerHandle, WorkerRequest};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

const NOW: Timestamp = 1_800_000_000_000;

fn codex_key() -> SessionKey {
    SessionKey {
        provider: Provider::Codex,
        provider_session_id: "codex-1".into(),
        source: SourceKey::File("codex-1.jsonl".into()),
    }
}

fn claude_key() -> SessionKey {
    SessionKey {
        provider: Provider::Claude,
        provider_session_id: "claude-1".into(),
        source: SourceKey::File("claude-1.jsonl".into()),
    }
}

fn session(key: SessionKey, title: &str, age_secs: i64, count: usize) -> SessionSummary {
    SessionSummary {
        key,
        title: title.into(),
        title_derived: false,
        project: Some("RipperGame".into()),
        cwd: None,
        models: vec![],
        model_details: vec![],
        provider_version: None,
        created_at: Some(NOW - 86_400_000),
        last_event_at: Some(NOW - age_secs * 1000),
        last_persisted_at: Some(NOW - age_secs * 1000),
        last_completion: None,
        unfinished_work: true,
        displayed_event_count: count,
        hidden_records: 0,
        source_available: true,
        archived: false,
        context_window: None,
        server_tools: None,
        lifecycle: Lifecycle::Unknown,
        process: ProcessAssociation::Unmatched,
        usage: None,
        lineage: Lineage {
            parent: None,
            fork: None,
            sidechain: false,
        },
        warnings: vec![],
    }
}

fn scan(done: bool) -> ScanProgress {
    ScanProgress {
        provider: Provider::Codex,
        scanned: 2,
        total: Some(9),
        done,
    }
}

fn roster(codex_age: i64, claude_age: i64, count: usize) -> WorkerEvent {
    roster_generation(1, codex_age, claude_age, count, vec![], true)
}

fn roster_generation(
    generation: u64,
    codex_age: i64,
    claude_age: i64,
    count: usize,
    warnings: Vec<Warning>,
    done: bool,
) -> WorkerEvent {
    WorkerEvent::Roster {
        generation,
        sessions: vec![
            session(codex_key(), "Inventory API", codex_age, count),
            session(claude_key(), "Scanner refactor", claude_age, 1),
        ],
        warnings,
        scan: scan(done),
    }
}

fn new_app() -> App {
    App::new(
        NOW,
        Thresholds {
            quiet: Duration::from_secs(300),
            stall: Duration::from_secs(900),
        },
    )
}

fn ctx() -> RosterContext {
    RosterContext {
        no_color: true,
        ..RosterContext::default()
    }
}

fn press(app: &mut App, code: KeyCode) -> KeyAction {
    handle_key(app, KeyEvent::new(code, KeyModifiers::NONE))
}

fn ch(app: &mut App, c: char) -> KeyAction {
    press(app, KeyCode::Char(c))
}

fn draw(app: &App, ctx: &RosterContext, w: u16, h: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal.draw(|f| render(f, app, ctx)).unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .chunks(w as usize)
        .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn provenance() -> Provenance {
    Provenance {
        provider: Provider::Codex,
        source: SourceKey::File("codex-1.jsonl".into()),
        locator: SourceLocator {
            session: codex_key(),
            kind: LocatorKind::FileOffset {
                path: "codex-1.jsonl".into(),
                offset: 0,
            },
            generation: None,
        },
        record_type: "test".into(),
        provider_version: None,
    }
}

fn event(i: u64, body: &str) -> Event {
    Event {
        id: format!("e{i}"),
        session: codex_key(),
        timestamp: Some(NOW - 3_000_000 + i as i64 * 60_000),
        source_order: i,
        kind: EventKind::AssistantMessage,
        actor: Actor::Assistant,
        text: Some(BoundedText {
            preview: body.into(),
            byte_len: body.len(),
            truncated: false,
            locator: None,
        }),
        tool: None,
        usage: None,
        provenance: provenance(),
    }
}

fn anchor(i: u64) -> PageAnchor {
    PageAnchor {
        timestamp: Some(NOW - 3_000_000 + i as i64 * 60_000),
        source_order: i,
        stable_id: format!("e{i}"),
    }
}

fn page(bodies: &[&str], next: PageAnchor) -> TranscriptPage {
    TranscriptPage {
        events: bodies
            .iter()
            .enumerate()
            .map(|(i, b)| event(i as u64, b))
            .collect(),
        diagnostics: vec![],
        next: Some(next),
        has_more: false,
        truncated: false,
    }
}

/// Open the first roster row (Codex) with three loaded events, selecting the middle one.
fn open_three(app: &mut App, ctx: &mut RosterContext) {
    apply_worker_event(app, ctx, roster(60, 600, 3));
    assert_eq!(app.selected_key(), Some(codex_key()));
    press(app, KeyCode::Enter);
    let requests = app.take_requests();
    let Some(WorkerRequest::Select { request_id, .. }) = requests.first() else {
        panic!("expected Select, got {requests:?}");
    };
    apply_worker_event(
        app,
        ctx,
        WorkerEvent::Transcript {
            request_id: *request_id,
            page: page(&["first", "second", "third"], anchor(2)),
        },
    );
    ch(app, 'j');
    assert_eq!(app.transcript().unwrap().selected(), 1);
}

#[test]
fn live_append_updates_roster_and_counts_new_events_without_auto_scroll() {
    let mut app = new_app();
    let mut ctx = ctx();
    open_three(&mut app, &mut ctx);
    // Opening away from the tail may queue a baseline count; settle it.
    for request in app.take_requests() {
        if let WorkerRequest::CountAfter { request_id, .. } = request {
            apply_worker_event(
                &mut app,
                &mut ctx,
                WorkerEvent::EventsAfter {
                    request_id,
                    count: 0,
                },
            );
        }
    }

    // Two events are appended; the worker publishes a newer roster.
    apply_worker_event(
        &mut app,
        &mut ctx,
        roster_generation(2, 5, 600, 5, vec![], true),
    );
    assert_eq!(
        app.session(&codex_key())
            .unwrap()
            .session
            .displayed_event_count,
        5
    );
    let requests = app.take_requests();
    let Some(WorkerRequest::CountAfter {
        request_id,
        anchor: a,
        ..
    }) = requests.first()
    else {
        panic!("expected CountAfter, got {requests:?}");
    };
    assert_eq!(a, &anchor(2));
    apply_worker_event(
        &mut app,
        &mut ctx,
        WorkerEvent::EventsAfter {
            request_id: *request_id,
            count: 2,
        },
    );

    assert_eq!(app.new_event_count(), 2);
    assert_eq!(app.transcript().unwrap().selected(), 1, "no auto-scroll");
    assert_eq!(app.transcript().unwrap().events().len(), 3, "no auto-load");
    assert!(
        app.take_requests().is_empty(),
        "nothing is fetched on its own"
    );
    let screen = draw(&app, &ctx, 110, 30);
    assert!(screen.contains("2 new"), "{screen}");

    // G jumps to the newest page; it is requested, not assumed.
    ch(&mut app, 'G');
    assert!(matches!(
        app.take_requests().as_slice(),
        [WorkerRequest::LoadPage {
            anchor: None,
            direction: PageDirection::Backward,
            ..
        }]
    ));
}

#[test]
fn roster_selection_follows_its_key_when_live_updates_reorder_rows() {
    let mut app = new_app();
    let mut ctx = ctx();
    apply_worker_event(&mut app, &mut ctx, roster(60, 600, 3));
    ch(&mut app, 'j');
    assert_eq!(app.selected_key(), Some(claude_key()));
    // Claude becomes the most recently active; it moves to the top.
    apply_worker_event(
        &mut app,
        &mut ctx,
        roster_generation(2, 60, 5, 3, vec![], true),
    );
    assert_eq!(app.selected_key(), Some(claude_key()));
    assert_eq!(app.selected_index(), Some(0));
    // A stale publication is ignored.
    apply_worker_event(
        &mut app,
        &mut ctx,
        roster_generation(1, 5, 900, 3, vec![], true),
    );
    assert_eq!(app.selected_index(), Some(0));
    assert_eq!(app.generation(), 2);
}

#[test]
fn live_in_place_message_update_reloads_visible_row_without_new_event_or_selection_loss() {
    let mut app = new_app();
    let mut ctx = ctx();
    open_three(&mut app, &mut ctx);
    app.take_requests();

    apply_worker_event(
        &mut app,
        &mut ctx,
        WorkerEvent::TranscriptInvalidated { key: codex_key() },
    );
    let requests = app.take_requests();
    let [
        WorkerRequest::LoadPage {
            request_id,
            anchor: None,
            direction: PageDirection::Backward,
            ..
        },
    ] = requests.as_slice()
    else {
        panic!("expected one tail reload, got {requests:?}");
    };
    // The visible rows stay on screen while the reload is in flight.
    let screen = draw(&app, &ctx, 110, 30);
    assert!(screen.contains("second"), "{screen}");

    // Search while the reload is in flight: nothing matches yet.
    ch(&mut app, '/');
    for c in "streamed".chars() {
        ch(&mut app, c);
    }
    let screen = draw(&app, &ctx, 110, 30);
    assert!(screen.contains("Search: streamed_"), "{screen}");
    assert!(screen.contains("no matches (loaded events)"), "{screen}");
    press(&mut app, KeyCode::Enter);
    apply_worker_event(
        &mut app,
        &mut ctx,
        WorkerEvent::Transcript {
            request_id: *request_id,
            page: page(
                &["first", "second, now streamed to completion", "third"],
                anchor(0),
            ),
        },
    );
    let t = app.transcript().unwrap();
    assert_eq!(t.selected(), 1);
    assert_eq!(t.selected_event().unwrap().id, "e1");
    assert_eq!(t.events().len(), 3);
    assert_eq!(app.new_event_count(), 0, "an edit is not a new event");
    let screen = draw(&app, &ctx, 110, 30);
    assert!(screen.contains("streamed to completion"), "{screen}");
    // The search follows the reloaded content, not a stale cache.
    assert!(screen.contains("match 1/1 (loaded events)"), "{screen}");
}

fn candidate_snapshot(pid: u32) -> ProcessSnapshot {
    ProcessSnapshot {
        sampled_at: NOW,
        processes: vec![
            ProcessInfo::new(
                pid,
                Some(NOW - 60_000),
                "/usr/local/bin/codex".into(),
                None,
                Some(vec![
                    "/usr/local/bin/codex".into(),
                    "--model".into(),
                    "x".into(),
                ]),
            ),
            ProcessInfo::new(
                pid + 1,
                None,
                "/usr/bin/vim".into(),
                None,
                Some(vec!["vim".into()]),
            ),
        ],
    }
}

#[test]
fn process_snapshot_and_warning_banner_update_without_blocking_input() {
    let mut app = new_app();
    let mut ctx = ctx();
    apply_worker_event(&mut app, &mut ctx, roster(60, 600, 3));

    // No sample yet: the panel says so instead of implying activity.
    assert_eq!(ch(&mut app, 'p'), KeyAction::None);
    assert_eq!(app.panel(), Panel::Processes);
    let screen = draw(&app, &ctx, 100, 30);
    assert!(screen.contains("No process sample yet"), "{screen}");

    // A sample arrives while the panel is open and shows only candidates.
    apply_worker_event(
        &mut app,
        &mut ctx,
        WorkerEvent::Processes {
            snapshot: candidate_snapshot(4242),
        },
    );
    let screen = draw(&app, &ctx, 100, 30);
    assert!(screen.contains("PID 4242"), "{screen}");
    assert!(!screen.contains("vim"), "{screen}");
    assert!(
        screen.contains("not matches") || screen.contains("not a match"),
        "{screen}"
    );
    // The panel stays responsive and a newer sample replaces the old one.
    ch(&mut app, 'j');
    apply_worker_event(
        &mut app,
        &mut ctx,
        WorkerEvent::Processes {
            snapshot: candidate_snapshot(5151),
        },
    );
    let screen = draw(&app, &ctx, 100, 30);
    assert!(
        screen.contains("PID 5151") && !screen.contains("PID 4242"),
        "{screen}"
    );
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.panel(), Panel::None);

    // A provider warning appears in the banner, without blocking input, and
    // the next publication that no longer carries it retires it.
    apply_worker_event(
        &mut app,
        &mut ctx,
        WorkerEvent::Warning {
            warning: Warning {
                key: WarningKey {
                    provider: Provider::Claude,
                    source: "projects".into(),
                    kind: WarningKind::Parse,
                },
                message: "unreadable record".into(),
                count: 2,
            },
        },
    );
    ch(&mut app, 'j');
    let screen = draw(&app, &ctx, 100, 30);
    assert!(
        screen.contains("Warning: Claude: unreadable record"),
        "{screen}"
    );
    assert_eq!(app.selected_key(), Some(claude_key()));
    apply_worker_event(
        &mut app,
        &mut ctx,
        roster_generation(2, 60, 600, 3, vec![], true),
    );
    let screen = draw(&app, &ctx, 100, 30);
    assert!(!screen.contains("Warning:"), "{screen}");

    // Scan progress shows provider and counts, never paths.
    apply_worker_event(
        &mut app,
        &mut ctx,
        roster_generation(3, 60, 600, 3, vec![], false),
    );
    let screen = draw(&app, &ctx, 100, 30);
    assert!(screen.contains("Scanning Codex 2/9"), "{screen}");
}

#[test]
fn a_paused_worker_is_shown_and_r_asks_for_a_replacement() {
    let mut app = new_app();
    let mut ctx = ctx();
    apply_worker_event(&mut app, &mut ctx, roster(60, 600, 3));
    assert!(draw(&app, &ctx, 100, 30).contains("Live: on"));

    worker_stopped(&mut app, &mut ctx);
    let screen = draw(&app, &ctx, 100, 30);
    assert!(screen.contains("Live: paused"), "{screen}");
    assert!(screen.contains("Worker stopped"), "{screen}");
    // The last good roster stays; input still works.
    ch(&mut app, 'j');
    assert_eq!(app.selected_key(), Some(claude_key()));
    assert_eq!(ch(&mut app, 'r'), KeyAction::Refresh);

    reset_for_new_worker(&mut app, &mut ctx);
    assert!(draw(&app, &ctx, 100, 30).contains("Live: on"));
    assert_eq!(app.screen, Screen::Roster);
}

// ---- end to end through a real worker thread ------------------------------

#[derive(Default)]
struct State {
    sessions: Vec<SessionSummary>,
    bodies: Vec<String>,
}

struct Live(Arc<Mutex<State>>);

impl ProviderReader for Live {
    fn provider(&self) -> Provider {
        Provider::Codex
    }
    fn refresh(
        &mut self,
        _now: Timestamp,
        _budget: RefreshBudget,
    ) -> Result<ProviderDelta, ProviderError> {
        let s = self.0.lock().unwrap();
        Ok(ProviderDelta {
            sessions: s.sessions.clone(),
            removed: vec![],
            invalidated: vec![],
            warnings: vec![],
            scan: ScanProgress {
                provider: Provider::Codex,
                scanned: s.sessions.len(),
                total: None,
                done: true,
            },
        })
    }
    fn transcript_page(
        &mut self,
        _key: &SessionKey,
        from: Option<PageAnchor>,
        direction: PageDirection,
        _request: PageRequest,
    ) -> Result<TranscriptPage, ProviderError> {
        let s = self.0.lock().unwrap();
        let at = |a: &PageAnchor| a.source_order as usize;
        let range = match (from.as_ref(), direction) {
            (None, _) => 0..s.bodies.len(),
            (Some(a), PageDirection::Forward) => at(a) + 1..s.bodies.len(),
            (Some(a), PageDirection::Backward) => 0..at(a),
        };
        let events: Vec<Event> = range
            .clone()
            .map(|i| event(i as u64, &s.bodies[i]))
            .collect();
        Ok(TranscriptPage {
            next: range.end.checked_sub(1).map(|i| anchor(i as u64)),
            events,
            diagnostics: vec![],
            has_more: false,
            truncated: false,
        })
    }
    fn count_events_after(
        &mut self,
        _key: &SessionKey,
        anchor: &PageAnchor,
    ) -> Result<usize, ProviderError> {
        let s = self.0.lock().unwrap();
        Ok(s.bodies
            .len()
            .saturating_sub(anchor.source_order as usize + 1))
    }
    fn source_record(&mut self, _locator: &SourceLocator) -> Result<SourceRecord, ProviderError> {
        Ok(SourceRecord {
            raw: String::new(),
            display: String::new(),
            unavailable: Some("not available in this test".into()),
        })
    }
}

fn pump_until(
    app: &mut App,
    ctx: &mut RosterContext,
    worker: &WorkerHandle,
    done: impl Fn(&App) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        assert!(drain_worker(app, ctx, worker), "worker exited");
        flush_requests(app, worker);
        if done(app) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("condition not reached");
}

#[test]
fn a_real_worker_delivers_live_appends_to_the_open_inspector() {
    let state = Arc::new(Mutex::new(State {
        sessions: vec![session(codex_key(), "Inventory API", 60, 3)],
        bodies: vec!["first".into(), "second".into(), "third".into()],
    }));
    let mut config = Config::load_with_env(&CliOverrides::default(), &Env::new()).unwrap();
    config.refresh_ms = 3_600_000;
    config.process_ms = 3_600_000;
    config.process_enabled = false;
    let worker = WorkerHandle::spawn(vec![Box::new(Live(state.clone()))], config);
    let mut app = new_app();
    let mut ctx = ctx();

    pump_until(&mut app, &mut ctx, &worker, |a| a.total_sessions() == 1);
    press(&mut app, KeyCode::Enter);
    pump_until(&mut app, &mut ctx, &worker, |a| {
        a.transcript().is_some_and(|t| t.events().len() == 3)
    });
    ch(&mut app, 'j');

    {
        let mut s = state.lock().unwrap();
        s.bodies.push("fourth".into());
        s.bodies.push("fifth".into());
        s.sessions = vec![session(codex_key(), "Inventory API", 1, 5)];
    }
    // `r` asks the worker to refresh now.
    assert_eq!(ch(&mut app, 'r'), KeyAction::Refresh);
    worker.tx.send(WorkerRequest::Refresh).unwrap();
    pump_until(&mut app, &mut ctx, &worker, |a| a.new_event_count() == 2);

    assert_eq!(app.transcript().unwrap().selected(), 1, "no auto-scroll");
    assert_eq!(app.transcript().unwrap().events().len(), 3);
    assert_eq!(
        app.session(&codex_key())
            .unwrap()
            .session
            .displayed_event_count,
        5
    );
    assert!(worker.shutdown());
}
