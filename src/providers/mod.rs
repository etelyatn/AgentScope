//! Provider adapter contract. Adapters implement [`ProviderReader`]; the worker
//! drives them. No provider-specific parsing lives here.

pub mod claude;
pub mod codex;

use std::fmt;

use crate::domain::{
    PageAnchor, PageDirection, PageRequest, Provider, RefreshBudget, ScanProgress, SessionKey,
    SessionSummary, SourceLocator, Timestamp, TranscriptPage, Warning,
};

/// Incremental result of one bounded refresh. `scan.done == false` means the
/// budget was exhausted and the caller should call `refresh` again.
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderDelta {
    pub sessions: Vec<SessionSummary>,
    pub removed: Vec<SessionKey>,
    pub warnings: Vec<Warning>,
    pub scan: ScanProgress,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderError {
    Io(String),
    Parse(String),
    Unsupported(String),
    Unavailable(String),
    Internal(String),
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (kind, msg) = match self {
            Self::Io(m) => ("io error", m),
            Self::Parse(m) => ("parse error", m),
            Self::Unsupported(m) => ("unsupported", m),
            Self::Unavailable(m) => ("unavailable", m),
            Self::Internal(m) => ("internal error", m),
        };
        write!(f, "{kind}: {msg}")
    }
}

impl std::error::Error for ProviderError {}

/// Raw and display forms of a persisted source record, or why it is unavailable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceRecord {
    pub raw: String,
    pub display: String,
    pub unavailable: Option<String>,
}

pub trait ProviderReader: Send {
    fn provider(&self) -> Provider;
    fn refresh(
        &mut self,
        now: Timestamp,
        budget: RefreshBudget,
    ) -> Result<ProviderDelta, ProviderError>;
    fn transcript_page(
        &mut self,
        key: &SessionKey,
        anchor: Option<PageAnchor>,
        direction: PageDirection,
        request: PageRequest,
    ) -> Result<TranscriptPage, ProviderError>;
    fn count_events_after(
        &mut self,
        key: &SessionKey,
        anchor: &PageAnchor,
    ) -> Result<usize, ProviderError>;
    fn source_record(&mut self, locator: &SourceLocator) -> Result<SourceRecord, ProviderError>;
}
