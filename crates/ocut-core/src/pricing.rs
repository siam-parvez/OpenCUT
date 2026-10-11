//! Per-model token prices, loaded from `pricing/models.json`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::event::UsageEvent;

/// The bundled pricing table.
pub const BUILTIN_PRICING_JSON: &str = include_str!("../../../pricing/models.json");

/// USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rates {
    pub input: f64,
    pub output: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
    pub cache_read: f64,
}

/// Rates that apply when the prompt exceeds `threshold_tokens`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LongContextRates {
    pub threshold_tokens: u64,
    #[serde(flatten)]
    pub rates: Rates,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelPrice {
    /// `opus`, `sonnet`, `haiku`, `fable`.
    pub family: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(flatten)]
    pub rates: Rates,
    #[serde(default)]
    pub long_context: Option<LongContextRates>,
    /// Free-form note, used to flag uncertain prices.
    #[serde(default)]
    pub comment: Option<String>,
}

impl ModelPrice {
    /// Rates for a prompt of `prompt_tokens`.
    pub fn rates_for(&self, prompt_tokens: u64) -> &Rates {
        match &self.long_context {
            Some(lc) if prompt_tokens > lc.threshold_tokens => &lc.rates,
            _ => &self.rates,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PricingTable {
    /// e.g. `builtin@2026-10-06`; copied into `cost.pricing` in the snapshot.
    pub version: String,
    #[serde(default)]
    pub comment: Option<String>,
    pub models: BTreeMap<String, ModelPrice>,
}

#[derive(Debug, thiserror::Error)]
#[error("invalid pricing table: {0}")]
pub struct PricingError(#[from] serde_json::Error);

impl PricingTable {
    pub fn from_json(json: &str) -> Result<Self, PricingError> {
        Ok(serde_json::from_str(json)?)
    }

    /// The table bundled at build time.
    pub fn builtin() -> Self {
        Self::from_json(BUILTIN_PRICING_JSON).expect("bundled pricing/models.json is valid")
    }

    /// Resolves a logged model id: exact id, alias, then the id with provider
    /// prefixes (`anthropic.`), a `[1m]` suffix, a trailing `-YYYYMMDD` date
    /// and Bedrock `-vN:N` suffixes removed.
    pub fn lookup(&self, model: &str) -> Option<(&str, &ModelPrice)> {
        let find = |id: &str| {
            self.models
                .get_key_value(id)
                .or_else(|| {
                    self.models
                        .iter()
                        .find(|(_, p)| p.aliases.iter().any(|a| a == id))
                })
                .map(|(k, v)| (k.as_str(), v))
        };
        find(model).or_else(|| find(&normalize_model_id(model)))
    }

    /// Model family (`opus`, `sonnet`, ...), falling back to a guess from the id.
    pub fn family(&self, model: &str) -> String {
        if let Some((_, p)) = self.lookup(model) {
            return p.family.clone();
        }
        ["fable", "mythos", "opus", "sonnet", "haiku"]
            .into_iter()
            .find(|f| model.contains(f))
            .map(|f| if f == "mythos" { "fable" } else { f })
            .unwrap_or("unknown")
            .to_owned()
    }

    /// Cost computed from token counts; `None` if the model is unknown.
    pub fn compute_cost(&self, e: &UsageEvent) -> Option<f64> {
        let (_, price) = self.lookup(&e.model)?;
        let r = price.rates_for(e.prompt_tokens());
        let usd = (e.input_tokens as f64 * r.input
            + e.output_tokens as f64 * r.output
            + e.cache_write_5m_tokens as f64 * r.cache_write_5m
            + e.cache_write_1h_tokens as f64 * r.cache_write_1h
            + e.cache_read_tokens as f64 * r.cache_read)
            / 1_000_000.0;
        // A user pricing file with negative rates must not yield a negative
        // cost (the snapshot schema requires costs >= 0).
        Some(usd.max(0.0))
    }

    /// Cost of an event: the logged `cost_usd` when present (cost-state
    /// preference), otherwise computed. Unknown models cost 0.
    pub fn cost(&self, e: &UsageEvent) -> f64 {
        e.cost_usd
            .filter(|c| c.is_finite() && *c >= 0.0)
            .or_else(|| self.compute_cost(e))
            .unwrap_or(0.0)
    }
}

/// Strips provider prefixes and version/date/context suffixes from a model id.
pub fn normalize_model_id(model: &str) -> String {
    let mut id = model.trim().to_ascii_lowercase();
    for prefix in ["anthropic/", "anthropic.", "us.anthropic.", "eu.anthropic."] {
        if let Some(rest) = id.strip_prefix(prefix) {
            id = rest.to_owned();
        }
    }
    if let Some(i) = id.find('[') {
        id.truncate(i);
    }
    if let Some(i) = id.find('@') {
        id.truncate(i);
    }
    // Bedrock: `...-v1:0`
    if let Some(i) = id.rfind("-v")
        && id[i + 2..].chars().all(|c| c.is_ascii_digit() || c == ':')
        && id.len() > i + 2
    {
        id.truncate(i);
    }
    // Trailing `-YYYYMMDD`.
    if let Some(i) = id.rfind('-') {
        let tail = &id[i + 1..];
        if tail.len() == 8 && tail.chars().all(|c| c.is_ascii_digit()) {
            id.truncate(i);
        }
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(model: &str) -> UsageEvent {
        UsageEvent {
            model: model.into(),
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_write_5m_tokens: 1_000_000,
            cache_write_1h_tokens: 1_000_000,
            cache_read_tokens: 1_000_000,
            ..Default::default()
        }
    }

    #[test]
    fn builtin_loads_and_has_required_models() {
        let t = PricingTable::builtin();
        for id in [
            "claude-opus-5-5",
            "claude-sonnet-5-5",
            "claude-haiku-5-5",
            "claude-fable-5-1",
            "claude-opus-4-8",
            "claude-sonnet-4-5",
            "claude-haiku-4-5",
        ] {
            assert!(t.lookup(id).is_some(), "{id}");
        }
    }

    #[test]
    fn cache_multipliers_are_consistent() {
        for (id, p) in PricingTable::builtin().models {
            let r = p.rates;
            assert!((r.cache_write_5m - r.input * 1.25).abs() < 1e-9, "{id} 5m");
            assert!((r.cache_write_1h - r.input * 2.0).abs() < 1e-9, "{id} 1h");
            assert!(r.cache_read <= r.input * 0.1 + 1e-9, "{id} read");
        }
    }

    #[test]
    fn per_kind_cost() {
        let t = PricingTable::builtin();
        let one = |f: fn(&mut UsageEvent)| {
            let mut e = UsageEvent {
                model: "claude-opus-5-5".into(),
                ..Default::default()
            };
            f(&mut e);
            t.cost(&e)
        };
        assert!((one(|e| e.input_tokens = 1_000_000) - 4.0).abs() < 1e-9);
        assert!((one(|e| e.output_tokens = 1_000_000) - 20.0).abs() < 1e-9);
        assert!((one(|e| e.cache_write_5m_tokens = 1_000_000) - 5.0).abs() < 1e-9);
        assert!((one(|e| e.cache_write_1h_tokens = 1_000_000) - 8.0).abs() < 1e-9);
        assert!((one(|e| e.cache_read_tokens = 1_000_000) - 0.2).abs() < 1e-9);
    }

    #[test]
    fn logged_cost_is_preferred() {
        let t = PricingTable::builtin();
        let mut e = ev("claude-opus-5-5");
        e.cost_usd = Some(1.23);
        assert_eq!(t.cost(&e), 1.23);
        e.cost_usd = Some(f64::NAN);
        assert!((t.cost(&e) - (4.0 + 20.0 + 5.0 + 8.0 + 0.2)).abs() < 1e-9);
    }

    #[test]
    fn long_context_tier() {
        let t = PricingTable::builtin();
        let small = UsageEvent {
            model: "claude-haiku-5-5".into(),
            input_tokens: 100_000,
            ..Default::default()
        };
        let big = UsageEvent {
            input_tokens: 100_001,
            ..small.clone()
        };
        assert!((t.cost(&small) - 0.01).abs() < 1e-9);
        assert!((t.cost(&big) - 100_001.0 * 0.5 / 1e6).abs() < 1e-9);
    }

    #[test]
    fn id_normalisation() {
        assert_eq!(
            normalize_model_id("claude-sonnet-4-20250514"),
            "claude-sonnet-4"
        );
        assert_eq!(
            normalize_model_id("claude-opus-4-1-20250805"),
            "claude-opus-4-1"
        );
        assert_eq!(normalize_model_id("claude-opus-5-5[1m]"), "claude-opus-5-5");
        assert_eq!(
            normalize_model_id("anthropic.claude-haiku-4-5-20251001-v1:0"),
            "claude-haiku-4-5"
        );
        assert_eq!(
            normalize_model_id("claude-opus-4-5@20251101"),
            "claude-opus-4-5"
        );
        let t = PricingTable::builtin();
        assert_eq!(
            t.lookup("claude-sonnet-4-20250514").unwrap().0,
            "claude-sonnet-4-0"
        );
        assert_eq!(
            t.lookup("claude-opus-4-20250514").unwrap().0,
            "claude-opus-4-0"
        );
        assert!(t.lookup("<synthetic>").is_none());
        assert_eq!(t.family("claude-opus-9-9"), "opus");
        assert_eq!(t.cost(&ev("<synthetic>")), 0.0);
    }
}
