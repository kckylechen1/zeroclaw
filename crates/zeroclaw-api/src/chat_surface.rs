//! Presentation register requested by a chat client, never an authority grant.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatSurface {
    Web,
    Telegram,
    Cli,
}
