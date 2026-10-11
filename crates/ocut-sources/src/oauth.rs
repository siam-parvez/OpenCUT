//! Source A: `GET https://api.anthropic.com/api/oauth/usage`.
//!
//! Opt-in (`sources.oauth_api = "on"`). Accepts both response shapes: flat
//! `five_hour` / `seven_day*` keys and the `limits[]` array of
//! `{kind: session|weekly_all|weekly_scoped, percent, resets_at}`.
//! `resets_at` may be epoch seconds or ISO; normalise with
//! [`ocut_core::time::parse_timestamp`]. 401 means `token_expired`; never
//! refresh the token. Only ever send the token to [`USAGE_URL`]'s host over
//! rustls.

use std::time::Duration;

use chrono::{DateTime, Utc};
use reqwest::StatusCode;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderValue, RETRY_AFTER};
use serde_json::Value;

use crate::credentials::{AccessToken, OAuthCredentials};
use crate::{LimitReading, LimitReadings, Source};
use ocut_core::config::PollingConfig;
use ocut_core::snapshot::ExtraUsage;
use ocut_core::time::parse_timestamp;

pub const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
pub const BETA_HEADER: (&str, &str) = ("anthropic-beta", "oauth-2025-04-20");

#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    /// `expiresAt` passed or HTTP 401: tell the user to run `claude` once.
    #[error("OAuth token expired; run `claude` once to refresh it")]
    TokenExpired,
    #[error("rate limited")]
    RateLimited { retry_after: Option<Duration> },
    #[error("server error (HTTP {status})")]
    Server {
        status: u16,
        retry_after: Option<Duration>,
    },
    #[error("unexpected HTTP status {status}")]
    Status { status: u16 },
    /// Transport failure. Must not contain headers or the token.
    #[error("network error: {0}")]
    Network(String),
    /// Body did not match either known shape. Must not quote the body.
    #[error("unrecognised usage response: {0}")]
    Parse(String),
}

impl OAuthError {
    /// Snapshot `errors[].code`.
    pub fn code(&self) -> &'static str {
        match self {
            OAuthError::TokenExpired => "token_expired",
            OAuthError::RateLimited { .. } => "rate_limited",
            OAuthError::Server { .. } | OAuthError::Status { .. } => "http_error",
            OAuthError::Network(_) => "offline",
            OAuthError::Parse(_) => "schema_drift",
        }
    }
}

/// Parses a 200 response body (either shape). Unknown fields are ignored.
///
/// Flat keys and `limits[]` may both be present; `limits[]` entries win.
/// A window object of `null` means the window does not apply; a
/// `resets_at` of `null` means no window is currently open.
pub fn parse_usage_response(
    body: &str,
    fetched_at: DateTime<Utc>,
) -> Result<LimitReadings, OAuthError> {
    // serde_json messages can quote the body; report only the position.
    let root: Value = serde_json::from_str(body).map_err(|e| {
        OAuthError::Parse(format!(
            "invalid JSON at line {} column {}",
            e.line(),
            e.column()
        ))
    })?;
    let obj = root
        .as_object()
        .ok_or_else(|| OAuthError::Parse("top level is not an object".into()))?;

    let mut out = LimitReadings::default();
    let mut recognised = false;

    for (key, slot) in [
        ("five_hour", &mut out.session_5h),
        ("seven_day", &mut out.weekly_all),
        ("seven_day_opus", &mut out.weekly_opus),
        ("seven_day_sonnet", &mut out.weekly_sonnet),
    ] {
        if let Some(v) = obj.get(key) {
            recognised = true;
            *slot = flat_window(v, fetched_at);
        }
    }
    if let Some(v) = obj.get("extra_usage") {
        recognised = true;
        out.extra_usage = extra_usage(v);
    }

    if let Some(limits) = obj.get("limits") {
        let limits = limits
            .as_array()
            .ok_or_else(|| OAuthError::Parse("`limits` is not an array".into()))?;
        recognised = true;
        for entry in limits {
            let Some((slot, reading)) = limit_entry(entry, fetched_at) else {
                continue;
            };
            let target = match slot {
                Slot::Session => &mut out.session_5h,
                Slot::WeeklyAll => &mut out.weekly_all,
                Slot::WeeklyOpus => &mut out.weekly_opus,
                Slot::WeeklySonnet => &mut out.weekly_sonnet,
            };
            *target = Some(reading);
        }
    }

    if recognised {
        Ok(out)
    } else {
        Err(OAuthError::Parse("no known usage keys".into()))
    }
}

fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .filter(|x: &f64| x.is_finite())
}

fn reading(pct: f64, resets_at: Option<&Value>, fetched_at: DateTime<Utc>) -> LimitReading {
    LimitReading {
        pct,
        resets_at: resets_at.and_then(parse_timestamp),
        fetched_at,
    }
}

/// `{"utilization": 12.0, "resets_at": ...}`; `null` or no utilisation is `None`.
fn flat_window(v: &Value, fetched_at: DateTime<Utc>) -> Option<LimitReading> {
    let pct = number(v.get("utilization")?)?;
    Some(reading(pct, v.get("resets_at"), fetched_at))
}

/// `extra_usage` amounts (`used_credits`, `monthly_limit`) are in cents.
/// `*_usd` keys, if a later shape adds them, are taken as dollars.
fn extra_usage(v: &Value) -> Option<ExtraUsage> {
    let obj = v.as_object()?;
    // Negative amounts are not meaningful (and the schema forbids them).
    let usd = |usd_key: &str, cents_key: &str| {
        obj.get(usd_key)
            .and_then(number)
            .or_else(|| obj.get(cents_key).and_then(number).map(|c| c / 100.0))
            .map(|x| x.max(0.0))
    };
    let enabled = obj
        .get("is_enabled")
        .or_else(|| obj.get("enabled"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some(ExtraUsage {
        enabled,
        used_usd: usd("used_usd", "used_credits").unwrap_or(0.0),
        limit_usd: usd("limit_usd", "monthly_limit"),
    })
}

enum Slot {
    Session,
    WeeklyAll,
    WeeklyOpus,
    WeeklySonnet,
}

/// One `limits[]` entry. Unknown kinds, and `weekly_scoped` entries whose
/// model cannot be identified, are skipped.
fn limit_entry(entry: &Value, fetched_at: DateTime<Utc>) -> Option<(Slot, LimitReading)> {
    let kind = entry.get("kind")?.as_str()?;
    let slot = match kind {
        "session" | "five_hour" => Slot::Session,
        "weekly_all" | "seven_day" => Slot::WeeklyAll,
        "weekly_scoped" => {
            let scope = ["model", "scope", "model_family", "name"]
                .iter()
                .find_map(|k| entry.get(*k).and_then(Value::as_str))?
                .to_ascii_lowercase();
            if scope.contains("opus") {
                Slot::WeeklyOpus
            } else if scope.contains("sonnet") {
                Slot::WeeklySonnet
            } else {
                return None;
            }
        }
        "seven_day_opus" => Slot::WeeklyOpus,
        "seven_day_sonnet" => Slot::WeeklySonnet,
        _ => return None,
    };
    let pct = entry
        .get("percent")
        .or_else(|| entry.get("utilization"))
        .and_then(number)?;
    Some((slot, reading(pct, entry.get("resets_at"), fetched_at)))
}

/// Parses a `Retry-After` header value (delta seconds or HTTP date).
pub fn parse_retry_after(value: &str, now: DateTime<Utc>) -> Option<Duration> {
    let value = value.trim();
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    // IMF-fixdate ("Sun, 06 Nov 1994 08:49:37 GMT") is valid RFC 2822.
    let at = DateTime::parse_from_rfc2822(value)
        .ok()?
        .with_timezone(&Utc);
    Some((at - now).to_std().unwrap_or(Duration::ZERO))
}

/// Floor for every OAuth delay (PLAN.md: never poll under 60 s).
pub const MIN_INTERVAL: Duration = Duration::from_secs(60);
/// Cap for the exponential part of [`backoff_delay`].
pub const MAX_BACKOFF: Duration = Duration::from_secs(3600);
/// Activity within this long counts as "active" in [`next_poll_interval`].
pub const ACTIVE_WINDOW: Duration = Duration::from_secs(300);

/// Exponential backoff with jitter for 429/5xx, honouring `retry_after` and
/// never below 60 s. `attempt` starts at 1; `jitter` is in `0.0..1.0`.
///
/// The delay is `min(60 s * 2^(attempt-1), 1 h)`, stretched by up to 50 % of
/// jitter, then raised to `retry_after` if the server asked for longer.
pub fn backoff_delay(attempt: u32, retry_after: Option<Duration>, jitter: f64) -> Duration {
    let exp = MIN_INTERVAL
        .saturating_mul(1u32 << attempt.saturating_sub(1).min(16))
        .min(MAX_BACKOFF);
    let jitter = if jitter.is_finite() {
        jitter.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let delay = exp.mul_f64(1.0 + jitter / 2.0);
    delay.max(retry_after.unwrap_or_default()).max(MIN_INTERVAL)
}

/// A jitter value in `0.0..1.0` for [`backoff_delay`], from std's
/// randomly seeded hasher (no RNG dependency needed for this).
pub fn random_jitter() -> f64 {
    use std::hash::{BuildHasher, RandomState};
    let bits = RandomState::new().hash_one(std::time::SystemTime::now());
    (bits >> 11) as f64 / (1u64 << 53) as f64
}

/// Next poll interval: 60 s while JSONL shows activity, 600 s after
/// `idle_after_s` of inactivity, else `oauth_interval_s`; never under 60 s.
///
/// "Activity" means a JSONL event within [`ACTIVE_WINDOW`]. With no known
/// activity at all (e.g. usage only on claude.ai) the normal interval is used.
pub fn next_poll_interval(
    cfg: &PollingConfig,
    last_activity: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Duration {
    let secs = match last_activity.map(|t| (now - t).to_std().unwrap_or(Duration::ZERO)) {
        Some(idle) if idle >= Duration::from_secs(cfg.idle_after_s) => cfg.oauth_idle_interval_s,
        Some(idle) if idle < ACTIVE_WINDOW => cfg.oauth_active_interval_s,
        _ => cfg.oauth_interval_s,
    };
    Duration::from_secs(secs).max(MIN_INTERVAL)
}

/// Total request timeout.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// HTTP client for the usage endpoint.
#[derive(Debug, Clone)]
pub struct OAuthClient {
    url: String,
}

impl Default for OAuthClient {
    fn default() -> Self {
        Self {
            url: USAGE_URL.to_owned(),
        }
    }
}

impl OAuthClient {
    pub fn new() -> Self {
        Self::default()
    }

    /// Points the client at a mock server. Tests only; production code must
    /// use [`OAuthClient::new`].
    #[doc(hidden)]
    pub fn with_url_for_tests(url: impl Into<String>) -> Self {
        Self { url: url.into() }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// One request. Maps 401 to [`OAuthError::TokenExpired`], 429/5xx to
    /// their variants with `Retry-After`, 200 to [`parse_usage_response`].
    ///
    /// Blocking: do not call from inside an async runtime (use
    /// `spawn_blocking`). Redirects are not followed, so the token can only
    /// reach the configured host. The client is built per call; at a 60 s+
    /// cadence that costs nothing and keeps this type plain data.
    pub fn fetch_usage(
        &self,
        token: &AccessToken,
        now: DateTime<Utc>,
    ) -> Result<LimitReadings, OAuthError> {
        let client = reqwest::blocking::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("opencut/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(network_error)?;

        let mut auth = HeaderValue::try_from(format!("Bearer {}", token.expose()))
            .map_err(|_| OAuthError::Network("token is not a valid header value".into()))?;
        auth.set_sensitive(true);

        let resp = client
            .get(&self.url)
            .header(AUTHORIZATION, auth)
            .header(BETA_HEADER.0, BETA_HEADER.1)
            .header(ACCEPT, "application/json")
            .send()
            .map_err(network_error)?;

        let status = resp.status();
        let retry_after = || {
            resp.headers()
                .get(RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| parse_retry_after(v, now))
        };
        match status {
            StatusCode::OK => {
                let body = resp.text().map_err(network_error)?;
                parse_usage_response(&body, now)
            }
            StatusCode::UNAUTHORIZED => Err(OAuthError::TokenExpired),
            StatusCode::TOO_MANY_REQUESTS => Err(OAuthError::RateLimited {
                retry_after: retry_after(),
            }),
            s if s.is_server_error() => Err(OAuthError::Server {
                status: s.as_u16(),
                retry_after: retry_after(),
            }),
            s => Err(OAuthError::Status { status: s.as_u16() }),
        }
    }
}

/// Describes a transport error by kind only: no URL, headers or body.
fn network_error(e: reqwest::Error) -> OAuthError {
    let kind = if e.is_timeout() {
        "timeout"
    } else if e.is_connect() {
        "connection failed"
    } else if e.is_body() || e.is_decode() {
        "failed to read response body"
    } else if e.is_builder() {
        "failed to build HTTP client"
    } else {
        "request failed"
    };
    OAuthError::Network(kind.into())
}

/// Source A with its token. Polling cadence and backoff are the caller's.
#[derive(Debug)]
pub struct OAuthSource {
    pub client: OAuthClient,
    pub token: AccessToken,
    /// From `expiresAt`. Once passed, `poll` returns
    /// [`OAuthError::TokenExpired`] without a request; the token is never
    /// refreshed.
    pub expires_at: Option<DateTime<Utc>>,
}

impl OAuthSource {
    pub fn new(client: OAuthClient, credentials: &OAuthCredentials) -> Self {
        Self {
            client,
            token: credentials.access_token.clone(),
            expires_at: credentials.expires_at,
        }
    }
}

impl Source for OAuthSource {
    type Output = LimitReadings;
    type Error = OAuthError;

    fn name(&self) -> &'static str {
        "oauth_api"
    }

    fn poll(&mut self, now: DateTime<Utc>) -> Result<LimitReadings, OAuthError> {
        if self.expires_at.is_some_and(|t| t <= now) {
            return Err(OAuthError::TokenExpired);
        }
        self.client.fetch_usage(&self.token, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap()
    }

    #[test]
    fn retry_after_forms() {
        assert_eq!(
            parse_retry_after("120", now()),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            parse_retry_after("Sun, 11 Oct 2026 12:02:00 GMT", now()),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            parse_retry_after("Sun, 11 Oct 2026 11:00:00 GMT", now()),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_retry_after("soon", now()), None);
    }

    #[test]
    fn backoff_grows_caps_and_honours_retry_after() {
        assert_eq!(backoff_delay(1, None, 0.0), Duration::from_secs(60));
        assert_eq!(backoff_delay(2, None, 0.0), Duration::from_secs(120));
        assert_eq!(
            backoff_delay(3, None, 0.999),
            Duration::from_secs(240).mul_f64(1.4995)
        );
        assert_eq!(backoff_delay(40, None, 0.0), MAX_BACKOFF);
        assert_eq!(backoff_delay(0, None, f64::NAN), MIN_INTERVAL);
        assert_eq!(
            backoff_delay(1, Some(Duration::from_secs(900)), 0.5),
            Duration::from_secs(900)
        );
        assert_eq!(
            backoff_delay(1, Some(Duration::from_secs(5)), 0.0),
            MIN_INTERVAL
        );
        for _ in 0..100 {
            let j = random_jitter();
            assert!((0.0..1.0).contains(&j));
        }
    }

    #[test]
    fn poll_interval() {
        let cfg = PollingConfig::default();
        let at = |mins: i64| Some(now() - chrono::Duration::minutes(mins));
        let s = |d: Duration| d.as_secs();
        assert_eq!(
            s(next_poll_interval(&cfg, at(1), now())),
            cfg.oauth_active_interval_s
        );
        assert_eq!(
            s(next_poll_interval(&cfg, at(10), now())),
            cfg.oauth_interval_s
        );
        assert_eq!(
            s(next_poll_interval(&cfg, at(20), now())),
            cfg.oauth_idle_interval_s
        );
        assert_eq!(
            s(next_poll_interval(&cfg, None, now())),
            cfg.oauth_interval_s
        );
        let fast = PollingConfig {
            oauth_interval_s: 1,
            oauth_active_interval_s: 1,
            ..cfg
        };
        assert_eq!(next_poll_interval(&fast, at(1), now()), MIN_INTERVAL);
    }

    #[test]
    fn expired_source_does_not_call_network() {
        let mut src = OAuthSource {
            // Unroutable: a request would fail with Network, not TokenExpired.
            client: OAuthClient::with_url_for_tests("http://127.0.0.1:9/"),
            token: AccessToken::new("CANARY".into()),
            expires_at: Some(now()),
        };
        assert!(matches!(src.poll(now()), Err(OAuthError::TokenExpired)));
    }

    #[test]
    fn extra_usage_and_absurd_resets_are_sanitised() {
        let r = parse_usage_response(
            r#"{"five_hour":{"utilization":10,"resets_at":1e15},
                "extra_usage":{"is_enabled":true,"used_credits":-500,"monthly_limit":-1}}"#,
            now(),
        )
        .unwrap();
        assert_eq!(r.session_5h.unwrap().resets_at, None);
        let x = r.extra_usage.unwrap();
        assert_eq!(x.used_usd, 0.0);
        assert_eq!(x.limit_usd, Some(0.0));
    }

    #[test]
    fn rejects_unknown_shape_without_quoting_body() {
        for body in [
            r#"{"something":"CANARY"}"#,
            "CANARY not json",
            r#"["CANARY"]"#,
        ] {
            let e = parse_usage_response(body, now()).unwrap_err();
            assert_eq!(e.code(), "schema_drift");
            assert!(!format!("{e} {e:?}").contains("CANARY"));
        }
    }
}
