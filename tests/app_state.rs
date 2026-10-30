use std::time::Duration;

use agentscope::app::{App, FilterState, SortMode, UsageAvailability};
use agentscope::domain::*;

const NOW: Timestamp = 10_000_000;

fn thresholds() -> Thresholds {
    Thresholds {
        quiet: Duration::from_secs(300),
        stall: Duration::from_secs(900),
    }
}

fn app() -> App {
    App::new(NOW, thresholds())
}

fn key(provider: Provider, id: &str) -> SessionKey {
    SessionKey {
        provider,
        provider_session_id: id.into(),
        source: SourceKey::File(format!("{id}.jsonl").into()),
    }
}

fn dup_key(provider: Provider, id: &str, file: &str) -> SessionKey {
    SessionKey {
        provider,
        provider_session_id: id.into(),
        source: SourceKey::File(file.into()),
    }
}

fn usage(total: Option<u64>, usd: Option<f64>) -> UsageSummary {
    UsageSummary {
        model: None,
        categories: TokenCategories::default(),
        total,
        class: AggregationClass::Reported,
        cost: CostEstimate {
            usd,
            class: AggregationClass::Estimated,
            as_of: None,
            note: None,
        },
    }
}

/// Active-looking session (recent persisted activity, matched process).
fn session(provider: Provider, id: &str, last: Timestamp) -> SessionSummary {
    SessionSummary {
        key: key(provider, id),
        title: format!("title {id}"),
        title_derived: false,
        project: None,
        cwd: None,
        models: vec![],
        model_details: vec![],
        provider_version: None,
        created_at: None,
        last_event_at: Some(last),
        last_persisted_at: Some(last),
        last_completion: None,
        unfinished_work: false,
        displayed_event_count: 0,
        hidden_records: 0,
        source_available: true,
        archived: false,
        context_window: None,
        server_tools: None,
        lifecycle: Lifecycle::Ongoing,
        process: ProcessAssociation::Unmatched,
        usage: None,
        lineage: Lineage {
            parent: None,
            fork: None,
            sidechain: false,
        },
        warnings: vec![],
    }
}

fn ids(app: &App) -> Vec<String> {
    app.visible_rows()
        .iter()
        .map(|r| r.session.key.provider_session_id.clone())
        .collect()
}

fn selected_id(app: &App) -> Option<String> {
    app.selected_key().map(|k| k.provider_session_id)
}

fn warning(source: &str, message: &str, count: u32) -> Warning {
    Warning {
        key: WarningKey {
            provider: Provider::Codex,
            source: source.into(),
            kind: WarningKind::Parse,
        },
        message: message.into(),
        count,
    }
}

#[test]
fn merges_roster_deltas_and_preserves_selection() {
    let mut app = app();
    app.apply_roster(
        vec![
            session(Provider::Codex, "a", NOW - 1000),
            session(Provider::Claude, "b", NOW - 2000),
            session(Provider::OpenCode, "c", NOW - 3000),
        ],
        vec![],
    );
    // First row selected by default.
    assert_eq!(selected_id(&app).as_deref(), Some("a"));
    app.move_selection(1);
    assert_eq!(selected_id(&app).as_deref(), Some("b"));

    // A new, newer session arrives and `a` is updated; selection stays on `b`.
    let mut a2 = session(Provider::Codex, "a", NOW - 10);
    a2.title = "renamed".into();
    app.apply_roster(
        vec![
            session(Provider::Codex, "z", NOW - 5),
            a2,
            session(Provider::Claude, "b", NOW - 2000),
            session(Provider::OpenCode, "c", NOW - 3000),
        ],
        vec![],
    );
    assert_eq!(ids(&app), ["z", "a", "b", "c"]);
    assert_eq!(selected_id(&app).as_deref(), Some("b"));
    assert_eq!(app.visible_rows()[1].session.title, "renamed");

    // Sessions absent from the new snapshot are removed.
    app.apply_roster(vec![session(Provider::Claude, "b", NOW - 2000)], vec![]);
    assert_eq!(ids(&app), ["b"]);
    assert_eq!(selected_id(&app).as_deref(), Some("b"));
    assert_eq!(app.total_sessions(), 1);

    // Empty roster clears selection.
    app.apply_roster(vec![], vec![]);
    assert!(app.selected_key().is_none());
    assert!(app.visible_rows().is_empty());
}

#[test]
fn counts_providers_and_synthesized_states() {
    let mut app = app();
    let mut done = session(Provider::Claude, "done", NOW - 1000);
    done.lifecycle = Lifecycle::Failed;
    let mut gone = session(Provider::Claude, "gone", NOW - 1000);
    gone.source_available = false;
    app.apply_roster(
        vec![
            session(Provider::Codex, "a", NOW - 1000),
            session(Provider::Codex, "b", NOW - 1000),
            done,
            gone,
        ],
        vec![],
    );
    let providers = app.provider_counts();
    assert_eq!(
        providers,
        vec![
            (Provider::Codex, 2),
            (Provider::Claude, 2),
            (Provider::OpenCode, 0)
        ]
    );
    let states = app.status_counts();
    let count = |l: StatusLabel| states.iter().find(|(x, _)| *x == l).map(|(_, n)| *n);
    assert_eq!(count(StatusLabel::Failed), Some(1));
    assert_eq!(count(StatusLabel::Unavailable), Some(1));
    assert_eq!(states.iter().map(|(_, n)| n).sum::<usize>(), 4);
    // Counts cover discovered sessions, not the filtered view.
    app.set_filter(FilterState {
        providers: Some(vec![Provider::Codex]),
        ..FilterState::default()
    });
    assert_eq!(app.visible_rows().len(), 2);
    assert_eq!(app.provider_counts()[1], (Provider::Claude, 2));
}

#[test]
fn filters_and_sorts_with_unknown_tokens_last() {
    let mut app = app();
    let mut a = session(Provider::Codex, "a", NOW - 3000);
    a.usage = Some(usage(Some(100), None));
    a.project = Some("Zeta".into());
    let mut b = session(Provider::Claude, "b", NOW - 2000);
    b.usage = Some(usage(None, None));
    b.project = Some("alpha".into());
    let mut c = session(Provider::OpenCode, "c", NOW - 1000);
    c.usage = Some(usage(Some(900), None));
    let d = session(Provider::Codex, "d", NOW - 4000); // usage None
    app.apply_roster(vec![a, b, c, d], vec![]);

    // Default: last activity, newest first.
    assert_eq!(ids(&app), ["c", "b", "a", "d"]);

    app.sort(SortMode::Tokens);
    assert_eq!(ids(&app), ["c", "a", "b", "d"], "unknown tokens last");
    app.sort(SortMode::Tokens); // reverse: ascending
    assert_eq!(ids(&app), ["a", "c", "b", "d"], "unknown still last");

    app.sort(SortMode::Provider);
    assert_eq!(ids(&app), ["a", "d", "b", "c"]);
    app.sort(SortMode::Project);
    // case-insensitive ascending, missing project last
    assert_eq!(ids(&app), ["b", "a", "c", "d"]);
    app.sort(SortMode::Project); // reversed: still missing last
    assert_eq!(ids(&app), ["a", "b", "c", "d"]);

    app.sort(SortMode::LastActivity);
    assert_eq!(ids(&app), ["c", "b", "a", "d"]);
    app.sort(SortMode::LastActivity);
    assert_eq!(ids(&app), ["d", "a", "b", "c"]);

    // Provider + status + project filters.
    app.set_filter(FilterState {
        providers: Some(vec![Provider::Codex, Provider::Claude]),
        ..FilterState::default()
    });
    assert_eq!(app.visible_rows().len(), 3);
    app.set_filter(FilterState {
        project: Some("ALPHA".into()),
        ..FilterState::default()
    });
    assert_eq!(ids(&app), ["b"]);
    app.set_filter(FilterState {
        statuses: Some(vec![StatusLabel::Complete]),
        ..FilterState::default()
    });
    assert!(app.visible_rows().is_empty());
    app.set_filter(FilterState {
        statuses: Some(vec![app_status_of(&app, "a")]),
        ..FilterState::default()
    });
    assert_eq!(app.visible_rows().len(), 4, "all share one status here");
}

fn app_status_of(app: &App, id: &str) -> StatusLabel {
    app.all_rows()
        .into_iter()
        .find(|r| r.session.key.provider_session_id == id)
        .unwrap()
        .status
        .label
}

#[test]
fn search_matches_title_project_cwd_and_session_id() {
    let mut app = app();
    let mut a = session(Provider::Codex, "abc-123", NOW - 1);
    a.title = "Inventory API".into();
    let mut b = session(Provider::Claude, "b", NOW - 2);
    b.project = Some("RipperGame".into());
    let mut c = session(Provider::Claude, "c", NOW - 3);
    c.cwd = Some("/work/Sandbox".into());
    app.apply_roster(vec![a, b, c], vec![]);

    let q = |app: &mut App, q: &str| {
        app.set_query(q);
        ids(app)
    };
    assert_eq!(q(&mut app, "inventory"), ["abc-123"]);
    assert_eq!(q(&mut app, "RIPPERgame"), ["b"]);
    assert_eq!(q(&mut app, "sandbox"), ["c"]);
    assert_eq!(q(&mut app, "ABC-1"), ["abc-123"]);
    assert_eq!(q(&mut app, "nomatch"), Vec::<String>::new());
    assert_eq!(q(&mut app, "  "), ["abc-123", "b", "c"]);
}

#[test]
fn removing_the_selected_session_selects_the_nearest_row() {
    let mut app = app();
    let mk = |id: &str, age: i64| session(Provider::Codex, id, NOW - age);
    app.apply_roster(vec![mk("a", 1), mk("b", 2), mk("c", 3), mk("d", 4)], vec![]);
    app.move_selection(2);
    assert_eq!(selected_id(&app).as_deref(), Some("c"));
    app.apply_roster(vec![mk("a", 1), mk("b", 2), mk("d", 4)], vec![]);
    assert_eq!(
        selected_id(&app).as_deref(),
        Some("d"),
        "takes the same slot"
    );

    // Removing the last row moves to the new last row.
    app.apply_roster(vec![mk("a", 1), mk("b", 2)], vec![]);
    assert_eq!(selected_id(&app).as_deref(), Some("b"));

    // Filtering out the selection selects the nearest matching row;
    // a filter that still matches preserves it.
    app.set_query("title a");
    assert_eq!(selected_id(&app).as_deref(), Some("a"));
    app.set_query("");
    assert_eq!(selected_id(&app).as_deref(), Some("a"));
    app.move_selection(5);
    assert_eq!(selected_id(&app).as_deref(), Some("b"), "clamped");
    app.move_selection(-9);
    assert_eq!(selected_id(&app).as_deref(), Some("a"), "clamped");
}

#[test]
fn warning_keys_replace_and_count_instead_of_accumulating() {
    let mut app = app();
    app.apply_roster(vec![], vec![warning("x", "first", 1)]);
    app.apply_roster(vec![], vec![warning("x", "second", 3)]);
    assert_eq!(app.warnings().len(), 1);
    assert_eq!(app.warnings()[0].message, "second");
    assert_eq!(app.warnings()[0].count, 3);
    assert_eq!(app.warning_total(), 3);

    // A single-warning event replaces by key too; a new key adds a banner.
    app.apply_warning(warning("x", "third", 4));
    app.apply_warning(warning("y", "other", 2));
    assert_eq!(app.warnings().len(), 2);
    assert_eq!(app.warning_total(), 6);
    assert_eq!(
        app.warnings()
            .iter()
            .find(|w| w.key.source == "x")
            .unwrap()
            .message,
        "third"
    );

    // Keys absent from the next roster are retired (recovery clears them).
    app.apply_roster(vec![], vec![warning("y", "other", 2)]);
    assert_eq!(app.warnings().len(), 1);
    assert_eq!(app.warnings()[0].key.source, "y");
    app.apply_roster(vec![], vec![]);
    assert!(app.warnings().is_empty());
    assert_eq!(app.warning_total(), 0);
}

#[test]
fn filters_usage_availability_and_composes_with_search() {
    let mut app = app();
    let mut known = session(Provider::Codex, "known", NOW - 1);
    known.usage = Some(usage(Some(10), None));
    let mut cost = session(Provider::Codex, "cost", NOW - 2);
    cost.usage = Some(usage(Some(20), Some(0.5)));
    let mut unk = session(Provider::Claude, "unk", NOW - 3);
    unk.usage = Some(usage(None, None));
    let none = session(Provider::Claude, "none", NOW - 4);
    app.apply_roster(vec![known, cost, unk, none], vec![]);

    let with = |app: &mut App, usage| {
        app.set_filter(FilterState {
            usage,
            ..FilterState::default()
        });
        ids(app)
    };
    assert_eq!(with(&mut app, UsageAvailability::Any).len(), 4);
    assert_eq!(with(&mut app, UsageAvailability::Known), ["known", "cost"]);
    assert_eq!(with(&mut app, UsageAvailability::Unknown), ["unk", "none"]);
    assert_eq!(with(&mut app, UsageAvailability::HasCost), ["cost"]);

    // Composes with search and provider filters.
    app.set_filter(FilterState {
        usage: UsageAvailability::Known,
        query: "KNOWN".into(),
        ..FilterState::default()
    });
    assert_eq!(ids(&app), ["known"]);
    app.set_filter(FilterState {
        usage: UsageAvailability::Unknown,
        providers: Some(vec![Provider::Claude]),
        query: "none".into(),
        ..FilterState::default()
    });
    assert_eq!(ids(&app), ["none"]);
    app.set_filter(FilterState {
        usage: UsageAvailability::HasCost,
        query: "known".into(),
        ..FilterState::default()
    });
    assert!(ids(&app).is_empty());
}

#[test]
fn duplicate_parent_resolution_is_ambiguous_on_equal_recency_and_resolves_newest_otherwise() {
    let parent_target = key(Provider::Codex, "parent");
    let mut child = session(Provider::Codex, "child", NOW - 1);
    child.lineage.parent = Some(SessionLink::Resolved(parent_target.clone()));

    let mut old = session(Provider::Codex, "parent", NOW - 5000);
    old.key = dup_key(Provider::Codex, "parent", "old.jsonl");
    let mut new = session(Provider::Codex, "parent", NOW - 100);
    new.key = dup_key(Provider::Codex, "parent", "new.jsonl");

    let mut app = app();
    app.apply_roster(vec![child.clone(), old.clone(), new.clone()], vec![]);
    assert_eq!(
        app.resolved_parent(&child.key),
        Some(SessionLink::Resolved(new.key.clone()))
    );

    // Equal recency across duplicates: never guess.
    let mut tie = old.clone();
    tie.last_event_at = new.last_event_at;
    tie.last_persisted_at = new.last_persisted_at;
    app.apply_roster(vec![child.clone(), tie, new.clone()], vec![]);
    assert_eq!(
        app.resolved_parent(&child.key),
        Some(SessionLink::Ambiguous)
    );

    // A single match resolves to itself; a missing target keeps the link.
    app.apply_roster(vec![child.clone(), new.clone()], vec![]);
    assert_eq!(
        app.resolved_parent(&child.key),
        Some(SessionLink::Resolved(new.key.clone()))
    );
    app.apply_roster(vec![child.clone()], vec![]);
    assert_eq!(
        app.resolved_parent(&child.key),
        Some(SessionLink::Resolved(parent_target))
    );

    // Unresolved ids resolve against same-provider sessions; Ambiguous is preserved.
    let mut by_id = session(Provider::Codex, "c2", NOW - 2);
    by_id.lineage.parent = Some(SessionLink::Unresolved("parent".into()));
    by_id.lineage.fork = Some(SessionLink::Ambiguous);
    app.apply_roster(vec![by_id.clone(), new.clone()], vec![]);
    assert_eq!(
        app.resolved_parent(&by_id.key),
        Some(SessionLink::Resolved(new.key.clone()))
    );
    assert_eq!(app.resolved_fork(&by_id.key), Some(SessionLink::Ambiguous));
    assert_eq!(app.resolved_parent(&new.key), None);
}

#[test]
fn stale_generations_are_ignored_and_clock_updates_statuses() {
    let mut app = app();
    let scan = ScanProgress {
        provider: Provider::Codex,
        scanned: 1,
        total: Some(1),
        done: true,
    };
    assert!(app.apply_roster_generation(
        2,
        vec![session(Provider::Codex, "a", NOW - 1000)],
        vec![],
        scan
    ));
    assert!(!app.apply_roster_generation(1, vec![], vec![], scan));
    assert_eq!(app.total_sessions(), 1);
    assert_eq!(app.generation(), 2);
    let before = app_status_of(&app, "a");
    app.set_now(NOW + 3_600_000);
    assert_ne!(app_status_of(&app, "a"), before);
    assert_eq!(selected_id(&app).as_deref(), Some("a"));
}

#[test]
fn reset_generation_accepts_a_restarted_worker() {
    let scan = ScanProgress {
        provider: Provider::Codex,
        scanned: 0,
        total: None,
        done: true,
    };
    let mut app = app();
    for g in 1..=4 {
        assert!(app.apply_roster_generation(g, vec![], vec![], scan));
    }
    assert!(!app.apply_roster_generation(1, vec![], vec![], scan));
    app.reset_generation();
    assert_eq!(app.generation(), 0);
    assert!(app.scan().is_none());
    assert!(app.apply_roster_generation(1, vec![], vec![], scan));
}
