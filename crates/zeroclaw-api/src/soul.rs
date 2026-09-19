//! Shared AgentSoul domain types.

use serde::{Deserialize, Serialize};

/// Stable owner and kind of an external evidence record.
///
/// The record's own id stays on the referencing domain type. Together,
/// `(owner, kind, id)` is the evidence identity, so equal opaque ids from two
/// source systems never collide.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EvidenceSource {
    /// Stable identifier of the source owner or source-system instance.
    pub owner: String,
    /// Stable record kind within that owner (for example, `benchmark_run`).
    pub kind: String,
}
