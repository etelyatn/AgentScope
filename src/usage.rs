//! Session totals and cost estimation (spec §8). Missing data is `Unknown`,
//! never zero.

use crate::config::{PriceEntry, PriceSchedule};
use crate::domain::{AggregationClass, CostEstimate, Provider, TokenCategories};

use std::collections::HashSet;

use crate::domain::{ServerToolCounts, UsageSummary};

/// Exact, bounded message identity ledger, independent of transcript retention.
pub(crate) struct ClaudeLedger {
    ids: HashSet<String>,
    bytes: usize,
    max_entries: usize,
    max_bytes: usize,
    frozen: bool,
    partial: bool,
    categories: Option<TokenCategories>,
    model: Option<String>,
    mixed_models: bool,
    server_tools: Option<ServerToolCounts>,
}

impl ClaudeLedger {
    pub(crate) fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            ids: HashSet::new(),
            bytes: 0,
            max_entries,
            max_bytes,
            frozen: false,
            partial: false,
            categories: None,
            model: None,
            mixed_models: false,
            server_tools: None,
        }
    }

    pub(crate) fn observe(
        &mut self,
        id: Option<&str>,
        usage: Option<&crate::domain::UsageObservation>,
        tools: Option<ServerToolCounts>,
    ) {
        if self.frozen {
            return;
        }
        let Some(id) = id.filter(|id| !id.is_empty()) else {
            self.partial = true;
            return;
        };
        if self.ids.contains(id) {
            return;
        }
        if self.ids.len() >= self.max_entries
            || id.len() > self.max_bytes.saturating_sub(self.bytes)
        {
            self.frozen = true;
            self.partial = true;
            return;
        }
        // Do not reserve an ID until its usage exists: a later block can supply it.
        let Some(usage) = usage else {
            self.partial = true;
            return;
        };
        self.ids.insert(id.to_owned());
        self.bytes += id.len();
        if let Some(a) = self.categories.as_mut() {
            if self.model != usage.model {
                self.mixed_models = true;
            }
            let b = &usage.categories;
            fn add(a: &mut Option<u64>, b: Option<u64>) {
                *a = a.and_then(|x| x.checked_add(b?));
            }
            add(&mut a.input, b.input);
            add(&mut a.cached_input, b.cached_input);
            add(&mut a.cache_write, b.cache_write);
            add(&mut a.cache_write_5m, b.cache_write_5m);
            add(&mut a.cache_write_1h, b.cache_write_1h);
            add(&mut a.cache_read, b.cache_read);
            add(&mut a.output, b.output);
            add(&mut a.reasoning, b.reasoning);
        } else {
            self.categories = Some(usage.categories.clone());
            self.model = usage.model.clone();
        }
        if let Some(tools) = tools {
            let counts = self.server_tools.get_or_insert_default();
            counts.web_search = counts.web_search.saturating_add(tools.web_search);
            counts.web_fetch = counts.web_fetch.saturating_add(tools.web_fetch);
        }
    }

    pub(crate) fn summary(&self, prices: &PriceSchedule) -> Option<UsageSummary> {
        if self.categories.is_none() && !self.partial {
            return None;
        }
        let categories = self.categories.clone().unwrap_or_default();
        let model = if self.mixed_models {
            None
        } else {
            self.model.clone()
        };
        let mut cost = match &model {
            Some(model) if !self.partial => {
                estimate_cost(Provider::Claude, model, &categories, prices)
            }
            _ => unknown("partial usage or mixed models"),
        };
        if cost.class == AggregationClass::Estimated
            && self
                .server_tools
                .is_some_and(|counts| counts.web_search != 0 || counts.web_fetch != 0)
        {
            cost.note = Some("server-tool fees excluded".into());
        }
        Some(UsageSummary {
            total: claude_total(&categories),
            categories,
            model,
            cost,
            class: if self.partial {
                AggregationClass::Partial
            } else {
                AggregationClass::Derived
            },
        })
    }

    pub(crate) fn server_tools(&self) -> Option<ServerToolCounts> {
        self.server_tools
    }
}

/// Codex: the latest cumulative `total_tokens` snapshot. Never a sum of
/// snapshots. Without a reported total, `input + output` (cached input and
/// reasoning are subsets of those).
pub fn codex_total(input: &TokenCategories) -> Option<u64> {
    input
        .total
        .or_else(|| input.input?.checked_add(input.output?))
}

/// Claude: `input + cache_creation + cache_read + output`. Thinking is a
/// subset of output and iterations are not added. Any missing category is `None`.
pub fn claude_total(input: &TokenCategories) -> Option<u64> {
    input
        .input?
        .checked_add(cache_write_total(input)?)?
        .checked_add(input.cache_read?)?
        .checked_add(input.output?)
}

/// OpenCode: the reported session total; otherwise the additive categories.
pub fn opencode_total(input: &TokenCategories) -> Option<u64> {
    if input.total.is_some() {
        return input.total;
    }
    input
        .input?
        .checked_add(input.output?)?
        .checked_add(input.reasoning?)?
        .checked_add(input.cache_read?)?
        .checked_add(cache_write_total(input)?)
}

/// Usage derived from unique OpenCode assistant messages. Only used when the
/// session row reports no totals; never added to reported totals.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct DerivedUsage {
    /// Sums over the messages that reported each category (`None` when none did).
    pub categories: TokenCategories,
    /// Number of unique assistant messages counted.
    pub messages: usize,
    /// Sum of message-reported costs, only when every message reported one.
    pub cost: Option<f64>,
    /// False when any message lacked data or the scan hit its row cap.
    pub complete: bool,
}

/// Accumulates assistant messages by exact id so a repeated id counts once.
pub(crate) struct OpenCodeDerivation {
    seen: HashSet<String>,
    sums: [Option<u64>; 5],
    missing: [bool; 5],
    cost: f64,
    cost_missing: bool,
    complete: bool,
}

impl OpenCodeDerivation {
    pub(crate) fn new() -> Self {
        Self {
            seen: HashSet::new(),
            sums: [None; 5],
            missing: [false; 5],
            cost: 0.0,
            cost_missing: false,
            complete: true,
        }
    }

    /// Counts one assistant message. `tokens` is `None` when the record was
    /// unreadable (oversized or malformed). Repeated ids are ignored.
    pub(crate) fn add(&mut self, id: &str, tokens: Option<&TokenCategories>, cost: Option<f64>) {
        if !self.seen.insert(id.to_owned()) {
            return;
        }
        let Some(t) = tokens else {
            self.missing = [true; 5];
            self.cost_missing = true;
            self.complete = false;
            return;
        };
        let values = [t.input, t.output, t.reasoning, t.cache_read, t.cache_write];
        for (i, value) in values.into_iter().enumerate() {
            match value {
                Some(v) => self.sums[i] = Some(self.sums[i].unwrap_or(0).saturating_add(v)),
                None => self.missing[i] = true,
            }
        }
        match cost {
            Some(c) => self.cost += c,
            None => self.cost_missing = true,
        }
    }

    /// The scan stopped before the last message.
    pub(crate) fn mark_truncated(&mut self) {
        self.complete = false;
    }

    pub(crate) fn finish(self) -> DerivedUsage {
        let pick = |i: usize| self.sums[i];
        let complete = self.complete && !self.missing.iter().any(|m| *m);
        let messages = self.seen.len();
        DerivedUsage {
            categories: TokenCategories {
                input: pick(0),
                output: pick(1),
                reasoning: pick(2),
                cache_read: pick(3),
                cache_write: pick(4),
                ..TokenCategories::default()
            },
            messages,
            cost: (messages > 0 && !self.cost_missing && complete).then_some(self.cost),
            complete,
        }
    }
}

fn has_all_categories(c: &TokenCategories) -> bool {
    [c.input, c.output, c.reasoning, c.cache_read, c.cache_write]
        .iter()
        .all(Option::is_some)
}

fn any_category(c: &TokenCategories) -> bool {
    [c.input, c.output, c.reasoning, c.cache_read, c.cache_write]
        .iter()
        .any(Option::is_some)
}

/// OpenCode session usage. Session columns are authoritative (`Reported`);
/// message-derived totals are used only when every token column is absent and are
/// labeled `Derived` (`Partial` when incomplete). Reported cost, including `0`, wins
/// over everything; otherwise message-reported costs, else a configured price.
pub(crate) fn opencode_summary(
    model: Option<String>,
    session: &TokenCategories,
    session_cost: Option<f64>,
    derived: Option<&DerivedUsage>,
    prices: &PriceSchedule,
) -> Option<UsageSummary> {
    let derived = derived.filter(|d| d.messages > 0);
    let (categories, class) = if any_category(session) {
        let class = if has_all_categories(session) {
            AggregationClass::Reported
        } else {
            AggregationClass::Partial
        };
        (session.clone(), class)
    } else if let Some(d) = derived.filter(|d| any_category(&d.categories)) {
        let class = if d.complete && has_all_categories(&d.categories) {
            AggregationClass::Derived
        } else {
            AggregationClass::Partial
        };
        (d.categories.clone(), class)
    } else if session_cost.is_some() || derived.is_some_and(|d| d.cost.is_some()) {
        (TokenCategories::default(), AggregationClass::Unknown)
    } else {
        return None;
    };
    // A partial sum is not a total.
    let total = match class {
        AggregationClass::Reported | AggregationClass::Derived => opencode_total(&categories),
        _ => None,
    };
    let cost = if let Some(usd) = session_cost {
        CostEstimate {
            usd: Some(usd),
            class: AggregationClass::Reported,
            as_of: None,
            note: None,
        }
    } else if let Some(usd) = derived.and_then(|d| d.cost) {
        CostEstimate {
            usd: Some(usd),
            class: AggregationClass::Derived,
            as_of: None,
            note: Some("sum of unique message-reported costs".into()),
        }
    } else if !matches!(
        class,
        AggregationClass::Reported | AggregationClass::Derived
    ) {
        unknown("token data incomplete")
    } else if let Some(model) = &model {
        estimate_cost(Provider::OpenCode, model, &categories, prices)
    } else {
        unknown("no model recorded")
    };
    Some(UsageSummary {
        model,
        categories,
        total,
        class,
        cost,
    })
}

/// Cache-write tokens: the single total, else the sum of the TTL split.
fn cache_write_total(u: &TokenCategories) -> Option<u64> {
    match (u.cache_write, u.cache_write_5m, u.cache_write_1h) {
        (Some(w), _, _) => Some(w),
        (None, Some(a), Some(b)) => a.checked_add(b),
        _ => None,
    }
}

fn unknown(note: &str) -> CostEstimate {
    CostEstimate {
        usd: None,
        class: AggregationClass::Unknown,
        as_of: None,
        note: Some(note.to_owned()),
    }
}

/// Cost of `tokens` at `price` per million; a zero count needs no price.
fn part(tokens: u64, price: Option<f64>) -> Result<f64, &'static str> {
    if tokens == 0 {
        return Ok(0.0);
    }
    price
        .map(|p| tokens as f64 * p / 1_000_000.0)
        .ok_or("missing price for a used token category")
}

fn need(v: Option<u64>) -> Result<u64, &'static str> {
    v.ok_or("missing token category")
}

/// Estimates cost from an exact provider+model price entry. Returns `Unknown`
/// when the price entry or any required category/price is missing.
pub fn estimate_cost(
    provider: Provider,
    model: &str,
    usage: &TokenCategories,
    prices: &PriceSchedule,
) -> CostEstimate {
    let Some(price) = prices.price_for(provider, model) else {
        return unknown("no price configured for model");
    };
    match compute(provider, usage, price) {
        Ok(usd) => CostEstimate {
            usd: Some(usd),
            class: AggregationClass::Estimated,
            as_of: price.as_of.clone(),
            note: None,
        },
        Err(note) => unknown(note),
    }
}

fn compute(provider: Provider, u: &TokenCategories, p: &PriceEntry) -> Result<f64, &'static str> {
    let input = need(u.input)?;
    let output = need(u.output)?;
    let base = part(output, Some(p.output_per_mtok))?;
    match provider {
        Provider::Codex => {
            let cached = need(u.cached_input)?;
            let write = need(u.cache_write)?;
            if write > 0 {
                // Task 5B probe: subset-vs-additive semantics unverified.
                return Err("codex cache-write accounting unverified");
            }
            let uncached = input.saturating_sub(cached).saturating_sub(write);
            Ok(part(uncached, Some(p.input_per_mtok))?
                + part(cached, p.cached_input_per_mtok)?
                + base)
        }
        Provider::Claude | Provider::OpenCode => {
            let read = need(u.cache_read)?;
            let mut cost =
                part(input, Some(p.input_per_mtok))? + part(read, p.cache_read_per_mtok)? + base;
            cost += match (u.cache_write_5m, u.cache_write_1h) {
                (Some(a), Some(b)) => {
                    part(a, p.cache_write_5m_per_mtok)? + part(b, p.cache_write_1h_per_mtok)?
                }
                _ => part(need(u.cache_write)?, p.cache_write_per_mtok)?,
            };
            if provider == Provider::OpenCode {
                cost += part(need(u.reasoning)?, p.reasoning_per_mtok)?;
            }
            Ok(cost)
        }
    }
}
