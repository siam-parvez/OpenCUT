//! The merge rule (PLAN.md section 2).
//!
//! Limit `pct` / `resets_at` come from the OAuth API (A), then the statusline
//! cache (B) if under 10 minutes old, then the local estimate (C, 5h only).
//! An estimate always sets `estimated: true`; weekly windows have no local
//! estimate and are `None` ("n/a") without A or B.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::snapshot::{ExtraUsage, Window, WindowSource, Windows};
use crate::time::seconds_until;

/// A raw reading of one window from any source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LimitReading {
    /// Utilisation percent; clamped to `0..=100` when merged.
    pub pct: f64,
    /// `None` means no window is open.
    pub resets_at: Option<DateTime<Utc>>,
    /// When the source produced the reading.
    pub fetched_at: DateTime<Utc>,
}

impl LimitReading {
    /// A reading whose window already reset describes a closed window and
    /// must not be shown as current.
    pub fn is_current(&self, now: DateTime<Utc>) -> bool {
        self.resets_at.is_none_or(|r| r > now)
    }

    fn age(&self, now: DateTime<Utc>) -> Duration {
        (now - self.fetched_at).max(Duration::zero())
    }
}

/// Everything one source (A or B) reports. Sources fill what they know.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct LimitReadings {
    pub session_5h: Option<LimitReading>,
    pub weekly_all: Option<LimitReading>,
    pub weekly_opus: Option<LimitReading>,
    pub weekly_sonnet: Option<LimitReading>,
    pub extra_usage: Option<ExtraUsage>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MergePolicy {
    /// B is ignored when older than this.
    pub statusline_max_age: Duration,
    /// A older than this is still used (after a fresh B) but marked stale.
    pub oauth_stale_after: Duration,
}

impl Default for MergePolicy {
    fn default() -> Self {
        Self {
            statusline_max_age: Duration::minutes(10),
            oauth_stale_after: Duration::minutes(10),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MergeInputs<'a> {
    /// Priority A.
    pub oauth: Option<&'a LimitReadings>,
    /// Priority B.
    pub statusline: Option<&'a LimitReadings>,
    /// Priority C, from [`crate::blocks::local_session_estimate`].
    pub local_session: Option<&'a LimitReading>,
}

/// Merges all sources into the snapshot windows. Projections are left
/// `None`; attach them with [`crate::burn::project`].
pub fn merge_windows(inputs: MergeInputs<'_>, policy: &MergePolicy, now: DateTime<Utc>) -> Windows {
    let pick = |f: fn(&LimitReadings) -> Option<&LimitReading>, c: Option<&LimitReading>| {
        merge_one(
            inputs.oauth.and_then(f),
            inputs.statusline.and_then(f),
            c,
            policy,
            now,
        )
    };
    Windows {
        session_5h: pick(|r| r.session_5h.as_ref(), inputs.local_session),
        weekly_all: pick(|r| r.weekly_all.as_ref(), None),
        weekly_opus: pick(|r| r.weekly_opus.as_ref(), None),
        weekly_sonnet: pick(|r| r.weekly_sonnet.as_ref(), None),
        extra_usage: inputs.oauth.and_then(|r| r.extra_usage.clone()),
    }
}

/// Applies the priority order to one window.
pub fn merge_one(
    oauth: Option<&LimitReading>,
    statusline: Option<&LimitReading>,
    local: Option<&LimitReading>,
    policy: &MergePolicy,
    now: DateTime<Utc>,
) -> Option<Window> {
    let oauth = oauth.filter(|r| r.is_current(now));
    let statusline =
        statusline.filter(|r| r.is_current(now) && r.age(now) < policy.statusline_max_age);
    let local = local.filter(|r| r.is_current(now));

    let (r, source, stale) = match (oauth, statusline) {
        (Some(a), _) if a.age(now) <= policy.oauth_stale_after => {
            (a, WindowSource::OauthApi, false)
        }
        (_, Some(b)) => (b, WindowSource::Statusline, false),
        (Some(a), None) => (a, WindowSource::OauthApi, true),
        (None, None) => (local?, WindowSource::LocalEstimate, false),
    };
    Some(window_from(r, source, stale, now))
}

fn window_from(r: &LimitReading, source: WindowSource, stale: bool, now: DateTime<Utc>) -> Window {
    Window {
        pct: clamp_pct(r.pct),
        resets_at: r.resets_at,
        seconds_to_reset: r.resets_at.map(|at| seconds_until(now, at)),
        source,
        estimated: source == WindowSource::LocalEstimate,
        fetched_at: Some(r.fetched_at),
        stale,
        projection: None,
    }
}

/// Clamps to `0..=100`; NaN becomes 0.
pub fn clamp_pct(p: f64) -> f64 {
    if p.is_nan() { 0.0 } else { p.clamp(0.0, 100.0) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap()
    }

    fn r(pct: f64, age_min: i64) -> LimitReading {
        LimitReading {
            pct,
            resets_at: Some(now() + Duration::hours(2)),
            fetched_at: now() - Duration::minutes(age_min),
        }
    }

    fn m(
        a: Option<&LimitReading>,
        b: Option<&LimitReading>,
        c: Option<&LimitReading>,
    ) -> Option<Window> {
        merge_one(a, b, c, &MergePolicy::default(), now())
    }

    #[test]
    fn priority_order() {
        let (a, b, c) = (r(10.0, 1), r(20.0, 1), r(30.0, 0));
        assert_eq!(
            m(Some(&a), Some(&b), Some(&c)).unwrap().source,
            WindowSource::OauthApi
        );
        assert_eq!(
            m(None, Some(&b), Some(&c)).unwrap().source,
            WindowSource::Statusline
        );
        let w = m(None, None, Some(&c)).unwrap();
        assert_eq!(w.source, WindowSource::LocalEstimate);
        assert!(w.estimated);
        assert_eq!(w.seconds_to_reset, Some(7200));
        assert!(m(None, None, None).is_none());
    }

    #[test]
    fn old_statusline_ignored() {
        let (b, c) = (r(20.0, 11), r(30.0, 0));
        assert_eq!(
            m(None, Some(&b), Some(&c)).unwrap().source,
            WindowSource::LocalEstimate
        );
    }

    #[test]
    fn stale_oauth_after_fresh_statusline() {
        let (a, b) = (r(10.0, 30), r(20.0, 2));
        assert_eq!(
            m(Some(&a), Some(&b), None).unwrap().source,
            WindowSource::Statusline
        );
        let w = m(Some(&a), None, Some(&r(1.0, 0))).unwrap();
        assert_eq!(w.source, WindowSource::OauthApi);
        assert!(w.stale);
    }

    #[test]
    fn expired_window_skipped() {
        let mut a = r(90.0, 1);
        a.resets_at = Some(now() - Duration::seconds(1));
        assert_eq!(
            m(Some(&a), None, Some(&r(5.0, 0))).unwrap().source,
            WindowSource::LocalEstimate
        );
    }

    #[test]
    fn weekly_has_no_estimate() {
        let local = r(50.0, 0);
        let w = merge_windows(
            MergeInputs {
                local_session: Some(&local),
                ..Default::default()
            },
            &MergePolicy::default(),
            now(),
        );
        assert!(w.session_5h.is_some());
        assert!(w.weekly_all.is_none());
        assert!(w.weekly_opus.is_none());
    }

    #[test]
    fn clamps() {
        assert_eq!(m(Some(&r(140.0, 0)), None, None).unwrap().pct, 100.0);
        assert_eq!(m(Some(&r(f64::NAN, 0)), None, None).unwrap().pct, 0.0);
        assert_eq!(m(Some(&r(-3.0, 0)), None, None).unwrap().pct, 0.0);
    }
}
