# ADR-0002: Read-Only OpenCode Access and WAL Sidecars

**Status:** Accepted for v1<br>
**Date:** 2026-10-29

## Context

OpenCode stores sessions in a live SQLite database (`opencode.db`) that uses WAL mode. SQLite documents that a connection opened with `SQLITE_OPEN_READONLY` may still create or update the `-shm`/`-wal` sidecars when they are needed for a WAL read ([SQLite WAL: Read-Only Databases](https://sqlite.org/wal.html#read_only_databases)). `PRAGMA query_only` prevents SQL writes but does not prevent SQLite's own sidecar management.

AgentScope's passive contract forbids modifying provider session content, databases, configuration, permissions, credentials, or runtime files. A zero-sidecar guarantee is not achievable with SQLite flags alone. The alternatives were:

- A custom read-only VFS/private shared-memory boundary: strict and live-capable, but a substantial unsafe/FFI component with new correctness risks.
- Refusing live-WAL access: strict and simple, but makes OpenCode unavailable while it is running, which defeats the live observability use case.
- Copying the database and WAL to an AgentScope-owned temporary location: provider untouched, but a 3.86 GiB copy with snapshot-consistency, privacy, and cleanup costs.
- Allowing SQLite's transient sidecars only: live-capable and pragmatic, but narrows the absolute “no writes” claim.

## Decision

AgentScope opens the OpenCode database read-only and **allows SQLite to create or update only its transient `-shm`/`-wal` sidecars** as a documented exception. It does not:

- write session content, the main database, provider configuration, permissions, or credentials;
- manually delete, repair, chmod, or copy provider files;
- use `immutable=1` on a live WAL database;
- infer or synthesize data when the database cannot be read.

A SQLite authorizer allows only read operations on the allowlisted session tables/columns and denies everything else. If the directory is not writable and a live WAL cannot be read without creating a sidecar, the provider is reported as `Unavailable` and retried later; permissions are never changed. The exception is disclosed in the README and tested by asserting that session content and the main database are unchanged while only transient sidecars may differ.

## Consequences

**Benefits:** live OpenCode sessions remain observable; no database copy; no low-level VFS in v1; no SQL writes; credential and account tables remain inaccessible.

**Limitations:** the product cannot claim that the provider directory is byte-for-byte untouched while OpenCode is live. `-shm`/`-wal` may be created or updated by SQLite as runtime state. Those files are not transcript content, but they are still writes in the provider directory.

**Testing:** read-only regression checks the main database and session files, the authorizer, the no-manual-repair rule, and the sidecar exception. A test fixture with a non-writable directory verifies the `Unavailable` path without chmod or copy.

## Revisit conditions

If a strict zero-sidecar guarantee becomes required, implement a reviewed read-only VFS/private-shm boundary or an explicit snapshot workflow, and record a new ADR. Do not silently tighten the promise without changing the implementation.
