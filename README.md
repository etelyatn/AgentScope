# AgentScope

**Read-only observability for AI coding agents.**

AgentScope is a local, keyboard-first terminal application that discovers **Codex CLI**, **Claude Code**, and **OpenCode** sessions on one machine, lets you read what each session did, and reports token usage, estimated cost, and health only with visible evidence — without changing how those agents run.

**Project status: v1 implemented (pre-release).** All three providers, the transcript explorer, usage accounting, process candidates, and the TUI are implemented and covered by synthetic-fixture tests. Validation against real installations is partial; see [Verification status](#verification-status) and [Known limitations](#known-limitations) before relying on it.

## What it does

- **Unified session roster** for Codex, Claude Code, and OpenCode, with search, filter, and sort.
- **Transcript explorer**: user/assistant messages, reasoning summaries, tool calls and results (expandable), errors, and timestamps when the provider recorded them. Pages load lazily; huge records are bounded previews with an explicit way to view the source record.
- **Usage** per provider rules (cumulative snapshots, duplicate streamed records, cache categories), labeled `Reported`, `Derived`, `Partial`, `Estimated`, or `Unknown`. Cost is shown only when you configure an exact provider+model price; otherwise it is `Unknown`, never `$0.00`.
- **Evidence-based status**: `Active`, `Recent`, `Quiet`, `Possibly stalled`, `Complete`, `Failed`, `Cancelled`, `Unavailable`, or `Unknown`, each with its reason. Silence is never presented as proof of a crash.
- **Process candidates**: same-provider processes are listed as *candidates*; a session is shown as *matched* only when an identity-bearing argument (its session ID) appears on a process command line.
- **Live refresh**: appended records, partial trailing lines, in-place revisions, and rotation are handled without restarting.

## Run

```bash
cargo run --release
```

Options: `--config <path>`, `--codex-sessions <dir>`, `--claude-projects <dir>`, `--opencode-db <path>`, `--no-processes`, `--no-color`, `--help`, `--version`. The TUI needs an interactive terminal on both stdin and stdout.

Roster keys: `j`/`k` move, `g`/`G` first/last, `Enter` open, `/` search, `n`/`N` next/previous match, `f` filter, `s` sort (`S` reverse), `r` refresh, `p` process candidates, `?` help, `q` or `Ctrl+c` quit. Inspector keys: `j`/`k`/`g`/`G` move, `Enter` expand or collapse, `/` search loaded events, `n`/`N` next/previous match, `d` details, `v` source record, `D` diagnostics, `x` toggle masking in source views, `r` retry/refresh, `Esc` back.

Default data locations are the providers' own session stores (Codex sessions directory, Claude Code projects directory, OpenCode database); override them with the flags above or the config file (`agentscope/config.toml` in the platform config directory, or `--config`). An optional `[[pricing]]` table supplies exact per-provider, per-model USD prices; there is no built-in price list.

The experimental `inspect` command prints a Codex transcript from an explicitly supplied JSONL file without the TUI:

```bash
cargo run -- inspect path/to/session.jsonl
```

It escapes terminal controls and applies best-effort masking, reports malformed, incomplete, or over-1-MiB records with byte offsets, and may still print sensitive session content.

## Design principles

**No interference.** AgentScope does not start, stop, signal, or approve coding agents and does not modify their session stores, configuration, permissions, or credentials. **Evidence over guesses.** Missing logs, ambiguous process matches, and unknown costs remain explicitly unknown. **Local-first.** No network listeners, telemetry, or uploads.

## Read-only guarantee and its one exception

AgentScope opens Codex and Claude Code transcripts for reading only, and opens the OpenCode SQLite database read-only behind an authorizer that permits only allowlisted session tables and columns and denies credential, account, share-URL, write, and schema operations. It never reads `auth.json`, credential stores, or other provider state, and never repairs, deletes, chmods, or copies provider files.

**Exception ([ADR-0002](docs/decisions/0002-opencode-readonly-wal-sidecars.md)):** OpenCode uses SQLite WAL mode. SQLite itself may create or update its transient `-shm`/`-wal` sidecar files next to the database when a read-only connection reads a live WAL database. Those are SQLite runtime files, not session content, but they are writes in the provider directory, so the provider directory is **not** guaranteed byte-for-byte untouched while OpenCode is running. If the directory is not writable and a live WAL cannot be read without a sidecar, OpenCode is shown as `Unavailable` and retried; permissions are never changed.

## Verification status

Run on Windows 11 (rustc/cargo 1.99.1) with provider versions codex-cli 0.159.2, Claude Code 2.1.311, and opencode 2.0.29 installed:

- `cargo fmt --all -- --check`, `cargo check --all-targets`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, and `cargo build --release` pass.
- **Fixture-backed (synthetic) acceptance** for AC-01..AC-10 in `tests/acceptance.rs`, read-only regression in `tests/read_only.rs` (session files, the main database, mtimes, and permissions unchanged; only `-shm`/`-wal` sidecars may appear), and privacy in `tests/privacy.rs` (sentinel secrets absent from Debug output, warnings, errors, and rendered screens; control sequences neutralized; credential-shaped text masked).
- **Real-data smoke (read-only, counts only):** `cargo test --test acceptance -- --ignored --nocapture real_data_smoke` read this developer's real Codex, Claude Code, and OpenCode stores through the adapters (discovery and transcript page reading only) and compared a content-hash inventory before and after. Changes that coincide with a length or mtime change, new files, and removals are reported as inconclusive (a live provider or a SQLite sidecar); only a same-length, same-mtime content change fails. The last run: 2058 files, 2057 identical, 1 inconclusive (concurrent change), 0 silent changes. It prints counts only, never content.
- **Not verified:** an interactive TUI session on real data (the harness has no interactive terminal; the keyboard path is covered by `tests/tui_flow.rs` and the acceptance flow on fixtures), anything on macOS or Linux, and live-append against a real running agent (covered on synthetic fixtures only).
- Performance: deterministic work-budget tests plus informational timings in [the benchmark note](docs/superpowers/notes/2026-10-30-benchmarks.md). Measured on Windows only.

## Known limitations

- **Roster scan is not metadata-only.** Codex and Claude Code files are streamed once in the initial pass (bounded per call, each byte read once); header/tail-only scanning is deferred. Appending to a large file re-reads the consumed prefix for an integrity check.
- **Benchmarks:** Windows only; OpenCode not benchmarked; Claude Code keeps one open handle per scanned file (a roster of more than a few hundred Claude files may hit the default macOS descriptor limit). Preview building can allocate source-sized temporaries for one record (bounded by the 1 MiB record cap).
- **Process matching is conservative.** Most live agents do not put their session ID on the command line, so most sessions show `Unmatched` or only same-provider *candidates*. On Windows the working directory of other processes is not read.
- **Cost estimates need your price config** and exact model IDs; unsupported cache-accounting combinations stay `Unknown`.
- Codex `function_call` records are source-only (viewable with `v`); tool interactions come from completed thread items. Some uncommon provider part shapes are unverified.
- OpenCode: arrivals between opening a transcript and the first count can be missed by the "N new" indicator (content itself is not lost).
- No macOS or Linux validation yet; both are targets, not claims.

## Development

See [Product requirements](docs/PRODUCT.md), [Architecture](docs/ARCHITECTURE.md), and [Contributing](CONTRIBUTING.md). Use a stable Rust toolchain with `rustfmt` and `clippy`:

```bash
cargo fmt --all -- --check
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

## Security and privacy

Coding-agent logs may include private prompts, repository contents, paths, credentials, and untrusted terminal output. AgentScope neutralizes control sequences, masks common credential patterns by default (best effort — it cannot identify every secret), bounds record and page sizes, and uses synthetic fixtures only. Do not paste real transcripts into issues. In the inspector, `x` toggles masking for the selected record's source view (local only); nothing is ever transmitted. Redaction and read-only behavior are tested, not certified.

## Out of scope

A web dashboard, multi-device hub, cloud sync, MCP server, agent control, Hermes-specific integration, and LLM-based reviewer.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
