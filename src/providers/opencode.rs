//! OpenCode SQLite adapter: read-only connection, schema detection, roster scan.
//!
//! Access policy (ADR-0002): the database is opened `SQLITE_OPEN_READ_ONLY` with
//! `query_only`, and an authorizer allows only `SELECT` of allowlisted tables and
//! columns. SQLite may create/update its transient `-shm`/`-wal` sidecars; this
//! module never writes, deletes, repairs, or copies any provider file, and never
//! uses `immutable=1`. Account, credential, share, and key-value tables are denied
//! at the connection boundary, and `share_url` is never selectable.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::{Connection, ErrorCode, OpenFlags, params};
use serde_json::Value;

use super::{ProviderDelta, ProviderError, ProviderReader, SourceRecord};
use crate::config::Config;
use crate::domain::{
    Actor, BoundedText, DiagnosticEntry, Event, EventKind, Lifecycle, Lineage, LocatorKind,
    ModelDetail, PageAnchor, PageDirection, PageRequest, ProcessAssociation, Provenance, Provider,
    RefreshBudget, ScanProgress, SessionKey, SessionLink, SessionSummary, SourceKey, SourceLocator,
    Timestamp, ToolInteraction, ToolStatus, TranscriptPage, Warning, WarningKey, WarningKind,
};
use crate::ingest::text::{display_safe, preview};

const TITLE_BYTES: usize = 1024;
const TITLE_COLS: usize = 80;
const DEFAULT_BUSY_MS: u64 = 2000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Schema {
    /// `session_v2` + `session_message` generation.
    V2,
    /// `session` + `message` + `part`, only when v2 is absent or empty.
    Legacy,
    Unsupported,
}

/// Tables and the only columns of each that SQL may read. Anything else is denied.
const ALLOWED: &[(&str, &[&str])] = &[
    (
        "session_v2",
        &[
            "id",
            "title",
            "directory",
            "project_id",
            "parent_id",
            "fork_session_id",
            "model",
            "version",
            "time_created",
            "time_updated",
            "time_archived",
        ],
    ),
    (
        "session_message",
        &[
            "id",
            "session_id",
            "type",
            "data",
            "time_created",
            "time_updated",
        ],
    ),
    ("project", &["id", "name"]),
    (
        "session",
        &[
            "id",
            "title",
            "directory",
            "project_id",
            "parent_id",
            "version",
            "time_created",
            "time_updated",
            "time_archived",
        ],
    ),
    (
        "message",
        &["id", "session_id", "time_created", "time_updated"],
    ),
    (
        "part",
        &[
            "id",
            "message_id",
            "session_id",
            "time_created",
            "time_updated",
        ],
    ),
    // Only the table-existence count used to force the WAL/shm handshake on open.
    ("sqlite_master", &["name"]),
    ("sqlite_schema", &["name"]),
];

/// Aggregates only; no extension, file, or text-producing functions.
const ALLOWED_FUNCTIONS: &[&str] = &["count", "max", "min", "substr", "length", "coalesce"];

fn authorize(ctx: AuthContext<'_>) -> Authorization {
    match ctx.action {
        AuthAction::Select => Authorization::Allow,
        AuthAction::Read {
            table_name,
            column_name,
        } => {
            let allowed = ALLOWED.iter().any(|(table, columns)| {
                table.eq_ignore_ascii_case(table_name)
                    && (column_name.is_empty()
                        || columns.iter().any(|c| c.eq_ignore_ascii_case(column_name)))
            });
            if allowed {
                Authorization::Allow
            } else {
                Authorization::Deny
            }
        }
        AuthAction::Function { function_name } => {
            if ALLOWED_FUNCTIONS
                .iter()
                .any(|f| f.eq_ignore_ascii_case(function_name))
            {
                Authorization::Allow
            } else {
                Authorization::Deny
            }
        }
        _ => Authorization::Deny,
    }
}

fn map_error(error: rusqlite::Error) -> ProviderError {
    match &error {
        rusqlite::Error::SqliteFailure(failure, _)
        | rusqlite::Error::SqlInputError { error: failure, .. } => match failure.code {
            ErrorCode::DatabaseBusy
            | ErrorCode::DatabaseLocked
            | ErrorCode::CannotOpen
            | ErrorCode::ReadOnly
            | ErrorCode::SystemIoFailure
            | ErrorCode::DiskFull
            | ErrorCode::OutOfMemory
            | ErrorCode::FileLockingProtocolFailed
            | ErrorCode::NoLargeFileSupport => ProviderError::Unavailable(error.to_string()),
            ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt => {
                ProviderError::Parse(error.to_string())
            }
            _ => ProviderError::Internal(error.to_string()),
        },
        _ => ProviderError::Internal(error.to_string()),
    }
}

/// Opens `path` read-only behind the authorizer. The first schema read happens
/// here so a WAL that needs a sidecar SQLite cannot create surfaces as
/// `Unavailable` immediately rather than on a later query.
pub fn open_read_only(path: &Path) -> Result<Connection, ProviderError> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let mut conn = Connection::open_with_flags(path, flags).map_err(map_error)?;
    conn.busy_timeout(Duration::from_millis(DEFAULT_BUSY_MS))
        .map_err(map_error)?;
    conn.pragma_update(None, "query_only", true)
        .map_err(map_error)?;
    conn.authorizer(Some(authorize)).map_err(map_error)?;
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
        r.get::<_, i64>(0)
    })
    .map_err(map_error)?;
    Ok(conn)
}

const V2_SESSION_COLUMNS: &str = "id, title, directory, project_id, parent_id, fork_session_id, \
     model, version, time_created, time_updated, time_archived";
const LEGACY_SESSION_COLUMNS: &str = "id, title, directory, project_id, parent_id, version, time_created, time_updated, time_archived";

/// True when `sql` prepares. A SQLite "no such table/column" error means the shape
/// is absent; authorization and I/O failures are real errors.
fn shape_present(conn: &Connection, sql: &str) -> Result<bool, ProviderError> {
    match conn.prepare(sql) {
        Ok(_) => Ok(true),
        Err(
            rusqlite::Error::SqliteFailure(f, _) | rusqlite::Error::SqlInputError { error: f, .. },
        ) if f.code == ErrorCode::Unknown => Ok(false),
        Err(e) => Err(map_error(e)),
    }
}

fn has_rows(conn: &Connection, table: &str) -> Result<bool, ProviderError> {
    conn.query_row(&format!("SELECT EXISTS(SELECT 1 FROM {table})"), [], |r| {
        r.get::<_, bool>(0)
    })
    .map_err(map_error)
}

/// v2 when its tables are recognised and populated; legacy when v2 is absent or
/// empty and the legacy shape is recognised; otherwise unsupported. Never guesses.
pub fn detect_schema(conn: &Connection) -> Result<Schema, ProviderError> {
    let v2 = shape_present(
        conn,
        &format!("SELECT {V2_SESSION_COLUMNS} FROM session_v2 LIMIT 0"),
    )? && shape_present(
        conn,
        "SELECT id, session_id, time_created, time_updated FROM session_message LIMIT 0",
    )?;
    let legacy = shape_present(
        conn,
        &format!("SELECT {LEGACY_SESSION_COLUMNS} FROM session LIMIT 0"),
    )? && shape_present(
        conn,
        "SELECT id, session_id, time_created, time_updated FROM message LIMIT 0",
    )? && shape_present(conn, "SELECT id, message_id, session_id FROM part LIMIT 0")?;
    if v2 {
        let populated = has_rows(conn, "session_v2")? || has_rows(conn, "session_message")?;
        if populated || !legacy || !has_rows(conn, "session")? {
            return Ok(Schema::V2);
        }
        return Ok(Schema::Legacy);
    }
    Ok(if legacy {
        Schema::Legacy
    } else {
        Schema::Unsupported
    })
}

struct Tables {
    session: &'static str,
    message: &'static str,
    /// `session` column expressions that legacy databases lack.
    model: &'static str,
    fork: &'static str,
}

impl Tables {
    fn for_schema(schema: Schema) -> Option<Self> {
        match schema {
            Schema::V2 => Some(Self {
                session: "session_v2",
                message: "session_message",
                model: "s.model",
                fork: "s.fork_session_id",
            }),
            Schema::Legacy => Some(Self {
                session: "session",
                message: "message",
                model: "NULL",
                fork: "NULL",
            }),
            Schema::Unsupported => None,
        }
    }
}

struct Row {
    id: String,
    title: Option<String>,
    directory: Option<String>,
    project_name: Option<String>,
    parent: Option<String>,
    parent_exists: bool,
    fork: Option<String>,
    fork_exists: bool,
    model: Option<String>,
    version: Option<String>,
    created: Option<i64>,
    updated: i64,
    archived: Option<i64>,
    message_updated: Option<i64>,
}

struct Pass {
    cursor: Option<(i64, String)>,
    seen: HashSet<String>,
    processed: usize,
    cap: usize,
    total: usize,
    oversized: usize,
}

pub struct OpenCodeScanner {
    config: Config,
    path: PathBuf,
    connection: Option<Connection>,
    schema: Schema,
    has_project: bool,
    pass: Option<Pass>,
    known: HashMap<String, SessionSummary>,
}

impl OpenCodeScanner {
    pub fn new(config: &Config) -> Result<Self, ProviderError> {
        let path = config.providers.opencode.db.clone().ok_or_else(|| {
            ProviderError::Unavailable("no OpenCode database path configured".into())
        })?;
        let mut scanner = Self {
            config: config.clone(),
            path,
            connection: None,
            schema: Schema::Unsupported,
            has_project: false,
            pass: None,
            known: HashMap::new(),
        };
        scanner.ensure_open()?;
        Ok(scanner)
    }

    fn ensure_open(&mut self) -> Result<(), ProviderError> {
        if self.connection.is_some() {
            return Ok(());
        }
        let conn = open_read_only(&self.path)?;
        conn.busy_timeout(Duration::from_millis(self.config.refresh_busy_timeout_ms))
            .map_err(map_error)?;
        self.schema = detect_schema(&conn)?;
        self.has_project = shape_present(&conn, "SELECT id, name FROM project LIMIT 0")?;
        self.connection = Some(conn);
        Ok(())
    }

    fn warning(&self, label: &str, kind: WarningKind, message: &str) -> Warning {
        Warning {
            key: WarningKey {
                provider: Provider::OpenCode,
                source: format!("{}#{label}", self.path.display()),
                kind,
            },
            message: message.into(),
            count: 1,
        }
    }

    /// One bounded step of a roster pass. Sessions are visited newest
    /// `time_updated` first, `max_records` per call, up to `max_sessions` per pass;
    /// only changed sessions are returned. On any read error the connection is
    /// dropped and reopened by the next call.
    pub fn scan_roster(
        &mut self,
        _now: Timestamp,
        budget: RefreshBudget,
    ) -> Result<ProviderDelta, ProviderError> {
        let refresh_ms = self.config.refresh_busy_timeout_ms;
        let result = self
            .ensure_open()
            .and_then(|()| self.set_busy_timeout(refresh_ms))
            .and_then(|()| self.step(budget));
        if result.is_err() {
            self.connection = None;
            self.pass = None;
        }
        result
    }

    fn step(&mut self, budget: RefreshBudget) -> Result<ProviderDelta, ProviderError> {
        let started = Instant::now();
        let mut warnings = Vec::new();
        let Some(tables) = Tables::for_schema(self.schema) else {
            warnings.push(self.warning(
                "schema",
                WarningKind::Unsupported,
                "unsupported OpenCode database schema; no sessions listed",
            ));
            return Ok(ProviderDelta {
                sessions: vec![],
                removed: vec![],
                warnings,
                scan: ScanProgress {
                    provider: Provider::OpenCode,
                    scanned: 0,
                    total: Some(0),
                    done: true,
                },
            });
        };
        let conn = self.connection.as_ref().expect("opened");
        if self.schema == Schema::Legacy
            && shape_present(conn, "SELECT id FROM session_v2 LIMIT 0")?
        {
            warnings.push(self.warning(
                "schema",
                WarningKind::Unsupported,
                "v2 tables are empty; reading legacy session tables",
            ));
        }

        let mut pass = match self.pass.take() {
            Some(pass) => pass,
            None => {
                let count = |sql: String| -> Result<usize, ProviderError> {
                    let n: i64 = conn.query_row(&sql, [], |r| r.get(0)).map_err(map_error)?;
                    Ok(usize::try_from(n).unwrap_or(0))
                };
                let all = count(format!("SELECT count(*) FROM {}", tables.session))?;
                let total = count(format!(
                    "SELECT count(*) FROM {} s WHERE {}",
                    tables.session,
                    eligible_ids(&tables)
                ))?;
                Pass {
                    cursor: None,
                    seen: HashSet::new(),
                    processed: 0,
                    cap: total.min(self.config.max_sessions),
                    total,
                    oversized: all - total.min(all),
                }
            }
        };

        let remaining = pass.cap.saturating_sub(pass.processed);
        let limit = budget.max_records.min(remaining);
        let (rows, ended) = if limit == 0 {
            (Vec::new(), false)
        } else {
            read_page(
                conn,
                &tables,
                self.has_project,
                pass.cursor.as_ref(),
                limit,
                &budget,
                started,
            )?
        };

        let mut sessions = Vec::new();
        for row in &rows {
            pass.cursor = Some((row.updated, row.id.clone()));
            pass.seen.insert(row.id.clone());
            let summary = self.summarize(row, &tables);
            if self.known.get(&row.id) != Some(&summary) {
                self.known.insert(row.id.clone(), summary.clone());
                sessions.push(summary);
            }
        }
        pass.processed += rows.len();
        let done = ended || pass.processed >= pass.cap;
        let mut removed = Vec::new();
        if done {
            let conn = self.connection.as_ref().expect("opened");
            let capped = pass.total > pass.cap;
            let missing: Vec<String> = self
                .known
                .keys()
                .filter(|id| !pass.seen.contains(*id))
                .cloned()
                .collect();
            // Retained sessions must match this pass's selection. Rows displaced by the
            // cap are evicted even though they still exist; without a cap, a row that
            // merely moved in the ordering mid-pass is kept when it still exists.
            for id in missing {
                let keep = !capped
                    && conn
                        .query_row(
                            &format!(
                                "SELECT EXISTS(SELECT 1 FROM {} WHERE id = ?1)",
                                tables.session
                            ),
                            params![id],
                            |r| r.get::<_, bool>(0),
                        )
                        .map_err(map_error)?;
                if !keep && let Some(summary) = self.known.remove(&id) {
                    removed.push(summary.key);
                }
            }
            if pass.oversized > 0 {
                let mut w = self.warning(
                    "oversized-id",
                    WarningKind::Unsupported,
                    "sessions skipped: identifier too long to key exactly",
                );
                w.count = u32::try_from(pass.oversized).unwrap_or(u32::MAX);
                warnings.push(w);
            }
            if pass.total > pass.cap {
                let mut w = self.warning(
                    "omitted",
                    WarningKind::Unsupported,
                    "sessions omitted by max_sessions",
                );
                w.count = u32::try_from(pass.total - pass.cap).unwrap_or(u32::MAX);
                warnings.push(w);
            }
        }
        let scan = ScanProgress {
            provider: Provider::OpenCode,
            scanned: pass.processed,
            total: Some(pass.cap),
            done,
        };
        if !done {
            self.pass = Some(pass);
        }
        Ok(ProviderDelta {
            sessions,
            removed,
            warnings,
            scan,
        })
    }

    fn summarize(&self, row: &Row, tables: &Tables) -> SessionSummary {
        let details: Vec<ModelDetail> = row
            .model
            .as_deref()
            .and_then(model_detail)
            .into_iter()
            .collect();
        let link = |id: &Option<String>, exists: bool| {
            id.as_ref().map(|id| {
                if exists {
                    SessionLink::Resolved(key(tables, id))
                } else {
                    SessionLink::Unresolved(id.clone())
                }
            })
        };
        let directory = row.directory.as_deref().filter(|d| !d.is_empty());
        let project = row.project_name.as_deref().and_then(short).or_else(|| {
            directory
                .map(Path::new)
                .and_then(Path::file_name)
                .and_then(|n| short(&n.to_string_lossy()))
        });
        SessionSummary {
            key: key(tables, &row.id),
            title: row
                .title
                .as_deref()
                .and_then(short)
                .unwrap_or_else(|| row.id.clone()),
            title_derived: false,
            project,
            cwd: directory.map(PathBuf::from),
            models: details.iter().map(|d| d.id.clone()).collect(),
            model_details: details,
            provider_version: row.version.as_deref().and_then(short),
            created_at: row.created,
            last_event_at: None,
            last_persisted_at: Some(row.updated.max(row.message_updated.unwrap_or(i64::MIN))),
            last_completion: None,
            unfinished_work: false,
            displayed_event_count: 0,
            hidden_records: 0,
            source_available: true,
            archived: row.archived.is_some(),
            context_window: None,
            server_tools: None,
            lifecycle: Lifecycle::Unknown,
            process: ProcessAssociation::NotChecked,
            usage: None,
            lineage: Lineage {
                parent: link(&row.parent, row.parent_exists),
                fork: link(&row.fork, row.fork_exists),
                sidechain: false,
            },
            warnings: vec![],
        }
    }
}

fn key(tables: &Tables, id: &str) -> SessionKey {
    SessionKey {
        provider: Provider::OpenCode,
        provider_session_id: id.to_owned(),
        source: SourceKey::DbRow {
            table: tables.session.to_owned(),
            id: id.to_owned(),
        },
    }
}

/// Collapsed, redacted, bounded one-line text; `None` when nothing is left.
fn short(text: &str) -> Option<String> {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let out = preview(&collapsed, TITLE_BYTES, TITLE_COLS).preview;
    (!out.trim().is_empty()).then_some(out)
}

/// `model` is JSON (`{"id", "providerID", "variant"}`); a bare string is its own id.
fn model_detail(raw: &str) -> Option<ModelDetail> {
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(map)) => {
            let text = |name: &str| map.get(name).and_then(Value::as_str).and_then(short);
            Some(ModelDetail {
                id: text("id")?,
                provider: text("providerID"),
                variant: text("variant"),
            })
        }
        Ok(Value::String(s)) => short(&s).map(bare_model),
        Ok(_) => None,
        Err(_) => {
            let t = raw.trim_start();
            if t.starts_with(['{', '[', '"']) {
                None
            } else {
                short(raw).map(bare_model)
            }
        }
    }
}

fn bare_model(id: String) -> ModelDetail {
    ModelDetail {
        id,
        provider: None,
        variant: None,
    }
}

/// Display strings are cut in SQL so one oversized value is never materialised whole.
/// Identifiers are never cut: identity, cache keys, and keyset cursors need exact
/// values, so sessions whose id/parent/fork exceed `ID_MAX` chars are skipped (and
/// counted in a warning) instead of being truncated into colliding keys.
const ID_MAX: usize = 4096;
const MODEL_MAX: usize = 65_536;
const TEXT_CHARS: usize = 1024;
const PATH_CHARS: usize = 4096;
const VERSION_CHARS: usize = 128;

/// SQL predicate (alias `s`) selecting sessions with exact, bounded identifiers.
fn eligible_ids(tables: &Tables) -> String {
    let mut sql =
        format!("length(s.id) <= {ID_MAX} AND coalesce(length(s.parent_id), 0) <= {ID_MAX}");
    if tables.fork != "NULL" {
        sql.push_str(&format!(
            " AND coalesce(length({}), 0) <= {ID_MAX}",
            tables.fork
        ));
    }
    sql
}

fn row_bytes(row: &Row) -> usize {
    let opt = |s: &Option<String>| s.as_ref().map_or(0, String::len);
    64 + row.id.len()
        + opt(&row.title)
        + opt(&row.directory)
        + opt(&row.project_name)
        + opt(&row.parent)
        + opt(&row.fork)
        + opt(&row.model)
        + opt(&row.version)
}

/// Reads up to `limit` rows, stopping early once the byte or time allowance is
/// spent. A zero byte/time allowance reads nothing; otherwise the first row is
/// always taken so a pass makes progress (its size is bounded by the column caps).
/// Returns the rows and whether the result set ended (no more rows to read).
fn read_page(
    conn: &Connection,
    tables: &Tables,
    has_project: bool,
    cursor: Option<&(i64, String)>,
    limit: usize,
    budget: &RefreshBudget,
    started: Instant,
) -> Result<(Vec<Row>, bool), ProviderError> {
    if budget.max_bytes == 0 || budget.max_millis == 0 {
        return Ok((Vec::new(), false));
    }
    let deadline = Duration::from_millis(budget.max_millis);
    let (project_name, project_join) = if has_project {
        (
            format!("substr(p.name, 1, {TEXT_CHARS})"),
            "LEFT JOIN project p ON p.id = s.project_id",
        )
    } else {
        ("NULL".to_owned(), "")
    };
    let fork = if tables.fork == "NULL" {
        "NULL".to_owned()
    } else {
        tables.fork.to_owned()
    };
    // Whole JSON or nothing: a cut fragment must never be read as a model id.
    let model = if tables.model == "NULL" {
        "NULL".to_owned()
    } else {
        format!(
            "CASE WHEN length({m}) <= {MODEL_MAX} THEN {m} END",
            m = tables.model
        )
    };
    let eligible = eligible_ids(tables);
    let sql = format!(
        "SELECT s.id, substr(s.title, 1, {TEXT_CHARS}), \
            substr(s.directory, 1, {PATH_CHARS}), {project_name}, \
            s.parent_id, \
            EXISTS(SELECT 1 FROM {t} x WHERE x.id = s.parent_id), {fork}, \
            EXISTS(SELECT 1 FROM {t} y WHERE y.id = {fork}), {model}, \
            substr(s.version, 1, {VERSION_CHARS}), \
            s.time_created, s.time_updated, s.time_archived, \
            (SELECT max(m.time_updated) FROM {m} m WHERE m.session_id = s.id) \
         FROM {t} s {project_join} \
         WHERE {eligible} AND (s.time_updated < ?1 OR (s.time_updated = ?1 AND s.id < ?2)) \
         ORDER BY s.time_updated DESC, s.id DESC LIMIT ?3",
        t = tables.session,
        m = tables.message,
    );
    let (after_time, after_id) = match cursor {
        Some((time, id)) => (*time, id.as_str()),
        None => (i64::MAX, ""),
    };
    let mut stmt = conn.prepare(&sql).map_err(map_error)?;
    let limit_sql = i64::try_from(limit).unwrap_or(i64::MAX);
    let mut query = stmt
        .query(params![after_time, after_id, limit_sql])
        .map_err(map_error)?;
    let mut rows = Vec::new();
    let mut bytes = 0usize;
    while let Some(r) = query.next().map_err(map_error)? {
        let row = Row {
            id: r.get(0).map_err(map_error)?,
            title: r.get(1).map_err(map_error)?,
            directory: r.get(2).map_err(map_error)?,
            project_name: r.get(3).map_err(map_error)?,
            parent: r.get(4).map_err(map_error)?,
            parent_exists: r.get(5).map_err(map_error)?,
            fork: r.get(6).map_err(map_error)?,
            fork_exists: r.get(7).map_err(map_error)?,
            model: r.get(8).map_err(map_error)?,
            version: r.get(9).map_err(map_error)?,
            created: r.get(10).map_err(map_error)?,
            updated: r.get(11).map_err(map_error)?,
            archived: r.get(12).map_err(map_error)?,
            message_updated: r.get(13).map_err(map_error)?,
        };
        bytes = bytes.saturating_add(row_bytes(&row));
        rows.push(row);
        if rows.len() == limit {
            return Ok((rows, false));
        }
        if bytes >= budget.max_bytes || started.elapsed() >= deadline {
            return Ok((rows, false));
        }
    }
    Ok((rows, true))
}

// ---------------------------------------------------------------------------
// Transcript: `session_message` mapping, keyset paging, source records.
// ---------------------------------------------------------------------------

const MESSAGE_TABLE: &str = "session_message";
const PREVIEW_CAP: usize = 2048;
const DIAG_CAP: usize = 200;
const TYPE_CHARS: usize = 64;

/// One `session_message` row. `data` is `None` when the column is NULL or longer
/// than the record cap (`data_len` then tells which); the oversized body is never
/// read into memory.
struct RawRow {
    id: String,
    created: i64,
    updated: i64,
    ty: String,
    data: Option<String>,
    data_len: Option<i64>,
}

/// The row's change marker. `time_updated` moves whenever the provider rewrites a
/// row in place, so a locator minted for older content no longer matches.
fn generation(row: &RawRow) -> u64 {
    u64::try_from(row.updated).unwrap_or(0)
}

fn lossy(value: rusqlite::types::ValueRef<'_>) -> Option<String> {
    match value {
        rusqlite::types::ValueRef::Text(b) | rusqlite::types::ValueRef::Blob(b) => {
            Some(String::from_utf8_lossy(b).into_owned())
        }
        _ => None,
    }
}

/// Streams `session_message` rows of one session in `(time_created, id)` order,
/// starting at (and including) the anchor's row when given. Keyset comparison on the
/// `(session_id, time_created, id)` index; no `OFFSET`, no row accumulation: the
/// callback sees one row at a time and returns `false` to stop.
fn stream_rows(
    conn: &Connection,
    session: &str,
    record_cap: usize,
    direction: PageDirection,
    from: Option<(i64, &str)>,
    mut each: impl FnMut(RawRow) -> Result<bool, ProviderError>,
) -> Result<(), ProviderError> {
    let (cmp, order) = match direction {
        PageDirection::Forward => (">=", "ASC"),
        PageDirection::Backward => ("<=", "DESC"),
    };
    // Only static fragments are spliced into the statement; every value is bound.
    let bound = if from.is_some() {
        format!(" AND (time_created, id) {cmp} (?4, ?5)")
    } else {
        String::new()
    };
    let sql = format!(
        "SELECT id, time_created, time_updated, substr(type, 1, {TYPE_CHARS}), \
         CASE WHEN length(CAST(data AS BLOB)) <= ?2 THEN data END, length(CAST(data AS BLOB)) \
         FROM {MESSAGE_TABLE} \
         WHERE session_id = ?1 AND length(id) <= ?3{bound} \
         ORDER BY time_created {order}, id {order}"
    );
    let cap = i64::try_from(record_cap).unwrap_or(i64::MAX);
    let max_id = i64::try_from(ID_MAX).unwrap_or(i64::MAX);
    let mut statement = conn.prepare(&sql).map_err(map_error)?;
    let mut rows = match from {
        Some((created, id)) => statement.query(params![session, cap, max_id, created, id]),
        None => statement.query(params![session, cap, max_id]),
    }
    .map_err(map_error)?;
    while let Some(row) = rows.next().map_err(map_error)? {
        let raw = RawRow {
            id: row.get(0).map_err(map_error)?,
            created: row.get(1).map_err(map_error)?,
            updated: row.get(2).map_err(map_error)?,
            ty: row
                .get_ref(3)
                .map_err(map_error)
                .map(lossy)?
                .unwrap_or_default(),
            data: row.get_ref(4).map_err(map_error).map(lossy)?,
            data_len: row.get(5).map_err(map_error)?,
        };
        if !each(raw)? {
            break;
        }
    }
    Ok(())
}

enum Mapped {
    Event(Box<Event>),
    Diag(DiagnosticEntry),
}

/// Maps rows to events and diagnostics. `cap` bounds every preview.
struct Mapper<'a> {
    key: &'a SessionKey,
    version: Option<&'a str>,
    cap: usize,
}

/// Text of a JSON value: strings verbatim, anything else compact JSON.
fn value_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(s) => (!s.is_empty()).then(|| s.clone()),
        other => Some(other.to_string()),
    }
}

/// Tool content is often `[{"type":"text","text":...}]`; show the text, not the JSON.
fn content_text(value: &Value) -> Option<String> {
    if let Value::Array(items) = value {
        let texts: Vec<&str> = items
            .iter()
            .filter_map(|i| i.get("text").and_then(Value::as_str))
            .collect();
        if !texts.is_empty() && texts.len() == items.len() {
            return Some(texts.join("\n"));
        }
    }
    value_text(value)
}

fn str_field<'v>(value: &'v Value, name: &str) -> Option<&'v str> {
    value.get(name).and_then(Value::as_str)
}

impl Mapper<'_> {
    fn locator(&self, row: &RawRow, part: usize) -> SourceLocator {
        SourceLocator {
            session: self.key.clone(),
            generation: Some(generation(row)),
            kind: LocatorKind::DbRow {
                table: MESSAGE_TABLE.to_owned(),
                row_id: row.id.clone(),
                part_index: Some(part),
            },
        }
    }

    fn bounded(&self, text: &str, locator: &SourceLocator) -> BoundedText {
        let mut bounded = preview(text, self.cap, self.cap);
        bounded.locator = Some(locator.clone());
        bounded
    }

    fn diag(&self, row: &RawRow, part: usize, label: &str) -> (usize, Mapped) {
        (
            part,
            Mapped::Diag(DiagnosticEntry {
                locator: self.locator(row, part),
                label: label.to_owned(),
                hidden: true,
            }),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn event(
        &self,
        row: &RawRow,
        part: usize,
        record_type: String,
        kind: EventKind,
        actor: Actor,
        text: Option<String>,
        tool: Option<ToolInteraction>,
    ) -> (usize, Mapped) {
        let locator = self.locator(row, part);
        let text = text
            .filter(|t| !t.is_empty())
            .map(|t| self.bounded(&t, &locator));
        (
            part,
            Mapped::Event(Box::new(Event {
                id: format!("{}#{part}", row.id),
                session: self.key.clone(),
                timestamp: Some(row.created),
                source_order: part as u64,
                kind,
                actor,
                text,
                tool,
                usage: None,
                provenance: Provenance {
                    provider: Provider::OpenCode,
                    source: self.key.source.clone(),
                    locator,
                    record_type,
                    provider_version: self.version.map(str::to_owned),
                },
            })),
        )
    }

    /// Items of one row in part order. A row that displays nothing yields one
    /// "source-only" diagnostic so it stays reachable in the source view.
    fn map_row(&self, row: &RawRow, record_cap: usize) -> Vec<(usize, Mapped)> {
        let Some(data) = &row.data else {
            let label = if row.data_len.is_some_and(|n| n > record_cap as i64) {
                "oversized record"
            } else {
                "source-only record (no data)"
            };
            return vec![self.diag(row, 0, label)];
        };
        let Ok(value @ Value::Object(_)) = serde_json::from_str::<Value>(data) else {
            return vec![self.diag(row, 0, "malformed record")];
        };
        let ty = row.ty.as_str();
        let one = |kind, actor, text: Option<String>| {
            self.event(row, 0, ty.to_owned(), kind, actor, text, None)
        };
        let mut items: Vec<(usize, Mapped)> = Vec::new();
        match ty {
            "user" => {
                if let Some(text) = str_field(&value, "text").filter(|t| !t.is_empty()) {
                    items.push(one(
                        EventKind::UserPrompt,
                        Actor::User,
                        Some(text.to_owned()),
                    ));
                }
                let mut part = 1;
                for (label, field) in [("file", "files"), ("agent", "agents")] {
                    for item in value
                        .get(field)
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        let name = ["name", "filename", "path", "uri", "url"]
                            .iter()
                            .find_map(|k| str_field(item, k))
                            .map(str::to_owned)
                            .or_else(|| value_text(item))
                            .unwrap_or_default();
                        items.push(self.event(
                            row,
                            part,
                            ty.to_owned(),
                            EventKind::System,
                            Actor::User,
                            Some(format!("{label}: {name}")),
                            None,
                        ));
                        part += 1;
                    }
                }
            }
            "system" | "synthetic" => {
                if let Some(text) = str_field(&value, "text").filter(|t| !t.is_empty()) {
                    items.push(one(EventKind::System, Actor::System, Some(text.to_owned())));
                }
            }
            "compaction" => {
                let text = ["summary", "text"]
                    .iter()
                    .find_map(|k| str_field(&value, k))
                    .filter(|t| !t.is_empty())
                    .unwrap_or("Context compacted");
                items.push(one(
                    EventKind::Compaction,
                    Actor::System,
                    Some(text.to_owned()),
                ));
            }
            "idle" => {
                let text = match str_field(&value, "outcome").filter(|o| !o.is_empty()) {
                    Some(outcome) => format!("Idle \u{2014} {outcome}"),
                    None => "Idle".to_owned(),
                };
                items.push(one(EventKind::Lifecycle, Actor::System, Some(text)));
            }
            "assistant" => {
                if let Some(parts) = value.get("content").and_then(Value::as_array) {
                    for (part, item) in parts.iter().enumerate() {
                        items.extend(self.assistant_part(row, part, item));
                    }
                }
            }
            _ => return vec![self.diag(row, 0, "unknown record type")],
        }
        if items.is_empty() {
            items.push(self.diag(row, 0, "source-only record (nothing to display)"));
        }
        items
    }

    fn assistant_part(&self, row: &RawRow, part: usize, item: &Value) -> Option<(usize, Mapped)> {
        let record_type = |inner: &str| format!("assistant/{inner}");
        let text_of = |name: &str| str_field(item, name).filter(|t| !t.is_empty());
        match str_field(item, "type") {
            Some("text") => text_of("text").map(|t| {
                self.event(
                    row,
                    part,
                    record_type("text"),
                    EventKind::AssistantMessage,
                    Actor::Assistant,
                    Some(t.to_owned()),
                    None,
                )
            }),
            Some("reasoning") => text_of("text").map(|t| {
                self.event(
                    row,
                    part,
                    record_type("reasoning"),
                    EventKind::Reasoning,
                    Actor::Assistant,
                    Some(t.to_owned()),
                    None,
                )
            }),
            Some("tool") => Some(self.tool_part(row, part, item)),
            Some(label @ ("patch" | "file" | "agent" | "subtask")) => {
                let mut rest = item.clone();
                if let Value::Object(map) = &mut rest {
                    map.remove("type");
                }
                Some(self.event(
                    row,
                    part,
                    record_type(label),
                    EventKind::System,
                    Actor::System,
                    Some(format!("{label}: {rest}")),
                    None,
                ))
            }
            _ => Some(self.diag(row, part, "unknown part type")),
        }
    }

    fn tool_part(&self, row: &RawRow, part: usize, item: &Value) -> (usize, Mapped) {
        let locator = self.locator(row, part);
        let null = Value::Null;
        let state = item.get("state").unwrap_or(&null);
        let status = match str_field(state, "status") {
            Some("completed") => ToolStatus::Completed,
            Some("error") => ToolStatus::Error,
            _ => ToolStatus::Unknown,
        };
        let time = item.get("time").unwrap_or(&null);
        let stamp = |name: &str| time.get(name).and_then(Value::as_i64);
        let metadata = state.get("metadata").unwrap_or(&null);
        let exit_code = ["exit", "exitCode", "exit_code"]
            .iter()
            .find_map(|k| metadata.get(*k).and_then(Value::as_i64))
            .and_then(|n| i32::try_from(n).ok());
        let name = str_field(item, "name")
            .filter(|n| !n.is_empty())
            .unwrap_or("tool");
        let tool = ToolInteraction {
            name: preview(name, self.cap, self.cap).preview,
            call_id: str_field(item, "id").map(str::to_owned),
            input: state
                .get("input")
                .and_then(value_text)
                .map(|t| self.bounded(&t, &locator)),
            output: state
                .get("content")
                .and_then(content_text)
                .map(|t| self.bounded(&t, &locator)),
            status,
            exit_code,
            started_at: stamp("created").or_else(|| stamp("ran")),
            completed_at: stamp("completed"),
            orphan: false,
        };
        self.event(
            row,
            part,
            "assistant/tool".to_owned(),
            EventKind::Tool,
            Actor::Tool,
            None,
            Some(tool),
        )
    }
}

/// Anchor whose derived `Ord` (timestamp, source_order, stable_id) equals the SQL
/// order `(time_created, id, part)`: `source_order` is a constant 0 and `stable_id`
/// is the lowercase hex of the row id's bytes (byte order is preserved, any id is
/// representable), `#`, and the zero-padded part index. `#` sorts below every hex
/// digit, so an id that prefixes another still orders first, as in SQL.
fn anchor_of(e: &Event, row_id: &str) -> PageAnchor {
    let mut stable_id = String::with_capacity(row_id.len() * 2 + 11);
    for byte in row_id.bytes() {
        stable_id.push_str(&format!("{byte:02x}"));
    }
    stable_id.push_str(&format!("#{:010}", e.source_order));
    PageAnchor {
        timestamp: e.timestamp,
        source_order: 0,
        stable_id,
    }
}

/// Bytes an event contributes to the page budget: its bounded previews.
fn event_cost(e: &Event) -> usize {
    let len = |t: &Option<BoundedText>| t.as_ref().map_or(0, |b| b.preview.len());
    len(&e.text)
        + e.tool
            .as_ref()
            .map_or(0, |t| t.name.len() + len(&t.input) + len(&t.output))
}

fn event_truncated(e: &Event) -> bool {
    let cut = |t: &Option<BoundedText>| t.as_ref().is_some_and(|b| b.truncated);
    cut(&e.text)
        || e.tool
            .as_ref()
            .is_some_and(|t| cut(&t.input) || cut(&t.output))
}

fn push_diag(into: &mut Vec<DiagnosticEntry>, entry: DiagnosticEntry, truncated: &mut bool) {
    if into.len() < DIAG_CAP {
        into.push(entry);
    } else {
        *truncated = true;
    }
}

/// Splits an anchor into `(time_created, row id, part index)`.
fn anchor_parts(anchor: &PageAnchor) -> Result<(i64, String, usize), ProviderError> {
    let bad = || ProviderError::Unsupported("not an OpenCode anchor".into());
    let created = anchor.timestamp.ok_or_else(bad)?;
    let (hex, part) = anchor.stable_id.rsplit_once('#').ok_or_else(bad)?;
    if anchor.source_order != 0 || part.len() != 10 || hex.len() % 2 != 0 || !hex.is_ascii() {
        return Err(bad());
    }
    let bytes = (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16))
        .collect::<Result<Vec<u8>, _>>()
        .map_err(|_| bad())?;
    let id = String::from_utf8(bytes).map_err(|_| bad())?;
    Ok((created, id, part.parse().map_err(|_| bad())?))
}

impl OpenCodeScanner {
    fn set_busy_timeout(&self, millis: u64) -> Result<(), ProviderError> {
        match &self.connection {
            Some(conn) => conn
                .busy_timeout(Duration::from_millis(millis))
                .map_err(map_error),
            None => Ok(()),
        }
    }

    /// Opens (if needed) with the page timeout, runs `f`, and drops the connection
    /// on any failure that is not a plain refusal so the next call reopens.
    fn guarded<T>(
        &mut self,
        f: impl FnOnce(&Self, &Connection) -> Result<T, ProviderError>,
    ) -> Result<T, ProviderError> {
        let page_ms = self.config.page_busy_timeout_ms;
        let result = self
            .ensure_open()
            .and_then(|()| self.set_busy_timeout(page_ms))
            .and_then(|()| {
                let conn = self.connection.as_ref().expect("opened");
                f(self, conn)
            });
        if matches!(&result, Err(e) if !matches!(e, ProviderError::Unsupported(_))) {
            self.connection = None;
        }
        result
    }

    /// Validates a session key and that the transcript generation is readable.
    fn check_session(&self, conn: &Connection, key: &SessionKey) -> Result<(), ProviderError> {
        let Some(tables) = Tables::for_schema(self.schema).filter(|_| self.schema == Schema::V2)
        else {
            return Err(ProviderError::Unsupported(
                "OpenCode transcripts need the session_v2 generation".into(),
            ));
        };
        let valid = key.provider == Provider::OpenCode
            && matches!(&key.source, SourceKey::DbRow { table, id }
                if table == tables.session && *id == key.provider_session_id);
        if !valid {
            return Err(ProviderError::Unsupported("not an OpenCode session".into()));
        }
        let exists: i64 = conn
            .query_row(
                "SELECT count(*) FROM session_v2 WHERE id = ?1",
                params![key.provider_session_id],
                |r| r.get(0),
            )
            .map_err(map_error)?;
        if exists == 0 {
            return Err(ProviderError::Unavailable(
                "unknown OpenCode session".into(),
            ));
        }
        Ok(())
    }

    fn version_of(&self, key: &SessionKey) -> Option<&str> {
        self.known
            .get(&key.provider_session_id)
            .and_then(|s| s.provider_version.as_deref())
    }

    fn page(
        &self,
        conn: &Connection,
        key: &SessionKey,
        anchor: Option<&PageAnchor>,
        direction: PageDirection,
        request: PageRequest,
    ) -> Result<TranscriptPage, ProviderError> {
        self.check_session(conn, key)?;
        let at = anchor.map(anchor_parts).transpose()?;
        let record_cap = self.config.max_record_bytes;
        let mapper = Mapper {
            key,
            version: self.version_of(key),
            cap: (request.max_bytes / 4).clamp(1, PREVIEW_CAP),
        };
        let max_events = request.max_events.max(1);
        let forward = direction == PageDirection::Forward;
        let mut events: Vec<(Event, String)> = Vec::new();
        let mut diags = Vec::new();
        // Forward: diagnostics waiting for the next event. Backward: the page's own.
        // A diagnostic belongs to the page of the next event after it (or the last
        // page), so both directions partition diagnostics identically.
        let mut pending = Vec::new();
        let mut truncated = false;
        let mut bytes = 0usize;
        let mut has_more = false;
        stream_rows(
            conn,
            &key.provider_session_id,
            record_cap,
            direction,
            at.as_ref().map(|(created, id, _)| (*created, id.as_str())),
            |row| {
                let mut items = mapper.map_row(&row, record_cap);
                if !forward {
                    items.reverse();
                }
                for (part, mapped) in items {
                    // The anchor's own row resumes after (forward) / before
                    // (backward) the anchored part.
                    if let Some((created, id, at_part)) = &at
                        && row.created == *created
                        && row.id == *id
                        && (if forward {
                            part <= *at_part
                        } else {
                            part >= *at_part
                        })
                    {
                        continue;
                    }
                    let keep_diag = forward || anchor.is_none() || !events.is_empty();
                    match mapped {
                        Mapped::Diag(d) => {
                            if keep_diag {
                                let sink = if forward { &mut pending } else { &mut diags };
                                push_diag(sink, d, &mut truncated);
                            }
                        }
                        Mapped::Event(event) => {
                            let cost = event_cost(&event);
                            if !events.is_empty()
                                && (events.len() >= max_events || bytes + cost > request.max_bytes)
                            {
                                has_more = true;
                                return Ok(false);
                            }
                            if forward {
                                for d in pending.drain(..) {
                                    push_diag(&mut diags, d, &mut truncated);
                                }
                            }
                            bytes += cost;
                            events.push((*event, row.id.clone()));
                        }
                    }
                }
                Ok(true)
            },
        )?;
        if forward && !has_more {
            for d in pending {
                push_diag(&mut diags, d, &mut truncated);
            }
        }
        if !forward {
            events.reverse();
            diags.reverse();
        }
        truncated |= events.iter().any(|(e, _)| event_truncated(e));
        let next = if forward {
            events.last()
        } else {
            events.first()
        }
        .map(|(e, id)| anchor_of(e, id));
        Ok(TranscriptPage {
            events: events.into_iter().map(|(e, _)| e).collect(),
            diagnostics: diags,
            next,
            has_more,
            truncated,
        })
    }

    fn count_after(
        &self,
        conn: &Connection,
        key: &SessionKey,
        anchor: &PageAnchor,
    ) -> Result<usize, ProviderError> {
        self.check_session(conn, key)?;
        let (created, at_id, at_part) = anchor_parts(anchor)?;
        let record_cap = self.config.max_record_bytes;
        let mapper = Mapper {
            key,
            version: None,
            cap: 1,
        };
        let mut count = 0usize;
        stream_rows(
            conn,
            &key.provider_session_id,
            record_cap,
            PageDirection::Forward,
            Some((created, at_id.as_str())),
            |row| {
                let same = row.created == created && row.id == at_id;
                count += mapper
                    .map_row(&row, record_cap)
                    .iter()
                    .filter(|(part, mapped)| {
                        matches!(mapped, Mapped::Event(_)) && !(same && *part <= at_part)
                    })
                    .count();
                Ok(true)
            },
        )?;
        Ok(count)
    }

    fn record(
        &self,
        conn: &Connection,
        locator: &SourceLocator,
    ) -> Result<SourceRecord, ProviderError> {
        let LocatorKind::DbRow {
            table,
            row_id,
            part_index,
        } = &locator.kind
        else {
            return Err(ProviderError::Unsupported(
                "not an OpenCode row locator".into(),
            ));
        };
        if table != MESSAGE_TABLE {
            return Err(ProviderError::Unsupported(
                "locator names a table outside the OpenCode allowlist".into(),
            ));
        }
        self.check_session(conn, &locator.session)?;
        let session = &locator.session.provider_session_id;
        let record_cap = self.config.max_record_bytes;
        let cap = i64::try_from(record_cap).unwrap_or(i64::MAX);
        let found = conn
            .query_row(
                "SELECT session_id, time_created, time_updated, substr(type, 1, ?3), \
                 CASE WHEN length(CAST(data AS BLOB)) <= ?2 THEN data END, length(CAST(data AS BLOB)) \
                 FROM session_message WHERE id = ?1 AND length(id) <= ?4",
                params![
                    row_id,
                    cap,
                    i64::try_from(TYPE_CHARS).unwrap_or(0),
                    i64::try_from(ID_MAX).unwrap_or(i64::MAX)
                ],
                |r| {
                    let ty = lossy(r.get_ref(3)?).unwrap_or_default();
                    let data = lossy(r.get_ref(4)?);
                    Ok((
                        r.get::<_, String>(0)?,
                        RawRow {
                            id: row_id.clone(),
                            created: r.get(1)?,
                            updated: r.get(2)?,
                            ty,
                            data,
                            data_len: r.get(5)?,
                        },
                    ))
                },
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(map_error(other)),
            })?;
        let Some((owner, row)) = found else {
            return Err(ProviderError::Unavailable(
                "source row no longer exists".into(),
            ));
        };
        if owner != *session {
            return Err(ProviderError::Unsupported(
                "locator does not belong to this session".into(),
            ));
        }
        if locator.generation != Some(generation(&row)) {
            return Err(ProviderError::Unavailable(
                "stale source locator: the row was updated".into(),
            ));
        }
        let mapper = Mapper {
            key: &locator.session,
            version: None,
            cap: 1,
        };
        if let Some(part) = part_index
            && !mapper
                .map_row(&row, record_cap)
                .iter()
                .any(|(p, _)| p == part)
        {
            return Err(ProviderError::Unavailable(
                "locator names a part the record does not have".into(),
            ));
        }
        let Some(raw) = row.data else {
            let reason = if row.data_len.is_some() {
                format!("record exceeds the {record_cap}-byte limit")
            } else {
                "record has no data".to_owned()
            };
            return Ok(SourceRecord {
                raw: String::new(),
                display: String::new(),
                unavailable: Some(reason),
            });
        };
        let display = display_safe(&raw);
        Ok(SourceRecord {
            raw,
            display,
            unavailable: None,
        })
    }
}

impl ProviderReader for OpenCodeScanner {
    fn provider(&self) -> Provider {
        Provider::OpenCode
    }

    fn refresh(
        &mut self,
        now: Timestamp,
        budget: RefreshBudget,
    ) -> Result<ProviderDelta, ProviderError> {
        self.scan_roster(now, budget)
    }

    fn transcript_page(
        &mut self,
        key: &SessionKey,
        anchor: Option<PageAnchor>,
        direction: PageDirection,
        request: PageRequest,
    ) -> Result<TranscriptPage, ProviderError> {
        self.guarded(|this, conn| this.page(conn, key, anchor.as_ref(), direction, request))
    }

    fn count_events_after(
        &mut self,
        key: &SessionKey,
        anchor: &PageAnchor,
    ) -> Result<usize, ProviderError> {
        self.guarded(|this, conn| this.count_after(conn, key, anchor))
    }

    fn source_record(&mut self, locator: &SourceLocator) -> Result<SourceRecord, ProviderError> {
        self.guarded(|this, conn| this.record(conn, locator))
    }
}
