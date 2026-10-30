//! Normalized, provider-independent domain types shared by every layer.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

/// Epoch milliseconds, UTC.
pub type Timestamp = i64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RefreshBudget {
    pub max_records: usize,
    pub max_bytes: usize,
    pub max_millis: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Provider {
    Codex,
    Claude,
    OpenCode,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SourceKey {
    File(PathBuf),
    DbRow { table: String, id: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionKey {
    pub provider: Provider,
    pub provider_session_id: String,
    pub source: SourceKey,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionLink {
    Resolved(SessionKey),
    Ambiguous,
    Unresolved(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lineage {
    pub parent: Option<SessionLink>,
    pub fork: Option<SessionLink>,
    pub sidechain: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    UserPrompt,
    AssistantMessage,
    Reasoning,
    Tool,
    System,
    Lifecycle,
    Compaction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Actor {
    User,
    Assistant,
    System,
    Tool,
}

/// Display-safe text. `Debug` prints lengths only, never the preview.
#[derive(Clone, PartialEq, Eq)]
pub struct BoundedText {
    pub preview: String,
    pub byte_len: usize,
    pub truncated: bool,
    pub locator: Option<SourceLocator>,
}

impl fmt::Debug for BoundedText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoundedText")
            .field("preview_len", &self.preview.len())
            .field("byte_len", &self.byte_len)
            .field("truncated", &self.truncated)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceLocator {
    pub session: SessionKey,
    pub kind: LocatorKind,
    /// Opaque identity of the scanned source generation.
    pub generation: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocatorKind {
    FileOffset {
        path: PathBuf,
        offset: u64,
    },
    DbRow {
        table: String,
        row_id: String,
        part_index: Option<usize>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provenance {
    pub provider: Provider,
    pub source: SourceKey,
    pub locator: SourceLocator,
    pub record_type: String,
    pub provider_version: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TokenCategories {
    pub input: Option<u64>,
    pub cached_input: Option<u64>,
    pub cache_write: Option<u64>,
    pub cache_write_5m: Option<u64>,
    pub cache_write_1h: Option<u64>,
    pub cache_read: Option<u64>,
    pub output: Option<u64>,
    pub reasoning: Option<u64>,
    pub total: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggregationClass {
    Reported,
    Derived,
    Partial,
    Estimated,
    Unknown,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CostEstimate {
    pub usd: Option<f64>,
    pub class: AggregationClass,
    pub as_of: Option<String>,
    pub note: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsageScope {
    Session,
    Turn,
    Message,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageObservation {
    pub model: Option<String>,
    pub scope: UsageScope,
    pub categories: TokenCategories,
    pub provenance: Provenance,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UsageSummary {
    pub model: Option<String>,
    pub categories: TokenCategories,
    pub total: Option<u64>,
    pub class: AggregationClass,
    pub cost: CostEstimate,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ServerToolCounts {
    pub web_search: u64,
    pub web_fetch: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WarningKind {
    Io,
    Parse,
    Unsupported,
    Unavailable,
    Process,
    Pricing,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WarningKey {
    pub provider: Provider,
    pub source: String,
    pub kind: WarningKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Warning {
    pub key: WarningKey,
    pub message: String,
    pub count: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanProgress {
    pub provider: Provider,
    pub scanned: usize,
    pub total: Option<usize>,
    pub done: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiagnosticEntry {
    pub locator: SourceLocator,
    pub label: String,
    pub hidden: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub id: String,
    pub session: SessionKey,
    pub timestamp: Option<Timestamp>,
    pub source_order: u64,
    pub kind: EventKind,
    pub actor: Actor,
    pub text: Option<BoundedText>,
    pub tool: Option<ToolInteraction>,
    pub usage: Option<UsageObservation>,
    pub provenance: Provenance,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolInteraction {
    pub name: String,
    pub call_id: Option<String>,
    pub input: Option<BoundedText>,
    pub output: Option<BoundedText>,
    pub status: ToolStatus,
    pub exit_code: Option<i32>,
    pub started_at: Option<Timestamp>,
    pub completed_at: Option<Timestamp>,
    pub orphan: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolStatus {
    Pending,
    Completed,
    Error,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lifecycle {
    Unknown,
    Ongoing,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessAssociation {
    Matched {
        pid: u32,
        started_at: Option<Timestamp>,
    },
    Candidates {
        count: usize,
    },
    Unmatched,
    NotChecked,
    Unavailable,
}

/// Display metadata for one model. `id` is the exact-match pricing key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelDetail {
    pub id: String,
    pub provider: Option<String>,
    pub variant: Option<String>,
}

/// Session metadata. `Debug` omits titles, paths, and other content-bearing fields.
#[derive(Clone, PartialEq)]
pub struct SessionSummary {
    pub key: SessionKey,
    pub title: String,
    pub title_derived: bool,
    pub project: Option<String>,
    pub cwd: Option<PathBuf>,
    pub models: Vec<String>,
    /// Provider/variant metadata for `models` entries when the provider reports it.
    pub model_details: Vec<ModelDetail>,
    pub provider_version: Option<String>,
    pub created_at: Option<Timestamp>,
    pub last_event_at: Option<Timestamp>,
    pub last_persisted_at: Option<Timestamp>,
    pub last_completion: Option<Timestamp>,
    pub unfinished_work: bool,
    pub displayed_event_count: usize,
    pub hidden_records: usize,
    pub source_available: bool,
    /// Provider-reported archived state (OpenCode `time_archived`); never inferred.
    pub archived: bool,
    pub context_window: Option<u64>,
    pub server_tools: Option<ServerToolCounts>,
    pub lifecycle: Lifecycle,
    pub process: ProcessAssociation,
    pub usage: Option<UsageSummary>,
    pub lineage: Lineage,
    pub warnings: Vec<Warning>,
}

impl fmt::Debug for SessionSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionSummary")
            .field("provider", &self.key.provider)
            .field("title_len", &self.title.len())
            .field("title_derived", &self.title_derived)
            .field("last_persisted_at", &self.last_persisted_at)
            .field("last_completion", &self.last_completion)
            .field("unfinished_work", &self.unfinished_work)
            .field("displayed_event_count", &self.displayed_event_count)
            .field("hidden_records", &self.hidden_records)
            .field("source_available", &self.source_available)
            .field("lifecycle", &self.lifecycle)
            .field("process", &self.process)
            .field("warnings", &self.warnings.len())
            .finish_non_exhaustive()
    }
}

/// Total order: timestamp (`None` first), then source order, then stable ID.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PageAnchor {
    pub timestamp: Option<Timestamp>,
    pub source_order: u64,
    pub stable_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageDirection {
    Forward,
    Backward,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageRequest {
    pub max_events: usize,
    pub max_bytes: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TranscriptPage {
    pub events: Vec<Event>,
    pub diagnostics: Vec<DiagnosticEntry>,
    pub next: Option<PageAnchor>,
    pub has_more: bool,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub label: StatusLabel,
    pub evidence: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusLabel {
    Active,
    Recent,
    Quiet,
    PossiblyStalled,
    Complete,
    Failed,
    Cancelled,
    Unavailable,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Thresholds {
    pub quiet: Duration,
    pub stall: Duration,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_text_debug_hides_preview() {
        let t = BoundedText {
            preview: "TOPSECRET".into(),
            byte_len: 9,
            truncated: false,
            locator: None,
        };
        let dbg = format!("{t:?}");
        assert!(!dbg.contains("TOPSECRET"));
        assert!(dbg.contains("byte_len"));
    }

    #[test]
    fn anchor_orders_missing_timestamp_first_then_order_then_id() {
        let a = |t, o, id: &str| PageAnchor {
            timestamp: t,
            source_order: o,
            stable_id: id.into(),
        };
        assert!(a(None, 9, "z") < a(Some(0), 0, "a"));
        assert!(a(Some(1), 1, "z") < a(Some(1), 2, "a"));
        assert!(a(Some(1), 1, "a") < a(Some(1), 1, "b"));
    }
}
