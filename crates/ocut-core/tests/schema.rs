//! Snapshots produced by `ocut-core` validate against `schema/usage.v1.json`.

use chrono::{Duration, TimeZone, Utc};
use ocut_core::snapshot::{Projection, Snapshot, TokenCounts};
use ocut_core::*;
use serde_json::Value;

const SCHEMA: &str = include_str!("../../../schema/usage.v1.json");
const EXAMPLE: &str = include_str!("../../../tests/fixtures/snapshot/usage.v1.example.json");

fn assert_valid(instance: &Value) {
    let schema: Value = serde_json::from_str(SCHEMA).unwrap();
    let v = jsonschema::validator_for(&schema).expect("schema compiles");
    let errors: Vec<String> = v
        .iter_errors(instance)
        .map(|e| format!("{e} at {}", e.instance_path()))
        .collect();
    assert!(
        errors.is_empty(),
        "schema errors: {errors:#?}\n{instance:#}"
    );
}

#[test]
fn example_fixture_validates_and_parses() {
    let v: Value = serde_json::from_str(EXAMPLE).unwrap();
    assert_valid(&v);
    let s: Snapshot = serde_json::from_value(v).unwrap();
    assert_eq!(s.schema, SCHEMA_ID);
    assert_eq!(s.account.plan, Plan::Pro);
    assert!(s.windows.weekly_sonnet.is_none());
    assert_eq!(s.tokens.today, TokenCounts::default());
    // Round trip stays valid.
    assert_valid(&serde_json::to_value(&s).unwrap());
}

#[test]
fn empty_snapshot_validates() {
    let s = Snapshot::empty(Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap());
    assert_valid(&serde_json::to_value(&s).unwrap());
}

#[test]
fn pipeline_snapshot_validates() {
    let now = Utc.with_ymd_and_hms(2026, 10, 11, 12, 30, 0).unwrap();
    let pricing = PricingTable::builtin();
    let events: Vec<UsageEvent> = (0..20)
        .map(|i| UsageEvent {
            timestamp: now - Duration::minutes(i * 7),
            model: if i % 2 == 0 {
                "claude-opus-5-5"
            } else {
                "claude-sonnet-5-5"
            }
            .into(),
            message_id: Some(format!("msg_{i}")),
            request_id: Some(format!("req_{i}")),
            input_tokens: 100,
            output_tokens: 2_000,
            cache_write_5m_tokens: 5_000,
            cache_read_tokens: 40_000,
            ..Default::default()
        })
        .collect();
    let events = dedup_events(events);
    let blocks = estimate_blocks(&events, &pricing);
    let preset = Plan::Pro.preset().unwrap();
    let local = local_session_estimate(&blocks, &preset, now);
    let mut windows = merge_windows(
        MergeInputs {
            local_session: Some(&local),
            ..Default::default()
        },
        &MergePolicy::default(),
        now,
    );
    let samples: Vec<PctSample> = (0..6)
        .map(|i| PctSample {
            at: now - Duration::minutes(25 - i * 5),
            pct: 40.0 + i as f64 * 2.0,
        })
        .collect();
    let rate = ewma_pct_rate(
        &samples,
        now,
        burn::DEFAULT_HALF_LIFE,
        burn::DEFAULT_LOOKBACK,
    )
    .unwrap();
    if let Some(w) = windows.session_5h.as_mut() {
        w.projection = project(w, &rate, now);
        assert!(w.estimated);
    }
    let mut snap = Snapshot::empty(now);
    snap.windows = windows;
    snap.account.plan = Plan::Pro;
    snap.cost.pricing = pricing.version.clone();
    snap.burn_rate = burn_rate(&events, &pricing, now, 15);
    snap.burn_rate.pct_per_hour = rate.pct_per_hour;
    snap.tokens.session_5h = active_block(&blocks, now).unwrap().tokens;
    snap.errors.push(snapshot::ErrorEntry {
        code: "token_expired".into(),
        message: "run `claude` once".into(),
        at: now,
    });
    let v = serde_json::to_value(&snap).unwrap();
    assert_valid(&v);
    assert!(v["windows"]["weekly_all"].is_null());
}

#[test]
fn schema_rejects_out_of_range_and_bad_estimate() {
    let schema: Value = serde_json::from_str(SCHEMA).unwrap();
    let v = jsonschema::validator_for(&schema).unwrap();
    let mut x: Value = serde_json::from_str(EXAMPLE).unwrap();
    x["windows"]["session_5h"]["pct"] = 101.0.into();
    assert!(!v.is_valid(&x));
    let mut y: Value = serde_json::from_str(EXAMPLE).unwrap();
    y["windows"]["session_5h"]["source"] = "local_estimate".into();
    assert!(!v.is_valid(&y), "local_estimate must set estimated: true");
    let _ = Projection {
        pct_at_reset: 0.0,
        hits_limit_at: None,
        confidence: Default::default(),
    };
}
