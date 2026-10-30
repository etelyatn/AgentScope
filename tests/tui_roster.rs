use std::process::{Command, Stdio};
use std::time::Duration;

use agentscope::app::App;
use agentscope::domain::*;
use agentscope::ui::{RosterContext, render_roster};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::Color;

const NOW: Timestamp = 100_000_000;

fn app() -> App {
    App::new(
        NOW,
        Thresholds {
            quiet: Duration::from_secs(300),
            stall: Duration::from_secs(900),
        },
    )
}

fn session(
    provider: Provider,
    id: &str,
    title: &str,
    age_secs: i64,
    tokens: Option<u64>,
    process: ProcessAssociation,
) -> SessionSummary {
    SessionSummary {
        key: SessionKey {
            provider,
            provider_session_id: id.into(),
            source: SourceKey::File(format!("{id}.jsonl").into()),
        },
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
        displayed_event_count: 1,
        hidden_records: 0,
        source_available: true,
        archived: false,
        context_window: None,
        server_tools: None,
        lifecycle: Lifecycle::Unknown,
        process,
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

fn populated() -> App {
    let mut app = app();
    app.apply_roster(
        vec![
            session(
                Provider::Codex,
                "a",
                "Inventory API",
                12,
                Some(91_000),
                ProcessAssociation::Matched {
                    pid: 7,
                    started_at: None,
                },
            ),
            session(
                Provider::Claude,
                "b",
                "Scanner refactor",
                600,
                Some(73_000),
                ProcessAssociation::Unmatched,
            ),
            {
                let mut c = session(
                    Provider::OpenCode,
                    "c",
                    "PR review",
                    7200,
                    None,
                    ProcessAssociation::Candidates { count: 2 },
                );
                c.last_event_at = None;
                c.last_persisted_at = None;
                c.last_completion = None;
                c
            },
        ],
        vec![],
    );
    app
}

fn ctx(no_color: bool) -> RosterContext {
    RosterContext {
        no_color,
        roots: vec![
            "Codex: /home/u/.codex/sessions".into(),
            "Claude: /home/u/.claude/projects".into(),
            "OpenCode: /home/u/opencode.db".into(),
        ],
        notes: vec![],
        last_refresh: Some("12:00:00".into()),
        error: None,
    }
}

fn draw(app: &App, ctx: &RosterContext, w: u16, h: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal.draw(|f| render_roster(f, app, ctx)).unwrap();
    terminal.backend().buffer().clone()
}

fn text(buf: &Buffer) -> String {
    let w = buf.area.width as usize;
    buf.content()
        .chunks(w)
        .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn renders_status_provider_session_last_tokens_and_process_at_120x40() {
    let out = text(&draw(&populated(), &ctx(false), 120, 40));
    for want in [
        "AgentScope",
        "Codex 1",
        "Claude 1",
        "OpenCode 1",
        "Live: on",
        "Sort: last activity",
        "STATUS",
        "PROVIDER",
        "SESSION",
        "LAST",
        "TOKENS",
        "PROC",
        "● Active",
        "! Quiet",
        "? Unknown",
        "Codex ",
        "Claude ",
        "OpenCode ",
        "Inventory API · RipperGame",
        "12s",
        "10m",
        "—",
        "91k",
        "73k",
        "matched",
        "unmatched",
        "2 cand.",
        "q quit",
    ] {
        assert!(out.contains(want), "missing {want:?} in:\n{out}");
    }
}

#[test]
fn collapses_process_then_tokens_on_narrow_terminals() {
    let app = populated();
    let w80 = text(&draw(&app, &ctx(false), 80, 24));
    assert!(w80.contains("TOKENS") && !w80.contains("PROC"), "{w80}");
    assert!(
        w80.contains("● Active") && w80.contains("Inventory API"),
        "{w80}"
    );

    let w60 = text(&draw(&app, &ctx(false), 60, 20));
    assert!(!w60.contains("TOKENS") && !w60.contains("PROC"), "{w60}");
    for want in [
        "STATUS",
        "PROVIDER",
        "SESSION",
        "LAST",
        "● Active",
        "Codex",
        "12s",
    ] {
        assert!(w60.contains(want), "missing {want:?} in:\n{w60}");
    }
    for line in w60.lines() {
        assert!(line.chars().count() <= 60);
    }
}

#[test]
fn empty_state_lists_checked_roots_and_warnings() {
    let mut app = app();
    app.apply_roster(
        vec![],
        vec![Warning {
            key: WarningKey {
                provider: Provider::Claude,
                source: "projects".into(),
                kind: WarningKind::Io,
            },
            message: "permission denied".into(),
            count: 1,
        }],
    );
    let out = text(&draw(&app, &ctx(false), 100, 30));
    for want in [
        "No sessions found",
        "/home/u/.codex/sessions",
        "/home/u/.claude/projects",
        "/home/u/opencode.db",
        "Claude",
        "permission denied",
        "12:00:00",
    ] {
        assert!(out.contains(want), "missing {want:?} in:\n{out}");
    }
    assert!(!out.to_lowercase().contains("start an agent"));
}

#[test]
fn no_color_mode_keeps_text_labels() {
    let app = populated();
    let plain = draw(&app, &ctx(true), 120, 40);
    for cell in plain.content() {
        assert_eq!(cell.fg, Color::Reset);
        assert_eq!(cell.bg, Color::Reset);
    }
    let out = text(&plain);
    assert!(out.contains("● Active") && out.contains("! Quiet") && out.contains("? Unknown"));

    let colored = draw(&app, &ctx(false), 120, 40);
    assert!(colored.content().iter().any(|c| c.fg != Color::Reset));
    assert_eq!(out, text(&colored));
}

#[test]
fn too_small_terminal_shows_message() {
    let out = text(&draw(&populated(), &ctx(false), 50, 15));
    assert!(out.contains("too small"), "{out}");
    assert!(out.contains("60x20"), "{out}");
    assert!(!out.contains("STATUS"));
}

#[test]
fn untrusted_text_is_sanitized() {
    let mut app = app();
    app.apply_roster(
        vec![session(
            Provider::Codex,
            "x",
            "evil\x1b[31m title\u{202e}",
            5,
            None,
            ProcessAssociation::NotChecked,
        )],
        vec![],
    );
    let out = text(&draw(&app, &ctx(false), 120, 40));
    assert!(!out.contains('\x1b') && !out.contains('\u{202e}'));
}

#[test]
fn non_tty_exits_with_a_message_without_entering_raw_mode() {
    let output = Command::new(env!("CARGO_BIN_EXE_agentscope"))
        .arg("--no-processes")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(
        err.contains("terminal") && err.contains("agentscope inspect"),
        "{err}"
    );
    assert!(
        !output.stdout.contains(&0x1b),
        "no escape sequences on stdout"
    );
    assert!(!err.contains('\x1b'));
}

#[test]
fn help_and_version_exit_zero_without_starting_the_tui() {
    for (flag, want) in [
        ("--help", "--codex-sessions"),
        ("--version", "agentscope 0.1.0"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_agentscope"))
            .arg(flag)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success(), "{flag}");
        let out = String::from_utf8_lossy(&output.stdout);
        assert!(out.contains(want), "{flag}: {out}");
        assert!(!out.contains('\x1b'));
    }
}

#[test]
fn unknown_flag_fails_with_usage() {
    let output = Command::new(env!("CARGO_BIN_EXE_agentscope"))
        .arg("--bogus")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Usage"));
}

#[test]
fn every_documented_flag_parses_before_the_tty_check() {
    let output = Command::new(env!("CARGO_BIN_EXE_agentscope"))
        .args([
            "--config",
            "c.toml",
            "--codex-sessions=a",
            "--claude-projects",
            "b",
            "--opencode-db",
            "d.db",
            "--no-processes",
            "--no-color",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("must be a terminal"), "{err}");

    let output = Command::new(env!("CARGO_BIN_EXE_agentscope"))
        .arg("--config")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&output.stderr).contains("requires a value"));
}
