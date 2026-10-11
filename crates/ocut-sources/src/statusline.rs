//! Source B: `ocut hook statusline` tees Claude Code's statusline stdin
//! (`rate_limits`) to `$XDG_CACHE_HOME/opencut/statusline.json` and prints
//! one ANSI line. Must return in under 50 ms and work when `rate_limits` is
//! missing.
//!
//! Only `model`, `cost.total_cost_usd`, `version` and `rate_limits` are kept;
//! `session_id`, `cwd`, `transcript_path` and `workspace` are never stored.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use ocut_core::merge::clamp_pct;
use ocut_core::snapshot::{Snapshot, Status, Window};
use ocut_core::time::{parse_timestamp, seconds_until};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::fsutil::write_private_atomic;
use crate::{LimitReading, LimitReadings, Source};

/// A cache older than this is stale; matches `MergePolicy::statusline_max_age`.
pub const STALE_AFTER: Duration = Duration::minutes(10);

/// The cache file contents.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StatuslineCache {
    /// When the hook received stdin. Also the readings' `fetched_at`.
    #[serde(alias = "captured_at")]
    pub received_at: DateTime<Utc>,
    /// 5h and 7d readings; empty when stdin had no `rate_limits`.
    pub readings: LimitReadings,
    /// `version` from the hook input, for `ocut doctor`.
    pub claude_code_version: Option<String>,
    /// `model.id` / `model.display_name`.
    pub model: Option<ModelInfo>,
    /// `cost.total_cost_usd` for the current Claude Code session.
    pub session_cost_usd: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelInfo {
    pub id: Option<String>,
    pub display_name: Option<String>,
}

impl ModelInfo {
    fn label(&self) -> Option<&str> {
        self.display_name.as_deref().or(self.id.as_deref())
    }
}

impl StatuslineCache {
    /// Time since the hook last ran, never negative.
    pub fn age(&self, now: DateTime<Utc>) -> Duration {
        (now - self.received_at).max(Duration::zero())
    }

    /// True when older than [`STALE_AFTER`]; the merge then ignores it.
    pub fn is_stale(&self, now: DateTime<Utc>) -> bool {
        self.age(now) > STALE_AFTER
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StatuslineError {
    #[error("invalid statusline input: {0}")]
    Parse(String),
    #[error("statusline cache IO: {0}")]
    Io(#[from] std::io::Error),
}

/// `$XDG_CACHE_HOME/opencut/statusline.json`, else `~/.cache/opencut/...`.
pub fn default_cache_path(xdg_cache_home: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    xdg_cache_home
        .filter(|p| p.is_absolute())
        .map(Path::to_path_buf)
        .or_else(|| home.map(|h| h.join(".cache")))
        .map(|d| d.join("opencut").join("statusline.json"))
}

/// Parses the hook's stdin JSON. Every field is optional; only a non-object
/// top level is an error.
pub fn parse_hook_input(
    stdin_json: &str,
    now: DateTime<Utc>,
) -> Result<StatuslineCache, StatuslineError> {
    let v: Value =
        serde_json::from_str(stdin_json).map_err(|e| StatuslineError::Parse(e.to_string()))?;
    if !v.is_object() {
        return Err(StatuslineError::Parse("expected a JSON object".into()));
    }
    let str_at = |ptr: &str| v.pointer(ptr).and_then(Value::as_str).map(str::to_owned);
    let model = ModelInfo {
        id: str_at("/model/id"),
        display_name: str_at("/model/display_name"),
    };
    let rl = v.get("rate_limits");
    let reading = |key: &str| {
        rl.and_then(|r| r.get(key))
            .and_then(|w| parse_window(w, now))
    };
    Ok(StatuslineCache {
        received_at: now,
        readings: LimitReadings {
            session_5h: reading("five_hour"),
            weekly_all: reading("seven_day"),
            weekly_opus: reading("seven_day_opus"),
            weekly_sonnet: reading("seven_day_sonnet"),
            extra_usage: None,
        },
        claude_code_version: str_at("/version"),
        model: (model != ModelInfo::default()).then_some(model),
        session_cost_usd: v
            .pointer("/cost/total_cost_usd")
            .and_then(Value::as_f64)
            .filter(|c| c.is_finite()),
    })
}

/// `{used_percentage, resets_at}`; `utilization` is accepted as an alias.
/// A window without a percentage is dropped.
fn parse_window(w: &Value, now: DateTime<Utc>) -> Option<LimitReading> {
    let pct = ["used_percentage", "utilization"]
        .iter()
        .find_map(|k| w.get(*k).and_then(Value::as_f64))
        .filter(|p| p.is_finite())?;
    Some(LimitReading {
        pct: clamp_pct(pct),
        resets_at: w.get("resets_at").and_then(parse_timestamp),
        fetched_at: now,
    })
}

/// Atomic write (temp file in the same directory + rename, mode 0600).
pub fn write_cache(path: &Path, cache: &StatuslineCache) -> Result<(), StatuslineError> {
    let body = serde_json::to_vec_pretty(cache).map_err(std::io::Error::other)?;
    Ok(write_private_atomic(path, &body)?)
}

/// `Ok(None)` when the file does not exist. Use [`StatuslineCache::age`] and
/// [`StatuslineCache::is_stale`] on the result.
pub fn read_cache(path: &Path) -> Result<Option<StatuslineCache>, StatuslineError> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| StatuslineError::Parse(e.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";

/// Green below 75%, yellow below 90%, red from 90% (the notify thresholds).
fn pct_color(pct: f64) -> &'static str {
    if pct >= 90.0 {
        "\x1b[31m"
    } else if pct >= 75.0 {
        "\x1b[33m"
    } else {
        "\x1b[32m"
    }
}

/// `1h29m`, `12m`, `3d4h`.
pub fn fmt_duration(secs: i64) -> String {
    let m = secs.max(0) / 60;
    let (d, h, m) = (m / 1440, (m / 60) % 24, m % 60);
    match (d, h) {
        (0, 0) => format!("{m}m"),
        (0, _) => format!("{h}h{m:02}m"),
        _ => format!("{d}d{h}h"),
    }
}

/// `5h 62% ↻1h29m`, colour by threshold; `~` marks an estimate.
fn fmt_window(
    label: &str,
    pct: Option<f64>,
    resets_at: Option<DateTime<Utc>>,
    estimated: bool,
    now: DateTime<Utc>,
) -> String {
    let Some(pct) = pct else {
        return format!("{DIM}{label} n/a{RESET}");
    };
    let est = if estimated { "~" } else { "" };
    let mut s = format!("{label} {}{est}{pct:.0}%{RESET}", pct_color(pct));
    if let Some(r) = resets_at {
        s.push_str(&format!(
            " {DIM}↻{}{RESET}",
            fmt_duration(seconds_until(now, r))
        ));
    }
    s
}

const SEP: &str = " · ";

/// The one-line ANSI status printed back to Claude Code, from the hook's
/// own input. Windows that already reset show as `n/a`.
pub fn render_line(cache: &StatuslineCache, now: DateTime<Utc>) -> String {
    let win = |label: &str, r: Option<&LimitReading>| {
        let r = r.filter(|r| r.is_current(now));
        fmt_window(
            label,
            r.map(|r| r.pct),
            r.and_then(|r| r.resets_at),
            false,
            now,
        )
    };
    let mut parts = vec![
        win("5h", cache.readings.session_5h.as_ref()),
        win("7d", cache.readings.weekly_all.as_ref()),
    ];
    if let Some(m) = cache.model.as_ref().and_then(ModelInfo::label) {
        parts.push(m.to_owned());
    }
    if let Some(c) = cache.session_cost_usd {
        parts.push(format!("${c:.2}"));
    }
    parts.join(SEP)
}

/// The one-line ANSI status for a merged snapshot (`ocut --format line`).
pub fn render_snapshot_line(snap: &Snapshot, now: DateTime<Utc>) -> String {
    let win = |label: &str, w: Option<&Window>| {
        fmt_window(
            label,
            w.map(|w| w.pct),
            w.and_then(|w| w.resets_at),
            w.is_some_and(|w| w.estimated),
            now,
        )
    };
    let mut parts = vec![
        win("5h", snap.windows.session_5h.as_ref()),
        win("7d", snap.windows.weekly_all.as_ref()),
        format!("${:.2}", snap.cost.session_5h_usd),
    ];
    let stale = snap.status == Status::Stale
        || [&snap.windows.session_5h, &snap.windows.weekly_all]
            .into_iter()
            .flatten()
            .any(|w| w.stale);
    match snap.status {
        Status::Ok if !stale => {}
        Status::Ok | Status::Stale => parts.push(format!("{DIM}stale{RESET}")),
        Status::Degraded => parts.push(format!("{DIM}degraded{RESET}")),
        Status::Offline => parts.push(format!("{DIM}offline{RESET}")),
        Status::TokenExpired => parts.push("\x1b[31mtoken expired\x1b[0m".to_owned()),
        Status::Error => parts.push("\x1b[31merror\x1b[0m".to_owned()),
    }
    parts.join(SEP)
}

/// Reads the cache file on each poll. `None` when the hook never ran.
#[derive(Debug, Clone)]
pub struct StatuslineSource {
    pub path: PathBuf,
}

impl Source for StatuslineSource {
    type Output = Option<StatuslineCache>;
    type Error = StatuslineError;

    fn name(&self) -> &'static str {
        "statusline"
    }

    fn poll(&mut self, _now: DateTime<Utc>) -> Result<Self::Output, Self::Error> {
        read_cache(&self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use ocut_core::snapshot::WindowSource;

    const FULL: &str = include_str!("../../../tests/fixtures/statusline/full.json");
    const NO_LIMITS: &str = include_str!("../../../tests/fixtures/statusline/no_rate_limits.json");
    const PARTIAL: &str = include_str!("../../../tests/fixtures/statusline/partial.json");

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap()
    }

    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut it = s.chars();
        while let Some(c) = it.next() {
            if c == '\x1b' {
                it.by_ref().find(|&c| c == 'm');
            } else {
                out.push(c);
            }
        }
        out
    }

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "ocut-statusline-{tag}-{}-{}",
                std::process::id(),
                Utc::now().timestamp_nanos_opt().unwrap_or(0)
            ));
            Self(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn path() {
        assert_eq!(
            default_cache_path(None, Some(Path::new("/h"))),
            Some(PathBuf::from("/h/.cache/opencut/statusline.json"))
        );
        assert_eq!(
            default_cache_path(Some(Path::new("/x")), Some(Path::new("/h"))),
            Some(PathBuf::from("/x/opencut/statusline.json"))
        );
        // A relative XDG_CACHE_HOME is invalid per the spec and ignored.
        assert_eq!(
            default_cache_path(Some(Path::new("rel")), Some(Path::new("/h"))),
            Some(PathBuf::from("/h/.cache/opencut/statusline.json"))
        );
    }

    #[test]
    fn parses_full_input() {
        let c = parse_hook_input(FULL, now()).unwrap();
        assert_eq!(c.received_at, now());
        assert_eq!(c.claude_code_version.as_deref(), Some("2.3.0"));
        assert_eq!(
            c.model.as_ref().unwrap().id.as_deref(),
            Some("claude-opus-5-5")
        );
        assert_eq!(c.model.as_ref().unwrap().label(), Some("Opus 5.5"));
        assert_eq!(c.session_cost_usd, Some(1.2345));
        let s = c.readings.session_5h.unwrap();
        assert_eq!(s.pct, 62.5);
        assert_eq!(
            s.resets_at,
            Some(Utc.with_ymd_and_hms(2026, 10, 11, 13, 30, 0).unwrap())
        );
        assert_eq!(s.fetched_at, now());
        let w = c.readings.weekly_all.unwrap();
        assert_eq!(w.pct, 34.0);
        assert_eq!(
            w.resets_at,
            Some(Utc.with_ymd_and_hms(2026, 10, 18, 0, 0, 0).unwrap())
        );
        assert!(c.readings.weekly_opus.is_none());
    }

    #[test]
    fn tolerates_missing_rate_limits() {
        let c = parse_hook_input(NO_LIMITS, now()).unwrap();
        assert_eq!(c.readings, LimitReadings::default());
        assert_eq!(c.session_cost_usd, Some(0.05));
        let c = parse_hook_input("{}", now()).unwrap();
        assert_eq!(c.model, None);
        assert_eq!(c.session_cost_usd, None);
        assert_eq!(c.readings, LimitReadings::default());
    }

    #[test]
    fn parses_partial_windows() {
        let c = parse_hook_input(PARTIAL, now()).unwrap();
        let s = c.readings.session_5h.unwrap();
        assert_eq!(s.pct, 100.0, "clamped");
        assert_eq!(s.resets_at, None);
        assert!(c.readings.weekly_all.is_none(), "no percentage, dropped");
        let o = c.readings.weekly_opus.unwrap();
        assert_eq!(
            o.resets_at,
            Some(Utc.with_ymd_and_hms(2026, 10, 18, 0, 0, 0).unwrap())
        );
        assert_eq!(c.model.unwrap().label(), Some("claude-haiku-4-5"));
    }

    #[test]
    fn rejects_bad_input() {
        assert!(matches!(
            parse_hook_input("", now()),
            Err(StatuslineError::Parse(_))
        ));
        assert!(matches!(
            parse_hook_input("[1]", now()),
            Err(StatuslineError::Parse(_))
        ));
        assert!(matches!(
            parse_hook_input("{", now()),
            Err(StatuslineError::Parse(_))
        ));
    }

    #[test]
    fn cache_never_stores_private_fields() {
        let c = parse_hook_input(FULL, now()).unwrap();
        let json = serde_json::to_string(&c).unwrap();
        for needle in ["session_id", "transcript_path", "cwd", "/home/example"] {
            assert!(!json.contains(needle), "{needle} leaked into cache");
        }
    }

    #[test]
    fn write_then_read_roundtrip() {
        let tmp = TempDir::new("rt");
        let path = tmp.0.join("opencut").join("statusline.json");
        assert!(read_cache(&path).unwrap().is_none());
        let c = parse_hook_input(FULL, now()).unwrap();
        write_cache(&path, &c).unwrap();
        assert_eq!(read_cache(&path).unwrap(), Some(c.clone()));
        // Overwrite in place; no temp file is left behind.
        let c2 = parse_hook_input(NO_LIMITS, now() + Duration::minutes(1)).unwrap();
        write_cache(&path, &c2).unwrap();
        assert_eq!(read_cache(&path).unwrap(), Some(c2));
        let names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec!["statusline.json"]);
    }

    #[cfg(unix)]
    #[test]
    fn cache_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new("mode");
        let path = tmp.0.join("statusline.json");
        write_cache(&path, &StatuslineCache::default()).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn read_rejects_garbage() {
        let tmp = TempDir::new("bad");
        fs::create_dir_all(&tmp.0).unwrap();
        let path = tmp.0.join("statusline.json");
        fs::write(&path, "not json").unwrap();
        assert!(matches!(read_cache(&path), Err(StatuslineError::Parse(_))));
    }

    #[test]
    fn reads_legacy_captured_at() {
        let c: StatuslineCache =
            serde_json::from_str(r#"{"captured_at":"2026-10-11T12:00:00Z"}"#).unwrap();
        assert_eq!(c.received_at, now());
    }

    #[test]
    fn age_and_staleness() {
        let c = parse_hook_input(FULL, now()).unwrap();
        assert_eq!(c.age(now() + Duration::minutes(3)), Duration::minutes(3));
        assert!(!c.is_stale(now() + Duration::minutes(10)));
        assert!(c.is_stale(now() + Duration::minutes(10) + Duration::seconds(1)));
        // Clock skew: a cache from the future is age zero, not stale.
        assert_eq!(c.age(now() - Duration::minutes(5)), Duration::zero());
    }

    #[test]
    fn source_polls_cache() {
        let tmp = TempDir::new("src");
        let mut src = StatuslineSource {
            path: tmp.0.join("statusline.json"),
        };
        assert_eq!(src.name(), "statusline");
        assert!(src.poll(now()).unwrap().is_none());
        let c = parse_hook_input(FULL, now()).unwrap();
        write_cache(&src.path, &c).unwrap();
        assert_eq!(src.poll(now()).unwrap(), Some(c));
    }

    #[test]
    fn renders_hook_line() {
        let c = parse_hook_input(FULL, now()).unwrap();
        let line = render_line(&c, now());
        assert_eq!(
            strip_ansi(&line),
            "5h 62% ↻1h30m · 7d 34% ↻6d12h · Opus 5.5 · $1.23"
        );
        assert!(line.contains("\x1b[32m"), "green under 75%");
        assert!(!line.contains('\n'));
    }

    #[test]
    fn renders_hook_line_without_limits() {
        let c = parse_hook_input(NO_LIMITS, now()).unwrap();
        assert_eq!(
            strip_ansi(&render_line(&c, now())),
            "5h n/a · 7d n/a · Sonnet 5 · $0.05"
        );
        assert_eq!(
            strip_ansi(&render_line(&StatuslineCache::default(), now())),
            "5h n/a · 7d n/a"
        );
    }

    #[test]
    fn hook_line_hides_reset_windows() {
        let c = parse_hook_input(FULL, now()).unwrap();
        let later = Utc.with_ymd_and_hms(2026, 10, 11, 14, 0, 0).unwrap();
        assert!(strip_ansi(&render_line(&c, later)).starts_with("5h n/a · 7d 34%"));
    }

    #[test]
    fn colours_by_threshold() {
        assert_eq!(pct_color(74.9), "\x1b[32m");
        assert_eq!(pct_color(75.0), "\x1b[33m");
        assert_eq!(pct_color(90.0), "\x1b[31m");
    }

    #[test]
    fn formats_durations() {
        assert_eq!(fmt_duration(-5), "0m");
        assert_eq!(fmt_duration(59), "0m");
        assert_eq!(fmt_duration(12 * 60), "12m");
        assert_eq!(fmt_duration(89 * 60), "1h29m");
        assert_eq!(fmt_duration(3 * 86400 + 4 * 3600 + 59), "3d4h");
    }

    fn window(pct: f64, source: WindowSource, estimated: bool, stale: bool) -> Window {
        Window {
            pct,
            resets_at: Some(now() + Duration::minutes(89)),
            seconds_to_reset: Some(89 * 60),
            source,
            estimated,
            fetched_at: Some(now()),
            stale,
            projection: None,
        }
    }

    #[test]
    fn renders_snapshot_line() {
        let mut snap = Snapshot::empty(now());
        snap.windows.session_5h = Some(window(91.0, WindowSource::OauthApi, false, false));
        snap.cost.session_5h_usd = 4.5;
        let line = render_snapshot_line(&snap, now());
        assert_eq!(strip_ansi(&line), "5h 91% ↻1h29m · 7d n/a · $4.50");
        assert!(line.contains("\x1b[31m91%"), "red at 90%+");
    }

    #[test]
    fn snapshot_line_marks_estimates_and_status() {
        let mut snap = Snapshot::empty(now());
        snap.windows.session_5h = Some(window(40.0, WindowSource::LocalEstimate, true, false));
        assert_eq!(
            strip_ansi(&render_snapshot_line(&snap, now())),
            "5h ~40% ↻1h29m · 7d n/a · $0.00"
        );
        snap.windows.weekly_all = Some(window(10.0, WindowSource::OauthApi, false, true));
        assert!(strip_ansi(&render_snapshot_line(&snap, now())).ends_with(" · stale"));
        snap.status = Status::TokenExpired;
        assert!(strip_ansi(&render_snapshot_line(&snap, now())).ends_with(" · token expired"));
    }
}
