//! Pure application state for the roster (spec §10.2–§10.4).
//!
//! No rendering, no I/O, no clock: the caller supplies `now`. Selection is
//! tracked by [`SessionKey`], so roster updates never move it.

use std::cmp::Ordering;

use crate::domain::{
    Provider, ScanProgress, SessionKey, SessionLink, SessionSummary, Status, StatusLabel,
    Thresholds, Timestamp, Warning,
};
use crate::status::synthesize;

const PROVIDERS: [Provider; 3] = [Provider::Codex, Provider::Claude, Provider::OpenCode];

const STATUS_ORDER: [StatusLabel; 9] = [
    StatusLabel::Active,
    StatusLabel::Recent,
    StatusLabel::Quiet,
    StatusLabel::PossiblyStalled,
    StatusLabel::Complete,
    StatusLabel::Failed,
    StatusLabel::Cancelled,
    StatusLabel::Unavailable,
    StatusLabel::Unknown,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortMode {
    LastActivity,
    Provider,
    Project,
    Tokens,
    Status,
}

impl SortMode {
    /// Order used by `s` to cycle (spec §10.4).
    const CYCLE: [SortMode; 5] = [
        SortMode::LastActivity,
        SortMode::Provider,
        SortMode::Project,
        SortMode::Tokens,
        SortMode::Status,
    ];

    /// Newest activity and largest token counts first; names and states ascending.
    fn default_ascending(self) -> bool {
        !matches!(self, SortMode::LastActivity | SortMode::Tokens)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UsageAvailability {
    #[default]
    Any,
    /// A reported token total exists.
    Known,
    /// No token total is available.
    Unknown,
    /// A cost figure exists.
    HasCost,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FilterState {
    pub providers: Option<Vec<Provider>>,
    pub statuses: Option<Vec<StatusLabel>>,
    pub usage: UsageAvailability,
    pub project: Option<String>,
    pub query: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Screen {
    #[default]
    Roster,
    Inspector,
    Details,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Focus {
    #[default]
    Roster,
    Search,
    Filter,
}

pub struct RosterRow<'a> {
    pub session: &'a SessionSummary,
    pub status: Status,
}

pub struct App {
    now: Timestamp,
    thresholds: Thresholds,
    sessions: Vec<SessionSummary>,
    /// Parallel to `sessions`.
    statuses: Vec<Status>,
    /// Indices into `sessions` that pass the filter, in display order.
    view: Vec<usize>,
    selected: Option<SessionKey>,
    sort: SortMode,
    ascending: bool,
    filter: FilterState,
    warnings: Vec<Warning>,
    generation: u64,
    scan: Option<ScanProgress>,
    pub screen: Screen,
    pub focus: Focus,
    pub live: bool,
}

impl App {
    pub fn new(now: Timestamp, thresholds: Thresholds) -> Self {
        Self {
            now,
            thresholds,
            sessions: Vec::new(),
            statuses: Vec::new(),
            view: Vec::new(),
            selected: None,
            sort: SortMode::LastActivity,
            ascending: SortMode::LastActivity.default_ascending(),
            filter: FilterState::default(),
            warnings: Vec::new(),
            generation: 0,
            scan: None,
            screen: Screen::default(),
            focus: Focus::default(),
            live: true,
        }
    }

    /// Replace the roster with the latest snapshot. Sessions are merged by
    /// key (updated, added, or removed), and the selection follows its key.
    /// The warning set is replaced wholesale, which retires cleared keys.
    pub fn apply_roster(&mut self, sessions: Vec<SessionSummary>, warnings: Vec<Warning>) {
        let previous = self.selected_position();
        self.sessions = sessions;
        self.warnings = warnings;
        self.rebuild(previous);
    }

    /// Apply a worker roster publication. Returns `false` (and changes nothing)
    /// for a generation older than one already applied.
    pub fn apply_roster_generation(
        &mut self,
        generation: u64,
        sessions: Vec<SessionSummary>,
        warnings: Vec<Warning>,
        scan: ScanProgress,
    ) -> bool {
        if generation < self.generation {
            return false;
        }
        self.generation = generation;
        self.scan = Some(scan);
        self.apply_roster(sessions, warnings);
        true
    }

    /// Insert or replace one warning by key.
    pub fn apply_warning(&mut self, warning: Warning) {
        match self.warnings.iter_mut().find(|w| w.key == warning.key) {
            Some(slot) => *slot = warning,
            None => self.warnings.push(warning),
        }
    }

    pub fn warnings(&self) -> &[Warning] {
        &self.warnings
    }

    /// Sum of occurrence counts across all warning keys.
    pub fn warning_total(&self) -> u32 {
        self.warnings.iter().map(|w| w.count).sum()
    }

    pub fn scan(&self) -> Option<ScanProgress> {
        self.scan
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The injected clock value used for status and age display.
    pub fn now(&self) -> Timestamp {
        self.now
    }

    /// Advance the injected clock; statuses and status filters are recomputed.
    pub fn set_now(&mut self, now: Timestamp) {
        let previous = self.selected_position();
        self.now = now;
        self.rebuild(previous);
    }

    pub fn total_sessions(&self) -> usize {
        self.sessions.len()
    }

    /// Discovered sessions per provider (unfiltered), in fixed provider order.
    pub fn provider_counts(&self) -> Vec<(Provider, usize)> {
        PROVIDERS
            .iter()
            .map(|&p| {
                (
                    p,
                    self.sessions.iter().filter(|s| s.key.provider == p).count(),
                )
            })
            .collect()
    }

    /// Discovered sessions per synthesized state (unfiltered), in fixed order.
    pub fn status_counts(&self) -> Vec<(StatusLabel, usize)> {
        STATUS_ORDER
            .iter()
            .map(|&l| (l, self.statuses.iter().filter(|s| s.label == l).count()))
            .collect()
    }

    pub fn filter(&self) -> &FilterState {
        &self.filter
    }

    pub fn set_filter(&mut self, filter: FilterState) {
        let previous = self.selected_position();
        self.filter = filter;
        self.rebuild(previous);
    }

    pub fn set_query(&mut self, query: &str) {
        let previous = self.selected_position();
        self.filter.query = query.to_owned();
        self.rebuild(previous);
    }

    pub fn sort_mode(&self) -> SortMode {
        self.sort
    }

    pub fn sort_ascending(&self) -> bool {
        self.ascending
    }

    /// Select `mode`; selecting the active mode again reverses direction.
    pub fn sort(&mut self, mode: SortMode) {
        let previous = self.selected_position();
        if mode == self.sort {
            self.ascending = !self.ascending;
        } else {
            self.sort = mode;
            self.ascending = mode.default_ascending();
        }
        self.rebuild(previous);
    }

    /// Advance to the next sort mode in cycle order (spec §10.4).
    pub fn cycle_sort(&mut self) {
        let at = SortMode::CYCLE
            .iter()
            .position(|&m| m == self.sort)
            .unwrap_or(0);
        self.sort(SortMode::CYCLE[(at + 1) % SortMode::CYCLE.len()]);
    }

    pub fn visible_rows(&self) -> Vec<RosterRow<'_>> {
        self.view.iter().map(|&i| self.row(i)).collect()
    }

    /// Every discovered session, ignoring filter and sort.
    pub fn all_rows(&self) -> Vec<RosterRow<'_>> {
        (0..self.sessions.len()).map(|i| self.row(i)).collect()
    }

    pub fn selected_key(&self) -> Option<SessionKey> {
        self.selected.clone()
    }

    /// Position of the selection within the visible rows.
    pub fn selected_index(&self) -> Option<usize> {
        self.selected_position()
    }

    /// Move by `delta` rows, clamped to the visible range.
    pub fn move_selection(&mut self, delta: isize) {
        if self.view.is_empty() {
            self.selected = None;
            return;
        }
        let current = self.selected_position().unwrap_or(0) as isize;
        let last = self.view.len() as isize - 1;
        let next = current.saturating_add(delta).clamp(0, last) as usize;
        self.selected = Some(self.sessions[self.view[next]].key.clone());
    }

    /// Resolved parent link for a session (spec §6.3): the most recent source
    /// among duplicate IDs, or `Ambiguous` when the newest tie.
    pub fn resolved_parent(&self, key: &SessionKey) -> Option<SessionLink> {
        let session = self.sessions.iter().find(|s| &s.key == key)?;
        let link = session.lineage.parent.as_ref()?;
        Some(self.resolve_link(&session.key, link))
    }

    pub fn resolved_fork(&self, key: &SessionKey) -> Option<SessionLink> {
        let session = self.sessions.iter().find(|s| &s.key == key)?;
        let link = session.lineage.fork.as_ref()?;
        Some(self.resolve_link(&session.key, link))
    }

    fn resolve_link(&self, child: &SessionKey, link: &SessionLink) -> SessionLink {
        let (provider, id) = match link {
            SessionLink::Ambiguous => return SessionLink::Ambiguous,
            SessionLink::Resolved(target) => (target.provider, &target.provider_session_id),
            SessionLink::Unresolved(id) => (child.provider, id),
        };
        let mut newest: Option<&SessionSummary> = None;
        let mut tied = false;
        for s in self
            .sessions
            .iter()
            .filter(|s| s.key.provider == provider && &s.key.provider_session_id == id)
        {
            match newest {
                None => newest = Some(s),
                Some(best) => match activity(s).cmp(&activity(best)) {
                    Ordering::Greater => {
                        newest = Some(s);
                        tied = false;
                    }
                    Ordering::Equal => tied = true,
                    Ordering::Less => {}
                },
            }
        }
        match newest {
            None => link.clone(),
            Some(_) if tied => SessionLink::Ambiguous,
            Some(s) => SessionLink::Resolved(s.key.clone()),
        }
    }

    fn row(&self, i: usize) -> RosterRow<'_> {
        RosterRow {
            session: &self.sessions[i],
            status: self.statuses[i].clone(),
        }
    }

    fn selected_position(&self) -> Option<usize> {
        let key = self.selected.as_ref()?;
        self.view.iter().position(|&i| &self.sessions[i].key == key)
    }

    /// Recompute statuses, filter, and sort; keep the selection if it is still
    /// visible, else take the row now occupying its previous position.
    fn rebuild(&mut self, previous_position: Option<usize>) {
        self.statuses = self
            .sessions
            .iter()
            .map(|s| synthesize(s, self.now, &self.thresholds))
            .collect();

        let query = self.filter.query.trim().to_lowercase();
        let project = self.filter.project.as_deref().map(str::to_lowercase);
        let mut view: Vec<usize> = (0..self.sessions.len())
            .filter(|&i| self.matches(i, &query, project.as_deref()))
            .collect();
        let (sort, asc) = (self.sort, self.ascending);
        view.sort_by(|&a, &b| self.compare(a, b, sort, asc));
        self.view = view;

        let keep = self
            .selected
            .as_ref()
            .is_some_and(|k| self.view.iter().any(|&i| &self.sessions[i].key == k));
        if !keep {
            self.selected = if self.view.is_empty() {
                None
            } else {
                let at = previous_position.unwrap_or(0).min(self.view.len() - 1);
                Some(self.sessions[self.view[at]].key.clone())
            };
        }
    }

    fn matches(&self, i: usize, query: &str, project: Option<&str>) -> bool {
        let s = &self.sessions[i];
        let f = &self.filter;
        if f.providers
            .as_ref()
            .is_some_and(|p| !p.contains(&s.key.provider))
        {
            return false;
        }
        if f.statuses
            .as_ref()
            .is_some_and(|l| !l.contains(&self.statuses[i].label))
        {
            return false;
        }
        if let Some(p) = project
            && s.project.as_deref().map(str::to_lowercase).as_deref() != Some(p)
        {
            return false;
        }
        let total = s.usage.as_ref().and_then(|u| u.total);
        let usage_ok = match f.usage {
            UsageAvailability::Any => true,
            UsageAvailability::Known => total.is_some(),
            UsageAvailability::Unknown => total.is_none(),
            UsageAvailability::HasCost => s.usage.as_ref().is_some_and(|u| u.cost.usd.is_some()),
        };
        if !usage_ok {
            return false;
        }
        if query.is_empty() {
            return true;
        }
        let contains = |text: &str| text.to_lowercase().contains(query);
        contains(&s.title)
            || s.project.as_deref().is_some_and(contains)
            || s.cwd
                .as_ref()
                .is_some_and(|c| contains(&c.to_string_lossy()))
            || contains(&s.key.provider_session_id)
    }

    /// Missing values sort last in either direction; ties fall back to newest
    /// activity, then identity, independent of direction.
    fn compare(&self, a: usize, b: usize, mode: SortMode, asc: bool) -> Ordering {
        let (sa, sb) = (&self.sessions[a], &self.sessions[b]);
        let primary = match mode {
            SortMode::LastActivity => opt_cmp(activity(sa), activity(sb), asc),
            SortMode::Provider => directed(sa.key.provider.cmp(&sb.key.provider), asc),
            SortMode::Project => opt_cmp(
                sa.project.as_deref().map(str::to_lowercase),
                sb.project.as_deref().map(str::to_lowercase),
                asc,
            ),
            SortMode::Tokens => opt_cmp(
                sa.usage.as_ref().and_then(|u| u.total),
                sb.usage.as_ref().and_then(|u| u.total),
                asc,
            ),
            SortMode::Status => directed(
                status_rank(self.statuses[a].label).cmp(&status_rank(self.statuses[b].label)),
                asc,
            ),
        };
        primary
            .then_with(|| opt_cmp(activity(sa), activity(sb), false))
            .then_with(|| sa.key.provider.cmp(&sb.key.provider))
            .then_with(|| sa.key.provider_session_id.cmp(&sb.key.provider_session_id))
            .then_with(|| format!("{:?}", sa.key.source).cmp(&format!("{:?}", sb.key.source)))
    }
}

fn activity(s: &SessionSummary) -> Option<Timestamp> {
    [s.last_persisted_at, s.last_event_at, s.last_completion]
        .into_iter()
        .flatten()
        .max()
}

fn status_rank(label: StatusLabel) -> usize {
    STATUS_ORDER
        .iter()
        .position(|&l| l == label)
        .unwrap_or(STATUS_ORDER.len())
}

fn directed(ord: Ordering, asc: bool) -> Ordering {
    if asc { ord } else { ord.reverse() }
}

fn opt_cmp<T: Ord>(a: Option<T>, b: Option<T>, asc: bool) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(a), Some(b)) => directed(a.cmp(&b), asc),
    }
}
