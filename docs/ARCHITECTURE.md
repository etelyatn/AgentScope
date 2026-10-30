# AgentScope Architecture

**Status: v1 implemented (2026-10-30). Read-only, privacy, and boundary claims below are covered by `tests/read_only.rs`, `tests/privacy.rs`, and `tests/acceptance.rs` on synthetic fixtures; items marked "future" are not implemented.**

This document records durable architectural boundaries. Update it as components are implemented and their behavior is validated. Requirements and acceptance scenarios live in [PRODUCT.md](PRODUCT.md).

## 1. Local-first architecture

```text
  Codex session files  ──> Codex reader ──────┐
  Claude session files ──> Claude reader ─────┤
  OpenCode SQLite      ──> OpenCode reader ───┼──> Normalized records
  OS process snapshot  ──> Process observer ──┘            │
                                                          v
                                               In-memory app state
                                                          │
                                                          v
                                                Keyboard-first TUI
```

**Core boundary:** AgentScope reads external data and never becomes part of agent execution. Hermes and other orchestrators remain untouched.

## 2. Responsibility boundaries

**Provider readers** discover session stores, read safely, interpret only fields evidenced by installed versions, and preserve source provenance. Provider-specific quirks stay within each reader.

**Normalized domain** represents sessions, events, tool interactions, usage observations, and observation confidence. It must preserve unknown/missing fields rather than force a universal shape.

**Usage accounting** interprets provider-specific counters and deduplicates snapshots. It must remain separate from TUI formatting and from optional pricing configuration.

**Process observer** identifies candidate processes and records PID, start time, CPU, memory, and observation time. Process/session matching requires independent evidence; fuzzy candidates must be visibly unlinked.

**Application state and TUI** hold a roster, selection, filters, and transcript navigation. Renderers consume normalized data and should not parse raw provider files on every keypress.

## 3. Minimal domain vocabulary

| Concept | Purpose |
|---|---|
| Session key | `(host_id, provider, provider_session_id)`; local host ID is a placeholder until multi-device support |
| Session | Provider identity, source, project metadata, timeline boundaries, explicit lifecycle evidence |
| Event | Time, event kind, content, provider/source location, stable ID or offset |
| Tool interaction | Tool name, arguments, outcome, linked invocation/result IDs |
| Usage observation | Model, input/output/cache tokens, aggregation scope, provider evidence |
| Process observation | PID + start time + sample time, metrics, possible session association |

The shared provider interface and the types above are implemented in `src/domain.rs` and `src/providers/`; the vocabulary is intentionally small.

## 4. Read-only and format safety

- **JSONL:** stream complete records; buffer incomplete final lines; detect append/truncate/rotation; bound allocations; unknown or malformed records must not crash the application.
- **SQLite:** open with explicit read-only flags, short bounded queries, and WAL-aware handling. Do not use `immutable=1` to read a live writable database. SQLite may create/update its transient `-shm`/`-wal` sidecars; see [ADR-0002](decisions/0002-opencode-readonly-wal-sidecars.md). Never manually repair, delete, chmod, or copy provider files.
- **Data sourcing:** discover known provider data roots and allow explicit overrides; never read auth files or broad arbitrary directories simply to discover sessions.
- **Rendering:** neutralize ANSI/control sequences and mask common secret patterns by default. No implicit log upload or raw prompt diagnostics.
- **Isolation:** a corrupt or unsupported session must not prevent healthy providers and sessions from being inspected.

## 5. State and measurement semantics

Keep **session lifecycle**, **process observation**, and **activity/health inference** separate. “No new events for 10 minutes” is not proof of a crash, inactivity, or idle state. Explicit terminal events or captured process exit codes provide stronger evidence. Show the reason for each displayed status.

Token records can be cumulative, per-request, or repeated streams. Provider-specific aggregation tests are required before presenting totals. Show `Reported`, `Derived`, `Estimated`, or `Unknown` where appropriate; missing cost must not be displayed as `$0.00`.

## 6. Implemented structure and deliberate omissions

- **Threads and channels:** one background worker thread owns every provider reader (`ProviderReader`: bounded `refresh`, paged `transcript_page`, `count_events_after`, `source_record`) and the process sampler. The UI thread exchanges `WorkerRequest`/`WorkerEvent` messages over standard-library channels and never parses provider files on a keypress.
- **Bounded work:** each refresh call is limited by record, byte, and time budgets; roster size, page size, record size, loaded-event count, and the Claude dedup ledger are capped, with tests in `tests/perf_budget.rs`.
- **Known gap:** roster scans stream each file once rather than reading only headers/tails (metadata-only scan deferred); see the benchmark note.
- **Deliberately absent:** event bus, daemon, remote API, cache database, async runtime, network access, and any provider write path.

## 7. Future expansion (out of MVP)

For multiple machines, each host could run a read-only collector reporting to an authenticated hub, with encrypted transport and optional transcript sharing. The same normalized vocabulary and TUI may remain useful. This is an architectural direction, not a committed API or supported feature.

For orchestrator integration, correlation between Hermes tasks and provider sessions must come from explicit persisted identifiers or a consented integration. Do not infer lineage from time or directory alone.

## 8. Decision records

- [ADR-0001: Passive, read-only observation](decisions/0001-passive-read-only-observation.md)
- [ADR-0002: Read-only OpenCode access and WAL sidecars](decisions/0002-opencode-readonly-wal-sidecars.md)

Source compatibility is verified only against synthetic fixtures and, read-only, against codex-cli 0.159.2, Claude Code 2.1.311, and opencode 2.0.29 on one Windows machine. Provider formats are internal and may change; unsupported records are reported, not guessed.
