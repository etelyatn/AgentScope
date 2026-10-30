//! Search input, filter panel, help, process candidates, and pane focus
//! (spec §10.4–§10.5). Pure state, no I/O.
//!
//! While a search is being edited (`App::is_editing`), the keyboard layer sends
//! every printable key here, so `q` is an ordinary character in the input.

use std::cell::Cell;

use crate::domain::{ProcessAssociation, Provider, SessionSummary, StatusLabel, Timestamp};
use crate::processes::{self, ProcessInfo, ProcessSnapshot};

use super::{App, FilterState, Focus, PROVIDERS, STATUS_ORDER, Screen, UsageAvailability};

/// Longest search text accepted (characters).
const MAX_QUERY_CHARS: usize = 200;

/// A modal panel drawn over the current screen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Panel {
    #[default]
    None,
    Help,
    Processes,
    Filter,
}

/// What a search edit applies to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchScope {
    Roster,
    Transcript,
}

struct Editing {
    scope: SearchScope,
    buffer: String,
    /// Query to restore on `Esc`.
    backup: String,
    /// Transcript selection when editing began; matches are searched from here.
    origin: usize,
}

#[derive(Default)]
pub(super) struct InputState {
    panel: Panel,
    editing: Option<Editing>,
    processes: Option<ProcessSnapshot>,
    cursor: usize,
    scroll: usize,
    /// Largest scroll offset of the last rendered help/process panel.
    extent: Cell<usize>,
}

/// One row of the filter panel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FilterTarget {
    Provider(Provider),
    Status(StatusLabel),
    Usage(UsageAvailability),
    /// `None` is "any project".
    Project(Option<String>),
    Clear,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilterItem {
    pub target: FilterTarget,
    pub selected: bool,
}

/// Process candidates for the session in focus.
pub struct ProcessView<'a> {
    pub session: Option<&'a SessionSummary>,
    pub association: Option<ProcessAssociation>,
    pub sampled_at: Option<Timestamp>,
    pub candidates: Vec<&'a ProcessInfo>,
}

impl App {
    // ---- search ----------------------------------------------------------

    /// A search input is open; printable keys (including `q`) are text.
    pub fn is_editing(&self) -> bool {
        self.input.editing.is_some()
    }

    /// The text being edited and what it searches.
    pub fn search_input(&self) -> Option<(SearchScope, &str)> {
        self.input
            .editing
            .as_ref()
            .map(|e| (e.scope, e.buffer.as_str()))
    }

    /// `/`: edit the roster query, or the transcript query inside the inspector.
    pub fn begin_search(&mut self) -> bool {
        if self.input.editing.is_some() || self.input.panel != Panel::None {
            return false;
        }
        let (scope, current, origin) = match (self.screen, &self.transcript) {
            (Screen::Roster, _) => (SearchScope::Roster, self.filter.query.clone(), 0),
            (Screen::Inspector, Some(t)) => (
                SearchScope::Transcript,
                t.search_query().to_owned(),
                t.selected(),
            ),
            _ => return false,
        };
        self.input.editing = Some(Editing {
            scope,
            buffer: current.clone(),
            backup: current,
            origin,
        });
        self.focus = Focus::Search;
        true
    }

    pub fn search_push(&mut self, c: char) {
        if c.is_control() {
            return;
        }
        if let Some(e) = &mut self.input.editing
            && e.buffer.chars().count() < MAX_QUERY_CHARS
        {
            e.buffer.push(c);
            self.apply_search();
        }
    }

    pub fn search_backspace(&mut self) {
        if let Some(e) = &mut self.input.editing {
            e.buffer.pop();
            self.apply_search();
        }
    }

    /// `Ctrl+u`: clear the input.
    pub fn search_clear(&mut self) {
        if let Some(e) = &mut self.input.editing {
            e.buffer.clear();
            self.apply_search();
        }
    }

    /// `Enter`: keep the query and leave the input.
    pub fn search_commit(&mut self) {
        if self.input.editing.take().is_some() {
            self.focus = Focus::Roster;
        }
    }

    /// `Esc`: restore the previous query and selection and leave the input.
    pub fn search_cancel(&mut self) {
        let Some(e) = &mut self.input.editing else {
            return;
        };
        e.buffer = e.backup.clone();
        let origin = e.origin;
        self.apply_search();
        if let Some(t) = &mut self.transcript
            && self.screen == Screen::Inspector
        {
            t.select_index(origin);
        }
        self.input.editing = None;
        self.focus = Focus::Roster;
    }

    /// Apply the edit buffer. The roster narrows live; the transcript jumps to
    /// the first match at or after where the search began.
    fn apply_search(&mut self) {
        let Some(e) = &self.input.editing else {
            return;
        };
        let (scope, buffer, origin) = (e.scope, e.buffer.clone(), e.origin);
        match scope {
            SearchScope::Roster => self.set_query(&buffer),
            SearchScope::Transcript => {
                if let Some(t) = &mut self.transcript {
                    t.set_search_query(&buffer);
                    // No match keeps the reader where the search began.
                    let target = t.match_from(origin, true, true).unwrap_or(origin);
                    t.select_index(target);
                }
                self.pump_transcript();
            }
        }
    }

    /// `n`/`N`: the next or previous transcript match (loaded events only).
    pub fn search_step(&mut self, forward: bool) {
        if self.screen != Screen::Inspector {
            return;
        }
        if let Some(t) = &mut self.transcript
            && let Some(i) = t.match_from(t.selected(), forward, false)
        {
            t.select_index(i);
        }
        self.pump_transcript();
    }

    // ---- panels ----------------------------------------------------------

    pub fn panel(&self) -> Panel {
        self.input.panel
    }

    fn open_panel(&mut self, panel: Panel) {
        self.input.panel = panel;
        self.input.cursor = 0;
        self.input.scroll = 0;
        self.input.extent.set(0);
    }

    /// `?`: help for the current screen.
    pub fn open_help(&mut self) -> bool {
        if self.input.editing.is_some() || self.input.panel != Panel::None {
            return false;
        }
        self.open_panel(Panel::Help);
        true
    }

    /// `p`: process candidates for the session in focus.
    pub fn open_processes(&mut self) -> bool {
        if self.input.editing.is_some()
            || self.input.panel != Panel::None
            || !matches!(
                self.screen,
                Screen::Roster | Screen::Inspector | Screen::Details
            )
        {
            return false;
        }
        self.open_panel(Panel::Processes);
        true
    }

    /// `f`: the filter panel (roster only).
    pub fn open_filter(&mut self) -> bool {
        if self.input.editing.is_some()
            || self.input.panel != Panel::None
            || self.screen != Screen::Roster
        {
            return false;
        }
        self.open_panel(Panel::Filter);
        self.focus = Focus::Filter;
        true
    }

    pub fn close_panel(&mut self) {
        self.input.panel = Panel::None;
        if self.input.editing.is_none() {
            self.focus = Focus::Roster;
        }
    }

    /// Render reports how far the help or process panel can scroll.
    pub fn set_panel_extent(&self, max_scroll: usize) {
        self.input.extent.set(max_scroll);
    }

    pub fn panel_scroll(&self) -> usize {
        self.input.scroll.min(self.input.extent.get())
    }

    /// The filter panel cursor, clamped to the rows.
    pub fn panel_cursor(&self) -> usize {
        self.input
            .cursor
            .min(self.filter_items().len().saturating_sub(1))
    }

    /// Move the filter cursor, or scroll the help/process panel.
    pub fn panel_move(&mut self, delta: isize) {
        match self.input.panel {
            Panel::Filter => {
                let last = self.filter_items().len().saturating_sub(1) as isize;
                self.input.cursor = (self.panel_cursor() as isize)
                    .saturating_add(delta)
                    .clamp(0, last) as usize;
            }
            Panel::Help | Panel::Processes => {
                self.input.scroll = self
                    .panel_scroll()
                    .saturating_add_signed(delta)
                    .min(self.input.extent.get());
            }
            Panel::None => {}
        }
    }

    /// Rows of the filter panel with their current state. Distinct projects
    /// come from the discovered sessions, compared case-insensitively.
    pub fn filter_items(&self) -> Vec<FilterItem> {
        let f = &self.filter;
        let mut items = Vec::new();
        for p in PROVIDERS {
            items.push(FilterItem {
                target: FilterTarget::Provider(p),
                selected: f.providers.as_ref().is_some_and(|v| v.contains(&p)),
            });
        }
        for l in STATUS_ORDER {
            items.push(FilterItem {
                target: FilterTarget::Status(l),
                selected: f.statuses.as_ref().is_some_and(|v| v.contains(&l)),
            });
        }
        for u in [
            UsageAvailability::Any,
            UsageAvailability::Known,
            UsageAvailability::Unknown,
            UsageAvailability::HasCost,
        ] {
            items.push(FilterItem {
                target: FilterTarget::Usage(u),
                selected: f.usage == u,
            });
        }
        items.push(FilterItem {
            target: FilterTarget::Project(None),
            selected: f.project.is_none(),
        });
        let mut projects: Vec<&str> = self
            .sessions
            .iter()
            .filter_map(|s| s.project.as_deref())
            .filter(|p| !p.is_empty())
            .collect();
        projects.sort_by_key(|p| (p.to_lowercase(), *p));
        projects.dedup_by_key(|p| p.to_lowercase());
        let active = f.project.as_deref().map(str::to_lowercase);
        for p in projects {
            items.push(FilterItem {
                target: FilterTarget::Project(Some(p.to_owned())),
                selected: active.as_deref() == Some(p.to_lowercase().as_str()),
            });
        }
        items.push(FilterItem {
            target: FilterTarget::Clear,
            selected: false,
        });
        items
    }

    /// `Enter`/`Space` on the focused filter row: toggle it and apply at once.
    /// Filters compose with each other and with the search query.
    pub fn panel_activate(&mut self) {
        if self.input.panel != Panel::Filter {
            return;
        }
        let items = self.filter_items();
        let Some(item) = items.get(self.panel_cursor()) else {
            return;
        };
        let mut f = self.filter.clone();
        match &item.target {
            FilterTarget::Provider(p) => {
                let mut v = f.providers.take().unwrap_or_default();
                match v.iter().position(|x| x == p) {
                    Some(i) => {
                        v.remove(i);
                    }
                    None => v.push(*p),
                }
                v.sort();
                f.providers = (!v.is_empty()).then_some(v);
            }
            FilterTarget::Status(l) => {
                let mut v = f.statuses.take().unwrap_or_default();
                match v.iter().position(|x| x == l) {
                    Some(i) => {
                        v.remove(i);
                    }
                    None => v.push(*l),
                }
                v.sort_by_key(|&l| super::status_rank(l));
                f.statuses = (!v.is_empty()).then_some(v);
            }
            FilterTarget::Usage(u) => f.usage = *u,
            FilterTarget::Project(p) => f.project = p.clone(),
            FilterTarget::Clear => {
                f = FilterState {
                    query: f.query,
                    ..FilterState::default()
                };
            }
        }
        self.set_filter(f);
    }

    /// `c` in the filter panel: drop every filter but keep the search text.
    pub fn clear_filters(&mut self) {
        let f = FilterState {
            query: self.filter.query.clone(),
            ..FilterState::default()
        };
        self.set_filter(f);
    }

    // ---- processes -------------------------------------------------------

    /// Keep the latest process sample for the candidates panel.
    pub fn apply_processes(&mut self, snapshot: ProcessSnapshot) {
        self.input.processes = Some(snapshot);
    }

    /// Candidates for the open session, or else the selected roster row.
    pub fn process_view(&self) -> ProcessView<'_> {
        let key = self
            .transcript
            .as_ref()
            .map(|t| t.key().clone())
            .or_else(|| self.selected.clone());
        let session = key.and_then(|k| self.sessions.iter().find(|s| s.key == k));
        let snapshot = self.input.processes.as_ref();
        ProcessView {
            session,
            association: session.map(|s| s.process),
            sampled_at: snapshot.map(|s| s.sampled_at),
            candidates: match (session, snapshot) {
                (Some(s), Some(snap)) => processes::candidates(s.key.provider, snap),
                _ => Vec::new(),
            },
        }
    }

    // ---- pane focus ------------------------------------------------------

    /// `Tab`: roster → transcript → details; from details back to the roster.
    pub fn focus_next(&mut self) {
        match self.screen {
            Screen::Roster => {
                self.open_inspector();
            }
            Screen::Inspector => {
                self.open_details();
            }
            Screen::Details => self.close_inspector(),
            Screen::Source | Screen::Diagnostics => {}
        }
    }

    /// `Shift+Tab`: the previous pane.
    pub fn focus_prev(&mut self) {
        match self.screen {
            Screen::Inspector => self.close_inspector(),
            Screen::Details => {
                self.close_overlay();
            }
            Screen::Roster | Screen::Source | Screen::Diagnostics => {}
        }
    }
}
