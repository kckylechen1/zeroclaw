//! A Telegram bridge for the ZeroClaw gateway.
//!
//! The bridge is a gateway client like `zeroclaw chat`: it attaches to one
//! session over `/ws/chat` and relays between that session and the owner's
//! private chat with a Telegram bot. The gateway owns the conversation, so
//! the owner's Telegram chat and `zeroclaw chat -s <session>` on a laptop
//! see the same turns.

mod bridge;
pub mod render;
pub mod telegram;

pub use bridge::{BridgeConfig, run};
