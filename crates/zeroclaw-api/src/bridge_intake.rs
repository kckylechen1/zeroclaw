//! Durable bridge input ownership inside the canonical session store.
//!
//! A receipt proves persisted intake, not completion of a turn or its effects.
//! Only `pending` inputs are eligible to be claimed for execution. Reading or
//! reopening the store never converts an uncertain outcome back to pending.

use serde::{Deserialize, Serialize};

/// Token-independent source identity, bound permanently to one conversation.
/// The authenticated gateway chooses the key; untrusted frames cannot choose
/// another bridge's namespace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeSource {
    pub key: String,
    pub session_key: String,
    pub agent_alias: String,
}

/// One observed source update. `previous_cursor` links the observed update
/// sequence, so missing numeric update IDs do not create artificial gaps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeInput {
    pub update_id: i64,
    pub previous_cursor: i64,
    pub request_id: String,
    /// The gateway's serialized input. Ephemeral attachment handles do not
    /// imply persistence of the bytes they name.
    pub payload: String,
    /// Initially `pending`, `ignored`, `rejected`, or `control`. Resumed rows
    /// carry their current execution state instead.
    pub state: String,
}

/// Application acceptance with the contiguous persisted source cursor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeReceipt {
    pub cursor: i64,
    pub state: String,
    pub duplicate: bool,
}

/// A consistent source snapshot. Includes retained terminal and uncertain
/// inputs; the caller must only schedule a successfully claimed pending input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeResume {
    pub cursor: i64,
    pub inputs: Vec<BridgeInput>,
}
