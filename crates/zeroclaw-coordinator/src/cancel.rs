// NOT derived from grok-build: this file contains no upstream code, so it
// carries no per-file change notice. It exists only to name the cancellation
// type this crate uses — which is the same `tokio_util::sync::CancellationToken`
// upstream used, and the type the rest of the ZeroClaw workspace already holds.

//! Cooperative cancellation shared by a spawn caller, the coordinator, and the
//! running child.
//!
//! Cancellation is *observed*, never forced: holding a cancelled token means
//! "stop when you can", so a child mid-write finishes its write. The
//! coordinator only ever cancels; what a cancelled child then reports is the
//! runner's decision.
//!
//! ## Why an alias and not a type
//!
//! The token is a seam: a host hands one in on every
//! [`ChildRequest`](crate::ChildRequest), and `zeroclaw-api`,
//! `zeroclaw-gateway` and `zeroclaw-channels` all already hold
//! `tokio_util::sync::CancellationToken`s. A bespoke token here would need a
//! bridging task per child to forward one cancellation into the other — a
//! translation layer between two names for one meaning. So there is one type;
//! this alias only spells it in this crate's vocabulary.

pub use tokio_util::sync::CancellationToken as CancelToken;
