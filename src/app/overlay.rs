//! Details, source, and diagnostics overlays (spec §10.3, §11.3).
//!
//! Pure state, no I/O. A source record is loaded by the worker on demand and
//! reduced to bounded display text the moment it arrives; the raw record text
//! is never stored. Toggling redaction therefore cannot flip a flag over
//! retained raw content: it clears the text and requests the record again.

use std::cell::Cell;
use std::fmt;

use crate::domain::SourceLocator;
use crate::ingest::text::{display_safe, sanitize};
use crate::providers::SourceRecord;
use crate::worker::WorkerRequest;

use super::{App, Screen};

/// Most source text kept for display (bytes, after sanitization).
pub const MAX_SOURCE_BYTES: usize = 64 * 1024;

#[derive(Clone, PartialEq, Eq)]
pub enum SourceState {
    /// The worker has been asked; nothing is shown yet.
    Loading,
    Ready {
        /// Display-safe (and, unless revealed, redacted) text, at most
        /// [`MAX_SOURCE_BYTES`].
        text: String,
        truncated: bool,
    },
    Unavailable(String),
    Failed,
}

/// `Debug` prints lengths only, never the record text.
impl fmt::Debug for SourceState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Loading => f.write_str("Loading"),
            Self::Ready { text, truncated } => f
                .debug_struct("Ready")
                .field("text_len", &text.len())
                .field("truncated", truncated)
                .finish(),
            Self::Unavailable(_) => f.write_str("Unavailable"),
            Self::Failed => f.write_str("Failed"),
        }
    }
}

/// The open source record. `request_id` names the only response it accepts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceView {
    pub locator: SourceLocator,
    pub state: SourceState,
    pub revealed: bool,
    pub scroll: usize,
    request_id: u64,
}

#[derive(Default)]
pub(super) struct Overlays {
    source: Option<SourceView>,
    /// Redaction is off for source records. Never persisted.
    reveal: bool,
    details_scroll: usize,
    diag_selected: usize,
    /// Largest scroll offset of the last rendered overlay body.
    extent: Cell<usize>,
    /// Where `Esc` returns from the source overlay.
    source_return: Option<Screen>,
}

fn truncate_to(mut s: String, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s, false);
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    (s, true)
}

impl App {
    pub fn source_view(&self) -> Option<&SourceView> {
        self.overlays.source.as_ref()
    }

    /// Redaction is currently off (`x`).
    pub fn redaction_revealed(&self) -> bool {
        self.overlays.reveal
    }

    pub fn details_scroll(&self) -> usize {
        self.overlays.details_scroll
    }

    pub fn diagnostics_selected(&self) -> usize {
        self.overlays.diag_selected
    }

    /// `d`: details for the selected event.
    pub fn open_details(&mut self) -> bool {
        if self.screen != Screen::Inspector
            || self
                .transcript
                .as_ref()
                .is_none_or(|t| t.selected_event().is_none())
        {
            return false;
        }
        self.overlays.details_scroll = 0;
        self.screen = Screen::Details;
        true
    }

    /// `D`: diagnostics for the open transcript.
    pub fn open_diagnostics(&mut self) -> bool {
        if !matches!(self.screen, Screen::Inspector | Screen::Details) || self.transcript.is_none()
        {
            return false;
        }
        self.overlays.diag_selected = 0;
        self.screen = Screen::Diagnostics;
        true
    }

    /// Move the diagnostics selection, clamped to the listed entries.
    pub fn diagnostics_move(&mut self, delta: isize) {
        let len = self
            .transcript
            .as_ref()
            .map_or(0, |t| t.diagnostics().len());
        let last = len.saturating_sub(1) as isize;
        let next = (self.overlays.diag_selected as isize)
            .saturating_add(delta)
            .clamp(0, last);
        self.overlays.diag_selected = next as usize;
    }

    /// Render reports how far the current overlay body can scroll.
    pub fn set_overlay_extent(&self, max_scroll: usize) {
        self.overlays.extent.set(max_scroll);
    }

    /// Scroll the details or source overlay by `delta` lines.
    pub fn overlay_scroll(&mut self, delta: isize) {
        let max = self.overlays.extent.get();
        let apply = |v: &mut usize| *v = v.saturating_add_signed(delta).min(max);
        match self.screen {
            Screen::Details => apply(&mut self.overlays.details_scroll),
            Screen::Source => {
                if let Some(s) = &mut self.overlays.source {
                    apply(&mut s.scroll);
                }
            }
            _ => {}
        }
    }

    /// `v`: the source record of the selected event.
    pub fn open_selected_source(&mut self) -> bool {
        let Some(locator) = self
            .transcript
            .as_ref()
            .and_then(|t| t.selected_event())
            .map(|e| e.provenance.locator.clone())
        else {
            return false;
        };
        self.open_source(locator)
    }

    /// Enter in the diagnostics overlay: open the selected entry's record.
    pub fn open_diagnostic_source(&mut self) -> bool {
        let Some(locator) = self.transcript.as_ref().and_then(|t| {
            t.diagnostics()
                .get(self.overlays.diag_selected)
                .map(|d| d.locator.clone())
        }) else {
            return false;
        };
        self.open_source(locator)
    }

    /// Open the source overlay for `locator` and queue the worker request.
    /// Locators that the open transcript does not reference (another session,
    /// or a superseded source generation) are rejected without any request.
    pub fn open_source(&mut self, locator: SourceLocator) -> bool {
        if !matches!(
            self.screen,
            Screen::Inspector | Screen::Details | Screen::Diagnostics
        ) || !self
            .transcript
            .as_ref()
            .is_some_and(|t| t.knows_locator(&locator))
        {
            return false;
        }
        self.overlays.source_return = Some(self.screen);
        self.screen = Screen::Source;
        self.request_source(locator);
        true
    }

    /// `x`: toggle redaction. An open source record is cleared and requested
    /// again, so the new form is rebuilt from the source, not from retained
    /// text. Outside the source overlay the setting applies to the next `v`.
    pub fn toggle_redaction(&mut self) {
        if self.transcript.is_none() {
            return;
        }
        self.overlays.reveal = !self.overlays.reveal;
        if let Some(mut view) = self.overlays.source.take() {
            if self
                .transcript
                .as_ref()
                .is_some_and(|t| t.knows_locator(&view.locator))
            {
                self.request_source(view.locator);
            } else {
                view.state = SourceState::Unavailable(
                    "record is no longer part of the loaded transcript".into(),
                );
                view.revealed = self.overlays.reveal;
                self.overlays.source = Some(view);
            }
        }
    }

    /// `Esc`/`Backspace` inside an overlay. Returns `false` on the inspector
    /// itself, which the caller handles.
    pub fn close_overlay(&mut self) -> bool {
        match self.screen {
            Screen::Source => {
                self.overlays.source = None;
                self.overlays.reveal = false;
                self.screen = self
                    .overlays
                    .source_return
                    .take()
                    .unwrap_or(Screen::Inspector);
                true
            }
            Screen::Details | Screen::Diagnostics => {
                self.screen = Screen::Inspector;
                true
            }
            _ => false,
        }
    }

    /// Accept the worker's source record for the open request. The record is
    /// consumed here; only bounded display text survives.
    pub fn apply_source(&mut self, request_id: u64, record: SourceRecord) -> bool {
        let Some(view) = self
            .overlays
            .source
            .as_mut()
            .filter(|v| v.request_id == request_id && v.state == SourceState::Loading)
        else {
            return false;
        };
        view.state = match record.unavailable {
            Some(reason) => SourceState::Unavailable(display_safe(&reason)),
            None => {
                let (text, truncated) = if view.revealed {
                    // Redaction off; terminal sanitization stays on.
                    let (raw, cut) = truncate_to(record.raw, MAX_SOURCE_BYTES);
                    let (text, cut2) = truncate_to(sanitize(&raw), MAX_SOURCE_BYTES);
                    (text, cut || cut2)
                } else {
                    // Redact before cutting so a secret is never split.
                    truncate_to(display_safe(&record.display), MAX_SOURCE_BYTES)
                };
                SourceState::Ready { text, truncated }
            }
        };
        true
    }

    pub(super) fn fail_source(&mut self, request_id: u64) {
        if let Some(view) = self
            .overlays
            .source
            .as_mut()
            .filter(|v| v.request_id == request_id && v.state == SourceState::Loading)
        {
            view.state = SourceState::Failed;
        }
    }

    fn request_source(&mut self, locator: SourceLocator) {
        let request_id = self.next_request_id();
        self.overlays.source = Some(SourceView {
            locator: locator.clone(),
            state: SourceState::Loading,
            revealed: self.overlays.reveal,
            scroll: 0,
            request_id,
        });
        self.outbox.push(WorkerRequest::LoadSource {
            request_id,
            locator,
        });
    }
}
