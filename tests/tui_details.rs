use std::time::Duration;

use agentscope::app::{App, Screen, SourceState};
use agentscope::domain::*;
use agentscope::providers::SourceRecord;
use agentscope::ui::{RosterContext, render};
use agentscope::worker::WorkerRequest;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

const NOW: Timestamp = 1_800_000_000_000;
const SECRET: &str = "sk-ant-abcdefghijklmnop1234";

fn key() -> SessionKey {
    SessionKey {
        provider: Provider::Codex,
        provider_session_id: "sess-1".into(),
        source: SourceKey::File("sess-1.jsonl".into()),
    }
}

fn locator_for(session: SessionKey, offset: u64, generation: Option<u64>) -> SourceLocator {
    SourceLocator {
        session,
        kind: LocatorKind::FileOffset {
            path: "sess-1.jsonl".into(),
            offset,
        },
        generation,
    }
}

fn locator(offset: u64) -> SourceLocator {
    locator_for(key(), offset, Some(3))
}

fn summary() -> SessionSummary {
    SessionSummary {
        key: key(),
        title: "Inventory API".into(),
        title_derived: false,
        project: Some("RipperGame".into()),
        cwd: None,
        models: vec![],
        model_details: vec![],
        provider_version: None,
        created_at: None,
        last_event_at: Some(NOW - 540_000),
        last_persisted_at: Some(NOW - 540_000),
        last_completion: None,
        unfinished_work: false,
        displayed_event_count: 3,
        hidden_records: 4,
        source_available: true,
        archived: false,
        context_window: None,
        server_tools: None,
        lifecycle: Lifecycle::Unknown,
        process: ProcessAssociation::Unavailable,
        usage: None,
        lineage: Lineage {
            parent: None,
            fork: None,
            sidechain: false,
        },
        warnings: vec![],
    }
}

fn app() -> App {
    let mut app = App::new(
        NOW,
        Thresholds {
            quiet: Duration::from_secs(300),
            stall: Duration::from_secs(900),
        },
    );
    app.apply_roster(vec![summary()], vec![]);
    app
}

fn text(s: &str, truncated: bool, byte_len: usize) -> BoundedText {
    BoundedText {
        preview: s.into(),
        byte_len,
        truncated,
        locator: None,
    }
}

fn event(i: u64, kind: EventKind, body: &str) -> Event {
    Event {
        id: format!("e{i}"),
        session: key(),
        timestamp: Some(NOW - 3_000_000 + i as i64 * 60_000),
        source_order: i,
        kind,
        actor: Actor::Assistant,
        text: Some(text(body, false, body.len())),
        tool: None,
        usage: None,
        provenance: Provenance {
            provider: Provider::Codex,
            source: SourceKey::File("sess-1.jsonl".into()),
            locator: locator(100 * i),
            record_type: "response_item".into(),
            provider_version: None,
        },
    }
}

fn tool_event(i: u64) -> Event {
    let mut e = event(i, EventKind::Tool, "cargo test");
    e.actor = Actor::Tool;
    e.tool = Some(ToolInteraction {
        name: "shell".into(),
        call_id: Some("call-9".into()),
        input: Some(text("cargo test --all", false, 16)),
        output: Some(text("2 failed assertions\nsecond line", true, 90_000)),
        status: ToolStatus::Error,
        exit_code: Some(1),
        started_at: Some(NOW - 10_000),
        completed_at: Some(NOW - 7_500),
        orphan: false,
    });
    e
}

fn diagnostic(offset: u64, label: &str, hidden: bool) -> DiagnosticEntry {
    DiagnosticEntry {
        locator: locator(offset),
        label: label.into(),
        hidden,
    }
}

fn page(events: Vec<Event>, diagnostics: Vec<DiagnosticEntry>) -> TranscriptPage {
    let next = events.last().map(|e| PageAnchor {
        timestamp: e.timestamp,
        source_order: e.source_order,
        stable_id: e.id.clone(),
    });
    TranscriptPage {
        events,
        diagnostics,
        next,
        has_more: false,
        truncated: false,
    }
}

fn open(app: &mut App, first: TranscriptPage) {
    assert!(app.open_inspector());
    let requests = app.take_requests();
    let [WorkerRequest::Select { request_id, .. }] = requests.as_slice() else {
        panic!("expected a single Select, got {requests:?}");
    };
    assert!(app.apply_transcript(*request_id, first));
    app.take_requests();
}

fn draw(app: &App) -> String {
    let (w, h) = (100u16, 24u16);
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    let ctx = RosterContext {
        no_color: true,
        ..RosterContext::default()
    };
    terminal.draw(|f| render(f, app, &ctx)).unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .chunks(w as usize)
        .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn source_request(app: &mut App) -> (u64, SourceLocator) {
    let requests = app.take_requests();
    match requests.as_slice() {
        [
            WorkerRequest::LoadSource {
                request_id,
                locator,
            },
        ] => (*request_id, locator.clone()),
        other => panic!("expected a single LoadSource, got {other:?}"),
    }
}

fn record(raw: &str) -> SourceRecord {
    SourceRecord {
        raw: raw.into(),
        display: agentscope::ingest::text::display_safe(raw),
        unavailable: None,
    }
}

#[test]
fn shows_tool_arguments_output_and_oversized_markers() {
    let mut app = app();
    open(&mut app, page(vec![tool_event(0)], vec![]));
    assert!(app.open_details());
    assert_eq!(app.screen, Screen::Details);
    let out = draw(&app);
    for want in [
        "Tool: shell",
        "call-9",
        "Status: Error",
        "Duration: 2.5s",
        "Exit: 1",
        "Input: cargo test --all",
        "Output: 2 failed assertions",
        "second line",
        "[truncated: showing 31 of 90000 bytes]",
    ] {
        assert!(out.contains(want), "missing {want:?} in:\n{out}");
    }
}

#[test]
fn details_rendering_is_bounded_for_huge_previews() {
    let mut app = app();
    let mut e = tool_event(0);
    let huge = "x".repeat(2_000_000);
    e.tool.as_mut().unwrap().output = Some(text(&huge, true, 9_000_000));
    open(&mut app, page(vec![e], vec![]));
    assert!(app.open_details());
    let out = draw(&app);
    assert!(out.contains("Input: cargo test --all"));
    app.overlay_scroll(isize::MAX);
    assert!(draw(&app).contains("[display limited"), "marker at the end");
}

#[test]
fn source_view_loads_by_locator_and_rejects_foreign_locators() {
    let mut app = app();
    open(
        &mut app,
        page(vec![event(0, EventKind::AssistantMessage, "hello")], vec![]),
    );

    let other = SessionKey {
        provider_session_id: "other".into(),
        ..key()
    };
    // Another session, a superseded generation, and an unreferenced offset.
    for foreign in [
        locator_for(other, 0, Some(3)),
        locator_for(key(), 0, Some(2)),
        locator_for(key(), 12_345, Some(3)),
    ] {
        assert!(!app.open_source(foreign));
        assert!(app.take_requests().is_empty());
        assert_eq!(app.screen, Screen::Inspector);
        assert!(app.source_view().is_none());
    }

    assert!(app.open_selected_source());
    assert_eq!(app.screen, Screen::Source);
    let (id, loc) = source_request(&mut app);
    assert_eq!(loc, locator(0));
    assert!(draw(&app).contains("Loading source record"));

    assert!(!app.apply_source(id + 99, record("wrong request")));
    assert!(app.apply_source(id, record(r#"{"type":"message","text":"hello"}"#)));
    let out = draw(&app);
    assert!(
        out.contains(r#"{"type":"message","text":"hello"}"#),
        "{out}"
    );
    assert!(out.contains("sess-1.jsonl"), "{out}");
    assert!(out.contains("Redacted"), "{out}");

    // A second response for the same request is ignored.
    assert!(!app.apply_source(id, record("again")));

    assert!(app.close_overlay());
    assert_eq!(app.screen, Screen::Inspector);
    assert!(app.source_view().is_none());
}

#[test]
fn unavailable_and_failed_sources_are_shown() {
    let mut app = app();
    open(
        &mut app,
        page(vec![event(0, EventKind::AssistantMessage, "hello")], vec![]),
    );
    assert!(app.open_selected_source());
    let (id, _) = source_request(&mut app);
    assert!(app.apply_source(
        id,
        SourceRecord {
            raw: String::new(),
            display: String::new(),
            unavailable: Some("record exceeds limit".into()),
        }
    ));
    assert!(draw(&app).contains("record exceeds limit"));
    app.close_overlay();

    assert!(app.open_selected_source());
    let (id, _) = source_request(&mut app);
    app.transcript_load_failed(id);
    assert!(draw(&app).contains("could not be loaded"));
}

#[test]
fn diagnostics_lists_hidden_unknown_and_source_only_records() {
    let mut app = app();
    open(
        &mut app,
        page(
            vec![event(0, EventKind::AssistantMessage, "hello")],
            vec![
                diagnostic(500, "suppressed call record", true),
                diagnostic(600, "unknown record type", false),
                diagnostic(700, "source-only record", false),
            ],
        ),
    );
    assert!(app.open_diagnostics());
    assert_eq!(app.screen, Screen::Diagnostics);
    let out = draw(&app);
    for want in [
        "Diagnostics",
        "Hidden records: 4",
        "[hidden] suppressed call record",
        "[listed] unknown record type",
        "[listed] source-only record",
        "sess-1.jsonl @ byte 500",
        "sess-1.jsonl @ byte 700",
    ] {
        assert!(out.contains(want), "missing {want:?} in:\n{out}");
    }
}

#[test]
fn diagnostics_with_no_entries_says_so() {
    let mut app = app();
    open(
        &mut app,
        page(vec![event(0, EventKind::UserPrompt, "hi")], vec![]),
    );
    assert!(app.open_diagnostics());
    assert!(draw(&app).contains("No hidden, unknown, or source-only records"));
    assert!(!app.open_diagnostic_source());
    assert!(app.take_requests().is_empty());
}

#[test]
fn diagnostic_entry_selects_and_opens_its_source_record() {
    let mut app = app();
    open(
        &mut app,
        page(
            vec![event(0, EventKind::AssistantMessage, "hello")],
            vec![
                diagnostic(500, "unknown record type", false),
                diagnostic(600, "suppressed call record", true),
            ],
        ),
    );
    assert!(app.open_diagnostics());
    app.diagnostics_move(1);
    assert_eq!(app.diagnostics_selected(), 1);
    app.diagnostics_move(10);
    assert_eq!(app.diagnostics_selected(), 1, "clamped to the last entry");

    assert!(app.open_diagnostic_source());
    assert_eq!(app.screen, Screen::Source);
    let (id, loc) = source_request(&mut app);
    assert_eq!(loc, locator(600));
    assert!(app.apply_source(id, record(r#"{"type":"function_call","name":"shell"}"#)));
    assert!(draw(&app).contains("function_call"));

    // Esc returns to the diagnostics list, then to the inspector.
    assert!(app.close_overlay());
    assert_eq!(app.screen, Screen::Diagnostics);
    assert!(app.close_overlay());
    assert_eq!(app.screen, Screen::Inspector);
}

#[test]
fn redaction_toggle_reloads_source_and_never_retains_raw_text() {
    let mut app = app();
    open(
        &mut app,
        page(vec![event(0, EventKind::AssistantMessage, "hello")], vec![]),
    );
    let raw = format!(r#"{{"text":"key {SECRET} end"}}"#);

    assert!(app.open_selected_source());
    let (first, _) = source_request(&mut app);
    assert!(app.apply_source(first, record(&raw)));
    let out = draw(&app);
    assert!(!out.contains(SECRET), "redacted by default:\n{out}");
    assert!(out.contains("key"), "{out}");
    assert!(!app.redaction_revealed());

    // x clears the shown text and asks the worker for the record again.
    app.toggle_redaction();
    assert!(app.redaction_revealed());
    assert_eq!(app.source_view().unwrap().state, SourceState::Loading);
    let (second, loc) = source_request(&mut app);
    assert_ne!(first, second);
    assert_eq!(loc, locator(0));
    assert!(draw(&app).contains("redaction is OFF"));
    // The superseded response cannot fill the view.
    assert!(!app.apply_source(first, record(&raw)));
    assert!(app.apply_source(second, record(&raw)));
    assert!(draw(&app).contains(SECRET), "revealed after reload");

    // Back to redacted: the revealed text is gone and rebuilt from a reload.
    app.toggle_redaction();
    assert!(!app.redaction_revealed());
    assert_eq!(app.source_view().unwrap().state, SourceState::Loading);
    let (third, _) = source_request(&mut app);
    assert!(app.apply_source(third, record(&raw)));
    let out = draw(&app);
    assert!(!out.contains(SECRET), "{out}");
    let dbg = format!("{:?}", app.source_view().unwrap());
    assert!(!dbg.contains(SECRET) && !dbg.contains("key"), "{dbg}");

    // Reveal, close, reopen: redaction is back on and nothing was kept.
    app.toggle_redaction();
    let (fourth, _) = source_request(&mut app);
    assert!(app.apply_source(fourth, record(&raw)));
    assert!(app.close_overlay());
    assert!(app.source_view().is_none());
    assert!(!app.redaction_revealed());
    assert!(app.open_selected_source());
    let (fifth, _) = source_request(&mut app);
    assert!(app.apply_source(fifth, record(&raw)));
    assert!(!draw(&app).contains(SECRET));
}

#[test]
fn revealed_source_keeps_terminal_sanitization_and_is_bounded() {
    let mut app = app();
    open(
        &mut app,
        page(vec![event(0, EventKind::AssistantMessage, "hello")], vec![]),
    );
    app.toggle_redaction();
    assert_eq!(app.screen, Screen::Source);
    let (id, _) = source_request(&mut app);
    let raw = format!("\u{1b}[31mred{}", "a".repeat(200_000));
    assert!(app.apply_source(id, record(&raw)));
    let view = app.source_view().unwrap();
    let SourceState::Ready { text, truncated } = &view.state else {
        panic!("expected ready");
    };
    assert!(*truncated);
    assert!(text.len() <= agentscope::app::MAX_SOURCE_BYTES);
    assert!(!text.contains('\u{1b}'));
    assert!(!draw(&app).contains('\u{1b}'));
}

#[test]
fn toggling_redaction_after_the_record_left_the_window_does_not_request() {
    let mut app = app();
    open(
        &mut app,
        page(
            vec![event(0, EventKind::AssistantMessage, "hello")],
            vec![diagnostic(500, "unknown record type", false)],
        ),
    );
    assert!(app.open_diagnostics());
    assert!(app.open_diagnostic_source());
    let (id, _) = source_request(&mut app);
    assert!(app.apply_source(id, record("x")));

    // Reload the transcript with a new source generation: old locators are stale.
    app.invalidate_transcript(&key());
    let requests = app.take_requests();
    let reload_id = requests
        .iter()
        .find_map(|r| match r {
            WorkerRequest::LoadPage { request_id, .. } => Some(*request_id),
            _ => None,
        })
        .expect("reload page request");
    let mut moved = event(0, EventKind::AssistantMessage, "hello");
    moved.provenance.locator = locator_for(key(), 0, Some(4));
    assert!(app.apply_transcript(reload_id, page(vec![moved], vec![])));
    app.take_requests();

    app.toggle_redaction();
    assert!(
        app.take_requests().is_empty(),
        "stale locator is not requested"
    );
    assert!(draw(&app).contains("no longer part of the loaded transcript"));
}

#[test]
fn x_in_inspector_reloads_the_selected_record_and_reveal_is_scoped_to_it() {
    let mut app = app();
    open(
        &mut app,
        page(
            vec![
                event(0, EventKind::AssistantMessage, "a"),
                event(1, EventKind::AssistantMessage, "b"),
            ],
            vec![],
        ),
    );
    let raw = format!("key {SECRET}");
    app.toggle_redaction();
    assert_eq!(app.screen, Screen::Source);
    let (id, loc) = source_request(&mut app);
    assert_eq!(loc, locator(0));
    assert!(app.apply_source(id, record(&raw)));
    assert!(draw(&app).contains(SECRET));
    assert!(app.close_overlay());
    assert!(!app.redaction_revealed());

    // Another selection is never revealed by the earlier x.
    app.inspector_move(1);
    assert!(app.open_selected_source());
    let (id, loc) = source_request(&mut app);
    assert_eq!(loc, locator(100));
    assert!(app.apply_source(id, record(&raw)));
    assert!(!draw(&app).contains(SECRET));
    assert!(!app.revealed_for(&locator(0)));

    // x in Details and Diagnostics also reloads immediately.
    app.close_overlay();
    assert!(app.open_details());
    app.toggle_redaction();
    assert_eq!(app.screen, Screen::Source);
    assert_eq!(source_request(&mut app).1, locator(100));
    assert!(app.close_overlay());
    assert_eq!(app.screen, Screen::Details);
}

#[test]
fn a_transcript_reload_invalidates_the_open_source_and_stale_responses() {
    let mut app = app();
    open(
        &mut app,
        page(vec![event(0, EventKind::AssistantMessage, "hello")], vec![]),
    );
    assert!(app.open_selected_source());
    let (pending, _) = source_request(&mut app);

    app.invalidate_transcript(&key());
    let reload_id = app
        .take_requests()
        .iter()
        .find_map(|r| match r {
            WorkerRequest::LoadPage { request_id, .. } => Some(*request_id),
            _ => None,
        })
        .unwrap();
    let mut moved = event(0, EventKind::AssistantMessage, "hello");
    moved.provenance.locator = locator_for(key(), 0, Some(4));
    assert!(app.apply_transcript(reload_id, page(vec![moved], vec![])));
    app.take_requests();

    assert!(!app.apply_source(pending, record("generation 3 content")));
    let out = draw(&app);
    assert!(!out.contains("generation 3 content"));
    assert!(out.contains("no longer part of the loaded transcript"));
    assert!(matches!(
        app.source_view().unwrap().state,
        SourceState::Unavailable(_)
    ));
}

#[test]
fn diagnostics_selection_is_clamped_after_the_list_shrinks() {
    let mut app = app();
    open(
        &mut app,
        page(
            vec![event(0, EventKind::AssistantMessage, "hello")],
            vec![
                diagnostic(500, "a", false),
                diagnostic(600, "b", false),
                diagnostic(700, "c", false),
            ],
        ),
    );
    assert!(app.open_diagnostics());
    app.diagnostics_move(2);
    app.invalidate_transcript(&key());
    let reload_id = app
        .take_requests()
        .iter()
        .find_map(|r| match r {
            WorkerRequest::LoadPage { request_id, .. } => Some(*request_id),
            _ => None,
        })
        .unwrap();
    assert!(app.apply_transcript(
        reload_id,
        page(
            vec![event(0, EventKind::AssistantMessage, "hello")],
            vec![diagnostic(500, "a", false)],
        )
    ));
    app.take_requests();
    assert_eq!(app.diagnostics_selected(), 0);
    assert!(app.open_diagnostic_source());
    assert_eq!(source_request(&mut app).1, locator(500));
}
