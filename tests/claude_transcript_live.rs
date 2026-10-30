mod support;
use agentscope::config::{CliOverrides, Config, Env};
use agentscope::domain::{PageDirection, PageRequest, RefreshBudget, SessionKey};
use agentscope::providers::{ProviderReader, claude::ClaudeScanner};
use serde_json::{Value, json};
use std::io::Write;
use support::{NOW, write_lines};

const BUDGET: RefreshBudget = RefreshBudget {
    max_records: 1000,
    max_bytes: 1_000_000,
    max_millis: 60_000,
};
const PAGE: PageRequest = PageRequest {
    max_events: 1,
    max_bytes: 4096,
};
fn setup(
    records: &[Value],
) -> (
    tempfile::TempDir,
    std::path::PathBuf,
    ClaudeScanner,
    SessionKey,
) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let path = root.join("p/s.jsonl");
    write_lines(
        &path,
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
    let key = scanner.refresh(NOW, BUDGET).unwrap().sessions.remove(0).key;
    (dir, path, scanner, key)
}
fn assistant(uuid: &str, content: Value) -> Value {
    json!({"type":"assistant","uuid":uuid,"sessionId":"s","message":{"id":uuid,"content":content,"usage":{"input_tokens":1,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":2}}})
}
#[test]
fn forward_and_backward_pages_are_equivalent_and_never_split_tool_pairs() {
    let records = [
        assistant(
            "a",
            json!([{"type":"text","text":"hello"},{"type":"tool_use","id":"c","name":"Read","input":{}}]),
        ),
        json!({"type":"user","uuid":"b","message":{"content":[{"type":"tool_result","tool_use_id":"c","content":"done"}]}}),
        assistant("c", json!("last")),
    ];
    let (_dir, _, mut s, key) = setup(&records);
    let mut forward = Vec::new();
    let mut anchor = None;
    loop {
        let p = s
            .transcript_page(&key, anchor, PageDirection::Forward, PAGE)
            .unwrap();
        forward.extend(p.events);
        if !p.has_more {
            break;
        }
        anchor = p.next;
    }
    let mut backward = Vec::new();
    let mut anchor = None;
    loop {
        let p = s
            .transcript_page(&key, anchor, PageDirection::Backward, PAGE)
            .unwrap();
        backward.splice(0..0, p.events);
        if !p.has_more {
            break;
        }
        anchor = p.next;
    }
    assert_eq!(forward, backward);
    assert_eq!(forward.len(), 3);
    assert_eq!(
        forward[1]
            .tool
            .as_ref()
            .unwrap()
            .output
            .as_ref()
            .unwrap()
            .preview,
        "done"
    );
    let first = agentscope::domain::PageAnchor {
        timestamp: forward[0].timestamp,
        source_order: forward[0].source_order,
        stable_id: forward[0].id.clone(),
    };
    assert_eq!(s.count_events_after(&key, &first).unwrap(), 2);
}
#[test]
fn source_locator_round_trip_and_foreign_locator_rejection() {
    let (_dir, _, mut s, key) = setup(&[assistant("a", json!("hello"))]);
    let p = s
        .transcript_page(&key, None, PageDirection::Forward, PAGE)
        .unwrap();
    let loc = p.events[0].provenance.locator.clone();
    assert!(s.source_record(&loc).unwrap().raw.contains("hello"));
    let mut foreign = loc;
    foreign.session.provider_session_id = "other".into();
    assert!(s.source_record(&foreign).is_err());
}
#[test]
fn partial_append_then_completion_adds_one_event() {
    let (_dir, path, mut s, key) = setup(&[assistant("a", json!("first"))]);
    let line = assistant("b", json!("second")).to_string();
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    write!(f, "{}", &line[..line.len() / 2]).unwrap();
    f.flush().unwrap();
    s.refresh(NOW, BUDGET).unwrap();
    assert_eq!(
        s.transcript_page(
            &key,
            None,
            PageDirection::Forward,
            PageRequest {
                max_events: 10,
                ..PAGE
            }
        )
        .unwrap()
        .events
        .len(),
        1
    );
    writeln!(f, "{}", &line[line.len() / 2..]).unwrap();
    f.flush().unwrap();
    s.refresh(NOW, BUDGET).unwrap();
    assert_eq!(
        s.transcript_page(
            &key,
            None,
            PageDirection::Forward,
            PageRequest {
                max_events: 10,
                ..PAGE
            }
        )
        .unwrap()
        .events
        .len(),
        2
    );
}
#[test]
fn rotation_resets_cursor_content_and_usage_ledger() {
    let (_dir, path, mut s, key) = setup(&[
        assistant("a", json!("old content")),
        assistant("b", json!("old second")),
    ]);
    std::fs::write(&path, format!("{}\n", assistant("a", json!("new")))).unwrap();
    let delta = s.refresh(NOW, BUDGET).unwrap();
    assert_eq!(delta.sessions[0].usage.as_ref().unwrap().total, Some(3));
    let p = s
        .transcript_page(
            &key,
            None,
            PageDirection::Forward,
            PageRequest {
                max_events: 10,
                ..PAGE
            },
        )
        .unwrap();
    assert_eq!(p.events.len(), 1);
    assert_eq!(p.events[0].text.as_ref().unwrap().preview, "new");
}
#[test]
fn suppressed_and_unknown_records_appear_in_diagnostics() {
    let (_dir, _, mut s, key) = setup(&[
        json!({"type":"file-history-snapshot"}),
        json!({"type":"mystery"}),
        assistant("a", json!("visible")),
    ]);
    let p = s
        .transcript_page(&key, None, PageDirection::Forward, PAGE)
        .unwrap();
    assert_eq!(p.diagnostics.len(), 2);
    for d in p.diagnostics {
        assert!(s.source_record(&d.locator).is_ok());
    }
}
