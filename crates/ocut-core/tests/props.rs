//! Property tests for the pure algorithms.

use chrono::{DateTime, Duration, TimeZone, Utc};
use ocut_core::snapshot::{Window, WindowSource};
use ocut_core::*;
use proptest::prelude::*;

fn base() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap()
}

prop_compose! {
    fn arb_event()(min in 0i64..60 * 24 * 14, inp in 0u64..200_000, out in 0u64..50_000,
                   cw in 0u64..100_000, cr in 0u64..2_000_000, model in 0usize..4) -> UsageEvent {
        UsageEvent {
            timestamp: base() + Duration::minutes(min),
            model: ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5", "unknown-model"][model].into(),
            input_tokens: inp,
            output_tokens: out,
            cache_write_5m_tokens: cw,
            cache_read_tokens: cr,
            ..Default::default()
        }
    }
}

fn arb_reading() -> impl Strategy<Value = Option<LimitReading>> {
    proptest::option::of((-50.0f64..200.0, -600i64..20_000, 0i64..120).prop_map(
        |(pct, reset_s, age_min)| LimitReading {
            pct,
            resets_at: Some(base() + Duration::seconds(reset_s)),
            fetched_at: base() - Duration::minutes(age_min),
        },
    ))
}

proptest! {
    #[test]
    fn blocks_are_well_formed(events in proptest::collection::vec(arb_event(), 0..80)) {
        let p = PricingTable::builtin();
        let blocks = estimate_blocks(&events, &p);
        let total: u64 = blocks.iter().map(|b| b.tokens.messages).sum();
        prop_assert_eq!(total, events.len() as u64);
        for b in &blocks {
            prop_assert_eq!(b.end - b.start, blocks::BLOCK_DURATION);
            prop_assert_eq!(b.start, blocks::floor_hour(b.start));
            prop_assert!(b.start <= b.first_event && b.first_event <= b.last_event && b.last_event < b.end);
            prop_assert!(b.cost_usd >= 0.0 && b.cost_usd.is_finite());
        }
        for w in blocks.windows(2) {
            prop_assert!(w[0].end <= w[1].start || w[1].first_event >= w[0].end
                || w[1].first_event - w[0].last_event >= blocks::BLOCK_DURATION);
            prop_assert!(w[0].last_event < w[1].first_event);
        }
    }

    #[test]
    fn local_estimate_in_range_and_reset_in_future(
        events in proptest::collection::vec(arb_event(), 1..60),
        now_min in 0i64..60 * 24 * 15,
        plan in prop_oneof![Just(Plan::Pro), Just(Plan::Max5x), Just(Plan::Max20x)],
    ) {
        let p = PricingTable::builtin();
        let now = base() + Duration::minutes(now_min);
        let r = local_session_estimate(&estimate_blocks(&events, &p), &plan.preset().unwrap(), now);
        prop_assert!((0.0..=100.0).contains(&r.pct));
        if let Some(reset) = r.resets_at {
            prop_assert!(reset > now);
            prop_assert!(reset - now <= blocks::BLOCK_DURATION);
        }
    }

    #[test]
    fn merged_windows_are_valid(a in arb_reading(), b in arb_reading(), c in arb_reading()) {
        let now = base();
        if let Some(w) = merge::merge_one(a.as_ref(), b.as_ref(), c.as_ref(), &MergePolicy::default(), now) {
            prop_assert!((0.0..=100.0).contains(&w.pct));
            if let Some(reset) = w.resets_at {
                prop_assert!(reset > now);
                prop_assert!(w.seconds_to_reset.unwrap() >= 0);
            }
            prop_assert_eq!(w.estimated, w.source == WindowSource::LocalEstimate);
        }
    }

    #[test]
    fn projection_in_range(
        pct in -10.0f64..150.0,
        rate in -50.0f64..500.0,
        reset_min in 0i64..600,
        samples in 2usize..10,
    ) {
        let now = base();
        let w = Window {
            pct, resets_at: Some(now + Duration::minutes(reset_min)), seconds_to_reset: None,
            source: WindowSource::OauthApi, estimated: false, fetched_at: None, stale: false, projection: None,
        };
        let r = RateEstimate { pct_per_hour: rate, samples, span: Duration::minutes(10) };
        let p = project(&w, &r, now).unwrap();
        prop_assert!((0.0..=100.0).contains(&p.pct_at_reset));
        if let Some(hit) = p.hits_limit_at {
            prop_assert!(hit >= now && hit <= now + Duration::minutes(reset_min));
        }
    }

    #[test]
    fn ewma_rate_non_negative(pcts in proptest::collection::vec(0.0f64..100.0, 0..20)) {
        let now = base() + Duration::minutes(30);
        let s: Vec<PctSample> = pcts.iter().enumerate()
            .map(|(i, &pct)| PctSample { at: base() + Duration::minutes(i as i64), pct })
            .collect();
        if let Some(r) = ewma_pct_rate(&s, now, burn::DEFAULT_HALF_LIFE, burn::DEFAULT_LOOKBACK) {
            prop_assert!(r.pct_per_hour >= 0.0 && r.pct_per_hour.is_finite());
        }
    }

    #[test]
    fn timestamp_parse_never_panics(s in ".*") {
        let _ = time::parse_timestamp_str(&s);
    }

    #[test]
    fn cost_is_finite(e in arb_event()) {
        let c = PricingTable::builtin().cost(&e);
        prop_assert!(c.is_finite() && c >= 0.0);
    }
}

/// DST: blocks are floored in UTC, so a local DST change (Europe, 2026-03-29)
/// does not shift block boundaries.
#[test]
fn dst_transition_is_utc_based() {
    let p = PricingTable::builtin();
    let berlin_before = chrono::FixedOffset::east_opt(3600).unwrap();
    let berlin_after = chrono::FixedOffset::east_opt(7200).unwrap();
    let e1 = berlin_before
        .with_ymd_and_hms(2026, 3, 29, 1, 30, 0)
        .unwrap()
        .with_timezone(&Utc);
    let e2 = berlin_after
        .with_ymd_and_hms(2026, 3, 29, 3, 30, 0)
        .unwrap()
        .with_timezone(&Utc);
    let ev = |t| UsageEvent {
        timestamp: t,
        model: "claude-opus-5-5".into(),
        output_tokens: 10,
        ..Default::default()
    };
    let blocks = estimate_blocks(&[ev(e1), ev(e2)], &p);
    assert_eq!(blocks.len(), 1);
    assert_eq!(
        blocks[0].start,
        Utc.with_ymd_and_hms(2026, 3, 29, 0, 0, 0).unwrap()
    );
    assert_eq!(
        blocks[0].end,
        Utc.with_ymd_and_hms(2026, 3, 29, 5, 0, 0).unwrap()
    );
}

/// Half-hour offsets (India, +05:30) floor to the UTC hour, not the local one.
#[test]
fn half_hour_timezone() {
    let p = PricingTable::builtin();
    let ist = chrono::FixedOffset::east_opt(5 * 3600 + 1800).unwrap();
    let t = ist
        .with_ymd_and_hms(2026, 10, 11, 10, 10, 0)
        .unwrap()
        .with_timezone(&Utc); // 04:40Z
    let blocks = estimate_blocks(
        &[UsageEvent {
            timestamp: t,
            model: "claude-opus-5-5".into(),
            ..Default::default()
        }],
        &p,
    );
    assert_eq!(
        blocks[0].start,
        Utc.with_ymd_and_hms(2026, 10, 11, 4, 0, 0).unwrap()
    );
}
