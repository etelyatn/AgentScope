# Contributing to AgentScope

AgentScope is in early development. Contributions should preserve its passive, local-first, read-only model and document behavior that has actually been implemented.

## Before making changes

Read [AGENTS.md](AGENTS.md), [product requirements](docs/PRODUCT.md), and the relevant section of [architecture](docs/ARCHITECTURE.md). For a provider adapter, validate the format against a concrete installed version; stored formats are often implementation details rather than public contracts.

## Local development

Install Rust through rustup; `rust-toolchain.toml` selects stable Rust with `rustfmt` and `clippy`. The package uses edition 2024 and `serde_json` for JSON parsing. From the repository root:

```bash
cargo check --all-targets
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The GitHub Actions workflow runs these checks on a Linux runner for pushes and pull requests. Its presence is not a claim of a passing CI run. Include a short verification summary with any PR. `cargo run` currently prints an under-development notice and exits.

## Pull requests

- Make one focused change at a time; describe its user-visible behavior and motivation.
- Mention which provider/tool versions and OS were used to validate parsing or process observation.
- Add synthetic fixtures and regression tests for edge cases (unknown records, partial writes, duplicate usage, malformed content, locked databases).
- Clearly state assumptions, limitations, and untested cases.
- Update public documentation when behavior, interfaces, or supported formats change.
- Avoid sweeping unrelated refactors or premature generic frameworks.

## Session data safety

**Do not upload, paste, commit, or attach real session transcripts by default.** They may contain private code, secrets, personal prompts, and shell output. Prefer hand-written synthetic JSONL/SQLite fixtures. If a format issue requires a redacted example, review every field and embedded content before publication; simple automated secret scanning is not sufficient.

External source stores must be read-only. The application must never change monitored agents or their data. When source evidence is incomplete, show an explicit unknown state instead of manufacturing certainty.

## Reporting issues

For provider compatibility issues, include the provider and version, operating system, a minimal *synthetic* reproduction, the expected/actual behavior, and whether any session status or usage figure is inferred. Do not include credential files or raw logs.

## Licensing

Contributions are governed by the [Apache License, Version 2.0](LICENSE), including its terms for submitted contributions.
