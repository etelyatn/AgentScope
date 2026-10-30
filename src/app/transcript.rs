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

use crate::domain::{DiagnosticEntry, Event, PageAnchor, PageDirection, TranscriptPage};
use crate::worker::WorkerRequest;

/// Most events kept loaded; farther pages are dropped (spec §10.3).
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

pub struct Transcript {
    key: crate::domain::SessionKey,
    events: Vec<Event>,
    chunks: Vec<Chunk>,
    selected: usize,
    /// `Some` while older events exist: the anchor to page backward from.
    older: Option<PageAnchor>,
    /// `Some` while newer events exist: the anchor to page forward from.
    newer: Option<PageAnchor>,
    pending: Option<Pending>,
    expanded: HashSet<String>,
    /// Displayed-event count at the moment the newest events were last loaded.
    baseline: usize,
    /// True once any page has been applied.
    loaded: bool,
}

impl Transcript {
    pub(super) fn new(key: crate::domain::SessionKey, baseline: usize) -> Self {
        Self {
            key,
            events: Vec::new(),
            chunks: Vec::new(),
            selected: 0,
            older: None,
            newer: None,
            pending: None,
            expanded: HashSet::new(),
            baseline,
            loaded: false,
        }
    }

    pub fn key(&self) -> &crate::domain::SessionKey {
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

    /// Forget the load in flight (it failed for good), so a later action may retry.
    pub(super) fn abort_pending(&mut self) {
        self.pending = None;
    }

    /// Events persisted since the newest events were last loaded.
    pub fn new_events(&self, displayed_count: usize) -> usize {
        displayed_count.saturating_sub(self.baseline)
    }

    /// Carry user-visible state (expansion) over to a replacement transcript.
    pub(super) fn carry_over(&mut self, previous: &Transcript) {
        self.expanded = previous.expanded.clone();
    }

    pub(super) fn select_request(&mut self, request_id: u64) -> WorkerRequest {
        self.pending = Some(Pending {
            request_id,
            kind: PageKind::Initial,
        });
        WorkerRequest::Select {
            request_id,
            key: self.key.clone(),
        }
    }

    /// Request the newest page. A new request supersedes any load in flight.
    pub(super) fn tail_request(&mut self, request_id: u64) -> WorkerRequest {
        self.pending = Some(Pending {
            request_id,
            kind: PageKind::Tail,
        });
        WorkerRequest::LoadPage {
            request_id,
            key: self.key.clone(),
            anchor: None,
            direction: PageDirection::Backward,
        }
    }

    /// The page to fetch next, if the selection is near a window edge and no
    /// load is in flight.
    pub(super) fn next_load(&mut self, request_id: u64) -> Option<WorkerRequest> {
        if self.pending.is_some() || !self.loaded {
            return None;
        }
        let (kind, anchor, direction) = if self.selected < NEAR_EDGE && self.older.is_some() {
            (PageKind::Older, self.older.clone(), PageDirection::Backward)
        } else if self.selected + NEAR_EDGE >= self.events.len() && self.newer.is_some() {
            (PageKind::Newer, self.newer.clone(), PageDirection::Forward)
        } else {
            return None;
        };
        self.pending = Some(Pending { request_id, kind });
        Some(WorkerRequest::LoadPage {
            request_id,
            key: self.key.clone(),
            anchor,
            direction,
        })
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

    /// Merge a page response. Returns `false` (and changes nothing) when the
    /// response does not answer the load in flight.
    pub(super) fn apply_page(
        &mut self,
        request_id: u64,
        page: TranscriptPage,
        displayed_count: usize,
    ) -> bool {
        let Some(pending) = self.pending.filter(|p| p.request_id == request_id) else {
            return false;
        };
        self.pending = None;
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
            }
            PageKind::Tail => {
                self.replace(page, PageDirection::Backward);
                self.older = continuation;
                self.newer = None;
                self.selected = self.events.len().saturating_sub(1);
            }
            PageKind::Newer => {
                self.append(page);
                self.newer = continuation;
                self.drop_front();
            }
            PageKind::Older => {
                self.prepend(page);
                self.older = continuation;
                self.drop_back();
            }
        }
        if self.newer.is_none() {
            self.baseline = displayed_count;
        }
        true
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
