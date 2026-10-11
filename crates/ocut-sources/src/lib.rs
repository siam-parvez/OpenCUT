//! OpenCUT data sources. Each module is one source from PLAN.md section 2:
//!
//! | module          | priority | produces                                   |
//! |-----------------|----------|--------------------------------------------|
//! | [`oauth`]       | A        | [`LimitReadings`] from the OAuth usage API |
//! | [`statusline`]  | B        | [`LimitReadings`] from the statusline tee  |
//! | [`jsonl`]       | C        | [`UsageEvent`]s from Claude Code logs      |
//! | [`sessions`]    | -        | active session count                       |
//! | [`credentials`] | -        | the OAuth token used by [`oauth`]          |
//!
//! Sources never read message content and never log tokens.

use chrono::{DateTime, Utc};

pub mod collect;
pub mod credentials;
pub mod fsutil;
pub mod jsonl;
pub mod oauth;
pub mod paths;
pub mod sessions;
pub mod statusline;

pub use ocut_core::UsageEvent;
pub use ocut_core::merge::{LimitReading, LimitReadings};

/// A pollable source. The daemon calls `poll` on its own schedule.
pub trait Source {
    type Output;
    type Error: std::error::Error + Send + Sync + 'static;

    /// Stable name used in logs and `ocut doctor` (`oauth_api`, `statusline`,
    /// `local_logs`, `sessions`).
    fn name(&self) -> &'static str;

    fn poll(&mut self, now: DateTime<Utc>) -> Result<Self::Output, Self::Error>;
}
