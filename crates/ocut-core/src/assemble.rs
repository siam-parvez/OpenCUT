//! Builds a full [`Snapshot`] from source outputs (pure, no IO).
//!
//! Callers (the `ocut` CLI in-process path and `ocutd`) gather the inputs;
//! this module applies the merge rule, token and cost periods, per-model
//! breakdown, burn rate, projection and the overall status.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};

use crate::blocks::{active_block, estimate_blocks, local_session_estimate};
use crate::burn::{
    DEFAULT_HALF_LIFE, DEFAULT_LOOKBACK, PctSample, burn_rate, ewma_pct_rate, project,
};
use crate::event::{UsageEvent, sum_tokens};
use crate::merge::{LimitReadings, MergeInputs, MergePolicy, merge_windows};
use crate::plan::{Plan, PlanPreset};
use crate::pricing::PricingTable;
use crate::snapshot::{ErrorEntry, ModelUsage, Snapshot, SourcesHealth, Status, WindowSource};

/// Length of the `week` token/cost period (rolling).
pub const WEEK: Duration = Duration::days(7);
/// Trailing window used for the burn rate.
pub const BURN_WINDOW_MIN: u32 = 15;

/// Everything [`assemble`] needs. `events` must already be deduplicated.
#[derive(Debug, Clone, Copy)]
pub struct AssembleInputs<'a> {
    pub now: DateTime<Utc>,
    /// Start of the local calendar day, for the `today` period.
    pub today_start: DateTime<Utc>,
    pub events: &'a [UsageEvent],
    pub pricing: &'a PricingTable,
    /// Scale for the local 5h estimate; `None` disables it.
    pub preset: Option<PlanPreset>,
    pub plan: Plan,
    /// Source A readings (only when they are the latest successful poll).
    pub oauth: Option<&'a LimitReadings>,
    /// Source B readings.
    pub statusline: Option<&'a LimitReadings>,
    /// Recorded history of the merged 5h pct, for the projection.
    pub pct_history: &'a [PctSample],
    pub active_sessions: u32,
    pub sources: &'a SourcesHealth,
    pub errors: &'a [ErrorEntry],
}

/// Assembles the snapshot.
pub fn assemble(i: AssembleInputs<'_>) -> Snapshot {
    let now = i.now;
    let blocks = estimate_blocks(i.events, i.pricing);
    let local = i.preset.map(|p| local_session_estimate(&blocks, &p, now));
    let mut windows = merge_windows(
        MergeInputs {
            oauth: i.oauth,
            statusline: i.statusline,
            local_session: local.as_ref(),
        },
        &MergePolicy::default(),
        now,
    );

    let mut burn = burn_rate(i.events, i.pricing, now, BURN_WINDOW_MIN);
    if let Some(w) = windows.session_5h.as_mut() {
        let local_samples;
        let samples = match (w.source, i.preset) {
            (WindowSource::LocalEstimate, Some(p)) => {
                local_samples = local_pct_samples(i.events, i.pricing, &p, now);
                &local_samples[..]
            }
            _ => i.pct_history,
        };
        if let Some(rate) = ewma_pct_rate(samples, now, DEFAULT_HALF_LIFE, DEFAULT_LOOKBACK) {
            w.projection = project(w, &rate, now);
            burn.pct_per_hour = rate.pct_per_hour;
        }
    }

    let week_start = now - WEEK;
    let in_range =
        |from: DateTime<Utc>| move |e: &&UsageEvent| e.timestamp >= from && e.timestamp <= now;
    let cost_of = |from: DateTime<Utc>| -> f64 {
        i.events
            .iter()
            .filter(in_range(from))
            .map(|e| i.pricing.cost(e))
            .sum()
    };
    let active = active_block(&blocks, now);

    let mut snap = Snapshot::empty(now);
    snap.account.plan = i.plan;
    snap.windows = windows;
    snap.tokens.session_5h = active.map(|b| b.tokens).unwrap_or_default();
    snap.tokens.today = sum_tokens(i.events.iter().filter(in_range(i.today_start)));
    snap.tokens.week = sum_tokens(i.events.iter().filter(in_range(week_start)));
    snap.cost.session_5h_usd = active.map_or(0.0, |b| b.cost_usd);
    snap.cost.today_usd = cost_of(i.today_start);
    snap.cost.week_usd = cost_of(week_start);
    snap.cost.pricing = i.pricing.version.clone();
    snap.by_model = by_model(i.events.iter().filter(in_range(week_start)), i.pricing);
    snap.burn_rate = burn;
    snap.active_sessions = i.active_sessions;
    snap.sources = i.sources.clone();
    snap.errors = i.errors.to_vec();
    snap.status = derive_status(&snap);
    snap
}

/// Per-model totals over `events`, largest first. `share_pct` is the share
/// of tokens.
pub fn by_model<'a>(
    events: impl Iterator<Item = &'a UsageEvent>,
    pricing: &PricingTable,
) -> Vec<ModelUsage> {
    let mut acc: BTreeMap<&str, (u64, f64)> = BTreeMap::new();
    for e in events {
        let a = acc.entry(e.model.as_str()).or_default();
        a.0 += e.total_tokens();
        a.1 += pricing.cost(e);
    }
    let grand: u64 = acc.values().map(|a| a.0).sum();
    let mut out: Vec<ModelUsage> = acc
        .into_iter()
        .map(|(model, (total, cost))| ModelUsage {
            model: model.to_owned(),
            family: pricing.family(model),
            total,
            est_cost_usd: cost,
            share_pct: if grand == 0 {
                0.0
            } else {
                total as f64 * 100.0 / grand as f64
            },
        })
        .collect();
    out.sort_by(|a, b| b.total.cmp(&a.total).then_with(|| a.model.cmp(&b.model)));
    out
}

/// The local 5h estimate as it would have read every 5 minutes over the
/// lookback, so a projection exists without recorded history.
pub fn local_pct_samples(
    events: &[UsageEvent],
    pricing: &PricingTable,
    preset: &PlanPreset,
    now: DateTime<Utc>,
) -> Vec<PctSample> {
    (0..=6)
        .rev()
        .map(|k| {
            let at = now - Duration::minutes(5 * k);
            let past: Vec<UsageEvent> = events
                .iter()
                .filter(|e| e.timestamp <= at)
                .cloned()
                .collect();
            let blocks = estimate_blocks(&past, pricing);
            PctSample {
                at,
                pct: local_session_estimate(&blocks, preset, at).pct,
            }
        })
        .collect()
}

/// Overall status: `token_expired` > `error` (no source works) > `offline`
/// (OAuth enabled but unreachable) > `degraded` (any other error) > `stale`
/// (a shown window is stale) > `ok`.
pub fn derive_status(s: &Snapshot) -> Status {
    let has = |code: &str| s.errors.iter().any(|e| e.code == code);
    let src = &s.sources;
    if has("token_expired") {
        Status::TokenExpired
    } else if !src.local_logs.ok && !src.oauth_api.ok && !src.statusline.ok {
        Status::Error
    } else if src.oauth_api.enabled && has("offline") {
        Status::Offline
    } else if !s.errors.is_empty() {
        Status::Degraded
    } else if [&s.windows.session_5h, &s.windows.weekly_all]
        .into_iter()
        .flatten()
        .any(|w| w.stale)
    {
        Status::Stale
    } else {
        Status::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merge::LimitReading;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 11, 12, 30, 0).unwrap()
    }

    fn ev(min_ago: i64, model: &str) -> UsageEvent {
        UsageEvent {
            timestamp: now() - Duration::minutes(min_ago),
            model: model.into(),
            input_tokens: 1_000,
            output_tokens: 2_000,
            ..Default::default()
        }
    }

    fn inputs<'a>(
        events: &'a [UsageEvent],
        pricing: &'a PricingTable,
        sources: &'a SourcesHealth,
    ) -> AssembleInputs<'a> {
        AssembleInputs {
            now: now(),
            today_start: Utc.with_ymd_and_hms(2026, 10, 11, 0, 0, 0).unwrap(),
            events,
            pricing,
            preset: Plan::Pro.preset(),
            plan: Plan::Pro,
            oauth: None,
            statusline: None,
            pct_history: &[],
            active_sessions: 2,
            sources,
            errors: &[],
        }
    }

    #[test]
    fn periods_and_models() {
        let pricing = PricingTable::builtin();
        let events = vec![
            ev(5, "claude-opus-5-5"),
            ev(20, "claude-sonnet-5-5"),
            ev(60 * 24, "claude-opus-5-5"),
            ev(60 * 24 * 8, "claude-opus-5-5"),
        ];
        let mut sources = SourcesHealth::default();
        sources.local_logs.ok = true;
        let s = assemble(inputs(&events, &pricing, &sources));
        assert_eq!(s.tokens.today.messages, 2);
        assert_eq!(s.tokens.week.messages, 3);
        assert_eq!(s.tokens.session_5h.messages, 2);
        assert_eq!(s.by_model[0].model, "claude-opus-5-5");
        assert!((s.by_model.iter().map(|m| m.share_pct).sum::<f64>() - 100.0).abs() < 1e-9);
        let w = s.windows.session_5h.as_ref().unwrap();
        assert!(w.estimated);
        assert!(w.projection.is_some());
        assert!(s.windows.weekly_all.is_none());
        assert_eq!(s.status, Status::Ok);
        assert_eq!(s.active_sessions, 2);
        assert!(s.cost.week_usd >= s.cost.today_usd);
    }

    #[test]
    fn oauth_wins_and_status() {
        let pricing = PricingTable::builtin();
        let mut sources = SourcesHealth::default();
        sources.oauth_api.enabled = true;
        sources.oauth_api.ok = true;
        let a = LimitReadings {
            session_5h: Some(LimitReading {
                pct: 40.0,
                resets_at: Some(now() + Duration::hours(1)),
                fetched_at: now(),
            }),
            ..Default::default()
        };
        let mut i = inputs(&[], &pricing, &sources);
        i.oauth = Some(&a);
        let s = assemble(i);
        assert_eq!(
            s.windows.session_5h.as_ref().unwrap().source,
            WindowSource::OauthApi
        );
        assert_eq!(s.status, Status::Ok);

        let errs = [ErrorEntry {
            code: "token_expired".into(),
            message: "run `claude` once".into(),
            at: now(),
        }];
        i.errors = &errs;
        assert_eq!(assemble(i).status, Status::TokenExpired);
    }

    #[test]
    fn nothing_works_is_error() {
        let pricing = PricingTable::builtin();
        let sources = SourcesHealth::default();
        assert_eq!(
            assemble(inputs(&[], &pricing, &sources)).status,
            Status::Error
        );
    }
}
