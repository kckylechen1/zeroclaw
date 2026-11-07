//! Tachi bridge — the ZeroClaw CLIENT half of delegation (ADR-017).
//!
//! - [`staff`] — the production client of Tachi's `tachi_staff` MCP tool
//!   (start / status / result / cancel over the MCP HTTP transport). This is
//!   the surface a real Tachi daemon serves; see ADR-017 "ZeroClaw ↔
//!   tachi_staff mapping".
//! - The unserved V2b `TaskIntent` port was retired with `procedure_v1` and
//!   `supervisor_v1` (#381 PR D).

pub mod staff;

#[cfg(test)]
mod staff_tests;

pub use staff::{
    CancelOutcome, CancelReceipt, RunResult, RunState, RunStatus, StaffReceipt, StaffRefs,
    StaffingReason, TachiStaffClient, TachiStaffError, TachiStaffSettings,
};
