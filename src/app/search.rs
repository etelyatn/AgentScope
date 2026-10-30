//! Case-insensitive search over the loaded transcript window (spec §10.4).
//!
//! Only stored display-safe previews are searched (spec §11.2); nothing raw is
//! retained and nothing outside the loaded window is read. Matches are cached
//! until the query or the loaded events change.

use std::cell::{Ref, RefCell};

use crate::domain::Event;

#[derive(Default)]
pub(super) struct TranscriptSearch {
    query: String,
    /// Indices of matching events; `None` when stale.
    cache: RefCell<Option<Vec<usize>>>,
}

impl TranscriptSearch {
    pub(super) fn query(&self) -> &str {
        &self.query
    }

    pub(super) fn set_query(&mut self, query: &str) {
        if self.query != query {
            self.query = query.to_owned();
            self.invalidate();
        }
    }

    /// The loaded events changed.
    pub(super) fn invalidate(&self) {
        *self.cache.borrow_mut() = None;
    }

    /// Ascending indices of matching events.
    pub(super) fn matches(&self, events: &[Event]) -> Ref<'_, Vec<usize>> {
        if self.cache.borrow().is_none() {
            let needle = self.query.trim().to_lowercase();
            let found = if needle.is_empty() {
                Vec::new()
            } else {
                events
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| event_matches(e, &needle))
                    .map(|(i, _)| i)
                    .collect()
            };
            *self.cache.borrow_mut() = Some(found);
        }
        Ref::map(self.cache.borrow(), |c| {
            c.as_ref().expect("cache was just filled")
        })
    }
}

fn event_matches(event: &Event, needle: &str) -> bool {
    let contains = |text: &str| text.to_lowercase().contains(needle);
    event.text.as_ref().is_some_and(|t| contains(&t.preview))
        || event.tool.as_ref().is_some_and(|tool| {
            contains(&tool.name)
                || tool.input.as_ref().is_some_and(|t| contains(&t.preview))
                || tool.output.as_ref().is_some_and(|t| contains(&t.preview))
        })
}
