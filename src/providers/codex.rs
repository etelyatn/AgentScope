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
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

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
use crate::ingest::jsonl::{ContentHash, JsonlCursor, Record};
use crate::ingest::text::{display_safe, preview, sanitize};
use crate::usage::{codex_total, estimate_cost};

/// Unpaired calls and orphan outputs tracked per session (each).
const MAX_TRACKED: usize = 256;
/// Superseded call IDs remembered so their late outputs are not orphans.
const MAX_CLOSED: usize = 1024;
/// Records indexed per page/count request, and the most entries (24 bytes each) one
/// session's transcript index may retain.
const INDEX_STEP: usize = 250_000;
const INDEX_CAP: usize = 2_000_000;
/// Consumed prefixes up to this size are re-hashed exactly; larger ones are sampled.
const PREFIX_VERIFY_MAX: u64 = 32 << 20;
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);
/// Largest preview kept for one text field of a page event. `preview` is quadratic
/// in the kept length, and full content is one source-record load away.
const PREVIEW_CAP: usize = 2048;
/// Diagnostics returned (and buffered) per page.
const DIAG_CAP: usize = 200;
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

struct Call {
    id: String,
    offset: u64,
    open: bool,
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
    /// Calls no completion has claimed yet, oldest first. `open` calls have no output.
    calls: VecDeque<Call>,
    /// Offsets of unpaired calls and orphan outputs, which render as tool rows.
    /// Every offset here was also counted in `hidden` when it was read.
    shown: BTreeSet<u64>,
    /// Outputs with no call seen.
    orphans: u32,
    /// More calls were open than tracked, so later unmatched outputs are not orphans.
    /// More calls were open than tracked, so later unmatched outputs are not orphans.
    pairing_overflow: bool,
    /// Calls superseded by an `item_completed`; their late output is not an orphan.
    closed: HashSet<String>,
    /// Unique per parse restart; keys the transcript index cache.
    generation: u64,
    /// `(end, hash)` of every consumed byte, from the cursor's running hash.
    prefix: Option<(u64, u64)>,
    /// File identity captured when the scan first stamped this parse.
    identity: Option<FileId>,
    /// Length and modified time observed at the last stamp.
    stamp_meta: Option<(u64, Option<SystemTime>)>,
    /// Sampled `(end, hash)`, only kept for files too large to re-hash.
    fingerprint: Option<(u64, u64)>,
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

    fn tool_completed(&mut self, item: &Value, ts: Option<Timestamp>) {
        self.display(ts);
        if !item_pending(item) {
            self.claim_oldest_call();
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
            Class::Reasoning(_) | Class::Compaction(_) => self.display(ts),
            Class::Tool(item) => self.tool_completed(item, ts),
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
        if let Some(at) = self.calls.iter().position(|c| c.id == id)
            && let Some(previous) = self.calls.remove(at)
        {
            self.shown.remove(&previous.offset);
        }
        while self.calls.len() >= MAX_TRACKED && self.calls.front().is_some_and(|c| !c.open) {
            self.calls.pop_front();
        }
        if self.calls.len() >= MAX_TRACKED {
            self.pairing_overflow = true;
            return;
        }
        self.calls.push_back(Call {
            id,
            offset,
            open: true,
        });
        self.shown.insert(offset);
        self.last_event = self.last_event.max(ts);
    }

    fn output(&mut self, offset: u64, p: &Value, ts: Option<Timestamp>) {
        self.hidden += 1;
        let Some(id) = text_field(p, "call_id") else {
            return;
        };
        if let Some(call) = self.calls.iter_mut().find(|c| c.id == id) {
            // Resolved by its own output. It keeps its place in the queue until an
            // `item_completed` claims it, so completions stay aligned with calls.
            if call.open {
                call.open = false;
                self.shown.remove(&call.offset);
            }
        } else if self.closed.remove(&id) {
            // The authoritative completion already replaced this call; stay source-only.
        } else if !self.pairing_overflow && (self.orphans as usize) < MAX_TRACKED {
            self.orphans += 1;
            self.shown.insert(offset);
            self.last_event = self.last_event.max(ts);
        }
    }

    /// An authoritative `item_completed` tool row claims the oldest call no earlier
    /// completion has claimed. Calls already resolved by their raw output are still
    /// claimed (they just have no pending row to retire), so a completion never
    /// consumes a different call that is genuinely still open. IDs never join
    /// (spec 6.3), so completion order is the only evidence.
    fn claim_oldest_call(&mut self) {
        let Some(call) = self.calls.pop_front() else {
            return;
        };
        if !call.open {
            return;
        }
        self.shown.remove(&call.offset);
        if self.closed.len() >= MAX_CLOSED {
            self.pairing_overflow = true;
        } else {
            self.closed.insert(call.id);
        }
    }

    fn set_end(&mut self, end: u64, hash: Option<u64>) {
        if self.generation == 0 {
            self.generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
        }
        self.end = end;
        self.prefix = hash.map(|h| (end, h));
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
    /// Already-parsed content was rewritten or rotated; consumed by the next delta.
    invalidated: bool,
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
            invalidated: false,
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

    /// Records what the scan consumed (metadata, identity, and for very large files
    /// sampled bytes) so later reads can tell the scanned file from a replacement.
    fn stamp(&mut self) {
        let Ok(meta) = fs::metadata(&self.source.path) else {
            return;
        };
        let p = &mut self.parsed;
        p.stamp_meta = Some((meta.len(), meta.modified().ok()));
        if p.identity.is_none() {
            p.identity = Some(file_id(&meta));
        }
        if p.end > PREFIX_VERIFY_MAX && p.fingerprint.map(|f| f.0) != Some(p.end) {
            p.fingerprint = fingerprint(&self.source.path, p.end).map(|hash| (p.end, hash));
        }
    }

    /// Whether the file on disk still holds the bytes this parse consumed.
    fn source_intact(&self) -> bool {
        let p = &self.parsed;
        if p.end == 0 {
            return true;
        }
        let (Some((end, hash)), Some(identity)) = (p.prefix, p.identity) else {
            return false;
        };
        let Ok(meta) = fs::metadata(&self.source.path) else {
            return false;
        };
        if file_id(&meta) != identity || meta.len() < end {
            return false;
        }
        if p.stamp_meta == Some((meta.len(), meta.modified().ok())) {
            return true;
        }
        // Appended or rewritten since the scan: the consumed prefix must be unchanged.
        if end <= PREFIX_VERIFY_MAX {
            prefix_hash(&self.source.path, end) == Some(hash)
        } else {
            p.fingerprint
                .is_some_and(|(e, h)| e == end && fingerprint(&self.source.path, end) == Some(h))
        }
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
    /// Page index of the one session being inspected.
    index: Option<Index>,
    index_limits: IndexLimits,
}

impl CodexScanner {
    pub fn new(config: &Config) -> Result<Self, ProviderError> {
        Ok(Self {
            config: config.clone(),
            states: HashMap::new(),
            expired: HashSet::new(),
            pass: None,
            index: None,
            index_limits: IndexLimits {
                step: INDEX_STEP,
                cap: INDEX_CAP,
            },
        })
    }

    /// Overrides how many records one page/count request indexes and the most one
    /// session may hold. Beyond the cap, transcript requests fail with a clear error.
    pub fn set_index_limits(&mut self, step_records: usize, max_entries: usize) {
        self.index_limits = IndexLimits {
            step: step_records.max(1),
            cap: max_entries,
        };
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
        let (sessions, retired, invalidated) = self.emit_changed();
        removed.extend(retired);
        Ok(ProviderDelta {
            sessions,
            removed,
            invalidated,
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
            // A pure append keeps the consumed prefix intact; a rewrite does not,
            // and loaded pages built from the old bytes are then stale.
            let rewritten = !state.source_intact();
            state.restart(source);
            state.invalidated |= rewritten;
        } else {
            state.source = source;
        }
        if state.cursor.is_some() && !state.source_intact() {
            // The bytes already consumed changed (replaced or rewritten, possibly
            // then grown): nothing parsed from them can be trusted, so start over.
            state.cursor = None;
            state.parsed = Parsed::default();
            state.rotations_seen = 0;
            state.rotation_warnings += 1;
            state.reached_eof = false;
            state.dirty = true;
            state.invalidated = true;
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
                state.invalidated = true;
            }
            let record = match next {
                Ok(Some(record)) => record,
                Ok(None) => {
                    state.reached_eof = true;
                    state.stamp();
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
            let hash = state.cursor.as_ref().map(JsonlCursor::content_hash);
            state.parsed.set_end(after, hash);
            state.dirty = true;
            meter.records += 1;
            if meter.spent() {
                state.stamp();
                return Advance::Exhausted;
            }
        }
    }

    /// Refreshes duplicate-ID and parent-link context, then drains dirty sessions.
    fn emit_changed(&mut self) -> (Vec<SessionSummary>, Vec<SessionKey>, Vec<SessionKey>) {
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
        let mut invalidated = Vec::new();
        for state in self.states.values_mut() {
            if state.dirty {
                let key = state.key();
                if std::mem::take(&mut state.invalidated) {
                    invalidated.push(key.clone());
                }
                if let Some(old) = state.emitted.replace(key.clone())
                    && old != key
                {
                    retired.push(old);
                }
                state.dirty = false;
            }
        }
        (summaries, retired, invalidated)
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
            model_details: vec![],
            provider_version: p.version.clone(),
            created_at: p.created_at,
            last_event_at: p.last_event,
            last_persisted_at: state.persisted_at(),
            last_completion: p.last_completion,
            unfinished_work: p.turn_open == Some(true),
            displayed_event_count: p.displayed + p.shown.len(),
            hidden_records: p.hidden.saturating_sub(p.shown.len()),
            source_available: state.available,
            archived: false,
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
struct ForwardLines<R: Read> {
    reader: BufReader<io::Take<R>>,
    offset: u64,
    max: usize,
}

impl ForwardLines<File> {
    fn open(path: &Path, start: u64, end: u64, max: usize) -> io::Result<Self> {
        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(start))?;
        Ok(Self::new(file, start, end, max))
    }
}

impl<R: Read> ForwardLines<R> {
    /// `source` must already be positioned at `start`.
    fn new(source: R, start: u64, end: u64, max: usize) -> Self {
        Self {
            reader: BufReader::new(source.take(end.saturating_sub(start))),
            offset: start,
            max,
        }
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

fn item_pending(item: &Value) -> bool {
    let status = field(item, "status")
        .as_str()
        .map(|s| s.to_ascii_lowercase().replace(['_', '-'], ""));
    matches!(
        status.as_deref(),
        Some("inprogress" | "running" | "pending" | "started")
    )
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
    generation: u64,
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
            generation: Some(self.generation),
            kind: LocatorKind::FileOffset {
                path: self.path.to_path_buf(),
                offset,
            },
        }
    }

    /// Sanitized, redacted, then cut. Redaction sees the whole value (already bounded
    /// by the record cap), so a secret longer than the preview cap is never left
    /// half-masked by a cut that removed its closing quote.
    fn bounded(&self, text: &str, locator: &SourceLocator) -> BoundedText {
        let mut bounded = preview(text, self.cap, self.cap);
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
}

impl Limits {
    fn full(&self, events: &[Event], bytes: usize, next: &Event) -> bool {
        !events.is_empty()
            && (events.len() >= self.max_events || bytes + event_cost(next) > self.max_bytes)
    }
}

/// Shrinks one row so the whole interaction (name, input, output, text) fits
/// `budget` bytes. Previews are already redacted, so cutting them is safe. Returns
/// whether anything was cut; the tool name has no flag of its own, so the caller
/// marks the page truncated.
fn fit(event: &mut Event, budget: usize) -> bool {
    let mut fields: Vec<(&mut String, Option<&mut bool>)> = Vec::new();
    if let Some(text) = event.text.as_mut() {
        fields.push((&mut text.preview, Some(&mut text.truncated)));
    }
    if let Some(tool) = event.tool.as_mut() {
        fields.push((&mut tool.name, None));
        for part in [tool.input.as_mut(), tool.output.as_mut()]
            .into_iter()
            .flatten()
        {
            fields.push((&mut part.preview, Some(&mut part.truncated)));
        }
    }
    let mut cut_any = false;
    let mut left = budget;
    let mut remaining = fields.len();
    for (text, flag) in fields {
        let share = left / remaining;
        remaining -= 1;
        if text.len() > share {
            let mut cut = share;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            cut_any = true;
            if let Some(flag) = flag {
                *flag = true;
            }
        }
        left -= text.len();
    }
    cut_any
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum IxKind {
    /// Always a timeline row.
    Row,
    /// Raw call/output: a row only while unpaired (see `Parsed::shown`).
    Pairable,
    /// Hidden, unknown, malformed, or oversized: diagnostics only.
    Diag,
}

#[derive(Clone, Copy)]
struct Ix {
    ts: Option<Timestamp>,
    offset: u64,
    kind: IxKind,
}

impl Ix {
    /// The `PageAnchor` total order; the stable ID (`codex:<offset>`) is a function
    /// of the offset, so it never breaks a tie the offset has not.
    fn key(&self) -> (Option<Timestamp>, u64) {
        (self.ts, self.offset)
    }

    fn displayed(&self, shown: &BTreeSet<u64>) -> bool {
        match self.kind {
            IxKind::Row => true,
            IxKind::Pairable => shown.contains(&self.offset),
            IxKind::Diag => false,
        }
    }
}

/// Records of one session in `PageAnchor` order. Built lazily for the session being
/// inspected (one slot), a bounded number of records per request, extended on
/// append, rebuilt on replacement.
struct Index {
    path: PathBuf,
    generation: u64,
    end: u64,
    entries: Vec<Ix>,
    /// The session has more records than the cap allows; no entries are kept.
    over: bool,
}

#[derive(Clone, Copy)]
struct IndexLimits {
    /// Records indexed per page/count request.
    step: usize,
    /// Most entries one session may hold.
    cap: usize,
}

fn ensure_index<'a>(
    slot: &'a mut Option<Index>,
    state: &FileState,
    max: usize,
    limits: IndexLimits,
) -> Result<&'a Index, ProviderError> {
    let parsed = &state.parsed;
    let stale = slot.as_ref().is_none_or(|i| {
        i.path != state.source.path || i.generation != parsed.generation || i.end > parsed.end
    });
    if stale {
        *slot = Some(Index {
            path: state.source.path.clone(),
            generation: parsed.generation,
            end: 0,
            entries: Vec::new(),
            over: false,
        });
    }
    let too_large = || {
        ProviderError::Unsupported(format!(
            "session transcript exceeds the {}-record index limit",
            limits.cap
        ))
    };
    let index = slot.as_mut().expect("index slot was just filled");
    if index.over {
        return Err(too_large());
    }
    if index.end < parsed.end {
        let mut lines = ForwardLines::open(&index.path, index.end, parsed.end, max)
            .map_err(|e| io_error(&e))?;
        let mut fresh: Vec<Ix> = Vec::new();
        let mut reached_end = false;
        for _ in 0..limits.step {
            let Some(line) = lines.next_line().map_err(|e| io_error(&e))? else {
                reached_end = true;
                break;
            };
            let (ts, kind) = match &line.body {
                Body::Json(value) => (
                    parse_ts(field(value, "timestamp")),
                    match classify(value) {
                        Class::Message(_)
                        | Class::Reasoning(_)
                        | Class::Compaction(_)
                        | Class::Turn { .. }
                        | Class::Tool(_) => IxKind::Row,
                        Class::Call(_) | Class::Output(_) => IxKind::Pairable,
                        Class::Duplicate(_) | Class::Hidden(_) | Class::Unknown(_) => IxKind::Diag,
                    },
                ),
                Body::Invalid | Body::TooLarge => (None, IxKind::Diag),
            };
            fresh.push(Ix {
                ts,
                offset: line.offset,
                kind,
            });
        }
        index.end = if reached_end {
            parsed.end
        } else {
            lines.offset
        };
        if index.entries.len() + fresh.len() > limits.cap {
            index.entries = Vec::new();
            index.over = true;
            return Err(too_large());
        }
        let ordered = fresh.windows(2).all(|w| w[0].key() <= w[1].key())
            && index
                .entries
                .last()
                .zip(fresh.first())
                .is_none_or(|(a, b)| a.key() <= b.key());
        index.entries.extend(fresh);
        if !ordered {
            index.entries.sort_unstable_by_key(Ix::key);
        }
        if index.end < parsed.end {
            return Err(ProviderError::Unavailable(format!(
                "transcript index is building ({} of {} bytes); retry",
                index.end, parsed.end
            )));
        }
    }
    Ok(index)
}

impl Index {
    /// Position of the entry an anchor names; `None` for an anchor from another
    /// generation of the file (or a record that no longer exists).
    fn position(&self, anchor: &PageAnchor) -> Option<usize> {
        let key = (anchor.timestamp, anchor.source_order);
        let i = self.entries.partition_point(|e| e.key() < key);
        let entry = self.entries.get(i)?;
        (entry.key() == key && anchor.stable_id == format!("codex:{}", entry.offset)).then_some(i)
    }
}

/// Random access to records by offset, never past the scanned end.
struct Reader {
    file: File,
    end: u64,
    max: usize,
}

impl Reader {
    fn line(&mut self, offset: u64) -> io::Result<Line> {
        (&self.file).seek(SeekFrom::Start(offset))?;
        ForwardLines::new(&self.file, offset, self.end, self.max)
            .next_line()?
            .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))
    }
}

/// Pages in `PageAnchor` order (timestamp, offset, stable ID). Forward returns the
/// rows after the anchor; backward the rows before it, oldest first. A diagnostic
/// record belongs to the page of the next row after it in that order, or to the
/// last page, so both directions partition diagnostics identically.
fn page(
    mapper: &Mapper<'_>,
    index: &Index,
    reader: &mut Reader,
    anchor: Option<&PageAnchor>,
    direction: PageDirection,
    limits: Limits,
) -> io::Result<TranscriptPage> {
    let at = match anchor {
        Some(a) => match index.position(a) {
            Some(i) => Some(i),
            None => return Ok(empty_page()),
        },
        None => None,
    };
    let len = index.entries.len();
    let forward = direction == PageDirection::Forward;
    let order: Box<dyn Iterator<Item = usize>> = if forward {
        Box::new(at.map_or(0, |i| i + 1)..len)
    } else {
        Box::new((0..at.unwrap_or(len)).rev())
    };
    let mut events: Vec<Event> = Vec::new();
    let mut diags = Vec::new();
    // Forward: diagnostics waiting for the next row. Backward: the page's own.
    let mut pending = Vec::new();
    let mut truncated = false;
    let mut bytes = 0usize;
    let mut has_more = false;
    for i in order {
        let entry = index.entries[i];
        let keep_diag = forward || anchor.is_none() || !events.is_empty();
        if entry.kind == IxKind::Diag {
            let sink = if forward { &pending } else { &diags };
            if !keep_diag {
                continue;
            }
            if sink.len() >= DIAG_CAP {
                truncated = true;
                continue;
            }
        }
        let line = reader.line(entry.offset)?;
        match mapper.map(&line) {
            Mapped::Diag(d) => {
                if !keep_diag {
                    continue;
                }
                push_diag(
                    if forward { &mut pending } else { &mut diags },
                    d,
                    &mut truncated,
                );
            }
            Mapped::Event(event) => {
                let mut event = *event;
                let cut = fit(&mut event, limits.max_bytes);
                if limits.full(&events, bytes, &event) {
                    has_more = true;
                    break;
                }
                if forward {
                    for d in pending.drain(..) {
                        push_diag(&mut diags, d, &mut truncated);
                    }
                }
                bytes += event_cost(&event);
                truncated |= cut;
                events.push(event);
            }
        }
    }
    if forward && !has_more {
        for d in pending {
            push_diag(&mut diags, d, &mut truncated);
        }
    }
    if !forward {
        events.reverse();
        diags.reverse();
    }
    truncated |= events.iter().any(event_truncated);
    let next = if forward {
        events.last()
    } else {
        events.first()
    }
    .map(anchor_of);
    Ok(TranscriptPage {
        events,
        diagnostics: diags,
        next,
        has_more,
        truncated,
    })
}

#[cfg(unix)]
type FileId = (u64, u64);
#[cfg(windows)]
type FileId = u64;
#[cfg(not(any(unix, windows)))]
type FileId = ();

#[cfg(unix)]
fn file_id(meta: &fs::Metadata) -> FileId {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

#[cfg(windows)]
fn file_id(meta: &fs::Metadata) -> FileId {
    use std::os::windows::fs::MetadataExt;
    meta.creation_time()
}

#[cfg(not(any(unix, windows)))]
fn file_id(_meta: &fs::Metadata) -> FileId {}

/// Exact hash of the first `end` bytes, in the same FNV stream the cursor keeps.
fn prefix_hash(path: &Path, end: u64) -> Option<u64> {
    let mut file = File::open(path).ok()?;
    let mut hash = ContentHash::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut left = end;
    while left > 0 {
        let want = usize::try_from(left.min(buf.len() as u64)).ok()?;
        file.read_exact(&mut buf[..want]).ok()?;
        hash.update(&buf[..want]);
        left -= want as u64;
    }
    Some(hash.finish())
}

/// Hash of three samples of the consumed bytes plus their length. A file replaced
/// by different content no longer matches; a replacement that only differs between
/// the samples is a documented false negative.
fn fingerprint(path: &Path, end: u64) -> Option<u64> {
    let mut file = File::open(path).ok()?;
    if file.metadata().ok()?.len() < end {
        return None;
    }
    let mut hasher = DefaultHasher::new();
    end.hash(&mut hasher);
    let len = end.min(512);
    for start in [0, (end / 2).min(end - len), end - len] {
        file.seek(SeekFrom::Start(start)).ok()?;
        let mut buf = vec![0u8; usize::try_from(len).ok()?];
        file.read_exact(&mut buf).ok()?;
        buf.hash(&mut hasher);
    }
    Some(hasher.finish())
}

fn find<'a>(
    states: &'a HashMap<PathBuf, FileState>,
    key: &SessionKey,
) -> Result<&'a FileState, ProviderError> {
    let SourceKey::File(path) = &key.source else {
        return Err(ProviderError::Unsupported(
            "not a Codex file session".into(),
        ));
    };
    if key.provider != Provider::Codex {
        return Err(ProviderError::Unsupported("not a Codex session".into()));
    }
    let state = states
        .get(path)
        .filter(|s| s.key() == *key)
        .ok_or_else(|| ProviderError::Unavailable("unknown Codex session".into()))?;
    if !state.available {
        return Err(ProviderError::Unavailable("source unavailable".into()));
    }
    Ok(state)
}

/// End of the data the roster consumed, provided the file on disk still holds the
/// bytes that were scanned. A replaced or rewritten file must not be read through
/// the old offsets.
fn verified_end(state: &FileState) -> Result<u64, ProviderError> {
    if state.source_intact() {
        Ok(state.parsed.end)
    } else {
        Err(ProviderError::Unavailable(
            "source changed since the last refresh".into(),
        ))
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
        let state = find(&self.states, key)?;
        let end = verified_end(state)?;
        let max = self.config.max_record_bytes;
        let index = ensure_index(&mut self.index, state, max, self.index_limits)?;
        let mapper = Mapper {
            key,
            path: &state.source.path,
            generation: state.parsed.generation,
            version: state.parsed.version.as_deref(),
            shown: &state.parsed.shown,
            cap: request.max_bytes.clamp(1, PREVIEW_CAP),
        };
        let limits = Limits {
            max_events: request.max_events.max(1),
            max_bytes: request.max_bytes,
        };
        let mut reader = Reader {
            file: File::open(&state.source.path).map_err(|e| io_error(&e))?,
            end,
            max,
        };
        page(
            &mapper,
            index,
            &mut reader,
            anchor.as_ref(),
            direction,
            limits,
        )
        .map_err(|e| io_error(&e))
    }

    fn count_events_after(
        &mut self,
        key: &SessionKey,
        anchor: &PageAnchor,
    ) -> Result<usize, ProviderError> {
        let state = find(&self.states, key)?;
        verified_end(state)?;
        let max = self.config.max_record_bytes;
        let index = ensure_index(&mut self.index, state, max, self.index_limits)?;
        let key = (anchor.timestamp, anchor.source_order);
        let from = index.entries.partition_point(|e| e.key() <= key);
        Ok(index.entries[from..]
            .iter()
            .filter(|e| e.displayed(&state.parsed.shown))
            .count())
    }

    fn source_record(&mut self, locator: &SourceLocator) -> Result<SourceRecord, ProviderError> {
        let LocatorKind::FileOffset { path, offset } = &locator.kind else {
            return Err(ProviderError::Unsupported(
                "not a Codex file locator".into(),
            ));
        };
        let state = find(&self.states, &locator.session)?;
        if locator.generation != Some(state.parsed.generation) {
            return Err(ProviderError::Unavailable("stale source locator".into()));
        }
        if *path != state.source.path {
            return Err(ProviderError::Unsupported(
                "locator does not belong to this session source".into(),
            ));
        }
        let end = verified_end(state)?;
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
        let Some(bytes) = read().map_err(|e| io_error(&e))? else {
            return Err(ProviderError::Parse(
                "locator is not a record boundary".into(),
            ));
        };
        // Same rule as `parse_line`: the cap applies after the CRLF terminator is gone.
        let bytes = bytes.map(|mut b| {
            if b.last() == Some(&b'\r') {
                b.pop();
            }
            b
        });
        let Some(bytes) = bytes.filter(|b| b.len() <= max) else {
            return Ok(SourceRecord {
                raw: String::new(),
                display: String::new(),
                unavailable: Some(format!("record exceeds the {max}-byte limit")),
            });
        };
        let raw = String::from_utf8_lossy(&bytes).into_owned();
        let display = display_safe(&raw);
        Ok(SourceRecord {
            raw,
            display,
            unavailable: None,
        })
    }
}
