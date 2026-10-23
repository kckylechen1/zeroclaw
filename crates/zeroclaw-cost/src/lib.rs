//! Cost accounting: pricing, budget checks and the existing JSONL ledger.
//! Configuration lives in `zeroclaw-config`; shared records live in `zeroclaw-api`.

mod pricing;
pub mod tracker;

pub use pricing::calculate_token_usage;
pub use tracker::CostTracker;
pub use zeroclaw_api::cost as types;
pub use zeroclaw_api::cost::*;
