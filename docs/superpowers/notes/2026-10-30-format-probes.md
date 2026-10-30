# Provider format probes (read-only)

Date: 2026-10-30. Provider versions: Codex CLI 0.159.2, Claude Code 2.1.311, OpenCode 2.0.29 (SQLite 3.50.4). Only counts and field names recorded; no content, IDs, or user paths.

## 1. Codex cache-write tokens — UNVERIFIED
- Scanned 13 rollout JSONL files under the Codex root; 1115 token-usage records carry `cache_write_input_tokens`.
- Usage fields observed: `input_tokens`, `cached_input_tokens`, `cache_write_input_tokens`, `output_tokens`, `reasoning_output_tokens`, `total_tokens`.
- Records with `cache_write_input_tokens > 0`: **0**. Whether `input_tokens >= cached + cache_write` holds is **unverified**.
- Decision: Task 6 MUST return `Unknown` cost for any record with nonzero cache-write tokens (no guessing on subset vs. additive).

## 2. Claude message.id sharing — CONFIRMED
- 19 session files, 457 distinct assistant `message.id` values; 383 appear on more than one record (max 7 records per id).
- 364 of those 383 ids span records with different content-block type sequences (distinct blocks persisted as separate records sharing one id); the rest repeat the same block shape.
- `usage` was identical across all records of every shared id (383/383) — usage repeats per record.
- Conclusion: key assistant usage by `message.id` and count usage once per id (dedupe); never sum per record. Content blocks sharing an id must be merged, not treated as duplicates.

## 3. OpenCode session_message
- Columns: `id` (PK), `session_id`, `type`, `seq`, `time_created`, `time_updated`, `data`.
- Index `session_message_session_time_created_id_idx` column order: `(session_id, time_created, id)`. Also: unique `(session_id, seq)`, `(session_id, type, seq)`, `(time_created)`.
- Rows update in place: of 33610 rows, 33606 have `time_updated > time_created`, 4 equal, 0 less (every type, incl. assistant, user, system, compaction, synthetic). Cursors must not rely on `time_updated` being static nor on row immutability; ordering by `(session_id, time_created, id)` is stable, but content of already-read rows can change, so re-read/replace by `id`.
- Journal mode `wal`. Observed WAL state: main DB ~4.1 GB, `-wal` ~292 MB non-empty, `-shm` 32 KB present, no OpenCode process running (no live writer). Implementation must open read-only and tolerate WAL with/without shm; the other states (live writer, stale WAL with missing shm) were not observed locally and are unverified.

## 3b. OpenCode WAL states (synthetic temp fixtures, Python sqlite3 3.50.4 on Windows; no provider store touched)
- Live writer (WAL mode, autocheckpoint off, committed rows only in WAL): `-wal` and `-shm` exist; a second `mode=ro` connection sees the WAL-only committed rows. Safe to read.
- Stale WAL / missing shm (DB + WAL copied without shm, writer still alive at copy time): `mode=ro` open succeeds, sees the WAL rows, and creates a new `-shm` sidecar. `immutable=1` on the same copy ignores the WAL and fails with "no such table" (rows invisible) — never use `immutable=1` on a WAL database.
- No active WAL: after the last connection closes, the sidecars are removed (only the main file remains); `mode=ro` open succeeds with all data, and re-creates `-wal`/`-shm` sidecars on open.
- Conclusion: readers must use `mode=ro` (not `immutable`), tolerate sidecars appearing/disappearing, and never assume the directory is writable. Task 11 fixtures can build all three states this way. Truly corrupt/torn WAL was not constructed (unverified).
