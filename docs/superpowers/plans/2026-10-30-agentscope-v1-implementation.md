# AgentScope v1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the approved three-provider, read-only AgentScope TUI: discover Codex, Claude Code, and OpenCode sessions, inspect transcripts and tool activity, show evidence-based status and usage, and never control or alter an agent.

**Architecture:** Provider adapters read external stores into a normalized in-memory model. One worker thread owns discovery, adapters, usage accounting, and process sampling; the ratatui UI thread owns view state and communicates by channels. Codex/Claude use an incremental JSONL cursor, OpenCode uses a read-only, authorizer-restricted SQLite connection, and all state is bounded and rebuilt from source.

**Tech Stack:** Rust stable edition 2024, `ratatui`/`crossterm`, `serde`/`serde_json`, `rusqlite` with bundled SQLite, `sysinfo`, `toml`, `thiserror`, `chrono`, `unicode-width`, `tempfile` for tests.

**Spec:** [`docs/superpowers/specs/2026-10-29-agentscope-v1-design.md`](../specs/2026-10-29-agentscope-v1-design.md)

## Global Constraints

- Stable Rust, edition 2024. Safe Rust only; `unsafe` requires a narrow justification and review.
- Windows and macOS are primary targets; Linux is best effort. CI runs `ubuntu-latest`, `windows-latest`, and `macos-latest`.
- No Tokio, async runtime, daemon, network listener, telemetry, web UI, MCP server, LLM integration, or persistent cache database.
- No agent launch, stop, resume, approval, messaging, signal, or configuration change.
- Read-only access. AgentScope never writes session content, the OpenCode main database, provider configuration, permissions, or credentials. Under ADR-0002, SQLite may create/update only transient `-shm`/`-wal` sidecars; never manually repair, delete, chmod, or copy provider files.
- Never read `auth.json`, `.credentials.json`, OpenCode `account`/`credential`/`control_account`/`session_share` tables, or process environments.
- Bounded values: 1 MiB record cap; 500 events or 256 KiB per page; 2,000 sessions per provider; 20,000 loaded events; 100,000 dedup entries or 8 MiB per session; refresh 2 s; process sample 5 s; quiet 5 min; stall 15 min.
- Sanitize and redact previews at ingest; raw source is loaded on demand. Fixtures are synthetic; never commit real transcripts.
- The UI thread performs no disk or SQLite I/O; the worker services page/source requests between refresh chunks.
- Do not run commit steps unless the user has explicitly authorized commits. The repository defaults to no commits; when unauthorized, stop after the verification step and report.
- Verification commands: `cargo fmt --all -- --check`, `cargo check --all-targets`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## Review Focus

1. **Malformed and live-changing JSONL:** truncated final lines, appended records, unknown record types, oversized records, and file replacement must be reported and never crash or duplicate the session. Tests live in Tasks 5, 8, and 10.
2. **Duplicate or cumulative usage:** Codex cumulative snapshots, Claude `iterations`, OpenCode message sums versus session totals, and missing categories must not double count or become zero. Tests live in Tasks 6, 8, 10, and 13.
3. **OpenCode read-only WAL and in-place rows:** the main database and session content must remain unchanged, transient sidecars may change, activity must follow `time_updated`, and paging must stay stable when a row is updated. Tests live in Tasks 11, 12, 13, and 21.
4. **Ambiguous process attribution:** executable names, working directories, and session IDs inside prompt text must never produce a matched session; candidates must remain visibly unlinked. Tests live in Task 14.
5. **Terminal escape and credential injection:** control sequences, bidi text, invalid bytes, and secret-shaped strings must be neutralized or redacted before display, and credential stores must be unreachable. Tests live in Tasks 4, 19, and 21.

## File Structure

| Path | Responsibility |
|---|---|
| `src/main.rs` | CLI parsing, TUI bootstrap, terminal guard, `inspect` diagnostic |
| `src/lib.rs` | Public module tree |
| `src/domain.rs` | Sessions, events, tools, usage, provenance, paging, warnings |
| `src/status.rs` | Pure status synthesis and evidence labels |
| `src/time.rs` | Timestamp parsing, local/relative formatting with explicit offset |
| `src/config.rs` | Config path, precedence, validation, price schedule |
| `src/discovery.rs` | Bounded provider root discovery and source selection |
| `src/ingest/mod.rs` | Ingest module boundary |
| `src/ingest/jsonl.rs` | Bounded incremental JSONL cursor, tail/read-at, rotation |
| `src/ingest/text.rs` | Sanitization, redaction, bounded previews |
| `src/providers/mod.rs` | `ProviderReader` trait, deltas, provider construction |
| `src/providers/codex.rs` | Codex discovery, roster, transcript, tools, usage |
| `src/providers/claude.rs` | Claude discovery, blocks, tools, usage ledger |
| `src/providers/opencode.rs` | Read-only SQLite, schema selection, roster, transcript |
| `src/usage.rs` | Token categories, totals, cost formulas, dedup ledger |
| `src/processes.rs` | sysinfo sampling, identity-argument matching, candidates |
| `src/worker.rs` | Worker thread, request/event protocol, refresh scheduling |
| `src/app.rs` | UI state, sorting/filtering/search, selection |
| `src/ui/mod.rs` | Layout, theme, banners, help, terminal guard |
| `src/ui/roster.rs` | Roster table and status/usage/process columns |
| `src/ui/inspector.rs` | Session header, usage, timeline, paging |
| `src/ui/details.rs` | Tool details, source view, diagnostics overlay |
| `src/ui/widgets.rs` | Shared text/truncation/status widgets |
| `tests/support/mod.rs` | Synthetic fixture builders and injected clock |
| `tests/support/codex.rs` | Codex JSONL fixture builder |
| `tests/support/claude.rs` | Claude JSONL fixture builder |
| `tests/support/opencode.rs` | OpenCode v2/legacy SQLite fixture builder |
| `tests/*.rs` | Contract, adapter, worker, TUI, privacy, acceptance tests |

## Task 1: CI Matrix and Baseline

**Files:**
- Modify: `.github/workflows/ci.yml`
- Test: existing `tests/codex_reader.rs`, `tests/inspect.rs`

**Interfaces:**
- Consumes: current `cargo` commands.
- Produces: three-OS CI matrix used by every later task.

- [ ] **Step 1: Change the CI job to a matrix**

Set `runs-on: ${{ matrix.os }}` and add:

```yaml
strategy:
  fail-fast: false
  matrix:
    os: [ubuntu-latest, windows-latest, macos-latest]
```

Keep the existing pinned action SHAs and the four verification commands.

- [ ] **Step 2: Run the existing local checks**

Run: `cargo fmt --all -- --check && cargo check --all-targets && cargo clippy --all-targets -- -D warnings && cargo test`

Expected: all pass on the current code.

- [ ] **Step 3: Commit (only if authorized)**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: verify on Windows, macOS, and Linux"
```

## Task 2: Domain Model and Status Synthesis

**Files:**
- Create: `src/domain.rs`, `src/status.rs`, `src/time.rs`
- Modify: `src/lib.rs`, `Cargo.toml` (`chrono` with `clock`)
- Test: `tests/status.rs`, `tests/time.rs`, `src/domain.rs` unit tests

**Interfaces:**
- Consumes: none.
- Produces: the shared types below; every later task uses them.

```rust
pub type Timestamp = i64; // epoch milliseconds, UTC

pub struct RefreshBudget { pub max_records: usize, pub max_bytes: usize, pub max_millis: u64 }
pub enum Provider { Codex, Claude, OpenCode }
pub enum SourceKey { File(PathBuf), DbRow { table: String, id: String } }
pub struct SessionKey { pub provider: Provider, pub provider_session_id: String, pub source: SourceKey }
pub enum SessionLink { Resolved(SessionKey), Ambiguous, Unresolved(String) }
pub struct Lineage { pub parent: Option<SessionLink>, pub fork: Option<SessionLink>, pub sidechain: bool }
pub enum EventKind { UserPrompt, AssistantMessage, Reasoning, Tool, System, Lifecycle, Compaction }
pub enum Actor { User, Assistant, System, Tool }
pub struct BoundedText { pub preview: String, pub byte_len: usize, pub truncated: bool, pub locator: Option<SourceLocator> }
pub struct SourceLocator { pub session: SessionKey, pub kind: LocatorKind }
pub enum LocatorKind { FileOffset { path: PathBuf, offset: u64 }, DbRow { table: String, row_id: String, part_index: Option<usize> } }
pub struct Provenance { pub provider: Provider, pub source: SourceKey, pub locator: SourceLocator, pub record_type: String, pub provider_version: Option<String> }
pub struct TokenCategories { pub input: Option<u64>, pub cached_input: Option<u64>, pub cache_write: Option<u64>, pub cache_write_5m: Option<u64>, pub cache_write_1h: Option<u64>, pub cache_read: Option<u64>, pub output: Option<u64>, pub reasoning: Option<u64>, pub total: Option<u64> }
pub enum AggregationClass { Reported, Derived, Partial, Estimated, Unknown }
pub struct CostEstimate { pub usd: Option<f64>, pub class: AggregationClass, pub as_of: Option<String>, pub note: Option<String> }
pub enum UsageScope { Session, Turn, Message }
pub struct UsageObservation { pub model: Option<String>, pub scope: UsageScope, pub categories: TokenCategories, pub provenance: Provenance }
pub struct UsageSummary { pub model: Option<String>, pub categories: TokenCategories, pub total: Option<u64>, pub class: AggregationClass, pub cost: CostEstimate }
pub struct ServerToolCounts { pub web_search: u64, pub web_fetch: u64 }
pub enum WarningKind { Io, Parse, Unsupported, Unavailable, Process, Pricing }
pub struct WarningKey { pub provider: Provider, pub source: String, pub kind: WarningKind }
pub struct Warning { pub key: WarningKey, pub message: String, pub count: u32 }
pub struct ScanProgress { pub provider: Provider, pub scanned: usize, pub total: Option<usize>, pub done: bool }
pub struct DiagnosticEntry { pub locator: SourceLocator, pub label: String, pub hidden: bool }
pub struct Event { pub id: String, pub session: SessionKey, pub timestamp: Option<Timestamp>, pub source_order: u64, pub kind: EventKind, pub actor: Actor, pub text: Option<BoundedText>, pub tool: Option<ToolInteraction>, pub usage: Option<UsageObservation>, pub provenance: Provenance }
pub struct ToolInteraction { pub name: String, pub call_id: Option<String>, pub input: Option<BoundedText>, pub output: Option<BoundedText>, pub status: ToolStatus, pub exit_code: Option<i32>, pub started_at: Option<Timestamp>, pub completed_at: Option<Timestamp>, pub orphan: bool }
pub enum ToolStatus { Pending, Completed, Error, Unknown }
pub struct SessionSummary { pub key: SessionKey, pub title: String, pub title_derived: bool, pub project: Option<String>, pub cwd: Option<PathBuf>, pub models: Vec<String>, pub provider_version: Option<String>, pub created_at: Option<Timestamp>, pub last_event_at: Option<Timestamp>, pub last_persisted_at: Option<Timestamp>, pub last_completion: Option<Timestamp>, pub unfinished_work: bool, pub displayed_event_count: usize, pub hidden_records: usize, pub source_available: bool, pub context_window: Option<u64>, pub server_tools: Option<ServerToolCounts>, pub lifecycle: Lifecycle, pub process: ProcessAssociation, pub usage: Option<UsageSummary>, pub lineage: Lineage, pub warnings: Vec<Warning> }
pub enum Lifecycle { Unknown, Ongoing, Completed, Failed, Cancelled }
pub enum ProcessAssociation { Matched { pid: u32, started_at: Option<Timestamp> }, Candidates { count: usize }, Unmatched, NotChecked, Unavailable }
pub struct PageAnchor { pub timestamp: Option<Timestamp>, pub source_order: u64, pub stable_id: String }
pub enum PageDirection { Forward, Backward }
pub struct PageRequest { pub max_events: usize, pub max_bytes: usize }
pub struct TranscriptPage { pub events: Vec<Event>, pub diagnostics: Vec<DiagnosticEntry>, pub next: Option<PageAnchor>, pub has_more: bool, pub truncated: bool }
pub struct Status { pub label: StatusLabel, pub evidence: String }
pub enum StatusLabel { Active, Recent, Quiet, PossiblyStalled, Complete, Failed, Cancelled, Unavailable, Unknown }
pub struct Thresholds { pub quiet: Duration, pub stall: Duration }
pub fn synthesize(session: &SessionSummary, now: Timestamp, thresholds: &Thresholds) -> Status;
```

`Debug` for `BoundedText` and `SessionSummary` must not print previews; use a non-exhaustive debug struct with lengths only.

- [ ] **Step 1: Write the failing status and ordering tests**

```rust
#[test]
fn status_truth_table() {
    // Thresholds: quiet 300s, stall 900s. `now` is 1,000s after the epoch.
    let now = 1_000_000;
    let cases = [
        ("matched recent", matched(), 999_000, None, true, true, StatusLabel::Active),
        ("matched quiet", matched(), 600_000, None, true, true, StatusLabel::Quiet),
        ("matched stalled", matched(), 0, None, true, true, StatusLabel::PossiblyStalled),
        ("matched completed", matched(), 0, Some(0), false, true, StatusLabel::Complete),
        ("matched completed then work", matched(), 999_000, Some(0), true, true, StatusLabel::Active),
        ("unmatched recent", unmatched(), 999_000, None, true, true, StatusLabel::Recent),
        ("unmatched completed quiet", unmatched(), 0, Some(0), false, true, StatusLabel::Complete),
        ("recent completion is recent", unmatched(), 999_000, Some(999_000), false, true, StatusLabel::Recent),
        ("source unavailable", unmatched(), 999_000, None, false, false, StatusLabel::Unavailable),
        ("process unavailable, source available", unavailable(), 999_000, None, true, true, StatusLabel::Recent),
    ];
    for (name, process, last, completion, unfinished, source_available, expected) in cases {
        let status = synthesize(&summary(process, last, completion, unfinished, source_available), now, &thresholds());
        assert_eq!(status.label, expected, "{name}: {}", status.evidence);
    }
}
```

Use `thresholds()` = `Thresholds { quiet: Duration::from_secs(300), stall: Duration::from_secs(900) }`.

```rust
#[test]
fn event_total_order_handles_missing_timestamps() {
    let anchors = [
        PageAnchor { timestamp: None, source_order: 2, stable_id: "b".into() },
        PageAnchor { timestamp: None, source_order: 1, stable_id: "a".into() },
        PageAnchor { timestamp: Some(1), source_order: 0, stable_id: "c".into() },
    ];
    assert_eq!(sorted_keys(anchors), vec!["a", "b", "c"]);
}
```

Add `tests/time.rs` covering relative labels (`12s`, `9m`, `2h`, `1d`) and absolute local formatting with an explicit UTC offset, including a DST/offset case.

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --test status`

Expected: FAIL with missing types/functions.

- [ ] **Step 3: Implement the domain types and `synthesize`**

Implement the type definitions and the nine status rules from the spec §9.3. `synthesize` is pure: injected `now`, no `SystemTime::now()`, no filesystem. It returns a short evidence string such as `last turn completed; process unmatched`.

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `cargo test --test status`

Expected: PASS. Add a `Debug` leak test using a sentinel in `BoundedText` and assert it is absent.

- [ ] **Step 5: Commit (only if authorized)**

```bash
git add src/domain.rs src/status.rs src/time.rs src/lib.rs Cargo.toml Cargo.lock tests/status.rs tests/time.rs
git commit -m "feat: add normalized domain model and status synthesis"
```

## Task 3: Configuration and Price Schedule

**Files:**
- Create: `src/config.rs`
- Modify: `src/lib.rs`, `Cargo.toml` (`serde` derive, `toml`, `thiserror`; dev `tempfile`)
- Test: `tests/config.rs`

**Interfaces:**
- Consumes: `Provider` from Task 2.
- Produces: `Config`, `CliOverrides`, `ConfigError`, `PriceSchedule`.

```rust
pub struct Config { pub refresh_ms: u64, pub process_ms: u64, pub process_enabled: bool, pub quiet_after_secs: u64, pub stall_after_secs: u64, pub max_sessions: usize, pub max_page_events: usize, pub max_page_bytes: usize, pub max_record_bytes: usize, pub dedup_ledger_entries: usize, pub dedup_ledger_bytes: usize, pub refresh_busy_timeout_ms: u64, pub page_busy_timeout_ms: u64, pub redaction: bool, pub providers: ProviderConfigs, pub pricing: PriceSchedule }
pub struct CliOverrides { pub config: Option<PathBuf>, pub codex_sessions: Option<PathBuf>, pub claude_projects: Option<PathBuf>, pub opencode_db: Option<PathBuf>, pub no_processes: bool, pub no_color: bool }
pub struct ProviderConfigs { pub codex: ProviderConfig, pub claude: ProviderConfig, pub opencode: ProviderConfig }
pub struct ProviderConfig { pub root: Option<PathBuf>, pub db: Option<PathBuf> }
pub struct PriceEntry { pub provider: Provider, pub model: String, pub as_of: Option<String>, pub input_per_mtok: f64, pub cached_input_per_mtok: Option<f64>, pub cache_write_per_mtok: Option<f64>, pub cache_write_5m_per_mtok: Option<f64>, pub cache_write_1h_per_mtok: Option<f64>, pub cache_read_per_mtok: Option<f64>, pub output_per_mtok: f64, pub reasoning_per_mtok: Option<f64> }
pub struct PriceSchedule { /* exact provider+model entries */ }
impl Config { pub fn load(cli: &CliOverrides) -> Result<Self, ConfigError>; }
impl PriceSchedule { pub fn price_for(&self, provider: Provider, model: &str) -> Option<&PriceEntry>; }
```

Test helpers `load_from`/`load_with` write a TOML file in a tempdir and build explicit env maps; they do not mutate process-global environment.

- [ ] **Step 1: Write the failing config tests**

```rust
#[test]
fn malformed_non_usd_and_bad_thresholds_are_fatal() {
    assert!(load_from("currency = \"EUR\"").is_err());
    assert!(load_from("quiet_after_secs = 900\nstall_after_secs = 300").is_err());
    assert!(load_from("this is not toml").is_err());
}

#[test]
fn cli_beats_env_beats_file_beats_defaults() {
    let cfg = load_with(cli_codex_sessions("/cli"), env_codex_home("/env"), file_root("/file"));
    assert_eq!(cfg.providers.codex.root, PathBuf::from("/cli"));
}

#[test]
fn price_lookup_is_exact_and_duplicate_keys_are_rejected() { /* ... */ }

#[test]
fn no_processes_no_color_and_config_path_are_preserved() { /* ... */ }
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --test config`

Expected: FAIL with missing config module.

- [ ] **Step 3: Implement config loading and validation**

Resolve the config path per OS, apply CLI > environment > file > defaults, and validate:
`quiet < stall`, positive intervals/caps, `currency == "USD"`, non-empty exact price keys.
`CODEX_HOME`/`CLAUDE_CONFIG_DIR` are provider homes and get `sessions`/`projects` appended; `--codex-sessions`/`--claude-projects` are exact roots.

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `cargo test --test config`

Expected: PASS.

- [ ] **Step 5: Commit (only if authorized)**

```bash
git add src/config.rs src/lib.rs Cargo.toml Cargo.lock tests/config.rs
git commit -m "feat: add configuration and price schedule"
```

## Task 4: Text Sanitization, Redaction, and Previews

**Files:**
- Create: `src/ingest/mod.rs`, `src/ingest/text.rs`
- Modify: `src/lib.rs`, `src/main.rs` (wire `inspect` after Task 5), `Cargo.toml` (`unicode-width`)
- Test: `tests/text_safety.rs`

**Interfaces:**
- Consumes: `BoundedText`, `SourceLocator` from Task 2.
- Produces: `sanitize`, `redact`, `display_safe`, `preview`.

```rust
pub fn sanitize(input: &str) -> String;
pub fn redact(input: &str) -> String;
pub fn display_safe(input: &str) -> String; // sanitize then redact
pub fn preview(input: &str, max_bytes: usize, max_cols: usize) -> BoundedText;
```

- [ ] **Step 1: Write the failing safety tests**

```rust
#[test]
fn strips_csi_osc_bidi_and_controls() {
    let input = "\x1b[31mred\x1b[0m \x1b]0;title\x07 \u{202e}reversed\x1b[2J";
    let out = sanitize(input);
    assert!(!out.contains('\u{1b}'));
    assert!(!out.contains('\u{202e}'));
    assert!(out.contains("red"));
}

#[test]
fn masks_common_secret_shapes_but_not_ordinary_code() {
    let out = redact("key sk-live-synthetic token=abc password = hunter2");
    assert!(!out.contains("sk-live-synthetic"));
    assert!(!out.contains("hunter2"));
    assert!(redact("let token = compute()").contains("compute()"));
}

#[test]
fn preview_truncates_by_bytes_and_display_columns() { /* assert truncated, byte_len, width */ }
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --test text_safety`

Expected: FAIL with missing functions.

- [ ] **Step 3: Implement sanitize, redact, and preview**

Strip ANSI CSI/OSC/ESC, C0/C1 controls, bidi controls, and zero-width directional characters; expand tabs; normalize CRLF. Redact the named credential patterns. `preview` stores only the display-safe text, the original byte length, and the locator. Use `unicode-width` for column measurement.

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `cargo test --test text_safety`

Expected: PASS.

- [ ] **Step 5: Commit (only if authorized)**

```bash
git add src/ingest/mod.rs src/ingest/text.rs src/lib.rs Cargo.toml Cargo.lock tests/text_safety.rs
git commit -m "feat: sanitize, redact, and bound untrusted display text"
```

## Task 5: JSONL Cursor and `inspect` Migration

**Files:**
- Create: `src/ingest/jsonl.rs`
- Modify: `src/main.rs`, `src/lib.rs`, `src/codex_reader.rs` (remove or re-export only if needed)
- Test: `tests/jsonl_cursor.rs`, existing `tests/inspect.rs`, `tests/codex_reader.rs`

**Interfaces:**
- Consumes: `SourceLocator`, `BoundedText`.
- Produces: `JsonlCursor`, `Record`, rotation and tail helpers.

```rust
pub enum Record { Json { offset: u64, value: serde_json::Value }, Invalid { offset: u64 }, TooLarge { offset: u64 }, Incomplete { offset: u64 } }
pub struct JsonlCursor { /* path, file, offset, pending bytes, identity */ }
impl JsonlCursor {
    pub fn open(path: PathBuf, max_record_bytes: usize) -> io::Result<Self>;
    pub fn next_record(&mut self) -> io::Result<Option<Record>>;
    pub fn read_at(&mut self, offset: u64, max_bytes: usize) -> io::Result<Option<Vec<u8>>>;
    pub fn tail_records(&mut self, max_records: usize, max_bytes: usize) -> io::Result<Vec<Record>>;
}
```

- [ ] **Step 1: Write the failing cursor tests**

```rust
#[test]
fn holds_partial_line_until_newline_arrives() { /* append "{\"a\":1}" then "\n" */ }

#[test]
fn reports_invalid_oversized_and_shrink_without_stopping() { /* ... */ }

#[test]
fn tail_records_returns_newest_complete_records_in_order() { /* ... */ }

#[test]
fn read_at_returns_the_record_bytes_for_source_view() { /* ... */ }
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --test jsonl_cursor`

Expected: FAIL with missing cursor.

- [ ] **Step 3: Implement the cursor and migrate `inspect`**

Move the bounded reader logic out of `codex_reader.rs` into `ingest/jsonl.rs`. Add the file-identity tuple: Unix `(dev, ino)`; Windows `(creation_time, length, last_write_time)`. Update `tests/codex_reader.rs` to target `ingest::jsonl::JsonlCursor`; remove `src/codex_reader.rs` and the `pub mod codex_reader;` export once no caller remains. `inspect` uses `JsonlCursor` and `text::display_safe`; it remains Codex-only and diagnostic only.

- [ ] **Step 4: Run the cursor and existing inspect tests**

Run: `cargo test --test jsonl_cursor --test inspect --test codex_reader`

Expected: PASS; `inspect` output keeps its current safety properties.

- [ ] **Step 5: Commit (only if authorized)**

```bash
git add src/ingest/jsonl.rs src/main.rs src/lib.rs src/codex_reader.rs tests/
git commit -m "refactor: add bounded JSONL cursor and keep inspect diagnostic"
```

## Task 5B: Format Probe Gate

**Files:**
- Create: `docs/superpowers/notes/2026-10-30-format-probes.md`
- Modify: none

**Interfaces:**
- Consumes: Task 5 cursor, Task 4 text safety.
- Produces: recorded, redacted answers that Tasks 6–13 must apply; no production code.

- [ ] **Step 1: Run the read-only probes**

Probe the local provider stores without copying transcript content:
- Codex: find a nonzero `cache_write_input_tokens` sample; determine whether it is a subset of `input_tokens`. If none exists, record “unverified.”
- Claude: confirm whether distinct content blocks can share a `message.id` and whether usage repeats.
- OpenCode: confirm the index order for `(session_id, time_created, id)`, in-place `time_updated` changes, and WAL states (live writer, stale WAL/missing shm, no active WAL).

Record only field names, counts, and sanitized conclusions in the notes file.

- [ ] **Step 2: Apply the findings before freezing interfaces**

Update the plan’s formulas/interface notes if a probe contradicts them. If cache-write inclusion is unverified, Task 6 must return `Unknown` cost for nonzero cache-write tokens rather than guessing. Do not start Task 6 until the cache-write decision and OpenCode order/update behavior are recorded.

- [ ] **Step 3: Verify the gate**

Run: `test -s docs/superpowers/notes/2026-10-30-format-probes.md`

Expected: exit 0; the notes contain all five probe answers or an explicit “unverified” for Codex cache-write.

- [ ] **Step 4: Commit (only if authorized)**

```bash
git add docs/superpowers/notes/2026-10-30-format-probes.md
git commit -m "docs: record read-only provider format probes"
```

## Task 6: Provider Contract and Usage Primitives

**Files:**
- Create: `src/providers/mod.rs`, `src/usage.rs`
- Modify: `src/lib.rs`
- Test: `tests/usage.rs`, compile test in `tests/provider_contract.rs`

**Interfaces:**
- Consumes: Task 2 domain types (including `TokenCategories`, `AggregationClass`, `CostEstimate`).
- Produces: `ProviderReader`, `ProviderDelta`, `ProviderError`, `SourceRecord`, token/cost helpers.

```rust
pub trait ProviderReader: Send {
    fn provider(&self) -> Provider;
    fn refresh(&mut self, now: Timestamp, budget: RefreshBudget) -> Result<ProviderDelta, ProviderError>;
    fn transcript_page(&mut self, key: &SessionKey, anchor: Option<PageAnchor>, direction: PageDirection, request: PageRequest) -> Result<TranscriptPage, ProviderError>;
    fn count_events_after(&mut self, key: &SessionKey, anchor: &PageAnchor) -> Result<usize, ProviderError>;
    fn source_record(&mut self, locator: &SourceLocator) -> Result<SourceRecord, ProviderError>;
}
pub struct ProviderDelta { pub sessions: Vec<SessionSummary>, pub removed: Vec<SessionKey>, pub warnings: Vec<Warning>, pub scan: ScanProgress }
pub enum ProviderError { Io(String), Parse(String), Unsupported(String), Unavailable(String), Internal(String) }
pub struct SourceRecord { pub raw: String, pub display: String, pub unavailable: Option<String> }
pub fn codex_total(input: &TokenCategories) -> Option<u64>;
pub fn claude_total(input: &TokenCategories) -> Option<u64>;
pub fn opencode_total(input: &TokenCategories) -> Option<u64>;
pub fn estimate_cost(provider: Provider, model: &str, usage: &TokenCategories, prices: &PriceSchedule) -> CostEstimate;
```

`RefreshBudget` is `{ max_records, max_bytes, max_millis }`; `refresh` returns `scan.done = false` when the budget is exhausted, and the worker calls it again after servicing requests.

- [ ] **Step 1: Write the failing usage tests**

```rust
#[test]
fn codex_uses_latest_cumulative_total_not_a_sum() { /* latest thread snapshot wins */ }

#[test]
fn claude_does_not_add_iterations_or_thinking() { /* categories and total */ }

#[test]
fn opencode_uses_session_totals_and_reported_zero_cost() { /* ... */ }

#[test]
fn missing_price_or_category_is_unknown_not_zero() { /* ... */ }

#[test]
fn claude_ttl_rates_produce_distinct_costs() {
    let usage = TokenCategories { cache_write: Some(1000), cache_write_5m: Some(600), cache_write_1h: Some(400), ..default() };
    // price 5m = 3.75/M, 1h = 6.0/M; expected usd = 0.00225 + 0.0024
    assert!((estimate_cost(Provider::Claude, "m", &usage, &prices()).usd.unwrap() - 0.00465).abs() < 1e-9);
}

#[test]
fn unverified_codex_cache_write_yields_unknown() {
    // Task 5B records whether cache_write is a subset of input.
    // If unverified, a nonzero cache_write must not produce a guessed cost.
    assert!(estimate_cost(Provider::Codex, "m", &codex_with_cache_write(), &prices()).usd.is_none());
}
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --test usage --test provider_contract`

Expected: FAIL with missing trait/helpers.

- [ ] **Step 3: Implement the trait, token helpers, and cost formulas**

Use the exact formulas from spec §8.3. Codex subtracts cached and cache-write from input with a clamp **only if Task 5B verified cache-write inclusion; otherwise any nonzero cache-write yields `Unknown`**. Claude uses the TTL split (`cache_write_5m`, `cache_write_1h`) with a single-write fallback; OpenCode prefers reported cost. Do not add reasoning/thinking to output. Missing category or price yields `Unknown`.

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `cargo test --test usage --test provider_contract`

Expected: PASS.

- [ ] **Step 5: Commit (only if authorized)**

```bash
git add src/providers/mod.rs src/usage.rs src/lib.rs Cargo.toml Cargo.lock tests/usage.rs tests/provider_contract.rs
git commit -m "feat: add provider contract and usage accounting"
```

## Task 7: Codex Discovery, Roster, and Usage

**Files:**
- Create: `src/discovery.rs`, `src/providers/codex.rs`, `tests/support/mod.rs`, `tests/support/codex.rs`
- Modify: `src/providers/mod.rs`, `src/lib.rs`
- Test: `tests/codex_roster.rs`

**Interfaces:**
- Consumes: `ProviderReader`, `JsonlCursor`, `Config`, `usage`.
- Produces: `CodexScanner`, `discover_codex`.

```rust
pub struct Discovery { pub sources: Vec<SourceFile>, pub warnings: Vec<Warning> }
pub struct SourceFile { pub path: PathBuf, pub modified: SystemTime, pub len: u64 }
pub struct CodexScanner { /* config, cursors, summaries */ }
impl CodexScanner {
    pub fn new(config: &Config) -> Result<Self, ProviderError>;
    pub fn scan_roster(&mut self, now: Timestamp, budget: RefreshBudget) -> Result<ProviderDelta, ProviderError>;
}
pub fn discover_codex(config: &Config) -> Result<Discovery, ProviderError>;
```

- [ ] **Step 1: Write the failing Codex roster tests**

```rust
#[test]
fn discovers_rollout_files_and_skips_symlinks_and_missing_roots() { /* ... */ }

#[test]
fn maps_meta_cwd_source_lineage_and_model() { /* session_meta + turn_context fixture */ }

#[test]
fn uses_first_user_prompt_as_derived_title_and_never_developer_text() { /* ... */ }

#[test]
fn usage_uses_latest_thread_snapshot_with_token_count_fallback() { /* ... */ }

#[test]
fn latest_cumulative_snapshot_wins_even_when_the_value_decreases() { /* ... */ }

#[test]
fn duplicate_session_ids_remain_separate_rows() { /* ... */ }

#[test]
fn malformed_unknown_and_oversized_records_are_counted() { /* ... */ }
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --test codex_roster`

Expected: FAIL with missing Codex adapter.

- [ ] **Step 3: Implement discovery and roster mapping**

Walk `$CODEX_HOME/sessions` or `~/.codex/sessions`, cap by file mtime, and parse `session_meta`, `turn_context`, `task_started`, `task_complete`, `token_usage_record`, and `token_count`. Use `id`/`session_id`, cwd, source, lineage, version, `model_context_window`, `source_available`, and last persisted time. Expose `scan_roster`; do not implement the full `ProviderReader` yet. Never sum cumulative usage.

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `cargo test --test codex_roster`

Expected: PASS.

- [ ] **Step 5: Commit (only if authorized)**

```bash
git add src/discovery.rs src/providers/codex.rs src/providers/mod.rs src/lib.rs tests/support tests/codex_roster.rs
git commit -m "feat: add Codex discovery, roster, and usage"
```

## Task 8: Codex Transcript, Tools, Paging, and Streaming

**Files:**
- Modify: `src/providers/codex.rs`, `src/discovery.rs`
- Test: `tests/codex_transcript.rs`

**Interfaces:**
- Consumes: Task 7 `CodexScanner` and `scan_roster`.
- Produces: `impl ProviderReader for CodexScanner`; `transcript_page`, `count_events_after`, `source_record`; `refresh` delegates to `scan_roster`.

- [ ] **Step 1: Write the failing transcript tests**

```rust
#[test]
fn renders_user_assistant_reasoning_and_one_row_per_tool() { /* item_completed fixture */ }

#[test]
fn tool_exit_code_is_tool_status_not_session_failure() { /* exit_code 1 */ }

#[test]
fn pending_and_orphan_tools_are_labeled() { /* ... */ }

#[test]
fn suppressed_custom_calls_are_source_only_and_addressable() { /* diagnostics locator */ }

#[test]
fn forward_and_backward_pages_do_not_duplicate_or_split_tool_pairs() { /* ... */ }

#[test]
fn partial_append_rotation_and_oversized_records_are_safe() { /* ... */ }
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --test codex_transcript`

Expected: FAIL with missing transcript methods.

- [ ] **Step 3: Implement the Codex event mapping**

Map `response_item` messages/reasoning, `event_msg.item_completed` tools, `compacted`, and hidden/unknown records. `custom_tool_call` and output are source-only and appear in `TranscriptPage.diagnostics`. Implement forward/backward pages, tail scan, count-after, and source locators. One `Tool` event per interaction; never split call/result. Finally, implement `ProviderReader for CodexScanner`; `refresh` delegates to `scan_roster`.

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `cargo test --test codex_transcript`

Expected: PASS.

- [ ] **Step 5: Commit (only if authorized)**

```bash
git add src/providers/codex.rs src/discovery.rs tests/codex_transcript.rs
git commit -m "feat: add Codex transcript, tools, and streaming"
```

## Task 9: Claude Discovery, Messages, Tools, and Titles

**Files:**
- Create: `src/providers/claude.rs`, `tests/support/claude.rs`
- Modify: `src/discovery.rs`, `src/providers/mod.rs`, `src/lib.rs`
- Test: `tests/claude_transcript.rs`

**Interfaces:**
- Consumes: `ProviderReader`, `JsonlCursor`, `text`.
- Produces: `ClaudeScanner`, `discover_claude`.

```rust
pub struct ClaudeScanner { /* config, cursors, summaries, usage ledgers */ }
impl ClaudeScanner {
    pub fn new(config: &Config) -> Result<Self, ProviderError>;
    pub fn scan_roster(&mut self, now: Timestamp, budget: RefreshBudget) -> Result<ProviderDelta, ProviderError>;
}
pub fn discover_claude(config: &Config) -> Result<Discovery, ProviderError>;
```

- [ ] **Step 1: Write the failing Claude mapping tests**

```rust
#[test]
fn maps_user_assistant_thinking_and_tool_blocks() { /* ... */ }

#[test]
fn links_tool_use_and_tool_result_by_id_and_marks_errors() { /* ... */ }

#[test]
fn prefers_ai_title_and_falls_back_to_first_non_tool_user_text() { /* ... */ }

#[test]
fn image_and_attachment_parts_are_placeholders_not_decoded() { /* ... */ }

#[test]
fn hidden_auxiliary_records_are_counted_not_rendered() { /* ... */ }
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --test claude_transcript`

Expected: FAIL with missing Claude adapter.

- [ ] **Step 3: Implement discovery and mapping**

Walk `$CLAUDE_CONFIG_DIR/projects` or `~/.claude/projects`, use `sessionId`/`cwd`/`version`, prefer `ai-title`, map content parts, and set `source_available` from the read result. Tool results link by `tool_use_id`; orphan results are labeled. Images are `[image]`.

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `cargo test --test claude_transcript`

Expected: PASS.

- [ ] **Step 5: Commit (only if authorized)**

```bash
git add src/providers/claude.rs src/discovery.rs src/providers/mod.rs src/lib.rs tests/support/claude.rs tests/claude_transcript.rs
git commit -m "feat: add Claude discovery and transcript mapping"
```

## Task 10: Claude Usage, Content Blocks, Ledger, and Edge Cases

**Files:**
- Modify: `src/providers/claude.rs`, `src/usage.rs`
- Test: `tests/claude_usage.rs`, `tests/claude_edge.rs`

**Interfaces:**
- Consumes: Task 9 `ClaudeScanner`.
- Produces: Claude usage summaries with `Derived`/`Partial` and a bounded dedup ledger.

- [ ] **Step 1: Write the failing usage and block tests**

```rust
#[test]
fn uses_top_level_usage_once_and_ignores_iterations() { /* ... */ }

#[test]
fn preserves_text_thinking_and_tool_blocks_sharing_a_message_id() { /* ... */ }

#[test]
fn dedups_usage_by_message_id_but_not_content_blocks() { /* ... */ }

#[test]
fn ledger_overflow_freezes_aggregate_and_marks_partial() { /* ... */ }

#[test]
fn cache_ttl_split_and_thinking_subset_are_not_double_counted() { /* ... */ }

#[test]
fn sidechain_is_labeled_and_never_changes_parent_totals() { /* ... */ }
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --test claude_usage --test claude_edge`

Expected: FAIL with missing usage/block behavior.

- [ ] **Step 3: Implement Claude usage and dedup**

Use top-level `message.usage`, split cache creation into `cache_write_5m`/`cache_write_1h`, ignore `iterations` and `thinking_tokens` as additive values, and preserve distinct content blocks by `uuid`/block identity. Map `server_tool_use` counts to `server_tools` and leave context unknown. Implement a per-session ledger capped by entries/bytes; on overflow freeze the verified aggregate and mark `Partial`. Label sidechains and keep them out of parent totals.

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `cargo test --test claude_usage --test claude_edge`

Expected: PASS.

- [ ] **Step 5: Commit (only if authorized)**

```bash
git add src/providers/claude.rs src/usage.rs tests/claude_usage.rs tests/claude_edge.rs
git commit -m "feat: add Claude usage, content blocks, and dedup ledger"
```

## Task 10B: Claude Transcript Contract, Paging, and Live Input

**Files:**
- Modify: `src/providers/claude.rs`
- Test: `tests/claude_transcript_live.rs`

**Interfaces:**
- Consumes: Task 9 `ClaudeScanner`, Task 10 usage/block mapping.
- Produces: `impl ProviderReader for ClaudeScanner`; `transcript_page`, `count_events_after`, `source_record`; reset-on-rotation behavior.

- [ ] **Step 1: Write the failing transcript-contract tests**

```rust
#[test]
fn forward_and_backward_pages_are_equivalent_and_never_split_tool_pairs() { /* ... */ }

#[test]
fn source_locator_round_trip_and_foreign_locator_rejection() { /* ... */ }

#[test]
fn partial_append_then_completion_adds_one_event() { /* ... */ }

#[test]
fn rotation_resets_cursor_content_and_usage_ledger() { /* ... */ }

#[test]
fn suppressed_and_unknown_records_appear_in_diagnostics() { /* ... */ }
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --test claude_transcript_live`

Expected: FAIL with missing Claude transcript contract.

- [ ] **Step 3: Implement Claude paging and live input**

Add `transcript_page`, `count_events_after`, `source_record`, and the `ProviderReader` impl. Use the shared total-order anchors and page byte budget; preserve tool pairs. On file rotation or shrink, reset the cursor, content cache, and usage ledger so no stale block or aggregate survives. Populate `TranscriptPage.diagnostics` with hidden/source-only entries.

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `cargo test --test claude_transcript_live`

Expected: PASS.

- [ ] **Step 5: Commit (only if authorized)**

```bash
git add src/providers/claude.rs tests/claude_transcript_live.rs
git commit -m "feat: add Claude transcript paging, source, and live input"
```

## Task 11: OpenCode Read-Only Connection, Schema, and Roster

**Files:**
- Create: `src/providers/opencode.rs`, `tests/support/opencode.rs`
- Modify: `src/providers/mod.rs`, `src/lib.rs`, `Cargo.toml` (`rusqlite` bundled)
- Test: `tests/opencode_roster.rs`

**Interfaces:**
- Consumes: `Config`, `ProviderReader`, domain types.
- Produces: `OpenCodeScanner`, `open_read_only`, `detect_schema`; roster scan only.

```rust
pub enum Schema { V2, Legacy, Unsupported }
pub fn open_read_only(path: &std::path::Path) -> Result<rusqlite::Connection, ProviderError>;
pub fn detect_schema(conn: &rusqlite::Connection) -> Result<Schema, ProviderError>;
pub struct OpenCodeScanner { /* config, connection, schema, cursors */ }
impl OpenCodeScanner {
    pub fn new(config: &Config) -> Result<Self, ProviderError>;
    pub fn scan_roster(&mut self, now: Timestamp, budget: RefreshBudget) -> Result<ProviderDelta, ProviderError>;
}
```

- [ ] **Step 1: Write the failing read-only and roster tests**

```rust
#[test]
fn opens_read_only_with_authorizer_and_rejects_sensitive_tables() { /* temp DB */ }

#[test]
fn selects_v2_then_legacy_then_unsupported() { /* ... */ }

#[test]
fn maps_title_directory_model_lineage_archived_and_time_updated() { /* ... */ }

#[test]
fn in_place_message_update_advances_last_persisted_at() { /* ... */ }

#[test]
fn live_writer_with_existing_shm_sees_wal_only_rows() { /* ... */ }

#[test]
fn stale_wal_with_writable_directory_reads_and_lets_sqlite_manage_sidecars() { /* ... */ }

#[test]
fn no_active_wal_opens_read_only() { /* ... */ }

#[test]
fn non_writable_directory_with_required_sidecar_shows_unavailable() { /* ... */ }

#[test]
fn main_database_is_unchanged_and_only_sidecars_may_change() { /* ... */ }
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --test opencode_roster`

Expected: FAIL with missing OpenCode adapter.

- [ ] **Step 3: Implement the read-only connection and roster**

Open with `SQLITE_OPEN_READ_ONLY`, `query_only`, and an authorizer allowing only SELECT on `session_v2`, `session_message`, `project`, and legacy tables. Never query account/credential/share tables. Use `time_updated` for `last_persisted_at`, map `lineage`, and set `source_available` from the read result. Detect v2/legacy/unsupported. Apply ADR-0002 sidecar policy: allow SQLite transient sidecars, never manually create/repair/copy, and show `Unavailable` when sidecars are required but the directory is not writable. Expose `scan_roster`; do not implement the full `ProviderReader` yet.

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `cargo test --test opencode_roster`

Expected: PASS.

- [ ] **Step 5: Commit (only if authorized)**

```bash
git add src/providers/opencode.rs src/providers/mod.rs src/lib.rs Cargo.toml Cargo.lock tests/support/opencode.rs tests/opencode_roster.rs
git commit -m "feat: add read-only OpenCode connection and roster"
```

## Task 12: OpenCode Transcript, Paging, and In-Place Updates

**Files:**
- Modify: `src/providers/opencode.rs`
- Test: `tests/opencode_transcript.rs`

**Interfaces:**
- Consumes: Task 11 `OpenCodeScanner` and `scan_roster`.
- Produces: `impl ProviderReader for OpenCodeScanner`; `transcript_page`, `count_events_after`, `source_record`; `refresh` delegates to `scan_roster`.

- [ ] **Step 1: Write the failing transcript tests**

```rust
#[test]
fn maps_user_assistant_system_compaction_idle_and_synthetic() { /* ... */ }

#[test]
fn maps_text_reasoning_tool_patch_file_agent_and_subtask_parts() { /* ... */ }

#[test]
fn tool_state_completed_and_error_map_to_tool_status() { /* ... */ }

#[test]
fn forward_and_backward_pages_are_equivalent_at_equal_timestamps() { /* ... */ }

#[test]
fn updated_row_invalidates_page_content_and_advances_activity() { /* ... */ }

#[test]
fn source_only_and_unknown_records_appear_in_diagnostics() { /* ... */ }
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --test opencode_transcript`

Expected: FAIL with missing transcript methods.

- [ ] **Step 3: Implement transcript mapping and paging**

Order by `(time_created, id)`, include part index, page by the indexed session/time/id key without `OFFSET`, and reverse backward pages for display. Detect `time_updated` changes and invalidate cached content. Map tool state and compaction/idle rows. Populate `TranscriptPage.diagnostics`. Finally, implement `ProviderReader for OpenCodeScanner`; `refresh` delegates to `scan_roster`.

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `cargo test --test opencode_transcript`

Expected: PASS.

- [ ] **Step 5: Commit (only if authorized)**

```bash
git add src/providers/opencode.rs tests/opencode_transcript.rs
git commit -m "feat: add OpenCode transcript and paging"
```

## Task 13: OpenCode Usage, Lifecycle, and Legacy Fallback

**Files:**
- Modify: `src/providers/opencode.rs`, `src/usage.rs`
- Test: `tests/opencode_usage.rs`, `tests/opencode_legacy.rs`

**Interfaces:**
- Consumes: Task 11/12 reader.
- Produces: session totals, reported cost, finish/idle lifecycle, legacy transcript.

- [ ] **Step 1: Write the failing usage and legacy tests**

```rust
#[test]
fn session_totals_beat_message_sums_and_zero_cost_stays_reported() { /* ... */ }

#[test]
fn missing_session_totals_derive_from_unique_messages_and_mark_derived() { /* ... */ }

#[test]
fn finish_and_idle_map_to_turn_evidence_not_session_failure() { /* ... */ }

#[test]
fn legacy_schema_falls_back_when_v2_is_absent_or_empty() { /* ... */ }
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --test opencode_usage --test opencode_legacy`

Expected: FAIL with missing usage/lifecycle/legacy behavior.

- [ ] **Step 3: Implement usage, lifecycle, and fallback**

Use `session_v2` totals and reported `cost`; derive from unique messages only when totals are absent, labeled `Derived`. Map `finish` and `idle` to turn evidence. Implement legacy `session`/`message`/`part` fallback for databases without v2.

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `cargo test --test opencode_usage --test opencode_legacy`

Expected: PASS.

- [ ] **Step 5: Commit (only if authorized)**

```bash
git add src/providers/opencode.rs src/usage.rs tests/opencode_usage.rs tests/opencode_legacy.rs
git commit -m "feat: add OpenCode usage, lifecycle, and legacy fallback"
```

## Task 14: Process Observer and Identity Matching

**Files:**
- Create: `src/processes.rs`
- Modify: `src/lib.rs`, `Cargo.toml` (`sysinfo`)
- Test: `tests/processes.rs`

**Interfaces:**
- Consumes: `ProcessAssociation`, `SessionKey`, `Provider`.
- Produces: `ProcessSnapshot`, `observe_processes`, `associate`.

```rust
pub struct ProcessSnapshot { pub sampled_at: Timestamp, pub processes: Vec<ProcessInfo> }
pub struct ProcessInfo { pub pid: u32, pub started_at: Option<Timestamp>, pub executable: String, pub cwd: Option<PathBuf>, pub command_line: Option<String> }
pub fn observe_processes(now: Timestamp) -> Result<ProcessSnapshot, ProcessError>;
pub fn associate(session: &SessionSummary, snapshot: &ProcessSnapshot) -> ProcessAssociation;
```

- [ ] **Step 1: Write the failing matcher tests**

```rust
#[test]
fn matches_only_identity_bearing_arguments() {
    assert!(matches("codex", &["resume", "01a1"]));
    assert!(matches("claude", &["--resume", "01a1"]));
    assert!(matches("opencode", &["--session", "ses_01a1"]));
}

#[test]
fn session_id_inside_prompt_or_reference_is_not_a_match() {
    assert!(!matches("claude", &["-p", "summarize 01a1"]));
    assert!(!matches("codex", &["queue", "01a1"]));
}

#[test]
fn candidates_never_become_matched_by_name_or_cwd() { /* ... */ }

#[test]
fn permission_failure_is_unavailable_not_not_found() { /* ... */ }
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --test processes`

Expected: FAIL with missing matcher.

- [ ] **Step 3: Implement the observer and matcher**

Use `sysinfo` for PID, start time, executable, cwd when available, and command line. Match only the provider-specific identity argument positions from spec §9.4. Show other processes as candidates. Never read environment or arbitrary memory. Sanitize command hints.

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `cargo test --test processes`

Expected: PASS. Add a positive test using a spawned test binary whose argv contains a synthetic session ID.

- [ ] **Step 5: Commit (only if authorized)**

```bash
git add src/processes.rs src/lib.rs Cargo.toml Cargo.lock tests/processes.rs
git commit -m "feat: add process observation and identity matching"
```

## Task 15: Worker Thread and Protocol

**Files:**
- Create: `src/worker.rs`
- Modify: `src/lib.rs`
- Test: `tests/worker.rs`

**Interfaces:**
- Consumes: `Vec<Box<dyn ProviderReader>>`, `Config`.
- Produces: `WorkerHandle`, `WorkerRequest`, `WorkerEvent`.

```rust
pub enum WorkerRequest { Refresh, Select { request_id: u64, key: SessionKey }, LoadPage { request_id: u64, key: SessionKey, anchor: Option<PageAnchor>, direction: PageDirection }, LoadSource { request_id: u64, locator: SourceLocator }, Shutdown }
pub enum WorkerEvent { Roster { generation: u64, sessions: Vec<SessionSummary>, warnings: Vec<Warning>, scan: ScanProgress }, Transcript { request_id: u64, page: TranscriptPage }, TranscriptInvalidated { key: SessionKey }, Source { request_id: u64, record: SourceRecord }, Processes { snapshot: ProcessSnapshot }, Warning { warning: Warning }, RefreshFinished { generation: u64 } }
pub struct WorkerHandle { pub tx: Sender<WorkerRequest>, pub rx: Receiver<WorkerEvent>, /* join handle */ }
```

- [ ] **Step 1: Write the failing worker tests with a fake provider**

```rust
#[test]
fn coalesces_refresh_and_drops_stale_transcript_responses() { /* fake provider */ }

#[test]
fn services_page_requests_between_refresh_chunks() { /* multi-chunk fake provider */ }

#[test]
fn provider_failure_keeps_last_good_sessions_and_replaces_warning() { /* ... */ }

#[test]
fn opencode_in_place_update_emits_transcript_invalidated_for_selected_session() { /* ... */ }

#[test]
fn shutdown_joins_without_panicking() { /* ... */ }
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --test worker`

Expected: FAIL with missing worker.

- [ ] **Step 3: Implement the worker**

Own the adapters and process sampler. Call `refresh` with a bounded `RefreshBudget`; while `scan.done` is false, service queued page/source requests, then continue the scan. Use request IDs and refresh generations; ignore stale responses. Emit `TranscriptInvalidated` when a provider reports an in-place change to the selected session so the UI reloads the visible page without counting it as a new event. Merge each fresh `ProcessAssociation` into the roster summaries before publishing them. Replace warnings by `WarningKey` per generation. Keep the worker panic-isolated and restore-friendly.

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `cargo test --test worker`

Expected: PASS.

- [ ] **Step 5: Commit (only if authorized)**

```bash
git add src/worker.rs src/lib.rs tests/worker.rs
git commit -m "feat: add worker protocol and refresh scheduling"
```

## Task 16: App State, Roster Logic, Search, Filter, and Sort

**Files:**
- Create: `src/app.rs`
- Modify: `src/lib.rs`
- Test: `tests/app_state.rs`

**Interfaces:**
- Consumes: `SessionSummary`, `Warning`, `Status`, `WorkerEvent`.
- Produces: `App`, `SortMode`, `FilterState`, `RosterRow`, key-independent roster operations.

```rust
pub struct App { /* sessions, filtered indices, selection, sort, filter, focus, screen, transcript, warnings, scan, live */ }
pub enum SortMode { LastActivity, Provider, Project, Tokens, Status }
pub enum UsageAvailability { Any, Known, Unknown, HasCost }
pub struct FilterState { pub providers: Option<Vec<Provider>>, pub statuses: Option<Vec<StatusLabel>>, pub usage: UsageAvailability, pub project: Option<String>, pub query: String }
pub struct RosterRow<'a> { pub session: &'a SessionSummary, pub status: Status }
impl App { pub fn apply_roster(&mut self, sessions: Vec<SessionSummary>, warnings: Vec<Warning>); pub fn visible_rows(&self) -> Vec<RosterRow<'_>>; pub fn selected_key(&self) -> Option<SessionKey>; pub fn move_selection(&mut self, delta: isize); pub fn sort(&mut self, mode: SortMode); }
```

- [ ] **Step 1: Write the failing app-state tests**

```rust
#[test]
fn merges_roster_deltas_and_preserves_selection() { /* ... */ }

#[test]
fn filters_and_sorts_with_unknown_tokens_last() { /* ... */ }

#[test]
fn search_matches_title_project_cwd_and_session_id() { /* ... */ }

#[test]
fn removing_the_selected_session_selects_the_nearest_row() { /* ... */ }

#[test]
fn warning_keys_replace_and_count_instead_of_accumulating() { /* ... */ }

#[test]
fn filters_usage_availability_and_composes_with_search() { /* known vs unknown vs has-cost */ }

#[test]
fn duplicate_parent_resolution_is_ambiguous_on_equal_recency_and_resolves_newest_otherwise() { /* ... */ }
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --test app_state`

Expected: FAIL with missing app state.

- [ ] **Step 3: Implement app state**

Apply roster generations, preserve selection by `SessionKey`, count states, filter by provider/status/project/query/usage availability, sort with `Unknown` tokens last, resolve parent links to the newest source or `Ambiguous` on a tie, and replace warnings by key. No formatting or I/O in this module.

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `cargo test --test app_state`

Expected: PASS.

- [ ] **Step 5: Commit (only if authorized)**

```bash
git add src/app.rs src/lib.rs tests/app_state.rs
git commit -m "feat: add roster state, filtering, and sorting"
```

## Task 17: Roster TUI and Terminal Guard

**Files:**
- Create: `src/ui/mod.rs`, `src/ui/roster.rs`, `src/ui/widgets.rs`
- Modify: `src/main.rs`, `src/app.rs`, `Cargo.toml` (`ratatui`, `crossterm`)
- Test: `tests/tui_roster.rs`

**Interfaces:**
- Consumes: `App`, `SessionSummary`, `Status`.
- Produces: `render_roster`, terminal initialization/restoration, main event loop.

- [ ] **Step 1: Write the failing roster render tests**

```rust
#[test]
fn renders_status_provider_session_last_tokens_and_process_at_120x40() { /* TestBackend */ }

#[test]
fn collapses_process_then_tokens_on_narrow_terminals() { /* 80x24 and 60x20 */ }

#[test]
fn empty_state_lists_checked_roots_and_warnings() { /* ... */ }

#[test]
fn no_color_mode_keeps_text_labels() { /* ... */ }

#[test]
fn non_tty_exits_with_a_message_without_entering_raw_mode() { /* piped stdio */ }

#[test]
fn help_and_version_exit_zero_without_starting_the_tui() { /* CLI */ }
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --test tui_roster`

Expected: FAIL with missing UI.

- [ ] **Step 3: Implement the roster and terminal lifecycle**

Parse the CLI before initializing the terminal: `--config`, `--codex-sessions`, `--claude-projects`, `--opencode-db`, `--no-processes`, `--no-color`, `--version`, `--help`, and `inspect <codex.jsonl>`. `inspect` stays diagnostic-only. Create the alternate-screen/raw-mode guard with restoration on normal exit, `Ctrl+c`, and panic. If stdin or stdout is not a TTY, print a concise error and exit before entering raw mode. Render the roster with icon+text status, provider counts, live indicator, filters/sort, warnings, and empty/too-small states. `q` quits only outside text input; `Ctrl+c` always quits.

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `cargo test --test tui_roster`

Expected: PASS.

- [ ] **Step 5: Commit (only if authorized)**

```bash
git add src/ui/mod.rs src/ui/roster.rs src/ui/widgets.rs src/main.rs src/app.rs Cargo.toml Cargo.lock tests/tui_roster.rs
git commit -m "feat: add roster TUI and terminal guard"
```

## Task 18: Inspector, Transcript, and Paging UI

**Files:**
- Create: `src/ui/inspector.rs`
- Modify: `src/ui/mod.rs`, `src/app.rs`
- Test: `tests/tui_inspector.rs`

**Interfaces:**
- Consumes: `TranscriptPage`, `Event`, `ToolInteraction`, `SessionSummary`.
- Produces: inspector header, timeline rows, tool expansion, page loading.

- [ ] **Step 1: Write the failing inspector tests**

```rust
#[test]
fn renders_header_usage_evidence_and_timeline() { /* TestBackend */ }

#[test]
fn renders_reported_context_window_and_server_tool_counts_and_unknown_otherwise() { /* ... */ }

#[test]
fn expands_a_tool_row_and_shows_status_duration_exit_and_truncation() { /* ... */ }

#[test]
fn loads_forward_and_backward_pages_without_duplicates() { /* ... */ }

#[test]
fn labels_reasoning_as_provider_persisted_and_images_as_placeholders() { /* ... */ }

#[test]
fn missing_timestamps_and_usage_show_unknown() { /* ... */ }
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --test tui_inspector`

Expected: FAIL with missing inspector.

- [ ] **Step 3: Implement the inspector and transcript**

Render the header, status evidence, usage, process association, and timeline. Enter expands details; Esc returns. Auto-load newer pages near the end; show `N new` without auto-scroll; `G` loads the tail. Keep loaded events bounded and do not split tool pairs.

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `cargo test --test tui_inspector`

Expected: PASS.

- [ ] **Step 5: Commit (only if authorized)**

```bash
git add src/ui/inspector.rs src/ui/mod.rs src/app.rs tests/tui_inspector.rs
git commit -m "feat: add inspector and transcript UI"
```

## Task 19: Details, Source View, Diagnostics, and Redaction

**Files:**
- Create: `src/ui/details.rs`
- Modify: `src/ui/inspector.rs`, `src/app.rs`
- Test: `tests/tui_details.rs`

**Interfaces:**
- Consumes: `ToolInteraction`, `SourceLocator`, `SourceRecord`.
- Produces: details pane, source overlay, diagnostics overlay, `x` redaction toggle.

- [ ] **Step 1: Write the failing details tests**

```rust
#[test]
fn shows_tool_arguments_output_and_oversized_markers() { /* ... */ }

#[test]
fn source_view_loads_by_locator_and_rejects_foreign_locators() { /* ... */ }

#[test]
fn diagnostics_lists_hidden_unknown_and_source_only_records() { /* ... */ }

#[test]
fn diagnostic_entry_selects_and_opens_its_source_record() { /* suppressed call -> locator -> source */ }

#[test]
fn redaction_toggle_reloads_source_and_never_retains_raw_text() { /* ... */ }
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --test tui_details`

Expected: FAIL with missing details/source UI.

- [ ] **Step 3: Implement details, source, and diagnostics**

Show tool arguments/output, status, duration, exit code, and truncation. `v` loads the redacted source by `SourceLocator`; `x` toggles redaction with a warning and forces a source reload. `D` opens diagnostics for hidden/unknown/source-only records. Reject locators that do not belong to the session.

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `cargo test --test tui_details`

Expected: PASS.

- [ ] **Step 5: Commit (only if authorized)**

```bash
git add src/ui/details.rs src/ui/inspector.rs src/app.rs tests/tui_details.rs
git commit -m "feat: add details, source, diagnostics, and redaction"
```

## Task 20: Search, Filter, Help, and Live Integration

**Files:**
- Modify: `src/app.rs`, `src/ui/mod.rs`, `src/ui/inspector.rs`, `src/worker.rs`
- Test: `tests/tui_flow.rs`, `tests/live_refresh.rs`

**Interfaces:**
- Consumes: Task 15 worker events, Task 16 app state, Tasks 17–19 UI.
- Produces: complete keyboard flow and live updates.

- [ ] **Step 1: Write the failing flow tests**

```rust
#[test]
fn keyboard_flow_discovers_opens_expands_searches_and_returns() { /* ... */ }

#[test]
fn slash_searches_roster_and_loaded_transcript_with_n_and_shift_n() { /* ... */ }

#[test]
fn filter_panel_and_sort_keys_update_the_roster() { /* ... */ }

#[test]
fn live_append_updates_roster_and_counts_new_events_without_auto_scroll() { /* ... */ }

#[test]
fn live_in_place_message_update_reloads_visible_row_without_new_event_or_selection_loss() { /* ... */ }

#[test]
fn process_snapshot_and_warning_banner_update_without_blocking_input() { /* ... */ }
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --test tui_flow --test live_refresh`

Expected: FAIL with missing key/live behavior.

- [ ] **Step 3: Implement keys, search, filter, help, and live updates**

Wire `/`, `f`, `s`, `n`/`N`, `?`, `r`, `p`, `D`, `v`, `x`, `q`, `Ctrl+c`, `j`/`k`, `g`/`G`, PageUp/PageDown, `Ctrl+d`/`Ctrl+u`, `Tab`, Enter, Esc. Poll crossterm and worker events; coalesce refresh; show scan progress, warnings, paused state, and new-event counts. No UI-thread I/O.

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `cargo test --test tui_flow --test live_refresh`

Expected: PASS.

- [ ] **Step 5: Commit (only if authorized)**

```bash
git add src/app.rs src/ui src/worker.rs tests/tui_flow.rs tests/live_refresh.rs
git commit -m "feat: add search, filters, help, and live refresh"
```

## Task 20B: Performance Corpus and Budget Checks

**Files:**
- Create: `tests/perf_budget.rs`
- Create: `docs/superpowers/notes/2026-10-30-benchmarks.md`
- Test: full suite

**Interfaces:**
- Consumes: adapters, worker, app, fixtures.
- Produces: deterministic work-budget tests and recorded Windows/macOS measurements.

- [ ] **Step 1: Add deterministic budget tests**

```rust
#[test]
fn roster_scan_reads_bounded_metadata_not_whole_files() { /* instrumented reader */ }

#[test]
fn page_allocation_never_exceeds_max_page_bytes() { /* oversized rows */ }

#[test]
fn unchanged_file_refresh_does_no_full_reparse() { /* byte counter */ }

#[test]
fn loaded_transcript_and_dedup_ledger_stay_within_caps() { /* ... */ }
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --test perf_budget`

Expected: FAIL until the instrumentation and caps are wired.

- [ ] **Step 3: Implement instrumentation and record measurements**

Add test-only counters for bytes read, records parsed, and allocated preview bytes. Measure initial roster availability, first-page latency, and refresh work on a synthetic 2,000-session corpus; record the results and the machine/provider versions in the notes file. Keep timing measurements out of deterministic pass/fail tests.

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `cargo test --test perf_budget`

Expected: PASS.

- [ ] **Step 5: Commit (only if authorized)**

```bash
git add tests/perf_budget.rs docs/superpowers/notes/2026-10-30-benchmarks.md
git commit -m "test: add performance budgets and benchmark record"
```

## Task 21: Read-Only, Privacy, Acceptance, and Documentation

**Files:**
- Create: `tests/read_only.rs`, `tests/privacy.rs`, `tests/acceptance.rs`
- Modify: `README.md`, `docs/PRODUCT.md`, `docs/ARCHITECTURE.md`, `docs/decisions/0002-opencode-readonly-wal-sidecars.md` (only if implementation reveals a correction)
- Test: full suite

**Interfaces:**
- Consumes: all prior tasks.
- Produces: release evidence.

- [ ] **Step 1: Write the failing read-only and privacy tests**

```rust
#[test]
fn provider_files_and_main_database_are_unchanged() { /* hash before/after */ }

#[test]
fn authorizer_denies_credential_tables_and_discovery_ignores_auth_files() { /* ... */ }

#[test]
fn sentinel_secret_never_appears_in_debug_warnings_or_errors() { /* ... */ }

#[test]
fn acceptance_scenarios_ac01_to_ac10_have_exercised_paths() { /* ... */ }
```

- [ ] **Step 2: Run the tests and confirm they fail or are incomplete**

Run: `cargo test --test read_only --test privacy --test acceptance`

Expected: FAIL until the final hardening assertions and docs are in place.

- [ ] **Step 3: Implement the audit assertions and update docs**

Assert no session-content/main-database/config/permission/credential writes; only transient SQLite sidecars may change under ADR-0002. Cover auth/credential fixtures, sentinel secrets, authorizer denial, and the ten acceptance scenarios. Include the Task 20B benchmark results and note the measured Windows/macOS conditions. Update README, PRODUCT, and ARCHITECTURE to verified behavior and limitations; keep ADR-0002 accurate.

- [ ] **Step 4: Run the full verification**

Run:

```bash
cargo fmt --all -- --check
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

Expected: all pass. Run the TUI against real local sessions on Windows and macOS, verify all three providers, drill-down, live append, process candidates, and no writes; record provider versions.

- [ ] **Step 5: Commit (only if authorized)**

```bash
git add README.md docs/ tests/
git commit -m "test: verify read-only behavior and update v1 documentation"
```

## Self-Review Notes

**Spec coverage:** Tasks 1–21 plus 5B, 10B, and 20B cover the design’s architecture, domain, config, sanitization, JSONL, provider adapters, usage/cost, process, worker, TUI, performance, privacy, read-only, and acceptance sections. The design’s “left to builder” items (visual polish, exact LRU sizing, fixture contents, warning wording) remain intentionally open.

**Type consistency:** `SessionKey`, `Lineage`, `PageAnchor`, `PageRequest`, `RefreshBudget`, `DiagnosticEntry`, `ProviderReader`, `TranscriptPage`, `SourceLocator`, `WarningKey`, `WorkerRequest`, and `WorkerEvent` are defined in Tasks 2, 6, and 15; later tasks consume those names without redefining them. `TokenCategories` carries the Claude 5m/1h cache-write split, and `TranscriptPage` carries source-only diagnostics.

**Review Focus tests:** Each of the five Review Focus items has an owning task and a named test. Malformed/live JSONL is covered by Tasks 5, 8, 10B, and 12; usage by Tasks 6, 8, 10, and 13; WAL/in-place updates by Tasks 11, 12, 15, 20, and 21; process attribution by Task 14; terminal/credential injection by Tasks 4, 19, and 21. Format probes run in Task 5B before the interfaces freeze.

**Proportion:** Tasks use signatures, test names, and assertions; bodies are described only where the spec pins an algorithm (usage formulas, status rules, identity matching, paging).
