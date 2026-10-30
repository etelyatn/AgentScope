//! Loaded-transcript window for the inspector (spec §10.3).
//!
//! Pure state: no rendering, no I/O. The window is a contiguous run of events
//! assembled from pages. Each page response carries one continuation anchor
//! (the last event of a forward page, the first event of a backward page), so
//! the window remembers one *chunk* per page and only ever cuts at a chunk
//! edge whose anchor it knows. That keeps `older`/`newer` request anchors exact:
//! scrolling never duplicates or skips events, and a tool call and its result
//! (one `Event`) are never separated.

use std::collections::HashSet;

use crate::domain::{
    DiagnosticEntry, Event, PageAnchor, PageDirection, SessionKey, SessionSummary, TranscriptPage,
};
use crate::worker::WorkerRequest;

/// Most events kept loaded; farther pages are dropped (spec §10.3). The worker
/// also never requests a page larger than this.
pub const MAX_LOADED_EVENTS: usize = 20_000;
/// Distance from a window edge at which the next page is requested.
const NEAR_EDGE: usize = 5;

/// Why a page was requested; decides how the response is merged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageKind {
    /// Oldest page, from `Select`. Replaces the window.
    Initial,
    /// Backward from the window start. Prepends.
    Older,
    /// Forward from the window end. Appends.
    Newer,
    /// Backward from the tail (`G`/`End`). Replaces the window.
    Tail,
    /// Re-read the loaded range after an in-place change, keeping the selected
    /// event. Pages are staged and swapped in once the selection is reached.
    Reload,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Pending {
    request_id: u64,
    kind: PageKind,
}

/// The anchor a chunk's page reported, and which end of the chunk it names.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Edge {
    /// Anchor of the chunk's last event (forward page).
    Last(PageAnchor),
    /// Anchor of the chunk's first event (backward page).
    First(PageAnchor),
    Unknown,
}

#[derive(Clone, Debug)]
struct Chunk {
    len: usize,
    edge: Edge,
    diagnostics: Vec<DiagnosticEntry>,
}

/// A reload in progress: forward pages from the window start, not yet shown.
struct Reload {
    staged: Vec<Event>,
    chunks: Vec<Chunk>,
    /// Anchor to continue forward from.
    anchor: Option<PageAnchor>,
    /// The event to keep selected.
    target: Option<String>,
}

pub struct Transcript {
    key: SessionKey,
    events: Vec<Event>,
    chunks: Vec<Chunk>,
    selected: usize,
    /// `Some` while older events exist: the anchor to page backward from.
    older: Option<PageAnchor>,
    /// `Some` while newer events exist: the anchor to page forward from.
    newer: Option<PageAnchor>,
    pending: Option<Pending>,
    reload: Option<Reload>,
    /// The last load failed for good; `r` retries it.
    failed: Option<PageKind>,
    expanded: HashSet<String>,
    /// Events counted after the loaded newest end (`N new`).
    new_events: usize,
    /// In-flight count request and how many loaded events follow its anchor.
    count_pending: Option<(u64, usize)>,
    /// Summary at the last probe; a different summary triggers a new count.
    probed: Option<SessionSummary>,
    /// True once any page has been applied.
    loaded: bool,
}

impl Transcript {
    pub(super) fn new(key: SessionKey, probed: Option<SessionSummary>) -> Self {
        Self {
            key,
            events: Vec::new(),
            chunks: Vec::new(),
            selected: 0,
            older: None,
            newer: None,
            pending: None,
            reload: None,
            failed: None,
            expanded: HashSet::new(),
            new_events: 0,
            count_pending: None,
            probed,
            loaded: false,
        }
    }

    pub fn key(&self) -> &SessionKey {
        &self.key
    }

    pub fn events(&self) -> &[Event] {
        &self.events
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn selected_event(&self) -> Option<&Event> {
        self.events.get(self.selected)
    }

    pub fn is_expanded(&self, event: &Event) -> bool {
        self.expanded.contains(&event.id)
    }

    /// Diagnostics for the loaded chunks, in window order.
    pub fn diagnostics(&self) -> Vec<&DiagnosticEntry> {
        self.chunks
            .iter()
            .flat_map(|c| c.diagnostics.iter())
            .collect()
    }

    pub fn has_older(&self) -> bool {
        self.older.is_some()
    }

    pub fn has_newer(&self) -> bool {
        self.newer.is_some()
    }

    pub fn loaded(&self) -> bool {
        self.loaded
    }

    /// The load in flight, for the "Loading…" indicator.
    pub fn loading(&self) -> Option<PageKind> {
        self.pending.map(|p| p.kind)
    }

    /// The kind of load that failed for good, if any.
    pub fn failed(&self) -> Option<PageKind> {
        self.failed
    }

    /// Events persisted after the newest loaded event.
    pub fn new_events(&self) -> usize {
        self.new_events
    }

    /// Carry user-visible state (expansion) over to a replacement transcript.
    pub(super) fn carry_over(&mut self, previous: &Transcript) {
        self.expanded = previous.expanded.clone();
    }

    fn begin(&mut self, request_id: u64, kind: PageKind) {
        self.pending = Some(Pending { request_id, kind });
        self.failed = None;
        self.count_pending = None;
        if kind != PageKind::Reload {
            self.reload = None;
        }
    }

    pub(super) fn select_request(&mut self, request_id: u64) -> WorkerRequest {
        self.begin(request_id, PageKind::Initial);
        WorkerRequest::Select {
            request_id,
            key: self.key.clone(),
        }
    }

    /// Request the newest page. A new request supersedes any load in flight.
    pub(super) fn tail_request(&mut self, request_id: u64) -> WorkerRequest {
        self.begin(request_id, PageKind::Tail);
        WorkerRequest::LoadPage {
            request_id,
            key: self.key.clone(),
            anchor: None,
            direction: PageDirection::Backward,
        }
    }

    /// Re-read from the window start forward, keeping the selected event. The
    /// window start is retained (it is the anchor event), so the request
    /// continues from its anchor with nothing skipped or repeated. Returns
    /// `None` before the first page has arrived.
    pub(super) fn reload_request(&mut self, request_id: u64) -> Option<WorkerRequest> {
        if !self.loaded {
            return None;
        }
        let (staged, chunks) = match (&self.older, self.events.first()) {
            (Some(anchor), Some(first)) => (
                vec![first.clone()],
                vec![Chunk {
                    len: 1,
                    edge: Edge::First(anchor.clone()),
                    diagnostics: Vec::new(),
                }],
            ),
            _ => (Vec::new(), Vec::new()),
        };
        let anchor = self.older.clone();
        self.reload = Some(Reload {
            staged,
            chunks,
            anchor: anchor.clone(),
            target: self.selected_event().map(|e| e.id.clone()),
        });
        self.begin(request_id, PageKind::Reload);
        Some(self.forward_request(request_id, anchor))
    }

    fn forward_request(&self, request_id: u64, anchor: Option<PageAnchor>) -> WorkerRequest {
        WorkerRequest::LoadPage {
            request_id,
            key: self.key.clone(),
            anchor,
            direction: PageDirection::Forward,
        }
    }

    /// Retry the load that failed for good. `None` when nothing failed or a
    /// load is already in flight.
    pub(super) fn retry_request(&mut self, request_id: u64) -> Option<WorkerRequest> {
        if self.pending.is_some() {
            return None;
        }
        match self.failed? {
            PageKind::Initial => Some(self.select_request(request_id)),
            PageKind::Tail => Some(self.tail_request(request_id)),
            PageKind::Reload => self.reload_request(request_id),
            PageKind::Older | PageKind::Newer => {
                // The edge load is re-requested by the next `next_load`.
                self.failed = None;
                None
            }
        }
    }

    /// A load or count request failed. Only the matching request is aborted;
    /// a superseded request's failure is ignored.
    pub(super) fn fail_request(&mut self, request_id: u64) {
        if let Some(pending) = self.pending.filter(|p| p.request_id == request_id) {
            self.pending = None;
            self.failed = Some(pending.kind);
            if pending.kind == PageKind::Reload {
                self.reload = None;
            }
        }
        if self.count_pending.is_some_and(|(id, _)| id == request_id) {
            self.count_pending = None;
        }
    }

    /// The page to fetch next, if a reload is mid-way, or the selection is near
    /// a window edge, and no load is in flight.
    pub(super) fn next_load(&mut self, request_id: u64) -> Option<WorkerRequest> {
        if self.pending.is_some() || !self.loaded || self.failed.is_some() {
            return None;
        }
        if let Some(anchor) = self.reload.as_ref().map(|r| r.anchor.clone()) {
            self.begin(request_id, PageKind::Reload);
            return Some(self.forward_request(request_id, anchor));
        }
        let (kind, anchor, direction) = if self.selected < NEAR_EDGE && self.older.is_some() {
            (PageKind::Older, self.older.clone(), PageDirection::Backward)
        } else if self.selected + NEAR_EDGE >= self.events.len() && self.newer.is_some() {
            (PageKind::Newer, self.newer.clone(), PageDirection::Forward)
        } else {
            return None;
        };
        self.begin(request_id, kind);
        Some(WorkerRequest::LoadPage {
            request_id,
            key: self.key.clone(),
            anchor,
            direction,
        })
    }

    /// Ask for the number of events after the newest loaded one when `summary`
    /// differs from the last probe and the window ends at the tail.
    pub(super) fn count_request(
        &mut self,
        request_id: u64,
        summary: &SessionSummary,
    ) -> Option<WorkerRequest> {
        if !self.loaded
            || self.pending.is_some()
            || self.reload.is_some()
            || self.newer.is_some()
            || self.count_pending.is_some()
            || self.probed.as_ref() == Some(summary)
        {
            return None;
        }
        if self.events.is_empty() {
            // Nothing to anchor a count on: re-read from the start.
            self.probed = Some(summary.clone());
            return self.reload_request(request_id);
        }
        // Newest chunk with a known anchor, and how many loaded events follow it.
        let mut start = 0;
        let mut found = None;
        for chunk in &self.chunks {
            if chunk.len > 0 {
                found = Some((chunk, start));
            }
            start += chunk.len;
        }
        let (chunk, chunk_start) = found?;
        let (anchor, loaded_after) = match &chunk.edge {
            Edge::Last(a) => (a.clone(), 0),
            Edge::First(a) => (a.clone(), self.events.len() - chunk_start - 1),
            Edge::Unknown => return None,
        };
        self.probed = Some(summary.clone());
        self.count_pending = Some((request_id, loaded_after));
        Some(WorkerRequest::CountAfter {
            request_id,
            key: self.key.clone(),
            anchor,
        })
    }

    /// Apply a count response. Stale responses are ignored.
    pub(super) fn apply_count(&mut self, request_id: u64, count: usize) -> bool {
        match self.count_pending {
            Some((id, loaded_after)) if id == request_id => {
                self.count_pending = None;
                self.new_events = count.saturating_sub(loaded_after);
                true
            }
            _ => false,
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.events.is_empty() {
            return;
        }
        let last = self.events.len() as isize - 1;
        self.selected = (self.selected as isize)
            .saturating_add(delta)
            .clamp(0, last) as usize;
    }

    pub fn select_first(&mut self) {
        self.selected = 0;
    }

    pub fn select_last(&mut self) {
        self.selected = self.events.len().saturating_sub(1);
    }

    /// Expand or collapse the selected row.
    pub fn toggle_selected(&mut self) {
        let Some(event) = self.events.get(self.selected) else {
            return;
        };
        if !self.expanded.remove(&event.id) {
            self.expanded.insert(event.id.clone());
        }
    }

    /// The newest end was just (re)loaded: nothing is unread, and any count in
    /// flight predates it.
    fn newest_refreshed(&mut self) {
        self.new_events = 0;
        self.count_pending = None;
    }

    /// Merge a page response. Returns `false` (and changes nothing) when the
    /// response does not answer the load in flight.
    pub(super) fn apply_page(&mut self, request_id: u64, page: TranscriptPage) -> bool {
        let Some(pending) = self.pending.filter(|p| p.request_id == request_id) else {
            return false;
        };
        self.pending = None;
        if page.events.len() > MAX_LOADED_EVENTS {
            // Cannot be kept without losing the page's anchor; treat as failed.
            self.fail_oversized(pending.kind);
            return true;
        }
        self.failed = None;
        self.loaded = true;
        let continuation = if page.has_more {
            page.next.clone()
        } else {
            None
        };
        match pending.kind {
            PageKind::Initial => {
                self.replace(page, PageDirection::Forward);
                self.older = None;
                self.newer = continuation;
                self.selected = 0;
                if self.newer.is_none() {
                    self.newest_refreshed();
                }
            }
            PageKind::Tail => {
                self.replace(page, PageDirection::Backward);
                self.older = continuation;
                self.newer = None;
                self.selected = self.events.len().saturating_sub(1);
                self.newest_refreshed();
            }
            PageKind::Newer => {
                self.append(page);
                self.newer = continuation;
                if self.newer.is_none() {
                    self.newest_refreshed();
                }
                self.drop_front();
            }
            PageKind::Older => {
                self.prepend(page);
                self.older = continuation;
                self.drop_back();
            }
            PageKind::Reload => self.apply_reload(page, continuation),
        }
        true
    }

    fn fail_oversized(&mut self, kind: PageKind) {
        self.failed = Some(kind);
        if kind == PageKind::Reload {
            self.reload = None;
        }
    }

    fn apply_reload(&mut self, mut page: TranscriptPage, continuation: Option<PageAnchor>) {
        let Some(mut reload) = self.reload.take() else {
            return;
        };
        let known: HashSet<&str> = reload.staged.iter().map(|e| e.id.as_str()).collect();
        page.events.retain(|e| !known.contains(e.id.as_str()));
        reload.chunks.push(Chunk {
            len: page.events.len(),
            edge: edge_of(&page, PageDirection::Forward),
            diagnostics: page.diagnostics,
        });
        reload.staged.extend(page.events);
        let found = reload
            .target
            .as_ref()
            .is_some_and(|t| reload.staged.iter().any(|e| &e.id == t));
        if continuation.is_some() && !found && reload.staged.len() < MAX_LOADED_EVENTS {
            reload.anchor = continuation;
            self.reload = Some(reload);
            return;
        }
        // The selection is reached (or gone): swap the refreshed window in.
        let current = self.selected_event().map(|e| e.id.clone());
        let at = |id: &Option<String>| {
            id.as_ref()
                .and_then(|id| reload.staged.iter().position(|e| &e.id == id))
        };
        self.selected = at(&current)
            .or_else(|| at(&reload.target))
            .unwrap_or(reload.staged.len().saturating_sub(1));
        self.events = reload.staged;
        self.chunks = reload.chunks;
        self.newer = continuation;
        if self.newer.is_none() {
            self.newest_refreshed();
        }
        self.drop_front();
    }

    fn replace(&mut self, page: TranscriptPage, direction: PageDirection) {
        let edge = edge_of(&page, direction);
        self.chunks = vec![Chunk {
            len: page.events.len(),
            edge,
            diagnostics: page.diagnostics,
        }];
        self.events = page.events;
    }

    fn fresh(&self, page: &mut TranscriptPage) {
        let known: HashSet<&str> = self.events.iter().map(|e| e.id.as_str()).collect();
        page.events.retain(|e| !known.contains(e.id.as_str()));
    }

    fn append(&mut self, mut page: TranscriptPage) {
        self.fresh(&mut page);
        self.chunks.push(Chunk {
            len: page.events.len(),
            edge: edge_of(&page, PageDirection::Forward),
            diagnostics: page.diagnostics,
        });
        self.events.extend(page.events);
    }

    fn prepend(&mut self, mut page: TranscriptPage) {
        self.fresh(&mut page);
        let added = page.events.len();
        self.chunks.insert(
            0,
            Chunk {
                len: added,
                edge: edge_of(&page, PageDirection::Backward),
                diagnostics: page.diagnostics,
            },
        );
        self.events.splice(0..0, page.events);
        self.selected += added;
    }

    /// Drop the oldest events until the window fits, cutting only at a chunk
    /// edge with a known anchor and never past the selection.
    fn drop_front(&mut self) {
        let excess = self.events.len().saturating_sub(MAX_LOADED_EVENTS);
        if excess == 0 {
            return;
        }
        let mut before = 0; // events in chunks preceding `k`
        let mut best: Option<(usize, usize, PageAnchor)> = None; // (chunk, drop, anchor)
        for (k, chunk) in self.chunks.iter().enumerate() {
            let candidate = match &chunk.edge {
                _ if chunk.len == 0 => None,
                Edge::First(a) => Some((before, a.clone())),
                Edge::Last(a) => Some((before + chunk.len.saturating_sub(1), a.clone())),
                Edge::Unknown => None,
            };
            if let Some((drop, anchor)) = candidate
                && drop > 0
                && drop <= self.selected
            {
                best = Some((k, drop, anchor));
                if drop >= excess {
                    break;
                }
            }
            before += chunk.len;
        }
        let Some((k, drop, anchor)) = best else {
            return;
        };
        self.events.drain(..drop);
        self.selected -= drop;
        self.chunks.drain(..k);
        if let Some(first) = self.chunks.first_mut() {
            // A forward chunk is trimmed to its anchor event; a backward chunk is whole.
            if matches!(first.edge, Edge::Last(_)) {
                first.len = 1;
                first.diagnostics.clear();
            }
        }
        self.older = Some(anchor);
    }

    /// Mirror of [`Self::drop_front`] for the newest end.
    fn drop_back(&mut self) {
        let excess = self.events.len().saturating_sub(MAX_LOADED_EVENTS);
        if excess == 0 {
            return;
        }
        let total = self.events.len();
        let mut upto = Vec::with_capacity(self.chunks.len());
        let mut running = 0;
        for chunk in &self.chunks {
            running += chunk.len;
            upto.push(running);
        }
        let mut best: Option<(usize, usize, PageAnchor)> = None;
        for (k, chunk) in self.chunks.iter().enumerate().rev() {
            let candidate = match &chunk.edge {
                _ if chunk.len == 0 => None,
                Edge::Last(a) => Some((total - upto[k], a.clone())),
                Edge::First(a) => Some((total - upto[k] + chunk.len.saturating_sub(1), a.clone())),
                Edge::Unknown => None,
            };
            if let Some((drop, anchor)) = candidate
                && drop > 0
                && total - drop > self.selected
            {
                best = Some((k, drop, anchor));
                if drop >= excess {
                    break;
                }
            }
        }
        let Some((k, drop, anchor)) = best else {
            return;
        };
        self.events.truncate(total - drop);
        self.chunks.truncate(k + 1);
        if let Some(last) = self.chunks.last_mut()
            && matches!(last.edge, Edge::First(_))
        {
            last.len = 1;
            last.diagnostics.clear();
        }
        self.newer = Some(anchor);
    }
}

fn edge_of(page: &TranscriptPage, direction: PageDirection) -> Edge {
    match (&page.next, direction) {
        (Some(a), PageDirection::Forward) => Edge::Last(a.clone()),
        (Some(a), PageDirection::Backward) => Edge::First(a.clone()),
        (None, _) => Edge::Unknown,
    }
}
