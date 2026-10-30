//! Keyboard flow: discover, open, expand, search, filter, sort, help, and back.
//! Keys go through `ui::handle_key`; worker events through `ui::apply_worker_event`.

use std::time::Duration;

use agentscope::app::{App, Panel, Screen};
use agentscope::domain::*;
use agentscope::ui::{KeyAction, RosterContext, apply_worker_event, handle_key, render};
use agentscope::worker::{WorkerEvent, WorkerRequest};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

const NOW: Timestamp = 1_800_000_000_000;

fn key_of(provider: Provider, id: &str) -> SessionKey {
    SessionKey {
        provider,
        provider_session_id: id.into(),
        source: SourceKey::File(format!("{id}.jsonl").into()),
    }
}

fn session(
    provider: Provider,
    id: &str,
    title: &str,
    project: &str,
    cwd: &str,
    age_secs: i64,
    tokens: Option<u64>,
) -> SessionSummary {
    SessionSummary {
        key: key_of(provider, id),
        title: title.into(),
        title_derived: false,
        project: Some(project.into()),
        cwd: Some(cwd.into()),
        models: vec![],
        model_details: vec![],
        provider_version: None,
        created_at: Some(NOW - 86_400_000),
        last_event_at: Some(NOW - age_secs * 1000),
        last_persisted_at: Some(NOW - age_secs * 1000),
        last_completion: None,
        unfinished_work: true,
        displayed_event_count: 3,
        hidden_records: 0,
        source_available: true,
        archived: false,
        context_window: None,
        server_tools: None,
        lifecycle: Lifecycle::Unknown,
        process: ProcessAssociation::Unmatched,
        usage: tokens.map(|t| UsageSummary {
            model: None,
            categories: TokenCategories::default(),
            total: Some(t),
            class: AggregationClass::Reported,
            cost: CostEstimate {
                usd: None,
                class: AggregationClass::Unknown,
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

fn roster() -> Vec<SessionSummary> {
    vec![
        session(
            Provider::Codex,
            "codex-1",
            "Inventory API",
            "RipperGame",
            "/work/ripper",
            30,
            Some(91_000),
        ),
        session(
            Provider::Claude,
            "claude-1",
            "Scanner refactor",
            "RipperGame",
            "/work/ripper",
            600,
            Some(73_000),
        ),
        session(
            Provider::OpenCode,
            "oc-9",
            "PR review",
            "CortexSandbox",
            "/work/sandbox",
            7_200,
            None,
        ),
    ]
}

fn scan() -> ScanProgress {
    ScanProgress {
        provider: Provider::Codex,
        scanned: 3,
        total: Some(3),
        done: true,
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

fn discover(app: &mut App, ctx: &mut RosterContext, generation: u64) {
    apply_worker_event(
        app,
        ctx,
        WorkerEvent::Roster {
            generation,
            sessions: roster(),
            warnings: vec![],
            scan: scan(),
        },
    );
}

fn press(app: &mut App, code: KeyCode) -> KeyAction {
    handle_key(app, KeyEvent::new(code, KeyModifiers::NONE))
}

fn ch(app: &mut App, c: char) -> KeyAction {
    let mods = if c.is_uppercase() {
        KeyModifiers::SHIFT
    } else {
        KeyModifiers::NONE
    };
    handle_key(app, KeyEvent::new(KeyCode::Char(c), mods))
}

fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        assert_eq!(ch(app, c), KeyAction::None, "typing {c:?}");
    }
}

fn draw(app: &App, w: u16, h: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal.draw(|f| render(f, app, &ctx())).unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .chunks(w as usize)
        .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn titles(app: &App) -> Vec<String> {
    app.visible_rows()
        .iter()
        .map(|r| r.session.title.clone())
        .collect()
}

// ---- transcript fixtures --------------------------------------------------

fn provenance(k: &SessionKey) -> Provenance {
    Provenance {
        provider: k.provider,
        source: k.source.clone(),
        locator: SourceLocator {
            session: k.clone(),
            kind: LocatorKind::FileOffset {
                path: "x.jsonl".into(),
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

fn event(k: &SessionKey, i: u64, kind: EventKind, body: &str) -> Event {
    Event {
        id: format!("e{i}"),
        session: k.clone(),
        timestamp: Some(NOW - 3_000_000 + i as i64 * 60_000),
        source_order: i,
        kind,
        actor: Actor::Assistant,
        text: Some(text(body)),
        tool: None,
        usage: None,
        provenance: provenance(k),
    }
}

fn anchor(i: u64) -> PageAnchor {
    PageAnchor {
        timestamp: Some(NOW - 3_000_000 + i as i64 * 60_000),
        source_order: i,
        stable_id: format!("e{i}"),
    }
}

fn transcript_page(k: &SessionKey, bodies: &[&str]) -> TranscriptPage {
    let events: Vec<Event> = bodies
        .iter()
        .enumerate()
        .map(|(i, b)| event(k, i as u64, EventKind::AssistantMessage, b))
        .collect();
    TranscriptPage {
        next: Some(anchor(events.len() as u64 - 1)),
        events,
        diagnostics: vec![],
        has_more: false,
        truncated: false,
    }
}

/// Enter on the roster, then answer the resulting `Select` with `bodies`.
fn open_with(app: &mut App, ctx: &mut RosterContext, bodies: &[&str]) {
    assert_eq!(press(app, KeyCode::Enter), KeyAction::None);
    assert_eq!(app.screen, Screen::Inspector);
    let requests = app.take_requests();
    let Some(WorkerRequest::Select { request_id, key }) = requests.first() else {
        panic!("expected Select, got {requests:?}");
    };
    let page = transcript_page(key, bodies);
    apply_worker_event(
        app,
        ctx,
        WorkerEvent::Transcript {
            request_id: *request_id,
            page,
        },
    );
}

// ---- tests ------------------------------------------------------------------

#[test]
fn keyboard_flow_discovers_opens_expands_searches_and_returns() {
    let mut app = new_app();
    let mut ctx = ctx();
    discover(&mut app, &mut ctx, 1);
    assert_eq!(app.visible_rows().len(), 3);
    assert_eq!(app.selected_key(), Some(key_of(Provider::Codex, "codex-1")));

    open_with(
        &mut app,
        &mut ctx,
        &[
            "Implement inventory checks",
            "I will inspect the code\nsecond line of detail",
            "Running cargo test",
            "Inventory tests pass",
        ],
    );
    assert_eq!(app.transcript().unwrap().events().len(), 4);

    // Expand the second event.
    ch(&mut app, 'j');
    press(&mut app, KeyCode::Enter);
    let t = app.transcript().unwrap();
    assert_eq!(t.selected(), 1);
    assert!(t.is_expanded(t.selected_event().unwrap()));

    // Search the loaded events.
    assert_eq!(ch(&mut app, '/'), KeyAction::None);
    assert!(app.is_editing());
    type_text(&mut app, "inventory");
    let screen = draw(&app, 100, 30);
    assert!(screen.contains("match 2/2 (loaded events)"), "{screen}");
    press(&mut app, KeyCode::Enter);
    assert!(!app.is_editing());
    assert_eq!(
        app.transcript().unwrap().selected(),
        3,
        "first match at or after the selection"
    );
    ch(&mut app, 'n');
    assert_eq!(app.transcript().unwrap().selected(), 0, "n wraps");
    let screen = draw(&app, 100, 30);
    assert!(screen.contains("match 1/2 (loaded events)"), "{screen}");

    // Back to the roster; the selection is the same session.
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.screen, Screen::Roster);
    assert!(app.transcript().is_none());
    assert_eq!(app.selected_key(), Some(key_of(Provider::Codex, "codex-1")));
    assert_eq!(ch(&mut app, 'q'), KeyAction::Quit);
}

#[test]
fn slash_searches_roster_and_loaded_transcript_with_n_and_shift_n() {
    let mut app = new_app();
    let mut ctx = ctx();
    discover(&mut app, &mut ctx, 1);

    // Roster search covers title, project, cwd, and session id; case-insensitive.
    ch(&mut app, '/');
    type_text(&mut app, "SANDBOX");
    assert_eq!(titles(&app), vec!["PR review"]);
    assert_eq!(app.selected_key(), Some(key_of(Provider::OpenCode, "oc-9")));
    press(&mut app, KeyCode::Backspace);
    press(&mut app, KeyCode::Backspace);
    press(&mut app, KeyCode::Backspace);
    press(&mut app, KeyCode::Backspace);
    press(&mut app, KeyCode::Backspace);
    press(&mut app, KeyCode::Backspace);
    press(&mut app, KeyCode::Backspace);
    assert_eq!(app.visible_rows().len(), 3, "empty query shows everything");
    type_text(&mut app, "claude-1");
    assert_eq!(titles(&app), vec!["Scanner refactor"]);
    press(&mut app, KeyCode::Enter);
    assert!(!app.is_editing());
    assert_eq!(
        titles(&app),
        vec!["Scanner refactor"],
        "Enter keeps the query"
    );

    // Esc cancels an edit and restores the committed query.
    ch(&mut app, '/');
    type_text(&mut app, "zzz");
    assert!(app.visible_rows().is_empty());
    press(&mut app, KeyCode::Esc);
    assert_eq!(titles(&app), vec!["Scanner refactor"]);
    assert_eq!(app.screen, Screen::Roster);

    // Clear with Ctrl+u inside the input, then `q` is a character.
    ch(&mut app, '/');
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
    );
    assert_eq!(app.visible_rows().len(), 3);
    assert_eq!(
        ch(&mut app, 'q'),
        KeyAction::None,
        "q is text while editing"
    );
    assert_eq!(app.search_input().map(|(_, s)| s), Some("q"));
    press(&mut app, KeyCode::Backspace);
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.visible_rows().len(), 3);

    // Transcript search: loaded events only, n/N cycle and wrap.
    open_with(
        &mut app,
        &mut ctx,
        &["alpha one", "beta", "ALPHA two", "gamma", "alpha three"],
    );
    ch(&mut app, '/');
    type_text(&mut app, "alpha");
    press(&mut app, KeyCode::Enter);
    let t = app.transcript().unwrap();
    assert_eq!((t.selected(), t.match_count()), (0, 3));
    ch(&mut app, 'n');
    assert_eq!(app.transcript().unwrap().selected(), 2);
    ch(&mut app, 'n');
    assert_eq!(app.transcript().unwrap().selected(), 4);
    ch(&mut app, 'n');
    assert_eq!(app.transcript().unwrap().selected(), 0, "n wraps");
    ch(&mut app, 'N');
    assert_eq!(app.transcript().unwrap().selected(), 4, "N wraps backwards");
    ch(&mut app, 'N');
    assert_eq!(app.transcript().unwrap().selected(), 2);
    let screen = draw(&app, 100, 30);
    assert!(screen.contains("match 2/3 (loaded events)"), "{screen}");

    // No match is reported, and the selection stays put.
    ch(&mut app, '/');
    type_text(&mut app, "-nothing");
    assert_eq!(app.transcript().unwrap().selected(), 2);
    let screen = draw(&app, 100, 30);
    assert!(screen.contains("no matches (loaded events)"), "{screen}");
    // Esc restores the previous query and selection.
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.screen, Screen::Inspector);
    assert_eq!(app.transcript().unwrap().search_query(), "alpha");
    assert_eq!(app.transcript().unwrap().selected(), 2);
}

#[test]
fn filter_panel_and_sort_keys_update_the_roster() {
    let mut app = new_app();
    let mut ctx = ctx();
    discover(&mut app, &mut ctx, 1);
    assert_eq!(
        titles(&app),
        vec!["Inventory API", "Scanner refactor", "PR review"]
    );

    // Provider filter: rows are Codex, Claude, OpenCode, ...
    assert_eq!(ch(&mut app, 'f'), KeyAction::None);
    assert_eq!(app.panel(), Panel::Filter);
    let screen = draw(&app, 100, 30);
    assert!(screen.contains("Filter"), "{screen}");
    assert_eq!(
        ch(&mut app, 'q'),
        KeyAction::None,
        "q is not a quit key inside the filter panel"
    );
    assert_eq!(app.panel(), Panel::Filter);
    ch(&mut app, 'j');
    press(&mut app, KeyCode::Enter);
    assert_eq!(titles(&app), vec!["Scanner refactor"]);
    // Selection moved to the only remaining row.
    assert_eq!(
        app.selected_key(),
        Some(key_of(Provider::Claude, "claude-1"))
    );
    // Multi-select within a group.
    ch(&mut app, 'k');
    press(&mut app, KeyCode::Enter);
    assert_eq!(titles(&app), vec!["Inventory API", "Scanner refactor"]);
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.panel(), Panel::None);

    // Filters compose with search.
    ch(&mut app, '/');
    type_text(&mut app, "scanner");
    press(&mut app, KeyCode::Enter);
    assert_eq!(titles(&app), vec!["Scanner refactor"]);
    let screen = draw(&app, 100, 30);
    assert!(screen.contains("provider=Codex,Claude"), "{screen}");
    assert!(screen.contains("search=scanner"), "{screen}");

    // Clear everything with `c` in the panel; the search text stays.
    ch(&mut app, 'f');
    ch(&mut app, 'c');
    press(&mut app, KeyCode::Esc);
    assert_eq!(titles(&app), vec!["Scanner refactor"]);
    assert!(app.filter().providers.is_none());
    ch(&mut app, '/');
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
    );
    press(&mut app, KeyCode::Enter);

    // Usage and project rows compose too.
    ch(&mut app, 'f');
    let items = app.filter_items();
    let usage_unknown = items
        .iter()
        .position(|i| {
            i.target
                == agentscope::app::FilterTarget::Usage(agentscope::app::UsageAvailability::Unknown)
        })
        .unwrap();
    for _ in 0..usage_unknown {
        ch(&mut app, 'j');
    }
    press(&mut app, KeyCode::Enter);
    assert_eq!(titles(&app), vec!["PR review"]);
    press(&mut app, KeyCode::Esc);

    // Sort: `s` cycles; the header names the mode and direction, `S` reverses.
    ch(&mut app, 'f');
    ch(&mut app, 'c');
    press(&mut app, KeyCode::Esc);
    ch(&mut app, 's');
    let screen = draw(&app, 100, 30);
    assert!(screen.contains("Sort: provider (asc)"), "{screen}");
    assert_eq!(
        titles(&app),
        vec!["Inventory API", "Scanner refactor", "PR review"]
    );
    ch(&mut app, 's');
    let screen = draw(&app, 100, 30);
    assert!(screen.contains("Sort: project (asc)"), "{screen}");
    assert_eq!(titles(&app)[0], "PR review");
    ch(&mut app, 'S');
    let screen = draw(&app, 100, 30);
    assert!(screen.contains("Sort: project (desc)"), "{screen}");
    ch(&mut app, 's');
    ch(&mut app, 's');
    let screen = draw(&app, 100, 30);
    assert!(screen.contains("Sort: status"), "{screen}");
}

#[test]
fn help_is_contextual_and_closes_without_quitting() {
    let mut app = new_app();
    let mut ctx = ctx();
    discover(&mut app, &mut ctx, 1);
    assert_eq!(ch(&mut app, '?'), KeyAction::None);
    assert_eq!(app.panel(), Panel::Help);
    let screen = draw(&app, 100, 30);
    assert!(screen.contains("Help"), "{screen}");
    assert!(screen.contains("filter"), "{screen}");
    // Other keys do not leak through the panel.
    ch(&mut app, 'j');
    ch(&mut app, 's');
    assert_eq!(app.selected_key(), Some(key_of(Provider::Codex, "codex-1")));
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.panel(), Panel::None);

    open_with(&mut app, &mut ctx, &["one", "two"]);
    ch(&mut app, '?');
    let screen = draw(&app, 100, 30);
    assert!(screen.contains("n/N"), "{screen}");
    ch(&mut app, '?');
    assert_eq!(app.panel(), Panel::None);
    assert_eq!(app.screen, Screen::Inspector);

    // Fits the smallest supported terminal.
    ch(&mut app, '?');
    let screen = draw(&app, 60, 20);
    assert!(screen.contains("Help"), "{screen}");
}

#[test]
fn tab_cycles_panes_and_ctrl_c_always_quits() {
    let mut app = new_app();
    let mut ctx = ctx();
    discover(&mut app, &mut ctx, 1);
    open_with(&mut app, &mut ctx, &["one", "two"]);
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.screen, Screen::Details);
    press(&mut app, KeyCode::BackTab);
    assert_eq!(app.screen, Screen::Inspector);
    press(&mut app, KeyCode::BackTab);
    assert_eq!(app.screen, Screen::Roster);
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.screen, Screen::Inspector);

    ch(&mut app, '/');
    assert_eq!(
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
        ),
        KeyAction::Quit
    );
}

fn backward_page_of(k: &SessionKey, range: std::ops::Range<u64>, has_more: bool) -> TranscriptPage {
    let events: Vec<Event> = range
        .clone()
        .map(|i| event(k, i, EventKind::AssistantMessage, &format!("message {i}")))
        .collect();
    TranscriptPage {
        next: Some(anchor(range.start)),
        events,
        diagnostics: vec![],
        has_more,
        truncated: false,
    }
}

#[test]
fn esc_restores_the_original_roster_selection_when_the_query_excluded_it() {
    let mut app = new_app();
    let mut ctx = ctx();
    discover(&mut app, &mut ctx, 1);
    ch(&mut app, 'j');
    assert_eq!(
        app.selected_key(),
        Some(key_of(Provider::Claude, "claude-1"))
    );
    ch(&mut app, '/');
    type_text(&mut app, "sandbox");
    assert_eq!(app.selected_key(), Some(key_of(Provider::OpenCode, "oc-9")));
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.visible_rows().len(), 3);
    assert_eq!(
        app.selected_key(),
        Some(key_of(Provider::Claude, "claude-1")),
        "Esc returns to the session selected before the search"
    );
}

#[test]
fn transcript_search_origin_survives_an_older_page_prepending() {
    let mut app = new_app();
    let mut ctx = ctx();
    discover(&mut app, &mut ctx, 1);
    press(&mut app, KeyCode::Enter);
    let k = key_of(Provider::Codex, "codex-1");
    let requests = app.take_requests();
    let Some(WorkerRequest::Select { request_id, .. }) = requests.first() else {
        panic!("expected Select, got {requests:?}");
    };
    apply_worker_event(
        &mut app,
        &mut ctx,
        WorkerEvent::Transcript {
            request_id: *request_id,
            page: transcript_page(&k, &["only"]),
        },
    );
    ch(&mut app, 'G');
    let requests = app.take_requests();
    let Some(WorkerRequest::LoadPage { request_id, .. }) = requests.last() else {
        panic!("expected tail load, got {requests:?}");
    };
    apply_worker_event(
        &mut app,
        &mut ctx,
        WorkerEvent::Transcript {
            request_id: *request_id,
            page: backward_page_of(&k, 10..14, true),
        },
    );
    // Start the search at e12 and type a query that jumps elsewhere.
    ch(&mut app, 'j');
    ch(&mut app, 'j');
    let at = app
        .transcript()
        .unwrap()
        .selected_event()
        .unwrap()
        .id
        .clone();
    ch(&mut app, '/');
    type_text(&mut app, "message 13");
    // An older page arrives while the input is open and prepends events.
    let older = app
        .take_requests()
        .into_iter()
        .find_map(|r| match r {
            WorkerRequest::LoadPage {
                request_id,
                direction: PageDirection::Backward,
                ..
            } => Some(request_id),
            _ => None,
        })
        .expect("older page requested near the window edge");
    apply_worker_event(
        &mut app,
        &mut ctx,
        WorkerEvent::Transcript {
            request_id: older,
            page: backward_page_of(&k, 6..10, true),
        },
    );
    press(&mut app, KeyCode::Esc);
    let t = app.transcript().unwrap();
    assert_eq!(t.events().first().unwrap().id, "e6");
    assert_eq!(
        t.selected_event().unwrap().id,
        at,
        "same event after cancel"
    );
}

#[test]
fn tab_focus_cycling_retains_the_transcript_and_its_state() {
    let mut app = new_app();
    let mut ctx = ctx();
    discover(&mut app, &mut ctx, 1);
    open_with(&mut app, &mut ctx, &["alpha", "beta", "alpha two"]);
    ch(&mut app, 'j');
    press(&mut app, KeyCode::Enter);
    ch(&mut app, '/');
    type_text(&mut app, "alpha");
    press(&mut app, KeyCode::Enter);
    let selected = app.transcript().unwrap().selected();

    press(&mut app, KeyCode::Tab); // details
    press(&mut app, KeyCode::Tab); // roster
    assert_eq!(app.screen, Screen::Roster);
    assert!(app.transcript().is_some());
    press(&mut app, KeyCode::Tab); // back to the same transcript
    assert_eq!(app.screen, Screen::Inspector);
    assert!(app.take_requests().is_empty(), "no reload on focus change");
    press(&mut app, KeyCode::BackTab);
    press(&mut app, KeyCode::Tab);
    let t = app.transcript().unwrap();
    assert_eq!(t.events().len(), 3);
    assert_eq!(t.selected(), selected);
    assert_eq!(t.search_query(), "alpha");
    assert!(t.is_expanded(&t.events()[1]));
    // Esc is the explicit close.
    press(&mut app, KeyCode::Esc);
    assert!(app.transcript().is_none());
}
