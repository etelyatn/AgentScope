use agentscope::domain::{
    LocatorKind, PageAnchor, PageDirection, PageRequest, Provider, RefreshBudget, ScanProgress,
    SessionKey, SourceKey, SourceLocator, Timestamp, TranscriptPage,
};
use agentscope::providers::{ProviderDelta, ProviderError, ProviderReader, SourceRecord};

struct Stub;

impl ProviderReader for Stub {
    fn provider(&self) -> Provider {
        Provider::Codex
    }
    fn refresh(&mut self, _: Timestamp, _: RefreshBudget) -> Result<ProviderDelta, ProviderError> {
        Ok(ProviderDelta {
            sessions: vec![],
            removed: vec![],
            invalidated: vec![],
            warnings: vec![],
            scan: ScanProgress {
                provider: Provider::Codex,
                scanned: 0,
                total: None,
                done: true,
            },
        })
    }
    fn transcript_page(
        &mut self,
        _: &SessionKey,
        _: Option<PageAnchor>,
        _: PageDirection,
        _: PageRequest,
    ) -> Result<TranscriptPage, ProviderError> {
        Err(ProviderError::Unsupported("stub".into()))
    }
    fn count_events_after(
        &mut self,
        _: &SessionKey,
        _: &PageAnchor,
    ) -> Result<usize, ProviderError> {
        Ok(0)
    }
    fn source_record(&mut self, _: &SourceLocator) -> Result<SourceRecord, ProviderError> {
        Err(ProviderError::Unavailable("stub".into()))
    }
}

fn assert_send<T: Send + ?Sized>() {}

#[test]
fn reader_is_send_and_object_safe() {
    assert_send::<Stub>();
    assert_send::<dyn ProviderReader>();
    assert_send::<Box<dyn ProviderReader>>();
    let mut b: Box<dyn ProviderReader> = Box::new(Stub);
    let budget = RefreshBudget {
        max_records: 1,
        max_bytes: 1,
        max_millis: 1,
    };
    assert!(b.refresh(0, budget).unwrap().scan.done);
    assert_eq!(b.provider(), Provider::Codex);
    assert!(b.source_record(&dummy_locator()).is_err());
}

fn dummy_locator() -> SourceLocator {
    SourceLocator {
        generation: None,
        session: SessionKey {
            provider: Provider::Codex,
            provider_session_id: "s".into(),
            source: SourceKey::DbRow {
                table: "t".into(),
                id: "1".into(),
            },
        },
        kind: LocatorKind::DbRow {
            table: "t".into(),
            row_id: "1".into(),
            part_index: None,
        },
    }
}
