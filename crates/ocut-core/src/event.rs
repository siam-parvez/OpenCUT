//! A single billed request, as recovered from Claude Code's JSONL logs.
//!
//! Lives in `ocut-core` so the pure algorithms (blocks, burn rate, cost) can
//! use it; `ocut-sources` re-exports it and produces it.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::snapshot::TokenCounts;

/// Usage numbers of one assistant message. Never carries message content.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UsageEvent {
    pub timestamp: DateTime<Utc>,
    /// Model id as logged, e.g. `claude-opus-5-5` or `claude-sonnet-4-20250514`.
    pub model: String,
    /// `message.id` in the JSONL line.
    pub message_id: Option<String>,
    /// `requestId` in the JSONL line.
    pub request_id: Option<String>,
    pub session_id: Option<String>,
    /// Project directory name (redact before display unless privacy allows).
    pub project: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// `cache_creation.ephemeral_5m_input_tokens`, or the undivided
    /// `cache_creation_input_tokens` when no split is logged.
    pub cache_write_5m_tokens: u64,
    /// `cache_creation.ephemeral_1h_input_tokens`.
    pub cache_write_1h_tokens: u64,
    pub cache_read_tokens: u64,
    /// Cost reported by Claude Code (`costUSD` / cost-state), preferred over
    /// the pricing table when present.
    pub cost_usd: Option<f64>,
}

impl UsageEvent {
    /// Dedupe key: `message.id + requestId`. `None` if either is missing,
    /// in which case the event is never treated as a duplicate.
    pub fn dedup_key(&self) -> Option<String> {
        match (&self.message_id, &self.request_id) {
            (Some(m), Some(r)) => Some(format!("{m}:{r}")),
            _ => None,
        }
    }

    pub fn cache_write_tokens(&self) -> u64 {
        self.cache_write_5m_tokens + self.cache_write_1h_tokens
    }

    /// Prompt size used for long-context price tiers.
    pub fn prompt_tokens(&self) -> u64 {
        self.input_tokens + self.cache_write_tokens() + self.cache_read_tokens
    }

    pub fn total_tokens(&self) -> u64 {
        self.prompt_tokens() + self.output_tokens
    }

    pub fn token_counts(&self) -> TokenCounts {
        TokenCounts {
            input: self.input_tokens,
            output: self.output_tokens,
            cache_write: self.cache_write_tokens(),
            cache_read: self.cache_read_tokens,
            total: 0,
            messages: 1,
        }
        .with_total()
    }
}

/// Removes duplicate events (same [`UsageEvent::dedup_key`]), keeping the
/// first occurrence and the original order.
pub fn dedup_events(events: Vec<UsageEvent>) -> Vec<UsageEvent> {
    let mut seen = std::collections::HashSet::new();
    events
        .into_iter()
        .filter(|e| e.dedup_key().is_none_or(|k| seen.insert(k)))
        .collect()
}

/// Sums token counts over `events`.
pub fn sum_tokens<'a>(events: impl IntoIterator<Item = &'a UsageEvent>) -> TokenCounts {
    let mut t = TokenCounts::default();
    for e in events {
        t += e.token_counts();
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(m: Option<&str>, r: Option<&str>) -> UsageEvent {
        UsageEvent {
            message_id: m.map(Into::into),
            request_id: r.map(Into::into),
            input_tokens: 1,
            ..Default::default()
        }
    }

    #[test]
    fn dedup_keeps_first_and_unkeyed() {
        let out = dedup_events(vec![
            ev(Some("a"), Some("1")),
            ev(Some("a"), Some("1")),
            ev(Some("a"), Some("2")),
            ev(None, Some("1")),
            ev(None, Some("1")),
        ]);
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn token_totals() {
        let e = UsageEvent {
            input_tokens: 10,
            output_tokens: 20,
            cache_write_5m_tokens: 3,
            cache_write_1h_tokens: 4,
            cache_read_tokens: 100,
            ..Default::default()
        };
        let t = e.token_counts();
        assert_eq!(t.cache_write, 7);
        assert_eq!(t.total, 137);
        assert_eq!(t.messages, 1);
        assert_eq!(sum_tokens([&e, &e]).total, 274);
    }
}
