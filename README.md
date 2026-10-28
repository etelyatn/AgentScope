# AgentScope

**Read-only observability for AI coding agents.**

AgentScope aims to give developers one place to see which local coding-agent sessions exist, inspect recorded conversations and tool activity, review token usage and estimated costs, and investigate sessions that may need attention — without changing how those agents run.

**Project status: early design / pre-alpha.** The features described below are planned; no functional release is implied by this repository scaffold.

## Planned MVP

- **Unified session roster** for Codex CLI, Claude Code, and OpenCode.
- **Transcript explorer** with user/assistant messages, tool calls, results, errors, and timestamps when available.
- **Usage visibility** for reported token categories, provider-aware aggregation, and clearly labeled cost estimates.
- **Activity insights** grounded in persisted events and local process observations, with uncertainty shown explicitly.
- **Keyboard-first TUI**, powered by Rust, usable alongside existing orchestration tools and manual agent sessions.
- **Local-first privacy** with read-only access to provider data and no background uploads.

The first deliverable is a Codex-only session explorer; support for all three providers defines the intended MVP. This is an incremental rollout, not an indication that the Codex slice or full MVP is already implemented.

## Design principles

**No interference.** AgentScope does not start, stop, or approve coding agents and does not modify their session stores. **Evidence over guesses.** Missing logs, ambiguous process matches, and unknown costs remain explicitly unknown. **Extensible by observation.** Provider-specific adapters feed common inspection views without requiring an orchestration layer.

## Out of scope for the MVP

A web dashboard, multi-device hub, cloud sync, MCP server, agent control, Hermes-specific integration, and LLM-based reviewer are future possibilities rather than initial deliverables.

## Development

See [Product requirements](docs/PRODUCT.md), [Architecture](docs/ARCHITECTURE.md), and [Contributing](CONTRIBUTING.md).

The executable currently prints an under-development notice and exits. A library component reads bounded JSONL records from a caller-supplied buffered source, reporting malformed, oversized, and incomplete lines with byte offsets. It retains partial lines for subsequent reads, but does not discover sessions, interpret provider records, or detect truncation or rotation. Monitoring, usage accounting, and the TUI are not implemented. Use a stable Rust toolchain with `rustfmt` and `clippy`:

```bash
cargo run
cargo test
cargo fmt --all -- --check
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
```

Supported platforms and provider versions will be documented as implementations are validated. Planned targets are Windows and macOS, with Linux support to be evaluated.

## Security and privacy

Coding-agent logs may include private prompts, repository contents, paths, credentials, and untrusted terminal output. AgentScope's design requires read-only local inspection, cautious display sanitization, and synthetic test fixtures. These guarantees must be validated in implemented releases; design intent is not a security certification.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
