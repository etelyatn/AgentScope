//! Session information overlay: token categories, pricing provenance, server
//! tools, and lineage. Pure: reads `App`, draws into a `Frame`; every value is
//! display-safe before it is drawn.

use ratatui::Frame;
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::domain::{
    AggregationClass, Lifecycle, SessionLink, SessionSummary, TokenCategories, UsageSummary,
};
use crate::ingest::text::display_safe;

use super::RosterContext;
use super::details::{chrome, draw_rows, put, wrap_into};
use super::inspector::{class_name, cost_text, grouped, process_line};
use super::widgets::provider_name;

fn count(value: Option<u64>) -> String {
    value.map_or_else(|| "—".into(), grouped)
}

/// One-line category breakdown shared by the session view and event details.
pub(super) fn categories_text(c: &TokenCategories) -> String {
    format!(
        "input {} · cached input {} · cache write {} (5m {} · 1h {}) · cache read {} · output {} · reasoning {} · total {}",
        count(c.input),
        count(c.cached_input),
        count(c.cache_write),
        count(c.cache_write_5m),
        count(c.cache_write_1h),
        count(c.cache_read),
        count(c.output),
        count(c.reasoning),
        count(c.total),
    )
}

fn link_text(link: &Option<SessionLink>) -> String {
    match link {
        None => "none".into(),
        Some(SessionLink::Resolved(key)) => format!(
            "resolved — {} {}",
            provider_name(key.provider),
            display_safe(&key.provider_session_id)
        ),
        Some(SessionLink::Ambiguous) => "ambiguous — more than one candidate session".into(),
        Some(SessionLink::Unresolved(id)) => {
            format!(
                "unresolved — id {} not found in the roster",
                display_safe(id)
            )
        }
    }
}

fn lifecycle_text(l: Lifecycle) -> &'static str {
    match l {
        Lifecycle::Unknown => "unknown",
        Lifecycle::Ongoing => "ongoing",
        Lifecycle::Completed => "completed",
        Lifecycle::Failed => "failed",
        Lifecycle::Cancelled => "cancelled",
    }
}

fn usage_lines(usage: &UsageSummary) -> Vec<String> {
    let mut out = vec![format!(
        "Tokens: {} ({})",
        usage.total.map_or_else(|| "Unknown".into(), grouped),
        class_name(usage.class)
    )];
    if let Some(model) = &usage.model {
        out.push(format!("Priced model: {}", display_safe(model)));
    }
    let c = &usage.categories;
    out.push("Categories:".into());
    for (label, value) in [
        ("Input", c.input),
        ("Cached input", c.cached_input),
        ("Cache write", c.cache_write),
        ("  Cache write 5m", c.cache_write_5m),
        ("  Cache write 1h", c.cache_write_1h),
        ("Cache read", c.cache_read),
        ("Output", c.output),
        ("Reasoning", c.reasoning),
        ("Total", c.total),
    ] {
        out.push(format!("  {label}: {}", count(value)));
    }
    out.push(format!("Cost: {}", cost_text(Some(usage))));
    out.push(format!(
        "Pricing as of: {}",
        usage
            .cost
            .as_of
            .as_deref()
            .map_or_else(|| "—".into(), display_safe)
    ));
    if let Some(note) = &usage.cost.note {
        out.push(format!("Cost note: {}", display_safe(note)));
    }
    if usage.cost.class == AggregationClass::Estimated {
        out.push("Cost is an estimate from the local price table, not a billed amount.".into());
    }
    out
}

pub(super) fn session_rows(
    s: &SessionSummary,
    parent: &Option<SessionLink>,
    fork: &Option<SessionLink>,
    width: usize,
) -> Vec<String> {
    let mut text: Vec<String> = vec![
        format!(
            "Session: {}   Provider: {}",
            display_safe(&s.key.provider_session_id),
            provider_name(s.key.provider)
        ),
        format!(
            "Lifecycle: {}   Unfinished work: {}",
            lifecycle_text(s.lifecycle),
            if s.unfinished_work { "yes" } else { "no" }
        ),
        format!("Process: {}", process_line(&s.process)),
        String::new(),
        "# Usage".into(),
    ];
    match &s.usage {
        Some(usage) => text.extend(usage_lines(usage)),
        None => text.push("No usage recorded for this session.".into()),
    }
    match s.server_tools {
        Some(t) => text.push(format!(
            "Server tools: web_search {} · web_fetch {}",
            t.web_search, t.web_fetch
        )),
        None => text.push("Server tools: Unknown".into()),
    }
    text.push(String::new());
    text.push("# Lineage".into());
    text.push(format!("Parent: {}", link_text(parent)));
    text.push(format!("Fork: {}", link_text(fork)));
    text.push(format!(
        "Sidechain: {}",
        if s.lineage.sidechain { "yes" } else { "no" }
    ));
    let mut rows = Vec::new();
    for line in &text {
        if wrap_into(&mut rows, line, width) {
            break;
        }
    }
    rows
}

pub fn render_session(frame: &mut Frame, app: &App, ctx: &RosterContext) {
    let Some(f) = chrome(frame, ctx) else { return };
    let Some(row) = app.transcript().and_then(|t| app.session(t.key())) else {
        frame.render_widget(
            Paragraph::new("Session is no longer in the roster."),
            f.body,
        );
        return;
    };
    let key = &row.session.key;
    let rows = session_rows(
        row.session,
        &app.resolved_parent(key),
        &app.resolved_fork(key),
        f.width,
    );
    draw_rows(frame, app, &rows, app.session_scroll(), f.body);
    put(
        frame,
        &f,
        "< Session: usage and lineage",
        "Usage is persisted evidence; cost is derived only from the dated price table.",
        "j/k scroll  Esc back  q quit",
    );
}
