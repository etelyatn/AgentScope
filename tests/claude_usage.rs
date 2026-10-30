mod support;
use agentscope::config::{CliOverrides, Config, Env, PriceEntry, PriceSchedule};
use agentscope::domain::{AggregationClass, Provider, RefreshBudget};
use agentscope::providers::claude::ClaudeScanner;
use serde_json::{Value, json};
use support::{NOW, write_lines};

fn scan_with_prices(
    records: &[Value],
    prices: Option<PriceSchedule>,
) -> agentscope::domain::SessionSummary {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    write_lines(
        &root.join("project/session.jsonl"),
        &records.iter().map(ToString::to_string).collect::<Vec<_>>(),
    );
    let mut config = Config::load_with_env(
        &CliOverrides {
            claude_projects: Some(root),
            ..Default::default()
        },
        &Env::new(),
    )
    .unwrap();
    if let Some(prices) = prices {
        config.pricing = prices;
    }
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
fn scan(records: &[Value]) -> agentscope::domain::SessionSummary {
    scan_with_prices(records, None)
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

fn prices() -> PriceSchedule {
    PriceSchedule::new(vec![PriceEntry {
        provider: Provider::Claude,
        model: "claude-test".into(),
        as_of: None,
        input_per_mtok: 3.0,
        cached_input_per_mtok: None,
        cache_write_per_mtok: Some(4.0),
        cache_write_5m_per_mtok: None,
        cache_write_1h_per_mtok: None,
        cache_read_per_mtok: Some(1.0),
        output_per_mtok: 5.0,
        reasoning_per_mtok: None,
    }])
    .unwrap()
}

#[test]
fn cost_note_only_when_server_tools_used_and_single_cache_write_fallback() {
    let mut r = record("a", "m", json!("a"));
    r["message"]["usage"]
        .as_object_mut()
        .unwrap()
        .remove("cache_creation");
    r["message"]["usage"]
        .as_object_mut()
        .unwrap()
        .remove("server_tool_use");
    let usage = scan_with_prices(&[r.clone()], Some(prices()))
        .usage
        .unwrap();
    assert_eq!(usage.total, Some(19));
    assert_eq!(usage.cost.class, AggregationClass::Estimated);
    assert!((usage.cost.usd.unwrap() - 64.0 / 1_000_000.0).abs() < 1e-12);
    assert!(usage.cost.note.is_none());
    r["message"]["usage"]["server_tool_use"] = json!({"web_search_requests":1});
    assert!(
        scan_with_prices(&[r], Some(prices()))
            .usage
            .unwrap()
            .cost
            .note
            .unwrap()
            .contains("server-tool")
    );
}

#[test]
fn mixed_models_sum_usage_but_do_not_estimate_cost() {
    let a = record("a", "one", json!("a"));
    let mut b = record("b", "two", json!("b"));
    b["message"]["model"] = json!("different");
    let usage = scan_with_prices(&[a, b], Some(prices())).usage.unwrap();
    assert_eq!(usage.total, Some(38));
    assert_eq!(usage.model, None);
    assert_eq!(usage.cost.class, AggregationClass::Unknown);
}
