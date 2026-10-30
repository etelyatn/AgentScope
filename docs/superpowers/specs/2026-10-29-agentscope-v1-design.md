# AgentScope v1 — Three-Provider Read-Only TUI

**Status:** Draft for approval<br>
**Date:** 2026-10-29<br>
**Scope:** Full documented MVP (option B)<br>
**Related:** [PRODUCT.md](../../PRODUCT.md), [ARCHITECTURE.md](../../ARCHITECTURE.md), [ADR-0001](../../decisions/0001-passive-read-only-observation.md), [ADR-0002](../../decisions/0002-opencode-readonly-wal-sidecars.md)

## 1. Intent

AgentScope v1 is the first usable release: a local, keyboard-first terminal application that discovers Codex CLI, Claude Code, and OpenCode sessions on one machine, lets a developer read what each session did, and reports token usage, costs, and health only with visible evidence.

It is for a developer running several coding agents, often through different tools, who currently has to switch among three storage formats and process lists to answer: what sessions exist, what did this one do, is it still working, and what did it cost?

The user chose the full MVP rather than the Codex-only slice. v1 therefore includes all three providers, the roster, the transcript inspector, live refresh, process observation, usage accounting, cost estimates, and explicit lineage. This is more surface, and more ways to mislead, than a transcript-only cut. The design keeps every uncertain value labeled instead of removing the uncertainty.

**Success:** launch AgentScope beside running agents, see sessions from all three providers, open a session and read its recorded work and tool results, watch new complete records appear without restarting, and distinguish reported facts from derived values, estimates, and unknowns. Agent data, processes, and the user’s workflow remain unchanged.

## 2. Goals, non-goals, anti-goals

### Goals

| ID | v1 goal |
|---|---|
| G1 | Discover supported local sessions from Codex, Claude Code, and OpenCode, with configured root overrides. |
| G2 | Present a unified roster with provider, title/project, recency, model, usage, process evidence, and state evidence. |
| G3 | Read a chronological transcript with user prompts, assistant text, persisted reasoning, tool calls, tool results, errors, system events, and timestamps when present. |
| G4 | Report token categories and costs with provenance: reported, derived, estimated, partial, or unknown. |
| G5 | Correlate sessions with local processes only on strong evidence; show candidates separately when evidence is weak. |
| G6 | Keep the application read-only, local-only, keyboard-operable, and responsive while scanning. |
| G7 | Isolate provider failures so one corrupt or unsupported session does not hide healthy data. |

### Non-goals

- Launching, stopping, resuming, approving, messaging, retrying, or otherwise controlling agents.
- Replacing Hermes, OMP, Codex, Claude Code, or OpenCode.
- Hooks, wrappers, PTY interception, or provider configuration changes.
- A web UI, remote collectors, cloud sync, authentication, or multi-device aggregation.
- An MCP server, agent orchestrator, LLM reviewer, or autonomous fixer.
- Reconstructing hidden reasoning, unsaved conversations, or missing stdout.
- A persistent cache database or a full-text search index.
- Export, notifications, alerts, or background daemons.
- Promising that a quiet session is crashed, stalled, or finished without evidence.

### Anti-goals

| Anti-goal | Why it matters |
|---|---|
| Showing “Crashed” because no events arrived recently | Quiet is not a crash. |
| Summing cumulative Codex snapshots | Multiplies token totals. |
| Adding Claude `iterations` on top of message usage | Double counts the same request. |
| Summing OpenCode message usage to get a session total | Diverges from the session’s reported totals and double counts in-flight work. |
| Displaying missing cost as `$0.00` | Makes an unknown look free. |
| Matching a process by executable name or working directory | Produces false confidence. |
| Reading `auth.json`, credentials, or OpenCode account tables | Violates the privacy boundary. |
| Scanning the whole home directory or loading a 3.86 GiB database into memory | Slow, invasive, and unnecessary. |
| Reparsing every session file on every refresh | Makes the TUI unresponsive and wasteful. |
| Performing disk or SQLite work on the UI thread | Keypresses stall behind I/O. |
| Hiding a corrupt session’s source warning | Hides the reason data is missing. |
| Logging raw prompts, tool output, or command lines | Leaks private content into diagnostics. |
| Writing session content, the main database, config, permissions, credentials, or manually repairing/copying provider files; SQLite sidecars are the documented exception | Breaks the passive product contract. |

## 3. Constraints and observed baseline

### Engineering constraints

- Stable Rust, edition 2024, safe Rust by default. `unsafe` requires a narrow justification and review.
- Windows and macOS are the primary validation targets; Linux is best effort.
- No Tokio or async runtime. One standard-library worker thread is sufficient.
- No network listeners, HTTP clients, telemetry, update checks, or cloud dependencies.
- Provider stores are untrusted, read-only inputs. AgentScope must not write session content, the OpenCode main database, provider configuration, permissions, or credentials. The chosen OpenCode WAL policy permits SQLite to create/update only its transient `-shm`/`-wal` sidecars; this exception is documented in ADR-0002 and is not a license to repair, delete, or copy provider files.
- Bounded memory and bounded work per refresh; no full-file or full-database load on every tick.
- The design must remain usable with a 3.86 GiB OpenCode database and multi-megabyte JSONL sessions.

### Local baseline observed on 2026-10-29

Structure only was inspected; no transcript content is reproduced here or used as a fixture.

| Provider | Installed version | Local data observed | Relevant format facts |
|---|---|---|---|
| Codex CLI | 0.159.2 | 9 rollout JSONL files under `~/.codex/sessions/YYYY/MM/DD/` | `session_meta` with `id`/`session_id`, `cwd`, `source`, `parent_thread_id`; `response_item` messages and reasoning; `event_msg.item_completed` with `CommandExecution`, `FileChange`, `Extension`, `McpToolCall`; `task_started`/`task_complete`; `token_usage_record` plus duplicate `token_count` snapshots. |
| Claude Code | 2.1.311; files written by 2.1.281–2.1.310 | 19 JSONL files under `~/.claude/projects/<project>/` | `user`/`assistant` messages; content parts `text`, `thinking`, `tool_use`, `tool_result`, `image`; per-message usage with `cache_creation` TTL split and `iterations`; `ai-title`; system records; no explicit session terminal event. |
| OpenCode | 2.0.29 | 3.86 GiB `opencode.db` with WAL and shm; 1,001 sessions; 33,610 `session_message` rows | `session_v2` and `session_message` are the complete current generation; legacy `session`/`message`/`part` also exist. All 1,001 v2 sessions have `session_message` rows. Tool parts carry `state.status` (`completed`, `error`). Session totals and reported cost are stored per session. 33,606 of 33,610 message rows have `time_updated > time_created`, so rows are updated in place; refresh must use `time_updated`. |

### Process baseline

At design time, the machine had 2 `codex.exe` and 10 `claude.exe` processes. Their command lines were readable but contained no provider session ID. Therefore explicit session-to-process matching would produce **no matches** for those processes. v1 must show candidate counts and “unmatched,” not infer an active session from process presence.

The design does not read process environments; environment scraping would expand the privacy surface and is unnecessary.

## 4. Approaches considered

| Approach | What it delivers | Tradeoff | Decision |
|---|---|---|---|
| A. Three-provider inspector with explicit unknowns | All three providers, roster, transcript, reported usage, live refresh; process/health/cost deferred | Ships sooner, but does not meet the full MVP rows for process observation, stall evidence, cost, or lineage | Rejected because the user chose the full MVP |
| B. Full documented MVP | A plus process observation, stall heuristics, cost estimates, and lineage | More surface; process correlation is often unprovable, and cost estimates require user configuration | **Chosen** |
| C. Readers first, TUI second | Adapters and a CLI/JSON inspector before UI | De-risks formats, but v1 would not be the promised interface, and the TUI could still reshape the model | Rejected; adapter tests provide the same risk reduction without delaying the product surface |

Option B is the release target. The design still sequences adapters before UI work so each format is proven, but no intermediate build is presented as v1.

## 5. Architecture

### 5.1 Shape

```text
  Codex JSONL ──────> Codex adapter ──────┐
  Claude JSONL ─────> Claude adapter ─────┤
  OpenCode SQLite ──> OpenCode adapter ───┼──> normalized sessions/events/usage
  OS processes ─────> process observer ───┘                 │
                                                            v
                                                   worker thread
                                                            │
                                       commands (mpsc)      │ events (mpsc)
                                                            v
                                                   UI thread (ratatui)
```

The UI thread owns view state and never performs disk or database I/O. A single worker thread owns discovery, adapters, usage accounting, and process sampling. The worker sends complete snapshots or pages over a channel; the UI drops stale responses by request ID. No shared mutable state, no `Arc<Mutex<_>>` around the data model.

Refresh cadence defaults to 2 seconds for sessions and 5 seconds for process samples. The UI polls crossterm events with a 50 ms timeout and drains worker events on each tick. Initial scanning reports progress per provider; already-loaded sessions remain usable.

### 5.2 Module layout

```text
src/
  main.rs            # CLI entry, terminal guard, worker/UI startup
  lib.rs
  app.rs             # AppState, screens, selection, filters, key handling
  worker.rs          # worker thread, command/event protocol, scheduling
  domain.rs          # SessionKey, SessionSummary, Event, ToolInteraction, Usage, Provenance
  config.rs          # config path, TOML parsing, defaults, price schedule
  discovery.rs       # provider roots, overrides, bounded file discovery
  ingest/
    jsonl.rs         # bounded incremental JSONL reader and cursor
    text.rs          # bounded text, sanitization, redaction
  providers/
    mod.rs           # ProviderReader trait and shared adapter helpers
    codex.rs
    claude.rs
    opencode.rs
  usage.rs           # provider-aware token normalization and cost calculation
  processes.rs       # sysinfo snapshots, candidates, explicit matching
  ui/
    mod.rs           # layout, theme, help, banners
    roster.rs
    inspector.rs
    details.rs
    widgets.rs
```

Dependencies: `ratatui`, `crossterm`, `serde` (derive), `serde_json`, `rusqlite` with `bundled` SQLite, `sysinfo`, `toml`, `thiserror`, `unicode-width`, and a local-time formatting crate (`chrono` with `clock`, or equivalent). Dev-dependency `tempfile`. `rusqlite` bundled avoids relying on a system SQLite and keeps Windows builds reproducible. `unicode-width` is required for display-column truncation; `SystemTime` alone cannot format local timestamps.

### 5.3 Provider interface

Three real implementations justify a small trait. Keep it to the operations the worker needs:

```rust
trait ProviderReader {
    fn provider(&self) -> Provider;
    fn refresh(&mut self, now: SystemTime) -> Result<ProviderDelta, ProviderError>;
    fn transcript_page(
        &mut self,
        key: &SessionKey,
        anchor: Option<PageAnchor>,
        direction: PageDirection,
        request: PageRequest,
    ) -> Result<TranscriptPage, ProviderError>;
    fn count_events_after(&mut self, key: &SessionKey, anchor: &PageAnchor)
        -> Result<usize, ProviderError>;
    fn source_record(&mut self, locator: &SourceLocator) -> Result<SourceRecord, ProviderError>;
}
```

- `refresh` returns changed session summaries, removals, warnings, and scan progress. Each adapter owns its cursors and caches; the trait does not force a file/database shape.
- `transcript_page` returns normalized events and tool interactions with a stable continuation anchor. `PageAnchor` is a **total order** `(Option<Timestamp>, source_order, stable_id)`; `source_order` includes a part index for multi-part OpenCode messages. This handles missing timestamps and call/result records that are not adjacent. `PageDirection` is `Forward` or `Backward`. `PageRequest` carries both `max_events` and `max_bytes`; a page stops at whichever limit is reached first and never splits a tool call from its result. The page reports its `next_anchor` and whether more data exists.
- The inspector opens at the **oldest** page with `Forward` from start. `G`/`End` requests a `Backward` page from the end; `g`/`Home` requests the oldest page. JSONL adapters implement a bounded tail scan for the backward page; OpenCode uses a descending indexed query and reverses the result for display. Paging never depends on `OFFSET`.
- `count_events_after` supports the “3 new” indicator without loading new pages. It counts displayed work events after the anchor; it does not count hidden bookkeeping.
- `source_record` takes a validated `SourceLocator` rather than an `Event`, so source-only records (suppressed Codex calls, unknown records, hidden bookkeeping) remain addressable. The locator must belong to the emitting adapter’s session source; reject others.
- `ProviderError` is per-provider. The worker converts it to a keyed warning and keeps the last known good data.

### 5.4 Worker protocol

Requests: `Refresh`, `SelectSession`, `LoadNextPage`, `LoadSource`, `SetProcessSampling(bool)`, `Shutdown`.

Events: `ScanProgress`, `RosterDelta`, `TranscriptPage`, `SourceRecord`, `ProcessSnapshot`, `Warning`, `RefreshFinished`.

- Only the latest selection request matters; stale transcript responses carry a request ID and are ignored.
- Refresh is coalesced: a refresh request while one is running marks “refresh again after this pass.” Refresh is chunked per provider/file; between chunks the worker services queued page/source requests. Page queries use a shorter SQLite `busy_timeout` (default 250 ms) than roster refresh (default 2 s).
- A failed provider refresh emits a keyed warning and leaves that provider’s last good sessions in place; other providers continue.
- Warnings have a stable `WarningKey` (provider + source + kind) and carry a count. They are replaced per refresh generation, not appended, so a permanently missing root or locked database does not accumulate banners or flicker. Recovery clears the key.
- The worker keeps normalized session summaries for the bounded roster, one active transcript window, a small source-record LRU, and a bounded usage-dedup ledger. No persistent cache.

## 6. Domain model

### 6.1 Core types

| Type | Minimum contents |
|---|---|
| `Provider` | `Codex`, `Claude`, `OpenCode` |
| `SessionKey` | provider, provider session ID, and source identity (file path or database row); local host placeholder is implicit in v1 |
| `SessionSummary` | key, title, project, cwd, model(s), source path/locator, provider version, created time, `last_event_at`, `last_persisted_at`, `displayed_event_count`, hidden-record count, lifecycle evidence, activity, usage summary, process association, parent/child links, warnings |
| `Event` | stable ID, session key, source order, timestamp optional, kind, actor, bounded text, optional tool interaction, optional usage observation, provenance |
| `PageAnchor` / `PageDirection` / `PageRequest` | total order `(Option<Timestamp>, source_order, stable_id)` or start/end; `Forward`/`Backward`; `max_events` + `max_bytes` |
| `SourceLocator` | session key + `FileOffset { path, offset }` or `DbRow { table, row_id, part_index }`; validated against the emitting source |
| `ToolInteraction` | name, call ID, input, output, status, exit code, started/completed times, duration, linked event IDs, provenance |
| `UsageObservation` | model, scope, categories, reported total if any, provenance, aggregation class |
| `ProcessObservation` | PID, process start time, executable, optional cwd/command hint, sample time, association confidence |
| `Provenance` | provider, source path or database locator, record offset/line/ID, provider record type, provider version |
| `BoundedText` | display-safe preview (control-sanitized and redacted by default), byte length, truncation flag, raw `SourceLocator` |
| `Warning` / `WarningKey` | provider + source + kind identity, safe message, count; replaced per refresh generation |

### 6.2 Event kinds and ordering

`UserPrompt`, `AssistantMessage`, `Reasoning`, `Tool`, `System`, `Lifecycle`, `Compaction`.

A `Tool` event carries one `ToolInteraction` containing the call and result (either side may be absent). An orphan result is a `Tool` event with no call and a warning; a pending call is a `Tool` event with no result. The UI expands the interaction to show arguments and output; there is not a separate call row and result row.

Usage observations attach to messages/events and to the session summary; standalone usage snapshots (Codex `token_count`/`token_usage_record`, OpenCode per-message token objects) are not separate timeline rows. The inspector has a usage section and the source view exposes the raw records.

Ordering is by timestamp when present, then by source order, then by stable ID. Missing timestamps sort before/after consistently via the `Option<Timestamp>` total order, never by fabricated time. OpenCode orders by `time_created`, `id`, then part index (not `seq`); Codex uses record ordinal; Claude uses file order plus `uuid`/`timestamp`. A tool call and its result are one `ToolInteraction` and one displayed row even when their source records are not adjacent; page boundaries never split the pair.

Two clocks are tracked. `last_event_at` is the newest **displayed work** event (message, reasoning, tool call/result, lifecycle, system/compaction). `last_persisted_at` is the newest complete record of any kind, including hidden bookkeeping such as `token_count`, `api-request*`, `file-history*`, `cost-state`, and `attachment`. The roster’s `LAST` column and the quiet/stall heuristics use `last_persisted_at`, because a hidden record is still evidence that the provider wrote. The inspector shows both clocks and the hidden-record count; a hidden record is not treated as proof of progress.

### 6.3 Deduplication rules

- **Codex usage:** `token_usage_record.thread_token_usage` is authoritative. If absent, use the latest `token_count.info.total_token_usage`. Never add both. `last_token_usage` and `turn_token_usage` are for per-turn detail only.
- **Codex tools:** `event_msg.item_completed` is authoritative for tool activity. `response_item.custom_tool_call`/`function_call` and their outputs remain available through the source view but are not rendered as a second tool row. Item IDs and call IDs do not match in observed data, so do not attempt to join them by ID.
- **Claude usage:** each assistant message’s top-level `message.usage` is authoritative. `iterations` is a breakdown, not an additional amount. Deduplicate by `message.id`; deduplicate displayed events by record `uuid`.
- **Claude tools:** link `tool_use.id` to `tool_result.tool_use_id`. A result without a call is shown as an orphan with a warning.
- **Claude duplicate messages:** deduplicate **usage** by `message.id`, but preserve distinct content blocks by record `uuid` and block identity. A response may persist text, thinking, and tool-use blocks in separate records that share a message ID; dropping all but the last would lose content and orphan tool results. Replace a block only when a record is a demonstrated revision of the same block identity, and count usage once. The duplicate/revision count is shown in the inspector.
- **OpenCode usage:** `session_v2` totals are authoritative for the roster. `session_message` token fields are for per-message detail. Do not sum message totals to replace session totals; observed message sums can lag session totals while work is in flight.
- **OpenCode events:** `session_message.id` is stable. Do not read legacy `message`/`part` when the v2 generation is present; legacy is a fallback for databases without v2 tables.
- **Duplicate session IDs:** v1 does not merge sources that share a provider session ID. Each source (file or database row) is its own `SessionKey` and roster row, marked `Duplicate ID`, because merging cumulative Codex snapshots or two Claude histories would make the summary and totals ambiguous. Parent/child links resolve to the most recent source; if that is also ambiguous, the link is shown as `Ambiguous` rather than guessed.

### 6.4 Parent/child lineage

Display lineage only from explicit fields:

- Codex: `session_meta.parent_thread_id`; the `source` object may also identify a subagent and parent.
- Claude: `parentUuid` within a file, `isSidechain`, and explicit session identifiers. A sidechain is labeled, not merged into the parent.
- OpenCode: `session_v2.parent_id` and `fork_session_id`.

Never infer lineage from time, project, or working directory. Do not roll child usage into parent totals; each session keeps its own reported usage.

### 6.5 Titles and projects

Title fallback order:

1. Provider-generated title (OpenCode `title`, Claude `ai-title`, Codex subagent nickname when present).
2. First non-injected user prompt text, truncated to 80 display columns.
3. Provider session ID.
4. File stem.

`developer`/system instructions are not titles. Claude user messages that contain only `tool_result` are not titles.

Project display uses, in order: provider project name, session `cwd`/`directory` basename, “Unknown.” Claude uses the record `cwd` basename; the encoded project directory name (for example `D--Projects-Foo`) is only a fallback and is marked `Derived`. The full cwd is in the inspector. Titles and projects are marked `Derived` when they come from prompts.

## 7. Provider adapters

### 7.1 Discovery

| Provider | Default root | Override |
|---|---|---|
| Codex | `$CODEX_HOME/sessions` or `~/.codex/sessions` | `--codex-sessions` (exact root), config `root` |
| Claude | `$CLAUDE_CONFIG_DIR/projects` or `~/.claude/projects` | `--claude-projects` (exact root), config `root` |
| OpenCode | `$XDG_DATA_HOME/opencode/opencode.db` or `~/.local/share/opencode/opencode.db` | `--opencode-db`, config `db` |

Environment variables name the provider home/config directory and have `sessions`/`projects` appended. CLI and config overrides name the exact session root. This difference is intentional and must be documented in the help text.

Rules:

- Discover only known session roots and explicit overrides. Never scan the home directory broadly.
- Codex files match `rollout-*.jsonl` under the date hierarchy; Claude files match `*/<session-id>.jsonl` under project directories.
- Skip symlinks. Do not follow them during discovery.
- Cap sessions per provider (`max_sessions`, default 2,000), applied after discovery. Selection is deterministic by source recency: file modified time for JSONL, `session_v2.time_updated` for OpenCode, because per-session persisted time is not known until after the cap. After summaries load, the roster sorts by `last_persisted_at`. Warn with the number omitted. A cap applies to sessions, not raw files; duplicate IDs are still separate sources.
- Exclude credential files and unrelated provider data by construction: no `auth.json`, `.credentials.json`, `account`, `credential`, `control_account`, or `session_share` access.
- A missing root is a warning, not a fatal error.
- New files discovered on refresh are added; removed files are removed from the roster with a warning. A file that becomes unreadable is retained as “source unavailable” for one refresh so the user can see why.

### 7.2 Codex adapter

Mapping:

| Source | Domain |
|---|---|
| `session_meta` | session ID (`id`/`session_id`), cwd, source/originator, provider version, parent thread, title hint |
| `turn_context` | model, cwd, collaboration mode; latest model wins; distinct models are listed |
| `response_item` message role `user` | user prompt event; role `developer` becomes a collapsed system/developer event |
| `response_item` message role `assistant` | assistant text event |
| `response_item` reasoning | reasoning event; `summary_text` is the preview, `raw_content` only through the source view |
| `event_msg.item_completed` | tool interaction: `CommandExecution`, `FileChange`, `Extension`, `McpToolCall`; includes status, timestamps, exit code, output, changes |
| `event_msg.task_started` / `task_complete` | turn lifecycle evidence and durations |
| `event_msg.token_count` | usage fallback only |
| `token_usage_record` | authoritative cumulative usage |
| `compacted` | system/compaction event |
| `world_state`, `additional_tools`, `inter_agent_communication_metadata` | hidden from the transcript with an unknown/hidden-record count |

Tool mapping details:

- `CommandExecution`: command, cwd, aggregated output, exit code, status, duration. A nonzero exit code is a tool error, not a session failure.
- `FileChange`: changed paths, stdout/stderr, status.
- `Extension` and `McpToolCall`: kind/query/action/results or server/tool/arguments/result.
- Tool calls without a completion are shown as `Unknown`/pending. Completions without a call are orphan results with a warning.
- `custom_tool_call` and `custom_tool_call_output` are not shown as duplicate rows; their raw record remains addressable for diagnostics.

Usage:

- `thread_token_usage` is cumulative for the thread; take the latest snapshot, never the sum.
- `turn_token_usage` is cumulative for the turn; `usage` is per response. Show them only in usage detail.
- `input_tokens` is treated as including `cached_input_tokens` and `cache_write_input_tokens`; `total_tokens = input_tokens + output_tokens`. Cached inclusion is observed; cache-write inclusion is unverified because the local data has zero cache writes and is a validation task. `reasoning_output_tokens` is a subset of output. Do not add cached or reasoning tokens to the total.
- `token_count.info.total_token_usage` and its `last_token_usage` deltas duplicate the usage record; use them only if `token_usage_record` is absent.
- `model_context_window` is displayed as a reported window size, not as a utilization percentage.

Lifecycle:

- `task_started` and `task_complete` are turn-level evidence. A session with a completed last turn can be labeled `Complete` only with the evidence text “last turn completed”; the session may still be resumed.
- Codex does not persist a session-level failure in observed records. Do not label a session failed from a tool exit code alone.
- `task_complete.last_agent_message` is already represented by assistant events and is not duplicated.

Streaming and edge cases:

- Read records incrementally from a byte offset; hold a trailing partial line until the newline arrives.
- File identity uses the strongest portable tuple: Unix `(dev, ino)`, Windows `(creation_time, length, last_write_time)`. A length shrink or identity/mtime change resets the cursor and emits a rotation warning. A same-length, same-mtime replacement is undetectable on Windows; this is a documented false negative and a test case.
- Source-only records (suppressed `custom_tool_call`/output, unknown records) keep bounded `SourceLocator` entries so the diagnostics view can reload them without rendering duplicate tool rows. The locator list is capped per session; overflow increments the hidden count.
- Malformed records, oversized records, and unknown record types are counted per session; they do not stop the file.
- A missing `session_meta` falls back to the file stem and emits a warning.
- Duplicate session IDs across files are not merged; each file is a separate source with a `Duplicate ID` warning. This keeps cumulative usage unambiguous.

### 7.3 Claude Code adapter

Mapping:

| Source | Domain |
|---|---|
| `user` message with text | user prompt event |
| `user` message with `tool_result` content | tool result; link by `tool_use_id`; keep `is_error` |
| `assistant` message | assistant text and tool calls; `message.model` |
| content `thinking` | reasoning event, collapsed by default; provider-persisted text only |
| content `image` | `[image]` placeholder; do not decode |
| `ai-title` | preferred title |
| `system` record | system event; subtypes such as `turn_duration`, `stop_hook_summary`, `away_summary`, `local_command` |
| `attachment`, `file-history-*`, `api-request*`, `cost-state`, `bridge-session`, `queue-operation`, `pr-link`, `mode`, `permission-mode` | hidden from the transcript; counted as hidden/auxiliary |

Tool mapping:

- `tool_use` has `id`, `name`, `input`, and optional `caller`.
- `tool_result` has `tool_use_id`, `content`, and `is_error`. Link to the call. Do not render the result as a second user prompt.
- Orphan results and calls without results are labeled.

Usage:

- Use the top-level `message.usage`: `input_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens`, `output_tokens`.
- `cache_creation` splits `ephemeral_5m_input_tokens` and `ephemeral_1h_input_tokens`; use those for cost when present.
- `output_tokens_details.thinking_tokens` is a subset of output; do not add it.
- `iterations` is a breakdown of the same request; do not add it to the top-level usage.
- Deduplicate usage by `message.id`; preserve distinct content blocks by record `uuid` and block identity; replace only a demonstrated revision of the same block. Keep one usage observation and one event per distinct block.
- Session usage is a derived sum of unique assistant-message observations. Mark it `Derived` and `Partial` if any assistant message lacks usage.
- Exact usage deduplication uses a per-session ledger of message IDs, separate from the transcript window and bounded by entry and byte budgets. If the ledger overflows, freeze the verified aggregate and mark it `Partial`; do not silently count later duplicates. Per-message usage remains visible.
- `server_tool_use` counts web searches/fetches; show them as reported counts and note that token-based cost estimates exclude server-tool fees.

Lifecycle:

- `stop_reason` (`end_turn`, `tool_use`, `max_tokens`, other) is turn-level evidence. `end_turn` can support “last turn complete”; there is no observed session-level terminal event.
- `system` records and `turn_duration` are evidence, not a session completion claim.
- Do not label a session failed from a tool `is_error` alone.

Version tolerance and file identity:

- Ignore unknown record types with a count; never fail the whole provider.
- Use `sessionId` from records as the key; fall back to the file stem. If multiple files share a session ID, do not merge them; each file is a separate source with a `Duplicate ID` warning. Duplicate `message.id` records deduplicate usage once but preserve distinct content blocks; only same-block revisions replace content.
- `isSidechain` records are labeled and kept separate from the parent; a sidechain must not change the parent’s totals.
- A missing timestamp is `Unknown`; ordering falls back to file order.

### 7.4 OpenCode adapter

Read-only access:

- Open with explicit read-only flags (`SQLITE_OPEN_READONLY`), set `PRAGMA query_only = ON`, and set a bounded `busy_timeout` (default 2 seconds).
- Register a SQLite authorizer that allows only `SELECT`/read operations on the allowlisted tables and columns; deny everything else at the connection boundary. This is enforcement, not just a test.
- Never use `immutable=1` for the live database.
- **Chosen policy (ADR-0002):** open the database read-only and allow SQLite to create/update only its transient `-shm`/`-wal` sidecars. AgentScope never writes session content, the main database, provider config, permissions, or credentials, and never manually deletes, repairs, or copies provider files. If the directory is not writable and the live WAL cannot be read without creating a sidecar, report `Unavailable` and retry; do not chmod, repair, or copy. The sidecar exception is surfaced in the docs and tests.
- Query only an allowlist of tables and columns: `session_v2`, `session_message`, `project`, and legacy `session`/`message`/`part` when v2 is absent. Never query `account`, `control_account`, `credential`, `session_share`, `kv`, or auth-related tables. Never select `share_url`.
- Keep the connection in the worker. On a lock or corruption error, back off, report a warning, and reopen on the next refresh.

Schema selection:

- If `session_v2` and `session_message` exist and `session_message` has rows, use the v2 generation exclusively. Observed data has complete v2 coverage for all sessions; legacy tables are not needed.
- If v2 tables are absent, fall back to `session` + `message` + `part`. If v2 exists but is empty and legacy has rows, use legacy and warn.
- If neither shape is recognized, list the database as an unsupported source with a warning; do not guess.

Roster mapping (`session_v2`):

- ID, title, directory, project ID, parent/fork IDs, model JSON (`id`, `providerID`, `variant`), timestamps (epoch milliseconds), archived time, idle outcome, session token totals, cost, version.
- `last_persisted_at` is `session_v2.time_updated` (or the maximum `session_message.time_updated` when the session row is stale). `last_event_at` uses the newest displayed work message. `session_message` rows are updated in place: 33,606 of 33,610 observed rows had `time_updated > time_created`. Refresh must therefore query changed rows by `time_updated`, not only new IDs, and invalidate cached page content for a changed row. New-event counting uses `time_created`, not `time_updated`.
- Project name comes from `project.name`, falling back to the directory basename.
- Model display uses the model ID and provider; variant is secondary.
- Parent and fork links are explicit; do not infer.
- `time_archived` sessions are included with an `Archived` marker; the default sort keeps recent activity first.

Transcript mapping (`session_message`):

- Types: `user`, `assistant`, `system`, `compaction`, `idle`, `synthetic`.
- User data has `text`, optional `files`, `agents`, and `time`.
- Assistant data has `content` parts (`text`, `reasoning`, `tool`, `patch`, `file`, `agent`, `subtask`), `model`, `tokens`, `cost`, `finish`, `rawFinish`, `time`.
- Tool part has `id`, `name`, `state` (`input`, `content`, `metadata`, `status`), and `time` (`created`, `completed`, `ran`). `state.status` is `completed` or `error` in observed data; other values are shown as unknown.
- `compaction` and `idle` are system/lifecycle rows. An idle row carries an outcome such as `succeeded`.
- `patch`/`file`/`agent`/`subtask` parts are shown as structured system/tool rows with previews.
- Paginate by `(time_created, id)` using `session_message_session_time_created_id_idx`; `seq` is not used for display ordering. Do not use large `OFFSET` scans.

Usage:

- `session_v2` totals are authoritative: `tokens_input`, `tokens_output`, `tokens_reasoning`, `tokens_cache_read`, `tokens_cache_write`, `cost`.
- Categories are additive in observed data; message sums can lag session totals while a turn is in flight. Do not replace session totals with a message sum.
- `cost` is provider-reported. A reported `0.0` is valid (for example, a free model) and must display as reported, not “Unknown.”
- If session totals are null/absent, derive from unique messages and label `Derived`; never multiply or double count.

Lifecycle:

- `finish` values observed: `stop`, `tool-calls`, `unknown`, `length`, plus null. `tool-calls` means the turn continued; `stop` supports “last turn complete.”
- `idle` outcome `succeeded` is explicit evidence; display `Idle — succeeded` with the source. It does not prove the session can never be resumed.
- `idle_outcome` is null for most observed sessions; do not synthesize an outcome.
- Parent/fork relationships are explicit; do not roll up usage.

### 7.5 Common adapter rules

- A malformed, oversized, unsupported, or unreadable record is counted and reported; it never halts other sessions.
- Unknown record types are ignored for display but counted; the raw source remains addressable.
- Missing timestamps stay missing. Sorting uses the provider’s source order as a tiebreaker.
- Never synthesize assistant text, tool output, completion, or parentage.
- Preserve the source path/locator, record type, and provider version in provenance.
- Adapter tests use synthetic fixtures shaped like the observed versions; real transcripts are never committed.

## 8. Usage and cost

### 8.1 Aggregation classes

| Class | Meaning |
|---|---|
| `Reported` | Emitted by the provider/client in a persisted record and used as-is |
| `Derived` | Computed from reported records using a documented provider rule |
| `Estimated` | Usage multiplied by a user-supplied price schedule |
| `Partial` | Derived from some, but not all, expected records |
| `Unknown` | No reliable observation; never coerced to zero |

### 8.2 Session totals

| Provider | Total source | Notes |
|---|---|---|
| Codex | Latest `token_usage_record.thread_token_usage.total_tokens` | Cumulative; do not sum snapshots. Cached input and reasoning output are subsets. |
| Claude | Sum of unique assistant-message usages: `input + cache_creation + cache_read + output` | Derived; cache categories are disjoint from `input_tokens`. `thinking_tokens` is a subset of output. |
| OpenCode | `session_v2` token columns | Reported by the application; categories are additive. Message sums are detail, not the session total. |

Token displays show the reported total where one exists, otherwise the derived sum, with a provenance marker. The inspector always shows the category breakdown. Missing categories are `Unknown`.

### 8.3 Cost

Cost is estimated only from a user-supplied, versioned price schedule. There are no bundled prices in v1; prices change and a stale default would be misleading.

Formulas:

- **Codex:** `uncached_input = max(input - cached - cache_write, 0)`; cost = uncached input × input price + cached input × cached price + cache write × write price + output × output price. Reasoning is included in output.
- **Claude:** cost = input × input price + 5m cache creation × 5m write price + 1h cache creation × 1h write price + cache read × read price + output × output price. If the TTL split is absent, a single cache-write price may be used. `thinking_tokens` is included in output. Server-tool fees are excluded and labeled.
- **OpenCode:** prefer the provider-reported `cost`. If absent, estimate with configured prices; if a required category price (including reasoning when nonzero) is missing, show `Unknown` rather than a partial number.

Rules:

- Missing prices produce `Unknown`, never `$0.00`.
- A reported cost of `0` is shown as `$0.00 reported`.
- Estimated costs are labeled `Estimated` and display the schedule’s `as_of` date.
- Subscription usage is not API spend; never infer a subscription charge.
- Currency is USD in v1; a non-USD configuration is rejected with a clear message.
- Display rounded cents for readability; retain exact values for sorting and details.

### 8.4 Context

Display a context-window size only when the provider reports one (Codex `model_context_window`). Do not display a utilization percentage unless the current-window usage and window size are both defensible. Claude and OpenCode show `Context: Unknown` in v1 rather than guessing from lifetime totals.

## 9. Session state, activity, and process observation

### 9.1 Separate axes

- **Lifecycle:** `Unknown`, `Ongoing`, `Completed`, `Failed`, `Cancelled` — only from explicit persisted evidence.
- **Process observation:** `ObservedRunning`, `ObservedExited`, `NotFound`, `NotChecked`, `Unavailable`.
- **Activity:** `Recent`, `Quiet`, `PossiblyStalled` — heuristic, never a terminal outcome.

### 9.2 Evidence by provider

| Provider | Lifecycle evidence | Activity evidence | Usage evidence |
|---|---|---|---|
| Codex | `task_started`, `task_complete`; item status/exit code is tool-level | `last_persisted_at` (any complete record); file append is a hint, not proof of process liveness | `token_usage_record` cumulative |
| Claude | `stop_reason`; system `turn_duration`; no session terminal event | `last_persisted_at` (any complete record, including hidden API/bookkeeping records) | per-assistant message usage |
| OpenCode | `finish`, `idle` outcome, `idle_outcome`; archived state | `last_persisted_at` = `session_v2.time_updated` (or max message `time_updated`) | session totals + reported cost |

### 9.3 Status synthesis

The roster shows one synthesized label plus a short evidence line in the inspector. Before applying the rules, compute the latest explicit turn completion and whether any unfinished work is newer than it: a later user prompt, turn start, tool call without a result, or `stop_reason`/`finish` indicating tool use. A matched process with a completed latest turn is idle, not stalled; a newer turn start or user prompt reopens the session.

1. **Source unavailable:** `Unavailable` with the read error; never infer a state.
2. **Explicit failure/cancel:** `Failed`/`Cancelled` only when an explicit terminal failure or cancellation is persisted. A tool error or nonzero command exit is shown on the tool row, not as session failure.
3. **Process matched, unfinished work:** show `Active` when `last_persisted_at` is recent, `Quiet` when it exceeds the quiet threshold, and `Possibly stalled` only after the stall threshold and only with the caveat that a long-running tool may explain silence.
4. **Process matched, latest turn completed:** show `Complete` with evidence `last turn completed; process observed`. Do not label it stalled; the process may be waiting for another prompt.
5. **Explicit last-turn completion and quiet (no matched process):** whether process observation is unmatched, disabled, or unavailable, show `Complete` with evidence `last turn completed` and the process state, but only after the quiet threshold. A completion that is still recent is not a permanent session label; a resumed session must be able to become recent again.
6. **No process match and recent persisted activity:** `Recent (process unmatched)` — not `Active`. Include last-turn evidence in the inspector when present.
7. **No process match and quiet:** `Quiet (process unmatched)`; if completion evidence exists, add it. Otherwise `Unknown` when no activity evidence is available.
8. **No useful evidence:** `Unknown` plus whatever lifecycle and process evidence is available.
9. Never display `Crashed` without an explicit captured failure or exit code. Codex tool exit codes are not agent exit codes.

Default thresholds: quiet after 5 minutes, possible stall after 15 minutes. Both are configurable. All three providers are treated as writers during work for stall purposes, but a long-running tool may be silent; `Possibly stalled` requires a matched process and the evidence line says why the label is uncertain. If process matching is unavailable on the host, `Possibly stalled` is not reachable and the UI says so.

### 9.4 Process observer

- Sample the OS process list with `sysinfo` every 5 seconds in the worker.
- Candidate detection: executable basename or command-line token matches `codex`, `claude`, or `opencode` (including the OpenCode node wrapper). Candidates are not matches.
- Strong match requires the session ID in an identity-bearing argument position, not anywhere in the command line:
  - Codex: `codex resume <uuid>` and `codex exec resume <uuid>`. Do not match `queue`, `archive`, `delete`, `fork`, or prompt text.
  - Claude: `-r/--resume <uuid>` or `--resume=<uuid>`. Do not match IDs inside `-p`/`--print` prompt text or parent/fork references.
  - OpenCode: `-s/--session <id>` or `--session=<id>`. Do not match `--continue` or prompt text.
- No observed provider record includes an agent PID, so v1 has no recorded-PID branch; add one only when a provider mapping supplies both a PID and a start time.
- Do not match by executable name, working directory, or timing alone; those are hints shown with candidates. IDs appearing in prompts or reference arguments are negative tests, not matches.
- PID reuse is not used for matching in v1. Candidate rows still show process start time so the user can distinguish same-named processes.
- The observer reads only OS-exposed process metadata: PID, start time, executable, cwd when the OS provides it, and the command line used for identity matching. It does not read environment variables, heaps, or arbitrary process memory. On Windows the OS command-line API may internally access the PEB; only the command-line field is used and it is never logged raw. `cwd` is generally unavailable on Windows and shows `Unknown`.
- On permission failure, mark `Unavailable`; never show a false `NotFound`.
- The inspector shows candidates: PID, executable, start time, cwd if available, and a redacted/truncated command hint. Other users’ command lines are not displayed.
- On this machine, the expected result is `unmatched` with several candidates. The roster must make that obvious rather than implying activity.

## 10. TUI

### 10.1 Screens

`Roster → Inspector → Details / Source`. Secondary overlays: Filter, Help, Process candidates, Diagnostics.

### 10.2 Roster

```text
AgentScope  LOCAL  Codex 8  Claude 12  OpenCode 1001  Live: on
Filters: none   Sort: last activity
──────────────────────────────────────────────────────────────
STATUS       PROVIDER   SESSION / PROJECT             LAST  TOKENS  PROC
● Active     Codex      Inventory API · RipperGame     12s   91k     matched
! Quiet      Claude     Scanner refactor · RipperGame   9m   73k     unmatched
? Unknown    OpenCode   PR review · CortexSandbox       2h   41k     2 cand.
✓ Complete   Codex      Localization · RipperGame       1d   28k     unmatched
──────────────────────────────────────────────────────────────
Enter inspect  / search  f filter  s sort  r refresh  ? help  q quit
```

- Columns collapse on narrow terminals: drop `PROC`, then `TOKENS`; keep status, provider, session/project, last activity.
- Header counts show discovered sessions by provider and synthesized state. “Live: on” means the refresh loop is running; process observation status is separate.
- Status always has an icon and a text label; color is never the only signal.
- Selecting a row uses a stable session key, so live updates do not move the selection.
- Empty state lists the roots checked and warnings; it does not tell the user to “start an agent.”

### 10.3 Inspector and transcript

```text
< Back  CODEX / Inventory API
Project: RipperGame   Model: gpt-5.1-codex   Session: …
Status: ! Quiet — process unmatched; no persisted events for 9m
Tokens: 73,214 reported · Cost: ~$0.23 estimated · Context: 475k window
Process: unmatched — 2 candidates
──────────────────────────────────────────────────────────────
12:14  user       Implement inventory checks and run tests
12:15  assistant  I will inspect the current implementation.
12:15  tool       Read InventoryComponent.cpp            [expand]
12:17  tool       Edit InventoryService.cpp             [expand]
12:18  tool       cargo test                            [expand]
12:18  tool       Exit 1; 2 failed assertions          [expand]
12:20  assistant  Investigating the validation path…
```

- Timeline rows show a preview. `Enter` opens Details; `Esc` closes.
- Tool rows combine call and result, with status, duration, exit code, and a truncation indicator. Full arguments/output load on demand, up to the record cap.
- Reasoning is collapsed by default and labeled “Provider reasoning (persisted)”; it is not presented as hidden chain-of-thought.
- Images and binary attachments are placeholders; they are not decoded or rendered.
- Missing timestamps show `—`; missing usage shows `Unknown`.
- The inspector opens at the oldest page; `G`/`End` loads the newest page from the tail. `g`/`Home` returns to the oldest loaded page. Page requests carry the anchor and direction so scrolling never duplicates or skips events. Pages are bounded by both event count and byte size (`max_page_events`, `max_page_bytes`); an oversized row is truncated with an explicit marker rather than allocated whole.
- Auto-load the next (newer) page when the selection approaches the end of loaded events. Show “Loading older/newer…” without blocking input. Keep at most 20,000 loaded events and drop far pages; the details cache is a small LRU. A tool call and its result are never split across a page boundary.
- New events while the inspector is open are counted (`3 new`) from `count_events_after` or the summary counter; do not auto-scroll. `G`/`End` jumps to the newest page.
- The Diagnostics overlay lists hidden/unknown/source-only records with their `SourceLocator`, counts, and warnings. Selecting an entry loads the redacted source record. This is the route to suppressed Codex call records and unknown records that have no displayed event.
- `x` toggles redaction for the selected content with a visible warning; `v` opens the redacted source record. Raw content is reloaded from the source, not retained in memory.

### 10.4 Search, filter, sort

- `/` searches titles, projects, cwd, and session IDs in the roster; inside the inspector it searches loaded transcript events. Search is case-insensitive.
- `n`/`N` cycle transcript matches; the status line shows `match 3/12 (loaded events)`.
- `f` opens a filter panel for provider, synthesized status, project, and usage availability. Filters compose with search.
- `s` cycles sort: last activity, provider, project, tokens, status. The header shows the active sort and direction (`s` again reverses).
- Filtering preserves the selected session if it still matches; otherwise select the nearest row.

### 10.5 Keyboard

| Key | Action |
|---|---|
| `j`/`k`, arrows | Move selection |
| `g`/`G`, Home/End | First/last row or loaded event |
| PageUp/PageDown, `Ctrl+u`/`Ctrl+d` | Page |
| `Enter` | Open inspector, expand event, or apply filter |
| `Esc`/`Backspace` | Back/close overlay |
| `Tab`/`Shift+Tab` | Switch focus between roster, transcript, details |
| `/` | Search |
| `f` | Filter |
| `s` | Sort |
| `r` | Refresh now |
| `p` | Process candidates |
| `D` | Diagnostics: hidden, unknown, and source-only records |
| `v` | Source record for the selection |
| `x` | Toggle redaction for the selected content |
| `?` | Contextual help |
| `q` | Quit when not in a text input |
| `Ctrl+c` | Quit |

`q` is a normal character inside search/filter input. A terminal guard restores raw mode, cursor, and the alternate screen on normal exit, `Ctrl+c`, and panic.

### 10.6 Accessibility and layout

- Respect `NO_COLOR` and `--no-color`; use text labels and simple glyphs.
- Do not assume a background color or a specific terminal font.
- Keep a minimum usable layout down to 60×20; below that, show a readable “terminal too small” message.
- Sanitize all untrusted text before measuring or rendering. Truncate with a visible marker and show the full value in Details.
- No mouse is required. Mouse capture is off.

### 10.7 Degraded and empty states

- A provider warning appears in a top banner with the provider name and a safe reason; data from other providers remains.
- Scan progress shows provider and counts, not full paths.
- If no sessions are found, show the checked roots and the last refresh time.
- If the worker stops, show “Live: paused” and the error; `r` retries.
- If the terminal is not a TTY, print a concise error and suggest `agentscope inspect <file>`.

## 11. Privacy and read-only safety

### 11.1 Access boundary

- Open provider files read-only. Do not create, modify, rename, delete, or chmod provider files, except for SQLite's transient WAL sidecars as documented in ADR-0002.
- Open SQLite read-only with `query_only`. Under ADR-0002, SQLite may create/update only transient `-shm`/`-wal` sidecars; AgentScope never writes session content, the main database, config, permissions, or credentials, and never manually deletes, repairs, or copies provider files. If a live WAL cannot be read without writing and the directory is not writable, warn and retry later.
- Never read `auth.json`, `.credentials.json`, OpenCode credential/account tables, or session share secrets.
- Never launch, signal, suspend, resume, or control an agent process.
- Never read process environments.
- Never log raw prompts, tool output, command lines, or file contents. Startup errors and warnings may include provider/source paths and offsets only.

### 11.2 Display sanitization

Apply to every rendered string, including titles, paths, model names, error messages, process hints, and raw source:

- Strip ANSI CSI/OSC/ESC sequences and other C0/C1 controls; expand tabs; normalize CRLF.
- Neutralize bidi control characters and other invisible directional formatting.
- Replace invalid UTF-8 decode results with a visible replacement marker.
- Bound each preview; expose lengths and truncation.
- Previews stored in `Event`/`BoundedText` are already control-sanitized and redacted by default. Raw text is not retained in the transcript model; `x`/`v` reload the source record on demand. Search operates on the stored display-safe preview. This keeps secrets out of long-lived memory and makes the redaction toggle force a source reload rather than flipping a flag over retained raw text.

### 11.3 Redaction

- Best-effort masking of common credential shapes: `sk-`, `sk-ant-`, `ghp_`, `github_pat_`, `xoxb-`, `AKIA`, bearer tokens, password/token assignments, and private-key blocks.
- Redaction is on by default for titles, transcript text, tool arguments/outputs, source records, and process command hints.
- `x` reveals the selected content after a visible warning. It does not persist and does not disable terminal sanitization.
- Redaction is not a guarantee; the UI and docs say so.

### 11.4 Filesystem and database traversal

- Discover only known roots plus explicit overrides.
- Skip symlinks and cap depth/count.
- Do not read files outside a session root through a session locator.
- For OpenCode, query only allowlisted tables/columns and only the selected session IDs.
- Reject source locators that do not match the session’s recorded path/table.

## 12. Configuration

### 12.1 Precedence and path

CLI flags > environment (`CODEX_HOME`, `CLAUDE_CONFIG_DIR`, `XDG_DATA_HOME`) > config file > defaults.

Default config path:

- Windows: `%APPDATA%\agentscope\config.toml`
- macOS: `~/Library/Application Support/agentscope/config.toml`
- Linux: `$XDG_CONFIG_HOME/agentscope/config.toml` or `~/.config/agentscope/config.toml`

A missing config file is fine. A malformed or non-USD config is a fatal startup error with a clear message; do not silently fall back.

### 12.2 Example

```toml
currency = "USD"
refresh_ms = 2000
process_ms = 5000
process_enabled = true
quiet_after_secs = 300
stall_after_secs = 900
max_sessions = 2000
max_page_events = 500
max_page_bytes = 262144
dedup_ledger_entries = 100000
dedup_ledger_bytes = 8388608
max_record_bytes = 1048576
refresh_busy_timeout_ms = 2000
page_busy_timeout_ms = 250
redaction = true

[providers.codex]
root = "C:\\Users\\me\\.codex\\sessions"

[providers.claude]
root = "C:\\Users\\me\\.claude\\projects"

[providers.opencode]
db = "C:\\Users\\me\\.local\\share\\opencode\\opencode.db"

[[pricing]]
provider = "codex"
model = "gpt-5.1-codex"
as_of = "2026-10-01"
input_per_mtok = 1.25
cached_input_per_mtok = 0.125
cache_write_per_mtok = 1.25
output_per_mtok = 10.0

[[pricing]]
provider = "claude"
model = "claude-sonnet-5-5"
as_of = "2026-10-01"
input_per_mtok = 3.0
cache_write_5m_per_mtok = 3.75
cache_write_1h_per_mtok = 6.0
cache_read_per_mtok = 0.30
output_per_mtok = 15.0
```

Pricing is exact-match by provider and model; no wildcards. An unrecognized model has `Unknown` cost. Optional OpenCode pricing may include `reasoning_per_mtok`; if reasoning tokens are nonzero and the price is absent, cost is `Unknown`.

### 12.3 CLI

```text
agentscope [--config PATH] [--codex-sessions PATH] [--claude-projects PATH]
           [--opencode-db PATH] [--no-processes] [--no-color] [--version]
agentscope inspect <codex.jsonl>
```

`--codex-sessions` and `--claude-projects` name exact session roots. The `CODEX_HOME` and `CLAUDE_CONFIG_DIR` environment variables name provider homes, so `sessions`/`projects` are appended. `inspect` remains a diagnostic for a single Codex file, backed by the shared bounded reader and sanitizer. It is not the product UI and is not expanded to a second display model. `--no-processes` disables sampling and reports `NotChecked`.

## 13. Performance and degradation

### 13.1 Targets

| Area | Target |
|---|---|
| Initial roster | Usable within 3 seconds on a moderate local dataset; scan progress visible |
| Keypress response | Visible response under 100 ms while scanning; no disk I/O on the UI thread |
| Transcript first page | Under 500 ms for a typical session when the database is not locked; OpenCode queries use indexes and a 250 ms page `busy_timeout` |
| Incremental refresh | Only changed files/rows; no full reparse of unchanged sessions; OpenCode changed rows are found by `time_updated` |
| Memory | Bounded roster, one transcript window, small source LRU, and a bounded per-session usage-dedup ledger; no full transcript retention |
| Robustness | Corrupt/truncated JSONL, locked SQLite, malformed records, and missing roots do not crash the TUI |

### 13.2 Strategy

- Phase 1 refresh reads metadata: Codex session header/tail, Claude header/title and last timestamp, OpenCode summary columns.
- Phase 2 fills usage in the worker; the roster shows `…`/`Unknown` until available.
- Skip unchanged files by length/modified time. Keep per-file cursors and per-session summaries.
- Read only the required SQLite columns; use `session_message_session_time_created_id_idx` for pages. No `SELECT *`, no large offsets. Query changed rows by `time_updated` and invalidate cached page content for updated messages.
- Cap records at 1 MiB, pages at 500 events **or 256 KiB**, roster at 2,000 sessions, and loaded transcript at 20,000 events. The page byte cap applies before allocation; an oversized row is truncated with a marker. Tool call/result pairs count as one row and are never split.
- The Claude usage-dedup ledger is separate from the transcript cache: default 100,000 entries or 8 MiB per session. On overflow, freeze the verified aggregate and mark it `Partial`; do not silently count later duplicates. Per-message usage remains visible.
- Refresh passes are chunked; page and source requests are serviced between chunks. If a page query hits the 250 ms page timeout, return a retryable error and keep the current page; the UI remains responsive.

### 13.3 Degradation rules

- Oversized record: skip with a count and source offset.
- Partial final line: hold and retry; report incomplete at EOF.
- File rotation/truncation: reset and warn.
- Unknown provider schema: show the source as unsupported; do not guess.
- SQLite busy/locked: warn, retain last data, retry.
- Missing `-shm`: follow ADR-0002. SQLite may create it if the directory is writable; if not, warn and show `Unavailable`. Never create, repair, chmod, or copy provider files manually.
- Process permission denied: `Unavailable`, not `NotFound`.
- No pricing: `Unknown`, not zero.

## 14. Testing and acceptance

### 14.1 Test layers

- **Unit:** sanitization/redaction, bounded text, timestamp parsing, cursor/partial-line handling, usage formulas, status synthesis, process matching, filter/sort.
- **Paging:** forward and backward traversals over the same fixture must produce the same event sequence with no duplicate or skipped events; boundaries at equal timestamps, missing timestamps, and call/result pairs split across source records; no spurious orphan warnings. Page byte budget truncates oversized rows with a marker.
- **Status:** a truth table over all §9.3 rules × lifecycle × process × activity with an injected clock; “Complete only after quiet,” “completed-but-running is not stalled,” “resumed session becomes recent,” and “process disabled still allows completion evidence.”
- **Adapter integration:** synthetic fixtures for each provider, including malformed/unknown/oversized records, duplicate usage, missing timestamps, parent/child links, and schema variants.
- **Claude content blocks:** text, thinking, and tool-use blocks sharing a `message.id` all survive; usage is counted once; a same-block revision replaces content; duplicate usage after transcript eviction is still deduplicated or marked `Partial` on ledger overflow.
- **SQLite:** temporary fixtures with v2 and legacy schemas; read-only open; WAL handling; no provider content writes; page cursors. Include a live-WAL fixture with an existing `-shm` (read succeeds) and a fixture where sidecars are required but the directory is not writable (provider shows `Unavailable`, no chmod/copy). Add a v2 fixture where a `session_message` row is updated in place; `last_persisted_at` and page invalidation must advance. Add a sensitive-table fixture (`account`, `credential`, `session_share`) that is structurally unreadable while session queries still work, proving the authorizer/allowlist.
- **TUI:** `ratatui::TestBackend` at 120×40, 80×24, and 60×20; roster/inspector/help/filter flows; search and truncation; no-color mode; warning replacement identity (one banner with count 3, not three banners); roster-delta generation ordering, removal of the selected session, and selection preservation.
- **Read-only regression:** hash/mtime/size of fixture files and the main database before/after; no session content, main database, config, permission, or credential writes. Under ADR-0002, only transient SQLite `-shm`/`-wal` sidecars may be created or updated; assert that no other files are created or deleted and that AgentScope never manually repairs or copies provider files. Add an authorizer test that denies credential tables and a discovery fixture containing `auth.json`/`.credentials.json` that is never opened.
- **Process:** pure matching tests for explicit identity-bearing arguments, IDs in prompt text (must not match), ambiguity, unavailable command lines, candidates, and no false match from executable name or working directory. Add one positive end-to-end match using a spawned test binary whose argv contains a synthetic session ID, and Active → Quiet → PossiblyStalled transitions on the injected clock.
- **Content-free diagnostics:** a sentinel secret in every fixture must not appear in `Debug` output of domain types, warnings, or errors.
- **Live:** append a complete record to a temp file; verify it appears after refresh; append a partial line and verify it is held; truncate/rotate and verify reset. Include the same-length, same-mtime replacement false negative.
- **Config:** precedence matrix (CLI > env > config > default), env-home vs CLI-exact-root asymmetry, malformed/non-USD fatal, invalid threshold ordering, duplicate pricing keys, exact price lookup.
- **Usage edge cases:** non-monotonic Codex `token_usage_record` (latest wins even when it decreases); `thread_token_usage` absent → `token_count` fallback; both present → no addition; Claude ledger overflow; OpenCode session totals vs message sums.
- **Local time:** formatter takes an explicit offset; DST/offset cases produce stable output independent of CI timezone.
- **Manual compatibility:** run against real local sessions, without committing them, and report provider versions. Use redacted/synthetic fixtures for any regression that must persist.

### 14.2 Golden usage vectors

- Codex: multiple cumulative snapshots produce the final total, not the sum; per-turn/response values remain detail; `token_count` does not double count.
- Claude: top-level usage is used once; `iterations` is not added; duplicate `message.id` records are deduplicated for usage while distinct content blocks survive; cache categories are disjoint; thinking tokens are not added.
- OpenCode: session totals are used; message sums do not replace them; reported cost `0.0` stays reported; missing totals derive only from unique messages.
- Costs: missing price → `Unknown`; missing category → `Unknown`; configured prices produce expected values; server-tool fees are excluded and labeled.

### 14.3 Acceptance mapping

| Scenario | v1 evidence |
|---|---|
| AC-01 No-disruption start | Controlled fixture plus live run: AgentScope issues no writes, renames, deletes, chmods, or process signals; a live provider may legitimately append records while it runs. Verify AgentScope’s operations, not an unchanged provider. |
| AC-02 Drill-down | Open a session, read ordered transcript, expand a tool result |
| AC-03 Multi-provider | Real roster contains Codex, Claude, and OpenCode sessions |
| AC-04 Live append | Temp/real complete record appears without restart |
| AC-05 Partial/invalid | Trailing partial line retried; malformed records reported; other sessions remain |
| AC-06 Accurate usage | Golden vectors above |
| AC-07 Missing data | Unknown cost/state/process shown as Unknown/Unmatched |
| AC-08 Read-only regression | Session/main-database/config/permission/credential hashes unchanged; only transient SQLite sidecars may change; authorizer and no-manual-repair tests |
| AC-09 Privacy | Control sequences neutralized; synthetic secrets masked by default |
| AC-10 Keyboard UX | Discover → open → expand → search → back, keyboard only |

### 14.4 Commands and CI

```bash
cargo fmt --all -- --check
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

CI runs the first four on `ubuntu-latest`, `windows-latest`, and `macos-latest`. The Windows/macOS jobs are required for path, terminal, and process behavior; Linux remains best effort for runtime support. Release builds are validated locally on Windows and macOS before claiming support.

## 15. Implementation sequence

1. **Foundation and probes:** dependency/toolchain/CI matrix, domain types, config, bounded text/sanitization, JSONL cursor, provider trait, test-fixture boundary. Before freezing the trait, run the read-only format probes in §16 (OpenCode index/in-place updates, WAL behavior, Claude topology, Codex cache-write) and apply any interface changes. Keep `inspect` working.
2. **Codex adapter:** real-shape synthetic fixtures; session summary, transcript, tools, usage, lifecycle, streaming edges.
3. **Claude adapter:** messages, tool blocks, tools, usage/iterations, titles, sidechains, version tolerance, bounded dedup ledger.
4. **OpenCode adapter:** v2 read-only queries, authorizer/allowlist, transcript paging, usage/cost, in-place update detection, idle/lifecycle, legacy fallback.
5. **Usage/cost/process:** normalization, golden vectors, price config, sysinfo observer, identity-argument matching and candidates. This can run parallel with adapters once token categories are frozen in phase 1.
6. **Worker + roster:** threading, chunked refresh, scan progress, live refresh, sorting/filtering, warning identity, empty/error states.
7. **Inspector/details:** transcript paging, tool expansion, source/diagnostics view, redaction toggle, search, help.
8. **Hardening and acceptance:** read-only regression, performance, CI matrix, real-data validation, docs/ADRs.

Each phase ends with its tests and a working binary. Intermediate builds may support only the adapters completed so far; the release gate is all three providers and the full acceptance set. Do not present an intermediate build as v1.

## 16. Decisions and remaining judgment

### Decisions made

- v1 is the full three-provider MVP (option B), not the Codex-only slice.
- One worker thread with channels; no async runtime and no UI-thread I/O.
- OpenCode v2 (`session_v2`/`session_message`) is authoritative; legacy tables are a fallback.
- Codex `item_completed` is the authoritative tool display; raw call records are source-only.
- Session totals use the latest cumulative Codex snapshot, derived Claude message sums, and OpenCode session totals.
- Process correlation requires the session ID in a provider-specific identity-bearing argument position; prompt text and reference arguments do not match.
- A matched process with a completed latest turn is `Complete`, not stalled; only newer unfinished work permits stall classification.
- Claude usage deduplication is by message ID, but distinct content blocks are preserved by record UUID/block identity.
- Exact Claude usage dedup uses a bounded ledger; overflow freezes the aggregate and marks it `Partial`.
- Source-only records remain addressable through a validated `SourceLocator` and the Diagnostics overlay.
- Previews are sanitized and redacted at ingest; raw source is reloaded on demand.
- Warnings have stable keys and are replaced per refresh generation.
- Session caps use source modified time/`time_updated` because per-session time is unknown before the cap.
- Transcript pages are bounded by events and bytes; tool pairs are never split.
- OpenCode changed rows are found by `time_updated`; new-event counting uses `time_created`.
- `--codex-sessions` and `--claude-projects` name exact roots; environment variables name provider homes.
- Costs require a user price schedule; no bundled prices.
- No persistent cache database, no network, no process control, no auth access.
- `inspect` remains a diagnostic; the TUI is the product.
- The roster and quiet/stall heuristics use `last_persisted_at` (any complete record); `last_event_at` remains the displayed-work clock.
- The inspector opens at the oldest transcript page; `G` loads the newest page from the tail.
- Duplicate provider session IDs are source-scoped rows, not merged histories.
- OpenCode transcript order is `(time_created, id)`; `seq` is not the display cursor.
- Usage snapshots are not timeline rows; they are attached to events and summarized in the inspector.

### Decision: OpenCode WAL sidecar exception (ADR-0002)

SQLite documents that a read-only WAL connection may create/update `-shm`/`-wal` when needed, even with `SQLITE_OPEN_READONLY` and `query_only`. The chosen policy is **practical**:

- Allow SQLite to create/update only its transient `-shm`/`-wal` sidecars.
- Never write session content, the OpenCode main database, provider configuration, permissions, or credentials.
- Never manually delete, repair, chmod, or copy provider files.
- Register an SQLite authorizer that denies writes and non-allowlisted tables.
- If the directory is not writable and the live WAL cannot be read without creating a sidecar, show `Unavailable` and retry; do not change permissions or copy the database.
- Document the exception in README and ADR-0002. The strict VFS, refusal, and snapshot options were rejected for v1 because they either add a low-level FFI component, remove live OpenCode coverage, or copy a 3.86 GiB provider database.

This resolves the last user decision.

### Left to the builder

- Visual polish, exact column widths, glyph set, and help wording.
- Redaction pattern list beyond the named minimums, with tests for false positives.
- Exact LRU sizes and page eviction policy within the stated caps.
- Filter panel layout and interaction details beyond the stated keys.
- Error message wording and banner ordering.
- Synthetic fixture contents, as long as they match the observed shapes and contain no real data.

### Validation tasks (not builder choices)

- Confirm Codex cache-write pricing semantics on a session with `cache_write_input_tokens > 0`; adjust the formula or mark cost `Unknown` if the field is not a subset of input.
- Confirm process identity-argument behavior on macOS and Linux; the local Windows processes expose no session ID in argv. The exact flag/position rules are fixed above.
- Confirm Claude content-block/message-ID topology and sidechain/session-file topology with consented, redacted fixtures.
- Confirm OpenCode schema behavior across versions; the local v2 data shows `session_message` rows are updated in place and the `(session_id, time_created, id)` index exists. Keep the v2/legacy detection and warning.
- Measure OpenCode WAL behavior under ADR-0002: a live writer, a stale `-wal` with missing `-shm`, a database with no active WAL, and a non-writable directory. Confirm AC-03/AC-04 and record the observed sidecar changes.
- Confirm Windows file identity semantics and the same-length/same-mtime replacement false negative.
- Build a benchmark corpus and measure the stated targets before treating them as release gates.

## 17. Definition of done

- `cargo fmt --all -- --check`, `cargo check --all-targets`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` pass on Windows, macOS, and Linux CI.
- A release build runs on Windows and macOS.
- The TUI discovers real sessions from all three providers, opens transcripts, expands tools, searches/filters/sorts, and survives malformed data.
- Live append works without restart; partial lines are retried; rotation/truncation is reported.
- Usage golden vectors pass; no cumulative/iteration double counting; missing costs are `Unknown`; reported zero stays reported.
- Process observation shows candidates and never fabricates a match; no process control or environment access.
- Read-only regression passes under ADR-0002: no provider session content, main database, config, permission, or credential writes; only transient SQLite `-shm`/`-wal` sidecars may change; no manual repair, deletion, or copy. No auth or credential tables/files are read.
- Sanitization and redaction tests pass; no raw transcript content is logged.
- README, PRODUCT.md, ARCHITECTURE.md, and relevant ADRs are updated from “proposed” to the verified behavior, with limitations stated honestly.
