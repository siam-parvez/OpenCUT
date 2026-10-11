//! `~/.config/opencut/config.toml`. Parsing only; callers do the file IO.
//!
//! Every field has a default, so an empty file (or no file) is valid.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::plan::Plan;

/// Hard floor for the OAuth poll interval.
pub const MIN_OAUTH_POLL_S: u64 = 60;

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub sources: SourcesConfig,
    pub polling: PollingConfig,
    pub notify: NotifyConfig,
    pub pricing: PricingConfig,
    pub display: DisplayConfig,
    pub privacy: PrivacyConfig,
}

#[derive(Debug, thiserror::Error)]
#[error("invalid config: {0}")]
pub struct ConfigError(#[from] toml::de::Error);

impl Config {
    /// Parses TOML and clamps out-of-range values (see [`Config::normalized`]).
    pub fn from_toml_str(s: &str) -> Result<Self, ConfigError> {
        Ok(toml::from_str::<Self>(s)?.normalized())
    }

    pub fn to_toml_string(&self) -> String {
        toml::to_string_pretty(self).expect("config serialises")
    }

    /// Enforces invariants: OAuth polling never under 60 s, thresholds sorted,
    /// deduplicated and within 1..=100.
    pub fn normalized(mut self) -> Self {
        let p = &mut self.polling;
        p.oauth_interval_s = p.oauth_interval_s.max(MIN_OAUTH_POLL_S);
        p.oauth_active_interval_s = p.oauth_active_interval_s.max(MIN_OAUTH_POLL_S);
        p.oauth_idle_interval_s = p.oauth_idle_interval_s.max(MIN_OAUTH_POLL_S);
        p.snapshot_interval_s = p.snapshot_interval_s.max(1);
        let t = &mut self.notify.thresholds;
        t.retain(|x| (1..=100).contains(x));
        t.sort_unstable();
        t.dedup();
        self
    }

    /// `$XDG_CONFIG_HOME/opencut/config.toml`, else `$HOME/.config/...`.
    pub fn default_path(
        xdg_config_home: Option<PathBuf>,
        home: Option<PathBuf>,
    ) -> Option<PathBuf> {
        xdg_config_home
            .filter(|p| p.is_absolute())
            .or_else(|| home.map(|h| h.join(".config")))
            .map(|d| d.join("opencut").join("config.toml"))
    }
}

/// Consent state for the OAuth usage endpoint (source A).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OAuthConsent {
    /// Not decided yet: `ocut` asks once on first run; the daemon treats it as off.
    #[default]
    Ask,
    On,
    Off,
}

impl OAuthConsent {
    pub fn is_enabled(self) -> bool {
        self == OAuthConsent::On
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SourcesConfig {
    pub oauth_api: OAuthConsent,
    pub statusline: bool,
    pub local_logs: bool,
    /// Extra Claude config dirs to scan besides the defaults.
    pub claude_dirs: Vec<PathBuf>,
    /// Plan preset for the local 5h estimate.
    pub local_plan: Plan,
}

impl Default for SourcesConfig {
    fn default() -> Self {
        Self {
            oauth_api: OAuthConsent::Ask,
            statusline: true,
            local_logs: true,
            claude_dirs: Vec::new(),
            local_plan: Plan::Pro,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PollingConfig {
    pub oauth_interval_s: u64,
    /// Used while JSONL shows activity.
    pub oauth_active_interval_s: u64,
    /// Used after `idle_after_s` without activity.
    pub oauth_idle_interval_s: u64,
    pub idle_after_s: u64,
    pub snapshot_interval_s: u64,
}

impl Default for PollingConfig {
    fn default() -> Self {
        Self {
            oauth_interval_s: 120,
            oauth_active_interval_s: 60,
            oauth_idle_interval_s: 600,
            idle_after_s: 900,
            snapshot_interval_s: 30,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyConfig {
    pub enabled: bool,
    /// Percent thresholds for the 5h and weekly windows.
    pub thresholds: Vec<u8>,
    /// Notify when the 5h window resets.
    pub reset_notice: bool,
    /// Points the pct must fall below a threshold before it can fire again.
    pub hysteresis: u8,
    /// Notify when the OAuth token has expired.
    pub token_expired: bool,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            thresholds: vec![75, 90, 100],
            reset_notice: true,
            hysteresis: 5,
            token_expired: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PricingConfig {
    /// Replaces the bundled `pricing/models.json` when set.
    pub file: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DisplayConfig {
    /// Default `ocut --format` template.
    pub format: String,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            format: "{5h.pct:.0}% ↻{5h.reset_in}".to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PrivacyConfig {
    pub redact_projects: bool,
}

impl Default for PrivacyConfig {
    fn default() -> Self {
        Self {
            redact_projects: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_default() {
        let c = Config::from_toml_str("").unwrap();
        assert_eq!(c, Config::default());
        assert_eq!(c.sources.oauth_api, OAuthConsent::Ask);
        assert!(!c.sources.oauth_api.is_enabled());
        assert_eq!(c.notify.thresholds, vec![75, 90, 100]);
        assert!(c.notify.reset_notice);
    }

    #[test]
    fn parses_and_normalises() {
        let c = Config::from_toml_str(
            r#"
            [sources]
            oauth_api = "on"
            local_plan = "max_5x"
            [polling]
            oauth_interval_s = 5
            [notify]
            thresholds = [100, 50, 0, 50, 120]
            reset_notice = false
            [future_section]
            x = 1
            "#,
        )
        .unwrap();
        assert!(c.sources.oauth_api.is_enabled());
        assert_eq!(c.sources.local_plan, Plan::Max5x);
        assert_eq!(c.polling.oauth_interval_s, MIN_OAUTH_POLL_S);
        assert_eq!(c.notify.thresholds, vec![50, 100]);
        assert!(!c.notify.reset_notice);
    }

    #[test]
    fn rejects_bad_consent() {
        assert!(Config::from_toml_str("[sources]\noauth_api = \"maybe\"").is_err());
    }

    #[test]
    fn round_trip() {
        let c = Config::default();
        assert_eq!(Config::from_toml_str(&c.to_toml_string()).unwrap(), c);
    }

    #[test]
    fn default_path() {
        let p = Config::default_path(None, Some("/home/u".into())).unwrap();
        assert_eq!(p, PathBuf::from("/home/u/.config/opencut/config.toml"));
        let p = Config::default_path(Some("/x".into()), Some("/home/u".into())).unwrap();
        assert_eq!(p, PathBuf::from("/x/opencut/config.toml"));
        let p = Config::default_path(Some("rel".into()), Some("/home/u".into())).unwrap();
        assert_eq!(p, PathBuf::from("/home/u/.config/opencut/config.toml"));
    }
}
