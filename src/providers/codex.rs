//! Codex rollout adapter: discovery-driven roster scanning and usage (spec §7.2).
//!
//! This module maps session metadata and usage only. Transcript paging, tool
//! mapping, and the [`ProviderReader`](super::ProviderReader) impl are separate.
//! Duplicate session IDs are never merged: every rollout file is its own row.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Instant, UNIX_EPOCH};

use chrono::DateTime;
use serde_json::Value;

use super::{ProviderDelta, ProviderError};
use crate::config::Config;
use crate::discovery::{SourceFile, discover_codex};
use crate::domain::{
    AggregationClass, CostEstimate, Lifecycle, Lineage, ProcessAssociation, Provider,
    RefreshBudget, ScanProgress, SessionKey, SessionLink, SessionSummary, SourceKey, Timestamp,
    TokenCategories, UsageSummary, Warning, WarningKey, WarningKind,
};
use crate::ingest::jsonl::{JsonlCursor, Record};
use crate::ingest::text::{preview, sanitize};
use crate::usage::{codex_total, estimate_cost};

/// Open file handles are bounded: only the most recently modified rollouts keep a
/// live cursor. Older files are re-read from the start if they ever change.
const KEEP_CURSORS: usize = 32;
const TITLE_COLS: usize = 80;
const TITLE_BYTES: usize = 1024;
const FIELD_CHARS: usize = 256;

/// Prompts Codex injects on the user's behalf; never used as a title.
const INJECTED_PREFIXES: &[&str] = &[
    "<environment_context",
    "<user_instructions",
    "<instructions",
    "<user_shell_command",
    "<turn_aborted",
    "<permissions",
    "<collaboration_mode",
    "<subagent_notification",
    "<skills_instructions",
    "<apps_instructions",
    "# agents.md",
];

static NULL: Value = Value::Null;

fn field<'a>(v: &'a Value, key: &str) -> &'a Value {
    v.get(key).unwrap_or(&NULL)
}

fn text_field(v: &Value, key: &str) -> Option<String> {
    let s = field(v, key).as_str()?;
    let clean: String = sanitize(s).chars().take(FIELD_CHARS).collect();
    (!clean.trim().is_empty()).then_some(clean)
}

/// Unbounded, unsanitized path text: paths must stay exact; only display text is bounded.
fn path_field(v: &Value, key: &str) -> Option<PathBuf> {
    let s = field(v, key).as_str()?;
    (!s.trim().is_empty()).then(|| PathBuf::from(s))
}

fn parse_ts(v: &Value) -> Option<Timestamp> {
    let parsed = DateTime::parse_from_rfc3339(v.as_str()?).ok()?;
    Some(parsed.timestamp_millis())
}

fn parse_usage(v: &Value) -> Option<TokenCategories> {
    let o = v.as_object()?;
    let get = |k: &str| o.get(k).and_then(Value::as_u64);
    let c = TokenCategories {
        input: get("input_tokens"),
        cached_input: get("cached_input_tokens"),
        cache_write: get("cache_write_input_tokens"),
        output: get("output_tokens"),
        reasoning: get("reasoning_output_tokens"),
        total: get("total_tokens"),
        ..TokenCategories::default()
    };
    (c.input.is_some() || c.output.is_some() || c.total.is_some()).then_some(c)
}

fn is_injected(text: &str) -> bool {
    let head = text.trim_start().to_ascii_lowercase();
    INJECTED_PREFIXES.iter().any(|p| head.starts_with(p))
}

fn message_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn title_from(text: &str) -> Option<String> {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let title = preview(&collapsed, TITLE_BYTES, TITLE_COLS).preview;
    (!title.trim().is_empty()).then_some(title)
}

/// Everything learned from one rollout file so far. Usage snapshots are
/// replaced, never accumulated: both Codex sources are cumulative.
#[derive(Default)]
struct Parsed {
    meta_seen: bool,
    id: Option<String>,
    cwd: Option<PathBuf>,
    version: Option<String>,
    parent: Option<String>,
    nickname: Option<String>,
    created_at: Option<Timestamp>,
    models: Vec<String>,
    prompt_title: Option<String>,
    record_usage: Option<TokenCategories>,
    count_usage: Option<TokenCategories>,
    context_window: Option<u64>,
    /// `Some(true)` after `task_started`, `Some(false)` after `task_complete`.
    turn_open: Option<bool>,
    last_completion: Option<Timestamp>,
    last_persisted: Option<Timestamp>,
    last_event: Option<Timestamp>,
    displayed: usize,
    hidden: usize,
    malformed: u32,
    oversized: u32,
    unknown: u32,
}

impl Parsed {
    fn display(&mut self, ts: Option<Timestamp>) {
        self.displayed += 1;
        self.last_event = self.last_event.max(ts);
    }

    fn window(&mut self, v: &Value) {
        if let Some(w) = field(v, "model_context_window").as_u64() {
            self.context_window = Some(w);
        }
    }

    fn apply(&mut self, value: &Value) {
        let ts = parse_ts(field(value, "timestamp"));
        self.last_persisted = self.last_persisted.max(ts);
        let payload = field(value, "payload");
        match field(value, "type").as_str() {
            Some("session_meta") => self.session_meta(payload, ts),
            Some("turn_context") => {
                self.hidden += 1;
                if let Some(model) = text_field(payload, "model") {
                    self.models.retain(|m| *m != model);
                    self.models.push(model);
                }
                if self.cwd.is_none() {
                    self.cwd = path_field(payload, "cwd");
                }
            }
            Some("response_item") => self.response_item(payload, ts),
            Some("event_msg") => self.event_msg(payload, ts),
            Some("token_usage_record") => {
                // Tolerate the record body at the top level as well as under `payload`.
                let body = if payload.is_null() { value } else { payload };
                self.usage_record(body);
            }
            Some("compacted") => self.display(ts),
            Some("world_state" | "additional_tools" | "inter_agent_communication_metadata") => {
                self.hidden += 1;
            }
            _ => {
                self.unknown += 1;
                self.hidden += 1;
            }
        }
    }

    fn session_meta(&mut self, p: &Value, ts: Option<Timestamp>) {
        self.hidden += 1;
        if self.meta_seen {
            return;
        }
        self.meta_seen = true;
        self.id = text_field(p, "id").or_else(|| text_field(p, "session_id"));
        if let Some(cwd) = path_field(p, "cwd") {
            self.cwd = Some(cwd);
        }
        self.version = text_field(p, "cli_version");
        self.created_at = parse_ts(field(p, "timestamp")).or(ts);
        let source = field(p, "source");
        let spawn = field(field(source, "subagent"), "thread_spawn");
        let subagent = field(source, "subagent");
        self.parent = [p, spawn, subagent, source]
            .iter()
            .find_map(|v| text_field(v, "parent_thread_id"));
        self.nickname = [p, spawn, subagent]
            .iter()
            .find_map(|v| text_field(v, "agent_nickname"));
    }

    fn response_item(&mut self, p: &Value, ts: Option<Timestamp>) {
        match field(p, "type").as_str() {
            Some("message") => {
                let role = field(p, "role").as_str();
                if role == Some("user") && self.prompt_title.is_none() {
                    let text = message_text(field(p, "content"));
                    if !is_injected(&text) {
                        self.prompt_title = title_from(&text);
                    }
                }
                self.display(ts);
            }
            Some("reasoning") => self.display(ts),
            // Raw call/output and other response items stay source-only.
            _ => self.hidden += 1,
        }
    }

    fn event_msg(&mut self, p: &Value, ts: Option<Timestamp>) {
        match field(p, "type").as_str() {
            Some("task_started") => {
                self.display(ts);
                self.turn_open = Some(true);
                self.window(p);
            }
            Some("task_complete") => {
                self.display(ts);
                self.turn_open = Some(false);
                self.last_completion = ts;
            }
            Some("item_completed") => self.display(ts),
            Some("token_count") => {
                self.hidden += 1;
                let info = field(p, "info");
                if let Some(total) = parse_usage(field(info, "total_token_usage")) {
                    self.count_usage = Some(total);
                }
                self.window(info);
            }
            Some("token_usage_record") => self.usage_record(p),
            _ => self.hidden += 1,
        }
    }

    fn usage_record(&mut self, body: &Value) {
        self.hidden += 1;
        if let Some(thread) = parse_usage(field(body, "thread_token_usage")) {
            self.record_usage = Some(thread);
        }
        self.window(body);
    }
}

struct FileState {
    source: SourceFile,
    cursor: Option<JsonlCursor>,
    parsed: Parsed,
    rotations_seen: u64,
    rotation_warnings: u32,
    reached_eof: bool,
    available: bool,
    /// Unavailable at the end of a completed pass; expires after the next one.
    retained: bool,
    dirty: bool,
    /// Key last sent to consumers, so an ID change can retire the old row.
    emitted: Option<SessionKey>,
    /// Duplicate-ID count and parent link as last emitted.
    context: (usize, Option<SessionLink>),
}

impl FileState {
    fn new(source: SourceFile) -> Self {
        Self {
            source,
            cursor: None,
            parsed: Parsed::default(),
            rotations_seen: 0,
            rotation_warnings: 0,
            reached_eof: false,
            available: true,
            retained: false,
            dirty: true,
            emitted: None,
            context: (1, None),
        }
    }

    fn restart(&mut self, source: SourceFile) {
        self.source = source;
        self.cursor = None;
        self.parsed = Parsed::default();
        self.rotations_seen = 0;
        self.reached_eof = false;
        self.dirty = true;
    }

    fn stem(&self) -> String {
        self.source
            .path
            .file_stem()
            .map(|s| sanitize(&s.to_string_lossy()))
            .unwrap_or_default()
    }

    fn id(&self) -> String {
        self.parsed.id.clone().unwrap_or_else(|| self.stem())
    }

    fn key(&self) -> SessionKey {
        SessionKey {
            provider: Provider::Codex,
            provider_session_id: self.id(),
            source: SourceKey::File(self.source.path.clone()),
        }
    }

    /// Record time when any record carried one, else the file's modified time.
    fn persisted_at(&self) -> Option<Timestamp> {
        self.parsed.last_persisted.or_else(|| {
            let since = self.source.modified.duration_since(UNIX_EPOCH).ok()?;
            i64::try_from(since.as_millis()).ok()
        })
    }
}

struct Pass {
    sources: Vec<SourceFile>,
    next: usize,
}

enum Advance {
    Done,
    Exhausted,
}

struct Meter {
    started: Instant,
    records: usize,
    bytes: usize,
    budget: RefreshBudget,
}

impl Meter {
    fn spent(&self) -> bool {
        self.records >= self.budget.max_records
            || self.bytes >= self.budget.max_bytes
            || self.started.elapsed().as_millis() >= u128::from(self.budget.max_millis)
    }
}

fn warn(path: &Path, label: &str, kind: WarningKind, message: String, count: u32) -> Warning {
    Warning {
        key: WarningKey {
            provider: Provider::Codex,
            source: format!("{}{label}", path.display()),
            kind,
        },
        message,
        count,
    }
}

pub struct CodexScanner {
    config: Config,
    states: HashMap<PathBuf, FileState>,
    /// Sources that stayed unreadable for a full refresh; probed silently until readable.
    expired: HashSet<PathBuf>,
    pass: Option<Pass>,
}

impl CodexScanner {
    pub fn new(config: &Config) -> Result<Self, ProviderError> {
        Ok(Self {
            config: config.clone(),
            states: HashMap::new(),
            expired: HashSet::new(),
            pass: None,
        })
    }

    /// Number of rollout files currently held open (bounded by `KEEP_CURSORS`).
    pub fn open_cursors(&self) -> usize {
        self.states.values().filter(|s| s.cursor.is_some()).count()
    }

    /// Reads as many new records as `budget` allows and returns the sessions that
    /// changed. `scan.done` is false while files remain; call again to continue.
    pub fn scan_roster(
        &mut self,
        _now: Timestamp,
        budget: RefreshBudget,
    ) -> Result<ProviderDelta, ProviderError> {
        let mut warnings = Vec::new();
        let mut pass = match self.pass.take() {
            Some(pass) => pass,
            None => {
                let found = discover_codex(&self.config)?;
                warnings = found.warnings;
                Pass {
                    sources: found.sources,
                    next: 0,
                }
            }
        };
        let mut meter = Meter {
            started: Instant::now(),
            records: 0,
            bytes: 0,
            budget,
        };
        while pass.next < pass.sources.len() {
            let source = pass.sources[pass.next].clone();
            let keep = pass.next < KEEP_CURSORS;
            match self.advance(source, keep, &mut meter) {
                Advance::Done => pass.next += 1,
                Advance::Exhausted => break,
            }
        }
        let done = pass.next >= pass.sources.len();
        let mut removed = Vec::new();
        if done {
            let live: HashSet<&Path> = pass.sources.iter().map(|s| s.path.as_path()).collect();
            self.expired.retain(|p| live.contains(p.as_path()));
            let mut gone: Vec<(PathBuf, &str, &str)> = Vec::new();
            for (path, state) in &mut self.states {
                if !live.contains(path.as_path()) {
                    gone.push((
                        path.clone(),
                        "#removed",
                        "session file is no longer present",
                    ));
                } else if !state.available && state.retained {
                    gone.push((
                        path.clone(),
                        "#expired",
                        "source stayed unreadable; row removed",
                    ));
                } else if !state.available {
                    state.retained = true;
                }
            }
            for (path, label, message) in gone {
                if let Some(state) = self.states.remove(&path) {
                    if label == "#expired" {
                        self.expired.insert(path.clone());
                    }
                    removed.extend(state.emitted.clone());
                    warnings.push(warn(
                        &path,
                        label,
                        WarningKind::Unavailable,
                        message.into(),
                        1,
                    ));
                }
            }
        }
        let scan = ScanProgress {
            provider: Provider::Codex,
            scanned: pass.next,
            total: Some(pass.sources.len()),
            done,
        };
        if !done {
            self.pass = Some(pass);
        }
        let (sessions, retired) = self.emit_changed();
        removed.extend(retired);
        Ok(ProviderDelta {
            sessions,
            removed,
            warnings,
            scan,
        })
    }

    fn advance(&mut self, source: SourceFile, keep: bool, meter: &mut Meter) -> Advance {
        let max_record_bytes = self.config.max_record_bytes;
        if self.expired.contains(&source.path) {
            if std::fs::File::open(&source.path).is_err() {
                return Advance::Done;
            }
            self.expired.remove(&source.path);
        }
        let state = self
            .states
            .entry(source.path.clone())
            .or_insert_with(|| FileState::new(source.clone()));
        if !keep && state.reached_eof {
            // Bound open handles to the newest sources even when nothing changed.
            state.cursor = None;
        }
        let unchanged = state.source.len == source.len && state.source.modified == source.modified;
        if state.reached_eof && unchanged && state.available {
            return Advance::Done;
        }
        if state.cursor.is_none() && state.reached_eof {
            // The cursor was dropped to bound open files; rebuild from the start.
            state.restart(source);
        } else {
            state.source = source;
        }
        if state.cursor.is_none() {
            match JsonlCursor::open(state.source.path.clone(), max_record_bytes) {
                Ok(cursor) => state.cursor = Some(cursor),
                Err(_) => {
                    state.dirty |= state.available;
                    state.available = false;
                    return Advance::Done;
                }
            }
        }
        loop {
            let Some(cursor) = state.cursor.as_mut() else {
                return Advance::Done;
            };
            let before = cursor.position();
            let rotations_before = cursor.rotation_count();
            let next = cursor.next_record();
            let after = cursor.position();
            let rotations = cursor.rotation_count();
            // A rotation restarts at 0, so the whole post-reset position is new data;
            // offsets alone cannot tell rotation from an append.
            let consumed = if rotations > rotations_before {
                after
            } else {
                after.saturating_sub(before)
            };
            meter.bytes = meter
                .bytes
                .saturating_add(usize::try_from(consumed).unwrap_or(usize::MAX));
            if rotations > state.rotations_seen {
                // The file was replaced or truncated: restart its parse state.
                state.parsed = Parsed::default();
                state.rotations_seen = rotations;
                state.rotation_warnings += 1;
                state.dirty = true;
            }
            let record = match next {
                Ok(Some(record)) => record,
                Ok(None) => {
                    state.reached_eof = true;
                    state.dirty |= !state.available;
                    state.available = true;
                    state.retained = false;
                    if !keep {
                        state.cursor = None;
                    }
                    return Advance::Done;
                }
                Err(_) => {
                    state.dirty |= state.available;
                    state.available = false;
                    // Reopening starts at byte 0, so discard what was parsed so far.
                    state.cursor = None;
                    state.parsed = Parsed::default();
                    state.rotations_seen = 0;
                    state.reached_eof = false;
                    return Advance::Done;
                }
            };
            state.reached_eof = false;
            state.dirty |= !state.available;
            state.available = true;
            state.retained = false;
            match record {
                Record::Json { ref value, .. } => state.parsed.apply(value),
                Record::Invalid { .. } => state.parsed.malformed += 1,
                Record::TooLarge { .. } => state.parsed.oversized += 1,
                // A partial trailing line is held by the cursor until it completes.
                Record::Incomplete { .. } => continue,
            }
            state.dirty = true;
            meter.records += 1;
            if meter.spent() {
                return Advance::Exhausted;
            }
        }
    }

    /// Refreshes duplicate-ID and parent-link context, then drains dirty sessions.
    fn emit_changed(&mut self) -> (Vec<SessionSummary>, Vec<SessionKey>) {
        let mut by_id: HashMap<String, Vec<PathBuf>> = HashMap::new();
        for (path, state) in &self.states {
            by_id.entry(state.id()).or_default().push(path.clone());
        }
        let contexts: Vec<(PathBuf, (usize, Option<SessionLink>))> = self
            .states
            .iter()
            .map(|(path, state)| {
                let dup = by_id.get(&state.id()).map_or(1, Vec::len);
                let parent = state
                    .parsed
                    .parent
                    .as_deref()
                    .map(|id| self.resolve_parent(id, path, &by_id));
                (path.clone(), (dup, parent))
            })
            .collect();
        for (path, context) in contexts {
            if let Some(state) = self.states.get_mut(&path)
                && state.context != context
            {
                state.context = context;
                state.dirty = true;
            }
        }
        let mut dirty: Vec<&PathBuf> = self
            .states
            .iter()
            .filter(|(_, s)| s.dirty)
            .map(|(p, _)| p)
            .collect();
        dirty.sort();
        let summaries: Vec<SessionSummary> = dirty
            .into_iter()
            .map(|p| self.summarize(&self.states[p]))
            .collect();
        let mut retired = Vec::new();
        for state in self.states.values_mut() {
            if state.dirty {
                let key = state.key();
                if let Some(old) = state.emitted.replace(key.clone())
                    && old != key
                {
                    retired.push(old);
                }
                state.dirty = false;
            }
        }
        (summaries, retired)
    }

    /// The most recently persisted source with that ID; a tie at the top is ambiguous.
    fn resolve_parent(
        &self,
        parent_id: &str,
        child: &Path,
        by_id: &HashMap<String, Vec<PathBuf>>,
    ) -> SessionLink {
        let candidates: Vec<&FileState> = by_id
            .get(parent_id)
            .into_iter()
            .flatten()
            .filter(|p| p.as_path() != child)
            .filter_map(|p| self.states.get(p))
            .collect();
        let Some(newest) = candidates.iter().map(|s| s.persisted_at()).max() else {
            return SessionLink::Unresolved(parent_id.to_owned());
        };
        let mut top = candidates.iter().filter(|s| s.persisted_at() == newest);
        match (top.next(), top.next()) {
            (Some(only), None) => SessionLink::Resolved(only.key()),
            _ => SessionLink::Ambiguous,
        }
    }

    fn summarize(&self, state: &FileState) -> SessionSummary {
        let p = &state.parsed;
        let path = &state.source.path;
        let id = state.id();
        let (title, title_derived) = match (&p.nickname, &p.prompt_title) {
            (Some(nick), _) => (title_from(nick).unwrap_or_else(|| id.clone()), false),
            (None, Some(prompt)) => (prompt.clone(), true),
            (None, None) => (id.clone(), false),
        };
        let (dup, parent) = state.context.clone();
        let mut warnings = Vec::new();
        let mut push = |label: &str, kind, message: &str, count: u32| {
            if count > 0 {
                warnings.push(warn(path, label, kind, message.to_owned(), count));
            }
        };
        push(
            "#malformed",
            WarningKind::Parse,
            "malformed records skipped",
            p.malformed,
        );
        push(
            "#oversized",
            WarningKind::Unsupported,
            "oversized records skipped",
            p.oversized,
        );
        push(
            "#unknown",
            WarningKind::Unsupported,
            "unknown record types",
            p.unknown,
        );
        push(
            "#rotation",
            WarningKind::Io,
            "source file was replaced or truncated",
            state.rotation_warnings,
        );
        push(
            "#missing-meta",
            WarningKind::Parse,
            "no session_meta record; using file name",
            u32::from(!p.meta_seen),
        );
        push(
            "#unavailable",
            WarningKind::Unavailable,
            "source unavailable",
            u32::from(!state.available),
        );
        if dup > 1 {
            push(
                "#duplicate-id",
                WarningKind::Unsupported,
                "Duplicate ID: another source shares this session ID",
                u32::try_from(dup).unwrap_or(u32::MAX),
            );
        }
        SessionSummary {
            key: state.key(),
            title,
            title_derived,
            project: p
                .cwd
                .as_deref()
                .and_then(Path::file_name)
                .map(|n| n.to_string_lossy().into_owned()),
            cwd: p.cwd.clone(),
            models: p.models.clone(),
            provider_version: p.version.clone(),
            created_at: p.created_at,
            last_event_at: p.last_event,
            last_persisted_at: state.persisted_at(),
            last_completion: p.last_completion,
            unfinished_work: p.turn_open == Some(true),
            displayed_event_count: p.displayed,
            hidden_records: p.hidden,
            source_available: state.available,
            context_window: p.context_window,
            server_tools: None,
            lifecycle: match p.turn_open {
                None => Lifecycle::Unknown,
                Some(true) => Lifecycle::Ongoing,
                Some(false) => Lifecycle::Completed,
            },
            process: ProcessAssociation::NotChecked,
            usage: self.usage_summary(p),
            lineage: Lineage {
                parent,
                fork: None,
                sidechain: false,
            },
            warnings,
        }
    }

    /// `token_usage_record.thread_token_usage` wins; `token_count` is only a
    /// fallback when no record exists. Snapshots are never added together.
    fn usage_summary(&self, p: &Parsed) -> Option<UsageSummary> {
        let categories = p.record_usage.clone().or_else(|| p.count_usage.clone())?;
        let model = p.models.last().cloned();
        let total = codex_total(&categories);
        let class = match (categories.total, total) {
            (Some(_), _) => AggregationClass::Reported,
            (None, Some(_)) => AggregationClass::Derived,
            (None, None) => AggregationClass::Unknown,
        };
        let cost = match &model {
            Some(model) => estimate_cost(Provider::Codex, model, &categories, &self.config.pricing),
            None => CostEstimate {
                usd: None,
                class: AggregationClass::Unknown,
                as_of: None,
                note: Some("no model recorded".into()),
            },
        };
        Some(UsageSummary {
            model,
            categories,
            total,
            class,
            cost,
        })
    }
}
