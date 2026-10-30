//! AC-09 and the credential boundary: sentinel secrets never appear in Debug output,
//! warnings, errors, or the rendered terminal; control sequences are neutralized and
//! credential-shaped transcript text is masked by default.

mod support;

use std::fmt::Debug;
use std::fs;
use std::io::Write;
use std::time::Duration;

use agentscope::app::App;
use agentscope::config::{CliOverrides, Config, Env};
use agentscope::domain::{Event, PageDirection, Provider, Thresholds};
use agentscope::processes::{ProcessInfo, ProcessSnapshot};
use agentscope::providers::codex::CodexScanner;
use agentscope::providers::opencode::OpenCodeScanner;
use agentscope::providers::{ProviderError, ProviderReader};
use agentscope::ui::{RosterContext, apply_worker_event, handle_key, render};
use agentscope::worker::{WorkerEvent, WorkerRequest};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use support::codex as cx;
use support::world::*;
use support::{dated, write_lines};

const FORBIDDEN: [&str; 7] = [
    SENTINEL,
    INLINE_SECRET,
    "SECRET-ACCOUNT",
    "SECRET-CONTROL",
    "SECRET-CREDENTIAL",
    "SECRET-SHARE",
    // The control byte itself (not its harmless escaped text) must never survive.
    "\u{1b}",
];

fn assert_clean(label: &str, text: &str) {
    for needle in FORBIDDEN {
        assert!(
            !text.contains(needle),
            "{label} leaked {needle:?}: {}",
            text.chars().take(200).collect::<String>()
        );
    }
}

fn assert_clean_debug(label: &str, value: &impl Debug) {
    assert_clean(label, &format!("{value:?}"));
    assert_clean(label, &format!("{value:#?}"));
}

fn preview(e: &Event) -> String {
    let mut out = String::new();
    if let Some(t) = &e.text {
        out.push_str(&t.preview);
    }
    if let Some(tool) = &e.tool {
        for t in [&tool.input, &tool.output].into_iter().flatten() {
            out.push_str(&t.preview);
        }
    }
    out
}

/// Adds damaged Codex/Claude files whose broken records embed the sentinel.
fn add_damaged_sources(world: &World) {
    let path = dated(&world.codex_root, "rollout-damaged.jsonl");
    write_lines(
        &path,
        &[
            cx::session_meta("2026-10-29T22:00:00Z", "damaged", "/work/damaged"),
            format!("{{not json {SENTINEL}"),
            format!("{{\"type\":\"future_record\",\"secret\":\"{SENTINEL}\"}}"),
            cx::user(
                "2026-10-29T22:00:03Z",
                &format!("password={INLINE_SECRET} {}", "y".repeat(5000)),
            ),
            cx::user("2026-10-29T22:00:04Z", "after the damage"),
        ],
    );
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(format!("{{\"type\":\"resp{SENTINEL}").as_bytes())
        .unwrap();
    write_lines(
        &world.claude_root.join("proj").join("damaged.jsonl"),
        &[
            format!("{{broken {SENTINEL}"),
            claude_line(
                "assistant",
                "d1",
                "2026-10-29T22:10:00Z",
                serde_json::json!([{"type":"unknown_nested","secret":SENTINEL}]),
                None,
            ),
        ],
    );
}

#[test]
fn sentinel_secret_never_appears_in_debug_warnings_or_errors() {
    let world = World::new();
    add_damaged_sources(&world);
    let mut config = world.config();
    config.max_record_bytes = 2048;
    let mut readers: Vec<Box<dyn ProviderReader>> = vec![
        Box::new(CodexScanner::new(&config).unwrap()),
        Box::new(agentscope::providers::claude::ClaudeScanner::new(&config).unwrap()),
        Box::new(OpenCodeScanner::new(&config).unwrap()),
    ];
    let surface = exercise_everything(&mut readers);
    assert!(
        surface.sessions.len() >= 4,
        "healthy and damaged sessions coexist"
    );
    assert!(
        !surface.warnings.is_empty() || surface.sessions.iter().any(|s| !s.warnings.is_empty())
    );

    for s in &surface.sessions {
        assert_clean_debug("SessionSummary", s);
        for w in &s.warnings {
            assert_clean("session warning", &w.message);
            assert_clean_debug("session warning", w);
        }
    }
    for w in &surface.warnings {
        assert_clean("warning message", &w.message);
        assert_clean("warning source", &w.key.source);
        assert_clean_debug("warning", w);
    }
    for page in &surface.pages {
        assert_clean_debug("TranscriptPage", page);
        for e in &page.events {
            assert_clean("event preview", &preview(e));
        }
        for d in &page.diagnostics {
            assert_clean("diagnostic label", &d.label);
        }
    }
    // Content-bearing titles and previews are masked by default, not just in Debug.
    for s in &surface.sessions {
        assert_clean("title", &s.title);
    }
    assert_clean_debug("Config", &config);
}

#[test]
fn provider_errors_never_echo_file_content() {
    let world = World::new();
    let bad = world.dir.path().join("opencode").join("junk.db");
    fs::write(&bad, format!("not a database {SENTINEL}")).unwrap();
    let cli = CliOverrides {
        opencode_db: Some(bad),
        ..CliOverrides::default()
    };
    let config = Config::load_with_env(&cli, &Env::new()).unwrap();
    let mut texts: Vec<String> = Vec::new();
    match OpenCodeScanner::new(&config) {
        Err(e) => texts.push(format!("{e} {e:?}")),
        Ok(mut s) => match s.refresh(support::NOW, BIG) {
            Err(e) => texts.push(format!("{e} {e:?}")),
            Ok(d) => texts.extend(d.warnings.iter().map(|w| format!("{w:?} {}", w.message))),
        },
    }
    // Every ProviderError variant renders only its (caller-supplied, content-free) message.
    for e in [
        ProviderError::Io("x".into()),
        ProviderError::Parse("x".into()),
        ProviderError::Unsupported("x".into()),
        ProviderError::Unavailable("x".into()),
        ProviderError::Internal("x".into()),
    ] {
        texts.push(e.to_string());
    }
    assert!(!texts.is_empty());
    for t in texts {
        assert_clean("provider error", &t);
    }
}

#[test]
fn transcripts_mask_credential_shapes_and_neutralize_control_sequences() {
    let world = World::new();
    let mut readers = world.readers();
    let surface = exercise_everything(&mut readers);
    let codex_events: Vec<_> = surface
        .pages
        .iter()
        .flat_map(|p| &p.events)
        .filter(|e| e.provenance.provider == Provider::Codex)
        .collect();
    assert!(!codex_events.is_empty());
    let joined: String = codex_events.iter().map(|e| preview(e)).collect();
    assert!(
        joined.contains("done"),
        "the surrounding text survives masking"
    );
    assert_clean("codex transcript", &joined);
    // Raw source access is the explicit opt-in and the only path that sees original bytes.
    let assistant = codex_events
        .iter()
        .find(|e| preview(e).starts_with("done"))
        .expect("assistant event");
    let rec = readers[0]
        .source_record(&assistant.provenance.locator)
        .expect("source record");
    assert!(
        rec.raw.contains(INLINE_SECRET),
        "raw view is explicit opt-in"
    );
    assert_clean("source display form", &rec.display);
}

#[test]
fn rendered_screen_and_worker_events_never_contain_secrets() {
    let world = World::new();
    let worker = agentscope::worker::WorkerHandle::spawn(world.readers(), world.config());
    let mut app = App::new(
        support::NOW,
        Thresholds {
            quiet: Duration::from_secs(300),
            stall: Duration::from_secs(900),
        },
    );
    let mut ctx = RosterContext {
        no_color: true,
        ..RosterContext::default()
    };
    let mut screens = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut requested = false;
    while std::time::Instant::now() < deadline {
        if let Ok(ev) = worker.rx.recv_timeout(Duration::from_millis(50)) {
            assert_clean_debug("worker event", &ev);
            apply_worker_event(&mut app, &mut ctx, ev);
        }
        if app.total_sessions() == 3 && !requested {
            requested = true;
            // Codex rows sort by recency; find the Codex row and open it.
            for _ in 0..3 {
                if app
                    .selected_key()
                    .is_some_and(|k| k.provider == Provider::Codex)
                {
                    break;
                }
                handle_key(
                    &mut app,
                    KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
                );
            }
            handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            for r in app.take_requests() {
                worker.tx.send(r).unwrap();
            }
        }
        if app.transcript().is_some_and(|t| t.loaded()) {
            break;
        }
    }
    assert!(app.transcript().is_some_and(|t| t.loaded()));
    for _ in 0..6 {
        handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
        );
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| render(f, &app, &ctx)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        screens.push(text);
    }
    for s in &screens {
        assert_clean("rendered screen", s);
    }
    assert!(screens.iter().any(|s| s.contains("Codex prompt")));
    let _ = worker.tx.send(WorkerRequest::Shutdown);
    let _ = WorkerEvent::RefreshFinished { generation: 0 };
}

#[test]
fn process_observations_never_expose_raw_arguments() {
    let info = ProcessInfo::new(
        4242,
        Some(support::NOW),
        "codex".into(),
        None,
        Some(vec![
            "codex".into(),
            "--api-key".into(),
            INLINE_SECRET.into(),
            format!("password={SENTINEL}"),
            "\u{1b}[31mred".into(),
        ]),
    );
    assert_clean_debug("ProcessInfo", &info);
    assert_clean("command hint", info.command_line.as_deref().unwrap_or(""));
    assert_clean_debug(
        "ProcessSnapshot",
        &ProcessSnapshot {
            sampled_at: support::NOW,
            processes: vec![info],
        },
    );
}

#[test]
fn page_direction_is_irrelevant_to_masking() {
    // Backward and forward reads share the same sanitizer.
    let world = World::new();
    let mut readers = world.readers();
    let mut surface = Surface::default();
    refresh_all(&mut readers, &mut surface);
    let key = surface
        .sessions
        .iter()
        .find(|s| s.key.provider == Provider::Codex)
        .unwrap()
        .key
        .clone();
    for d in [PageDirection::Forward, PageDirection::Backward] {
        let page = readers[0].transcript_page(&key, None, d, PAGE).unwrap();
        for e in &page.events {
            assert_clean("page preview", &preview(e));
        }
    }
}

#[test]
fn source_record_debug_is_content_free() {
    use agentscope::providers::SourceRecord;
    let record = SourceRecord {
        raw: format!("{{\"prompt\":\"{SENTINEL}\"}}"),
        display: format!("tool output {INLINE_SECRET}"),
        unavailable: Some("SECRET-CREDENTIAL".to_string()),
    };
    assert_clean_debug("source record", &record);
    let event = WorkerEvent::Source {
        request_id: 7,
        record,
    };
    assert_clean_debug("worker source event", &event);
    let text = format!("{event:?}");
    assert!(text.contains("raw_len"), "lengths still shown: {text}");
}
