//! Claude project discovery, roster scanning and persisted content mapping.
//! Usage aggregation is independent from transcript paging.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime};

use chrono::DateTime;
use serde_json::Value;

use crate::config::Config;
use crate::discovery::SourceFile;
pub use crate::discovery::discover_claude;
use crate::domain::*;
use crate::ingest::jsonl::{ContentHash, JsonlCursor, Record};
use crate::ingest::text::{display_safe, preview};
use crate::providers::{ProviderDelta, ProviderError, ProviderReader, SourceRecord};
use crate::usage::ClaudeLedger;
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

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
        generation: None,
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
    generation: u64,
    fingerprint: Option<(u64, u64)>,
    summary: SessionSummary,
    prompt: Option<String>,
    generated: Option<String>,
    calls: HashSet<String>,
    tool_rows: HashMap<BlockKey, ToolRow>,
    tool_count: usize,
    emitted: Option<SessionKey>,
    reopen: bool,
    dirty: bool,
    /// Content already readable changed; consumed (and cleared) by the next delta.
    invalidated: bool,
    ledger: ClaudeLedger,
    side_ledger: Option<ClaudeLedger>,
    parent_seen: bool,
    block_counts: HashMap<(String, usize), (usize, usize)>,
    metadata_entries: usize,
    metadata_bytes: usize,
    metadata_max_entries: usize,
    metadata_max_bytes: usize,
    counts_approximate: bool,
    /// Scan pass whose start-of-visit prefix check already ran for this source.
    checked_pass: u64,
    /// Length and modified time at the last prefix check.
    stamp: Option<(u64, Option<SystemTime>)>,
    /// Exact consumed-prefix verification in progress (files up to
    /// [`PREFIX_VERIFY_MAX`]); advanced under the refresh byte budget.
    verify: Option<PrefixVerify>,
}

struct PrefixVerify {
    file: File,
    hash: ContentHash,
    left: u64,
    meta: (u64, Option<SystemTime>),
}

enum Verdict {
    Intact,
    Changed,
    Pending,
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
            model_details: vec![],
            provider_version: None,
            created_at: None,
            last_event_at: None,
            last_persisted_at: None,
            last_completion: None,
            unfinished_work: false,
            displayed_event_count: 0,
            hidden_records: 0,
            source_available: false,
            archived: false,
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
            generation: NEXT_GENERATION.fetch_add(1, Ordering::Relaxed),
            fingerprint: None,
            summary,
            prompt: None,
            generated: None,
            calls: HashSet::new(),
            tool_rows: HashMap::new(),
            tool_count: 0,
            emitted: None,
            reopen: false,
            dirty: true,
            invalidated: false,
            ledger: ClaudeLedger::new(entries, bytes),
            side_ledger: Some(ClaudeLedger::new(entries, bytes)),
            parent_seen: false,
            block_counts: HashMap::new(),
            metadata_entries: 0,
            metadata_bytes: 0,
            metadata_max_entries: entries,
            metadata_max_bytes: bytes,
            counts_approximate: false,
            checked_pass: 0,
            stamp: None,
            verify: None,
        }
    }
    /// Starts checking that the file still holds the bytes this cursor consumed. A
    /// same-file rewrite that also grows the file looks like an append to the
    /// cursor, so the consumed prefix is verified whenever the file changed.
    /// Small prefixes are hashed exactly (resumable, see [`Self::verify_step`]);
    /// larger ones use the sampled fingerprint.
    fn begin_verify(&mut self, path: &Path, meta: (u64, Option<SystemTime>)) -> Verdict {
        let Some(cursor) = &self.cursor else {
            return Verdict::Intact;
        };
        let end = cursor.position();
        if end == 0 {
            self.stamp = Some(meta);
            return Verdict::Intact;
        }
        if meta.0 < end {
            return Verdict::Changed;
        }
        if end > PREFIX_VERIFY_MAX {
            let ok = self
                .fingerprint
                .is_some_and(|(e, h)| e == end && fingerprint(path, end) == Some(h));
            if ok {
                self.stamp = Some(meta);
                return Verdict::Intact;
            }
            return Verdict::Changed;
        }
        let Ok(file) = File::open(path) else {
            return Verdict::Changed;
        };
        self.verify = Some(PrefixVerify {
            file,
            hash: ContentHash::new(),
            left: end,
            meta,
        });
        Verdict::Pending
    }
    /// Reads at most `allowance` bytes of the pending verification; returns the
    /// bytes read and the verdict.
    fn verify_step(&mut self, allowance: usize) -> (usize, Verdict) {
        let (Some(v), Some(cursor)) = (self.verify.as_mut(), self.cursor.as_ref()) else {
            return (0, Verdict::Intact);
        };
        let mut read = 0usize;
        let mut buf = [0u8; 8192];
        while v.left > 0 && read < allowance {
            let size = v.left.min(buf.len() as u64).min((allowance - read) as u64) as usize;
            if v.file.read_exact(&mut buf[..size]).is_err() {
                self.verify = None;
                return (read, Verdict::Changed);
            }
            crate::perf::bytes_read(size);
            v.hash.update(&buf[..size]);
            v.left -= size as u64;
            read += size;
        }
        if v.left > 0 {
            return (read, Verdict::Pending);
        }
        let intact = v.hash.finish() == cursor.content_hash();
        let meta = v.meta;
        self.verify = None;
        if intact {
            self.stamp = Some(meta);
            (read, Verdict::Intact)
        } else {
            (read, Verdict::Changed)
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
    /// Turn lifecycle from persisted evidence: an `end_turn` stop completes the turn,
    /// a `tool_use` stop or later genuine user/tool work leaves it open. Sidechains
    /// belong to subagents and never move the parent's lifecycle.
    fn track_turn(&mut self, kind: &str, v: &Value) {
        if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            return;
        }
        let msg = &v["message"];
        match kind {
            "assistant" => match msg.get("stop_reason").and_then(Value::as_str) {
                Some("end_turn") => {
                    self.summary.last_completion = v
                        .get("timestamp")
                        .and_then(timestamp)
                        .or(self.summary.last_persisted_at)
                        .or(self.summary.last_completion);
                    self.summary.unfinished_work = false;
                    self.summary.lifecycle = Lifecycle::Completed;
                }
                Some("tool_use") => self.open_turn(),
                // Pending tool_use content is unfinished work even when the
                // record carries no (or a null) stop reason, once a lifecycle is known.
                _ => {
                    let pending = msg["content"].as_array().is_some_and(|parts| {
                        parts
                            .iter()
                            .any(|p| p.get("type").and_then(Value::as_str) == Some("tool_use"))
                    });
                    if pending && self.summary.lifecycle != Lifecycle::Unknown {
                        self.open_turn();
                    }
                }
            },
            "user" if v.get("isMeta").and_then(Value::as_bool) != Some(true) => {
                let genuine = match &msg["content"] {
                    Value::String(s) => !injected(s),
                    Value::Array(parts) => {
                        parts
                            .iter()
                            .any(|p| match p.get("type").and_then(Value::as_str) {
                                Some("tool_result") => true,
                                Some("text") => p
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .is_some_and(|t| !injected(t)),
                                _ => false,
                            })
                    }
                    _ => false,
                };
                // Without earlier turn evidence a prompt alone proves nothing.
                if genuine && self.summary.lifecycle != Lifecycle::Unknown {
                    self.open_turn();
                }
            }
            _ => {}
        }
    }
    fn open_turn(&mut self) {
        self.summary.unfinished_work = true;
        self.summary.lifecycle = Lifecycle::Ongoing;
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
        self.track_turn(kind, v);
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
                locator: SourceLocator {
                    generation: Some(self.generation),
                    ..locator(&self.summary.key, offset)
                },
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
                self.invalidated = true;
                self.calls.clear();
                let mut rows: Vec<_> = self.tool_rows.values().collect();
                rows.sort_unstable_by_key(|row| (row.offset, row.index));
                self.tool_count = rows.into_iter().map(|row| row.apply(&mut self.calls)).sum();
            } else {
                for key in &new_tools {
                    let row = &self.tool_rows[key];
                    // A result that retires an already-displayed pending call changes
                    // that row in place, so a loaded page is stale.
                    if !row.call
                        && self.emitted.is_some()
                        && row.id.as_ref().is_some_and(|id| self.calls.contains(id))
                    {
                        self.invalidated = true;
                    }
                    self.tool_count += row.apply(&mut self.calls);
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
                        self.invalidated = true;
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
    id: u64,
    sources: Vec<SourceFile>,
    next: usize,
}
/// Incremental roster scanner; transcript bodies are not retained.
pub struct ClaudeScanner {
    config: Config,
    states: HashMap<PathBuf, State>,
    pass: Option<Pass>,
    index: Option<TranscriptIndex>,
    pass_counter: u64,
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
            index: None,
            pass_counter: 0,
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
                self.pass_counter += 1;
                Pass {
                    id: self.pass_counter,
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
            let mut verdict = Verdict::Intact;
            if state.checked_pass != pass.id {
                state.checked_pass = pass.id;
                state.verify = None;
                let meta = std::fs::metadata(&source.path)
                    .ok()
                    .map(|m| (m.len(), m.modified().ok()));
                if let Some(meta) = meta
                    && state.cursor.is_some()
                    && Some(meta) != state.stamp
                {
                    verdict = state.begin_verify(&source.path, meta);
                }
            }
            if state.verify.is_some() {
                let (read, v) = state.verify_step(budget.max_bytes.saturating_sub(bytes).max(1));
                bytes = bytes.saturating_add(read);
                verdict = v;
            }
            match verdict {
                Verdict::Pending => continue,
                Verdict::Changed => {
                    // Consumed bytes changed in place: nothing parsed from them holds.
                    state.reset(
                        source.clone(),
                        self.config.dedup_ledger_entries,
                        self.config.dedup_ledger_bytes,
                    );
                    state.cursor =
                        JsonlCursor::open(source.path.clone(), self.config.max_record_bytes).ok();
                    state.invalidated = true;
                    state.checked_pass = pass.id;
                    state.stamp = std::fs::metadata(&source.path)
                        .ok()
                        .map(|m| (m.len(), m.modified().ok()));
                }
                Verdict::Intact => {}
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
                state.invalidated = true;
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
                Ok(Some(Record::Skipping { .. })) => {
                    // Oversized line still draining; bytes were charged above.
                    state.summary.source_available = true;
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
        for state in self.states.values_mut() {
            if let (Some(cursor), SourceKey::File(path)) =
                (&state.cursor, &state.summary.key.source)
            {
                let end = cursor.position();
                if end > PREFIX_VERIFY_MAX
                    && state
                        .fingerprint
                        .is_none_or(|(previous, _)| previous != end)
                {
                    state.fingerprint = fingerprint(path, end).map(|hash| (end, hash));
                }
            }
        }
        let mut invalidated = Vec::new();
        let sessions = self
            .states
            .values_mut()
            .filter_map(|s| {
                if s.dirty {
                    if std::mem::take(&mut s.invalidated) {
                        invalidated.push(s.summary.key.clone());
                    }
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
            invalidated,
            warnings,
            scan,
        })
    }
}

const PREFIX_VERIFY_MAX: u64 = 4 * 1024 * 1024;

// As with the Codex adapter, large consumed prefixes use bounded sampling.
// Rewrites outside the samples can evade this check.
fn fingerprint(path: &Path, end: u64) -> Option<u64> {
    let mut file = File::open(path).ok()?;
    if file.metadata().ok()?.len() < end {
        return None;
    }
    let mut hash = ContentHash::new();
    hash.update(&end.to_le_bytes());
    let len = end.min(512) as usize;
    let mut bytes = [0u8; 512];
    for start in [0, (end / 2).min(end - len as u64), end - len as u64] {
        file.seek(SeekFrom::Start(start)).ok()?;
        file.read_exact(&mut bytes[..len]).ok()?;
        crate::perf::bytes_read(len);
        hash.update(&bytes[..len]);
    }
    Some(hash.finish())
}

// Read only the generation consumed by the roster. Never retain transcript bodies
// between requests, and reject offsets if the scanned prefix has changed.
fn checked_source<'a>(
    states: &'a HashMap<PathBuf, State>,
    key: &SessionKey,
) -> Result<(&'a Path, u64), ProviderError> {
    let SourceKey::File(path) = &key.source else {
        return Err(ProviderError::Unsupported(
            "not a Claude file session".into(),
        ));
    };
    let state = states
        .get(path)
        .filter(|s| {
            key.provider == Provider::Claude && s.summary.key == *key && s.summary.source_available
        })
        .ok_or_else(|| {
            ProviderError::Unavailable("unknown or unavailable Claude session".into())
        })?;
    let cursor = state
        .cursor
        .as_ref()
        .ok_or_else(|| ProviderError::Unavailable("source unavailable".into()))?;
    if !cursor.same_file().unwrap_or(false) {
        return Err(ProviderError::Unavailable(
            "source identity changed since refresh".into(),
        ));
    }
    let end = cursor.position();
    let intact = if end > PREFIX_VERIFY_MAX {
        state.fingerprint == fingerprint(path, end).map(|hash| (end, hash))
            && state.fingerprint.is_some()
    } else {
        let mut file = File::open(path)
            .map_err(|_| ProviderError::Unavailable("source unavailable".into()))?;
        let mut hash = ContentHash::new();
        let mut left = end;
        let mut buf = [0u8; 8192];
        while left > 0 {
            let size = left.min(buf.len() as u64) as usize;
            file.read_exact(&mut buf[..size])
                .map_err(|_| ProviderError::Unavailable("source changed since refresh".into()))?;
            crate::perf::bytes_read(size);
            hash.update(&buf[..size]);
            left -= size as u64;
        }
        hash.finish() == cursor.content_hash()
    };
    if !intact {
        return Err(ProviderError::Unavailable(
            "source changed since refresh".into(),
        ));
    }
    let SourceKey::File(path) = &state.summary.key.source else {
        unreachable!()
    };
    Ok((path, end))
}

fn event_anchor(event: &Event) -> PageAnchor {
    PageAnchor {
        timestamp: event.timestamp,
        source_order: event.source_order,
        stable_id: event.id.clone(),
    }
}

fn event_size(event: &Event) -> usize {
    let len = |part: &Option<BoundedText>| part.as_ref().map_or(0, |v| v.preview.len());
    len(&event.text)
        + event
            .tool
            .as_ref()
            .map_or(0, |t| t.name.len() + len(&t.input) + len(&t.output))
}

fn fit_event(event: &mut Event, budget: usize) -> bool {
    let mut fields: Vec<(&mut String, Option<&mut bool>)> = Vec::new();
    if let Some(text) = &mut event.text {
        fields.push((&mut text.preview, Some(&mut text.truncated)));
    }
    if let Some(tool) = &mut event.tool {
        fields.push((&mut tool.name, None));
        for text in [tool.input.as_mut(), tool.output.as_mut()]
            .into_iter()
            .flatten()
        {
            fields.push((&mut text.preview, Some(&mut text.truncated)));
        }
    }
    let mut left = budget;
    let mut remaining = fields.len();
    let mut cut = false;
    for (text, flag) in fields {
        let share = left / remaining;
        remaining -= 1;
        if text.len() > share {
            let mut at = share;
            while !text.is_char_boundary(at) {
                at -= 1;
            }
            text.truncate(at);
            if let Some(flag) = flag {
                *flag = true;
            }
            cut = true;
        }
        left -= text.len();
    }
    cut
}

const INDEX_STEP: usize = 512;
const INDEX_BYTES_PER_STEP: u64 = 4 * 1024 * 1024;
const INDEX_CAP: usize = 2_000_000;
const INDEX_METADATA_CAP: usize = 64 * 1024 * 1024;

#[derive(Clone)]
enum BlockKind {
    Row,
    Call(String),
    Result(String),
    Hidden,
}

struct IndexedBlock {
    anchor: PageAnchor,
    uuid: Option<String>,
    block: usize,
    kind: BlockKind,
    live: bool,
    usage: bool,
    result: Option<u64>,
}

struct IndexedDiagnostic {
    timestamp: Option<Timestamp>,
    offset: u64,
    label: &'static str,
    hidden: bool,
}

struct IndexCursor {
    reader: BufReader<File>,
    position: u64,
    line_start: u64,
    buffer: Vec<u8>,
    oversized: bool,
}

impl IndexCursor {
    fn open(path: &Path) -> std::io::Result<Self> {
        Ok(Self {
            reader: BufReader::new(File::open(path)?),
            position: 0,
            line_start: 0,
            buffer: Vec::new(),
            oversized: false,
        })
    }

    // Consume no more than the scanned boundary and the caller's byte allowance.
    fn next(&mut self, end: u64, max: usize, allowance: u64) -> std::io::Result<Option<Record>> {
        let stop = end.min(self.position.saturating_add(allowance));
        while self.position < stop {
            let available = self.reader.fill_buf()?;
            if available.is_empty() {
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
            }
            let len = available.len().min((stop - self.position) as usize);
            let bytes = &available[..len];
            let newline = bytes.iter().position(|&b| b == b'\n');
            let data_len = newline.unwrap_or(len);
            let consumed = data_len + usize::from(newline.is_some());
            if !self.oversized {
                if data_len > max.saturating_add(1).saturating_sub(self.buffer.len()) {
                    self.buffer.clear();
                    self.oversized = true;
                } else {
                    self.buffer.extend_from_slice(&bytes[..data_len]);
                }
            }
            self.reader.consume(consumed);
            self.position += consumed as u64;
            crate::perf::bytes_read(consumed);
            if newline.is_some() {
                let offset = self.line_start;
                self.line_start = self.position;
                let record = if self.oversized {
                    Record::TooLarge { offset }
                } else {
                    let line = self.buffer.strip_suffix(b"\r").unwrap_or(&self.buffer);
                    if line.len() > max {
                        Record::TooLarge { offset }
                    } else {
                        crate::perf::record_parsed();
                        match serde_json::from_slice(line) {
                            Ok(value) => Record::Json { offset, value },
                            Err(_) => Record::Invalid { offset },
                        }
                    }
                };
                self.buffer.clear();
                self.oversized = false;
                return Ok(Some(record));
            }
        }
        Ok(None)
    }
}
#[derive(Default)]
struct Finalization {
    stage: u8,
    at: usize,
    revisions: BTreeMap<(String, usize), usize>,
    calls: BTreeMap<String, usize>,
    seen_ids: BTreeSet<String>,
    last_record: Option<u64>,
    rows: BTreeMap<PageAnchor, usize>,
    diagnostics: BTreeMap<(Option<Timestamp>, u64, usize), usize>,
}

struct TranscriptIndex {
    path: PathBuf,
    generation: u64,
    cursor: IndexCursor,
    end: u64,
    blocks: Vec<IndexedBlock>,
    diag_order: Vec<usize>,
    base_diags: usize,
    finalization: Option<Finalization>,
    ready: bool,
    diagnostics: Vec<IndexedDiagnostic>,
    rows: Vec<usize>,
    usage_ids: HashMap<u64, Option<String>>,
    metadata_bytes: usize,
    over: bool,
}

fn record_fits(index: &TranscriptIndex, value: &Value) -> bool {
    let uuid_len = value
        .get("uuid")
        .and_then(Value::as_str)
        .map_or(0, str::len);
    let message_id = value
        .pointer("/message/id")
        .and_then(Value::as_str)
        .map_or(0, str::len);
    let mut bytes = index
        .metadata_bytes
        .saturating_add(message_id)
        .saturating_add(128);
    let mut entries = index.blocks.len().saturating_add(index.diagnostics.len());
    let content = value.pointer("/message/content");
    let count = match content {
        Some(Value::Array(parts)) => parts.len(),
        Some(Value::String(_)) => 1,
        _ => usize::from(value.get("type").and_then(Value::as_str) == Some("system")),
    };
    for n in 0..count {
        let part = content
            .and_then(Value::as_array)
            .and_then(|parts| parts.get(n));
        let id_len = part
            .and_then(|part| part.get("id").or_else(|| part.get("tool_use_id")))
            .and_then(Value::as_str)
            .map_or(0, str::len);
        bytes = bytes
            .saturating_add(uuid_len.saturating_mul(2))
            .saturating_add(id_len)
            .saturating_add(256);
        entries = entries.saturating_add(2);
        if bytes > INDEX_METADATA_CAP || entries > INDEX_CAP {
            return false;
        }
    }
    bytes <= INDEX_METADATA_CAP && entries < INDEX_CAP
}

impl TranscriptIndex {
    fn add_record(&mut self, offset: u64, value: &Value) {
        let timestamp = value.get("timestamp").and_then(timestamp);
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        let uuid = value.get("uuid").and_then(Value::as_str);
        if kind == "system" {
            self.push_block(timestamp, offset, uuid, 0, BlockKind::Row);
            self.blocks.last_mut().expect("system block added").uuid = None;
            return;
        }
        if kind != "user" && kind != "assistant" {
            self.diagnostics.push(IndexedDiagnostic {
                timestamp,
                offset,
                label: "hidden or unknown record",
                hidden: true,
            });
            return;
        }
        if kind == "assistant"
            && value
                .pointer("/message/usage")
                .is_some_and(Value::is_object)
        {
            let id = value
                .pointer("/message/id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned);
            self.metadata_bytes = self
                .metadata_bytes
                .saturating_add(id.as_ref().map_or(0, String::len) + 48);
            self.usage_ids.insert(offset, id);
        }
        let content = &value["message"]["content"];
        let string_part;
        let parts = match content {
            Value::Array(parts) => parts.as_slice(),
            Value::String(s) => {
                string_part = serde_json::json!({"type":"text","text":s});
                std::slice::from_ref(&string_part)
            }
            _ => &[],
        };
        for (block, part) in parts.iter().enumerate() {
            let typ = part.get("type").and_then(Value::as_str).unwrap_or("");
            let item = match (kind, typ) {
                (_, "text" | "image") | ("assistant", "thinking") => BlockKind::Row,
                ("assistant", "tool_use") => BlockKind::Call(
                    part.get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                ),
                ("user", "tool_result") => {
                    if content_text(&part["content"]).1 != 0 {
                        self.diagnostics.push(IndexedDiagnostic {
                            timestamp,
                            offset,
                            label: "hidden nested tool content",
                            hidden: true,
                        });
                    }
                    BlockKind::Result(
                        part.get("tool_use_id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned(),
                    )
                }
                _ => {
                    self.diagnostics.push(IndexedDiagnostic {
                        timestamp,
                        offset,
                        label: "hidden content block",
                        hidden: true,
                    });
                    BlockKind::Hidden
                }
            };
            self.push_block(timestamp, offset, uuid, block, item);
        }
    }

    fn push_block(
        &mut self,
        timestamp: Option<Timestamp>,
        offset: u64,
        uuid: Option<&str>,
        block: usize,
        kind: BlockKind,
    ) {
        let stable_id = format!("{}:{offset}:{block}", uuid.unwrap_or("record"));
        let extra = stable_id.len()
            + uuid.map_or(0, str::len)
            + match &kind {
                BlockKind::Call(id) | BlockKind::Result(id) => id.len(),
                _ => 0,
            };
        self.metadata_bytes = self
            .metadata_bytes
            .saturating_add(extra + std::mem::size_of::<IndexedBlock>());
        self.blocks.push(IndexedBlock {
            anchor: PageAnchor {
                timestamp,
                source_order: offset,
                stable_id,
            },
            uuid: uuid.map(str::to_owned),
            block,
            kind,
            live: true,
            usage: false,
            result: None,
        });
    }

    fn finish_step(&mut self) -> bool {
        let mut work = self.finalization.take().unwrap_or_default();
        let mut remaining = INDEX_STEP;
        while remaining > 0 {
            match work.stage {
                0 if work.at < self.blocks.len() => {
                    let block = &self.blocks[work.at];
                    if let Some(uuid) = &block.uuid {
                        work.revisions.insert((uuid.clone(), block.block), work.at);
                    }
                }
                1 if work.at < self.blocks.len() => {
                    let block = &mut self.blocks[work.at];
                    block.live = block.uuid.as_ref().is_none_or(|uuid| {
                        work.revisions.get(&(uuid.clone(), block.block)) == Some(&work.at)
                    });
                    block.result = None;
                    block.usage = false;
                }
                2 if work.at < self.blocks.len() => {
                    let block = &self.blocks[work.at];
                    if block.live {
                        match &block.kind {
                            BlockKind::Call(id) if !id.is_empty() => {
                                work.calls.insert(id.clone(), work.at);
                            }
                            BlockKind::Result(id) if !id.is_empty() => {
                                if let Some(call) = work.calls.remove(id) {
                                    let offset = block.anchor.source_order;
                                    let timestamp = block.anchor.timestamp;
                                    self.blocks[call].result = Some(offset);
                                    self.blocks[work.at].live = false;
                                    self.diagnostics.push(IndexedDiagnostic {
                                        timestamp,
                                        offset,
                                        label: "source-only tool result",
                                        hidden: false,
                                    });
                                }
                            }
                            _ => {}
                        }
                    }
                }
                3 if work.at < self.blocks.len() => {
                    let block = &mut self.blocks[work.at];
                    if block.live && !matches!(block.kind, BlockKind::Hidden) {
                        work.rows.insert(block.anchor.clone(), work.at);
                        if work.last_record != Some(block.anchor.source_order) {
                            work.last_record = Some(block.anchor.source_order);
                            if let Some(id) = self.usage_ids.get(&block.anchor.source_order)
                                && id
                                    .as_ref()
                                    .is_none_or(|id| work.seen_ids.insert(id.clone()))
                            {
                                block.usage = true;
                            }
                        }
                    }
                }
                4 if work.at < self.diagnostics.len() => {
                    let diag = &self.diagnostics[work.at];
                    work.diagnostics
                        .insert((diag.timestamp, diag.offset, work.at), work.at);
                }
                5 => {
                    if let Some((_, row)) = work.rows.pop_first() {
                        self.rows.push(row);
                    } else if let Some((_, diag)) = work.diagnostics.pop_first() {
                        self.diag_order.push(diag);
                    } else {
                        work.stage = 6;
                        continue;
                    }
                    remaining -= 1;
                    continue;
                }
                6 => {
                    if work.revisions.pop_first().is_none()
                        && work.calls.pop_first().is_none()
                        && work.seen_ids.pop_first().is_none()
                    {
                        self.ready = true;
                        return true;
                    }
                    remaining -= 1;
                    continue;
                }
                _ => {
                    work.stage += 1;
                    work.at = 0;
                    continue;
                }
            }
            work.at += 1;
            remaining -= 1;
        }
        self.finalization = Some(work);
        false
    }
}

fn ensure_index<'a>(
    slot: &'a mut Option<TranscriptIndex>,
    path: &Path,
    generation: u64,
    end: u64,
    max: usize,
) -> Result<&'a TranscriptIndex, ProviderError> {
    if slot
        .as_ref()
        .is_none_or(|index| index.path != path || index.generation != generation || index.end > end)
    {
        *slot = Some(TranscriptIndex {
            path: path.to_owned(),
            generation,
            cursor: IndexCursor::open(path)
                .map_err(|_| ProviderError::Unavailable("source unavailable".into()))?,
            end: 0,
            blocks: Vec::new(),
            diag_order: Vec::new(),
            base_diags: 0,
            finalization: None,
            ready: false,
            diagnostics: Vec::new(),
            rows: Vec::new(),
            usage_ids: HashMap::new(),
            metadata_bytes: 0,
            over: false,
        });
    }
    let index = slot.as_mut().expect("index initialized");
    let over = || {
        ProviderError::Unsupported("Claude transcript exceeds index entry or metadata limit".into())
    };
    if index.over {
        return Err(over());
    }
    if index.end < end && index.finalization.is_some() && !index.finish_step() {
        return Err(ProviderError::Unavailable(
            "transcript index is building (finalizing); retry".into(),
        ));
    }
    if index.end < end {
        if index.ready || index.finalization.is_some() {
            index.diagnostics.truncate(index.base_diags);
            index.rows.clear();
            index.diag_order.clear();
            index.finalization = None;
            index.ready = false;
        }
        let start = index.cursor.position;
        let mut count = 0;
        while index.cursor.position < end
            && count < INDEX_STEP
            && index.cursor.position.saturating_sub(start) < INDEX_BYTES_PER_STEP
        {
            let allowance = INDEX_BYTES_PER_STEP - index.cursor.position.saturating_sub(start);
            let record = index
                .cursor
                .next(end, max, allowance)
                .map_err(|_| ProviderError::Unavailable("source unavailable".into()))?;
            match record {
                Some(Record::Json { offset, value }) => {
                    if !record_fits(index, &value) {
                        index.over = true;
                        return Err(over());
                    }
                    index.add_record(offset, &value);
                }
                Some(Record::Invalid { offset }) | Some(Record::TooLarge { offset }) => {
                    index.diagnostics.push(IndexedDiagnostic {
                        timestamp: None,
                        offset,
                        label: "unavailable record",
                        hidden: true,
                    })
                }
                Some(Record::Skipping { .. }) => continue,
                Some(Record::Incomplete { .. }) | None => break,
            }
            index.base_diags = index.diagnostics.len();
            count += 1;
            if index.blocks.len().saturating_add(index.diagnostics.len()) > INDEX_CAP
                || index.metadata_bytes > INDEX_METADATA_CAP
            {
                index.over = true;
                return Err(over());
            }
        }
        index.end = index.cursor.position;
        if index.end < end {
            return Err(ProviderError::Unavailable(format!(
                "transcript index is building ({} of {end} bytes); retry",
                index.end
            )));
        }
    }
    if !index.ready && !index.finish_step() {
        return Err(ProviderError::Unavailable(
            "transcript index is building (finalizing); retry".into(),
        ));
    }
    Ok(index)
}

fn read_indexed_value(
    path: &Path,
    offset: u64,
    end: u64,
    max: usize,
) -> Result<Value, ProviderError> {
    let mut file =
        File::open(path).map_err(|_| ProviderError::Unavailable("source unavailable".into()))?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|_| ProviderError::Unavailable("source unavailable".into()))?;
    let mut bytes = Vec::new();
    BufReader::new(file)
        .take((end - offset).min(max as u64 + 2))
        .read_until(b'\n', &mut bytes)
        .map_err(|_| ProviderError::Unavailable("source unavailable".into()))?;
    crate::perf::bytes_read(bytes.len());
    crate::perf::record_parsed();
    serde_json::from_slice(&bytes)
        .map_err(|_| ProviderError::Unavailable("indexed source changed".into()))
}

fn indexed_event(
    index: &TranscriptIndex,
    row: usize,
    key: &SessionKey,
    end: u64,
    max: usize,
) -> Result<Event, ProviderError> {
    let block = &index.blocks[index.rows[row]];
    let offset = block.anchor.source_order;
    let value = read_indexed_value(&index.path, offset, end, max)?;
    let result = block
        .result
        .map(|at| read_indexed_value(&index.path, at, end, max).map(|value| (at, value)))
        .transpose()?;
    let mut records = vec![(offset, value)];
    if let Some(result) = result {
        records.push(result);
    }
    let mut event = map_transcript(key, &records)
        .events
        .into_iter()
        .find(|event| event.id == block.anchor.stable_id)
        .ok_or_else(|| ProviderError::Unavailable("indexed event changed".into()))?;
    event.provenance.locator.generation = Some(index.generation);
    event.usage = if block.usage {
        usage(&records[0].1["message"], &event.provenance)
    } else {
        None
    };
    Ok(event)
}

impl ProviderReader for ClaudeScanner {
    fn provider(&self) -> Provider {
        Provider::Claude
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
        let (path, end) = checked_source(&self.states, key)?;
        let generation = self.states[path].generation;
        let index = ensure_index(
            &mut self.index,
            path,
            generation,
            end,
            self.config.max_record_bytes,
        )?;
        let position = match anchor.as_ref() {
            Some(anchor) => match index
                .rows
                .binary_search_by(|row| index.blocks[*row].anchor.cmp(anchor))
            {
                Ok(pos) => Some(pos),
                Err(_) => {
                    return Ok(TranscriptPage {
                        events: vec![],
                        diagnostics: vec![],
                        next: None,
                        has_more: false,
                        truncated: false,
                    });
                }
            },
            None => None,
        };
        let forward = direction == PageDirection::Forward;
        let indices: Box<dyn Iterator<Item = usize>> = if forward {
            Box::new(position.map_or(0, |p| p + 1)..index.rows.len())
        } else {
            Box::new((0..position.unwrap_or(index.rows.len())).rev())
        };
        let mut page = Vec::new();
        let mut size = 0usize;
        let mut truncated = false;
        let mut has_more = false;
        let mut selected = Vec::new();
        for row in indices {
            let mut event = indexed_event(index, row, key, end, self.config.max_record_bytes)?;
            let cut = fit_event(&mut event, request.max_bytes);
            let cost = event_size(&event);
            if !page.is_empty()
                && (page.len() >= request.max_events.max(1)
                    || size.saturating_add(cost) > request.max_bytes)
            {
                has_more = true;
                break;
            }
            size += cost;
            truncated |= cut
                || event.text.as_ref().is_some_and(|t| t.truncated)
                || event.tool.as_ref().is_some_and(|t| {
                    t.input.as_ref().is_some_and(|v| v.truncated)
                        || t.output.as_ref().is_some_and(|v| v.truncated)
                });
            page.push(event);
            selected.push(row);
        }
        if !forward {
            page.reverse();
            selected.reverse();
        }
        let next = if forward { page.last() } else { page.first() }.map(event_anchor);
        // Assign diagnostics to the next event in total order, or the last event.
        let mut page_diags = Vec::new();
        if !selected.is_empty() || (index.rows.is_empty() && anchor.is_none()) {
            let first = selected.first().copied().unwrap_or(0);
            let last = selected.last().copied().unwrap_or(0);
            let start = if first == 0 {
                0
            } else {
                let previous = &index.blocks[index.rows[first - 1]].anchor;
                index.diag_order.partition_point(|d| {
                    let d = &index.diagnostics[*d];
                    (d.timestamp, d.offset) <= (previous.timestamp, previous.source_order)
                })
            };
            let stop = if index.rows.is_empty() || last + 1 == index.rows.len() {
                index.diag_order.len()
            } else {
                let last_anchor = &index.blocks[index.rows[last]].anchor;
                index.diag_order.partition_point(|d| {
                    let d = &index.diagnostics[*d];
                    (d.timestamp, d.offset) <= (last_anchor.timestamp, last_anchor.source_order)
                })
            };
            let overflow = stop.saturating_sub(start) > 200;
            let visible_stop = if overflow { start + 199 } else { stop };
            for entry in &index.diag_order[start..visible_stop] {
                let d = &index.diagnostics[*entry];
                let mut loc = locator(key, d.offset);
                loc.generation = Some(generation);
                page_diags.push(DiagnosticEntry {
                    locator: loc,
                    label: d.label.into(),
                    hidden: d.hidden,
                });
            }
            if overflow {
                let omitted = &index.diagnostics[index.diag_order[visible_stop]];
                let mut loc = locator(key, omitted.offset);
                loc.generation = Some(generation);
                page_diags.push(DiagnosticEntry {
                    locator: loc,
                    label: "diagnostics truncated (200-entry limit); first omitted source".into(),
                    hidden: true,
                });
                truncated = true;
            }
        }
        Ok(TranscriptPage {
            events: page,
            diagnostics: page_diags,
            next,
            has_more,
            truncated,
        })
    }

    fn count_events_after(
        &mut self,
        key: &SessionKey,
        anchor: &PageAnchor,
    ) -> Result<usize, ProviderError> {
        let (path, end) = checked_source(&self.states, key)?;
        let generation = self.states[path].generation;
        let index = ensure_index(
            &mut self.index,
            path,
            generation,
            end,
            self.config.max_record_bytes,
        )?;
        Ok(index.rows.len()
            - index
                .rows
                .partition_point(|row| index.blocks[*row].anchor <= *anchor))
    }

    fn source_record(&mut self, source: &SourceLocator) -> Result<SourceRecord, ProviderError> {
        let LocatorKind::FileOffset { path, offset } = &source.kind else {
            return Err(ProviderError::Unsupported(
                "not a Claude file locator".into(),
            ));
        };
        let (expected, end) = checked_source(&self.states, &source.session)?;
        if self.states[expected].generation != source.generation.unwrap_or(0) {
            return Err(ProviderError::Unavailable("stale source locator".into()));
        }
        if path != expected {
            return Err(ProviderError::Unsupported(
                "locator does not belong to session".into(),
            ));
        }
        if *offset >= end {
            return Err(ProviderError::Unavailable(
                "record beyond scanned data".into(),
            ));
        }
        let mut file = File::open(path)
            .map_err(|_| ProviderError::Unavailable("source unavailable".into()))?;
        if *offset > 0 {
            file.seek(SeekFrom::Start(offset - 1))
                .map_err(|_| ProviderError::Unavailable("source unavailable".into()))?;
            let mut prior = [0u8; 1];
            file.read_exact(&mut prior)
                .map_err(|_| ProviderError::Unavailable("source unavailable".into()))?;
            if prior[0] != b'\n' {
                return Err(ProviderError::Parse(
                    "locator is not a record boundary".into(),
                ));
            }
        }
        file.seek(SeekFrom::Start(*offset))
            .map_err(|_| ProviderError::Unavailable("source unavailable".into()))?;
        let mut bytes = Vec::new();
        BufReader::new(file)
            .take((end - offset).min(self.config.max_record_bytes as u64 + 2))
            .read_until(b'\n', &mut bytes)
            .map_err(|_| ProviderError::Unavailable("source unavailable".into()))?;
        if bytes.last() != Some(&b'\n') {
            return Ok(SourceRecord {
                raw: String::new(),
                display: String::new(),
                unavailable: Some("record incomplete or exceeds limit".into()),
            });
        }
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        if bytes.len() > self.config.max_record_bytes {
            return Ok(SourceRecord {
                raw: String::new(),
                display: String::new(),
                unavailable: Some("record exceeds limit".into()),
            });
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
