//! Serde types for the `opencut/usage@1` snapshot (see `schema/usage.v1.json`).
//!
//! The schema is additive-only: readers must ignore unknown fields, so none of
//! these types use `deny_unknown_fields`, and every optional field has a default.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::plan::Plan;

/// Value of the top-level `schema` field.
pub const SCHEMA_ID: &str = "opencut/usage@1";

/// The snapshot written to `$XDG_RUNTIME_DIR/opencut/usage.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub schema: String,
    pub generated_at: DateTime<Utc>,
    pub status: Status,
    #[serde(default)]
    pub account: Account,
    #[serde(default)]
    pub windows: Windows,
    #[serde(default)]
    pub tokens: TokenPeriods,
    #[serde(default)]
    pub by_model: Vec<ModelUsage>,
    #[serde(default)]
    pub cost: Cost,
    #[serde(default)]
    pub burn_rate: BurnRate,
    #[serde(default)]
    pub active_sessions: u32,
    #[serde(default)]
    pub sources: SourcesHealth,
    #[serde(default)]
    pub errors: Vec<ErrorEntry>,
}

impl Snapshot {
    /// An empty snapshot with `status: ok` and the given generation time.
    pub fn empty(generated_at: DateTime<Utc>) -> Self {
        Self {
            schema: SCHEMA_ID.to_owned(),
            generated_at,
            status: Status::Ok,
            account: Account::default(),
            windows: Windows::default(),
            tokens: TokenPeriods::default(),
            by_model: Vec::new(),
            cost: Cost::default(),
            burn_rate: BurnRate::default(),
            active_sessions: 0,
            sources: SourcesHealth::default(),
            errors: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    #[default]
    Ok,
    Stale,
    Degraded,
    Offline,
    TokenExpired,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Account {
    #[serde(default)]
    pub plan: Plan,
}

/// Where a window's `pct` / `resets_at` came from (merge priority A, B, C).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowSource {
    OauthApi,
    Statusline,
    LocalEstimate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    #[default]
    Low,
    Med,
    High,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Projection {
    /// Projected percentage at `resets_at`, clamped to `0..=100`.
    pub pct_at_reset: f64,
    /// When the projection crosses 100%, if before the reset.
    pub hits_limit_at: Option<DateTime<Utc>>,
    pub confidence: Confidence,
}

/// One rate-limit window (5h session, weekly, scoped weekly).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    /// Utilisation in percent, `0..=100`.
    pub pct: f64,
    /// `None` means no window is currently open.
    #[serde(default)]
    pub resets_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds_to_reset: Option<i64>,
    pub source: WindowSource,
    #[serde(default)]
    pub estimated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub stale: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection: Option<Projection>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ExtraUsage {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub used_usd: f64,
    #[serde(default)]
    pub limit_usd: Option<f64>,
}

/// All windows. A `None` window serialises as `null` ("n/a" in widgets).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Windows {
    #[serde(default)]
    pub session_5h: Option<Window>,
    #[serde(default)]
    pub weekly_all: Option<Window>,
    #[serde(default)]
    pub weekly_opus: Option<Window>,
    #[serde(default)]
    pub weekly_sonnet: Option<Window>,
    #[serde(default)]
    pub extra_usage: Option<ExtraUsage>,
}

/// Token counters. `cache_write` covers both the 5m and 1h TTLs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TokenCounts {
    pub input: u64,
    pub output: u64,
    pub cache_write: u64,
    pub cache_read: u64,
    pub total: u64,
    pub messages: u64,
}

impl TokenCounts {
    /// Recomputes `total` from the four token kinds.
    pub fn with_total(mut self) -> Self {
        self.total = self.input + self.output + self.cache_write + self.cache_read;
        self
    }
}

impl std::ops::AddAssign for TokenCounts {
    fn add_assign(&mut self, o: Self) {
        self.input += o.input;
        self.output += o.output;
        self.cache_write += o.cache_write;
        self.cache_read += o.cache_read;
        self.total += o.total;
        self.messages += o.messages;
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TokenPeriods {
    pub session_5h: TokenCounts,
    pub today: TokenCounts,
    pub week: TokenCounts,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelUsage {
    pub model: String,
    pub family: String,
    pub total: u64,
    pub est_cost_usd: f64,
    pub share_pct: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cost {
    pub session_5h_usd: f64,
    pub today_usd: f64,
    pub week_usd: f64,
    /// Pricing table version, e.g. `builtin@2026-10-06`.
    pub pricing: String,
    pub note: String,
}

impl Default for Cost {
    fn default() -> Self {
        Self {
            session_5h_usd: 0.0,
            today_usd: 0.0,
            week_usd: 0.0,
            pricing: String::new(),
            note: "API-equivalent estimate; plan is flat-rate".to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BurnRate {
    pub tokens_per_min: f64,
    pub usd_per_hour: f64,
    pub pct_per_hour: f64,
    pub window_min: u32,
}

impl Default for BurnRate {
    fn default() -> Self {
        Self {
            tokens_per_min: 0.0,
            usd_per_hour: 0.0,
            pct_per_hour: 0.0,
            window_min: 15,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OAuthHealth {
    pub enabled: bool,
    pub ok: bool,
    pub last_success: Option<DateTime<Utc>>,
    pub next_poll_at: Option<DateTime<Utc>>,
    pub backoff_s: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StatuslineHealth {
    pub ok: bool,
    pub last_push: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalLogsHealth {
    pub ok: bool,
    pub files: u64,
    pub parse_errors: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SourcesHealth {
    pub oauth_api: OAuthHealth,
    pub statusline: StatuslineHealth,
    pub local_logs: LocalLogsHealth,
}

/// A user-facing error. `message` must never contain tokens, emails or prompts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorEntry {
    pub code: String,
    pub message: String,
    pub at: DateTime<Utc>,
}
