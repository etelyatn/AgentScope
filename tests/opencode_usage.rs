mod support;

use std::collections::HashMap;
use std::path::Path;

use agentscope::config::{PriceEntry, PriceSchedule};
use agentscope::domain::{
    AggregationClass, Lifecycle, Provider, RefreshBudget, SessionSummary, TokenCategories,
};
use agentscope::providers::opencode::OpenCodeScanner;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use support::NOW;
use support::opencode::*;

const BIG: RefreshBudget = RefreshBudget {
    max_records: 100_000,
    max_bytes: 100_000_000,
    max_millis: 60_000,
};

fn scan(scanner: &mut OpenCodeScanner) -> HashMap<String, SessionSummary> {
    let mut out = HashMap::new();
    loop {
        let delta = scanner.scan_roster(NOW, BIG).expect("scan");
        for s in delta.sessions {
            out.insert(s.key.provider_session_id.clone(), s);
        }
        if delta.scan.done {
            return out;
        }
    }
}

fn scanner_for(db: &Path) -> OpenCodeScanner {
    OpenCodeScanner::new(&config_for_db(db)).expect("scanner")
}

fn priced_scanner(db: &Path, entry: PriceEntry) -> OpenCodeScanner {
    let mut config = config_for_db(db);
    config.pricing = PriceSchedule::new(vec![entry]).expect("prices");
    OpenCodeScanner::new(&config).expect("scanner")
}

fn msg(conn: &Connection, session: &str, id: &str, ty: &str, at: i64, data: Value) {
    conn.execute(
        "INSERT INTO session_message(id, session_id, type, seq, time_created, time_updated, data)
         VALUES (?1, ?2, ?3, 0, ?4, ?4, ?5)",
        params![id, session, ty, at, data.to_string()],
    )
    .expect("insert message");
}

fn assistant(finish: Option<&str>, tokens: Value, cost: Option<f64>) -> Value {
    let mut v = json!({"content": [{"type": "text", "text": "ok"}], "tokens": tokens});
    if let Some(f) = finish {
        v["finish"] = json!(f);
    }
    if let Some(c) = cost {
        v["cost"] = json!(c);
    }
    v
}

fn tokens(input: u64, output: u64, reasoning: u64, read: u64, write: u64) -> Value {
    json!({"input": input, "output": output, "reasoning": reasoning,
           "cache": {"read": read, "write": write}})
}

fn fixture() -> (tempfile::TempDir, std::path::PathBuf, Connection) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("opencode.db");
    let conn = create_v2(&db);
    (dir, db, conn)
}

#[test]
fn session_totals_beat_message_sums_and_zero_cost_stays_reported() {
    let (_d, db, conn) = fixture();
    let model = model_json("free-model", "acme", "");
    let mut free = Session::new("free", 10, 20);
    free.tokens = Some([10, 5, 2, 3, 4]);
    free.cost = Some(0.0);
    free.model = Some(&model);
    insert_session(&conn, &free);
    // Message sums lag (and carry their own cost) while the turn is in flight; neither
    // may replace the session's reported figures.
    msg(
        &conn,
        "free",
        "m1",
        "assistant",
        11,
        assistant(Some("stop"), tokens(1, 1, 1, 1, 1), Some(9.9)),
    );
    let mut paid = Session::new("paid", 10, 30);
    paid.tokens = Some([100, 50, 0, 0, 0]);
    paid.cost = Some(1.25);
    insert_session(&conn, &paid);

    let out = scan(&mut scanner_for(&db));
    let usage = out["free"].usage.as_ref().expect("usage");
    assert_eq!(usage.class, AggregationClass::Reported);
    assert_eq!(
        usage.categories,
        TokenCategories {
            input: Some(10),
            output: Some(5),
            reasoning: Some(2),
            cache_read: Some(3),
            cache_write: Some(4),
            ..TokenCategories::default()
        }
    );
    assert_eq!(
        usage.total,
        Some(24),
        "session columns, not the message sum"
    );
    assert_eq!(usage.model.as_deref(), Some("free-model"));
    assert_eq!(usage.cost.usd, Some(0.0), "a reported 0 is a value");
    assert_eq!(usage.cost.class, AggregationClass::Reported);

    let usage = out["paid"].usage.as_ref().expect("usage");
    assert_eq!(usage.total, Some(150));
    assert_eq!(usage.cost.usd, Some(1.25));
    assert_eq!(usage.cost.class, AggregationClass::Reported);
    assert!(usage.model.is_none());
}

#[test]
fn partial_session_columns_are_partial_not_derived_or_zero() {
    let (_d, db, conn) = fixture();
    insert_session(&conn, &Session::new("s", 1, 2));
    conn.execute(
        "UPDATE session_v2 SET tokens_input = 7, tokens_output = 3 WHERE id = 's'",
        [],
    )
    .expect("update");
    msg(
        &conn,
        "s",
        "m1",
        "assistant",
        5,
        assistant(None, tokens(100, 100, 100, 100, 100), None),
    );
    let usage = scan(&mut scanner_for(&db))["s"]
        .usage
        .clone()
        .expect("usage");
    assert_eq!(usage.class, AggregationClass::Partial);
    assert_eq!(usage.categories.input, Some(7));
    assert_eq!(usage.categories.reasoning, None);
    assert_eq!(usage.total, None);
    assert_eq!(usage.cost.class, AggregationClass::Unknown);
}

#[test]
fn missing_session_totals_derive_from_unique_messages_and_mark_derived() {
    let (_d, db, conn) = fixture();
    insert_session(&conn, &Session::new("full", 1, 100));
    msg(&conn, "full", "u1", "user", 2, json!({"text": "go"}));
    msg(
        &conn,
        "full",
        "a1",
        "assistant",
        3,
        assistant(Some("tool-calls"), tokens(10, 5, 1, 2, 3), Some(0.5)),
    );
    msg(
        &conn,
        "full",
        "a2",
        "assistant",
        4,
        assistant(Some("stop"), tokens(20, 6, 0, 4, 0), Some(0.25)),
    );
    // Same-id rows cannot repeat (primary key); a copied record under a new id is a
    // distinct message and does count, so only exact-id duplicates are guarded.
    insert_session(&conn, &Session::new("lagging", 1, 90));
    msg(
        &conn,
        "lagging",
        "b1",
        "assistant",
        3,
        assistant(Some("stop"), tokens(10, 5, 1, 2, 3), None),
    );
    msg(
        &conn,
        "lagging",
        "b2",
        "assistant",
        4,
        json!({"content": [], "finish": "stop"}),
    );
    insert_session(&conn, &Session::new("empty", 1, 80));
    msg(&conn, "empty", "e1", "user", 2, json!({"text": "hi"}));

    let out = scan(&mut scanner_for(&db));
    let usage = out["full"].usage.as_ref().expect("usage");
    assert_eq!(usage.class, AggregationClass::Derived);
    assert_eq!(usage.categories.input, Some(30));
    assert_eq!(usage.categories.output, Some(11));
    assert_eq!(usage.categories.reasoning, Some(1));
    assert_eq!(usage.categories.cache_read, Some(6));
    assert_eq!(usage.categories.cache_write, Some(3));
    assert_eq!(usage.total, Some(51), "each message counted exactly once");
    assert_eq!(usage.cost.usd, Some(0.75));
    assert_eq!(usage.cost.class, AggregationClass::Derived);

    let usage = out["lagging"].usage.as_ref().expect("usage");
    assert_eq!(
        usage.class,
        AggregationClass::Partial,
        "a message without token data makes the derivation incomplete"
    );
    assert_eq!(usage.total, None);
    assert_eq!(usage.cost.usd, None);
    assert_eq!(usage.cost.class, AggregationClass::Unknown);

    assert!(out["empty"].usage.is_none(), "no data is not zero");
}

#[test]
fn derived_usage_without_reported_cost_uses_configured_prices_or_unknown() {
    let (_d, db, conn) = fixture();
    let model = model_json("priced", "acme", "");
    let mut s = Session::new("s", 1, 2);
    s.model = Some(&model);
    insert_session(&conn, &s);
    msg(
        &conn,
        "s",
        "a1",
        "assistant",
        3,
        assistant(
            Some("stop"),
            tokens(1_000_000, 1_000_000, 1_000_000, 0, 0),
            None,
        ),
    );
    let entry = |reasoning: Option<f64>| PriceEntry {
        provider: Provider::OpenCode,
        model: "priced".into(),
        as_of: Some("2026-01-01".into()),
        input_per_mtok: 1.0,
        cached_input_per_mtok: None,
        cache_write_per_mtok: Some(0.0),
        cache_write_5m_per_mtok: None,
        cache_write_1h_per_mtok: None,
        cache_read_per_mtok: Some(0.0),
        output_per_mtok: 2.0,
        reasoning_per_mtok: reasoning,
    };
    let out = scan(&mut priced_scanner(&db, entry(Some(3.0))));
    let cost = &out["s"].usage.as_ref().expect("usage").cost;
    assert_eq!(cost.class, AggregationClass::Estimated);
    assert_eq!(cost.usd, Some(6.0));
    assert_eq!(cost.as_of.as_deref(), Some("2026-01-01"));

    let out = scan(&mut priced_scanner(&db, entry(None)));
    let cost = &out["s"].usage.as_ref().expect("usage").cost;
    assert_eq!(
        cost.class,
        AggregationClass::Unknown,
        "nonzero reasoning without a price is never a partial number"
    );
    assert_eq!(cost.usd, None);
}

struct Case {
    id: &'static str,
    lifecycle: Lifecycle,
    completion: Option<i64>,
    unfinished: bool,
}

#[test]
fn finish_and_idle_map_to_turn_evidence_not_session_failure() {
    let (_d, db, conn) = fixture();
    let t = || tokens(1, 1, 0, 0, 0);
    let mut n = 0;
    let mut add = |conn: &Connection, id: &str, rows: &[(&str, i64, Value)]| {
        n += 1;
        insert_session(conn, &Session::new(id, 1, 1000 + n));
        for (i, (ty, at, data)) in rows.iter().enumerate() {
            msg(conn, id, &format!("{id}-{i}"), ty, *at, data.clone());
        }
    };
    let user = || json!({"text": "go"});

    add(
        &conn,
        "stop",
        &[
            ("user", 10, user()),
            ("assistant", 20, assistant(Some("stop"), t(), None)),
        ],
    );
    add(
        &conn,
        "tool-calls",
        &[
            ("user", 10, user()),
            ("assistant", 20, assistant(Some("tool-calls"), t(), None)),
        ],
    );
    add(
        &conn,
        "idle",
        &[
            ("user", 10, user()),
            ("assistant", 20, assistant(Some("tool-calls"), t(), None)),
            ("idle", 30, json!({"outcome": "succeeded"})),
        ],
    );
    add(
        &conn,
        "reopened",
        &[
            ("user", 10, user()),
            ("assistant", 20, assistant(Some("stop"), t(), None)),
            ("user", 40, user()),
        ],
    );
    add(
        &conn,
        "length",
        &[
            ("user", 5, json!({"text": "earlier"})),
            ("assistant", 20, assistant(Some("length"), t(), None)),
        ],
    );
    // Tool errors and `unknown` finishes are tool/turn details, not session failure.
    let mut failing = assistant(Some("unknown"), t(), None);
    failing["content"] = json!([{"type": "tool", "id": "c", "name": "bash",
        "state": {"status": "error", "metadata": {"exit": 2}}}]);
    add(&conn, "tool-error", &[("assistant", 20, failing)]);
    add(
        &conn,
        "odd-idle",
        &[
            ("assistant", 20, assistant(Some("unknown"), t(), None)),
            ("idle", 30, json!({"outcome": "failed"})),
        ],
    );
    add(&conn, "bare-idle", &[("idle", 30, json!({}))]);
    add(&conn, "silent", &[("system", 5, json!({"text": "x"}))]);

    insert_session(&conn, &{
        let mut s = Session::new("col-idle", 1, 2000);
        s.idle_outcome = Some("succeeded");
        s
    });

    let cases = [
        Case {
            id: "stop",
            lifecycle: Lifecycle::Completed,
            completion: Some(20),
            unfinished: false,
        },
        Case {
            id: "tool-calls",
            lifecycle: Lifecycle::Ongoing,
            completion: None,
            unfinished: true,
        },
        Case {
            id: "idle",
            lifecycle: Lifecycle::Completed,
            completion: Some(30),
            unfinished: false,
        },
        Case {
            id: "reopened",
            lifecycle: Lifecycle::Ongoing,
            completion: Some(20),
            unfinished: true,
        },
        Case {
            id: "length",
            lifecycle: Lifecycle::Ongoing,
            completion: None,
            unfinished: true,
        },
        Case {
            id: "tool-error",
            lifecycle: Lifecycle::Unknown,
            completion: None,
            unfinished: false,
        },
        Case {
            id: "odd-idle",
            lifecycle: Lifecycle::Unknown,
            completion: None,
            unfinished: false,
        },
        Case {
            id: "bare-idle",
            lifecycle: Lifecycle::Completed,
            completion: Some(30),
            unfinished: false,
        },
        Case {
            id: "silent",
            lifecycle: Lifecycle::Unknown,
            completion: None,
            unfinished: false,
        },
        Case {
            id: "col-idle",
            lifecycle: Lifecycle::Completed,
            completion: None,
            unfinished: false,
        },
    ];
    let out = scan(&mut scanner_for(&db));
    for case in cases {
        let s = &out[case.id];
        assert_eq!(s.lifecycle, case.lifecycle, "{}", case.id);
        assert_eq!(s.last_completion, case.completion, "{}", case.id);
        assert_eq!(s.unfinished_work, case.unfinished, "{}", case.id);
        assert_ne!(s.lifecycle, Lifecycle::Failed, "{}", case.id);
        assert_ne!(s.lifecycle, Lifecycle::Cancelled, "{}", case.id);
    }
}

#[test]
fn in_place_message_update_refreshes_lifecycle_and_usage() {
    let (_d, db, conn) = fixture();
    insert_session(&conn, &Session::new("s", 1, 50));
    msg(&conn, "s", "u", "user", 10, json!({"text": "go"}));
    msg(
        &conn,
        "s",
        "a",
        "assistant",
        20,
        assistant(Some("tool-calls"), tokens(1, 1, 0, 0, 0), None),
    );
    let mut scanner = scanner_for(&db);
    let first = scan(&mut scanner);
    assert_eq!(first["s"].lifecycle, Lifecycle::Ongoing);
    assert_eq!(first["s"].usage.as_ref().and_then(|u| u.total), Some(2));

    conn.execute(
        "UPDATE session_message SET time_updated = 60, data = ?1 WHERE id = 'a'",
        params![assistant(Some("stop"), tokens(4, 4, 0, 0, 0), None).to_string()],
    )
    .expect("update");
    let second = scan(&mut scanner);
    assert_eq!(second["s"].lifecycle, Lifecycle::Completed);
    assert_eq!(second["s"].last_completion, Some(60));
    assert_eq!(second["s"].usage.as_ref().and_then(|u| u.total), Some(8));
}
