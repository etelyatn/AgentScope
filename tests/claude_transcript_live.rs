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

#[test]
fn index_builds_in_bounded_resumable_steps() {
    let records: Vec<_> = (0..2200)
        .map(|n| assistant(&format!("{n}"), json!("x")))
        .collect();
    let (_dir, _, mut scanner, key) = setup(&records);
    loop {
        if scanner.refresh(NOW, BUDGET).unwrap().scan.done {
            break;
        }
    }
    let mut retries = 0;
    let page = loop {
        match scanner.transcript_page(&key, None, PageDirection::Forward, PAGE) {
            Ok(page) => break page,
            Err(agentscope::providers::ProviderError::Unavailable(message))
                if message.contains("index is building") =>
            {
                retries += 1
            }
            Err(error) => panic!("{error}"),
        }
    };
    assert!(retries > 0, "index construction must be resumable");
    assert_eq!(page.events.len(), 1);
    assert!(page.has_more);
}

#[test]
fn stale_locator_is_rejected_after_rotation_and_refresh() {
    let (_dir, path, mut scanner, key) =
        setup(&[assistant("old", json!("old content long enough"))]);
    let old = scanner
        .transcript_page(&key, None, PageDirection::Forward, PAGE)
        .unwrap()
        .events[0]
        .provenance
        .locator
        .clone();
    std::fs::write(path, format!("{}\n", assistant("new", json!("new")))).unwrap();
    scanner.refresh(NOW, BUDGET).unwrap();
    assert!(scanner.source_record(&old).is_err());
    let new = scanner
        .transcript_page(&key, None, PageDirection::Forward, PAGE)
        .unwrap()
        .events[0]
        .provenance
        .locator
        .clone();
    assert!(scanner.source_record(&new).unwrap().raw.contains("new"));
}

#[test]
fn nested_hidden_and_joined_result_have_openable_diagnostics() {
    let (_dir, _, mut scanner, key) = setup(&[
        assistant(
            "a",
            json!([{"type":"tool_use","id":"c","name":"Read","input":{}}]),
        ),
        json!({"type":"user","uuid":"b","message":{"content":[{"type":"tool_result","tool_use_id":"c","content":[{"type":"text","text":"done"},{"type":"unknown_nested","secret":"synthetic"}]}]}}),
    ]);
    let page = scanner
        .transcript_page(&key, None, PageDirection::Forward, PAGE)
        .unwrap();
    assert!(
        page.diagnostics
            .iter()
            .any(|d| d.label.contains("source-only"))
    );
    assert!(page.diagnostics.iter().any(|d| d.label.contains("hidden")));
    for diag in page.diagnostics {
        assert!(
            scanner
                .source_record(&diag.locator)
                .unwrap()
                .raw
                .contains("tool_result")
        );
    }
}

#[test]
fn revisions_and_shared_message_usage_are_consistent_across_pages() {
    let a = assistant("a", json!("old"));
    let mut b = assistant("b", json!("other"));
    b["message"]["id"] = json!("a");
    let replacement = assistant("a", json!("replacement"));
    let (_dir, _, mut scanner, key) = setup(&[a, b, replacement]);
    let page = scanner
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
    assert_eq!(page.events.len(), 2);
    assert_eq!(page.events.iter().filter(|e| e.usage.is_some()).count(), 1);
    assert_eq!(page.events[1].text.as_ref().unwrap().preview, "replacement");
}

#[test]
fn index_preserves_partial_record_completed_beyond_scanned_boundary() {
    let (_dir, path, mut scanner, key) = setup(&[assistant("first", json!("first"))]);
    let line = assistant("second", json!("second")).to_string();
    let split = line.len() / 2;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    write!(file, "{}", &line[..split]).unwrap();
    file.flush().unwrap();
    scanner.refresh(NOW, BUDGET).unwrap();
    writeln!(file, "{}", &line[split..]).unwrap();
    file.flush().unwrap();
    let page = scanner
        .transcript_page(&key, None, PageDirection::Forward, PAGE)
        .unwrap();
    assert_eq!(page.events.len(), 1);
    scanner.refresh(NOW, BUDGET).unwrap();
    let page = scanner
        .transcript_page(&key, None, PageDirection::Backward, PAGE)
        .unwrap();
    assert_eq!(page.events[0].text.as_ref().unwrap().preview, "second");
}

#[test]
fn large_sampled_source_rejects_replacement_with_identical_samples() {
    let (_dir, path, mut scanner, key) = setup(&[assistant("a", json!("visible"))]);
    let locator = scanner
        .transcript_page(&key, None, PageDirection::Forward, PAGE)
        .unwrap()
        .events[0]
        .provenance
        .locator
        .clone();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(file, "{}", "x".repeat(5 * 1024 * 1024)).unwrap();
    drop(file);
    scanner.refresh(NOW, BUDGET).unwrap();
    assert!(scanner.source_record(&locator).is_ok());
    let replacement = path.with_extension("replacement");
    std::fs::write(&replacement, std::fs::read(&path).unwrap()).unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::rename(&replacement, &path).unwrap();
    assert!(scanner.source_record(&locator).is_err());
}

#[test]
fn enormous_oversized_record_is_indexed_across_bounded_steps() {
    let (_dir, path, mut scanner, key) = setup(&[assistant("a", json!("visible"))]);
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{}", "x".repeat(10 * 1024 * 1024)).unwrap();
    scanner.refresh(NOW, BUDGET).unwrap();
    let mut retries = 0;
    loop {
        match scanner.transcript_page(&key, None, PageDirection::Forward, PAGE) {
            Ok(page) => {
                assert_eq!(page.events.len(), 1);
                break;
            }
            Err(agentscope::providers::ProviderError::Unavailable(message))
                if message.contains("index is building") =>
            {
                retries += 1
            }
            Err(error) => panic!("{error}"),
        }
    }
    assert!(retries >= 2);
}

#[test]
fn diagnostics_over_page_limit_are_explicitly_rejected() {
    for visible in [false, true] {
        let mut records = vec![json!({"type":"hidden"}); 201];
        if visible {
            records.push(assistant("a", json!("visible")));
        }
        let (_dir, _, mut scanner, key) = setup(&records);
        assert!(matches!(
            scanner.transcript_page(&key, None, PageDirection::Forward, PAGE),
            Err(agentscope::providers::ProviderError::Unsupported(_))
        ));
    }
}

#[test]
fn finalization_is_resumable_after_small_append() {
    let records: Vec<_> = (0..1100)
        .map(|n| assistant(&format!("{n}"), json!("x")))
        .collect();
    let (_dir, path, mut scanner, key) = setup(&records);
    while !scanner.refresh(NOW, BUDGET).unwrap().scan.done {}
    while scanner
        .transcript_page(&key, None, PageDirection::Forward, PAGE)
        .is_err()
    {}
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{}", assistant("new", json!("last"))).unwrap();
    file.flush().unwrap();
    scanner.refresh(NOW, BUDGET).unwrap();
    assert!(matches!(
        scanner.transcript_page(&key, None, PageDirection::Forward, PAGE),
        Err(agentscope::providers::ProviderError::Unavailable(_))
    ));
    let mut retries = 1;
    while scanner
        .transcript_page(&key, None, PageDirection::Forward, PAGE)
        .is_err()
    {
        retries += 1;
        assert!(retries < 100);
    }
    assert!(retries >= 2);
}

#[test]
fn large_uuid_many_blocks_hits_metadata_limit() {
    let record = assistant(
        &"u".repeat(500_000),
        json!(vec![json!({"type":"text","text":"x"}); 160]),
    );
    let (_dir, _, mut scanner, key) = setup(&[record]);
    assert!(matches!(
        scanner.transcript_page(&key, None, PageDirection::Forward, PAGE),
        Err(agentscope::providers::ProviderError::Unsupported(_))
    ));
}
