mod support;
use agentscope::config::{CliOverrides, Config, Env};
use agentscope::domain::{
    AggregationClass, EventKind, Provider, RefreshBudget, SessionKey, SourceKey,
};
use agentscope::providers::claude::ClaudeScanner;
use serde_json::{Value, json};
use std::io::Write;
use support::{NOW, write_lines};
const BIG: RefreshBudget = RefreshBudget {
    max_records: 1000,
    max_bytes: 1_000_000,
    max_millis: 60_000,
};
fn record(uuid: &str, id: &str, content: Value) -> Value {
    json!({"type":"assistant","uuid":uuid,"sessionId":"s","message":{"id":id,"content":content,"usage":{"input_tokens":1,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":2}}})
}
fn setup(
    records: &[Value],
    entries: usize,
    bytes: usize,
) -> (tempfile::TempDir, std::path::PathBuf, ClaudeScanner) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let path = root.join("p/s.jsonl");
    write_lines(
        &path,
        &records.iter().map(ToString::to_string).collect::<Vec<_>>(),
    );
    let mut c = Config::load_with_env(
        &CliOverrides {
            claude_projects: Some(root),
            ..Default::default()
        },
        &Env::new(),
    )
    .unwrap();
    c.dedup_ledger_entries = entries;
    c.dedup_ledger_bytes = bytes;
    (dir, path, ClaudeScanner::new(&c).unwrap())
}
#[test]
fn blocks_survive_shared_message_id_and_same_block_revision_replaces() {
    let key = SessionKey {
        provider: Provider::Claude,
        provider_session_id: "s".into(),
        source: SourceKey::File("x".into()),
    };
    let records = vec![
        (0, record("a", "m", json!([{"type":"text","text":"old"}]))),
        (
            1,
            record(
                "b",
                "m",
                json!([{"type":"thinking","thinking":"think"},{"type":"tool_use","id":"call","name":"Read","input":{}}]),
            ),
        ),
        (2, record("a", "m", json!([{"type":"text","text":"new"}]))),
    ];
    let events = ClaudeScanner::map_transcript(&key, &records).events;
    assert_eq!(events.len(), 3);
    assert_eq!(events.iter().filter(|e| e.usage.is_some()).count(), 1);
    assert!(
        events
            .iter()
            .any(|e| e.text.as_ref().is_some_and(|t| t.preview == "new"))
    );
    assert!(
        !events
            .iter()
            .any(|e| e.text.as_ref().is_some_and(|t| t.preview == "old"))
    );
    assert!(events.iter().any(|e| e.kind == EventKind::Reasoning));
    assert!(events.iter().any(|e| e.kind == EventKind::Tool));
}
#[test]
fn ledger_overflow_freezes_and_marks_partial() {
    for (entries, bytes) in [(1, 100), (100, 1)] {
        let (_dir, _path, mut scanner) = setup(
            &[
                record("a", "first", json!("a")),
                record("b", "second", json!("b")),
                record("c", "third", json!("c")),
            ],
            entries,
            bytes,
        );
        let u = scanner
            .scan_roster(NOW, BIG)
            .unwrap()
            .sessions
            .remove(0)
            .usage
            .unwrap();
        assert_eq!(u.class, AggregationClass::Partial);
        assert_eq!(u.total, if entries == 1 { Some(3) } else { None });
    }
}
#[test]
fn duplicate_after_refresh_does_not_add_again() {
    let (_dir, path, mut scanner) = setup(&[record("a", "m", json!("a"))], 100, 1000);
    assert_eq!(
        scanner
            .scan_roster(NOW, BIG)
            .unwrap()
            .sessions
            .remove(0)
            .usage
            .unwrap()
            .total,
        Some(3)
    );
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{}", record("b", "m", json!("b"))).unwrap();
    assert_eq!(
        scanner
            .scan_roster(NOW, BIG)
            .unwrap()
            .sessions
            .remove(0)
            .usage
            .unwrap()
            .total,
        Some(3)
    );
}
#[test]
fn sidechain_record_does_not_change_parent_totals() {
    let mut side = record("side", "child", json!("child"));
    side["isSidechain"] = json!(true);
    let (_dir, _path, mut scanner) = setup(
        &[record("parent", "parent", json!("parent")), side],
        100,
        1000,
    );
    let s = scanner.scan_roster(NOW, BIG).unwrap().sessions.remove(0);
    assert!(s.lineage.sidechain);
    assert_eq!(s.usage.unwrap().total, Some(3));
}

#[test]
fn sidechain_only_file_has_own_usage_and_parent_after_sidechain_excludes_it() {
    let mut side = record("side", "child", json!("child"));
    side["isSidechain"] = json!(true);
    let (_dir, _path, mut scanner) = setup(&[side.clone()], 100, 1000);
    let summary = scanner.scan_roster(NOW, BIG).unwrap().sessions.remove(0);
    assert_eq!(summary.usage.unwrap().total, Some(3));
    let (_dir, _path, mut scanner) = setup(
        &[side, record("parent", "parent", json!("parent"))],
        100,
        1000,
    );
    let summary = scanner.scan_roster(NOW, BIG).unwrap().sessions.remove(0);
    assert_eq!(summary.usage.unwrap().total, Some(3));
}

#[test]
fn partial_record_revision_keeps_other_blocks() {
    let key = SessionKey {
        provider: Provider::Claude,
        provider_session_id: "s".into(),
        source: SourceKey::File("x".into()),
    };
    let records = vec![
        (
            0,
            record(
                "a",
                "m",
                json!([{"type":"text","text":"old"},{"type":"thinking","thinking":"retained"}]),
            ),
        ),
        (1, record("a", "m", json!([{"type":"text","text":"new"}]))),
    ];
    let events = ClaudeScanner::map_transcript(&key, &records).events;
    assert_eq!(events.len(), 2);
    assert!(
        events
            .iter()
            .any(|e| e.text.as_ref().is_some_and(|t| t.preview == "retained"))
    );
    assert!(
        events
            .iter()
            .any(|e| e.text.as_ref().is_some_and(|t| t.preview == "new"))
    );
}
