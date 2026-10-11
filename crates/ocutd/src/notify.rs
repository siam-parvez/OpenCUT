//! Desktop notifications at usage thresholds, with hysteresis and state
//! persisted in `~/.cache/opencut/notify-state.json` so restarts never
//! repeat a notification.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use ocut_core::Snapshot;
use ocut_core::config::NotifyConfig;
use ocut_core::snapshot::{Status, Window};
use serde::{Deserialize, Serialize};

/// A reset is only announced if it happened this recently (not, say, on
/// the first start after a weekend).
const RESET_NOTICE_WINDOW: Duration = Duration::minutes(30);

/// `resets_at` values closer than this describe the same window. The API
/// and the statusline can report slightly different times for one window
/// (sub-second jitter, rounding), and the local estimate floors to the
/// hour; none of that may re-arm thresholds and repeat notifications.
const SAME_WINDOW_TOLERANCE: Duration = Duration::hours(1);

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowState {
    /// Thresholds already notified in the current window.
    pub fired: Vec<u8>,
    pub resets_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyState {
    pub windows: BTreeMap<String, WindowState>,
    pub token_expired_sent: bool,
}

impl NotifyState {
    /// A missing or corrupt file is an empty state.
    pub fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let body = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        ocut_sources::fsutil::write_private_atomic(path, &body)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Notice {
    pub summary: String,
    pub body: String,
    pub critical: bool,
}

/// Updates `state` from `snap` and returns the notifications to send.
pub fn evaluate(
    state: &mut NotifyState,
    snap: &Snapshot,
    cfg: &NotifyConfig,
    now: DateTime<Utc>,
) -> Vec<Notice> {
    if !cfg.enabled {
        return Vec::new();
    }
    let mut out = Vec::new();
    let windows = [
        ("session_5h", "5h session", snap.windows.session_5h.as_ref()),
        ("weekly_all", "Weekly", snap.windows.weekly_all.as_ref()),
    ];
    for (key, label, w) in windows {
        let ws = state.windows.entry(key.to_owned()).or_default();
        // A new window (reset passed, or a different reset time) re-arms
        // every threshold.
        if let Some(prev) = ws.resets_at {
            let new_window = w.is_some_and(|w| {
                w.resets_at
                    .is_some_and(|r| (r - prev).abs() > SAME_WINDOW_TOLERANCE)
            });
            if prev <= now || new_window {
                if key == "session_5h"
                    && cfg.reset_notice
                    && prev <= now
                    && now - prev <= RESET_NOTICE_WINDOW
                {
                    out.push(Notice {
                        summary: "Claude 5h window reset".into(),
                        body: "Your 5-hour session limit is available again.".into(),
                        critical: false,
                    });
                }
                ws.fired.clear();
                ws.resets_at = None;
            }
        }
        let Some(w) = w else { continue };
        if w.resets_at.is_some() {
            ws.resets_at = w.resets_at;
        }
        let pct = w.pct;
        let hyst = f64::from(cfg.hysteresis);
        ws.fired.retain(|t| pct >= f64::from(*t) - hyst);
        let crossed: Vec<u8> = cfg
            .thresholds
            .iter()
            .copied()
            .filter(|t| pct >= f64::from(*t) && !ws.fired.contains(t))
            .collect();
        if let Some(&top) = crossed.iter().max() {
            ws.fired.extend(crossed);
            ws.fired.sort_unstable();
            out.push(threshold_notice(label, top, w, now));
        }
    }
    let expired = snap.status == Status::TokenExpired;
    if expired && cfg.token_expired && !state.token_expired_sent {
        out.push(Notice {
            summary: "OpenCUT: Claude token expired".into(),
            body: "Run `claude` once to refresh it. Using local estimates meanwhile.".into(),
            critical: false,
        });
    }
    state.token_expired_sent = expired;
    out
}

fn threshold_notice(label: &str, threshold: u8, w: &Window, now: DateTime<Utc>) -> Notice {
    let est = if w.estimated { " (estimate)" } else { "" };
    let mut body = format!("{label} usage is at {:.0}%{est}.", w.pct);
    if let Some(r) = w.resets_at {
        body.push_str(&format!(
            " Resets in {}.",
            ocut_sources::statusline::fmt_duration((r - now).num_seconds())
        ));
    }
    Notice {
        summary: format!("Claude {label}: {threshold}% reached"),
        body,
        critical: threshold >= 90,
    }
}

/// Shows a notification. Blocking (D-Bus); call from a blocking task.
pub fn send(n: &Notice) -> Result<(), String> {
    use notify_rust::{Notification, Urgency};
    Notification::new()
        .appname("OpenCUT")
        .summary(&n.summary)
        .body(&n.body)
        .icon("dialog-information")
        .urgency(if n.critical {
            Urgency::Critical
        } else {
            Urgency::Normal
        })
        .show()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use ocut_core::WindowSource;

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap()
    }

    fn snap(pct: f64, resets_at: DateTime<Utc>) -> Snapshot {
        let mut s = Snapshot::empty(t0());
        s.windows.session_5h = Some(Window {
            pct,
            resets_at: Some(resets_at),
            seconds_to_reset: None,
            source: WindowSource::OauthApi,
            estimated: false,
            fetched_at: None,
            stale: false,
            projection: None,
        });
        s
    }

    fn run(
        st: &mut NotifyState,
        pct: f64,
        reset: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Vec<Notice> {
        evaluate(st, &snap(pct, reset), &NotifyConfig::default(), now)
    }

    #[test]
    fn fires_once_per_threshold_with_hysteresis() {
        let reset = t0() + Duration::hours(3);
        let mut st = NotifyState::default();
        assert!(run(&mut st, 50.0, reset, t0()).is_empty());
        let n = run(&mut st, 76.0, reset, t0());
        assert_eq!(n.len(), 1);
        assert!(n[0].summary.contains("75%"));
        assert!(!n[0].critical);
        assert!(
            run(&mut st, 74.0, reset, t0()).is_empty(),
            "within hysteresis"
        );
        assert!(run(&mut st, 76.0, reset, t0()).is_empty(), "no repeat");
        assert!(run(&mut st, 69.0, reset, t0()).is_empty());
        assert_eq!(
            run(&mut st, 75.0, reset, t0()).len(),
            1,
            "re-armed below 70"
        );
        let n = run(&mut st, 100.0, reset, t0());
        assert_eq!(n.len(), 1, "only the highest of several crossings");
        assert!(n[0].summary.contains("100%") && n[0].critical);
    }

    #[test]
    fn reset_time_jitter_does_not_repeat() {
        let reset = t0() + Duration::hours(3);
        let mut st = NotifyState::default();
        assert_eq!(run(&mut st, 80.0, reset, t0()).len(), 1);
        for jitter_ms in [1, -700, 1_500, -40 * 60 * 1000] {
            let r = reset + Duration::milliseconds(jitter_ms);
            assert!(
                run(&mut st, 80.0, r, t0()).is_empty(),
                "jitter {jitter_ms} ms"
            );
        }
        // A clearly different window re-arms.
        assert_eq!(
            run(&mut st, 80.0, reset + Duration::hours(5), t0()).len(),
            1
        );
    }

    #[test]
    fn survives_restart() {
        let reset = t0() + Duration::hours(3);
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("notify-state.json");
        let mut st = NotifyState::default();
        assert_eq!(run(&mut st, 91.0, reset, t0()).len(), 1);
        st.save(&p).unwrap();
        let mut st = NotifyState::load(&p);
        assert!(run(&mut st, 92.0, reset, t0()).is_empty());
        std::fs::write(&p, "garbage").unwrap();
        assert_eq!(NotifyState::load(&p), NotifyState::default());
    }

    #[test]
    fn reset_notice_and_rearm() {
        let reset = t0() + Duration::hours(1);
        let mut st = NotifyState::default();
        assert_eq!(run(&mut st, 80.0, reset, t0()).len(), 1);
        let later = reset + Duration::minutes(2);
        let n = run(&mut st, 0.0, later + Duration::hours(5), later);
        assert_eq!(n.len(), 1);
        assert!(n[0].summary.contains("reset"));
        assert!(st.windows["session_5h"].fired.is_empty());
        // An old reset (daemon was off for a day) is not announced.
        let mut st = NotifyState::default();
        run(&mut st, 10.0, reset, t0());
        let much_later = reset + Duration::hours(20);
        assert!(run(&mut st, 0.0, much_later + Duration::hours(5), much_later).is_empty());
    }

    #[test]
    fn disabled_and_token_expired() {
        let reset = t0() + Duration::hours(3);
        let cfg = NotifyConfig {
            enabled: false,
            ..Default::default()
        };
        let mut st = NotifyState::default();
        assert!(evaluate(&mut st, &snap(99.0, reset), &cfg, t0()).is_empty());

        let mut s = Snapshot::empty(t0());
        s.status = Status::TokenExpired;
        let cfg = NotifyConfig::default();
        assert_eq!(evaluate(&mut st, &s, &cfg, t0()).len(), 1);
        assert!(evaluate(&mut st, &s, &cfg, t0()).is_empty());
    }
}
