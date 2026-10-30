//! Roster screen rendering (spec §10.2, §10.6, §10.7). Pure: reads `App` and a
//! small context, draws into a `Frame`. No I/O.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::app::{App, FilterState, RosterRow, SortMode, UsageAvailability};
use crate::domain::Timestamp;
use crate::ingest::text::display_safe;
use crate::time::relative_label;

use super::RosterContext;
use super::widgets::{Palette, fit, process_text, provider_name, status_text, tokens_text};

pub const MIN_WIDTH: u16 = 60;
pub const MIN_HEIGHT: u16 = 20;
/// Width at or above which every column is shown.
const FULL_WIDTH: u16 = 90;
/// Width at or above which TOKENS is kept (PROC is dropped first).
const TOKENS_WIDTH: u16 = 70;

const MARKER_W: usize = 2;
const STATUS_W: usize = 13;
const PROVIDER_W: usize = 9;
const LAST_W: usize = 5;
const TOKENS_W: usize = 7;
const PROC_W: usize = 11;

#[derive(Clone, Copy)]
struct Columns {
    tokens: bool,
    process: bool,
    session: usize,
}

impl Columns {
    fn for_width(width: u16) -> Self {
        let tokens = width >= TOKENS_WIDTH;
        let process = width >= FULL_WIDTH;
        let mut fixed = MARKER_W + STATUS_W + 1 + PROVIDER_W + 1 + LAST_W + 1;
        if tokens {
            fixed += TOKENS_W + 1;
        }
        if process {
            fixed += PROC_W + 1;
        }
        // Reserve the separator before each trailing column already counted above.
        Self {
            tokens,
            process,
            session: (width as usize).saturating_sub(fixed).max(8),
        }
    }
}

pub fn render_roster(frame: &mut Frame, app: &App, ctx: &RosterContext) {
    let area = frame.area();
    let palette = Palette::new(ctx.no_color);
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        let msg = format!(
            "Terminal too small: {}x{} (need at least {MIN_WIDTH}x{MIN_HEIGHT}). Resize to continue; q quits.",
            area.width, area.height
        );
        frame.render_widget(Paragraph::new(msg).wrap(Wrap { trim: true }), area);
        return;
    }

    let total = app.total_sessions();
    let banner = banner_line(app, ctx, total);
    let [
        head,
        counts,
        banner_area,
        rule1,
        table_head,
        body,
        rule2,
        footer,
    ] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Length(u16::from(banner.is_some())),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);

    frame.render_widget(Paragraph::new(header_lines(app, ctx, palette)), head);
    frame.render_widget(
        Paragraph::new(Line::styled(
            fit(&counts_text(app), area.width as usize),
            palette.dim(),
        )),
        counts,
    );
    if let Some(line) = banner {
        frame.render_widget(
            Paragraph::new(Line::styled(
                fit(&line, area.width as usize),
                palette.warning(),
            )),
            banner_area,
        );
    }
    let rule = "─".repeat(area.width as usize);
    frame.render_widget(Paragraph::new(rule.as_str()), rule1);
    frame.render_widget(Paragraph::new(rule.as_str()), rule2);
    frame.render_widget(
        Paragraph::new(Line::styled(
            fit(
                &table_header(Columns::for_width(area.width)),
                area.width as usize,
            ),
            palette.title(),
        )),
        table_head,
    );
    frame.render_widget(
        Paragraph::new(Line::styled(
            fit(
                "j/k move  Enter open  s sort  S reverse  r refresh  q quit",
                area.width as usize,
            ),
            palette.dim(),
        )),
        footer,
    );

    let rows = app.visible_rows();
    if total == 0 {
        frame.render_widget(
            Paragraph::new(empty_lines(app, ctx, palette)).wrap(Wrap { trim: false }),
            body,
        );
    } else if rows.is_empty() {
        frame.render_widget(
            Paragraph::new("No sessions match the current filters.").wrap(Wrap { trim: true }),
            body,
        );
    } else {
        render_rows(frame, app, &rows, body, palette);
    }
}

fn render_rows(frame: &mut Frame, app: &App, rows: &[RosterRow<'_>], body: Rect, palette: Palette) {
    let cols = Columns::for_width(body.width);
    let height = body.height as usize;
    let selected = app.selected_index();
    let offset = selected.map_or(0, |s| (s + 1).saturating_sub(height));
    let now = app.now();
    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .skip(offset)
        .take(height)
        .map(|(i, row)| {
            let is_selected = selected == Some(i);
            row_line(row, cols, now, is_selected, palette)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), body);
}

fn row_line(
    row: &RosterRow<'_>,
    cols: Columns,
    now: Timestamp,
    selected: bool,
    palette: Palette,
) -> Line<'static> {
    let s = row.session;
    let (icon, label) = status_text(row.status.label);
    let title = display_safe(&s.title);
    let session = match s.project.as_deref().map(display_safe) {
        Some(project) if !project.is_empty() => format!("{title} · {project}"),
        _ => title,
    };
    let activity = [s.last_persisted_at, s.last_event_at, s.last_completion]
        .into_iter()
        .flatten()
        .max();
    let last = activity.map_or_else(|| "—".to_string(), |t| relative_label(t, now));

    let mut spans = vec![
        Span::raw(if selected { "> " } else { "  " }),
        Span::styled(
            fit(&format!("{icon} {label}"), STATUS_W),
            palette.status(row.status.label),
        ),
        Span::raw(" "),
        Span::raw(fit(provider_name(s.key.provider), PROVIDER_W)),
        Span::raw(" "),
        Span::raw(fit(&session, cols.session)),
        Span::raw(" "),
        Span::raw(fit(&last, LAST_W)),
    ];
    if cols.tokens {
        spans.push(Span::raw(" "));
        let total = s.usage.as_ref().and_then(|u| u.total);
        spans.push(Span::raw(fit(&tokens_text(total), TOKENS_W)));
    }
    if cols.process {
        spans.push(Span::raw(" "));
        spans.push(Span::raw(fit(&process_text(&s.process), PROC_W)));
    }
    let line = Line::from(spans);
    if selected {
        line.style(palette.selected())
    } else {
        line
    }
}

fn table_header(cols: Columns) -> String {
    let mut out = format!(
        "{}{} {} {} {}",
        " ".repeat(MARKER_W),
        fit("STATUS", STATUS_W),
        fit("PROVIDER", PROVIDER_W),
        fit("SESSION / PROJECT", cols.session),
        fit("LAST", LAST_W),
    );
    if cols.tokens {
        out.push(' ');
        out.push_str(&fit("TOKENS", TOKENS_W));
    }
    if cols.process {
        out.push(' ');
        out.push_str(&fit("PROC", PROC_W));
    }
    out
}

fn header_lines(app: &App, ctx: &RosterContext, palette: Palette) -> Vec<Line<'static>> {
    let mut title = vec![
        Span::styled("AgentScope", palette.title()),
        Span::raw("  LOCAL"),
    ];
    for (provider, count) in app.provider_counts() {
        title.push(Span::raw(format!("  {} {count}", provider_name(provider))));
    }
    let live = if app.live && ctx.error.is_none() {
        "Live: on"
    } else {
        "Live: paused"
    };
    title.push(Span::raw(format!("  {live}")));

    let order = if app.sort_ascending() { "asc" } else { "desc" };
    let shown = app.visible_rows().len();
    let filters = Line::from(format!(
        "Filters: {}   Sort: {} ({order})   Showing {shown} of {}",
        filters_text(app.filter()),
        sort_name(app.sort_mode()),
        app.total_sessions(),
    ));
    vec![Line::from(title), filters]
}

fn counts_text(app: &App) -> String {
    let mut parts: Vec<String> = app
        .status_counts()
        .into_iter()
        .filter(|&(_, n)| n > 0)
        .map(|(label, n)| {
            let (icon, text) = status_text(label);
            format!("{icon} {text} {n}")
        })
        .collect();
    if parts.is_empty() {
        parts.push("No sessions yet".into());
    }
    let mut out = parts.join("  ");
    if let Some(scan) = app.scan().filter(|s| !s.done) {
        let total = scan.total.map_or(String::new(), |t| format!("/{t}"));
        out.push_str(&format!(
            "  Scanning {} {}{total}",
            provider_name(scan.provider),
            scan.scanned
        ));
    }
    out
}

fn banner_line(app: &App, ctx: &RosterContext, total: usize) -> Option<String> {
    if let Some(error) = &ctx.error {
        return Some(format!(
            "Worker stopped: {}; r retries",
            display_safe(error)
        ));
    }
    if total == 0 {
        return None; // The empty state lists every warning.
    }
    let warnings = app.warnings();
    let mut items = ctx
        .notes
        .iter()
        .map(|n| display_safe(n))
        .chain(warnings.iter().map(|w| {
            format!(
                "{}: {}",
                provider_name(w.key.provider),
                display_safe(&w.message)
            )
        }));
    let first = items.next()?;
    let more = items.count();
    let suffix = if more > 0 {
        format!(" (+{more} more)")
    } else {
        String::new()
    };
    Some(format!("Warning: {first}{suffix}"))
}

fn empty_lines(app: &App, ctx: &RosterContext, palette: Palette) -> Vec<Line<'static>> {
    let mut lines = vec![Line::styled("No sessions found.", palette.title())];
    for note in &ctx.notes {
        lines.push(Line::styled(display_safe(note), palette.warning()));
    }
    lines.push(Line::from("Checked:"));
    for root in &ctx.roots {
        lines.push(Line::from(format!("  {}", display_safe(root))));
    }
    lines.push(Line::from(format!(
        "Last refresh: {}",
        ctx.last_refresh.as_deref().unwrap_or("not yet completed")
    )));
    if !app.warnings().is_empty() {
        lines.push(Line::from("Warnings:"));
        for w in app.warnings() {
            lines.push(Line::styled(
                format!(
                    "  {}: {}",
                    provider_name(w.key.provider),
                    display_safe(&w.message)
                ),
                palette.warning(),
            ));
        }
    }
    lines
}

fn sort_name(mode: SortMode) -> &'static str {
    match mode {
        SortMode::LastActivity => "last activity",
        SortMode::Provider => "provider",
        SortMode::Project => "project",
        SortMode::Tokens => "tokens",
        SortMode::Status => "status",
    }
}

fn filters_text(filter: &FilterState) -> String {
    let mut parts = Vec::new();
    if let Some(providers) = &filter.providers {
        let names: Vec<_> = providers.iter().map(|&p| provider_name(p)).collect();
        parts.push(format!("provider={}", names.join(",")));
    }
    if let Some(statuses) = &filter.statuses {
        let names: Vec<_> = statuses.iter().map(|&s| status_text(s).1).collect();
        parts.push(format!("status={}", names.join(",")));
    }
    if let Some(project) = &filter.project {
        parts.push(format!("project={}", display_safe(project)));
    }
    match filter.usage {
        UsageAvailability::Any => {}
        UsageAvailability::Known => parts.push("usage=known".into()),
        UsageAvailability::Unknown => parts.push("usage=unknown".into()),
        UsageAvailability::HasCost => parts.push("usage=has cost".into()),
    }
    if !filter.query.is_empty() {
        parts.push(format!("search={}", display_safe(&filter.query)));
    }
    if parts.is_empty() {
        "none".into()
    } else {
        parts.join(" ")
    }
}
