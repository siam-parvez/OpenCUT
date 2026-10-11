//! Snapshot loading and the pure snapshot -> tray view mapping.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use ocut_core::snapshot::SCHEMA_ID;
use ocut_core::{Snapshot, Status, Window, WindowSource};

use crate::icon::Level;

/// A snapshot older than this is shown as stale (3 x the 30 s write interval).
pub const MAX_AGE_S: i64 = 90;
/// Clock skew tolerated for a snapshot dated in the future.
const FUTURE_SKEW_S: i64 = 5;
/// Snapshots are a few KiB; anything larger is refused unread.
pub const MAX_BYTES: u64 = 1 << 20;
/// Longest text taken from the snapshot into a menu line.
const MAX_TEXT: usize = 120;
/// Most snapshot errors listed in the menu.
const MAX_ERRORS: usize = 5;

/// Result of reading the snapshot file.
#[derive(Debug, Clone, PartialEq)]
pub enum Load {
    /// No file: `ocutd` is probably not running.
    Missing,
    /// Present but unreadable, not JSON, or another schema.
    Invalid(String),
    Loaded(Box<Snapshot>),
}

/// `$XDG_RUNTIME_DIR/opencut/usage.json`, falling back to
/// `$XDG_CACHE_HOME/opencut/usage.json` or `~/.cache/opencut/usage.json`
/// (the same rule `ocutd` uses).
pub fn snapshot_path(
    runtime_dir: Option<PathBuf>,
    cache_home: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    let abs = |p: Option<PathBuf>| p.filter(|p| p.is_absolute());
    let dir = match abs(runtime_dir) {
        Some(r) => r.join("opencut"),
        None => abs(cache_home)
            .or_else(|| abs(home).map(|h| h.join(".cache")))?
            .join("opencut"),
    };
    Some(dir.join("usage.json"))
}

/// Reads and parses the snapshot.
pub fn load(path: &Path) -> Load {
    use std::io::Read;
    let file = match std::fs::File::open(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Load::Missing,
        Err(e) => return Load::Invalid(format!("cannot read snapshot: {}", e.kind())),
        Ok(f) => f,
    };
    let mut bytes = Vec::new();
    // Read one byte past the cap to detect an oversized file.
    if let Err(e) = file.take(MAX_BYTES + 1).read_to_end(&mut bytes) {
        return Load::Invalid(format!("cannot read snapshot: {}", e.kind()));
    }
    if bytes.len() as u64 > MAX_BYTES {
        return Load::Invalid("snapshot too large".into());
    }
    parse(&bytes)
}

/// Snapshot text made safe for a single menu line: control characters
/// become spaces and the result is cut to [`MAX_TEXT`] characters.
pub fn clean(text: &str) -> String {
    let mut out: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_TEXT + 1)
        .collect();
    if out.chars().count() > MAX_TEXT {
        out = out.chars().take(MAX_TEXT - 1).collect();
        out.push('…');
    }
    out.trim().to_string()
}

pub fn parse(bytes: &[u8]) -> Load {
    match serde_json::from_slice::<Snapshot>(bytes) {
        Ok(s) if s.schema == SCHEMA_ID => Load::Loaded(Box::new(s)),
        Ok(s) => Load::Invalid(format!("unsupported schema {:?}", clean(&s.schema))),
        Err(e) => Load::Invalid(format!("invalid snapshot (line {})", e.line())),
    }
}

/// What the tray shows.
#[derive(Debug, Clone, PartialEq)]
pub struct View {
    /// 5h percentage for the ring (`None` draws an empty ring).
    pub pct: Option<f64>,
    pub level: Level,
    /// One-line summary: title and tooltip title.
    pub summary: String,
    /// Tooltip body.
    pub details: String,
    /// Informational menu lines.
    pub lines: Vec<String>,
    /// Offer "Start ocutd".
    pub offer_start: bool,
}

impl View {
    /// Shown before the first read.
    pub fn loading() -> Self {
        Self {
            pct: None,
            level: Level::Unknown,
            summary: "OpenCUT: loading…".into(),
            details: String::new(),
            lines: vec!["Loading…".into()],
            offer_start: false,
        }
    }
}

/// `1h29m`, `45m`, `3d20h` (same as `ocut`).
pub fn fmt_duration(secs: i64) -> String {
    let m = secs.max(0) / 60;
    let (d, h, m) = (m / 1440, (m / 60) % 24, m % 60);
    match (d, h) {
        (0, 0) => format!("{m}m"),
        (0, _) => format!("{h}h{m:02}m"),
        _ => format!("{d}d{h}h"),
    }
}

/// `850`, `12.3k`, `1.25M`.
pub fn fmt_tokens(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.2}M", n as f64 / 1e6),
    }
}

/// `$1.12`; a hostile or non-finite value shows as `n/a`, not 300 digits.
pub fn fmt_usd(v: f64) -> String {
    if v.is_finite() && (0.0..1e9).contains(&v) {
        format!("${v:.2}")
    } else {
        "$n/a".into()
    }
}

/// `62%`, `~62%` for an estimate, `n/a` without a window.
pub fn fmt_pct(w: Option<&Window>) -> String {
    match w {
        None => "n/a".into(),
        Some(w) if !w.pct.is_finite() => "n/a".into(),
        // Clamped so a hostile value cannot produce a 300-digit label.
        Some(w) => format!(
            "{}{:.0}%",
            if w.estimated { "~" } else { "" },
            w.pct.clamp(0.0, 999.0)
        ),
    }
}

pub fn status_name(s: Status) -> &'static str {
    match s {
        Status::Ok => "ok",
        Status::Stale => "stale",
        Status::Degraded => "degraded",
        Status::Offline => "offline",
        Status::TokenExpired => "token expired (run `claude` once)",
        Status::Error => "error",
    }
}

fn source_name(s: WindowSource) -> &'static str {
    match s {
        WindowSource::OauthApi => "API",
        WindowSource::Statusline => "statusline",
        WindowSource::LocalEstimate => "local estimate",
    }
}

/// Stale: too old, dated in the future, or flagged stale by `ocutd`.
pub fn is_stale(s: &Snapshot, now: DateTime<Utc>) -> bool {
    let age = now - s.generated_at;
    age > Duration::seconds(MAX_AGE_S)
        || age < -Duration::seconds(FUTURE_SKEW_S)
        || s.status == Status::Stale
        || s.windows.session_5h.as_ref().is_some_and(|w| w.stale)
}

fn reset_in(w: &Window, now: DateTime<Utc>) -> Option<String> {
    w.resets_at.map(|r| fmt_duration((r - now).num_seconds()))
}

/// `5h 62% · resets 1h29m · weekly 34%`.
pub fn summary(s: &Snapshot, now: DateTime<Utc>) -> String {
    let w = &s.windows;
    let five = w.session_5h.as_ref();
    let mut out = format!("5h {}", fmt_pct(five));
    if let Some(r) = five.and_then(|f| reset_in(f, now)) {
        out.push_str(&format!(" · resets {r}"));
    }
    out.push_str(&format!(" · weekly {}", fmt_pct(w.weekly_all.as_ref())));
    out
}

fn window_line(label: &str, w: Option<&Window>, now: DateTime<Utc>) -> String {
    let mut l = format!("{label}: {}", fmt_pct(w));
    if let Some(w) = w {
        if let Some(r) = reset_in(w, now) {
            l.push_str(&format!(" · resets in {r}"));
        }
        if w.estimated {
            l.push_str(" (estimate)");
        }
    }
    l
}

fn ago(s: &Snapshot, now: DateTime<Utc>) -> String {
    let secs = (now - s.generated_at).num_seconds();
    if secs < 60 {
        "just now".into()
    } else {
        format!("{} ago", fmt_duration(secs))
    }
}

/// Maps a load result to the tray view at `now`. `path` is only shown.
pub fn view(load: &Load, path: &Path, now: DateTime<Utc>) -> View {
    let s = match load {
        Load::Missing => {
            return View {
                pct: None,
                level: Level::Unknown,
                summary: "OpenCUT: no data (ocutd not running?)".into(),
                details: format!("No snapshot at {}", path.display()),
                lines: vec![
                    "No usage snapshot".into(),
                    "Is the opencut service running?".into(),
                ],
                offer_start: true,
            };
        }
        Load::Invalid(why) => {
            return View {
                pct: None,
                level: Level::Unknown,
                summary: "OpenCUT: snapshot unreadable".into(),
                details: clean(why),
                lines: vec![format!("Snapshot unreadable: {}", clean(why))],
                offer_start: false,
            };
        }
        Load::Loaded(s) => s,
    };
    let stale = is_stale(s, now);
    let w = &s.windows;
    let five = w.session_5h.as_ref();
    let pct = five.map(|f| f.pct).filter(|p| p.is_finite());
    let level = match pct {
        Some(p) if !stale => Level::from_pct(p),
        _ => Level::Unknown,
    };
    let mut summary = summary(s, now);
    if stale {
        summary.push_str(" · stale");
    }
    let mut status = format!("Status: {}", status_name(s.status));
    if stale && s.status != Status::Stale {
        status.push_str(" (stale)");
    }
    status.push_str(&format!(" · updated {}", ago(s, now)));
    let mut lines = vec![
        window_line("5h session", five, now),
        window_line("Weekly", w.weekly_all.as_ref(), now),
        window_line("Weekly Opus", w.weekly_opus.as_ref(), now),
        window_line("Weekly Sonnet", w.weekly_sonnet.as_ref(), now),
        format!(
            "Today: {} tokens · {}",
            fmt_tokens(s.tokens.today.total),
            fmt_usd(s.cost.today_usd)
        ),
        status,
    ];
    if let Some(f) = five {
        lines.push(format!("Source: {}", source_name(f.source)));
    }
    for e in s.errors.iter().take(MAX_ERRORS) {
        lines.push(format!("! {}", clean(&e.message)));
    }
    if s.errors.len() > MAX_ERRORS {
        lines.push(format!("! … {} more", s.errors.len() - MAX_ERRORS));
    }
    let details = format!(
        "Today: {} tokens · {} (API-equivalent)\nStatus: {}",
        fmt_tokens(s.tokens.today.total),
        fmt_usd(s.cost.today_usd),
        status_name(s.status)
    );
    View {
        pct,
        level,
        summary,
        details,
        lines,
        offer_start: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../../../tests/fixtures/snapshot/usage.v1.example.json");

    fn example() -> (Snapshot, DateTime<Utc>) {
        let Load::Loaded(s) = parse(EXAMPLE.as_bytes()) else {
            panic!("example must parse");
        };
        // 25 s after generation; resets_at 14:00 is then 1h29m30s away.
        let now = s.generated_at + Duration::seconds(25);
        (*s, now)
    }

    fn p() -> PathBuf {
        PathBuf::from("/run/user/1000/opencut/usage.json")
    }

    #[test]
    fn usd_is_bounded() {
        assert_eq!(fmt_usd(1.12), "$1.12");
        assert_eq!(fmt_usd(1e300), "$n/a");
        assert_eq!(fmt_usd(-1.0), "$n/a");
        assert_eq!(fmt_usd(f64::NAN), "$n/a");
    }

    #[test]
    fn path_rule() {
        let pb = |s: &str| Some(PathBuf::from(s));
        assert_eq!(
            snapshot_path(pb("/run/user/1000"), None, pb("/home/u")),
            pb("/run/user/1000/opencut/usage.json")
        );
        assert_eq!(
            snapshot_path(pb("relative"), pb("/c"), pb("/home/u")),
            pb("/c/opencut/usage.json")
        );
        assert_eq!(
            snapshot_path(None, None, pb("/home/u")),
            pb("/home/u/.cache/opencut/usage.json")
        );
        assert_eq!(snapshot_path(None, None, None), None);
    }

    #[test]
    fn formatters() {
        assert_eq!(fmt_duration(5395), "1h29m");
        assert_eq!(fmt_duration(59), "0m");
        assert_eq!(fmt_duration(-10), "0m");
        assert_eq!(fmt_duration(2700), "45m");
        assert_eq!(fmt_duration(331_200), "3d20h");
        assert_eq!(fmt_tokens(850), "850");
        assert_eq!(fmt_tokens(12_345), "12.3k");
        assert_eq!(fmt_tokens(997_200), "997.2k");
        assert_eq!(fmt_tokens(1_250_000), "1.25M");
    }

    #[test]
    fn tooltip_text_for_example() {
        let (s, now) = example();
        assert_eq!(summary(&s, now), "5h 62% · resets 1h29m · weekly 34%");
        let v = view(&Load::Loaded(Box::new(s)), &p(), now);
        assert_eq!(v.summary, "5h 62% · resets 1h29m · weekly 34%");
        assert_eq!(v.pct, Some(62.0));
        assert_eq!(v.level, Level::Ok);
        assert!(!v.offer_start);
        assert_eq!(v.lines[0], "5h session: 62% · resets in 1h29m");
        assert!(v.lines[1].starts_with("Weekly: 34% · resets in 3d"));
        assert!(v.lines[2].starts_with("Weekly Opus: 12%"));
        assert_eq!(v.lines[3], "Weekly Sonnet: n/a");
        assert_eq!(v.lines[4], "Today: 0 tokens · $1.12");
        assert_eq!(v.lines[5], "Status: ok · updated just now");
        assert_eq!(v.lines[6], "Source: API");
    }

    #[test]
    fn levels_follow_thresholds() {
        let (mut s, now) = example();
        for (pct, level) in [(74.0, Level::Ok), (75.0, Level::Warn), (93.0, Level::Crit)] {
            s.windows.session_5h.as_mut().unwrap().pct = pct;
            let v = view(&Load::Loaded(Box::new(s.clone())), &p(), now);
            assert_eq!(v.level, level, "{pct}");
        }
    }

    #[test]
    fn estimate_and_missing_windows() {
        let (mut s, now) = example();
        {
            let f = s.windows.session_5h.as_mut().unwrap();
            f.estimated = true;
            f.resets_at = None;
        }
        s.windows.weekly_all = None;
        assert_eq!(summary(&s, now), "5h ~62% · weekly n/a");
        s.windows.session_5h = None;
        assert_eq!(summary(&s, now), "5h n/a · weekly n/a");
        let v = view(&Load::Loaded(Box::new(s)), &p(), now);
        assert_eq!(v.pct, None);
        assert_eq!(v.level, Level::Unknown);
    }

    #[test]
    fn old_snapshot_is_stale_and_grey() {
        let (s, _) = example();
        let now = s.generated_at + Duration::seconds(MAX_AGE_S + 1);
        assert!(is_stale(&s, now));
        let v = view(&Load::Loaded(Box::new(s.clone())), &p(), now);
        assert_eq!(v.level, Level::Unknown);
        assert_eq!(v.pct, Some(62.0), "stale value still drawn, in grey");
        assert!(v.summary.ends_with(" · stale"), "{}", v.summary);
        assert!(v.lines[5].starts_with("Status: ok (stale) · updated 1m ago"));
        // Not stale exactly at the limit.
        assert!(!is_stale(&s, s.generated_at + Duration::seconds(MAX_AGE_S)));
    }

    #[test]
    fn flagged_or_future_snapshot_is_stale() {
        let (mut s, now) = example();
        s.windows.session_5h.as_mut().unwrap().stale = true;
        assert!(is_stale(&s, now));
        let (mut s, now) = example();
        s.status = Status::Stale;
        assert!(is_stale(&s, now));
        let (s, _) = example();
        assert!(is_stale(&s, s.generated_at - Duration::seconds(60)));
    }

    #[test]
    fn missing_snapshot_offers_start() {
        let v = view(&Load::Missing, &p(), Utc::now());
        assert!(v.offer_start);
        assert_eq!(v.level, Level::Unknown);
        assert_eq!(v.pct, None);
        assert_eq!(v.summary, "OpenCUT: no data (ocutd not running?)");
        assert!(v.details.contains("/run/user/1000/opencut/usage.json"));
    }

    #[test]
    fn invalid_snapshot() {
        assert!(matches!(parse(b"{not json"), Load::Invalid(_)));
        let other = EXAMPLE.replace("opencut/usage@1", "opencut/usage@2");
        assert!(matches!(parse(other.as_bytes()), Load::Invalid(m) if m.contains("usage@2")));
        let v = view(&Load::Invalid("x".into()), &p(), Utc::now());
        assert!(!v.offer_start);
        assert_eq!(v.level, Level::Unknown);
    }

    #[test]
    fn load_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("usage.json");
        assert_eq!(load(&f), Load::Missing);
        std::fs::write(&f, EXAMPLE).unwrap();
        assert!(matches!(load(&f), Load::Loaded(_)));
        std::fs::write(&f, "").unwrap();
        assert!(matches!(load(&f), Load::Invalid(_)));
    }

    #[test]
    fn errors_and_token_expired_are_listed() {
        let (mut s, now) = example();
        s.status = Status::TokenExpired;
        s.errors.push(ocut_core::snapshot::ErrorEntry {
            code: "token_expired".into(),
            message: "OAuth token expired".into(),
            at: now,
        });
        let v = view(&Load::Loaded(Box::new(s)), &p(), now);
        assert!(v.lines.iter().any(|l| l.contains("token expired")));
        assert!(v.lines.iter().any(|l| l == "! OAuth token expired"));
    }

    #[test]
    fn hostile_snapshots_do_not_panic() {
        // Wrong types everywhere.
        let hostile = r#"{"schema":"opencut/usage@1","generated_at":"not a date",
            "status":"weird","windows":{"session_5h":{"pct":250,"resets_at":12345}},
            "tokens":{"today":{"total":-3}},"cost":{"today_usd":null},
            "errors":[null,5,{"code":1}]}"#;
        assert!(matches!(parse(hostile.as_bytes()), Load::Invalid(_)));
        for junk in [&b""[..], b"null", b"[]", b"\xff\xfe", b"{\"schema\":1}"] {
            assert!(matches!(parse(junk), Load::Invalid(_)));
        }
        // Well-typed but absurd values.
        let (mut s, now) = example();
        {
            let f = s.windows.session_5h.as_mut().unwrap();
            f.pct = 1e300;
            f.resets_at = Some(DateTime::<Utc>::MAX_UTC);
        }
        s.windows.weekly_all.as_mut().unwrap().pct = -40.0;
        s.tokens.today.total = u64::MAX;
        for _ in 0..1000 {
            s.errors.push(ocut_core::snapshot::ErrorEntry {
                code: "x".into(),
                message: format!("line1\nline2\u{7}{}", "A".repeat(10_000)),
                at: now,
            });
        }
        let v = view(&Load::Loaded(Box::new(s.clone())), &p(), now);
        assert_eq!(v.level, Level::Crit);
        assert!(v.summary.starts_with("5h 999% · resets "), "{}", v.summary);
        assert!(v.summary.ends_with("weekly 0%"), "{}", v.summary);
        assert!(v.lines.len() < 20);
        for l in &v.lines {
            assert!(l.chars().count() <= 160, "{l}");
            assert!(!l.chars().any(char::is_control), "{l:?}");
        }
        assert_eq!(v.lines.last().unwrap(), "! … 995 more");
        // A snapshot from the far past or future is merely stale.
        s.generated_at = DateTime::<Utc>::MIN_UTC;
        assert!(is_stale(&s, now));
        s.generated_at = DateTime::<Utc>::MAX_UTC;
        assert!(is_stale(&s, now));
        let v = view(&Load::Loaded(Box::new(s)), &p(), now);
        assert_eq!(v.level, Level::Unknown);
    }

    #[test]
    fn clean_strips_controls_and_truncates() {
        assert_eq!(clean("a\nb\tc"), "a b c");
        let long = clean(&"é".repeat(500));
        assert_eq!(long.chars().count(), MAX_TEXT);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn oversized_snapshot_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("usage.json");
        std::fs::write(&f, vec![b' '; (MAX_BYTES + 1) as usize]).unwrap();
        assert_eq!(load(&f), Load::Invalid("snapshot too large".into()));
    }
}
