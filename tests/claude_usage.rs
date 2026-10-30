mod support;
use agentscope::config::{CliOverrides, Config, Env};
use agentscope::domain::{AggregationClass, RefreshBudget};
use agentscope::providers::claude::ClaudeScanner;
use serde_json::{Value, json};
use support::{NOW, write_lines};

fn scan(records: &[Value]) -> agentscope::domain::SessionSummary {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    write_lines(
        &root.join("project/session.jsonl"),
        &records.iter().map(ToString::to_string).collect::<Vec<_>>(),
    );
    let config = Config::load_with_env(
        &CliOverrides {
            claude_projects: Some(root),
            ..Default::default()
        },
        &Env::new(),
    )
    .unwrap();
    let mut scanner = ClaudeScanner::new(&config).unwrap();
    scanner
        .scan_roster(
            NOW,
            RefreshBudget {
                max_records: 1000,
                max_bytes: 1_000_000,
                max_millis: 60_000,
            },
        )
        .unwrap()
        .sessions
        .remove(0)
}
fn record(uuid: &str, id: &str, content: Value) -> Value {
    json!({"type":"assistant","uuid":uuid,"sessionId":"s","message":{"id":id,"model":"claude-test","content":content,"usage":{"input_tokens":10,"output_tokens":4,"cache_creation_input_tokens":3,"cache_read_input_tokens":2,"cache_creation":{"ephemeral_5m_input_tokens":1,"ephemeral_1h_input_tokens":2},"output_tokens_details":{"thinking_tokens":3},"iterations":[{"input_tokens":1000}],"server_tool_use":{"web_search_requests":2,"web_fetch_requests":1}}}})
}
#[test]
fn top_level_usage_once_with_ttl_and_thinking_subset() {
    let s = scan(&[
        record("a", "m", json!([{"type":"text","text":"a"}])),
        record("b", "m", json!([{"type":"thinking","thinking":"b"}])),
    ]);
    let u = s.usage.unwrap();
    assert_eq!(u.class, AggregationClass::Derived);
    assert_eq!(u.total, Some(19));
    assert_eq!(
        (
            u.categories.cache_write_5m,
            u.categories.cache_write_1h,
            u.categories.reasoning
        ),
        (Some(1), Some(2), Some(3))
    );
    let counts = s.server_tools.unwrap();
    assert_eq!((counts.web_search, counts.web_fetch), (2, 1));
    assert!(s.context_window.is_none());
}
#[test]
fn missing_usage_marks_summary_partial() {
    let mut missing = record("b", "other", json!("b"));
    missing["message"].as_object_mut().unwrap().remove("usage");
    let s = scan(&[record("a", "m", json!("a")), missing]);
    assert_eq!(s.usage.unwrap().class, AggregationClass::Partial);
}
