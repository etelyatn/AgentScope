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

use super::{ProviderDelta, ProviderError};
use crate::config::Config;
use crate::domain::{
    Lifecycle, Lineage, ModelDetail, ProcessAssociation, Provider, RefreshBudget, ScanProgress,
    SessionKey, SessionLink, SessionSummary, SourceKey, Timestamp, Warning, WarningKey,
    WarningKind,
};
use crate::ingest::text::preview;

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
        &["id", "session_id", "time_created", "time_updated"],
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
const ALLOWED_FUNCTIONS: &[&str] = &["count", "max", "min", "substr"];

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
        let result = self.ensure_open().and_then(|()| self.step(budget));
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
                let total: i64 = conn
                    .query_row(
                        &format!("SELECT count(*) FROM {}", tables.session),
                        [],
                        |r| r.get(0),
                    )
                    .map_err(map_error)?;
                let total = usize::try_from(total).unwrap_or(0);
                Pass {
                    cursor: None,
                    seen: HashSet::new(),
                    processed: 0,
                    cap: total.min(self.config.max_sessions),
                    total,
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
        Err(_) => short(raw).map(bare_model),
    }
}

fn bare_model(id: String) -> ModelDetail {
    ModelDetail {
        id,
        provider: None,
        variant: None,
    }
}

/// Per-column SQL bounds so one oversized value can never be materialised whole.
const ID_CHARS: usize = 512;
const TEXT_CHARS: usize = 1024;
const PATH_CHARS: usize = 4096;
const MODEL_CHARS: usize = 4096;
const VERSION_CHARS: usize = 128;

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
        format!("substr({}, 1, {ID_CHARS})", tables.fork)
    };
    let model = if tables.model == "NULL" {
        "NULL".to_owned()
    } else {
        format!("substr({}, 1, {MODEL_CHARS})", tables.model)
    };
    let sql = format!(
        "SELECT substr(s.id, 1, {ID_CHARS}), substr(s.title, 1, {TEXT_CHARS}), \
            substr(s.directory, 1, {PATH_CHARS}), {project_name}, \
            substr(s.parent_id, 1, {ID_CHARS}), \
            EXISTS(SELECT 1 FROM {t} x WHERE x.id = s.parent_id), {fork}, \
            EXISTS(SELECT 1 FROM {t} y WHERE y.id = {fork_raw}), {model}, \
            substr(s.version, 1, {VERSION_CHARS}), \
            s.time_created, s.time_updated, s.time_archived, \
            (SELECT max(m.time_updated) FROM {m} m WHERE m.session_id = s.id) \
         FROM {t} s {project_join} \
         WHERE s.time_updated < ?1 OR (s.time_updated = ?1 AND s.id < ?2) \
         ORDER BY s.time_updated DESC, s.id DESC LIMIT ?3",
        t = tables.session,
        m = tables.message,
        fork_raw = tables.fork,
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
