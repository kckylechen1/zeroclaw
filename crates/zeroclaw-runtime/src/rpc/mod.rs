//! Transport-agnostic JSON-RPC 2.0 dispatch for the runtime.

pub mod approval_channel;
pub mod attachments;
pub mod config_handlers;
pub mod context;
pub mod cron_handlers;
pub mod dispatch;
pub mod fs;
pub mod git;
pub mod local;
pub mod locales;
pub mod memory_handlers;
pub mod session;
pub mod session_handlers;
pub mod transport;
pub mod tui_identity;
pub mod turn;
pub mod types;
pub mod wss;
