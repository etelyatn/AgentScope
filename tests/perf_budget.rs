//! Deterministic work-budget checks on synthetic corpora.
//!
//! Every assertion counts work (bytes read, records parsed, preview bytes allocated,
//! events retained) through the thread-local counters in `agentscope::perf`; none
//! depends on wall-clock time. `benchmark_report` (ignored by default) prints the
//! timings recorded in `docs/superpowers/notes/2026-10-30-benchmarks.md`.
mod support;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agentscope::app::{App, MAX_LOADED_EVENTS};
use agentscope::config::{CliOverrides, Config, Env};
use agentscope::domain::*;
use agentscope::perf;
use agentscope::providers::claude::ClaudeScanner;
use agentscope::providers::codex::CodexScanner;
use agentscope::providers::{ProviderDelta, ProviderReader};
use agentscope::worker::WorkerRequest;
use serde_json::{Value, json};
use support::codex::{assistant, session_meta, turn_context, usage, usage_record, user};
use support::{NOW, config_for, dated, set_modified, write_lines};

const SESSIONS: usize = 2_000;
const RECORDS_PER_SESSION: usize = 20;
const BIG: RefreshBudget = RefreshBudget {
    max_records: 1_000_000,
    max_bytes: 1_000_000_000,
    max_millis: 600_000,
};
/// The scan budget the worker hands each adapter chunk (records and bytes only).
const CHUNK: RefreshBudget = RefreshBudget {
    max_records: 256,
    max_bytes: 64 * 1024,
    max_millis: 600_000,
};
const TS: &str = "2026-10-29T10:00:00Z";

fn codex_lines(index: usize, records: usize) -> Vec<String> {
    let mut lines = vec![
        session_meta(TS, &format!("sess-{index:05}"), "/work/demo"),
        turn_context(TS, "gpt-test", "/work/demo"),
        user(TS, &format!("task {index}")),
    ];
    while lines.len() < records {
        let n = lines.len();
        lines.push(assistant(TS, &format!("reply {n} for session {index}")));
        if lines.len() < records {
            lines.push(usage_record(
                TS,
                usage(10 * n as u64, 0, 5, 0, 11 * n as u64),
            ));
        }
    }
    lines
}

struct Corpus {
    _dir: tempfile::TempDir,
    root: PathBuf,
    /// Paths ordered oldest to newest by modified time.
    files: Vec<PathBuf>,
    total_bytes: u64,
    total_records: u64,
    longest_line: usize,
}

fn codex_corpus(sessions: usize, records: usize) -> Corpus {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let mut corpus = Corpus {
        _dir: dir,
        root: root.clone(),
        files: Vec::new(),
        total_bytes: 0,
        total_records: 0,
        longest_line: 0,
    };
    for i in 0..sessions {
        let path = dated(&root, &format!("rollout-{i:05}.jsonl"));
        let lines = codex_lines(i, records);
        write_lines(&path, &lines);
        set_modified(&path, 1_000_000 + i as u64);
        corpus.total_bytes += fs::metadata(&path).unwrap().len();
        corpus.total_records += lines.len() as u64;
        corpus.longest_line = corpus
            .longest_line
            .max(lines.iter().map(String::len).max().unwrap());
        corpus.files.push(path);
    }
    corpus
}

fn scan_codex(scanner: &mut CodexScanner, budget: RefreshBudget) -> (Vec<SessionSummary>, usize) {
    let mut sessions = Vec::new();
    let mut calls = 0;
    loop {
        let delta = scanner.scan_roster(NOW, budget).expect("scan");
        calls += 1;
        sessions.extend(delta.sessions);
        if delta.scan.done {
            return (sessions, calls);
        }
        assert!(calls < 100_000, "scan never finished");
    }
}

fn append(path: &Path, line: &str) -> u64 {
    let mut file = OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{line}").unwrap();
    line.len() as u64 + 1
}

#[test]
fn initial_roster_pass_streams_each_file_once_under_per_call_budget() {
    let corpus = codex_corpus(SESSIONS, RECORDS_PER_SESSION);
    let mut scanner = CodexScanner::new(&config_for(&corpus.root)).unwrap();
    let (mut bytes, mut records, mut calls) = (0, 0, 0usize);
    let mut seen = std::collections::HashSet::new();
    loop {
        perf::reset();
        let delta = scanner.scan_roster(NOW, CHUNK).unwrap();
        let used = perf::snapshot();
        // One record may finish after the budget is crossed; never more.
        assert!(
            used.bytes_read <= (CHUNK.max_bytes + corpus.longest_line + 1) as u64,
            "call {calls} read {} bytes",
            used.bytes_read
        );
        assert!(used.records_parsed <= CHUNK.max_records as u64);
        bytes += used.bytes_read;
        records += used.records_parsed;
        calls += 1;
        seen.extend(delta.sessions.into_iter().map(|s| s.key));
        if delta.scan.done {
            break;
        }
        assert!(calls < 100_000, "scan never finished");
    }
    assert_eq!(seen.len(), SESSIONS);
    // Limitation (v1): the initial pass streams every record of every file; a
    // header/tail metadata-only scan is deferred. What is enforced is the per-call
    // budget and that no byte is read twice.
    // The scan is chunked into many calls instead of one corpus-sized pass...
    assert!(calls >= (corpus.total_bytes / CHUNK.max_bytes as u64) as usize);
    // ...and across the whole pass every source byte is consumed exactly once.
    assert_eq!(bytes, corpus.total_bytes);
    assert_eq!(records, corpus.total_records);
}

#[test]
fn roster_is_capped_at_max_sessions() {
    let corpus = codex_corpus(SESSIONS + 25, 3);
    let mut scanner = CodexScanner::new(&config_for(&corpus.root)).unwrap();
    perf::reset();
    let (sessions, _) = scan_codex(&mut scanner, BIG);
    assert_eq!(sessions.len(), SESSIONS);
    // Omitted sources are never opened: only the newest 2,000 files are read.
    assert_eq!(perf::snapshot().records_parsed, (SESSIONS * 3) as u64);
}

#[test]
fn unchanged_file_refresh_does_no_full_reparse() {
    let corpus = codex_corpus(SESSIONS, RECORDS_PER_SESSION);
    let mut scanner = CodexScanner::new(&config_for(&corpus.root)).unwrap();
    let (sessions, _) = scan_codex(&mut scanner, BIG);
    assert_eq!(sessions.len(), SESSIONS);

    // Nothing changed: the second pass reads and parses nothing.
    perf::reset();
    let (changed, _) = scan_codex(&mut scanner, BIG);
    let idle = perf::snapshot();
    assert!(changed.is_empty(), "unchanged sources must not re-emit");
    assert_eq!((idle.bytes_read, idle.records_parsed), (0, 0));

    // One append to a source whose cursor is held: the new line is parsed, and the
    // consumed prefix (below the 4 MiB verify limit) is re-read once to prove it is
    // unchanged. Both reads are counted.
    let newest = corpus.files.last().unwrap();
    let before = fs::metadata(newest).unwrap().len();
    let added = append(newest, &assistant(TS, "one more reply"));
    perf::reset();
    let (changed, _) = scan_codex(&mut scanner, BIG);
    let tail = perf::snapshot();
    assert_eq!(changed.len(), 1);
    assert_eq!(tail.bytes_read, added + before);
    assert_eq!(tail.records_parsed, 1);

    // An append to an old source (cursor released to bound open files) re-reads that
    // one file and nothing else: one integrity pass over the old prefix, then a full
    // re-parse of the file from the start.
    let old = &corpus.files[0];
    let old_before = fs::metadata(old).unwrap().len();
    append(old, &assistant(TS, "late reply"));
    let old_len = fs::metadata(old).unwrap().len();
    perf::reset();
    let (changed, _) = scan_codex(&mut scanner, BIG);
    let reread = perf::snapshot();
    assert_eq!(changed.len(), 1);
    assert_eq!(reread.bytes_read, old_before + old_len);
    assert_eq!(reread.records_parsed, RECORDS_PER_SESSION as u64 + 1);
}

fn claude_config(root: &Path, tweak: impl FnOnce(&mut Config)) -> Config {
    let mut config = Config::load_with_env(
        &CliOverrides {
            claude_projects: Some(root.to_path_buf()),
            ..CliOverrides::default()
        },
        &Env::new(),
    )
    .unwrap();
    tweak(&mut config);
    config
}

fn claude_record(session: &str, i: usize, text: &str) -> Value {
    json!({
        "type": "assistant", "uuid": format!("u{i}"), "sessionId": session,
        "timestamp": TS,
        "message": {
            "id": format!("msg-{i:06}"), "model": "claude-test",
            "content": [{"type": "text", "text": text}],
            "usage": {"input_tokens": 10, "output_tokens": 4,
                      "cache_creation_input_tokens": 3, "cache_read_input_tokens": 2}
        }
    })
}

fn write_claude(root: &Path, name: &str, records: &[Value]) -> PathBuf {
    let path = root.join("project").join(name);
    write_lines(
        &path,
        &records.iter().map(ToString::to_string).collect::<Vec<_>>(),
    );
    path
}

fn scan_claude(scanner: &mut ClaudeScanner, budget: RefreshBudget) -> Vec<SessionSummary> {
    let mut sessions = Vec::new();
    for _ in 0..100_000 {
        let ProviderDelta {
            sessions: found,
            scan,
            ..
        } = scanner.scan_roster(NOW, budget).unwrap();
        sessions.extend(found);
        if scan.done {
            return sessions;
        }
    }
    panic!("scan never finished");
}

#[test]
fn claude_unchanged_refresh_reads_nothing_and_append_reads_only_the_tail() {
    // Held cursors are bounded by the process file-handle limit, so this corpus stays
    // below the lowest default (256 on macOS).
    const FILES: usize = 100;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let paths: Vec<PathBuf> = (0..FILES)
        .map(|f| {
            let records: Vec<Value> = (0..RECORDS_PER_SESSION)
                .map(|i| claude_record(&format!("s{f}"), f * 1000 + i, "hello"))
                .collect();
            write_claude(&root, &format!("s{f:03}.jsonl"), &records)
        })
        .collect();
    let mut scanner = ClaudeScanner::new(&claude_config(&root, |_| {})).unwrap();
    perf::reset();
    assert_eq!(scan_claude(&mut scanner, BIG).len(), FILES);
    let total: u64 = paths.iter().map(|p| fs::metadata(p).unwrap().len()).sum();
    assert_eq!(perf::snapshot().bytes_read, total);
    assert_eq!(
        perf::snapshot().records_parsed,
        (FILES * RECORDS_PER_SESSION) as u64
    );

    perf::reset();
    assert!(scan_claude(&mut scanner, BIG).is_empty());
    assert_eq!(perf::snapshot(), perf::Counters::default());

    let prefix = fs::metadata(&paths[7]).unwrap().len();
    let added = append(&paths[7], &claude_record("s7", 999_999, "tail").to_string());
    perf::reset();
    assert_eq!(scan_claude(&mut scanner, BIG).len(), 1);
    let tail = perf::snapshot();
    assert_eq!(tail.records_parsed, 1);
    // The new line plus an exact verification of the consumed prefix, which
    // detects in-place rewrites that also grow the file. Unchanged files still
    // read nothing (asserted above).
    assert_eq!(
        tail.bytes_read,
        added + prefix,
        "read {} bytes for {added} appended after a {prefix}-byte prefix",
        tail.bytes_read
    );
}

fn page_cost(page: &TranscriptPage) -> usize {
    let len = |t: &Option<BoundedText>| t.as_ref().map_or(0, |b| b.preview.len());
    page.events
        .iter()
        .map(|e| {
            len(&e.text)
                + e.tool
                    .as_ref()
                    .map_or(0, |t| t.name.len() + len(&t.input) + len(&t.output))
        })
        .sum()
}

/// Source size of each oversized row in the page test.
const HUGE_ROW: u64 = 100_000;

/// Largest preview any adapter allocates for one field.
const PREVIEW_CAP: u64 = 2048;

fn check_page_budget(
    reader: &mut dyn ProviderReader,
    key: &SessionKey,
    file_len: u64,
    max_bytes: usize,
) {
    let request = PageRequest {
        max_events: 500,
        max_bytes,
    };
    perf::reset();
    let page = reader
        .transcript_page(key, None, PageDirection::Forward, request)
        .expect("page");
    let used = perf::snapshot();
    assert!(!page.events.is_empty());
    assert!(
        page_cost(&page) <= max_bytes,
        "page exceeds {max_bytes} bytes"
    );
    assert!(page.truncated && page.has_more);
    // Retained previews: each body is cut at the 2 KiB cap before it is kept, so a small
    // page budget can still hold one cap-sized preview per row visited (the returned
    // rows plus the one that did not fit), plus short per-row metadata.
    const ROW_METADATA: u64 = 64;
    let visited = page.events.len() as u64 + 1;
    assert!(used.max_preview_bytes <= PREVIEW_CAP);
    assert!(
        used.preview_bytes <= visited * (PREVIEW_CAP + ROW_METADATA),
        "{:?} kept {} preview bytes for a {max_bytes}-byte page",
        reader.provider(),
        used.preview_bytes
    );
    // Transient buffers: sanitize/redact run over the whole source text before the cut,
    // so each visited row costs about two source-sized buffers (bounded by the 1 MiB
    // record cap). This, not the 2 KiB cap, is the real peak allocation per row; the
    // page byte cap bounds what is kept, not what is built.
    assert!(
        used.preview_temp_bytes <= visited * (2 * HUGE_ROW + 2 * ROW_METADATA),
        "{:?} built {} temporary preview bytes",
        reader.provider(),
        used.preview_temp_bytes
    );
    // Paging touches only this session file: at most one verification pass, one index
    // pass, and a re-read of each row it builds (1 MiB is the record cap).
    let bound = 2 * file_len + visited * 1_048_576;
    assert!(used.bytes_read <= bound, "read {} bytes", used.bytes_read);
}

#[test]
fn page_allocation_never_exceeds_max_page_bytes() {
    let huge = "x".repeat(HUGE_ROW as usize);
    let dir = tempfile::tempdir().unwrap();

    // Codex: 40 rows of 100 KB each.
    let root = dir.path().join("codex");
    let mut lines = vec![session_meta(TS, "oversized", "/work/demo")];
    lines.extend((0..40).map(|_| assistant(TS, &huge)));
    let path = dated(&root, "rollout-big.jsonl");
    write_lines(&path, &lines);
    let len = fs::metadata(&path).unwrap().len();
    let mut scanner = CodexScanner::new(&config_for(&root)).unwrap();
    let (sessions, _) = scan_codex(&mut scanner, BIG);
    let key = sessions[0].key.clone();
    for max_bytes in [8192, 1000] {
        check_page_budget(&mut scanner, &key, len, max_bytes);
    }

    // Claude: same shape.
    let root = dir.path().join("projects");
    let records: Vec<Value> = (0..40).map(|i| claude_record("big", i, &huge)).collect();
    let path = write_claude(&root, "big.jsonl", &records);
    let len = fs::metadata(&path).unwrap().len();
    let mut scanner = ClaudeScanner::new(&claude_config(&root, |_| {})).unwrap();
    let key = scan_claude(&mut scanner, BIG).remove(0).key;
    for max_bytes in [8192, 1000] {
        check_page_budget(&mut scanner, &key, len, max_bytes);
    }
}

#[test]
fn first_page_reads_one_session_not_the_corpus() {
    let corpus = codex_corpus(SESSIONS, RECORDS_PER_SESSION);
    let mut scanner = CodexScanner::new(&config_for(&corpus.root)).unwrap();
    let (sessions, _) = scan_codex(&mut scanner, BIG);
    let target = sessions
        .iter()
        .find(|s| s.key.provider_session_id == "sess-01234")
        .unwrap();
    let len = fs::metadata(&corpus.files[1234]).unwrap().len();
    perf::reset();
    let page = scanner
        .transcript_page(
            &target.key,
            None,
            PageDirection::Forward,
            PageRequest {
                max_events: 500,
                max_bytes: 262_144,
            },
        )
        .unwrap();
    let used = perf::snapshot();
    assert!(!page.events.is_empty());
    assert!(
        used.bytes_read <= 2 * len,
        "read {} of {len}",
        used.bytes_read
    );
    assert!(used.records_parsed <= 2 * RECORDS_PER_SESSION as u64);
}

#[test]
fn dedup_ledger_stays_within_entry_and_byte_caps() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let records: Vec<Value> = (0..5_000)
        .map(|i| claude_record("ledger", i, "hi"))
        .collect();
    write_claude(&root, "ledger.jsonl", &records);
    let per_message = 10 + 4 + 3 + 2;

    // Entry cap: later messages are not counted, and the aggregate says so.
    let mut scanner = ClaudeScanner::new(&claude_config(&root, |c| {
        c.dedup_ledger_entries = 1_000;
    }))
    .unwrap();
    let usage = scan_claude(&mut scanner, BIG).remove(0).usage.unwrap();
    assert_eq!(usage.class, AggregationClass::Partial);
    assert_eq!(usage.total, Some(1_000 * per_message));

    // Byte cap: ids are 10 bytes each, so 10,000 bytes hold exactly 1,000 of them.
    let mut scanner = ClaudeScanner::new(&claude_config(&root, |c| {
        c.dedup_ledger_bytes = 10_000;
    }))
    .unwrap();
    let usage = scan_claude(&mut scanner, BIG).remove(0).usage.unwrap();
    assert_eq!(usage.class, AggregationClass::Partial);
    assert_eq!(usage.total, Some(1_000 * per_message));

    // Within the caps, the same file stays exact.
    let mut scanner = ClaudeScanner::new(&claude_config(&root, |_| {})).unwrap();
    let usage = scan_claude(&mut scanner, BIG).remove(0).usage.unwrap();
    assert_ne!(usage.class, AggregationClass::Partial);
    assert_eq!(usage.total, Some(5_000 * per_message));
}

fn view_key() -> SessionKey {
    SessionKey {
        provider: Provider::Codex,
        provider_session_id: "loaded".into(),
        source: SourceKey::File("loaded.jsonl".into()),
    }
}

fn view_summary() -> SessionSummary {
    SessionSummary {
        key: view_key(),
        title: "Loaded".into(),
        title_derived: false,
        project: None,
        cwd: None,
        models: vec![],
        model_details: vec![],
        provider_version: None,
        created_at: Some(NOW - 86_400_000),
        last_event_at: Some(NOW - 540_000),
        last_persisted_at: Some(NOW - 540_000),
        last_completion: None,
        unfinished_work: false,
        displayed_event_count: 100_000,
        hidden_records: 0,
        source_available: true,
        archived: false,
        context_window: None,
        server_tools: None,
        lifecycle: Lifecycle::Unknown,
        process: ProcessAssociation::NotChecked,
        usage: None,
        lineage: Lineage {
            parent: None,
            fork: None,
            sidechain: false,
        },
        warnings: vec![],
    }
}

fn view_event(i: u64) -> Event {
    Event {
        id: format!("e{i}"),
        session: view_key(),
        timestamp: Some(NOW - 3_000_000 + i as i64 * 1_000),
        source_order: i,
        kind: EventKind::AssistantMessage,
        actor: Actor::Assistant,
        text: Some(BoundedText {
            preview: format!("message {i}"),
            byte_len: 12,
            truncated: false,
            locator: None,
        }),
        tool: None,
        usage: None,
        provenance: Provenance {
            provider: Provider::Codex,
            source: SourceKey::File("loaded.jsonl".into()),
            locator: SourceLocator {
                session: view_key(),
                kind: LocatorKind::FileOffset {
                    path: "loaded.jsonl".into(),
                    offset: i,
                },
                generation: None,
            },
            record_type: "test".into(),
            provider_version: None,
        },
    }
}

fn view_anchor(i: u64) -> PageAnchor {
    PageAnchor {
        timestamp: Some(NOW - 3_000_000 + i as i64 * 1_000),
        source_order: i,
        stable_id: format!("e{i}"),
    }
}

fn view_page(range: std::ops::Range<u64>) -> TranscriptPage {
    TranscriptPage {
        events: range.clone().map(view_event).collect(),
        diagnostics: vec![],
        next: Some(view_anchor(range.end - 1)),
        has_more: true,
        truncated: false,
    }
}

#[test]
fn loaded_transcript_stays_within_event_cap() {
    const PAGE: u64 = 500;
    let mut app = App::new(
        NOW,
        Thresholds {
            quiet: Duration::from_secs(300),
            stall: Duration::from_secs(900),
        },
    );
    app.apply_roster(vec![view_summary()], vec![]);
    assert!(app.open_inspector());
    let requests = app.take_requests();
    let [WorkerRequest::Select { request_id, .. }] = requests.as_slice() else {
        panic!("expected Select, got {requests:?}");
    };
    assert!(app.apply_transcript(*request_id, view_page(0..PAGE)));

    // Keep scrolling to the end and serving the next page until 2.5x the cap has
    // been delivered; the retained window never grows past the cap.
    let mut delivered = PAGE;
    let mut high_water = app.transcript().unwrap().events().len();
    while delivered < 5 * MAX_LOADED_EVENTS as u64 / 2 {
        app.inspector_move(isize::MAX);
        let load = app
            .take_requests()
            .into_iter()
            .find_map(|r| match r {
                WorkerRequest::LoadPage { request_id, .. } => Some(request_id),
                _ => None,
            })
            .expect("scrolling to the end requests the next page");
        assert!(app.apply_transcript(load, view_page(delivered..delivered + PAGE)));
        delivered += PAGE;
        let held = app.transcript().unwrap().events().len();
        assert!(held <= MAX_LOADED_EVENTS, "{held} events retained");
        high_water = high_water.max(held);
    }
    assert_eq!(
        high_water, MAX_LOADED_EVENTS,
        "the window did fill to the cap"
    );
}

/// Prints the timings recorded in the benchmark notes. Not a pass/fail check:
/// `cargo test --release --test perf_budget -- --ignored --nocapture benchmark_report`.
#[test]
#[ignore = "timing record; run manually"]
fn benchmark_report() {
    let build = Instant::now();
    let corpus = codex_corpus(SESSIONS, RECORDS_PER_SESSION);
    println!(
        "corpus: {SESSIONS} codex sessions, {} bytes, {} records, built in {:?}",
        corpus.total_bytes,
        corpus.total_records,
        build.elapsed()
    );

    // Initial roster: time to the first delta and to a complete pass, with the
    // worker's chunk shape (256 records per call).
    let worker_chunk = RefreshBudget {
        max_records: 256,
        max_bytes: 8 * 1024 * 1024,
        max_millis: 100,
    };
    let mut scanner = CodexScanner::new(&config_for(&corpus.root)).unwrap();
    perf::reset();
    let start = Instant::now();
    let first = scanner.scan_roster(NOW, worker_chunk).unwrap();
    let first_delta = start.elapsed();
    let first_count = first.sessions.len();
    let mut sessions = first.sessions;
    let mut calls = 1;
    let mut worst_call = first_delta;
    let mut done = first.scan.done;
    while !done {
        let call = Instant::now();
        let delta = scanner.scan_roster(NOW, worker_chunk).unwrap();
        worst_call = worst_call.max(call.elapsed());
        sessions.extend(delta.sessions);
        done = delta.scan.done;
        calls += 1;
    }
    let used = perf::snapshot();
    println!(
        "codex initial roster: first delta {first_delta:?} ({} sessions), complete {:?} in {calls} calls, \
         longest call {worst_call:?}, {} bytes read, {} records parsed",
        first_count,
        start.elapsed(),
        used.bytes_read,
        used.records_parsed
    );

    let key = sessions
        .iter()
        .find(|s| s.key.provider_session_id == "sess-01234")
        .unwrap()
        .key
        .clone();
    perf::reset();
    let start = Instant::now();
    let page = scanner
        .transcript_page(
            &key,
            None,
            PageDirection::Forward,
            PageRequest {
                max_events: 500,
                max_bytes: 262_144,
            },
        )
        .unwrap();
    println!(
        "codex first page: {:?}, {} events, {} bytes read",
        start.elapsed(),
        page.events.len(),
        perf::snapshot().bytes_read
    );

    perf::reset();
    let start = Instant::now();
    let (changed, _) = scan_codex(&mut scanner, worker_chunk);
    println!(
        "codex unchanged refresh: {:?}, {} changed, {} bytes read, {} records parsed",
        start.elapsed(),
        changed.len(),
        perf::snapshot().bytes_read,
        perf::snapshot().records_parsed
    );

    append(
        corpus.files.last().unwrap(),
        &assistant(TS, "one more reply"),
    );
    perf::reset();
    let start = Instant::now();
    let (changed, _) = scan_codex(&mut scanner, worker_chunk);
    println!(
        "codex refresh after one append: {:?}, {} changed, {} bytes read, {} records parsed",
        start.elapsed(),
        changed.len(),
        perf::snapshot().bytes_read,
        perf::snapshot().records_parsed
    );

    // Claude over a corpus below the lowest default file-handle limit.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    for f in 0..200 {
        let records: Vec<Value> = (0..RECORDS_PER_SESSION)
            .map(|i| claude_record(&format!("s{f}"), f * 1000 + i, "hello"))
            .collect();
        write_claude(&root, &format!("s{f:03}.jsonl"), &records);
    }
    let mut scanner = ClaudeScanner::new(&claude_config(&root, |_| {})).unwrap();
    perf::reset();
    let start = Instant::now();
    let sessions = scan_claude(&mut scanner, worker_chunk);
    println!(
        "claude initial roster (200 sessions): {:?}, {} sessions, {} bytes read",
        start.elapsed(),
        sessions.len(),
        perf::snapshot().bytes_read
    );
    let key = sessions[0].key.clone();
    perf::reset();
    let start = Instant::now();
    let page = scanner
        .transcript_page(
            &key,
            None,
            PageDirection::Forward,
            PageRequest {
                max_events: 500,
                max_bytes: 262_144,
            },
        )
        .unwrap();
    println!(
        "claude first page: {:?}, {} events, {} bytes read",
        start.elapsed(),
        page.events.len(),
        perf::snapshot().bytes_read
    );
    perf::reset();
    let start = Instant::now();
    let changed = scan_claude(&mut scanner, worker_chunk);
    println!(
        "claude unchanged refresh: {:?}, {} changed, {} bytes read",
        start.elapsed(),
        changed.len(),
        perf::snapshot().bytes_read
    );
}

/// Tabs expand to up to four spaces in `sanitize`, so a tab-heavy row builds buffers
/// larger than its source: the expanded output, then redaction's copy of it.
#[test]
fn preview_temporaries_include_tab_expansion_and_redaction_output() {
    const TABS: u64 = 25_000;
    const EXPANDED: u64 = 4 * TABS;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("codex");
    let mut lines = vec![session_meta(TS, "tabs", "/work/demo")];
    lines.extend((0..10).map(|_| assistant(TS, &"\t".repeat(TABS as usize))));
    write_lines(&dated(&root, "rollout-tabs.jsonl"), &lines);
    let mut scanner = CodexScanner::new(&config_for(&root)).unwrap();
    let (sessions, _) = scan_codex(&mut scanner, BIG);
    perf::reset();
    let page = scanner
        .transcript_page(
            &sessions[0].key,
            None,
            PageDirection::Forward,
            PageRequest {
                max_events: 500,
                max_bytes: 1000,
            },
        )
        .unwrap();
    let used = perf::snapshot();
    let visited = page.events.len() as u64 + 1;
    // Retained previews stay at the cap...
    assert!(used.max_preview_bytes <= PREVIEW_CAP);
    // ...but each visited row built at least the expanded buffer plus redaction's
    // owned copy (2 x 4N), and at most a grown sanitize buffer (< 2 x 4N) plus the copy.
    assert!(
        used.preview_temp_bytes >= visited * 2 * EXPANDED,
        "only {} temporary bytes counted for {visited} rows",
        used.preview_temp_bytes
    );
    assert!(
        used.preview_temp_bytes <= visited * 3 * EXPANDED + visited * 1024,
        "{} temporary bytes for {visited} rows",
        used.preview_temp_bytes
    );
}
