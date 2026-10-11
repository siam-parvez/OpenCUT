//! Subscription plans and the presets used for the local 5h estimate.
//!
//! Anthropic does not publish per-window token or dollar limits. The presets
//! below are rough API-equivalent dollar budgets per 5h window (in line with
//! community monitors) and exist only so the local estimate has a scale. They
//! are replaced by [`PlanPreset::calibrate`] once the OAuth API or the
//! statusline has reported a real percentage.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Plan {
    Pro,
    #[serde(rename = "max_5x")]
    Max5x,
    #[serde(rename = "max_20x")]
    Max20x,
    Team,
    Api,
    #[default]
    #[serde(other)]
    Unknown,
}

impl Plan {
    /// The preset for the local estimate, if this plan has one.
    pub fn preset(self) -> Option<PlanPreset> {
        let usd_per_5h = match self {
            Plan::Pro => 18.0,
            Plan::Max5x => 35.0,
            Plan::Max20x => 140.0,
            Plan::Team | Plan::Api | Plan::Unknown => return None,
        };
        Some(PlanPreset {
            plan: self,
            usd_per_5h,
            calibrated: false,
        })
    }
}

/// Scale for the local estimate: the API-equivalent spend that counts as 100%
/// of one 5h window.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PlanPreset {
    pub plan: Plan,
    pub usd_per_5h: f64,
    /// `true` once derived from a real reading rather than the builtin guess.
    pub calibrated: bool,
}

impl PlanPreset {
    /// Estimated window utilisation for `cost_usd`, clamped to `0..=100`.
    pub fn pct_for_cost(&self, cost_usd: f64) -> f64 {
        if self.usd_per_5h.is_nan() || self.usd_per_5h <= 0.0 || !cost_usd.is_finite() {
            return 0.0;
        }
        (cost_usd / self.usd_per_5h * 100.0).clamp(0.0, 100.0)
    }

    /// Re-derives the budget from a real reading (`observed_pct` from A or B)
    /// and the locally computed cost of the same window. Readings under 5% are
    /// too noisy and leave the preset unchanged.
    pub fn calibrate(self, observed_pct: f64, observed_cost_usd: f64) -> Self {
        if !(5.0..=100.0).contains(&observed_pct)
            || observed_cost_usd.is_nan()
            || observed_cost_usd <= 0.0
        {
            return self;
        }
        Self {
            usd_per_5h: observed_cost_usd / (observed_pct / 100.0),
            calibrated: true,
            ..self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_serde_names() {
        assert_eq!(serde_json::to_string(&Plan::Max5x).unwrap(), "\"max_5x\"");
        assert_eq!(serde_json::to_string(&Plan::Max20x).unwrap(), "\"max_20x\"");
        let p: Plan = serde_json::from_str("\"enterprise_new\"").unwrap();
        assert_eq!(p, Plan::Unknown);
    }

    #[test]
    fn presets_exist_for_consumer_plans() {
        assert!(Plan::Pro.preset().is_some());
        assert!(Plan::Max5x.preset().is_some());
        assert!(Plan::Max20x.preset().is_some());
        assert!(Plan::Api.preset().is_none());
        let pro = Plan::Pro.preset().unwrap();
        assert!(pro.usd_per_5h < Plan::Max5x.preset().unwrap().usd_per_5h);
    }

    #[test]
    fn calibrate_uses_reading() {
        let p = Plan::Pro.preset().unwrap().calibrate(50.0, 10.0);
        assert!(p.calibrated);
        assert!((p.usd_per_5h - 20.0).abs() < 1e-9);
        assert!((p.pct_for_cost(5.0) - 25.0).abs() < 1e-9);
        // Too small to trust.
        let q = Plan::Pro.preset().unwrap().calibrate(1.0, 0.1);
        assert!(!q.calibrated);
    }
}
