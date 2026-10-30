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

struct State {
    cursor: Option<JsonlCursor>,
    summary: SessionSummary,
    prompt: Option<String>,
    generated: Option<String>,
    calls: HashSet<String>,
    emitted: Option<SessionKey>,
    reopen: bool,
    dirty: bool,
    ledger: ClaudeLedger,
    parent_usage_seen: bool,
    side_usage_seen: bool,
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
            emitted: None,
            reopen: false,
            dirty: true,
            ledger: ClaudeLedger::new(entries, bytes),
            parent_usage_seen: false,
            side_usage_seen: false,
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
        if kind == "assistant" && (!sidechain || !self.parent_usage_seen) {
            if sidechain {
                self.side_usage_seen = true;
            } else {
                if !self.parent_usage_seen && self.side_usage_seen {
                    self.ledger.clear();
                }
                self.parent_usage_seen = true;
            }
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
            self.ledger.observe(
                msg.get("id").and_then(Value::as_str),
                observation.as_ref(),
                tools,
            );
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
        let mapped = map_indexed(&self.summary.key, &[(offset, v)]);
        self.summary.hidden_records += mapped.hidden_records;
        for e in mapped.events {
            if let Some(t) = e.timestamp {
                self.summary.last_event_at =
                    Some(self.summary.last_event_at.map_or(t, |old| old.max(t)));
            }
            if let Some(tool) = e.tool {
                if tool.orphan
                    && tool
                        .call_id
                        .as_ref()
                        .is_some_and(|id| self.calls.remove(id))
                {
                    continue;
                }
                if !tool.orphan
                    && tool.status == ToolStatus::Pending
                    && let Some(id) = tool.call_id
                {
                    self.calls.insert(id);
                }
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
                Ok(Some(Record::Incomplete { .. })) | Ok(None) => {
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
