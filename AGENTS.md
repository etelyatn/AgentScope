# AgentScope — Engineering Guide

This file provides shared engineering instructions for human contributors and AI coding agents. It applies to the entire repository. Read the relevant source and documentation before editing.

## Product boundary

AgentScope is a **local-first, read-only observability tool** for AI coding agents. Its initial product is a TUI for discovering and inspecting Codex, Claude Code, and OpenCode sessions, including recorded transcripts, tool calls, usage, and evidence-based activity status. It must work alongside existing agent workflows without changing them.

- **Observe; do not orchestrate.** Do not launch, stop, resume, approve, message, or alter monitored agents.
- **Never write to provider data.** Session files and databases are external, untrusted, and read-only; do not change permissions, configuration, or runtime files of those tools.
- **Do not invent data.** Distinguish provider-reported facts, derived values, estimates, and unknowns. Quiet is not crashed; process presence is not session progress.
- **Protect privacy.** Session content may include prompts, source code, secrets, and terminal escape sequences. Use synthetic test fixtures, sanitize rendered untrusted text, and avoid diagnostic logging of raw content.
- **Stay local in the MVP.** No network listeners, telemetry, remote synchronization, cloud dependencies, or agent integrations in the initial release.

See [`docs/PRODUCT.md`](docs/PRODUCT.md) for scope and acceptance scenarios and [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for proposed system boundaries. The architecture document is a proposal until corresponding behavior is implemented and verified.

## Rust design and coding

- Target stable Rust, edition 2024. Prefer idiomatic **safe Rust**. `unsafe` requires a narrowly scoped, documented justification and review.
- Keep changes **small, focused, and understandable**. Prefer KISS and YAGNI; avoid speculative traits, plugin systems, event buses, and abstractions for hypothetical providers.
- Separate provider-specific parsing from normalized domain semantics and from UI presentation. Introduce shared traits only after multiple implementations demonstrate a concrete need.
- Prefer explicit `Result` and `Option` flows; propagate recoverable errors with context. Do not panic on malformed or missing external data. Avoid unchecked `unwrap()`/`expect()` in production code.
- Use straightforward ownership and borrowing. Do not clone or wrap everything in `Arc<Mutex<_>>` to bypass design questions.
- Prefer bounded streaming and lazy loading for large session files. Make partial JSONL lines, appended records, file truncation, SQLite WAL, and schema changes visible and safe.
- Avoid Tokio, background daemons, process hooks, and persistent storage until measured requirements justify them.
- Preserve original source identifiers and provenance, even when normalizing events. Keep fields optional when the provider may omit them.
- Treat data from transcripts, provider metadata, command output, and filesystem paths as **untrusted input**, including when shown in a terminal.
- Do not silently expand the MVP to include a web dashboard, multi-device transport, or Hermes integration.

## Correctness requirements

- Aggregate tokens **per provider's actual usage semantics**. Guard against duplicated streaming events and cumulative counters; never sum snapshots blindly.
- Mark costs **estimated** unless they are explicitly reported by a trustworthy source. A missing amount is `Unknown`, not zero; subscription usage is not actual API spend.
- Associate a process with a session only when strong evidence exists; ambiguous associations stay unlinked. Consider process start time and PID reuse.
- Explicit completion/failure evidence is different from inferred inactivity. Show evidence and uncertainty in status text.
- Handle unsupported format variants without crashing or hiding unrelated valid sessions.

## Tests and validation

For meaningful code changes, add or update tests for success, malformed/unknown data, privacy, and realistic edge cases. Prefer synthetic or carefully reviewed, fully sanitized fixtures; **never commit actual transcripts, credentials, or private repositories**.

Before reporting a change complete, run what is applicable:

```bash
cargo fmt --all -- --check
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo test
```

Report commands run, results, and any checks you could not run. Do not claim a check passed unless it actually ran. Update public docs when verified behavior or user-facing interfaces change.

## Collaboration workflow

1. Inspect the relevant code and docs; identify the smallest safe change.
2. State assumptions and unresolved provider-format questions rather than hard-coding guesses.
3. Implement one coherent change with focused tests.
4. Review safety, correctness, cross-platform effects, and error handling.
5. Summarize behavior and test evidence; flag any limitations or future work.

Do not refactor unrelated code, modify user files, or introduce dependencies merely for convenience. **Do not commit, push, open PRs, or publish releases unless explicitly requested.** Preserve existing uncommitted changes; do not reset, clean, or stash them without authorization.

## Documentation conventions

- `README.md`: public overview and honest implementation status.
- `docs/PRODUCT.md`: product requirements and future roadmap.
- `docs/ARCHITECTURE.md`: architecture with proposal/implemented status clearly marked.
- `docs/decisions/`: lasting architectural decisions when a decision warrants a record.
- `CONTRIBUTING.md`: contributor setup and expected checks.

Keep documentation concise and current. Publish verified contracts and decisions, not raw exploratory transcripts or obsolete speculative implementation plans.
