//! Claude project discovery, roster scanning and persisted content mapping.
//! Usage aggregation is independent from transcript paging.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use chrono::DateTime;
use serde_json::Value;

use crate::config::Config;
use crate::discovery::SourceFile;
pub use crate::discovery::discover_claude;
use crate::domain::*;
use crate::ingest::jsonl::{JsonlCursor, Record};
use crate::ingest::text::preview;
use crate::providers::{ProviderDelta, ProviderError};
use crate::usage::ClaudeLedger;

fn safe(s: &str) -> BoundedText {
    preview(s, 2048, 2048)
}
fn title(s: &str) -> Option<String> {
    let clean = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let result = preview(&clean, 1024, 80).preview;
    (!result.trim().is_empty()).then_some(result)
}
fn injected(s: &str) -> bool {
    let s = s.trim_start().to_ascii_lowercase();
    [
        "<environment_context",
        "<user_instructions",
        "<instructions",
        "<system-reminder",
        "<command-name",
        "<local-command",
        "<user_shell_command",
        "# agents.md",
    ]
    .iter()
    .any(|p| s.starts_with(p))
}
fn timestamp(v: &Value) -> Option<Timestamp> {
    DateTime::parse_from_rfc3339(v.as_str()?)
        .ok()
        .map(|v| v.timestamp_millis())
}
fn string(v: &Value, key: &str) -> Option<String> {
    v.get(key)?
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
}
fn usage(message: &Value, provenance: &Provenance) -> Option<UsageObservation> {
    let u = message.get("usage")?.as_object()?;
    let get = |name: &str| u.get(name).and_then(Value::as_u64);
    let split = u.get("cache_creation");
    Some(UsageObservation {
        model: string(message, "model").map(|v| safe(&v).preview),
        scope: UsageScope::Message,
        categories: TokenCategories {
            input: get("input_tokens"),
            cache_write: get("cache_creation_input_tokens"),
            cache_read: get("cache_read_input_tokens"),
            output: get("output_tokens"),
            cache_write_5m: split
                .and_then(|v| v.get("ephemeral_5m_input_tokens"))
                .and_then(Value::as_u64),
            cache_write_1h: split
                .and_then(|v| v.get("ephemeral_1h_input_tokens"))
                .and_then(Value::as_u64),
            reasoning: u
                .get("output_tokens_details")
                .and_then(|v| v.get("thinking_tokens"))
                .and_then(Value::as_u64),
            ..Default::default()
        },
        provenance: provenance.clone(),
    })
}
fn locator(key: &SessionKey, offset: u64) -> SourceLocator {
    let path = match &key.source {
        SourceKey::File(path) => path.clone(),
        _ => PathBuf::new(),
    };
    SourceLocator {
        session: key.clone(),
        kind: LocatorKind::FileOffset { path, offset },
    }
}
fn content_text(v: &Value) -> (String, usize) {
    match v {
        Value::String(s) => (s.clone(), 0),
        Value::Array(a) => {
            let mut hidden = 0;
            let text = a
                .iter()
                .filter_map(|p| match p.get("type").and_then(Value::as_str) {
                    Some("text") => p.get("text").and_then(Value::as_str),
                    Some("image") => Some("[image]"),
                    _ => {
                        hidden += 1;
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            (text, hidden)
        }
        _ => (String::new(), 0),
    }
}

/// Mapping result, with auxiliary records counted rather than displayed.
pub struct ClaudeTranscript {
    pub events: Vec<Event>,
    pub hidden_records: usize,
}

/// Map records with their original JSONL byte offsets.
pub fn map_transcript(key: &SessionKey, records: &[(u64, Value)]) -> ClaudeTranscript {
    let indexed: Vec<_> = records
        .iter()
        .map(|(offset, value)| (*offset, value))
        .collect();
    map_indexed(key, &indexed)
}

fn map_indexed(key: &SessionKey, records: &[(u64, &Value)]) -> ClaudeTranscript {
    let mut events: Vec<Event> = Vec::new();
    let mut calls: HashMap<String, usize> = HashMap::new();
    let mut hidden = 0;
    let mut latest = HashMap::new();
    for (offset, record) in records {
        if let Some(uuid) = record.get("uuid").and_then(Value::as_str) {
            let count = match &record["message"]["content"] {
                Value::Array(parts) => parts.len(),
                Value::String(_) => 1,
                _ => 0,
            };
            for index in 0..count {
                latest.insert((uuid, index), *offset);
            }
        }
    }
    let mut observed_usage = HashSet::new();
    for (offset, record) in records {
        let uuid = record.get("uuid").and_then(Value::as_str);
        let first_event = events.len();
        let kind = record.get("type").and_then(Value::as_str).unwrap_or("");
        let loc = locator(key, *offset);
        let prov = Provenance {
            provider: Provider::Claude,
            source: key.source.clone(),
            locator: loc.clone(),
            record_type: kind.into(),
            provider_version: string(record, "version").map(|v| safe(&v).preview),
        };
        let ts = record.get("timestamp").and_then(timestamp);
        let message = &record["message"];
        let assistant_record = kind == "assistant";
        let emit = |events: &mut Vec<Event>,
                    index: usize,
                    kind: EventKind,
                    actor: Actor,
                    body: Option<BoundedText>,
                    tool: Option<ToolInteraction>| {
            events.push(Event {
                id: format!(
                    "{}:{}:{}",
                    record
                        .get("uuid")
                        .and_then(Value::as_str)
                        .unwrap_or("record"),
                    offset,
                    index
                ),
                session: key.clone(),
                timestamp: ts,
                source_order: *offset,
                kind,
                actor,
                text: body,
                tool,
                usage: None,
                provenance: prov.clone(),
            });
        };
        if kind == "system" {
            let body = record
                .get("content")
                .and_then(Value::as_str)
                .or_else(|| record.get("subtype").and_then(Value::as_str))
                .unwrap_or("system");
            emit(
                &mut events,
                0,
                EventKind::System,
                Actor::System,
                Some(safe(body)),
                None,
            );
            continue;
        }
        if kind != "user" && kind != "assistant" {
            hidden += 1;
            continue;
        }
        let content = &message["content"];
        let text_part;
        let parts: &[Value] = match content {
            Value::Array(parts) => parts,
            Value::String(s) => {
                text_part = serde_json::json!({"type":"text","text":s});
                std::slice::from_ref(&text_part)
            }
            _ => &[],
        };
        for (index, part) in parts.iter().enumerate() {
            if uuid.is_some_and(|uuid| latest.get(&(uuid, index)) != Some(offset)) {
                continue;
            }
            let typ = part.get("type").and_then(Value::as_str).unwrap_or("");
            match typ {
                "text" => {
                    let text = part.get("text").and_then(Value::as_str).unwrap_or("");
                    emit(
                        &mut events,
                        index,
                        if kind == "user" {
                            EventKind::UserPrompt
                        } else {
                            EventKind::AssistantMessage
                        },
                        if kind == "user" {
                            Actor::User
                        } else {
                            Actor::Assistant
                        },
                        Some(safe(text)),
                        None,
                    );
                }
                "thinking" if kind == "assistant" => emit(
                    &mut events,
                    index,
                    EventKind::Reasoning,
                    Actor::Assistant,
                    Some(safe(
                        part.get("thinking").and_then(Value::as_str).unwrap_or(""),
                    )),
                    None,
                ),
                "image" => emit(
                    &mut events,
                    index,
                    if kind == "user" {
                        EventKind::UserPrompt
                    } else {
                        EventKind::AssistantMessage
                    },
                    if kind == "user" {
                        Actor::User
                    } else {
                        Actor::Assistant
                    },
                    Some(safe("[image]")),
                    None,
                ),
                "tool_use" if kind == "assistant" => {
                    let id = string(part, "id");
                    let tool = ToolInteraction {
                        name: safe(part.get("name").and_then(Value::as_str).unwrap_or("tool"))
                            .preview,
                        call_id: id.clone(),
                        input: part.get("input").map(|v| safe(&v.to_string())),
                        output: None,
                        status: ToolStatus::Pending,
                        exit_code: None,
                        started_at: ts,
                        completed_at: None,
                        orphan: false,
                    };
                    if let Some(id) = id {
                        calls.insert(id, events.len());
                    }
                    emit(
                        &mut events,
                        index,
                        EventKind::Tool,
                        Actor::Tool,
                        None,
                        Some(tool),
                    );
                }
                "tool_result" if kind == "user" => {
                    let id = string(part, "tool_use_id");
                    let (content, unknown) = content_text(&part["content"]);
                    hidden += unknown;
                    let output = safe(&content);
                    let status = if part.get("is_error").and_then(Value::as_bool) == Some(true) {
                        ToolStatus::Error
                    } else {
                        ToolStatus::Completed
                    };
                    if let Some(call) = id.as_ref().and_then(|id| calls.remove(id)) {
                        let tool = events[call].tool.as_mut().expect("tool call");
                        tool.output = Some(output);
                        tool.status = status;
                        tool.completed_at = ts;
                    } else {
                        let tool = ToolInteraction {
                            name: "Orphan tool result".into(),
                            call_id: id,
                            input: None,
                            output: Some(output),
                            status,
                            exit_code: None,
                            started_at: None,
                            completed_at: ts,
                            orphan: true,
                        };
                        emit(
                            &mut events,
                            index,
                            EventKind::Tool,
                            Actor::Tool,
                            None,
                            Some(tool),
                        );
                    }
                }
                _ => hidden += 1,
            }
        }
        if assistant_record && events.len() > first_event {
            let id = message
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty());
            if id.is_none_or(|id| !observed_usage.contains(id))
                && let Some(observation) = usage(message, &prov)
            {
                events[first_event].usage = Some(observation);
                if let Some(id) = id {
                    observed_usage.insert(id.to_owned());
                }
            }
        }
    }
    ClaudeTranscript {
        events,
        hidden_records: hidden,
    }
}

struct ToolRow {
    offset: u64,
    index: usize,
    call: bool,
    id: Option<String>,
}

impl ToolRow {
    fn apply(&self, calls: &mut HashSet<String>) -> usize {
        if self.call {
            if let Some(id) = &self.id {
                calls.insert(id.clone());
            }
            1
        } else if self.id.as_ref().is_some_and(|id| calls.remove(id)) {
            0
        } else {
            1
        }
    }
}

type BlockKey = (Option<String>, u64, usize);

fn block_key(uuid: Option<&str>, offset: u64, index: usize) -> BlockKey {
    (
        uuid.map(str::to_owned),
        if uuid.is_some() { 0 } else { offset },
        index,
    )
}

struct State {
    cursor: Option<JsonlCursor>,
    summary: SessionSummary,
    prompt: Option<String>,
    generated: Option<String>,
    calls: HashSet<String>,
    tool_rows: HashMap<BlockKey, ToolRow>,
    tool_count: usize,
    emitted: Option<SessionKey>,
    reopen: bool,
    dirty: bool,
    ledger: ClaudeLedger,
    side_ledger: Option<ClaudeLedger>,
    parent_seen: bool,
    block_counts: HashMap<(String, usize), (usize, usize)>,
    metadata_entries: usize,
    metadata_bytes: usize,
    metadata_max_entries: usize,
    metadata_max_bytes: usize,
    counts_approximate: bool,
}
impl State {
    fn new(source: SourceFile, entries: usize, bytes: usize) -> Self {
        let id = source
            .path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let key = SessionKey {
            provider: Provider::Claude,
            provider_session_id: id.clone(),
            source: SourceKey::File(source.path.clone()),
        };
        let summary = SessionSummary {
            key,
            title: id,
            title_derived: false,
            project: source
                .path
                .parent()
                .and_then(Path::file_name)
                .map(|s| safe(&s.to_string_lossy()).preview),
            cwd: None,
            models: vec![],
            provider_version: None,
            created_at: None,
            last_event_at: None,
            last_persisted_at: None,
            last_completion: None,
            unfinished_work: false,
            displayed_event_count: 0,
            hidden_records: 0,
            source_available: false,
            context_window: None,
            server_tools: None,
            lifecycle: Lifecycle::Unknown,
            process: ProcessAssociation::NotChecked,
            usage: None,
            lineage: Lineage {
                parent: None,
                fork: None,
                sidechain: false,
            },
            warnings: vec![],
        };
        Self {
            cursor: None,
            summary,
            prompt: None,
            generated: None,
            calls: HashSet::new(),
            tool_rows: HashMap::new(),
            tool_count: 0,
            emitted: None,
            reopen: false,
            dirty: true,
            ledger: ClaudeLedger::new(entries, bytes),
            side_ledger: Some(ClaudeLedger::new(entries, bytes)),
            parent_seen: false,
            block_counts: HashMap::new(),
            metadata_entries: 0,
            metadata_bytes: 0,
            metadata_max_entries: entries,
            metadata_max_bytes: bytes,
            counts_approximate: false,
        }
    }
    fn reset(&mut self, source: SourceFile, entries: usize, bytes: usize) {
        let emitted = self.emitted.take();
        let cursor = self.cursor.take();
        *self = Self::new(source, entries, bytes);
        self.emitted = emitted;
        self.cursor = cursor;
    }
    fn warning(&mut self, label: &str, kind: WarningKind, message: &str) {
        let source = match &self.summary.key.source {
            SourceKey::File(p) => format!("{}#{label}", p.display()),
            _ => label.into(),
        };
        if let Some(w) = self
            .summary
            .warnings
            .iter_mut()
            .find(|w| w.key.source == source)
        {
            w.count = w.count.saturating_add(1);
        } else {
            self.summary.warnings.push(Warning {
                key: WarningKey {
                    provider: Provider::Claude,
                    source,
                    kind,
                },
                message: message.into(),
                count: 1,
            });
        }
        self.dirty = true;
    }
    fn ingest(&mut self, offset: u64, v: &Value, prices: &crate::config::PriceSchedule) {
        if let Some(id) = string(v, "sessionId") {
            self.summary.key.provider_session_id = id;
        }
        if let Some(cwd) = string(v, "cwd") {
            let p = PathBuf::from(cwd);
            self.summary.project = p.file_name().map(|n| safe(&n.to_string_lossy()).preview);
            self.summary.cwd = Some(p);
        }
        if let Some(version) = string(v, "version") {
            self.summary.provider_version = Some(safe(&version).preview);
        }
        self.summary.lineage.sidechain |=
            v.get("isSidechain").and_then(Value::as_bool) == Some(true);
        if let Some(parent) =
            string(v, "parentSessionId").or_else(|| string(v, "parent_session_id"))
        {
            self.summary.lineage.parent = Some(SessionLink::Unresolved(parent));
        } else if self.summary.lineage.sidechain
            && self.summary.lineage.parent.is_none()
            && let Some(parent) = string(v, "parentUuid")
        {
            self.summary.lineage.parent = Some(SessionLink::Unresolved(parent));
        }
        if let Some(t) = v.get("timestamp").and_then(timestamp) {
            self.summary.created_at = Some(self.summary.created_at.map_or(t, |old| old.min(t)));
            self.summary.last_persisted_at =
                Some(self.summary.last_persisted_at.map_or(t, |old| old.max(t)));
        }
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "ai-title" {
            self.generated = v
                .get("aiTitle")
                .and_then(Value::as_str)
                .and_then(title)
                .or(self.generated.take());
        }
        let msg = &v["message"];
        let sidechain = v.get("isSidechain").and_then(Value::as_bool) == Some(true);
        if !sidechain && (kind == "user" || kind == "assistant") {
            if !self.parent_seen {
                self.summary.usage = None;
                self.summary.server_tools = None;
            }
            self.parent_seen = true;
            self.side_ledger = None;
        }
        if kind == "assistant" {
            let prov = Provenance {
                provider: Provider::Claude,
                source: self.summary.key.source.clone(),
                locator: locator(&self.summary.key, offset),
                record_type: kind.into(),
                provider_version: self.summary.provider_version.clone(),
            };
            let observation = usage(msg, &prov);
            let tools = msg
                .get("usage")
                .and_then(|u| u.get("server_tool_use"))
                .and_then(Value::as_object)
                .map(|v| ServerToolCounts {
                    web_search: v
                        .get("web_search_requests")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    web_fetch: v
                        .get("web_fetch_requests")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                });
            let ledger = if sidechain {
                self.side_ledger.as_mut()
            } else {
                Some(&mut self.ledger)
            };
            if let Some(ledger) = ledger {
                ledger.observe(
                    msg.get("id").and_then(Value::as_str),
                    observation.as_ref(),
                    tools,
                );
            }
        }
        if self.parent_seen && kind == "assistant" && !sidechain {
            self.summary.usage = self.ledger.summary(prices);
            self.summary.server_tools = self.ledger.server_tools();
        }
        if kind == "assistant"
            && let Some(model) = string(msg, "model").map(|s| safe(&s).preview)
            && !self.summary.models.contains(&model)
        {
            self.summary.models.push(model);
        }
        if kind == "user"
            && self.prompt.is_none()
            && v.get("isMeta").and_then(Value::as_bool) != Some(true)
        {
            let content = &msg["content"];
            let texts: Vec<&str> = match content {
                Value::String(s) => vec![s],
                Value::Array(a) => a
                    .iter()
                    .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect(),
                _ => vec![],
            };
            self.prompt = texts.into_iter().filter(|s| !injected(s)).find_map(title);
        }
        let uuid = v.get("uuid").and_then(Value::as_str);
        let parts = match &msg["content"] {
            Value::Array(parts) if kind == "user" || kind == "assistant" => Some(parts.as_slice()),
            _ => None,
        };
        let block_count = if parts.is_some() {
            parts.map_or(0, <[Value]>::len)
        } else if (kind == "user" || kind == "assistant") && msg["content"].is_string() {
            1
        } else {
            0
        };
        if !self.counts_approximate {
            // Charge every observation, including revisions, conservatively.
            // This bounds retained identity metadata and revision rebuild work.
            let uuid_bytes = uuid.map_or(0, str::len).saturating_mul(3);
            let required = (0..block_count).fold(0usize, |total, index| {
                let id_bytes = parts.and_then(|p| p.get(index)).map_or(0, |part| {
                    ["id", "tool_use_id"]
                        .into_iter()
                        .filter_map(|name| part.get(name).and_then(Value::as_str))
                        .map(str::len)
                        .max()
                        .unwrap_or(0)
                });
                total.saturating_add(
                    256usize
                        .saturating_add(uuid_bytes)
                        .saturating_add(id_bytes.saturating_mul(2)),
                )
            });
            if block_count
                > self
                    .metadata_max_entries
                    .saturating_sub(self.metadata_entries)
                || required > self.metadata_max_bytes.saturating_sub(self.metadata_bytes)
            {
                self.counts_approximate = true;
                self.block_counts = HashMap::new();
                self.tool_rows = HashMap::new();
                self.calls = HashSet::new();
                self.warning(
                    "approximate-count",
                    WarningKind::Unsupported,
                    "event and hidden counts approximate: revision metadata limit exceeded",
                );
            } else {
                self.metadata_entries += block_count;
                self.metadata_bytes += required;
            }
        }
        if !self.counts_approximate {
            let mut rebuild_tools = false;
            let mut new_tools = Vec::new();
            for index in 0..block_count {
                let key = block_key(uuid, offset, index);
                let old_tool = self.tool_rows.remove(&key);
                let revised =
                    uuid.is_some_and(|u| self.block_counts.contains_key(&(u.to_owned(), index)));
                let part = parts.and_then(|p| p.get(index));
                let row = part.and_then(|part| {
                    let typ = part.get("type").and_then(Value::as_str);
                    let (call, id) = match (kind, typ) {
                        ("assistant", Some("tool_use")) => (true, string(part, "id")),
                        ("user", Some("tool_result")) => (false, string(part, "tool_use_id")),
                        _ => return None,
                    };
                    Some(ToolRow {
                        offset,
                        index,
                        call,
                        id,
                    })
                });
                if old_tool.is_some() || (revised && row.is_some()) {
                    rebuild_tools = true;
                }
                if let Some(row) = row {
                    new_tools.push(key.clone());
                    self.tool_rows.insert(key, row);
                }
            }
            let old_tool_count = self.tool_count;
            if rebuild_tools {
                self.calls.clear();
                let mut rows: Vec<_> = self.tool_rows.values().collect();
                rows.sort_unstable_by_key(|row| (row.offset, row.index));
                self.tool_count = rows.into_iter().map(|row| row.apply(&mut self.calls)).sum();
            } else {
                for key in &new_tools {
                    self.tool_count += self.tool_rows[key].apply(&mut self.calls);
                }
            }
            self.summary.displayed_event_count =
                self.summary.displayed_event_count - old_tool_count + self.tool_count;
            if let Some(uuid) = uuid {
                for index in 0..block_count {
                    let hidden = parts.and_then(|p| p.get(index)).map_or(0, |part| {
                        match part.get("type").and_then(Value::as_str).unwrap_or("") {
                            "text" | "image" => 0,
                            "thinking" | "tool_use" if kind == "assistant" => 0,
                            "tool_result" if kind == "user" => content_text(&part["content"]).1,
                            _ => 1,
                        }
                    });
                    if let Some((displayed, old_hidden)) = self
                        .block_counts
                        .insert((uuid.to_owned(), index), (0, hidden))
                    {
                        self.summary.displayed_event_count =
                            self.summary.displayed_event_count.saturating_sub(displayed);
                        self.summary.hidden_records =
                            self.summary.hidden_records.saturating_sub(old_hidden);
                    }
                }
            }
        }
        let mapped = map_indexed(&self.summary.key, &[(offset, v)]);
        self.summary.hidden_records += mapped.hidden_records;
        for e in mapped.events {
            if let Some(t) = e.timestamp {
                self.summary.last_event_at =
                    Some(self.summary.last_event_at.map_or(t, |old| old.max(t)));
            }
            if e.tool.is_some() && !self.counts_approximate {
                continue;
            }
            if let Some(uuid) = uuid
                && let Some(index) =
                    e.id.rsplit(':')
                        .next()
                        .and_then(|s| s.parse::<usize>().ok())
                && let Some(counts) = self.block_counts.get_mut(&(uuid.to_owned(), index))
            {
                counts.0 += 1;
            }
            self.summary.displayed_event_count += 1;
        }
        let (name, derived) = if let Some(t) = &self.generated {
            (t.clone(), false)
        } else if let Some(t) = &self.prompt {
            (t.clone(), true)
        } else {
            (safe(&self.summary.key.provider_session_id).preview, false)
        };
        self.summary.title = name;
        self.summary.title_derived = derived;
        self.dirty = true;
    }
}
struct Pass {
    sources: Vec<SourceFile>,
    next: usize,
}
/// Incremental roster scanner; transcript bodies are not retained.
pub struct ClaudeScanner {
    config: Config,
    states: HashMap<PathBuf, State>,
    pass: Option<Pass>,
}
impl ClaudeScanner {
    /// Map persisted records without paging or loading source files.
    pub fn map_transcript(key: &SessionKey, records: &[(u64, Value)]) -> ClaudeTranscript {
        map_transcript(key, records)
    }
    pub fn new(config: &Config) -> Result<Self, ProviderError> {
        Ok(Self {
            config: config.clone(),
            states: HashMap::new(),
            pass: None,
        })
    }
    pub fn scan_roster(
        &mut self,
        _now: Timestamp,
        budget: RefreshBudget,
    ) -> Result<ProviderDelta, ProviderError> {
        let mut warnings = Vec::new();
        let mut pass = match self.pass.take() {
            Some(p) => p,
            None => {
                let d = discover_claude(&self.config)?;
                warnings = d.warnings;
                Pass {
                    sources: d.sources,
                    next: 0,
                }
            }
        };
        let start = Instant::now();
        let mut count = 0usize;
        let mut bytes = 0usize;
        while pass.next < pass.sources.len()
            && count < budget.max_records
            && bytes < budget.max_bytes
            && start.elapsed().as_millis() < u128::from(budget.max_millis)
        {
            let source = pass.sources[pass.next].clone();
            let state = self.states.entry(source.path.clone()).or_insert_with(|| {
                State::new(
                    source.clone(),
                    self.config.dedup_ledger_entries,
                    self.config.dedup_ledger_bytes,
                )
            });
            if state.cursor.is_none() {
                state.cursor =
                    JsonlCursor::open(source.path.clone(), self.config.max_record_bytes).ok();
                if state.cursor.is_some() && state.reopen {
                    state.reset(
                        source.clone(),
                        self.config.dedup_ledger_entries,
                        self.config.dedup_ledger_bytes,
                    );
                }
            }
            let Some(cursor) = state.cursor.as_mut() else {
                state.summary.source_available = false;
                state.dirty = true;
                pass.next += 1;
                continue;
            };
            let before = cursor.position();
            let rotations = cursor.rotation_count();
            let result = cursor.next_record();
            let after = cursor.position();
            let rotated = cursor.rotation_count() != rotations;
            if rotated {
                state.reset(
                    source.clone(),
                    self.config.dedup_ledger_entries,
                    self.config.dedup_ledger_bytes,
                );
            }
            bytes = bytes.saturating_add(
                usize::try_from(if rotated {
                    after
                } else {
                    after.saturating_sub(before)
                })
                .unwrap_or(usize::MAX),
            );
            match result {
                Ok(Some(Record::Json { offset, value })) => {
                    state.summary.source_available = true;
                    state.ingest(offset, &value, &self.config.pricing);
                    count += 1;
                }
                Ok(None) => {
                    state.summary.source_available = true;
                    if !state.parent_seen
                        && let Some(ledger) = &state.side_ledger
                    {
                        let usage = ledger.summary(&self.config.pricing);
                        let tools = ledger.server_tools();
                        if state.summary.usage != usage || state.summary.server_tools != tools {
                            state.summary.usage = usage;
                            state.summary.server_tools = tools;
                            state.dirty = true;
                        }
                    }
                    pass.next += 1;
                }
                Ok(Some(Record::Incomplete { .. })) => {
                    state.summary.source_available = true;
                    pass.next += 1;
                }
                Ok(Some(Record::Invalid { .. })) => {
                    state.summary.source_available = true;
                    state.warning("malformed", WarningKind::Parse, "malformed records skipped");
                    count += 1;
                }
                Ok(Some(Record::TooLarge { .. })) => {
                    state.summary.source_available = true;
                    state.warning(
                        "oversized",
                        WarningKind::Unsupported,
                        "oversized records skipped",
                    );
                    count += 1;
                }
                Err(_) => {
                    state.summary.source_available = false;
                    state.cursor = None;
                    state.reopen = true;
                    state.dirty = true;
                    pass.next += 1;
                }
            }
        }
        let done = pass.next == pass.sources.len();
        let mut removed = Vec::new();
        if done {
            let live: HashSet<_> = pass.sources.iter().map(|s| &s.path).collect();
            self.states.retain(|p, s| {
                let keep = live.contains(p);
                if !keep && let Some(key) = s.emitted.take() {
                    removed.push(key);
                }
                keep
            });
        }
        let mut ids: HashMap<String, usize> = HashMap::new();
        for state in self.states.values() {
            *ids.entry(state.summary.key.provider_session_id.clone())
                .or_default() += 1;
        }
        for state in self.states.values_mut() {
            let count = ids[&state.summary.key.provider_session_id];
            let source = match &state.summary.key.source {
                SourceKey::File(p) => format!("{}#duplicate-id", p.display()),
                _ => "#duplicate-id".into(),
            };
            let previous = state
                .summary
                .warnings
                .iter()
                .find(|w| w.key.source == source)
                .map(|w| w.count);
            let wanted = (count > 1).then(|| u32::try_from(count).unwrap_or(u32::MAX));
            if previous != wanted {
                state.summary.warnings.retain(|w| w.key.source != source);
                if let Some(count) = wanted {
                    state.summary.warnings.push(Warning {
                        key: WarningKey {
                            provider: Provider::Claude,
                            source,
                            kind: WarningKind::Unsupported,
                        },
                        message: "Duplicate ID: another source shares this session ID".into(),
                        count,
                    });
                }
                state.dirty = true;
            }
        }
        let scan = ScanProgress {
            provider: Provider::Claude,
            scanned: pass.next,
            total: Some(pass.sources.len()),
            done,
        };
        if !done {
            self.pass = Some(pass);
        }
        let sessions = self
            .states
            .values_mut()
            .filter_map(|s| {
                if s.dirty {
                    if let Some(old) = s.emitted.replace(s.summary.key.clone())
                        && old != s.summary.key
                    {
                        removed.push(old);
                    }
                    s.dirty = false;
                    Some(s.summary.clone())
                } else {
                    None
                }
            })
            .collect();
        Ok(ProviderDelta {
            sessions,
            removed,
            warnings,
            scan,
        })
    }
}
