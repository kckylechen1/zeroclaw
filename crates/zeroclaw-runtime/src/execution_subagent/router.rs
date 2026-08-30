//! The Parent-level three-path dispatch gate (#198 addendum #2; #261's
//! router discrimination).
//!
//! ```text
//! ExecutionRequestV1 ──▶ ExecutionRouteV1::route  (typed, total, api)
//!       └─▶ plan_dispatch              (THIS FILE: availability-gated)
//!              Reason      → the Parent runs reasoning_subagent itself
//!              Ephemeral   → ExecutionSubagentTool::run  (requires the tool)
//!              Durable     → TachiTaskBridge submit      (requires the bridge)
//! ```
//!
//! Law: **the route is chosen BEFORE availability and availability can
//! only fail the chosen path CLOSED.** A DURABLE request with no bridge
//! is a typed error (`DurableRequiresBridge`) — it never degrades to an
//! ephemeral session or local execution (TB-20's law, carried to this
//! gate). An EPHEMERAL request with no tool configured is likewise a
//! typed error, never a silent local run. A REASON request touches
//! neither the controller nor the bridge.

use crate::subagent_v1::ObjectiveV1;
use zeroclaw_api::session_exec::{ExecutionRequestV1, ExecutionRouteV1};

use super::tool::ExecutionRunRequest;

/// The gated dispatch plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DispatchPlan {
    /// Pure analysis — no session, no task, no ports touched.
    Reason,
    /// Run the ephemeral ExecutionSubAgent (the tool executes the run).
    Ephemeral { run: ExecutionRunRequest },
    /// Submit through the durable Tachi bridge. The plan carries no
    /// execution capability: the caller submits via `TachiBridgeClient`
    /// (submit/get/watch/collect), which mints the TaskRef tachi-side.
    Durable,
}

/// Typed dispatch failures. Every variant is fail-closed: there is no
/// `fallback` variant and no caller path that converts these into local
/// execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DispatchError {
    /// A durable request arrived but no bridge is configured. The request
    /// MUST NOT be executed locally or ephemerally; surface this typed
    /// error to the user.
    DurableRequiresBridge,
    /// An ephemeral request arrived but no ExecutionSubagentTool is
    /// configured. Never silently run locally.
    EphemeralRequiresController,
    /// The objective exceeded the bounded ceiling.
    ObjectiveTooLarge,
}

/// Plan one execution request. Availability inputs are configuration
/// facts (is the bridge configured?), never runtime probe results — a
/// runtime outage fails the EXECUTION typed (the bridge client returns
/// `Unavailable`; the tool returns `Refused`), never the route.
pub fn plan_dispatch(
    request: &ExecutionRequestV1,
    bridge_configured: bool,
    tool_configured: bool,
) -> Result<DispatchPlan, DispatchError> {
    match ExecutionRouteV1::route(request) {
        ExecutionRouteV1::Reason => Ok(DispatchPlan::Reason),
        ExecutionRouteV1::EphemeralExec => {
            let bounded = ObjectiveV1::new(request.objective.clone())
                .map_err(|_| DispatchError::ObjectiveTooLarge)?;
            if !tool_configured {
                return Err(DispatchError::EphemeralRequiresController);
            }
            Ok(DispatchPlan::Ephemeral {
                run: ExecutionRunRequest {
                    objective: bounded.as_str().to_string(),
                    correction_prompt: None,
                },
            })
        }
        ExecutionRouteV1::DurableExec => {
            // Bounded BEFORE the availability gate so an oversize durable
            // objective is a typed bound failure, not a bridge round-trip.
            ObjectiveV1::new(request.objective.clone())
                .map_err(|_| DispatchError::ObjectiveTooLarge)?;
            if !bridge_configured {
                return Err(DispatchError::DurableRequiresBridge);
            }
            Ok(DispatchPlan::Durable)
        }
    }
}
