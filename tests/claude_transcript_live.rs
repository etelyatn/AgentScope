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
    // The roster drains the oversized line under the refresh budget, so it takes
    // several refreshes; the transcript index then builds in its own bounded steps.
    let mut refreshes = 0;
    while !scanner.refresh(NOW, BUDGET).unwrap().scan.done {
        refreshes += 1;
        assert!(refreshes < 1000, "scan never finished");
    }
    assert!(refreshes >= 2, "oversized drain must span refreshes");
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
fn diagnostic_overflow_preserves_events_and_marks_truncation() {
    for visible in [false, true] {
        let mut records = vec![json!({"type":"hidden"}); 201];
        if visible {
            records.push(assistant("a", json!("visible")));
        }
        let (_dir, _, mut scanner, key) = setup(&records);
        let page = scanner
            .transcript_page(&key, None, PageDirection::Forward, PAGE)
            .unwrap();
        assert_eq!(page.events.len(), usize::from(visible));
        if visible {
            assert_eq!(page.events[0].text.as_ref().unwrap().preview, "visible");
        }
        assert!(page.truncated);
        assert_eq!(page.diagnostics.len(), 200);
        let marker = page.diagnostics.last().unwrap();
        assert!(marker.label.contains("diagnostics truncated"));
        assert!(scanner.source_record(&marker.locator).is_ok());
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

fn status_label(s: &agentscope::domain::SessionSummary) -> agentscope::domain::StatusLabel {
    let thresholds = agentscope::domain::Thresholds {
        quiet: std::time::Duration::from_secs(300),
        stall: std::time::Duration::from_secs(900),
    };
    agentscope::status::synthesize(s, NOW, &thresholds).label
}

fn stamped(kind: &str, uuid: &str, ts: &str, stop: Option<&str>, content: Value) -> Value {
    json!({"type":kind,"uuid":uuid,"sessionId":"s","timestamp":ts,
        "message":{"id":uuid,"role":kind,"stop_reason":stop,"content":content}})
}

fn append(path: &std::path::Path, records: &[Value]) {
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    for r in records {
        writeln!(f, "{r}").unwrap();
    }
    f.flush().unwrap();
}

#[test]
fn stop_reason_drives_completion_unfinished_work_and_reopening() {
    use agentscope::domain::{Lifecycle, StatusLabel};
    let (_dir, path, mut s, _key) = setup(&[
        stamped("user", "u1", "2026-10-29T10:00:00Z", None, json!("do it")),
        stamped(
            "assistant",
            "a1",
            "2026-10-29T10:00:01Z",
            Some("tool_use"),
            json!([{"type":"tool_use","id":"t1","name":"Read","input":{}}]),
        ),
    ]);
    append(
        &path,
        &[
            stamped(
                "user",
                "u2",
                "2026-10-29T10:00:02Z",
                None,
                json!([{"type":"tool_result","tool_use_id":"t1","content":"ok"}]),
            ),
            stamped(
                "assistant",
                "a2",
                "2026-10-29T10:00:03Z",
                Some("end_turn"),
                json!([{"type":"text","text":"all done"}]),
            ),
        ],
    );
    let done = s.refresh(NOW, BUDGET).unwrap().sessions.remove(0);
    assert_eq!(done.lifecycle, Lifecycle::Completed);
    assert!(!done.unfinished_work);
    let completed_at = done.last_completion.expect("end_turn is a completion");
    assert_eq!(status_label(&done), StatusLabel::Complete);

    // Later user work reopens it and keeps the earlier completion time.
    append(
        &path,
        &[stamped(
            "user",
            "u3",
            "2026-10-29T11:00:00Z",
            None,
            json!("one more thing"),
        )],
    );
    let reopened = s.refresh(NOW, BUDGET).unwrap().sessions.remove(0);
    assert!(reopened.unfinished_work);
    assert_eq!(reopened.lifecycle, Lifecycle::Ongoing);
    assert_eq!(reopened.last_completion, Some(completed_at));
    assert_ne!(status_label(&reopened), StatusLabel::Complete);

    // A tool_use stop keeps it open; injected-only user text does not reopen.
    append(
        &path,
        &[stamped(
            "assistant",
            "a3",
            "2026-10-29T11:00:01Z",
            Some("end_turn"),
            json!("fine"),
        )],
    );
    let closed = s.refresh(NOW, BUDGET).unwrap().sessions.remove(0);
    assert!(!closed.unfinished_work);
    append(
        &path,
        &[stamped(
            "user",
            "u4",
            "2026-10-29T11:00:02Z",
            None,
            json!("<system-reminder>ignore</system-reminder>"),
        )],
    );
    let still = s.refresh(NOW, BUDGET).unwrap().sessions.remove(0);
    assert!(!still.unfinished_work, "injected text is not user work");
    append(
        &path,
        &[stamped(
            "assistant",
            "a4",
            "2026-10-29T11:00:03Z",
            Some("tool_use"),
            json!([{"type":"tool_use","id":"t2","name":"Bash","input":{}}]),
        )],
    );
    let pending = s.refresh(NOW, BUDGET).unwrap().sessions.remove(0);
    assert!(pending.unfinished_work);
    assert_eq!(pending.lifecycle, Lifecycle::Ongoing);
}

#[test]
fn result_appended_for_a_displayed_pending_call_invalidates_the_session() {
    let (_dir, path, mut s, key) = setup(&[assistant(
        "a",
        json!([{"type":"tool_use","id":"c","name":"Read","input":{}}]),
    )]);
    let page = |s: &mut ClaudeScanner| {
        s.transcript_page(
            &key,
            None,
            PageDirection::Forward,
            PageRequest {
                max_events: 10,
                ..PAGE
            },
        )
        .unwrap()
    };
    assert_eq!(
        page(&mut s).events[0].tool.as_ref().unwrap().status,
        agentscope::domain::ToolStatus::Pending
    );
    // An unrelated append leaves the displayed row alone.
    append(&path, &[assistant("b", json!("note"))]);
    assert!(s.refresh(NOW, BUDGET).unwrap().invalidated.is_empty());

    append(
        &path,
        &[
            json!({"type":"user","uuid":"r","message":{"content":[{"type":"tool_result","tool_use_id":"c","content":"done"}]}}),
        ],
    );
    let delta = s.refresh(NOW, BUDGET).unwrap();
    assert_eq!(delta.invalidated, vec![key.clone()]);
    let after = page(&mut s);
    assert_eq!(
        after.events[0].tool.as_ref().unwrap().status,
        agentscope::domain::ToolStatus::Completed
    );
}

#[test]
fn growing_in_place_rewrite_resets_state_and_invalidates() {
    use std::io::Seek;
    let (_dir, path, mut s, key) = setup(&[
        assistant("a", json!("old content one")),
        assistant("b", json!("old content two")),
    ]);
    // Same file, same identity: overwrite the consumed bytes and grow past them.
    let new: String = ["new first", "new second", "new third", "new fourth"]
        .iter()
        .enumerate()
        .map(|(i, t)| format!("{}\n", assistant(&format!("n{i}"), json!(t))))
        .collect();
    let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    f.seek(std::io::SeekFrom::Start(0)).unwrap();
    f.write_all(new.as_bytes()).unwrap();
    f.flush().unwrap();
    drop(f);

    let delta = s.refresh(NOW, BUDGET).unwrap();
    assert!(
        delta.invalidated.contains(&key),
        "rewrite must invalidate: {:?}",
        delta.invalidated
    );
    let summary = &delta.sessions[0];
    assert_eq!(summary.displayed_event_count, 4);
    let page = s
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
    let texts: Vec<_> = page
        .events
        .iter()
        .map(|e| e.text.as_ref().unwrap().preview.clone())
        .collect();
    assert_eq!(
        texts,
        ["new first", "new second", "new third", "new fourth"]
    );
}
