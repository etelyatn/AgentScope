use agentscope::config::{PriceEntry, PriceSchedule};
use agentscope::domain::{AggregationClass, Provider, TokenCategories};
use agentscope::usage::{claude_total, codex_total, estimate_cost, opencode_total};

fn entry(provider: Provider) -> PriceEntry {
    PriceEntry {
        provider,
        model: "m".into(),
        as_of: Some("2026-10-01".into()),
        input_per_mtok: 3.0,
        cached_input_per_mtok: Some(0.3),
        cache_write_per_mtok: Some(3.75),
        cache_write_5m_per_mtok: Some(3.75),
        cache_write_1h_per_mtok: Some(6.0),
        cache_read_per_mtok: Some(0.3),
        output_per_mtok: 15.0,
        reasoning_per_mtok: Some(15.0),
    }
}

fn prices() -> PriceSchedule {
    PriceSchedule::new(vec![
        entry(Provider::Codex),
        entry(Provider::Claude),
        entry(Provider::OpenCode),
    ])
    .unwrap()
}

fn usd(p: Provider, u: &TokenCategories) -> Option<f64> {
    estimate_cost(p, "m", u, &prices()).usd
}

#[test]
fn codex_uses_latest_cumulative_total_not_a_sum() {
    // The caller passes the latest snapshot; the helper returns its reported
    // total and never adds input/output/cached/reasoning on top of it.
    let latest = TokenCategories {
        input: Some(900),
        cached_input: Some(400),
        output: Some(100),
        reasoning: Some(60),
        total: Some(1000),
        ..Default::default()
    };
    assert_eq!(codex_total(&latest), Some(1000));
    let no_total = TokenCategories {
        total: None,
        ..latest
    };
    assert_eq!(codex_total(&no_total), Some(1000));
    assert_eq!(codex_total(&TokenCategories::default()), None);
}

#[test]
fn claude_does_not_add_iterations_or_thinking() {
    let u = TokenCategories {
        input: Some(10),
        cache_write: Some(100),
        cache_read: Some(1000),
        output: Some(50),
        reasoning: Some(30),
        ..Default::default()
    };
    assert_eq!(claude_total(&u), Some(1160));
    // TTL split without a single total sums the split.
    let split = TokenCategories {
        cache_write: None,
        cache_write_5m: Some(60),
        cache_write_1h: Some(40),
        ..u
    };
    assert_eq!(claude_total(&split), Some(1160));
    let missing = TokenCategories {
        cache_read: None,
        ..u
    };
    assert_eq!(claude_total(&missing), None);
}

#[test]
fn opencode_uses_session_totals_and_reported_zero_cost() {
    let u = TokenCategories {
        input: Some(1),
        output: Some(2),
        reasoning: Some(3),
        cache_read: Some(4),
        cache_write: Some(5),
        total: Some(999),
        ..Default::default()
    };
    assert_eq!(opencode_total(&u), Some(999));
    let derived = TokenCategories { total: None, ..u };
    assert_eq!(opencode_total(&derived), Some(15));
    let missing = TokenCategories {
        total: None,
        reasoning: None,
        ..u
    };
    assert_eq!(opencode_total(&missing), None);
    // A reported cost (even 0) is carried by the adapter; estimating from
    // all-zero usage is an estimated $0, not Unknown.
    let zero = TokenCategories {
        input: Some(0),
        output: Some(0),
        reasoning: Some(0),
        cache_read: Some(0),
        cache_write: Some(0),
        ..Default::default()
    };
    let c = estimate_cost(Provider::OpenCode, "m", &zero, &prices());
    assert_eq!(c.usd, Some(0.0));
    assert_eq!(c.class, AggregationClass::Estimated);
}

#[test]
fn missing_price_or_category_is_unknown_not_zero() {
    let full = TokenCategories {
        input: Some(1000),
        cached_input: Some(0),
        cache_write: Some(0),
        output: Some(1000),
        ..Default::default()
    };
    assert!(usd(Provider::Codex, &full).is_some());
    // unknown model
    let c = estimate_cost(Provider::Codex, "other", &full, &prices());
    assert_eq!((c.usd, c.class), (None, AggregationClass::Unknown));
    // missing token category
    let no_out = TokenCategories {
        output: None,
        ..full
    };
    assert_eq!(usd(Provider::Codex, &no_out), None);
    // missing required price for a used category
    let mut e = entry(Provider::Codex);
    e.cached_input_per_mtok = None;
    let sched = PriceSchedule::new(vec![e]).unwrap();
    let used = TokenCategories {
        cached_input: Some(10),
        ..full
    };
    assert_eq!(estimate_cost(Provider::Codex, "m", &used, &sched).usd, None);
    // OpenCode reasoning without a reasoning price
    let mut e = entry(Provider::OpenCode);
    e.reasoning_per_mtok = None;
    let sched = PriceSchedule::new(vec![e]).unwrap();
    let oc = TokenCategories {
        input: Some(1),
        output: Some(1),
        reasoning: Some(5),
        cache_read: Some(0),
        cache_write: Some(0),
        ..Default::default()
    };
    assert_eq!(
        estimate_cost(Provider::OpenCode, "m", &oc, &sched).usd,
        None
    );
}

#[test]
fn codex_cost_subtracts_cached_and_includes_reasoning_in_output() {
    let u = TokenCategories {
        input: Some(1000),
        cached_input: Some(400),
        cache_write: Some(0),
        output: Some(100),
        reasoning: Some(60),
        ..Default::default()
    };
    // 600*3 + 400*0.3 + 100*15 = 1800+120+1500 per million
    let got = usd(Provider::Codex, &u).unwrap();
    assert!((got - 0.00342).abs() < 1e-9, "{got}");
    // cached > input clamps uncached to zero
    let over = TokenCategories {
        cached_input: Some(2000),
        input: Some(1000),
        ..u
    };
    let got = usd(Provider::Codex, &over).unwrap();
    assert!((got - (2000.0 * 0.3 + 1500.0) / 1e6).abs() < 1e-9);
}

#[test]
fn claude_ttl_rates_produce_distinct_costs() {
    let usage = TokenCategories {
        input: Some(0),
        output: Some(0),
        cache_read: Some(0),
        cache_write: Some(1000),
        cache_write_5m: Some(600),
        cache_write_1h: Some(400),
        ..Default::default()
    };
    let got = usd(Provider::Claude, &usage).unwrap();
    assert!((got - 0.00465).abs() < 1e-9, "{got}");
    let c = estimate_cost(Provider::Claude, "m", &usage, &prices());
    assert_eq!(c.class, AggregationClass::Estimated);
    assert_eq!(c.as_of.as_deref(), Some("2026-10-01"));
    // single-write fallback when no split
    let single = TokenCategories {
        cache_write_5m: None,
        cache_write_1h: None,
        ..usage
    };
    let got = usd(Provider::Claude, &single).unwrap();
    assert!((got - 0.00375).abs() < 1e-9, "{got}");
    // thinking is not added on top of output
    let with_thinking = TokenCategories {
        reasoning: Some(999),
        ..usage
    };
    assert_eq!(
        usd(Provider::Claude, &with_thinking),
        usd(Provider::Claude, &usage)
    );
}

#[test]
fn unverified_codex_cache_write_yields_unknown() {
    let u = TokenCategories {
        input: Some(1000),
        cached_input: Some(0),
        cache_write: Some(10),
        output: Some(1),
        ..Default::default()
    };
    let c = estimate_cost(Provider::Codex, "m", &u, &prices());
    assert_eq!((c.usd, c.class), (None, AggregationClass::Unknown));
    assert!(c.note.is_some());
}
