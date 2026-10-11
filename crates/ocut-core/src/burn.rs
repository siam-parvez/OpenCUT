//! Burn rate and EWMA projection of window utilisation.

use chrono::{DateTime, Duration, Utc};

use crate::event::UsageEvent;
use crate::merge::clamp_pct;
use crate::pricing::PricingTable;
use crate::snapshot::{BurnRate, Confidence, Projection, Window};

/// Token and dollar burn over the trailing `window_min` minutes ending at
/// `now`. `pct_per_hour` is left 0; set it from [`ewma_pct_rate`].
pub fn burn_rate(
    events: &[UsageEvent],
    pricing: &PricingTable,
    now: DateTime<Utc>,
    window_min: u32,
) -> BurnRate {
    let window_min = window_min.max(1);
    let from = now - Duration::minutes(i64::from(window_min));
    let (tokens, usd) = events
        .iter()
        .filter(|e| e.timestamp > from && e.timestamp <= now)
        .fold((0u64, 0.0f64), |(t, c), e| {
            (t + e.total_tokens(), c + pricing.cost(e))
        });
    BurnRate {
        tokens_per_min: tokens as f64 / f64::from(window_min),
        usd_per_hour: usd * 60.0 / f64::from(window_min),
        pct_per_hour: 0.0,
        window_min,
    }
}

/// One observation of a window's percentage.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PctSample {
    pub at: DateTime<Utc>,
    pub pct: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateEstimate {
    /// Smoothed growth in percentage points per hour (never negative).
    pub pct_per_hour: f64,
    /// Samples used (after the most recent reset).
    pub samples: usize,
    /// Time covered by those samples.
    pub span: Duration,
}

/// Default smoothing half-life.
pub const DEFAULT_HALF_LIFE: Duration = Duration::minutes(10);
/// Samples older than this are ignored.
pub const DEFAULT_LOOKBACK: Duration = Duration::minutes(30);

/// Exponentially weighted pct/hour over samples in the last `lookback`.
/// Intervals are weighted by `0.5^(age / half_life)` measured at their
/// midpoint. Samples before the most recent drop (a window reset) are
/// discarded. Returns `None` with fewer than two usable samples.
pub fn ewma_pct_rate(
    samples: &[PctSample],
    now: DateTime<Utc>,
    half_life: Duration,
    lookback: Duration,
) -> Option<RateEstimate> {
    let mut s: Vec<PctSample> = samples
        .iter()
        .copied()
        .filter(|x| x.pct.is_finite() && x.at <= now && now - x.at <= lookback)
        .collect();
    s.sort_by_key(|x| x.at);
    // Keep only the run after the last reset (a drop of more than 1 point).
    if let Some(i) = s.windows(2).rposition(|w| w[1].pct < w[0].pct - 1.0) {
        s.drain(..=i);
    }
    if s.len() < 2 {
        return None;
    }
    let hl = (half_life.num_milliseconds().max(1)) as f64;
    let (mut num, mut den) = (0.0, 0.0);
    for w in s.windows(2) {
        let dt_ms = (w[1].at - w[0].at).num_milliseconds();
        if dt_ms <= 0 {
            continue;
        }
        let rate = (w[1].pct - w[0].pct).max(0.0) / (dt_ms as f64 / 3_600_000.0);
        let mid = w[0].at + Duration::milliseconds(dt_ms / 2);
        let age = (now - mid).num_milliseconds().max(0) as f64;
        let weight = 0.5f64.powf(age / hl);
        num += weight * rate;
        den += weight;
    }
    if den <= 0.0 {
        return None;
    }
    Some(RateEstimate {
        pct_per_hour: num / den,
        samples: s.len(),
        span: s[s.len() - 1].at - s[0].at,
    })
}

fn confidence(rate: &RateEstimate) -> Confidence {
    if rate.samples >= 5 && rate.span >= Duration::minutes(20) {
        Confidence::High
    } else if rate.samples >= 3 && rate.span >= Duration::minutes(5) {
        Confidence::Med
    } else {
        Confidence::Low
    }
}

/// Extrapolates `window.pct` linearly at `rate` until `window.resets_at`.
/// `None` when no window is open.
pub fn project(window: &Window, rate: &RateEstimate, now: DateTime<Utc>) -> Option<Projection> {
    let resets_at = window.resets_at?;
    let pct = clamp_pct(window.pct);
    let per_hour = if rate.pct_per_hour.is_finite() {
        rate.pct_per_hour.max(0.0)
    } else {
        0.0
    };
    let remaining_h = ((resets_at - now).num_milliseconds().max(0)) as f64 / 3_600_000.0;
    let projected = pct + per_hour * remaining_h;
    let hits_limit_at = if pct >= 100.0 {
        Some(now)
    } else if per_hour > 0.0 && projected >= 100.0 {
        let ms = ((100.0 - pct) / per_hour * 3_600_000.0).ceil() as i64;
        Some((now + Duration::milliseconds(ms)).min(resets_at))
    } else {
        None
    };
    Some(Projection {
        pct_at_reset: clamp_pct(projected),
        hits_limit_at,
        confidence: confidence(rate),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::WindowSource;
    use chrono::TimeZone;

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap()
    }

    fn samples(points: &[(i64, f64)]) -> Vec<PctSample> {
        points
            .iter()
            .map(|&(min, pct)| PctSample {
                at: t0() + Duration::minutes(min),
                pct,
            })
            .collect()
    }

    fn window(pct: f64, reset_in_min: i64, now: DateTime<Utc>) -> Window {
        Window {
            pct,
            resets_at: Some(now + Duration::minutes(reset_in_min)),
            seconds_to_reset: None,
            source: WindowSource::OauthApi,
            estimated: false,
            fetched_at: None,
            stale: false,
            projection: None,
        }
    }

    #[test]
    fn constant_rate_is_recovered() {
        // 1 point every 5 minutes = 12 %/h.
        let s = samples(&[
            (0, 10.0),
            (5, 11.0),
            (10, 12.0),
            (15, 13.0),
            (20, 14.0),
            (25, 15.0),
        ]);
        let now = t0() + Duration::minutes(25);
        let r = ewma_pct_rate(&s, now, DEFAULT_HALF_LIFE, DEFAULT_LOOKBACK).unwrap();
        assert!((r.pct_per_hour - 12.0).abs() < 1e-9);
        assert_eq!(r.samples, 6);
        let p = project(&window(15.0, 60, now), &r, now).unwrap();
        assert!((p.pct_at_reset - 27.0).abs() < 1e-6);
        assert!(p.hits_limit_at.is_none());
        assert_eq!(p.confidence, Confidence::High);
    }

    #[test]
    fn reset_discards_older_samples() {
        let s = samples(&[(0, 80.0), (5, 90.0), (10, 2.0), (15, 4.0)]);
        let now = t0() + Duration::minutes(15);
        let r = ewma_pct_rate(&s, now, DEFAULT_HALF_LIFE, DEFAULT_LOOKBACK).unwrap();
        assert_eq!(r.samples, 2);
        assert!((r.pct_per_hour - 24.0).abs() < 1e-9);
        assert_eq!(confidence(&r), Confidence::Low);
    }

    #[test]
    fn hits_limit() {
        let now = t0();
        let r = RateEstimate {
            pct_per_hour: 60.0,
            samples: 3,
            span: Duration::minutes(10),
        };
        let p = project(&window(70.0, 120, now), &r, now).unwrap();
        assert_eq!(p.pct_at_reset, 100.0);
        assert_eq!(p.hits_limit_at, Some(now + Duration::minutes(30)));
        assert_eq!(p.confidence, Confidence::Med);
    }

    #[test]
    fn too_few_samples() {
        let s = samples(&[(0, 10.0)]);
        assert!(ewma_pct_rate(&s, t0(), DEFAULT_HALF_LIFE, DEFAULT_LOOKBACK).is_none());
    }

    #[test]
    fn burn_rate_window() {
        let p = PricingTable::builtin();
        let now = t0();
        let e = |min: i64| UsageEvent {
            timestamp: now - Duration::minutes(min),
            model: "claude-opus-5-5".into(),
            output_tokens: 1_500,
            ..Default::default()
        };
        let b = burn_rate(&[e(1), e(10), e(16)], &p, now, 15);
        assert!((b.tokens_per_min - 200.0).abs() < 1e-9);
        assert!((b.usd_per_hour - 3_000.0 * 20.0 / 1e6 * 4.0).abs() < 1e-9);
        assert_eq!(b.window_min, 15);
    }
}
