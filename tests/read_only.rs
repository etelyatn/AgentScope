//! AC-01 / AC-08: AgentScope never writes provider data. Session files, the main
//! OpenCode database, and permissions stay unchanged; under ADR-0002 only SQLite's
//! transient `-shm`/`-wal` sidecars may appear or change.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use agentscope::app::App;
use agentscope::discovery::{discover_claude, discover_codex};
use agentscope::domain::{Thresholds, WarningKind};
use agentscope::providers::opencode::open_read_only;
use agentscope::ui::{RosterContext, apply_worker_event, flush_requests, handle_key};
use agentscope::worker::{WorkerHandle, WorkerRequest};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use support::world::*;

type Meta = BTreeMap<PathBuf, (u64, std::time::SystemTime, bool)>;

/// (len, mtime, readonly) of every non-sidecar file.
fn metadata(world: &World) -> Meta {
    all_paths(world.dir.path())
        .into_iter()
        .filter(|p| {
            let n = p.to_string_lossy();
            !(n.ends_with("-shm") || n.ends_with("-wal") || n.ends_with("-journal"))
        })
        .filter_map(|rel| {
            let m = fs::metadata(world.dir.path().join(&rel)).ok()?;
            m.is_file().then(|| {
                (
                    rel,
                    (m.len(), m.modified().unwrap(), m.permissions().readonly()),
                )
            })
        })
        .collect()
}

fn assert_only_adr0002_sidecars_added(world: &World, before: &[PathBuf]) {
    let after = all_paths(world.dir.path());
    for p in &after {
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            before.contains(p) || name == "opencode.db-shm" || name == "opencode.db-wal",
            "unexpected new path: {p:?}"
        );
    }
    for p in before {
        assert!(after.contains(p), "path removed: {p:?}");
    }
}

fn wait_for(
    app: &mut App,
    ctx: &mut RosterContext,
    worker: &WorkerHandle,
    done: impl Fn(&App) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done(app) {
        assert!(Instant::now() < deadline, "worker did not deliver in time");
        if let Ok(ev) = worker.rx.recv_timeout(Duration::from_millis(50)) {
            apply_worker_event(app, ctx, ev);
        }
    }
}

#[test]
fn provider_files_and_main_database_are_unchanged() {
    let world = World::new();
    let before_files = snapshot(world.dir.path());
    let before_paths = all_paths(world.dir.path());
    let before_meta = metadata(&world);

    let mut readers = world.readers();
    let surface = exercise_everything(&mut readers);
    // A second pass hits the unchanged-source and cursor-reuse paths.
    let _ = exercise_everything(&mut readers);
    assert_eq!(surface.sessions.len(), 3, "all three providers were read");
    drop(readers);

    let after_files = snapshot(world.dir.path());
    assert_eq!(
        after_files.files, before_files.files,
        "session files, the main database, and credential decoys are byte-identical"
    );
    assert_eq!(
        metadata(&world),
        before_meta,
        "sizes, mtimes, permissions unchanged"
    );
    assert_only_adr0002_sidecars_added(&world, &before_paths);
    // The live writer still sees its own data: nothing was truncated or repaired.
    let n: i64 = world
        .writer
        .query_row("SELECT count(*) FROM session_message", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
}

#[test]
fn full_worker_and_tui_session_leaves_provider_data_unchanged() {
    let world = World::new();
    let before = snapshot(world.dir.path());
    let before_paths = all_paths(world.dir.path());
    let before_meta = metadata(&world);

    let worker = WorkerHandle::spawn(world.readers(), world.config());
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
    wait_for(&mut app, &mut ctx, &worker, |a| a.total_sessions() == 3);
    // Open every session, expand, and request source records.
    for _ in 0..3 {
        handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        flush_requests(&mut app, &worker);
        wait_for(&mut app, &mut ctx, &worker, |a| {
            a.transcript().is_some_and(|t| t.loaded())
        });
        handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE),
        );
        flush_requests(&mut app, &worker);
        handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
        );
    }
    let _ = worker.tx.send(WorkerRequest::Refresh);
    assert!(worker.shutdown(), "worker exits cleanly");

    assert_eq!(snapshot(world.dir.path()).files, before.files);
    assert_eq!(metadata(&world), before_meta);
    assert_only_adr0002_sidecars_added(&world, &before_paths);
}

#[test]
fn authorizer_denies_credential_tables_and_discovery_ignores_auth_files() {
    let world = World::new();
    let conn = open_read_only(&world.db).expect("open");
    for sql in [
        "SELECT secret FROM credential",
        "SELECT secret FROM account",
        "SELECT secret FROM control_account",
        "SELECT share_url FROM session_share",
        "SELECT share_url FROM session_v2",
        "SELECT seq FROM session_message",
        "INSERT INTO session_v2(id, time_created, time_updated) VALUES ('x', 1, 1)",
        "UPDATE credential SET secret = ''",
        "DELETE FROM credential",
        "DROP TABLE credential",
        "PRAGMA wal_checkpoint(TRUNCATE)",
        "PRAGMA journal_mode = DELETE",
        "ATTACH DATABASE 'other.db' AS other",
    ] {
        // Message content is only reachable through the adapter's bounded queries.
        assert!(conn.prepare(sql).is_err(), "must be denied: {sql}");
    }
    // VACUUM has no authorizer action; the read-only connection refuses it at execution.
    assert!(conn.execute_batch("VACUUM").is_err());

    // Discovery lists only transcript sources; decoys named like credentials are skipped.
    let codex = discover_codex(&world.config()).expect("codex discovery");
    let claude = discover_claude(&world.config()).expect("claude discovery");
    assert_eq!(codex.sources.len(), 1);
    assert_eq!(claude.sources.len(), 1);
    for s in codex.sources.iter().chain(&claude.sources) {
        let name = s.path.file_name().unwrap().to_string_lossy().to_lowercase();
        assert!(
            !name.contains("auth") && !name.contains("credential"),
            "{name}"
        );
    }
    assert!(
        codex
            .warnings
            .iter()
            .chain(&claude.warnings)
            .all(|w| !w.message.to_lowercase().contains("auth.json")),
        "decoys are neither read nor reported"
    );
}

#[test]
fn unreadable_database_is_reported_unavailable_and_never_repaired() {
    // A directory holding a WAL-mode database with no sidecars and a read-only
    // *file system view* cannot be simulated portably; the non-writable-directory
    // behavior is covered in tests/opencode_roster.rs. Here: a corrupt main file is
    // reported, not repaired or replaced.
    let world = World::new();
    let bad = world.dir.path().join("opencode").join("garbage.db");
    fs::write(&bad, b"this is not a database but it contains secrets").unwrap();
    let before = fs::read(&bad).unwrap();
    let cli = agentscope::config::CliOverrides {
        opencode_db: Some(bad.clone()),
        ..Default::default()
    };
    let config =
        agentscope::config::Config::load_with_env(&cli, &agentscope::config::Env::new()).unwrap();
    let result = agentscope::providers::opencode::OpenCodeScanner::new(&config);
    if let Ok(mut scanner) = result {
        use agentscope::providers::ProviderReader;
        let outcome = scanner.refresh(support::NOW, BIG);
        let reported = match outcome {
            Err(_) => true,
            Ok(d) => d.warnings.iter().any(|w| {
                matches!(
                    w.key.kind,
                    WarningKind::Unavailable | WarningKind::Unsupported | WarningKind::Io
                )
            }),
        };
        assert!(reported, "corrupt database must be surfaced");
    }
    assert_eq!(
        fs::read(&bad).unwrap(),
        before,
        "never repaired or truncated"
    );
}
