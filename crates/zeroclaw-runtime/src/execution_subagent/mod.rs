//! Ephemeral ExecutionSubAgent vertical (the frozen SubAgent and bridge
//! contracts at rev 3, the three-path addendum, and the tachi
//! attached-session receipt spine).
//!
//! ```text
//! Parent
//!   → ExecutionRouteV1          typed three-path selection      (addendum ticket 2)
//!   → ExecutionSubagentTool     bounded parent-side run         (the frozen SubAgent pattern)
//!       → SessionController     typed lifecycle seam over ACPX  (controller.rs)
//!           start / watch / prompt / interrupt / stop / collect / reattach
//!       → SessionFactSink       receipts-only fact reporting    (facts.rs)
//!           attach / advertise / ingest / interventions / reconnect / state
//!       → tachi spine           canonical truth; host-owned lifecycle
//!   → ExecutionSessionReportV1  the ONLY child→parent channel
//! ```
//!
//! Authority boundaries encoded here:
//!
//! - **The subagent is a typed supervisor, not a shell.** Its only
//!   operations are start / watch / prompt-correct / interrupt / stop /
//!   collect (the capability boundary). The harness behind the session
//!   operates the repository; the subagent context holds no filesystem,
//!   process, git, credential, or CLI-flag capability (the context
//!   inventory type is the structural proof; the negative-capability
//!   suite pins it).
//! - **The host owns the session lifecycle.** The controller port carries
//!   lifecycle vocabulary; the transport implementation is constructed
//!   with the host's own workspace/transport binding, which no port
//!   request can widen. Tachi never owns an ACP process (the spine is receipts-only:
//!   receipts-only spine).
//! - **Facts flow, they are not mutated.** The sink port is receipts-only
//!   (record/read/reconnect-bind); no operation can signal a process or
//!   write a second durable store (source-scan-tested; no DDL, no
//!   connection opens).
//! - **Fail closed, no silent fallback.** Controller or spine
//!   unavailability ends the run typed — a DURABLE request never degrades
//!   to an ephemeral session, an EPHEMERAL request never degrades to
//!   local execution, and unsupported lifecycle operations surface typed
//!   refusals, never fake success.
//! - **Reconnect honors recovery semantics.** `unknown_orphaned` is a
//!   recoverable state: `reattach`/`reconnect` resume from the
//!   spine-issued revision and authoritative facts replay exactly once
//!   (dedup by event id) without regressing canonical state.

pub mod acpx;
pub mod controller;
pub mod facts;
#[cfg(test)]
pub(crate) mod fixtures;
pub mod router;
pub mod tachi_sink;
pub mod tool;

#[cfg(test)]
mod tests;

pub use acpx::{AcpxController, AcpxControllerConfig};
pub use controller::{
    ControllerError, ControllerEvent, GatedSessionController, PromptReceipt, SessionCapabilities,
    SessionCollectView, SessionController, SessionEventPage, SessionHandle, SessionStartSpec,
    SessionStopReceipt,
};
pub use facts::{SessionBinding, SessionEventFact, SessionFactSink};
pub use router::{DispatchError, DispatchPlan, plan_dispatch};
pub use tachi_sink::{TachiFactSinkConfig, TachiSessionFactSink};
pub use tool::{
    ExecutionRunRequest, ExecutionSessionInventory, ExecutionSubagentProfile, ExecutionSubagentTool,
};
