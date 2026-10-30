mod support;

use agentscope::config::{CliOverrides, Config, Env};
use agentscope::domain::{EventKind, Provider, RefreshBudget, SessionKey, SourceKey, ToolStatus};
use agentscope::providers::claude::ClaudeScanner;
use serde_json::json;
use support::claude::message;
use support::{NOW, write_lines};

fn key() -> SessionKey {
    SessionKey {
        provider: Provider::Claude,
        provider_session_id: "session".into(),
        source: SourceKey::File("fixture.jsonl".into()),
    }
}
fn text(e: &agentscope::domain::Event) -> &str {
    &e.text.as_ref().unwrap().preview
}
fn map(records: Vec<serde_json::Value>) -> agentscope::providers::claude::ClaudeTranscript {
    let indexed = records
        .into_iter()
        .enumerate()
        .map(|(i, value)| (i as u64 * 100, value))
        .collect::<Vec<_>>();
    ClaudeScanner::map_transcript(&key(), &indexed)
}

#[test]
fn tool_only_assistant_exposes_reported_usage_and_real_source_offset() {
    let mut record = message(
        "assistant",
        "a",
        json!([{"type":"tool_use","id":"one","name":"Read","input":{}}]),
    );
    record["message"]["usage"] = json!({"input_tokens":10,"output_tokens":4,"cache_creation_input_tokens":3,"cache_read_input_tokens":2,"cache_creation":{"ephemeral_5m_input_tokens":3},"output_tokens_details":{"thinking_tokens":1},"iterations":[{"output_tokens":99}]});
    let mapped = ClaudeScanner::map_transcript(&key(), &[(42, record)]);
    let event = &mapped.events[0];
    let categories = &event.usage.as_ref().unwrap().categories;
    assert_eq!(
        (
            categories.input,
            categories.output,
            categories.cache_write_5m,
            categories.reasoning
        ),
        (Some(10), Some(4), Some(3), Some(1))
    );
    assert!(matches!(
        event.provenance.locator.kind,
        agentscope::domain::LocatorKind::FileOffset { offset: 42, .. }
    ));
}

#[test]
fn maps_distinct_blocks_even_when_assistant_message_ids_match() {
    let records = vec![
        message("user", "u", json!("hello")),
        message(
            "assistant",
            "a",
            json!([{"type":"thinking","thinking":"consider"},{"type":"text","text":"answer"}]),
        ),
        message("assistant", "b", json!([{"type":"text","text":"more"}])),
    ];
    let mapped = map(records);
    assert_eq!(
        mapped.events.iter().map(|e| e.kind).collect::<Vec<_>>(),
        [
            EventKind::UserPrompt,
            EventKind::Reasoning,
            EventKind::AssistantMessage,
            EventKind::AssistantMessage
        ]
    );
    assert_eq!(
        mapped.events.iter().map(text).collect::<Vec<_>>(),
        ["hello", "consider", "answer", "more"]
    );
    assert_ne!(mapped.events[2].id, mapped.events[3].id);
}

#[test]
fn links_tool_results_only_by_id_and_marks_errors_and_orphans() {
    let records = vec![
        message(
            "assistant",
            "a",
            json!([{"type":"tool_use","id":"one","name":"Read","input":{"path":"x"}},{"type":"tool_use","id":"two","name":"Write","input":{}}]),
        ),
        message(
            "user",
            "b",
            json!([{"type":"tool_result","tool_use_id":"two","content":"failed","is_error":true},{"type":"tool_result","tool_use_id":"missing","content":"orphan"}]),
        ),
    ];
    let mapped = map(records);
    assert_eq!(mapped.events.len(), 3);
    let tools: Vec<_> = mapped
        .events
        .iter()
        .map(|e| e.tool.as_ref().unwrap())
        .collect();
    assert_eq!(tools[0].status, ToolStatus::Pending);
    assert_eq!(tools[1].status, ToolStatus::Error);
    assert_eq!(tools[1].output.as_ref().unwrap().preview, "failed");
    assert!(tools[2].orphan);
}

#[test]
fn images_are_placeholders_and_auxiliary_records_are_hidden() {
    let records = vec![
        message(
            "user",
            "u",
            json!([{"type":"image","source":{"data":"SECRET_BINARY"}},{"type":"text","text":"look"}]),
        ),
        json!({"type":"attachment","attachment":{"text":"hidden"}}),
        json!({"type":"file-history-snapshot"}),
    ];
    let mapped = map(records);
    assert_eq!(
        mapped.events.iter().map(text).collect::<Vec<_>>(),
        ["[image]", "look"]
    );
    assert_eq!(mapped.hidden_records, 2);
}

fn config(root: &std::path::Path) -> Config {
    Config::load_with_env(
        &CliOverrides {
            claude_projects: Some(root.into()),
            ..Default::default()
        },
        &Env::new(),
    )
    .unwrap()
}
const BIG: RefreshBudget = RefreshBudget {
    max_records: 1000,
    max_bytes: 1_000_000,
    max_millis: 60_000,
};

#[test]
fn title_precedence_and_duplicate_sessions_are_separate_sources() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let a = root.join("project-a/a.jsonl");
    let b = root.join("project-b/b.jsonl");
    let records = [
        message(
            "user",
            "injected",
            json!("<environment_context>ignore</environment_context>"),
        ),
        message(
            "user",
            "result",
            json!([{"type":"tool_result","tool_use_id":"missing","content":"not title"}]),
        ),
        message("user", "u", json!("real prompt")),
    ];
    write_lines(
        &a,
        &records.iter().map(ToString::to_string).collect::<Vec<_>>(),
    );
    let mut with_title = records.to_vec();
    with_title.push(json!({"type":"ai-title","aiTitle":"Generated title"}));
    write_lines(
        &b,
        &with_title
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
    );
    let mut scanner = ClaudeScanner::new(&config(&root)).unwrap();
    let delta = scanner.scan_roster(NOW, BIG).unwrap();
    assert!(delta.scan.done);
    assert_eq!(delta.sessions.len(), 2);
    let first = delta
        .sessions
        .iter()
        .find(|s| s.key.source == SourceKey::File(a.clone()))
        .unwrap();
    let second = delta
        .sessions
        .iter()
        .find(|s| s.key.source == SourceKey::File(b.clone()))
        .unwrap();
    assert_eq!(first.title, "real prompt");
    assert!(first.title_derived);
    assert_eq!(second.title, "Generated title");
    assert!(!second.title_derived);
    assert!(first.source_available && second.source_available);
    assert_ne!(first.key, second.key);
}
