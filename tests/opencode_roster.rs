mod support;

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use agentscope::domain::{
    Provider, RefreshBudget, SessionKey, SessionLink, SessionSummary, SourceKey, Warning,
    WarningKind,
};
use agentscope::providers::ProviderError;
use agentscope::providers::opencode::{OpenCodeScanner, Schema, detect_schema, open_read_only};
use support::NOW;
use support::opencode::*;

const BIG: RefreshBudget = RefreshBudget {
    max_records: 100_000,
    max_bytes: 100_000_000,
    max_millis: 60_000,
};

struct Scan {
    sessions: HashMap<String, SessionSummary>,
    removed: Vec<SessionKey>,
    warnings: Vec<Warning>,
}

fn scan(scanner: &mut OpenCodeScanner, budget: RefreshBudget) -> Scan {
    let mut out = Scan {
        sessions: HashMap::new(),
        removed: vec![],
        warnings: vec![],
    };
    loop {
        let delta = scanner.scan_roster(NOW, budget).expect("scan");
        for s in delta.sessions {
            out.sessions.insert(s.key.provider_session_id.clone(), s);
        }
        out.removed.extend(delta.removed);
        out.warnings.extend(delta.warnings);
        if delta.scan.done {
            return out;
        }
    }
}

fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

fn scanner_for(db: &Path) -> OpenCodeScanner {
    OpenCodeScanner::new(&config_for_db(db)).expect("scanner")
}

fn file_bytes(path: &Path) -> Vec<u8> {
    fs::read(path).expect("read file")
}

fn files_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(dir)
        .expect("read_dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn opens_read_only_with_authorizer_and_rejects_sensitive_tables() {
    let dir = tmp();
    let db = dir.path().join("opencode.db");
    {
        let w = create_v2(&db);
        insert_session(&w, &Session::new("ses_1", 1, 2));
        insert_message(&w, "m1", "ses_1", 1, 2);
    }
    let conn = open_read_only(&db).expect("open");

    // Allowlisted reads work.
    let n: i64 = conn
        .query_row("SELECT count(*) FROM session_v2", [], |r| r.get(0))
        .expect("allowed count");
    assert_eq!(n, 1);
    let id: String = conn
        .query_row("SELECT id FROM session_v2", [], |r| r.get(0))
        .expect("allowed select");
    assert_eq!(id, "ses_1");

    // Sensitive tables, share URLs, message content, schema writes and pragmas are denied.
    for sql in [
        "SELECT * FROM account",
        "SELECT secret FROM control_account",
        "SELECT secret FROM credential",
        "SELECT share_url FROM session_share",
        "SELECT share_url FROM session_v2",
        "SELECT * FROM session_v2",
        "SELECT data FROM session_message",
        "INSERT INTO project(id, name) VALUES ('x', 'y')",
        "UPDATE session_v2 SET title = 'x'",
        "DELETE FROM session_v2",
        "CREATE TABLE t(x)",
        "DROP TABLE project",
        "PRAGMA query_only = OFF",
        "PRAGMA journal_mode = DELETE",
        "ATTACH DATABASE ':memory:' AS other",
        "SELECT load_extension('x')",
        "SELECT readfile('x')",
    ] {
        assert!(conn.prepare(sql).is_err(), "must be denied: {sql}");
    }
    // A join cannot smuggle a denied column past the authorizer.
    assert!(
        conn.prepare("SELECT s.id FROM session_v2 s JOIN account a ON a.id = s.id")
            .is_err()
    );
    assert!(
        conn.prepare(
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c) SELECT x FROM c"
        )
        .is_err()
    );
}

#[test]
fn selects_v2_then_legacy_then_unsupported() {
    let dir = tmp();

    // v2 with message rows.
    let v2 = dir.path().join("v2.db");
    {
        let w = create_v2(&v2);
        insert_session(&w, &Session::new("s", 1, 2));
        insert_message(&w, "m", "s", 1, 2);
        // Legacy tables are ignored when v2 is populated.
        w.execute_batch("CREATE TABLE session(id TEXT); CREATE TABLE message(id TEXT); CREATE TABLE part(id TEXT);")
            .unwrap();
    }
    assert_eq!(
        detect_schema(&open_read_only(&v2).unwrap()).unwrap(),
        Schema::V2
    );

    // Legacy only.
    let legacy = dir.path().join("legacy.db");
    {
        let w = create_legacy(&legacy);
        insert_legacy_session(&w, &Session::new("s", 1, 2));
        insert_legacy_message(&w, "m", "s", 1, 2);
    }
    assert_eq!(
        detect_schema(&open_read_only(&legacy).unwrap()).unwrap(),
        Schema::Legacy
    );

    // v2 tables exist but are empty and legacy has rows: legacy wins.
    let both = dir.path().join("both.db");
    {
        let w = create(&both, &format!("{V2_SCHEMA_NO_PROJECT}{LEGACY_NO_PROJECT}"));
        insert_legacy_session(&w, &Session::new("s", 1, 2));
        insert_legacy_message(&w, "m", "s", 1, 2);
    }
    assert_eq!(
        detect_schema(&open_read_only(&both).unwrap()).unwrap(),
        Schema::Legacy
    );

    // v2 shape and nothing anywhere: still v2 (an empty roster, not unsupported).
    let empty = dir.path().join("empty.db");
    drop(create_v2(&empty));
    assert_eq!(
        detect_schema(&open_read_only(&empty).unwrap()).unwrap(),
        Schema::V2
    );

    // Unrecognised shapes.
    let other = dir.path().join("other.db");
    drop(create(&other, "CREATE TABLE unrelated(x INTEGER);"));
    assert_eq!(
        detect_schema(&open_read_only(&other).unwrap()).unwrap(),
        Schema::Unsupported
    );
    let missing_column = dir.path().join("missing.db");
    drop(create(
        &missing_column,
        "CREATE TABLE session_v2(id TEXT, title TEXT);
         CREATE TABLE session_message(id TEXT, session_id TEXT);",
    ));
    assert_eq!(
        detect_schema(&open_read_only(&missing_column).unwrap()).unwrap(),
        Schema::Unsupported
    );

    // An unsupported database becomes a warning, not an error and not a guess.
    let mut scanner = scanner_for(&other);
    let out = scan(&mut scanner, BIG);
    assert!(out.sessions.is_empty());
    assert!(
        out.warnings
            .iter()
            .any(|w| w.key.kind == WarningKind::Unsupported && w.key.provider == Provider::OpenCode)
    );
}

const V2_SCHEMA_NO_PROJECT: &str = "
CREATE TABLE session_v2(
    id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT, fork_session_id TEXT,
    title TEXT, directory TEXT, model TEXT, version TEXT,
    time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, time_archived INTEGER);
CREATE TABLE session_message(
    id TEXT PRIMARY KEY, session_id TEXT NOT NULL, type TEXT NOT NULL,
    time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT);
";

const LEGACY_NO_PROJECT: &str = "
CREATE TABLE session(
    id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT, title TEXT, directory TEXT,
    version TEXT, share_url TEXT, time_created INTEGER NOT NULL,
    time_updated INTEGER NOT NULL, time_archived INTEGER);
CREATE TABLE message(
    id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
    time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT);
CREATE TABLE part(
    id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL,
    time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT);
";

#[test]
fn legacy_fallback_when_v2_is_empty_warns() {
    let dir = tmp();
    let db = dir.path().join("both.db");
    {
        let w = create(&db, &format!("{V2_SCHEMA_NO_PROJECT}{LEGACY_NO_PROJECT}"));
        let mut s = Session::new("old", 10, 20);
        s.parent_id = Some("older");
        insert_legacy_session(&w, &s);
        insert_legacy_session(&w, &Session::new("older", 5, 6));
        insert_legacy_message(&w, "m", "old", 10, 30);
    }
    let out = scan(&mut scanner_for(&db), BIG);
    let s = &out.sessions["old"];
    assert_eq!(
        s.key.source,
        SourceKey::DbRow {
            table: "session".into(),
            id: "old".into()
        }
    );
    assert_eq!(s.last_persisted_at, Some(30));
    assert!(
        matches!(&s.lineage.parent, Some(SessionLink::Resolved(k)) if k.provider_session_id == "older")
    );
    assert!(
        out.warnings
            .iter()
            .any(|w| w.key.kind == WarningKind::Unsupported)
    );
}

#[test]
fn maps_title_directory_model_lineage_archived_and_time_updated() {
    let dir = tmp();
    let db = dir.path().join("opencode.db");
    {
        let w = create_v2(&db);
        insert_project(&w, "p1", "Project One");
        let model = model_json("gpt-5", "openai", "high");
        let mut parent = Session::new("ses_parent", 100, 200);
        parent.title = Some("Parent  title");
        parent.directory = Some("/work/alpha");
        parent.project_id = Some("p1");
        parent.model = Some(&model);
        insert_session(&w, &parent);
        let mut child = Session::new("ses_child", 150, 300);
        child.parent_id = Some("ses_parent");
        child.fork_id = Some("ses_ghost");
        child.title = None;
        child.directory = Some("/work/beta-dir");
        child.archived = Some(250);
        insert_session(&w, &child);
        // Session row is stale: a message was updated later.
        insert_message(&w, "m1", "ses_parent", 110, 900);
    }
    let out = scan(&mut scanner_for(&db), BIG);
    assert_eq!(out.sessions.len(), 2);

    let p = &out.sessions["ses_parent"];
    assert_eq!(p.key.provider, Provider::OpenCode);
    assert_eq!(
        p.key.source,
        SourceKey::DbRow {
            table: "session_v2".into(),
            id: "ses_parent".into()
        }
    );
    assert_eq!(p.title, "Parent title");
    assert!(!p.title_derived);
    assert_eq!(p.cwd.as_deref(), Some(Path::new("/work/alpha")));
    assert_eq!(p.project.as_deref(), Some("Project One"));
    assert_eq!(p.models, vec!["gpt-5".to_string()]);
    assert_eq!(p.provider_version.as_deref(), Some("2.0.29"));
    assert_eq!(p.created_at, Some(100));
    assert_eq!(
        p.last_persisted_at,
        Some(900),
        "stale session row uses newest message time_updated"
    );
    assert!(!p.archived);
    assert!(p.source_available);
    assert!(p.lineage.parent.is_none() && p.lineage.fork.is_none());
    assert!(p.usage.is_none());

    let c = &out.sessions["ses_child"];
    assert_eq!(
        c.title, "ses_child",
        "no provider title falls back to the session id"
    );
    assert_eq!(
        c.project.as_deref(),
        Some("beta-dir"),
        "project falls back to the directory basename"
    );
    assert!(c.archived);
    assert_eq!(c.last_persisted_at, Some(300));
    assert!(c.models.is_empty());
    match &c.lineage.parent {
        Some(SessionLink::Resolved(k)) => {
            assert_eq!(k, &p.key);
        }
        other => panic!("parent: {other:?}"),
    }
    assert_eq!(
        c.lineage.fork,
        Some(SessionLink::Unresolved("ses_ghost".into()))
    );
    assert!(!c.lineage.sidechain);

    // Sensitive text never reaches the roster.
    let dump = format!(
        "{:?}",
        out.sessions
            .values()
            .map(|s| (&s.title, &s.project, &s.warnings))
            .collect::<Vec<_>>()
    );
    assert!(!dump.contains("SECRET"));
}

#[test]
fn in_place_message_update_advances_last_persisted_at() {
    let dir = tmp();
    let db = dir.path().join("opencode.db");
    let writer = create_v2(&db);
    insert_session(&writer, &Session::new("s", 100, 200));
    insert_message(&writer, "m", "s", 110, 120);
    let mut scanner = scanner_for(&db);
    let first = scan(&mut scanner, BIG);
    assert_eq!(first.sessions["s"].last_persisted_at, Some(200));

    // Unchanged database: nothing is re-emitted.
    assert!(scan(&mut scanner, BIG).sessions.is_empty());

    // The message row is rewritten in place; the session row is not touched.
    writer
        .execute(
            "UPDATE session_message SET time_updated = 5000 WHERE id = 'm'",
            [],
        )
        .unwrap();
    let second = scan(&mut scanner, BIG);
    assert_eq!(second.sessions["s"].last_persisted_at, Some(5000));
    assert!(second.removed.is_empty());

    // Rows disappearing are reported as removed once a pass completes.
    writer.execute("DELETE FROM session_v2", []).unwrap();
    let third = scan(&mut scanner, BIG);
    assert_eq!(third.removed.len(), 1);
    assert_eq!(third.removed[0].provider_session_id, "s");
}

#[test]
fn roster_pages_within_budget_and_honours_session_cap() {
    let dir = tmp();
    let db = dir.path().join("opencode.db");
    {
        let w = create_v2(&db);
        for i in 0..5 {
            insert_session(&w, &Session::new(&format!("s{i}"), 1, 100 + i));
        }
    }
    let mut scanner = scanner_for(&db);
    let small = RefreshBudget {
        max_records: 2,
        ..BIG
    };
    let mut calls = 0;
    let mut ids = vec![];
    loop {
        let d = scanner.scan_roster(NOW, small).unwrap();
        calls += 1;
        assert!(d.sessions.len() <= 2);
        ids.extend(d.sessions.iter().map(|s| s.key.provider_session_id.clone()));
        if d.scan.done {
            break;
        }
    }
    assert_eq!(calls, 3);
    assert_eq!(
        ids,
        ["s4", "s3", "s2", "s1", "s0"],
        "newest time_updated first"
    );
}

#[test]
fn live_writer_with_existing_shm_sees_wal_only_rows() {
    let dir = tmp();
    let db = dir.path().join("opencode.db");
    let writer = create_v2(&db);
    insert_session(&writer, &Session::new("wal_only", 1, 2));
    assert!(sidecar(&db, "-wal").exists());
    assert!(sidecar(&db, "-shm").exists());
    let main_before = file_bytes(&db);

    let out = scan(&mut scanner_for(&db), BIG);
    assert!(out.sessions.contains_key("wal_only"));
    assert!(out.sessions["wal_only"].source_available);
    assert_eq!(file_bytes(&db), main_before, "main database untouched");

    // The live writer keeps working while the reader is open.
    insert_session(&writer, &Session::new("later", 3, 4));
    drop(writer);
}

#[test]
fn stale_wal_with_writable_directory_reads_and_lets_sqlite_manage_sidecars() {
    let live = tmp();
    let live_db = live.path().join("opencode.db");
    let writer = create_v2(&live_db);
    insert_session(&writer, &Session::new("stale_wal", 1, 2));

    // Copy the database and WAL without the shm (fixture only; never provider files).
    let stale = tmp();
    let db = stale.path().join("opencode.db");
    fs::copy(&live_db, &db).unwrap();
    fs::copy(sidecar(&live_db, "-wal"), sidecar(&db, "-wal")).unwrap();
    assert!(!sidecar(&db, "-shm").exists());
    let main_before = file_bytes(&db);

    let out = scan(&mut scanner_for(&db), BIG);
    assert!(
        out.sessions.contains_key("stale_wal"),
        "WAL rows are visible"
    );
    assert_eq!(file_bytes(&db), main_before, "main database untouched");
    // SQLite may have created -shm; that is the ADR-0002 exception.
    let allowed = ["opencode.db", "opencode.db-shm", "opencode.db-wal"];
    assert!(
        files_in(stale.path())
            .iter()
            .all(|f| allowed.contains(&f.as_str()))
    );
    drop(writer);
}

#[test]
fn no_active_wal_opens_read_only() {
    let dir = tmp();
    let db = dir.path().join("opencode.db");
    {
        let w = create_v2(&db);
        insert_session(&w, &Session::new("plain", 1, 2));
    }
    assert!(
        !sidecar(&db, "-wal").exists(),
        "closing the last connection removes sidecars"
    );
    let main_before = file_bytes(&db);

    let out = scan(&mut scanner_for(&db), BIG);
    assert!(out.sessions.contains_key("plain"));
    assert_eq!(file_bytes(&db), main_before);
}

#[test]
fn non_writable_directory_with_required_sidecar_shows_unavailable() {
    let live = tmp();
    let live_db = live.path().join("opencode.db");
    let writer = create_v2(&live_db);
    insert_session(&writer, &Session::new("hidden", 1, 2));

    // A WAL with committed rows exists, but the shm sidecar cannot be created or opened:
    // a directory occupies its name. No chmod, no repair, no copy of provider files.
    let blocked = tmp();
    let db = blocked.path().join("opencode.db");
    fs::copy(&live_db, &db).unwrap();
    fs::copy(sidecar(&live_db, "-wal"), sidecar(&db, "-wal")).unwrap();
    fs::create_dir(sidecar(&db, "-shm")).unwrap();
    let main_before = file_bytes(&db);

    let result = OpenCodeScanner::new(&config_for_db(&db));
    assert!(
        matches!(result, Err(ProviderError::Unavailable(_))),
        "got {:?}",
        result.err()
    );
    assert_eq!(file_bytes(&db), main_before);
    assert!(
        sidecar(&db, "-shm").is_dir(),
        "nothing is deleted or repaired"
    );
    drop(writer);
}

#[test]
fn main_database_is_unchanged_and_only_sidecars_may_change() {
    let dir = tmp();
    let db = dir.path().join("opencode.db");
    let writer = create_v2(&db);
    insert_project(&writer, "p", "P");
    insert_session(&writer, &Session::new("s", 1, 2));
    insert_message(&writer, "m", "s", 1, 3);
    let main_before = file_bytes(&db);
    let names_before = files_in(dir.path());

    let mut scanner = scanner_for(&db);
    for _ in 0..3 {
        scan(&mut scanner, BIG);
    }
    drop(scanner);

    assert_eq!(
        file_bytes(&db),
        main_before,
        "main database bytes unchanged"
    );
    let names_after = files_in(dir.path());
    assert!(
        names_after
            .iter()
            .all(|f| names_before.contains(f) || f == "opencode.db-shm" || f == "opencode.db-wal"),
        "only sidecars may appear: {names_after:?}"
    );
    // Session content is intact for the writer.
    let content: String = writer
        .query_row("SELECT data FROM session_message WHERE id='m'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(content.contains("SECRET-CONTENT"));
}

#[test]
fn model_keeps_id_for_pricing_and_provider_variant_for_display() {
    let dir = tmp();
    let db = dir.path().join("opencode.db");
    {
        let w = create_v2(&db);
        let model = model_json("gpt-5", "openai", "high");
        let mut s = Session::new("a", 1, 3);
        s.model = Some(&model);
        insert_session(&w, &s);
        let mut bare = Session::new("b", 1, 2);
        bare.model = Some("plain-model");
        insert_session(&w, &bare);
    }
    let out = scan(&mut scanner_for(&db), BIG);
    let a = &out.sessions["a"];
    assert_eq!(a.models, vec!["gpt-5".to_string()]);
    assert_eq!(a.model_details.len(), 1);
    assert_eq!(a.model_details[0].id, "gpt-5");
    assert_eq!(a.model_details[0].provider.as_deref(), Some("openai"));
    assert_eq!(a.model_details[0].variant.as_deref(), Some("high"));
    let b = &out.sessions["b"];
    assert_eq!(b.models, vec!["plain-model".to_string()]);
    assert_eq!(b.model_details[0].provider, None);
}

#[test]
fn cap_displaced_sessions_are_evicted_across_refreshes() {
    let dir = tmp();
    let db = dir.path().join("opencode.db");
    let writer = create_v2(&db);
    insert_session(&writer, &Session::new("a", 1, 10));
    insert_session(&writer, &Session::new("b", 1, 20));
    let mut cfg = config_for_db(&db);
    cfg.max_sessions = 2;
    let mut scanner = OpenCodeScanner::new(&cfg).unwrap();
    let first = scan(&mut scanner, BIG);
    assert_eq!(first.sessions.len(), 2);
    assert!(first.removed.is_empty());

    // A newer session displaces the oldest; it still exists but leaves the roster.
    insert_session(&writer, &Session::new("c", 1, 30));
    let second = scan(&mut scanner, BIG);
    assert!(second.sessions.contains_key("c"));
    let removed: Vec<_> = second
        .removed
        .iter()
        .map(|k| k.provider_session_id.as_str())
        .collect();
    assert_eq!(removed, ["a"]);
    assert!(
        second
            .warnings
            .iter()
            .any(|w| w.count == 1 && w.message.contains("max_sessions"))
    );

    // Retained set stays at the cap and is stable.
    let third = scan(&mut scanner, BIG);
    assert!(third.sessions.is_empty() && third.removed.is_empty());

    // The displaced session returns once it is among the newest again.
    writer
        .execute("UPDATE session_v2 SET time_updated = 99 WHERE id = 'a'", [])
        .unwrap();
    let fourth = scan(&mut scanner, BIG);
    assert!(fourth.sessions.contains_key("a"));
    let removed: Vec<_> = fourth
        .removed
        .iter()
        .map(|k| k.provider_session_id.as_str())
        .collect();
    assert_eq!(removed, ["b"]);
}

fn seeded(n: usize) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tmp();
    let db = dir.path().join("opencode.db");
    {
        let w = create_v2(&db);
        for i in 0..n {
            insert_session(&w, &Session::new(&format!("s{i}"), 1, 100 + i as i64));
        }
    }
    (dir, db)
}

#[test]
fn exhausted_record_byte_and_time_budgets_read_nothing_and_never_finish() {
    let (_dir, db) = seeded(3);
    for budget in [
        RefreshBudget {
            max_records: 0,
            ..BIG
        },
        RefreshBudget {
            max_bytes: 0,
            ..BIG
        },
        RefreshBudget {
            max_millis: 0,
            ..BIG
        },
    ] {
        let mut scanner = scanner_for(&db);
        let d = scanner.scan_roster(NOW, budget).unwrap();
        assert!(d.sessions.is_empty(), "{budget:?}");
        assert!(!d.scan.done, "{budget:?}");
        assert_eq!(d.scan.scanned, 0);
        // A real budget afterwards completes the same pass.
        let all = scan(&mut scanner, BIG);
        assert_eq!(all.sessions.len(), 3);
    }
}

#[test]
fn byte_budget_bounds_a_page_but_always_progresses() {
    let (_dir, db) = seeded(5);
    let mut scanner = scanner_for(&db);
    let tiny = RefreshBudget {
        max_bytes: 1,
        ..BIG
    };
    let mut calls = 0;
    let mut seen = 0;
    loop {
        let d = scanner.scan_roster(NOW, tiny).unwrap();
        calls += 1;
        assert_eq!(
            d.sessions.len(),
            1,
            "one row per call under a 1-byte budget"
        );
        seen += d.sessions.len();
        if d.scan.done {
            break;
        }
        assert!(calls < 20);
    }
    assert_eq!(seen, 5);
}

#[test]
fn oversized_text_columns_are_bounded_at_the_query() {
    let dir = tmp();
    let db = dir.path().join("opencode.db");
    {
        let w = create_v2(&db);
        let huge = "x".repeat(2_000_000);
        let mut s = Session::new("big", 1, 2);
        s.title = Some(&huge);
        s.directory = Some(&huge);
        insert_session(&w, &s);
    }
    let out = scan(&mut scanner_for(&db), BIG);
    let s = &out.sessions["big"];
    assert!(s.title.len() <= 1024, "title {}", s.title.len());
    assert!(s.cwd.as_ref().unwrap().as_os_str().len() <= 4096);
}

/// Column names verified read-only (schema only, no row content) against a local
/// OpenCode 2.0.29 database. The adapter must accept the real column set.
#[test]
fn real_opencode_2_0_29_column_set_is_recognised() {
    const REAL_V2: &str = "
CREATE TABLE project(id TEXT PRIMARY KEY, worktree TEXT, vcs TEXT, name TEXT, icon_url TEXT,
    icon_url_override TEXT, icon_color TEXT, time_created INTEGER, time_updated INTEGER,
    time_initialized INTEGER, sandboxes TEXT, commands TEXT, time_active INTEGER, time_archived INTEGER);
CREATE TABLE session_v2(id TEXT PRIMARY KEY, project_id TEXT, workspace_id TEXT, parent_id TEXT,
    fork_session_id TEXT, fork_boundary TEXT, slug TEXT, directory TEXT, path TEXT, title TEXT,
    version TEXT, share_url TEXT, summary_additions INTEGER, summary_deletions INTEGER,
    summary_files INTEGER, summary_diffs TEXT, metadata TEXT, cost REAL, tokens_input INTEGER,
    tokens_output INTEGER, tokens_reasoning INTEGER, tokens_cache_read INTEGER,
    tokens_cache_write INTEGER, revert TEXT, permission TEXT, agent TEXT, model TEXT,
    time_created INTEGER, time_updated INTEGER, time_compacting INTEGER, time_archived INTEGER,
    time_suspended INTEGER, resume_attempts INTEGER, time_idle INTEGER, time_viewed INTEGER,
    idle_outcome TEXT);
CREATE TABLE session_message(id TEXT PRIMARY KEY, session_id TEXT, type TEXT, seq INTEGER,
    time_created INTEGER, time_updated INTEGER, data TEXT);
";
    let dir = tmp();
    let db = dir.path().join("opencode.db");
    {
        let w = create(&db, REAL_V2);
        w.execute(
            "INSERT INTO session_v2(id, title, model, time_created, time_updated) VALUES ('s', 't', ?1, 1, 2)",
            [model_json("m", "p", "v")],
        )
        .unwrap();
    }
    assert_eq!(
        detect_schema(&open_read_only(&db).unwrap()).unwrap(),
        Schema::V2
    );
    let out = scan(&mut scanner_for(&db), BIG);
    assert_eq!(
        out.sessions["s"].model_details[0].provider.as_deref(),
        Some("p")
    );
}

#[cfg(unix)]
#[test]
fn read_only_directory_with_required_sidecar_shows_unavailable() {
    use std::os::unix::fs::PermissionsExt;
    let live = tmp();
    let live_db = live.path().join("opencode.db");
    let writer = create_v2(&live_db);
    insert_session(&writer, &Session::new("hidden", 1, 2));

    let ro = tmp();
    let db = ro.path().join("opencode.db");
    fs::copy(&live_db, &db).unwrap();
    fs::copy(sidecar(&live_db, "-wal"), sidecar(&db, "-wal")).unwrap();
    fs::set_permissions(ro.path(), fs::Permissions::from_mode(0o555)).unwrap();
    let probe = fs::File::create(ro.path().join("probe"));
    let result = if probe.is_ok() {
        // Running as root: permissions are not enforced; nothing to assert.
        None
    } else {
        Some(OpenCodeScanner::new(&config_for_db(&db)))
    };
    fs::set_permissions(ro.path(), fs::Permissions::from_mode(0o755)).unwrap();
    if let Some(result) = result {
        assert!(
            matches!(result, Err(ProviderError::Unavailable(_))),
            "got {:?}",
            result.err()
        );
        assert!(
            !sidecar(&db, "-shm").exists(),
            "no sidecar was created or repaired"
        );
    }
    drop(writer);
}
