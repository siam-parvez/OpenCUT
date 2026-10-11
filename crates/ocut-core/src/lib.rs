//! OpenCUT core: snapshot types, the source merge rule, 5h block estimation,
//! burn rate and projection, plan presets, pricing and config types.
//!
//! Pure: no file, network or environment access.

pub mod assemble;
pub mod blocks;
pub mod burn;
pub mod config;
pub mod event;
pub mod merge;
pub mod plan;
pub mod pricing;
pub mod snapshot;
pub mod time;

pub use assemble::{AssembleInputs, assemble};
pub use blocks::{Block, active_block, estimate_blocks, local_session_estimate};
pub use burn::{PctSample, RateEstimate, burn_rate, ewma_pct_rate, project};
pub use config::{Config, OAuthConsent};
pub use event::{UsageEvent, dedup_events};
pub use merge::{LimitReading, LimitReadings, MergeInputs, MergePolicy, merge_windows};
pub use plan::{Plan, PlanPreset};
pub use pricing::PricingTable;
pub use snapshot::{SCHEMA_ID, Snapshot, Status, Window, WindowSource};
