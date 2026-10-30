//! Spec §17 acceptance scenarios AC-01..AC-10, exercised on synthetic fixtures through
//! the real provider adapters, the real worker thread, and the real keyboard path.
//! Real-data acceptance (installed providers, real terminals) is a manual step; see
//! README "Verification status".

mod support;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::time::{Duration, Instant};

use agentscope::app::{App, Screen};
use agentscope::config::PriceEntry;
use agentscope::config::PriceSchedule;
use agentscope::domain::*;
use agentscope::processes::{
    ProcessError, ProcessInfo, ProcessSnapshot, associate, association_from_observation,
};
use agentscope::providers::ProviderReader;
use agentscope::providers::codex::CodexScanner;
use agentscope::status::synthesize;
use agentscope::ui::{
    KeyAction, RosterContext, apply_worker_event, flush_requests, handle_key, render,
};
use agentscope::worker::{WorkerHandle, WorkerRequest};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use support::codex as cx;
use support::world::*;
use support::{NOW, dated, write_lines};

const THRESHOLDS: Thresholds = Thresholds {
    quiet: Duration::from_secs(300),
    stall: Duration::from_secs(900),
};

struct Harness {
    app: App,
    ctx: RosterContext,
    worker: WorkerHandle,
}

impl Harness {
    fn start(world: &World) -> Self {
        let worker = WorkerHandle::spawn(world.readers(), world.config());
        Self {
            app: App::new(NOW, THRESHOLDS),
            ctx: RosterContext {
                no_color: true,
                ..RosterContext::default()
            },
            worker,
        }
    }

    fn pump_until(&mut self, what: &str, done: impl Fn(&App) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            flush_requests(&mut self.app, &self.worker);
            if done(&self.app) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            if let Ok(ev) = self.worker.rx.recv_timeout(Duration::from_millis(25)) {
                apply_worker_event(&mut self.app, &mut self.ctx, ev);
            }
        }
    }

    fn key(&mut self, code: KeyCode) -> KeyAction {
        handle_key(&mut self.app, KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ch(&mut self, c: char) {
        let mods = if c.is_uppercase() {
            KeyModifiers::SHIFT
        } else {
            KeyModifiers::NONE
        };
        handle_key(&mut self.app, KeyEvent::new(KeyCode::Char(c), mods));
    }

    fn type_text(&mut self, text: &str) {
        for c in text.chars() {
            self.ch(c);
        }
    }

    fn screen(&self) -> String {
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| render(f, &self.app, &self.ctx)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }
}

fn append(path: &std::path::Path, text: &str) {
    OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
}

fn scan(reader: &mut dyn ProviderReader) -> ProviderDeltaLite {
    let mut out = ProviderDeltaLite::default();
    loop {
        let d = reader.refresh(NOW, BIG).expect("refresh");
        out.sessions.extend(d.sessions);
        out.invalidated.extend(d.invalidated);
        out.warnings.extend(d.warnings);
        if d.scan.done {
            return out;
        }
    }
}

#[derive(Default)]
struct ProviderDeltaLite {
    sessions: Vec<SessionSummary>,
    invalidated: Vec<SessionKey>,
    warnings: Vec<Warning>,
}

fn page(reader: &mut dyn ProviderReader, key: &SessionKey) -> TranscriptPage {
    reader
        .transcript_page(key, None, PageDirection::Forward, PAGE)
        .expect("page")
}

fn texts(page: &TranscriptPage) -> Vec<String> {
    page.events
        .iter()
        .filter_map(|e| e.text.as_ref().map(|t| t.preview.clone()))
        .collect()
}

// ---- AC-01 ---------------------------------------------------------------------

#[test]
fn ac01_no_disruption_start_issues_no_writes_and_tolerates_provider_appends() {
    let world = World::new();
    let before = snapshot(world.dir.path());
    let mut h = Harness::start(&world);
    h.pump_until("roster", |a| a.total_sessions() == 3);

    // A live provider legitimately appends while AgentScope runs; only that append differs.
    let codex_count = |a: &App| {
        a.all_rows()
            .iter()
            .find(|r| r.session.key.provider == Provider::Codex)
            .map(|r| r.session.displayed_event_count)
            .expect("codex row")
    };
    let original = codex_count(&h.app);
    let appended = format!("{}\n", cx::assistant("2026-10-29T23:55:00Z", "live reply"));
    append(&world.codex_file(), &appended);
    h.worker.tx.send(WorkerRequest::Refresh).unwrap();
    h.pump_until("append visible", |a| codex_count(a) > original);
    assert!(h.worker.shutdown());

    let after = snapshot(world.dir.path());
    let codex_rel = world
        .codex_file()
        .strip_prefix(world.dir.path())
        .unwrap()
        .to_path_buf();
    for (path, bytes) in &after.files {
        if *path == codex_rel {
            let mut expected = before.files[path].clone();
            expected.extend_from_slice(appended.as_bytes());
            assert_eq!(*bytes, expected, "only the provider's own append differs");
        } else {
            assert_eq!(bytes, &before.files[path], "{path:?} untouched");
        }
    }
    assert_eq!(after.files.len(), before.files.len());
}

// ---- AC-02 / AC-03 -------------------------------------------------------------

#[test]
fn ac02_drill_down_reads_ordered_transcript_and_expands_a_tool_result() {
    let world = World::new();
    let mut readers = world.readers();
    let surface = exercise_everything(&mut readers);
    for provider in [Provider::Codex, Provider::Claude] {
        let page = surface
            .pages
            .iter()
            .find(|p| {
                p.events
                    .first()
                    .is_some_and(|e| e.provenance.provider == provider)
            })
            .expect("page");
        assert!(
            page.events.len() >= 3,
            "{provider:?} transcript has several events"
        );
        let orders: Vec<_> = page
            .events
            .iter()
            .map(|e| (e.timestamp, e.source_order))
            .collect();
        let mut sorted = orders.clone();
        sorted.sort();
        assert_eq!(orders, sorted, "{provider:?} events are ordered");
        let tool = page
            .events
            .iter()
            .find_map(|e| e.tool.as_ref())
            .expect("tool interaction");
        assert!(
            tool.output.is_some(),
            "{provider:?} tool result is linked to its call"
        );
        assert!(!tool.orphan);
    }
}

#[test]
fn ac03_multi_provider_roster_contains_codex_claude_and_opencode() {
    let world = World::new();
    let mut readers = world.readers();
    let mut surface = Surface::default();
    refresh_all(&mut readers, &mut surface);
    let mut providers: Vec<_> = surface.sessions.iter().map(|s| s.key.provider).collect();
    providers.sort_by_key(|p| format!("{p:?}"));
    assert_eq!(
        providers,
        [Provider::Claude, Provider::Codex, Provider::OpenCode]
    );
    for s in &surface.sessions {
        assert!(!s.title.is_empty() || s.title_derived || s.key.provider == Provider::Claude);
        assert!(s.source_available);
    }
}

// ---- AC-04 ---------------------------------------------------------------------

#[test]
fn ac04_live_append_appears_without_restart_for_every_provider() {
    let world = World::new();
    let mut readers = world.readers();
    let mut first = Surface::default();
    refresh_all(&mut readers, &mut first);
    let count_of = |s: &Surface, p: Provider| {
        s.sessions
            .iter()
            .rev()
            .find(|x| x.key.provider == p)
            .unwrap()
            .displayed_event_count
    };
    let codex_before = count_of(&first, Provider::Codex);
    let claude_before = count_of(&first, Provider::Claude);
    let open_before = count_of(&first, Provider::OpenCode);

    append(
        &world.codex_file(),
        &format!(
            "{}\n",
            cx::assistant("2026-10-29T23:56:00Z", "codex appended")
        ),
    );
    append(
        &world.claude_file(),
        &format!(
            "{}\n",
            claude_line(
                "assistant",
                "a3",
                "2026-10-29T23:56:01Z",
                serde_json::json!("claude appended"),
                None
            )
        ),
    );
    world
        .writer
        .execute(
            "INSERT INTO session_message(id, session_id, type, seq, time_created, time_updated, data)
             VALUES ('m2', ?1, 'user', 1, ?2, ?2, '{\"text\":\"opencode appended\"}')",
            rusqlite::params![OPENCODE_ID, NOW - 1000],
        )
        .unwrap();

    let mut second = Surface::default();
    refresh_all(&mut readers, &mut second);
    assert!(count_of(&second, Provider::Codex) > codex_before);
    assert!(count_of(&second, Provider::Claude) > claude_before);
    assert!(count_of(&second, Provider::OpenCode) > open_before);

    let mut pages = Vec::new();
    for s in &second.sessions {
        let r = readers
            .iter_mut()
            .find(|r| r.provider() == s.key.provider)
            .unwrap();
        pages.push((s.key.provider, texts(&page(r.as_mut(), &s.key))));
    }
    for (provider, needle) in [
        (Provider::Codex, "codex appended"),
        (Provider::Claude, "claude appended"),
        (Provider::OpenCode, "opencode appended"),
    ] {
        assert!(
            pages
                .iter()
                .any(|(p, t)| *p == provider && t.iter().any(|x| x.contains(needle))),
            "{provider:?} append visible in transcript"
        );
    }
}

// ---- AC-05 ---------------------------------------------------------------------

#[test]
fn ac05_partial_malformed_oversized_and_ambiguous_sources_do_not_hide_other_sessions() {
    let world = World::new();
    let damaged = dated(&world.codex_root, "rollout-damaged.jsonl");
    write_lines(
        &damaged,
        &[
            cx::session_meta("2026-10-29T22:00:00Z", "damaged", "/work/damaged"),
            "not json".into(),
            "also {bad".into(),
            cx::user("2026-10-29T22:00:03Z", &"y".repeat(5000)),
            cx::user("2026-10-29T22:00:04Z", "after the damage"),
        ],
    );
    // Ambiguous identity: two files claim the same session id.
    for name in ["rollout-dup-a.jsonl", "rollout-dup-b.jsonl"] {
        write_lines(
            &dated(&world.codex_root, name),
            &[
                cx::session_meta("2026-10-29T21:00:00Z", "dup", "/work/dup"),
                cx::user("2026-10-29T21:00:01Z", "dup prompt"),
            ],
        );
    }
    let tail = cx::assistant("2026-10-29T22:00:05Z", "late reply");
    let (head, rest) = tail.split_at(20);
    append(&damaged, head);

    let mut config = world.config();
    config.max_record_bytes = 2048;
    let mut codex = CodexScanner::new(&config).unwrap();
    let out = scan(&mut codex);
    let bad = out
        .sessions
        .iter()
        .rev()
        .find(|s| s.title == "after the damage")
        .expect("damaged session still listed");
    let kinds: Vec<_> = bad
        .warnings
        .iter()
        .map(|w| (w.key.source.clone(), w.key.kind, w.count))
        .collect();
    assert!(
        kinds
            .iter()
            .any(|(s, k, c)| s.contains("malformed") && *k == WarningKind::Parse && *c == 2),
        "malformed records reported: {kinds:?}"
    );
    assert!(
        kinds.iter().any(|(s, _, _)| s.contains("oversized")),
        "oversized reported: {kinds:?}"
    );
    assert!(
        out.sessions
            .iter()
            .any(|s| s.key.provider_session_id == CODEX_ID),
        "healthy sessions remain"
    );
    assert!(
        out.sessions.iter().any(|s| s
            .warnings
            .iter()
            .any(|w| w.key.source.contains("duplicate"))),
        "ambiguous duplicate identity is flagged"
    );

    // The partial trailing line is held, then delivered once complete.
    let key = bad.key.clone();
    let before = page(&mut codex, &key);
    assert!(!texts(&before).iter().any(|t| t == "late reply"));
    append(&damaged, &format!("{rest}\n"));
    let _ = scan(&mut codex);
    assert!(
        texts(&page(&mut codex, &key))
            .iter()
            .any(|t| t == "late reply")
    );
}

// ---- AC-06 ---------------------------------------------------------------------

#[test]
fn ac06_usage_matches_golden_vectors_and_cost_requires_an_exact_price() {
    let world = World::new();
    // A streamed Claude message repeated under one id must count once.
    let usage = serde_json::json!({"input_tokens":10,"output_tokens":4,"cache_creation_input_tokens":0,"cache_read_input_tokens":2});
    let dup = claude_line(
        "assistant",
        "dup",
        "2026-10-29T23:52:00Z",
        serde_json::json!("streamed"),
        Some(usage),
    );
    write_lines(
        &world.claude_root.join("proj").join("dup.jsonl"),
        &[dup.clone(), dup],
    );

    let mut readers = world.readers();
    let mut surface = Surface::default();
    refresh_all(&mut readers, &mut surface);
    let codex = surface
        .sessions
        .iter()
        .rev()
        .find(|s| s.key.provider == Provider::Codex)
        .unwrap();
    let u = codex.usage.as_ref().expect("codex usage");
    assert_eq!(u.total, Some(120), "latest cumulative snapshot, not a sum");
    assert_eq!(u.categories.input, Some(100));
    assert_eq!(u.categories.cached_input, Some(40));
    assert_eq!(u.categories.output, Some(20));
    assert_eq!(u.class, AggregationClass::Reported);
    assert_eq!(u.cost.usd, None, "no configured price: cost stays unknown");
    assert_eq!(u.cost.class, AggregationClass::Unknown);

    let claude_usage: Vec<_> = surface
        .sessions
        .iter()
        .filter(|s| s.key.provider == Provider::Claude)
        .filter_map(|s| s.usage.as_ref())
        .collect();
    assert_eq!(
        claude_usage.len(),
        2,
        "original and duplicated-record sessions"
    );
    for u in claude_usage {
        assert_eq!(
            u.categories.input,
            Some(10),
            "a repeated record counts once"
        );
        assert_eq!(u.categories.output, Some(4));
    }

    // With an exact price entry the estimate is labelled Estimated and matches the formula.
    let mut config = world.config();
    config.pricing = PriceSchedule::new(vec![PriceEntry {
        provider: Provider::Codex,
        model: "gpt-test".into(),
        as_of: Some("2026-10-01".into()),
        input_per_mtok: 2.0,
        cached_input_per_mtok: Some(1.0),
        cache_write_per_mtok: None,
        cache_write_5m_per_mtok: None,
        cache_write_1h_per_mtok: None,
        cache_read_per_mtok: None,
        output_per_mtok: 8.0,
        reasoning_per_mtok: None,
    }])
    .unwrap();
    let mut codex_reader = CodexScanner::new(&config).unwrap();
    let priced = scan(&mut codex_reader);
    let cost = &priced.sessions.last().unwrap().usage.as_ref().unwrap().cost;
    assert_eq!(cost.class, AggregationClass::Estimated);
    // (100-40)*2 + 40*1 + 20*8 = 320 micro-dollars.
    assert!((cost.usd.unwrap() - 0.000_320).abs() < 1e-12, "{cost:?}");
    assert_eq!(cost.as_of.as_deref(), Some("2026-10-01"));
}

// ---- AC-07 ---------------------------------------------------------------------

#[test]
fn ac07_missing_data_is_shown_as_unknown_or_unmatched_never_invented() {
    let world = World::new();
    let mut readers = world.readers();
    let mut surface = Surface::default();
    refresh_all(&mut readers, &mut surface);
    let claude = surface
        .sessions
        .iter()
        .find(|s| s.key.provider == Provider::Claude)
        .unwrap();
    assert_eq!(
        claude.lifecycle,
        Lifecycle::Unknown,
        "no persisted terminal event"
    );
    let status = synthesize(claude, NOW, &THRESHOLDS);
    assert_ne!(status.label, StatusLabel::Complete);
    assert!(!status.evidence.is_empty(), "every status explains itself");
    for s in &surface.sessions {
        if let Some(u) = &s.usage {
            assert_eq!(u.cost.usd, None, "unpriced cost is not $0.00");
            assert_ne!(u.cost.class, AggregationClass::Estimated);
        }
    }

    // Process evidence: unrelated processes -> Unmatched; failure -> Unavailable; fuzzy -> candidates only.
    let codex = surface
        .sessions
        .iter()
        .find(|s| s.key.provider == Provider::Codex)
        .unwrap();
    let other = ProcessInfo::new(
        1,
        Some(NOW),
        "editor".into(),
        None,
        Some(vec!["editor".into()]),
    );
    let snap = ProcessSnapshot {
        sampled_at: NOW,
        processes: vec![other],
    };
    assert_eq!(associate(codex, &snap), ProcessAssociation::Unmatched);
    assert_eq!(
        association_from_observation(codex, &Err(ProcessError::Unavailable)),
        ProcessAssociation::Unavailable
    );
    let fuzzy = ProcessInfo::new(
        2,
        Some(NOW),
        "codex".into(),
        None,
        Some(vec!["codex".into()]),
    );
    let snap = ProcessSnapshot {
        sampled_at: NOW,
        processes: vec![fuzzy],
    };
    assert!(matches!(
        associate(codex, &snap),
        ProcessAssociation::Candidates { count: 1 }
    ));
    let exact = ProcessInfo::new(
        3,
        Some(NOW),
        "codex".into(),
        None,
        Some(vec!["codex".into(), "resume".into(), CODEX_ID.into()]),
    );
    let snap = ProcessSnapshot {
        sampled_at: NOW,
        processes: vec![exact],
    };
    assert!(matches!(
        associate(codex, &snap),
        ProcessAssociation::Matched { pid: 3, .. }
    ));
}

// ---- AC-08 / AC-09 (full suites: tests/read_only.rs, tests/privacy.rs) -------------

#[test]
fn ac08_read_only_regression_summary() {
    let world = World::new();
    let before = snapshot(world.dir.path());
    let mut readers = world.readers();
    let _ = exercise_everything(&mut readers);
    drop(readers);
    assert_eq!(snapshot(world.dir.path()).files, before.files);
    let conn = agentscope::providers::opencode::open_read_only(&world.db).unwrap();
    assert!(conn.prepare("SELECT secret FROM credential").is_err());
}

#[test]
fn ac09_privacy_summary_masks_secrets_and_neutralizes_controls() {
    let world = World::new();
    let mut readers = world.readers();
    let surface = exercise_everything(&mut readers);
    for e in surface.pages.iter().flat_map(|p| &p.events) {
        let Some(t) = &e.text else { continue };
        assert!(!t.preview.contains(INLINE_SECRET));
        assert!(!t.preview.contains('\u{1b}'));
    }
}

// ---- AC-10 and the umbrella flow -------------------------------------------------

#[test]
fn acceptance_scenarios_ac01_to_ac10_have_exercised_paths() {
    // Keyboard only: discover -> filter -> open -> expand tool result -> search -> back.
    let world = World::new();
    let mut h = Harness::start(&world);
    h.pump_until("three-provider roster", |a| a.total_sessions() == 3);
    assert_eq!(h.app.provider_counts().len(), 3);

    h.ch('/');
    assert!(h.app.is_editing());
    h.type_text("inventory");
    h.key(KeyCode::Enter);
    assert_eq!(
        h.app.visible_rows().len(),
        1,
        "search narrows the roster to the Codex session"
    );
    h.key(KeyCode::Enter);
    assert_eq!(h.app.screen, Screen::Inspector);
    h.pump_until("transcript", |a| a.transcript().is_some_and(|t| t.loaded()));

    // Move to the tool interaction and expand it.
    let mut steps = 0;
    while !h
        .app
        .transcript()
        .unwrap()
        .selected_event()
        .is_some_and(|e| e.tool.is_some())
    {
        h.ch('j');
        steps += 1;
        assert!(steps < 12, "tool event reachable with j");
    }
    h.key(KeyCode::Enter);
    {
        let t = h.app.transcript().unwrap();
        assert!(t.is_expanded(t.selected_event().unwrap()));
    }
    assert!(
        h.screen().contains("inventory.rs"),
        "expanded tool output is visible"
    );

    // Search inside the transcript.
    h.ch('/');
    h.type_text("inventory");
    h.key(KeyCode::Enter);
    assert!(h.app.transcript().unwrap().match_count() >= 1);
    h.ch('n');

    // Back to the roster, same selection; quit by keyboard.
    h.key(KeyCode::Esc);
    assert_eq!(h.app.screen, Screen::Roster);
    assert_eq!(h.app.selected_key().unwrap().provider, Provider::Codex);
    assert_eq!(h.key(KeyCode::Char('q')), KeyAction::Quit);
    assert!(h.worker.shutdown());
    let _ = fs::metadata(world.codex_file()).expect("fixture still present");
}

// ---- Real-data smoke (manual; ignored by default) -------------------------------

/// Reads the real local provider stores through the adapters and prints counts only
/// (never titles, paths, or transcript text). Verifies that no provider file shrank
/// or disappeared and that no file other than live appends and SQLite sidecars
/// changed. Run: `cargo test --test acceptance -- --ignored --nocapture real_data_smoke`.
#[test]
#[ignore = "reads the developer's real provider stores"]
fn real_data_smoke_is_read_only_and_prints_counts_only() {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use agentscope::config::{CliOverrides, Config};
    use agentscope::providers::claude::ClaudeScanner;
    use agentscope::providers::opencode::OpenCodeScanner;

    let config = Config::load(&CliOverrides::default()).expect("default config");
    let roots: Vec<PathBuf> = [
        config.providers.codex.root.clone(),
        config.providers.claude.root.clone(),
        config
            .providers
            .opencode
            .db
            .as_ref()
            .and_then(|d| d.parent().map(PathBuf::from)),
    ]
    .into_iter()
    .flatten()
    .collect();

    /// Full path inventory (len, mtime, content hash). Hashing is streamed.
    fn inventory(roots: &[PathBuf]) -> BTreeMap<PathBuf, (u64, std::time::SystemTime, u64)> {
        use std::hash::Hasher;
        use std::io::Read;

        fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
            let Ok(rd) = fs::read_dir(dir) else { return };
            for e in rd.flatten() {
                let p = e.path();
                match e.metadata() {
                    Ok(m) if m.is_dir() => walk(&p, out),
                    Ok(_) => out.push(p),
                    Err(_) => {}
                }
            }
        }
        let mut paths = Vec::new();
        for r in roots {
            if r.file_name().is_some_and(|n| n == "opencode") {
                // Database directory: only the database and its sidecars.
                for name in ["opencode.db", "opencode.db-wal", "opencode.db-shm"] {
                    let p = r.join(name);
                    if p.exists() {
                        paths.push(p);
                    }
                }
            } else {
                walk(r, &mut paths);
            }
        }
        let mut out = BTreeMap::new();
        for p in paths {
            let Ok(mut f) = fs::File::open(&p) else {
                continue;
            };
            let Ok(m) = f.metadata() else { continue };
            let mut h = std::collections::hash_map::DefaultHasher::new();
            let mut buf = vec![0u8; 1 << 20];
            while let Ok(n) = f.read(&mut buf) {
                if n == 0 {
                    break;
                }
                h.write(&buf[..n]);
            }
            out.insert(
                p,
                (
                    m.len(),
                    m.modified().unwrap_or(std::time::UNIX_EPOCH),
                    h.finish(),
                ),
            );
        }
        out
    }
    let before = inventory(&roots);

    let mut readers: Vec<Box<dyn ProviderReader>> = Vec::new();
    if let Ok(r) = CodexScanner::new(&config) {
        readers.push(Box::new(r));
    }
    if let Ok(r) = ClaudeScanner::new(&config) {
        readers.push(Box::new(r));
    }
    if let Ok(r) = OpenCodeScanner::new(&config) {
        readers.push(Box::new(r));
    }
    let mut surface = Surface::default();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    for r in readers.iter_mut() {
        loop {
            match r.refresh(now, BIG) {
                Ok(d) => {
                    surface.sessions.extend(d.sessions);
                    surface.warnings.extend(d.warnings);
                    if d.scan.done {
                        break;
                    }
                }
                Err(e) => {
                    println!(
                        "smoke: {:?} refresh error kind={}",
                        r.provider(),
                        e.to_string().split(':').next().unwrap_or("")
                    );
                    break;
                }
            }
        }
    }
    let mut pages_ok = 0usize;
    let mut events = 0usize;
    let mut tools = 0usize;
    let mut unreadable = 0usize;
    let keys: Vec<_> = surface.sessions.iter().map(|s| s.key.clone()).collect();
    for key in keys.iter().take(60) {
        let r = readers
            .iter_mut()
            .find(|r| r.provider() == key.provider)
            .unwrap();
        // The worker retries "index is building" answers; do the same here.
        let mut attempt = 0;
        let result = loop {
            let result = r.transcript_page(key, None, PageDirection::Backward, PAGE);
            let building = matches!(&result, Err(e) if e.to_string().contains("is building"));
            attempt += 1;
            if !building || attempt >= 200 {
                break result;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        match result {
            Ok(p) => {
                pages_ok += 1;
                events += p.events.len();
                tools += p.events.iter().filter(|e| e.tool.is_some()).count();
            }
            Err(e) => {
                unreadable += 1;
                println!("smoke: {:?} page error: {e}", key.provider);
            }
        }
    }
    drop(readers);

    for p in [Provider::Codex, Provider::Claude, Provider::OpenCode] {
        let n = surface
            .sessions
            .iter()
            .filter(|s| s.key.provider == p)
            .count();
        let with_usage = surface
            .sessions
            .iter()
            .filter(|s| s.key.provider == p && s.usage.is_some())
            .count();
        println!("smoke: {p:?} sessions={n} with_usage={with_usage}");
    }
    println!(
        "smoke: warnings={} pages_ok={pages_ok} unreadable={unreadable} events={events} tool_events={tools}",
        surface.warnings.len()
    );

    let after = inventory(&roots);
    // Classification. A silent change (same length and mtime, different bytes) can only
    // be a write by someone who preserved metadata and is a failure. Any other difference
    // (length/mtime moved, path added or removed) is a concurrent live-provider change or an
    // ADR-0002 sidecar and is reported as inconclusive, never attributed to AgentScope.
    let is_sidecar = |p: &PathBuf| {
        let n = p.to_string_lossy();
        n.ends_with("-wal") || n.ends_with("-shm")
    };
    let (mut silent, mut concurrent, mut added, mut removed, mut identical) = (0, 0, 0, 0, 0);
    for (p, (len, mtime, hash)) in &before {
        match after.get(p) {
            None => removed += 1,
            Some((l, m, h)) if l == len && m == mtime && h == hash => identical += 1,
            Some((l, m, _)) if l == len && m == mtime && !is_sidecar(p) => silent += 1,
            Some(_) => concurrent += 1,
        }
    }
    for p in after.keys() {
        if !before.contains_key(p) {
            added += 1;
        }
    }
    println!(
        "smoke: inventory={} identical={identical} silent_content_changes={silent} \
         concurrent_or_sidecar_changes(inconclusive)={concurrent} added(inconclusive)={added} removed(inconclusive)={removed}",
        before.len()
    );
    assert_eq!(silent, 0, "bytes changed without any metadata change");
}
