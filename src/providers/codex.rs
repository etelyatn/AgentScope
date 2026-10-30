//! Codex rollout adapter: discovery-driven roster scanning, usage, and transcripts
//! (spec §7.2).
//!
//! `event_msg.item_completed` is the single source of tool rows. Raw
//! `function_call`/`custom_tool_call` records and their outputs stay source-only
//! (diagnostics) unless a call never got an output (pending) or an output never had
//! a call (orphan). Pages are read straight from the file by byte offset: the
//! offset is the Codex source order and the basis of every [`PageAnchor`].
//! Duplicate session IDs are never merged: every rollout file is its own row.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Instant, UNIX_EPOCH};

use chrono::DateTime;
use serde_json::Value;

use super::{ProviderDelta, ProviderError, ProviderReader, SourceRecord};
use crate::config::Config;
use crate::discovery::{SourceFile, discover_codex};
use crate::domain::{
    Actor, AggregationClass, BoundedText, CostEstimate, DiagnosticEntry, Event, EventKind,
    Lifecycle, Lineage, LocatorKind, PageAnchor, PageDirection, PageRequest, ProcessAssociation,
    Provenance, Provider, RefreshBudget, ScanProgress, SessionKey, SessionLink, SessionSummary,
    SourceKey, SourceLocator, Timestamp, TokenCategories, ToolInteraction, ToolStatus,
    TranscriptPage, UsageSummary, Warning, WarningKey, WarningKind,
};
use crate::ingest::jsonl::{JsonlCursor, Record};
use crate::ingest::text::{display_safe, preview, sanitize};
use crate::usage::{codex_total, estimate_cost};

/// Unpaired calls and orphan outputs tracked per session (each).
const MAX_TRACKED: usize = 256;
/// Largest preview kept for one text field of a page event. `preview` is quadratic
/// in the kept length, and full content is one source-record load away.
const PREVIEW_CAP: usize = 2048;
/// Diagnostics returned (and buffered) per page.
const DIAG_CAP: usize = 200;
/// Backward reads walk the file in blocks of this size.
const BLOCK: usize = 64 * 1024;
const LABEL_CHARS: usize = 64;

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

/// How one persisted record is treated. Roster counting and transcript paging both
/// go through [`classify`], so displayed/hidden counts always agree with the pages.
enum Class<'a> {
    /// `response_item` message (payload).
    Message(&'a Value),
    /// `response_item` reasoning (payload).
    Reasoning(&'a Value),
    /// `compacted` (payload).
    Compaction(&'a Value),
    Turn {
        started: bool,
    },
    /// `event_msg.item_completed` tool item: the authoritative, single tool row.
    Tool(&'a Value),
    /// `response_item` function/custom call (payload); source-only unless unpaired.
    Call(&'a Value),
    /// `response_item` function/custom output (payload); source-only unless orphaned.
    Output(&'a Value),
    /// `item_completed` of a kind already rendered from `response_item` records.
    Duplicate(&'a str),
    /// Bookkeeping that never renders.
    Hidden(&'a str),
    Unknown(&'a str),
}

fn classify(value: &Value) -> Class<'_> {
    let payload = field(value, "payload");
    match field(value, "type").as_str() {
        Some("response_item") => match field(payload, "type").as_str() {
            Some("message") => Class::Message(payload),
            Some("reasoning") => Class::Reasoning(payload),
            Some("function_call" | "custom_tool_call") => Class::Call(payload),
            Some("function_call_output" | "custom_tool_call_output") => Class::Output(payload),
            Some(other) => Class::Hidden(other),
            None => Class::Hidden("response_item"),
        },
        Some("event_msg") => match field(payload, "type").as_str() {
            Some("task_started") => Class::Turn { started: true },
            Some("task_complete") => Class::Turn { started: false },
            Some("item_completed") => {
                let item = field(payload, "item");
                match field(item, "type").as_str() {
                    Some(
                        "CommandExecution"
                        | "FileChange"
                        | "McpToolCall"
                        | "Extension"
                        | "CollabAgentToolCall",
                    ) => Class::Tool(item),
                    Some(
                        kind @ ("UserMessage" | "AgentMessage" | "Reasoning" | "ContextCompaction"
                        | "SubAgentActivity"),
                    ) => Class::Duplicate(kind),
                    Some(other) => Class::Unknown(other),
                    None => Class::Unknown("item_completed"),
                }
            }
            Some(other) => Class::Hidden(other),
            None => Class::Hidden("event_msg"),
        },
        Some("compacted") => Class::Compaction(payload),
        Some(
            kind @ ("session_meta"
            | "turn_context"
            | "token_usage_record"
            | "world_state"
            | "additional_tools"
            | "inter_agent_communication_metadata"),
        ) => Class::Hidden(kind),
        Some(other) => Class::Unknown(other),
        None => Class::Unknown(""),
    }
}

/// Whether a record is a timeline row. Calls/outputs only render when unpaired.
fn is_displayed(class: &Class<'_>, offset: u64, shown: &BTreeSet<u64>) -> bool {
    match class {
        Class::Message(_)
        | Class::Reasoning(_)
        | Class::Compaction(_)
        | Class::Turn { .. }
        | Class::Tool(_) => true,
        Class::Call(_) | Class::Output(_) => shown.contains(&offset),
        Class::Duplicate(_) | Class::Hidden(_) | Class::Unknown(_) => false,
    }
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
    /// Byte offset just past the last complete record consumed. Transcript reads
    /// never go beyond it, so pages and counts agree with the roster.
    end: u64,
    /// Calls (by call ID) that have no output yet, mapped to their record offset.
    open_calls: HashMap<String, u64>,
    /// Offsets of unpaired calls and orphan outputs, which render as tool rows.
    /// Every offset here was also counted in `hidden` when it was read.
    shown: BTreeSet<u64>,
    /// Outputs with no call seen.
    orphans: u32,
    /// More calls were open than tracked, so later unmatched outputs are not orphans.
    pairing_overflow: bool,
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

    fn apply(&mut self, offset: u64, value: &Value) {
        let ts = parse_ts(field(value, "timestamp"));
        self.last_persisted = self.last_persisted.max(ts);
        let payload = field(value, "payload");
        match classify(value) {
            Class::Message(p) => {
                if field(p, "role").as_str() == Some("user") && self.prompt_title.is_none() {
                    let text = message_text(field(p, "content"));
                    if !is_injected(&text) {
                        self.prompt_title = title_from(&text);
                    }
                }
                self.display(ts);
            }
            Class::Reasoning(_) | Class::Compaction(_) | Class::Tool(_) => self.display(ts),
            Class::Turn { started } => {
                self.display(ts);
                if started {
                    self.turn_open = Some(true);
                    self.window(payload);
                } else {
                    self.turn_open = Some(false);
                    self.last_completion = ts;
                }
            }
            Class::Call(p) => self.call(offset, p, ts),
            Class::Output(p) => self.output(offset, p, ts),
            Class::Duplicate(_) | Class::Hidden(_) => self.hidden += 1,
            Class::Unknown(_) => {
                self.unknown += 1;
                self.hidden += 1;
            }
        }
        match field(value, "type").as_str() {
            Some("session_meta") => self.session_meta(payload, ts),
            Some("turn_context") => {
                if let Some(model) = text_field(payload, "model") {
                    self.models.retain(|m| *m != model);
                    self.models.push(model);
                }
                if self.cwd.is_none() {
                    self.cwd = path_field(payload, "cwd");
                }
            }
            Some("token_usage_record") => {
                // Tolerate the record body at the top level as well as under `payload`.
                let body = if payload.is_null() { value } else { payload };
                self.usage_record(body);
            }
            Some("event_msg") => match field(payload, "type").as_str() {
                Some("token_count") => {
                    let info = field(payload, "info");
                    if let Some(total) = parse_usage(field(info, "total_token_usage")) {
                        self.count_usage = Some(total);
                    }
                    self.window(info);
                }
                Some("token_usage_record") => self.usage_record(payload),
                _ => {}
            },
            _ => {}
        }
    }

    fn session_meta(&mut self, p: &Value, ts: Option<Timestamp>) {
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

    /// A call is source-only once its output arrives; until then it renders as a
    /// pending tool row. Pairing uses the call's own `call_id` and never the
    /// `item_completed` ID, which does not match in observed data.
    fn call(&mut self, offset: u64, p: &Value, ts: Option<Timestamp>) {
        self.hidden += 1;
        let Some(id) = text_field(p, "call_id") else {
            return;
        };
        if let Some(previous) = self.open_calls.remove(&id) {
            self.shown.remove(&previous);
        }
        if self.open_calls.len() >= MAX_TRACKED {
            self.pairing_overflow = true;
            return;
        }
        self.open_calls.insert(id, offset);
        self.shown.insert(offset);
        self.last_event = self.last_event.max(ts);
    }

    fn output(&mut self, offset: u64, p: &Value, ts: Option<Timestamp>) {
        self.hidden += 1;
        let Some(id) = text_field(p, "call_id") else {
            return;
        };
        if let Some(call) = self.open_calls.remove(&id) {
            self.shown.remove(&call);
        } else if !self.pairing_overflow && (self.orphans as usize) < MAX_TRACKED {
            self.orphans += 1;
            self.shown.insert(offset);
            self.last_event = self.last_event.max(ts);
        }
    }

    fn usage_record(&mut self, body: &Value) {
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
                // A partial trailing line is held by the cursor until it completes.
                Record::Incomplete { .. } => continue,
                Record::Json { offset, ref value } => state.parsed.apply(offset, value),
                Record::Invalid { .. } => state.parsed.malformed += 1,
                Record::TooLarge { .. } => state.parsed.oversized += 1,
            }
            state.parsed.end = after;
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
            "#orphan-tool",
            WarningKind::Parse,
            "tool results without a matching call",
            p.orphans,
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
            displayed_event_count: p.displayed + p.shown.len(),
            hidden_records: p.hidden.saturating_sub(p.shown.len()),
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

// ---------------------------------------------------------------------------
// Transcript reading
// ---------------------------------------------------------------------------

enum Body {
    Json(Value),
    Invalid,
    TooLarge,
}

struct Line {
    offset: u64,
    body: Body,
}

/// Same classification as `JsonlCursor`, so pages and the roster agree on what is
/// malformed or oversized.
fn parse_line(bytes: &[u8], max: usize) -> Body {
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    if bytes.len() > max {
        return Body::TooLarge;
    }
    serde_json::from_slice(bytes).map_or(Body::Invalid, Body::Json)
}

fn io_error(error: &io::Error) -> ProviderError {
    ProviderError::Io(error.kind().to_string())
}

/// Reads complete lines forward from `start` up to (never past) `end`. A line longer
/// than the record cap is consumed without being buffered.
struct ForwardLines {
    reader: BufReader<io::Take<File>>,
    offset: u64,
    max: usize,
}

impl ForwardLines {
    fn open(path: &Path, start: u64, end: u64, max: usize) -> io::Result<Self> {
        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(start))?;
        Ok(Self {
            reader: BufReader::new(file.take(end.saturating_sub(start))),
            offset: start,
            max,
        })
    }

    /// One line's start offset and bytes (`None` when oversized); `None` at the end.
    fn read(&mut self) -> io::Result<Option<(u64, Option<Vec<u8>>)>> {
        let start = self.offset;
        let limit = self.max.saturating_add(1);
        let mut buf = Vec::new();
        let mut over = false;
        let mut any = false;
        loop {
            let available = self.reader.fill_buf()?;
            if available.is_empty() {
                break;
            }
            any = true;
            let newline = available.iter().position(|&b| b == b'\n');
            let data = newline.unwrap_or(available.len());
            if !over {
                if buf.len() + data > limit {
                    over = true;
                    buf = Vec::new();
                } else {
                    buf.extend_from_slice(&available[..data]);
                }
            }
            let consumed = data + usize::from(newline.is_some());
            self.reader.consume(consumed);
            self.offset += consumed as u64;
            if newline.is_some() {
                break;
            }
        }
        Ok(any.then_some((start, (!over).then_some(buf))))
    }

    fn next_line(&mut self) -> io::Result<Option<Line>> {
        let max = self.max;
        Ok(self.read()?.map(|(offset, bytes)| Line {
            offset,
            body: bytes.map_or(Body::TooLarge, |b| parse_line(&b, max)),
        }))
    }
}

/// Reads complete lines backward (newest first) from `end`, in blocks. `end` is
/// normally a record boundary; a dangling partial line before it is discarded.
struct BackLines {
    file: File,
    pos: u64,
    max: usize,
    /// Tail of a line whose start is further left.
    carry: Vec<u8>,
    carry_len: u64,
    carry_over: bool,
    has_carry: bool,
    skip_next: bool,
    ready: VecDeque<Line>,
}

impl BackLines {
    fn open(path: &Path, end: u64, max: usize) -> io::Result<Self> {
        let mut file = File::open(path)?;
        let mut dangling = false;
        if end > 0 {
            file.seek(SeekFrom::Start(end - 1))?;
            let mut last = [0u8; 1];
            file.read_exact(&mut last)?;
            dangling = last[0] != b'\n';
        }
        Ok(Self {
            file,
            pos: end,
            max,
            carry: Vec::new(),
            carry_len: 0,
            carry_over: false,
            has_carry: dangling,
            skip_next: dangling,
            ready: VecDeque::new(),
        })
    }

    fn finish(&mut self, offset: u64, bytes: Option<Vec<u8>>) {
        if std::mem::take(&mut self.skip_next) {
            return;
        }
        let body = bytes.map_or(Body::TooLarge, |b| parse_line(&b, self.max));
        self.ready.push_back(Line { offset, body });
    }

    fn next_line(&mut self) -> io::Result<Option<Line>> {
        loop {
            if let Some(line) = self.ready.pop_front() {
                return Ok(Some(line));
            }
            if self.pos == 0 {
                if !self.has_carry {
                    return Ok(None);
                }
                self.has_carry = false;
                let bytes = (!self.carry_over).then(|| std::mem::take(&mut self.carry));
                self.carry_over = false;
                self.carry_len = 0;
                self.finish(0, bytes);
                continue;
            }
            self.read_block()?;
        }
    }

    fn read_block(&mut self) -> io::Result<()> {
        let n = BLOCK.min(usize::try_from(self.pos).unwrap_or(BLOCK));
        let start = self.pos - n as u64;
        let mut chunk = vec![0u8; n];
        self.file.seek(SeekFrom::Start(start))?;
        self.file.read_exact(&mut chunk)?;
        self.pos = start;
        let limit = self.max.saturating_add(1) as u64;
        let newlines: Vec<usize> = chunk
            .iter()
            .enumerate()
            .filter_map(|(i, &b)| (b == b'\n').then_some(i))
            .collect();
        let (Some(&first), Some(&last)) = (newlines.first(), newlines.last()) else {
            // No line start in this block: it only extends the carried line leftwards.
            self.carry_len += n as u64;
            if self.carry_over || self.carry_len > limit {
                self.carry_over = true;
                self.carry = Vec::new();
            } else {
                chunk.extend_from_slice(&self.carry);
                self.carry = chunk;
            }
            self.has_carry = true;
            return Ok(());
        };
        if self.has_carry {
            let tail = &chunk[last + 1..];
            let total = tail.len() as u64 + self.carry_len;
            let bytes = (!self.carry_over && total <= limit).then(|| {
                let mut joined = tail.to_vec();
                joined.extend_from_slice(&self.carry);
                joined
            });
            self.finish(start + last as u64 + 1, bytes);
        }
        for pair in newlines.windows(2).rev() {
            let line = &chunk[pair[0] + 1..pair[1]];
            let bytes = (line.len() as u64 <= limit).then(|| line.to_vec());
            self.finish(start + pair[0] as u64 + 1, bytes);
        }
        let head = &chunk[..first];
        self.carry_len = head.len() as u64;
        self.carry_over = self.carry_len > limit;
        self.carry = if self.carry_over {
            Vec::new()
        } else {
            head.to_vec()
        };
        self.has_carry = true;
        Ok(())
    }
}

fn label_text(s: &str) -> String {
    sanitize(s).chars().take(LABEL_CHARS).collect()
}

fn value_text(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => (!s.is_empty()).then(|| s.clone()),
        other => Some(other.to_string()),
    }
}

fn text_parts(parts: &[Value]) -> String {
    parts
        .iter()
        .filter_map(|p| p.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_message(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| match p.get("text").and_then(Value::as_str) {
                Some(text) => Some(text.to_owned()),
                None => p
                    .get("type")
                    .and_then(Value::as_str)
                    .filter(|t| t.contains("image"))
                    .map(|_| "[image]".to_owned()),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Only the persisted summary; `content`/`raw_content` stay in the source view.
fn reasoning_text(p: &Value) -> String {
    match field(p, "summary") {
        Value::Array(parts) => text_parts(parts),
        Value::String(s) => s.clone(),
        _ => String::new(),
    }
}

fn join_command(v: &Value) -> Option<String> {
    match v {
        Value::Array(parts) => {
            let joined = parts
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" ");
            (!joined.is_empty()).then_some(joined)
        }
        other => value_text(other),
    }
}

fn non_empty(parts: &[Option<String>]) -> Option<String> {
    let joined = parts
        .iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    (!joined.is_empty()).then_some(joined)
}

fn duration_ms(item: &Value) -> Option<i64> {
    let d = field(item, "duration");
    let secs = d.get("secs")?.as_i64()?;
    let nanos = d.get("nanos").and_then(Value::as_i64).unwrap_or(0);
    secs.checked_mul(1000)?.checked_add(nanos / 1_000_000)
}

fn tool_status(item: &Value, exit_code: Option<i32>, result_error: bool) -> ToolStatus {
    let status = field(item, "status")
        .as_str()
        .map(|s| s.to_ascii_lowercase().replace(['_', '-'], ""));
    let base = match status.as_deref() {
        Some("completed" | "complete" | "success" | "succeeded" | "ok") => ToolStatus::Completed,
        Some(
            "failed" | "error" | "declined" | "cancelled" | "canceled" | "aborted" | "rejected",
        ) => ToolStatus::Error,
        Some("inprogress" | "running" | "pending" | "started") => ToolStatus::Pending,
        _ => match exit_code {
            Some(0) => ToolStatus::Completed,
            Some(_) => ToolStatus::Error,
            None => ToolStatus::Unknown,
        },
    };
    // A nonzero exit code is the tool's failure, never the session's.
    if base == ToolStatus::Completed && (result_error || exit_code.is_some_and(|c| c != 0)) {
        ToolStatus::Error
    } else {
        base
    }
}

/// Raw strings of one tool item before they are bounded for display.
struct ToolParts {
    name: String,
    input: Option<String>,
    output: Option<String>,
    exit_code: Option<i32>,
    result_error: bool,
}

fn tool_parts(item: &Value) -> ToolParts {
    let exit_code = field(item, "exit_code")
        .as_i64()
        .and_then(|c| i32::try_from(c).ok());
    let mut parts = ToolParts {
        name: String::new(),
        input: None,
        output: None,
        exit_code,
        result_error: false,
    };
    match field(item, "type").as_str() {
        Some("CommandExecution") => {
            parts.name = "command".into();
            parts.input = join_command(field(item, "command"));
            parts.output = value_text(field(item, "aggregated_output"))
                .or_else(|| value_text(field(item, "formatted_output")))
                .or_else(|| {
                    non_empty(&[
                        value_text(field(item, "stdout")),
                        value_text(field(item, "stderr")),
                    ])
                });
        }
        Some("FileChange") => {
            parts.name = "file change".into();
            parts.input = match field(item, "changes") {
                Value::Object(changes) => {
                    let paths = changes.keys().cloned().collect::<Vec<_>>().join("\n");
                    (!paths.is_empty()).then_some(paths)
                }
                other => value_text(other),
            };
            parts.output = non_empty(&[
                value_text(field(item, "stdout")),
                value_text(field(item, "stderr")),
            ]);
        }
        Some("McpToolCall") => {
            let server = text_field(item, "server");
            let tool = text_field(item, "tool");
            parts.name = match (server, tool) {
                (Some(s), Some(t)) => format!("{s}/{t}"),
                (None, Some(t)) => t,
                (Some(s), None) => s,
                (None, None) => "mcp tool".into(),
            };
            parts.input = value_text(field(item, "arguments"));
            let result = field(item, "result");
            parts.result_error = field(result, "isError").as_bool() == Some(true);
            parts.output = match field(result, "content") {
                Value::Array(content) if !text_parts(content).is_empty() => {
                    Some(text_parts(content))
                }
                _ => value_text(result),
            };
        }
        Some("Extension") => {
            parts.name = text_field(item, "kind").unwrap_or_else(|| "extension".into());
            parts.input =
                value_text(field(item, "query")).or_else(|| value_text(field(item, "action")));
            parts.output = value_text(field(item, "results"));
        }
        _ => {
            parts.name = match text_field(item, "tool") {
                Some(tool) => format!("agent {tool}"),
                None => "agent call".into(),
            };
        }
    }
    parts
}

struct Mapper<'a> {
    key: &'a SessionKey,
    path: &'a Path,
    version: Option<&'a str>,
    shown: &'a BTreeSet<u64>,
    cap: usize,
}

enum Mapped {
    Event(Box<Event>),
    Diag(DiagnosticEntry),
}

impl Mapper<'_> {
    fn locator(&self, offset: u64) -> SourceLocator {
        SourceLocator {
            session: self.key.clone(),
            kind: LocatorKind::FileOffset {
                path: self.path.to_path_buf(),
                offset,
            },
        }
    }

    /// Sanitized, redacted, size-capped preview that never buffers more than a
    /// small multiple of the cap before the regexes run.
    fn bounded(&self, text: &str, locator: &SourceLocator) -> BoundedText {
        let mut cut = text.len().min(self.cap.saturating_mul(4).max(1024));
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        let mut bounded = preview(&text[..cut], self.cap, self.cap);
        bounded.byte_len = text.len();
        bounded.truncated |= cut < text.len();
        bounded.locator = Some(locator.clone());
        bounded
    }

    fn bounded_opt(&self, text: Option<String>, locator: &SourceLocator) -> Option<BoundedText> {
        text.filter(|t| !t.is_empty())
            .map(|t| self.bounded(&t, locator))
    }

    fn map(&self, line: &Line) -> Mapped {
        let offset = line.offset;
        let locator = self.locator(offset);
        let diag = |label: String, hidden: bool| {
            Mapped::Diag(DiagnosticEntry {
                locator: locator.clone(),
                label,
                hidden,
            })
        };
        let value = match &line.body {
            Body::TooLarge => return diag("oversized record".into(), true),
            Body::Invalid => return diag("malformed record".into(), true),
            Body::Json(value) => value,
        };
        let ts = parse_ts(field(value, "timestamp"));
        let event = |kind: EventKind,
                     actor: Actor,
                     text: Option<BoundedText>,
                     tool: Option<ToolInteraction>| {
            let top = field(value, "type").as_str().unwrap_or("");
            let record_type = match field(field(value, "payload"), "type").as_str() {
                Some(inner) => format!("{}/{}", label_text(top), label_text(inner)),
                None => label_text(top),
            };
            Mapped::Event(Box::new(Event {
                id: format!("codex:{offset}"),
                session: self.key.clone(),
                timestamp: ts,
                source_order: offset,
                kind,
                actor,
                text,
                tool,
                usage: None,
                provenance: Provenance {
                    provider: Provider::Codex,
                    source: SourceKey::File(self.path.to_path_buf()),
                    locator: locator.clone(),
                    record_type,
                    provider_version: self.version.map(str::to_owned),
                },
            }))
        };
        match classify(value) {
            Class::Message(p) => {
                let role = field(p, "role").as_str().unwrap_or("");
                let raw = render_message(field(p, "content"));
                let injected = role == "user" && is_injected(&raw);
                let (kind, actor) = match role {
                    "user" if !injected => (EventKind::UserPrompt, Actor::User),
                    "assistant" => (EventKind::AssistantMessage, Actor::Assistant),
                    _ => (EventKind::System, Actor::System),
                };
                event(kind, actor, self.bounded_opt(Some(raw), &locator), None)
            }
            Class::Reasoning(p) => event(
                EventKind::Reasoning,
                Actor::Assistant,
                self.bounded_opt(Some(reasoning_text(p)), &locator),
                None,
            ),
            Class::Compaction(p) => event(
                EventKind::Compaction,
                Actor::System,
                self.bounded_opt(value_text(field(p, "message")), &locator),
                None,
            ),
            Class::Turn { started } => event(
                EventKind::Lifecycle,
                Actor::System,
                Some(self.bounded(
                    if started {
                        "Turn started"
                    } else {
                        "Turn completed"
                    },
                    &locator,
                )),
                None,
            ),
            Class::Tool(item) => {
                let tool = self.tool_item(item, field(value, "payload"), ts, &locator);
                event(EventKind::Tool, Actor::Tool, None, Some(tool))
            }
            Class::Call(p) if self.shown.contains(&offset) => {
                let tool = ToolInteraction {
                    name: text_field(p, "name").unwrap_or_else(|| "tool call".into()),
                    call_id: text_field(p, "call_id"),
                    input: self.bounded_opt(
                        value_text(field(p, "arguments")).or_else(|| value_text(field(p, "input"))),
                        &locator,
                    ),
                    output: None,
                    status: ToolStatus::Pending,
                    exit_code: None,
                    started_at: ts,
                    completed_at: None,
                    orphan: false,
                };
                event(EventKind::Tool, Actor::Tool, None, Some(tool))
            }
            Class::Output(p) if self.shown.contains(&offset) => {
                let tool = ToolInteraction {
                    name: "tool result".into(),
                    call_id: text_field(p, "call_id"),
                    input: None,
                    output: self.bounded_opt(value_text(field(p, "output")), &locator),
                    status: ToolStatus::Unknown,
                    exit_code: None,
                    started_at: None,
                    completed_at: ts,
                    orphan: true,
                };
                event(EventKind::Tool, Actor::Tool, None, Some(tool))
            }
            Class::Call(p) | Class::Output(p) => diag(
                format!(
                    "source-only: {}",
                    label_text(field(p, "type").as_str().unwrap_or("tool record"))
                ),
                false,
            ),
            Class::Duplicate(kind) => diag(format!("duplicate item: {}", label_text(kind)), true),
            Class::Hidden(kind) => diag(format!("hidden: {}", label_text(kind)), true),
            Class::Unknown(kind) => diag(format!("unknown record: {}", label_text(kind)), true),
        }
    }

    fn tool_item(
        &self,
        item: &Value,
        payload: &Value,
        ts: Option<Timestamp>,
        locator: &SourceLocator,
    ) -> ToolInteraction {
        let parts = tool_parts(item);
        let status = tool_status(item, parts.exit_code, parts.result_error);
        let duration = duration_ms(item);
        let mut completed_at = field(payload, "completed_at_ms").as_i64().or(ts);
        let mut started_at = field(payload, "started_at_ms").as_i64();
        if started_at.is_none() {
            started_at = completed_at
                .zip(duration)
                .and_then(|(done, ms)| done.checked_sub(ms));
        }
        if status == ToolStatus::Pending {
            completed_at = None;
        }
        ToolInteraction {
            name: label_text(&parts.name),
            call_id: text_field(item, "id"),
            input: self.bounded_opt(parts.input, locator),
            output: self.bounded_opt(parts.output, locator),
            status,
            exit_code: parts.exit_code,
            started_at,
            completed_at,
            orphan: false,
        }
    }
}

fn anchor_of(e: &Event) -> PageAnchor {
    PageAnchor {
        timestamp: e.timestamp,
        source_order: e.source_order,
        stable_id: e.id.clone(),
    }
}

/// Bytes a row contributes to the page budget: its bounded previews.
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

#[derive(Clone, Copy)]
struct Limits {
    max_events: usize,
    max_bytes: usize,
    max_record: usize,
}

impl Limits {
    fn full(&self, events: &[Event], bytes: usize, next: &Event) -> bool {
        !events.is_empty()
            && (events.len() >= self.max_events || bytes + event_cost(next) > self.max_bytes)
    }
}

fn empty_page() -> TranscriptPage {
    TranscriptPage {
        events: Vec::new(),
        diagnostics: Vec::new(),
        next: None,
        has_more: false,
        truncated: false,
    }
}

/// Events strictly after `anchor` (or from the start), oldest first. A diagnostic
/// record belongs to the page of the next event after it, or to the last page.
fn forward_page(
    mapper: &Mapper<'_>,
    anchor: Option<&PageAnchor>,
    end: u64,
    limits: Limits,
) -> io::Result<TranscriptPage> {
    let start = anchor.map_or(0, |a| a.source_order);
    if start >= end {
        return Ok(empty_page());
    }
    let mut lines = ForwardLines::open(mapper.path, start, end, limits.max_record)?;
    if anchor.is_some() {
        lines.read()?;
    }
    let mut events: Vec<Event> = Vec::new();
    let mut diags = Vec::new();
    let mut trailing = Vec::new();
    let mut truncated = false;
    let mut bytes = 0usize;
    let mut has_more = false;
    while let Some(line) = lines.next_line()? {
        match mapper.map(&line) {
            Mapped::Diag(entry) => push_diag(&mut trailing, entry, &mut truncated),
            Mapped::Event(event) => {
                if limits.full(&events, bytes, &event) {
                    has_more = true;
                    break;
                }
                for entry in trailing.drain(..) {
                    push_diag(&mut diags, entry, &mut truncated);
                }
                bytes += event_cost(&event);
                events.push(*event);
            }
        }
    }
    if !has_more {
        for entry in trailing {
            push_diag(&mut diags, entry, &mut truncated);
        }
    }
    truncated |= events.iter().any(event_truncated);
    Ok(TranscriptPage {
        next: events.last().map(anchor_of),
        events,
        diagnostics: diags,
        has_more,
        truncated,
    })
}

/// The newest events strictly before `anchor` (or the end), returned oldest first.
fn backward_page(
    mapper: &Mapper<'_>,
    anchor: Option<&PageAnchor>,
    end: u64,
    limits: Limits,
) -> io::Result<TranscriptPage> {
    let before = anchor.map_or(end, |a| a.source_order.min(end));
    let mut lines = BackLines::open(mapper.path, before, limits.max_record)?;
    let mut events: Vec<Event> = Vec::new();
    let mut diags = Vec::new();
    let mut truncated = false;
    let mut bytes = 0usize;
    let mut has_more = false;
    while let Some(line) = lines.next_line()? {
        match mapper.map(&line) {
            // Records between the anchor and the page's newest event belong to the
            // anchor's page; with no anchor there is no later event to claim them.
            Mapped::Diag(entry) => {
                if anchor.is_none() || !events.is_empty() {
                    push_diag(&mut diags, entry, &mut truncated);
                }
            }
            Mapped::Event(event) => {
                if limits.full(&events, bytes, &event) {
                    has_more = true;
                    break;
                }
                bytes += event_cost(&event);
                events.push(*event);
            }
        }
    }
    events.reverse();
    diags.reverse();
    truncated |= events.iter().any(event_truncated);
    Ok(TranscriptPage {
        next: events.first().map(anchor_of),
        events,
        diagnostics: diags,
        has_more,
        truncated,
    })
}

impl CodexScanner {
    fn session(&self, key: &SessionKey) -> Result<&FileState, ProviderError> {
        let SourceKey::File(path) = &key.source else {
            return Err(ProviderError::Unsupported(
                "not a Codex file session".into(),
            ));
        };
        if key.provider != Provider::Codex {
            return Err(ProviderError::Unsupported("not a Codex session".into()));
        }
        let state = self
            .states
            .get(path)
            .filter(|s| s.key() == *key)
            .ok_or_else(|| ProviderError::Unavailable("unknown Codex session".into()))?;
        if !state.available {
            return Err(ProviderError::Unavailable("source unavailable".into()));
        }
        Ok(state)
    }

    /// End of the data the roster has consumed, provided the file still holds it.
    fn readable_end(state: &FileState) -> Result<u64, ProviderError> {
        let len = fs::metadata(&state.source.path)
            .map_err(|e| io_error(&e))?
            .len();
        if len < state.parsed.end {
            return Err(ProviderError::Unavailable(
                "source changed since the last refresh".into(),
            ));
        }
        Ok(state.parsed.end)
    }
}

impl ProviderReader for CodexScanner {
    fn provider(&self) -> Provider {
        Provider::Codex
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
        let state = self.session(key)?;
        let end = Self::readable_end(state)?;
        let mapper = Mapper {
            key,
            path: &state.source.path,
            version: state.parsed.version.as_deref(),
            shown: &state.parsed.shown,
            cap: request.max_bytes.clamp(1, PREVIEW_CAP),
        };
        let limits = Limits {
            max_events: request.max_events.max(1),
            max_bytes: request.max_bytes,
            max_record: self.config.max_record_bytes,
        };
        match direction {
            PageDirection::Forward => forward_page(&mapper, anchor.as_ref(), end, limits),
            PageDirection::Backward => backward_page(&mapper, anchor.as_ref(), end, limits),
        }
        .map_err(|e| io_error(&e))
    }

    fn count_events_after(
        &mut self,
        key: &SessionKey,
        anchor: &PageAnchor,
    ) -> Result<usize, ProviderError> {
        let state = self.session(key)?;
        let end = Self::readable_end(state)?;
        if anchor.source_order >= end {
            return Ok(0);
        }
        let max = self.config.max_record_bytes;
        let count = || -> io::Result<usize> {
            let mut lines = ForwardLines::open(&state.source.path, anchor.source_order, end, max)?;
            lines.read()?;
            let mut count = 0;
            while let Some(line) = lines.next_line()? {
                if let Body::Json(value) = &line.body
                    && is_displayed(&classify(value), line.offset, &state.parsed.shown)
                {
                    count += 1;
                }
            }
            Ok(count)
        };
        count().map_err(|e| io_error(&e))
    }

    fn source_record(&mut self, locator: &SourceLocator) -> Result<SourceRecord, ProviderError> {
        let LocatorKind::FileOffset { path, offset } = &locator.kind else {
            return Err(ProviderError::Unsupported(
                "not a Codex file locator".into(),
            ));
        };
        let state = self.session(&locator.session)?;
        if *path != state.source.path {
            return Err(ProviderError::Unsupported(
                "locator does not belong to this session source".into(),
            ));
        }
        let end = Self::readable_end(state)?;
        if *offset >= end {
            return Err(ProviderError::Unavailable(
                "record is beyond the scanned data".into(),
            ));
        }
        let max = self.config.max_record_bytes;
        let read = || -> io::Result<Option<Option<Vec<u8>>>> {
            if *offset > 0 {
                let mut file = File::open(path)?;
                file.seek(SeekFrom::Start(*offset - 1))?;
                let mut previous = [0u8; 1];
                file.read_exact(&mut previous)?;
                if previous[0] != b'\n' {
                    return Ok(None);
                }
            }
            Ok(ForwardLines::open(path, *offset, end, max)?
                .read()?
                .map(|(_, bytes)| bytes))
        };
        let bytes = match read().map_err(|e| io_error(&e))? {
            None => {
                return Err(ProviderError::Parse(
                    "locator is not a record boundary".into(),
                ));
            }
            Some(bytes) => bytes,
        };
        let Some(mut bytes) = bytes else {
            return Ok(SourceRecord {
                raw: String::new(),
                display: String::new(),
                unavailable: Some(format!("record exceeds the {max}-byte limit")),
            });
        };
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        let raw = String::from_utf8_lossy(&bytes).into_owned();
        let display = display_safe(&raw);
        Ok(SourceRecord {
            raw,
            display,
            unavailable: None,
        })
    }
}
