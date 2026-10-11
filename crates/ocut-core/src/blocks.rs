//! 5-hour block estimation (ccusage "blocks" semantics).
//!
//! A block starts at its first event's timestamp floored to the UTC hour and
//! lasts 5 h. An event at or after the block end, or 5 h or more after the
//! previous event, starts a new block. Flooring in UTC keeps the result
//! independent of the local timezone and DST.

use chrono::{DateTime, Duration, DurationRound, Utc};

use crate::event::UsageEvent;
use crate::merge::LimitReading;
use crate::plan::PlanPreset;
use crate::pricing::PricingTable;
use crate::snapshot::TokenCounts;

/// Length of a session window.
pub const BLOCK_DURATION: Duration = Duration::hours(5);

#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub start: DateTime<Utc>,
    /// `start + 5h`; the estimated `resets_at`.
    pub end: DateTime<Utc>,
    pub first_event: DateTime<Utc>,
    pub last_event: DateTime<Utc>,
    pub tokens: TokenCounts,
    pub cost_usd: f64,
    /// Distinct model ids seen, in first-seen order.
    pub models: Vec<String>,
}

impl Block {
    /// The window is open at `now`: started, not ended, and not idle for 5 h.
    pub fn is_active(&self, now: DateTime<Utc>) -> bool {
        now >= self.start && now < self.end && now - self.last_event < BLOCK_DURATION
    }

    fn new(e: &UsageEvent, pricing: &PricingTable) -> Self {
        let start = floor_hour(e.timestamp);
        let mut b = Self {
            start,
            end: start + BLOCK_DURATION,
            first_event: e.timestamp,
            last_event: e.timestamp,
            tokens: TokenCounts::default(),
            cost_usd: 0.0,
            models: Vec::new(),
        };
        b.add(e, pricing);
        b
    }

    fn add(&mut self, e: &UsageEvent, pricing: &PricingTable) {
        self.last_event = self.last_event.max(e.timestamp);
        self.tokens += e.token_counts();
        self.cost_usd += pricing.cost(e);
        if !self.models.iter().any(|m| m == &e.model) {
            self.models.push(e.model.clone());
        }
    }
}

/// Floors to the start of the UTC hour.
pub fn floor_hour(t: DateTime<Utc>) -> DateTime<Utc> {
    t.duration_trunc(Duration::hours(1)).unwrap_or(t)
}

/// Groups events into 5h blocks, oldest first. Input order does not matter;
/// callers should dedupe first (see [`crate::event::dedup_events`]).
pub fn estimate_blocks(events: &[UsageEvent], pricing: &PricingTable) -> Vec<Block> {
    let mut sorted: Vec<&UsageEvent> = events.iter().collect();
    sorted.sort_by_key(|e| e.timestamp);
    let mut blocks: Vec<Block> = Vec::new();
    for e in sorted {
        match blocks.last_mut() {
            Some(b) if e.timestamp < b.end && e.timestamp - b.last_event < BLOCK_DURATION => {
                b.add(e, pricing)
            }
            _ => blocks.push(Block::new(e, pricing)),
        }
    }
    blocks
}

/// The block open at `now`, if any.
pub fn active_block(blocks: &[Block], now: DateTime<Utc>) -> Option<&Block> {
    blocks.last().filter(|b| b.is_active(now))
}

/// Local (priority C) estimate of the 5h window. With no open block the
/// window reads 0% and `resets_at: None`.
pub fn local_session_estimate(
    blocks: &[Block],
    preset: &PlanPreset,
    now: DateTime<Utc>,
) -> LimitReading {
    match active_block(blocks, now) {
        Some(b) => LimitReading {
            pct: preset.pct_for_cost(b.cost_usd),
            resets_at: Some(b.end),
            fetched_at: now,
        },
        None => LimitReading {
            pct: 0.0,
            resets_at: None,
            fetched_at: now,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 11, h, m, 0).unwrap()
    }

    fn ev(t: DateTime<Utc>) -> UsageEvent {
        UsageEvent {
            timestamp: t,
            model: "claude-opus-5-5".into(),
            input_tokens: 1_000,
            output_tokens: 1_000,
            ..Default::default()
        }
    }

    #[test]
    fn floors_and_splits() {
        let p = PricingTable::builtin();
        let blocks = estimate_blocks(
            &[ev(at(9, 47)), ev(at(13, 59)), ev(at(14, 0)), ev(at(14, 30))],
            &p,
        );
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].start, at(9, 0));
        assert_eq!(blocks[0].end, at(14, 0));
        assert_eq!(blocks[0].tokens.messages, 2);
        assert_eq!(blocks[1].start, at(14, 0));
        assert_eq!(blocks[1].tokens.messages, 2);
    }

    #[test]
    fn unsorted_input() {
        let p = PricingTable::builtin();
        let a = estimate_blocks(&[ev(at(14, 30)), ev(at(9, 47)), ev(at(10, 0))], &p);
        let b = estimate_blocks(&[ev(at(9, 47)), ev(at(10, 0)), ev(at(14, 30))], &p);
        assert_eq!(a, b);
    }

    #[test]
    fn active_and_estimate() {
        let p = PricingTable::builtin();
        let blocks = estimate_blocks(&[ev(at(9, 47)), ev(at(10, 5))], &p);
        let preset = crate::plan::Plan::Pro.preset().unwrap();
        let r = local_session_estimate(&blocks, &preset, at(11, 0));
        assert_eq!(r.resets_at, Some(at(14, 0)));
        assert!(r.pct > 0.0 && r.pct <= 100.0);
        let r = local_session_estimate(&blocks, &preset, at(14, 0));
        assert_eq!(r.resets_at, None);
        assert_eq!(r.pct, 0.0);
    }

    #[test]
    fn empty() {
        assert!(estimate_blocks(&[], &PricingTable::builtin()).is_empty());
    }
}
