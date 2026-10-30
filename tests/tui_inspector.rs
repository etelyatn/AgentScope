use std::collections::HashSet;
use std::time::Duration;

use agentscope::app::{App, MAX_LOADED_EVENTS};
use agentscope::domain::*;
use agentscope::time::format_local;
use agentscope::ui::{RosterContext, render};
use agentscope::worker::WorkerRequest;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;

const NOW: Timestamp = 1_800_000_000_000;

fn key() -> SessionKey {
    SessionKey {
        provider: Provider::Codex,
        provider_session_id: "sess-1".into(),
        source: SourceKey::File("sess-1.jsonl".into()),
    }
}

fn summary(count: usize) -> SessionSummary {
    SessionSummary {
        key: key(),
        title: "Inventory API".into(),
        title_derived: false,
        project: Some("RipperGame".into()),
        cwd: None,
        models: vec!["gpt-5.1-codex".into()],
        model_details: vec![],
        provider_version: None,
        created_at: Some(NOW - 86_400_000),
        last_event_at: Some(NOW - 540_000),
        last_persisted_at: Some(NOW - 540_000),
        last_completion: None,
        unfinished_work: true,
        displayed_event_count: count,
        hidden_records: 0,
        source_available: true,
        archived: false,
        context_window: Some(475_000),
        server_tools: Some(ServerToolCounts {
            web_search: 3,
            web_fetch: 1,
        }),
        lifecycle: Lifecycle::Unknown,
        process: ProcessAssociation::Candidates { count: 2 },
        usage: Some(UsageSummary {
            model: None,
            categories: TokenCategories::default(),
            total: Some(73_214),
            class: AggregationClass::Reported,
            cost: CostEstimate {
                usd: Some(0.23),
                class: AggregationClass::Estimated,
                as_of: None,
                note: None,
            },
        }),
        lineage: Lineage {
            parent: None,
            fork: None,
            sidechain: false,
        },
        warnings: vec![],
    }
}

fn app_with(s: SessionSummary) -> App {
    let mut app = App::new(
        NOW,
        Thresholds {
            quiet: Duration::from_secs(300),
            stall: Duration::from_secs(900),
        },
    );
    app.apply_roster(vec![s], vec![]);
    app
}

fn provenance() -> Provenance {
    Provenance {
        provider: Provider::Codex,
        source: SourceKey::File("sess-1.jsonl".into()),
        locator: SourceLocator {
            session: key(),
            kind: LocatorKind::FileOffset {
                path: "sess-1.jsonl".into(),
                offset: 0,
            },
            generation: None,
        },
        record_type: "test".into(),
        provider_version: None,
    }
}

fn text(s: &str) -> BoundedText {
    BoundedText {
        preview: s.into(),
        byte_len: s.len(),
        truncated: false,
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
        actor: match kind {
            EventKind::UserPrompt => Actor::User,
            EventKind::Tool => Actor::Tool,
            _ => Actor::Assistant,
        },
        text: Some(text(body)),
        tool: None,
        usage: None,
        provenance: provenance(),
    }
}

fn msg(i: u64) -> Event {
    event(i, EventKind::AssistantMessage, &format!("message {i}"))
}

fn anchor(i: u64) -> PageAnchor {
    PageAnchor {
        timestamp: Some(NOW - 3_000_000 + i as i64 * 60_000),
        source_order: i,
        stable_id: format!("e{i}"),
    }
}

fn page(events: Vec<Event>, next: Option<PageAnchor>, has_more: bool) -> TranscriptPage {
    TranscriptPage {
        events,
        diagnostics: vec![],
        next,
        has_more,
        truncated: false,
    }
}

fn forward_page(range: std::ops::Range<u64>, has_more: bool) -> TranscriptPage {
    let events: Vec<Event> = range.clone().map(msg).collect();
    page(events, Some(anchor(range.end - 1)), has_more)
}

fn backward_page(range: std::ops::Range<u64>, has_more: bool) -> TranscriptPage {
    let events: Vec<Event> = range.clone().map(msg).collect();
    page(events, Some(anchor(range.start)), has_more)
}

fn ctx() -> RosterContext {
    RosterContext {
        no_color: true,
        ..RosterContext::default()
    }
}

fn draw(app: &App, w: u16, h: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal.draw(|f| render(f, app, &ctx())).unwrap();
    let buf: &Buffer = terminal.backend().buffer();
    buf.content()
        .chunks(w as usize)
        .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn hm(ts: Timestamp) -> String {
    format_local(ts)[11..16].to_string()
}

/// Open the inspector and answer its `Select` with `first`.
fn open(app: &mut App, first: TranscriptPage) -> u64 {
    assert!(app.open_inspector());
    let requests = app.take_requests();
    let [WorkerRequest::Select { request_id, key: k }] = requests.as_slice() else {
        panic!("expected a single Select, got {requests:?}");
    };
    assert_eq!(k, &key());
    let id = *request_id;
    assert!(app.apply_transcript(id, first));
    id
}

/// Page requests only: the baseline count requested after opening away from the
/// tail is not a page load.
fn page_requests(requests: &[WorkerRequest]) -> Vec<WorkerRequest> {
    requests
        .iter()
        .filter(|r| !matches!(r, WorkerRequest::CountAfter { .. }))
        .cloned()
        .collect()
}

fn load_page(requests: &[WorkerRequest]) -> (u64, Option<PageAnchor>, PageDirection) {
    let requests = page_requests(requests);
    match requests.as_slice() {
        [
            WorkerRequest::LoadPage {
                request_id,
                anchor,
                direction,
                ..
            },
        ] => (*request_id, anchor.clone(), *direction),
        other => panic!("expected a single LoadPage, got {other:?}"),
    }
}

fn ids(app: &App) -> Vec<String> {
    app.transcript()
        .unwrap()
        .events()
        .iter()
        .map(|e| e.id.clone())
        .collect()
}

#[test]
fn renders_header_usage_evidence_and_timeline() {
    let mut app = app_with(summary(3));
    let user = event(0, EventKind::UserPrompt, "Implement inventory checks");
    let reply = event(1, EventKind::AssistantMessage, "I will inspect the code.");
    let t0 = user.timestamp.unwrap();
    open(&mut app, page(vec![user, reply], None, false));

    let evidence = app.session(&key()).unwrap().status.evidence.clone();
    let out = draw(&app, 110, 30);
    for want in [
        "< Back  CODEX / Inventory API",
        "Project: RipperGame",
        "Model: gpt-5.1-codex",
        "Session: sess-1",
        "Status: ",
        evidence.as_str(),
        "Tokens: 73,214 reported",
        "Cost: ~$0.23 estimated",
        "Context: 475k window",
        "Process: unmatched — 2 candidates",
        "user",
        "Implement inventory checks",
        "assistant",
        "I will inspect the code.",
        hm(t0).as_str(),
        "Esc back",
    ] {
        assert!(out.contains(want), "missing {want:?} in:\n{out}");
    }
}

#[test]
fn renders_reported_context_window_and_server_tool_counts_and_unknown_otherwise() {
    let mut app = app_with(summary(1));
    open(&mut app, page(vec![msg(0)], None, false));
    let out = draw(&app, 110, 30);
    assert!(out.contains("Context: 475k window"), "{out}");
    assert!(
        out.contains("Server tools: web_search 3 · web_fetch 1"),
        "{out}"
    );

    let mut bare = summary(1);
    bare.context_window = None;
    bare.server_tools = None;
    let mut app = app_with(bare);
    open(&mut app, page(vec![msg(0)], None, false));
    let out = draw(&app, 110, 30);
    assert!(out.contains("Context: Unknown"), "{out}");
    assert!(out.contains("Server tools: Unknown"), "{out}");
}

#[test]
fn expands_a_tool_row_and_shows_status_duration_exit_and_truncation() {
    let mut app = app_with(summary(2));
    let mut tool = event(1, EventKind::Tool, "");
    tool.text = None;
    tool.tool = Some(ToolInteraction {
        name: "shell".into(),
        call_id: Some("c1".into()),
        input: Some(BoundedText {
            preview: "cargo test --all".into(),
            byte_len: 9_000,
            truncated: true,
            locator: None,
        }),
        output: Some(text("2 failed assertions\nsecond line")),
        status: ToolStatus::Error,
        exit_code: Some(1),
        started_at: Some(NOW),
        completed_at: Some(NOW + 1_200),
        orphan: false,
    });
    open(&mut app, page(vec![msg(0), tool], None, false));

    let collapsed = draw(&app, 110, 30);
    assert!(collapsed.contains("shell cargo test --all"), "{collapsed}");
    assert!(collapsed.contains("[expand]"), "{collapsed}");
    assert!(!collapsed.contains("Duration:"), "{collapsed}");

    app.inspector_move(1);
    app.inspector_toggle();
    let out = draw(&app, 110, 30);
    for want in [
        "[collapse]",
        "Status: Error",
        "Duration: 1.2s",
        "Exit: 1",
        "Input: cargo test --all",
        "[truncated: showing 16 of 9000 bytes]",
        "Output: 2 failed assertions",
        "second line",
    ] {
        assert!(out.contains(want), "missing {want:?} in:\n{out}");
    }

    app.inspector_toggle();
    assert!(!draw(&app, 110, 30).contains("Duration:"));
}

#[test]
fn loads_forward_and_backward_pages_without_duplicates() {
    let mut app = app_with(summary(10));
    open(&mut app, forward_page(0..5, true));
    // Selection sits at the start of a 5-event window with newer pages left:
    // the next forward page is requested from the continuation anchor.
    let (id, anchor_sent, direction) = load_page(&app.take_requests());
    assert_eq!(direction, PageDirection::Forward);
    assert_eq!(anchor_sent, Some(anchor(4)));
    // A response for another request is ignored.
    assert!(!app.apply_transcript(id + 99, forward_page(5..10, false)));
    // The overlapping page is de-duplicated.
    assert!(app.apply_transcript(id, forward_page(3..10, false)));
    let expected: Vec<String> = (0..10).map(|i| format!("e{i}")).collect();
    assert_eq!(ids(&app), expected);
    assert!(page_requests(&app.take_requests()).is_empty());
    assert!(!app.transcript().unwrap().has_newer());

    // `G` always reloads the tail, Backward from none, even when the window
    // already ends there: content can change without the row count growing.
    app.inspector_last();
    let (tail_id, tail_anchor, direction) = load_page(&app.take_requests());
    assert_eq!((tail_anchor, direction), (None, PageDirection::Backward));
    assert!(app.apply_transcript(tail_id, backward_page(20..25, true)));
    assert_eq!(app.transcript().unwrap().selected(), 4, "lands on newest");
    let tail: Vec<String> = (20..25).map(|i| format!("e{i}")).collect();
    assert_eq!(ids(&app), tail);

    // Scrolling to the start loads the older page from the first event.
    app.inspector_move(-100);
    let (older_id, older_anchor, direction) = load_page(&app.take_requests());
    assert_eq!(direction, PageDirection::Backward);
    assert_eq!(older_anchor, Some(anchor(20)));
    assert!(app.apply_transcript(older_id, backward_page(15..20, false)));
    let all: Vec<String> = (15..25).map(|i| format!("e{i}")).collect();
    assert_eq!(ids(&app), all);
    assert_eq!(
        app.transcript().unwrap().selected(),
        5,
        "selection stays on the same event after prepending"
    );
    assert_eq!(
        app.transcript().unwrap().selected_event().unwrap().id,
        "e20"
    );
    assert!(!app.transcript().unwrap().has_older());
    let unique: HashSet<_> = ids(&app).into_iter().collect();
    assert_eq!(unique.len(), 10);
}

#[test]
fn initial_select_is_forward_from_start() {
    let mut app = app_with(summary(1));
    app.open_inspector();
    let requests = app.take_requests();
    assert!(
        matches!(requests.as_slice(), [WorkerRequest::Select { key: k, .. }] if *k == key()),
        "{requests:?}"
    );
}

#[test]
fn labels_reasoning_as_provider_persisted_and_images_as_placeholders() {
    let mut app = app_with(summary(2));
    let reasoning = event(0, EventKind::Reasoning, "weighing the options");
    let image = event(1, EventKind::UserPrompt, "[image]");
    open(&mut app, page(vec![reasoning, image], None, false));
    let out = draw(&app, 110, 30);
    assert!(out.contains("Provider reasoning (persisted)"), "{out}");
    assert!(out.contains("[image]"), "{out}");
    assert!(!out.contains("weighing the options"), "collapsed: {out}");

    app.inspector_toggle();
    let out = draw(&app, 110, 30);
    assert!(out.contains("weighing the options"), "{out}");
    assert!(out.contains("not hidden chain-of-thought"), "{out}");
}

#[test]
fn missing_timestamps_and_usage_show_unknown() {
    let mut bare = summary(1);
    bare.usage = None;
    let mut app = app_with(bare);
    let mut e = msg(0);
    e.timestamp = None;
    open(&mut app, page(vec![e], None, false));
    let out = draw(&app, 110, 30);
    assert!(out.contains("Tokens: Unknown · Cost: Unknown"), "{out}");
    let row = out.lines().find(|l| l.contains("message 0")).unwrap();
    assert!(row.contains('—'), "{row}");
}

#[test]
fn new_events_are_counted_without_scrolling_or_loading() {
    let mut app = app_with(summary(3));
    open(&mut app, forward_page(0..3, false));
    app.inspector_move(1);
    assert_eq!(app.new_event_count(), 0);

    app.apply_roster(vec![summary(6)], vec![]);
    // The count goes through the worker (`count_events_after`), anchored on
    // the newest loaded event.
    let requests = app.take_requests();
    let [
        WorkerRequest::CountAfter {
            request_id,
            anchor: a,
            ..
        },
    ] = requests.as_slice()
    else {
        panic!("expected CountAfter, got {requests:?}");
    };
    assert_eq!(a, &anchor(2));
    assert!(!app.apply_events_after(request_id + 9, 3), "stale response");
    assert!(app.apply_events_after(*request_id, 3));
    assert_eq!(app.new_event_count(), 3);
    assert_eq!(app.transcript().unwrap().selected(), 1, "no auto-scroll");
    assert_eq!(ids(&app).len(), 3, "no auto-load");
    assert!(page_requests(&app.take_requests()).is_empty());
    let out = draw(&app, 110, 30);
    assert!(out.contains("3 new"), "{out}");

    // An in-place change is a reload, not a new event.
    app.invalidate_transcript(&key());
    assert!(matches!(
        app.take_requests().as_slice(),
        [WorkerRequest::LoadPage {
            direction: PageDirection::Backward,
            anchor: None,
            ..
        }]
    ));
    assert_eq!(app.new_event_count(), 3);
}

#[test]
fn counts_after_a_backward_page_exclude_the_loaded_tail() {
    let mut app = app_with(summary(60));
    open(&mut app, forward_page(0..1, false));
    app.inspector_last();
    let (id, _, _) = load_page(&app.take_requests());
    // Tail page 20..60: its anchor is the first event, so 39 loaded events
    // follow it and a count of 42 means 3 are new.
    assert!(app.apply_transcript(id, backward_page(20..60, true)));
    app.apply_roster(vec![summary(63)], vec![]);
    let requests = app.take_requests();
    let [
        WorkerRequest::CountAfter {
            request_id,
            anchor: a,
            ..
        },
    ] = requests.as_slice()
    else {
        panic!("expected CountAfter, got {requests:?}");
    };
    assert_eq!(a, &anchor(20));
    assert!(app.apply_events_after(*request_id, 42));
    assert_eq!(app.new_event_count(), 3);
}

#[test]
fn loading_older_history_keeps_the_unread_count() {
    let mut app = app_with(summary(60));
    open(&mut app, forward_page(0..1, false));
    app.inspector_last();
    let (id, _, _) = load_page(&app.take_requests());
    assert!(app.apply_transcript(id, backward_page(20..60, true)));
    app.apply_roster(vec![summary(63)], vec![]);
    let requests = app.take_requests();
    let [WorkerRequest::CountAfter { request_id, .. }] = requests.as_slice() else {
        panic!("expected CountAfter, got {requests:?}");
    };
    assert!(app.apply_events_after(*request_id, 42));
    assert_eq!(app.new_event_count(), 3);

    app.inspector_move(isize::MIN);
    let (older_id, _, direction) = load_page(&app.take_requests());
    assert_eq!(direction, PageDirection::Backward);
    assert!(app.apply_transcript(older_id, backward_page(0..20, false)));
    assert_eq!(
        app.new_event_count(),
        3,
        "older pages do not refresh the tail"
    );
    assert!(draw(&app, 110, 30).contains("3 new"));

    // Reloading the newest end does clear it.
    app.inspector_last();
    let (tail_id, _, _) = load_page(&app.take_requests());
    assert!(app.apply_transcript(tail_id, backward_page(23..63, true)));
    assert_eq!(app.new_event_count(), 0);
}

#[test]
fn invalidation_rereads_the_boundary_and_chases_the_current_selection() {
    let mut app = app_with(summary(160));
    open(&mut app, forward_page(0..1, false));
    app.inspector_last();
    let (id, _, _) = load_page(&app.take_requests());
    assert!(app.apply_transcript(id, backward_page(100..160, true)));
    app.inspector_move(-29); // e130
    assert!(page_requests(&app.take_requests()).is_empty());
    assert_eq!(
        app.transcript().unwrap().selected_event().unwrap().id,
        "e130"
    );

    app.invalidate_transcript(&key());
    let (rid, from, direction) = load_page(&app.take_requests());
    assert_eq!((from, direction), (None, PageDirection::Backward));
    // The first page reaches the selection but not the old window start: the
    // old window stays on screen and the next page is requested.
    assert!(app.apply_transcript(rid, backward_page(120..161, true)));
    assert_eq!(ids(&app).len(), 60, "old window shown until the swap");
    let (rid, from, direction) = load_page(&app.take_requests());
    assert_eq!(
        (from, direction),
        (Some(anchor(120)), PageDirection::Backward)
    );
    // The user moves beyond the original target while staging is in flight.
    app.inspector_move(-20); // e110
    let mut older = backward_page(95..120, true);
    older.events[5].text = Some(text("BOUNDARY-CHANGED")); // e100
    assert!(app.apply_transcript(rid, older));

    let t = app.transcript().unwrap();
    assert_eq!(t.selected_event().unwrap().id, "e110");
    let loaded = ids(&app);
    assert_eq!(loaded.first().unwrap(), "e95");
    assert_eq!(loaded.last().unwrap(), "e160");
    let unique: HashSet<_> = loaded.iter().collect();
    assert_eq!(unique.len(), loaded.len());
    assert!(t.has_older() && !t.has_newer());
    let e100 = t.events().iter().find(|e| e.id == "e100").unwrap();
    assert_eq!(e100.text.as_ref().unwrap().preview, "BOUNDARY-CHANGED");
}

#[test]
fn invalidation_while_reading_history_rereads_from_the_window_end() {
    let mut app = app_with(summary(200));
    open(&mut app, forward_page(0..60, true));
    app.inspector_move(30);
    assert!(page_requests(&app.take_requests()).is_empty());
    app.invalidate_transcript(&key());
    // One page forward from the window end: anchors are exclusive, so its last
    // event anchors a backward read that re-reads the old last event.
    let (rid, from, direction) = load_page(&app.take_requests());
    assert_eq!(
        (from, direction),
        (Some(anchor(59)), PageDirection::Forward)
    );
    assert!(app.apply_transcript(rid, forward_page(60..70, true)));
    let (rid, from, direction) = load_page(&app.take_requests());
    assert_eq!(
        (from, direction),
        (Some(anchor(69)), PageDirection::Backward)
    );
    let mut middle = backward_page(30..69, true);
    middle.events[29].text = Some(text("LAST-CHANGED")); // e59, the old window end
    assert!(app.apply_transcript(rid, middle));
    let (rid, from, _) = load_page(&app.take_requests());
    assert_eq!(from, Some(anchor(30)));
    let mut first = backward_page(0..30, false);
    first.events[0].text = Some(text("FIRST-CHANGED"));
    assert!(app.apply_transcript(rid, first));
    let t = app.transcript().unwrap();
    assert_eq!(t.selected_event().unwrap().id, "e30");
    // The probe page is dropped; the window ends at the re-read old last event
    // and continues from its own anchor.
    assert_eq!(ids(&app).len(), 60);
    assert_eq!(ids(&app).last().unwrap(), "e59");
    assert!(t.has_newer() && !t.has_older());
    let preview = |i: usize| t.events()[i].text.as_ref().unwrap().preview.clone();
    assert_eq!(preview(0), "FIRST-CHANGED");
    assert_eq!(preview(59), "LAST-CHANGED");
    app.inspector_move(isize::MAX);
    let (_, from, direction) = load_page(&app.take_requests());
    assert_eq!(
        (from, direction),
        (Some(anchor(59)), PageDirection::Forward)
    );
}

#[test]
fn arrivals_before_the_baseline_response_stay_visible() {
    let mut app = app_with(summary(100));
    open(&mut app, forward_page(0..20, true));
    let requests = app.take_requests();
    let [WorkerRequest::CountAfter { request_id, .. }] = requests.as_slice() else {
        panic!("expected a baseline CountAfter, got {requests:?}");
    };
    // Three events arrive before the worker answers the baseline count.
    app.apply_roster(vec![summary(103)], vec![]);
    assert!(app.take_requests().is_empty(), "one count at a time");
    assert!(app.apply_events_after(*request_id, 83));
    assert_eq!(app.new_event_count(), 3);
    assert!(draw(&app, 110, 30).contains("3 new"));

    app.apply_roster(vec![summary(104)], vec![]);
    let requests = app.take_requests();
    let [WorkerRequest::CountAfter { request_id, .. }] = requests.as_slice() else {
        panic!("expected CountAfter, got {requests:?}");
    };
    assert!(app.apply_events_after(*request_id, 84));
    assert_eq!(app.new_event_count(), 4);
}

#[test]
fn a_full_probe_does_not_consume_the_reload_cap() {
    let cap = MAX_LOADED_EVENTS as u64;
    let mut app = app_with(summary(2 * cap as usize));
    open(&mut app, forward_page(0..cap, true)); // e0..e19999, e0 selected
    app.invalidate_transcript(&key());
    let (rid, from, direction) = load_page(&app.take_requests());
    assert_eq!(
        (from, direction),
        (Some(anchor(cap - 1)), PageDirection::Forward)
    );
    // A full probe page of events past the window end.
    assert!(app.apply_transcript(rid, forward_page(cap..2 * cap, true)));
    let (rid, from, direction) = load_page(&app.take_requests());
    assert_eq!(
        (from, direction),
        (Some(anchor(2 * cap - 1)), PageDirection::Backward)
    );
    // The backward read overlaps the probe, then reaches the whole old window.
    assert!(app.apply_transcript(rid, backward_page(cap..2 * cap - 1, true)));
    let (rid, from, _) = load_page(&app.take_requests());
    assert_eq!(from, Some(anchor(cap)));
    let mut old = backward_page(0..cap, false);
    old.events[0].text = Some(text("E0-CHANGED"));
    assert!(app.apply_transcript(rid, old));

    let t = app.transcript().unwrap();
    assert!(t.failed().is_none() && !t.reload_capped());
    assert_eq!(t.events().len(), MAX_LOADED_EVENTS);
    assert_eq!(t.selected_event().unwrap().id, "e0");
    assert_eq!(t.events()[0].text.as_ref().unwrap().preview, "E0-CHANGED");
    assert!(t.has_newer());
}

#[test]
fn history_reload_with_no_newer_events_falls_back_to_the_tail() {
    let mut app = app_with(summary(60));
    open(&mut app, forward_page(0..20, true));
    app.invalidate_transcript(&key());
    let (rid, from, direction) = load_page(&app.take_requests());
    assert_eq!(
        (from, direction),
        (Some(anchor(19)), PageDirection::Forward)
    );
    // The old last event is now the newest event.
    assert!(app.apply_transcript(rid, page(vec![], None, false)));
    let (rid, from, direction) = load_page(&app.take_requests());
    assert_eq!((from, direction), (None, PageDirection::Backward));
    assert!(app.apply_transcript(rid, backward_page(0..20, false)));
    assert_eq!(ids(&app).len(), 20);
    assert!(!app.transcript().unwrap().has_newer());
}

#[test]
fn arrivals_are_counted_when_opened_at_oldest_history() {
    let mut app = app_with(summary(100));
    open(&mut app, forward_page(0..20, true));
    // The initial page does not reach the tail: a baseline count from the
    // window end discounts the history that was already there.
    let requests = app.take_requests();
    let [
        WorkerRequest::CountAfter {
            request_id,
            anchor: a,
            ..
        },
    ] = requests.as_slice()
    else {
        panic!("expected a baseline CountAfter, got {requests:?}");
    };
    assert_eq!(a, &anchor(19));
    assert!(app.apply_events_after(*request_id, 80));
    assert_eq!(app.new_event_count(), 0);

    app.apply_roster(vec![summary(103)], vec![]);
    let requests = app.take_requests();
    let [WorkerRequest::CountAfter { request_id, .. }] = requests.as_slice() else {
        panic!("expected CountAfter, got {requests:?}");
    };
    assert!(app.apply_events_after(*request_id, 83));
    assert_eq!(app.new_event_count(), 3);
    assert!(draw(&app, 110, 30).contains("3 new"));
    assert_eq!(app.transcript().unwrap().selected(), 0, "no auto-scroll");
}

#[test]
fn a_reload_beyond_the_window_cap_is_visible_and_retry_jumps_to_the_tail() {
    let cap = MAX_LOADED_EVENTS as u64;
    let mut app = app_with(summary(40_000));
    open(&mut app, forward_page(0..1, false));
    app.inspector_last();
    let (id, _, _) = load_page(&app.take_requests());
    assert!(app.apply_transcript(id, backward_page(10_000..10_000 + cap, true)));
    app.inspector_move(isize::MIN); // e10000, the reading position
    let (older, _, _) = load_page(&app.take_requests());
    app.transcript_load_failed(older); // keep the window as it is
    app.invalidate_transcript(&key());
    let (rid, from, direction) = load_page(&app.take_requests());
    assert_eq!((from, direction), (None, PageDirection::Backward));
    // 10,000 events arrived: the position is now 30,000 events from the tail.
    assert!(app.apply_transcript(rid, backward_page(30_000..40_000, true)));
    let (rid, _, _) = load_page(&app.take_requests());
    assert!(app.apply_transcript(rid, backward_page(20_000..30_000, true)));

    let t = app.transcript().unwrap();
    assert!(t.failed().is_some() && t.reload_capped());
    assert!(t.loading().is_none());
    assert_eq!(t.selected_event().unwrap().id, "e10000", "position kept");
    assert_eq!(t.events().len(), MAX_LOADED_EVENTS);
    let out = draw(&app, 110, 30);
    assert!(out.contains("loaded-event limit"), "{out}");

    app.retry_transcript();
    let (tail, from, direction) = load_page(&app.take_requests());
    assert_eq!((from, direction), (None, PageDirection::Backward));
    assert!(app.apply_transcript(tail, backward_page(30_000..40_000, true)));
    let t = app.transcript().unwrap();
    assert!(t.failed().is_none() && !t.reload_capped());
    assert_eq!(t.selected_event().unwrap().id, "e39999");
}

fn tail_at_60() -> App {
    let mut app = app_with(summary(60));
    open(&mut app, forward_page(0..1, false));
    app.inspector_last();
    let (id, _, _) = load_page(&app.take_requests());
    assert!(app.apply_transcript(id, backward_page(20..60, true)));
    app
}

#[test]
fn an_outstanding_count_survives_an_older_load() {
    let mut app = tail_at_60();
    app.apply_roster(vec![summary(63)], vec![]);
    let requests = app.take_requests();
    let [WorkerRequest::CountAfter { request_id, .. }] = requests.as_slice() else {
        panic!("{requests:?}");
    };
    app.inspector_move(isize::MIN);
    let (older_id, _, _) = load_page(&app.take_requests());
    assert!(
        app.apply_events_after(*request_id, 42),
        "count not discarded"
    );
    assert_eq!(app.new_event_count(), 3);
    assert!(app.apply_transcript(older_id, backward_page(0..20, false)));
    assert_eq!(app.new_event_count(), 3);
}

#[test]
fn arrivals_are_counted_while_older_history_is_displayed() {
    let mut app = tail_at_60();
    app.inspector_move(isize::MIN);
    let (older_id, _, _) = load_page(&app.take_requests());
    assert!(app.apply_transcript(older_id, backward_page(0..20, false)));

    app.apply_roster(vec![summary(66)], vec![]);
    let requests = app.take_requests();
    let [
        WorkerRequest::CountAfter {
            request_id,
            anchor: a,
            ..
        },
    ] = requests.as_slice()
    else {
        panic!("{requests:?}");
    };
    assert_eq!(a, &anchor(20));
    assert!(app.apply_events_after(*request_id, 45));
    assert_eq!(app.new_event_count(), 6);
}

#[test]
fn r_retries_a_failed_older_or_newer_load() {
    let mut app = tail_at_60();
    app.inspector_move(isize::MIN);
    let (id, from, _) = load_page(&app.take_requests());
    assert_eq!(from, Some(anchor(20)));
    app.transcript_load_failed(id);
    assert!(app.transcript().unwrap().failed().is_some());
    app.inspector_move(1);
    assert!(
        page_requests(&app.take_requests()).is_empty(),
        "failed edge load is not auto-retried"
    );

    app.retry_transcript();
    let (retry, from, direction) = load_page(&app.take_requests());
    assert_eq!(
        (from, direction),
        (Some(anchor(20)), PageDirection::Backward)
    );
    assert_ne!(retry, id);
    assert!(app.transcript().unwrap().failed().is_none());
    assert!(app.apply_transcript(retry, backward_page(0..20, false)));
    assert_eq!(ids(&app).len(), 60);

    // Newer edge.
    let mut app = app_with(summary(20));
    open(&mut app, forward_page(0..5, true));
    let (id, _, _) = load_page(&app.take_requests());
    app.transcript_load_failed(id);
    app.retry_transcript();
    let (_, from, direction) = load_page(&app.take_requests());
    assert_eq!(from, Some(anchor(4)));
    assert_eq!(direction, PageDirection::Forward);
}

#[test]
fn g_reloads_the_tail_even_when_the_row_count_is_unchanged() {
    let mut app = app_with(summary(3));
    open(&mut app, forward_page(0..3, false));
    app.inspector_last();
    let (_, anchor_sent, direction) = load_page(&app.take_requests());
    assert_eq!((anchor_sent, direction), (None, PageDirection::Backward));
}

#[test]
fn keeps_the_selected_row_visible_when_expanded_rows_fill_the_viewport() {
    let mut app = app_with(summary(5));
    let long: String = (0..8).map(|i| format!("line {i}\n")).collect();
    let events: Vec<Event> = (0..4)
        .map(|i| event(i, EventKind::AssistantMessage, &format!("{long}tail {i}")))
        .chain([event(4, EventKind::UserPrompt, "SELECTED-ROW")])
        .collect();
    open(&mut app, page(events, None, false));
    for _ in 0..4 {
        app.inspector_toggle();
        app.inspector_move(1);
    }
    assert_eq!(app.transcript().unwrap().selected(), 4);
    let out = draw(&app, 80, 20);
    assert!(out.contains("> "), "{out}");
    assert!(out.contains("SELECTED-ROW"), "{out}");
}

#[test]
fn a_failed_initial_load_is_shown_and_retried() {
    let mut app = app_with(summary(1));
    assert!(app.open_inspector());
    let requests = app.take_requests();
    let [WorkerRequest::Select { request_id, .. }] = requests.as_slice() else {
        panic!("{requests:?}");
    };
    app.transcript_load_failed(*request_id);
    let out = draw(&app, 110, 30);
    assert!(!out.contains("Loading…"), "{out}");
    assert!(out.contains("Could not load this transcript"), "{out}");
    assert!(out.contains("r retries"), "{out}");

    app.retry_transcript();
    let requests = app.take_requests();
    let [
        WorkerRequest::Select {
            request_id: retry, ..
        },
    ] = requests.as_slice()
    else {
        panic!("expected a new Select, got {requests:?}");
    };
    assert_ne!(retry, request_id);
    assert!(app.apply_transcript(*retry, page(vec![msg(0)], None, false)));
    let out = draw(&app, 110, 30);
    assert!(out.contains("message 0"), "{out}");
    assert!(!out.contains("r retries"), "{out}");
}

#[test]
fn a_superseded_load_failure_does_not_abort_the_current_request() {
    let mut app = app_with(summary(20));
    open(&mut app, forward_page(0..5, true));
    let (newer_id, _, _) = load_page(&app.take_requests());
    app.inspector_last(); // supersedes the newer-page load
    let (tail_id, _, _) = load_page(&app.take_requests());
    assert_ne!(newer_id, tail_id);

    app.transcript_load_failed(newer_id);
    assert!(app.transcript().unwrap().failed().is_none());
    assert!(app.transcript().unwrap().loading().is_some());
    assert!(app.apply_transcript(tail_id, backward_page(15..20, false)));
    assert_eq!(ids(&app).len(), 5);

    // A failure for the request that is current does abort it.
    app.inspector_last();
    let (current, _, _) = load_page(&app.take_requests());
    app.transcript_load_failed(current);
    assert!(app.transcript().unwrap().failed().is_some());
}

#[test]
fn a_replacement_page_larger_than_the_window_is_rejected() {
    let mut app = app_with(summary(1));
    assert!(app.open_inspector());
    let requests = app.take_requests();
    let [WorkerRequest::Select { request_id, .. }] = requests.as_slice() else {
        panic!("{requests:?}");
    };
    let big = forward_page(0..(MAX_LOADED_EVENTS as u64 + 1), true);
    assert!(app.apply_transcript(*request_id, big));
    assert!(ids(&app).is_empty());
    assert!(app.transcript().unwrap().failed().is_some());
    // A page of exactly the cap fits and keeps its continuation anchor.
    app.retry_transcript();
    let requests = app.take_requests();
    let [WorkerRequest::Select { request_id, .. }] = requests.as_slice() else {
        panic!("{requests:?}");
    };
    let full = forward_page(0..MAX_LOADED_EVENTS as u64, true);
    assert!(app.apply_transcript(*request_id, full));
    assert_eq!(ids(&app).len(), MAX_LOADED_EVENTS);
    assert!(app.transcript().unwrap().has_newer());
}

#[test]
fn loaded_events_stay_bounded_and_pages_stay_contiguous() {
    let half = (MAX_LOADED_EVENTS / 2 + 2_000) as u64;
    let mut app = app_with(summary(2 * half as usize));
    open(&mut app, forward_page(0..half, true));
    app.inspector_move(isize::MAX);
    let (id, from, direction) = load_page(&app.take_requests());
    assert_eq!(
        (from, direction),
        (Some(anchor(half - 1)), PageDirection::Forward)
    );
    assert!(app.apply_transcript(id, forward_page(half..2 * half, false)));

    let loaded = ids(&app);
    assert!(loaded.len() <= MAX_LOADED_EVENTS, "{}", loaded.len());
    assert_eq!(loaded.last().unwrap(), &format!("e{}", 2 * half - 1));
    // The window is one contiguous run, so nothing was skipped or repeated.
    let first: u64 = loaded[0][1..].parse().unwrap();
    for (i, id) in loaded.iter().enumerate() {
        assert_eq!(id, &format!("e{}", first + i as u64));
    }
    assert!(app.transcript().unwrap().has_older());

    // Scrolling back up requests exactly the events just before the window.
    app.inspector_move(isize::MIN);
    let (id, from, direction) = load_page(&app.take_requests());
    assert_eq!(direction, PageDirection::Backward);
    let from = from.expect("older anchor");
    assert_eq!(from.stable_id, format!("e{first}"));
    assert!(app.apply_transcript(id, backward_page(first - 100..first, true)));
    let loaded = ids(&app);
    assert!(loaded.len() <= MAX_LOADED_EVENTS);
    let first_now: u64 = loaded[0][1..].parse().unwrap();
    assert_eq!(first_now, first - 100);
    let unique: HashSet<_> = loaded.iter().collect();
    assert_eq!(unique.len(), loaded.len());
}

#[test]
fn escape_returns_to_the_roster() {
    let mut app = app_with(summary(1));
    open(&mut app, forward_page(0..1, false));
    app.close_inspector();
    assert!(app.transcript().is_none());
    let out = draw(&app, 110, 30);
    assert!(out.contains("AgentScope"), "{out}");
}

#[test]
fn opencode_shaped_activity_counts_surface_arrivals_before_the_baseline() {
    // OpenCode publishes a message-activity total, not displayed rows: 10
    // messages here expand to 20 loaded events.
    let mut app = app_with(summary(10));
    open(&mut app, forward_page(0..20, true));
    let requests = app.take_requests();
    let [WorkerRequest::CountAfter { request_id, .. }] = requests.as_slice() else {
        panic!("expected a baseline CountAfter, got {requests:?}");
    };
    // Three messages arrive before the baseline count executes.
    app.apply_roster(vec![summary(13)], vec![]);
    assert!(app.apply_events_after(*request_id, 83));
    assert_eq!(app.new_event_count(), 3, "not 0 and not 1");
    assert!(draw(&app, 110, 30).contains("3 new"));
}
