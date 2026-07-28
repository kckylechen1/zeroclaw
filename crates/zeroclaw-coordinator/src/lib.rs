// Derived from grok-build (Apache-2.0), revision
// 1adcd1f477870e4a97bacbd6be78c8a3bfbac46d, from
// `crates/codegen/xai-grok-tools/src/implementations/grok_build/task/`.
// Copyright 2023-2026 SpaceXAI. Licensed under the Apache License, Version 2.0.
//
// This file was CHANGED by ZeroClaw Labs: it is a new crate root that did not
// exist upstream (upstream shipped the coordinator as a private module inside a
// tool crate). See ../LICENSE and ../NOTICE.

//! Single-writer coordinator for child agent runs.
//!
//! One actor owns every piece of mutable lifecycle state: the pending, active
//! and completed children, the blocking waiters, the foreground deadlines, the
//! cancellation tokens, and the decision of who gets told when a child ends.
//! Nothing else may write it. That is the whole design: a child's ending is
//! delivered exactly once, to exactly one place, chosen by a single writer that
//! saw every prior transition.
//!
//! ## The seam
//!
//! [`ChildRunner`] is the only host-specific part. The coordinator never learns
//! how a child is actually executed — it hands the runner a [`ChildRunRequest`]
//! and receives a [`ChildRunOutput`]. The runner reports back through
//! [`ChildReporter`], whose [`started`](ChildReporter::started) acknowledgement
//! closes the cancel-at-promote race.
//!
//! The runner's associated futures carry no unconditional `Send` bound: a
//! single-threaded host may return non-`Send` futures and the resulting actor
//! future inherits that property.
//!
//! ## What this crate deliberately does not do
//!
//! It has no tool frontend, no persistence, and no dependency on the rest of
//! the ZeroClaw workspace. [`ChildOutcome`] is defined here rather than
//! imported from `zeroclaw-api` so that the wiring phase decides that
//! dependency edge on purpose; its variants are ZeroClaw's five terminal
//! outcomes, one-to-one, so that mapping is a rename and never a translation.

mod backend;
mod cancel;
mod coordinator;
mod outcome;
mod state;
mod types;

pub use backend::{
    ChannelBackend, CoordinatorError, VALIDATE_TYPE_TIMEOUT, VALIDATE_TYPE_TIMEOUT_ENV_VAR,
    env_duration_or, validate_type_timeout,
};
pub use cancel::CancelToken;
pub use coordinator::Coordinator;
pub use outcome::ChildOutcome;
pub use state::{
    ChildCompletion, ChildControl, ChildProgress, ChildReporter, ChildRunOutput, ChildRunRequest,
    ChildRunner, CompletionDisposition, CoordinatorConfig, LocalBoxFuture, MAX_COMPLETED_ENTRIES,
    MAX_PENDING_COMPLETIONS, SendBoxFuture, StartedChild, cap_completion_output, completion_summary,
};
pub use types::{
    ActiveChildSummary, CancelCommand, CancelOutcome, CancelTarget, ChildCompletionSummary,
    ChildInspection, ChildOverrides, ChildRequest, ChildResult, ChildSnapshot, ChildStatus,
    ChildTypeSummary, CommandSender, CompletionsCommand, CoordinatorCommand, DescribeOutcome,
    DescribeTypeCommand, InspectCommand, ListActiveCommand, ListRunningCommand,
    LoopUnitActiveCommand, OutstandingCommand, OutstandingReply, QueryCommand, RegistryCounts,
    RegistryCountsCommand, ResumeLookup, ResumeSource, SpawnCommand, SpawnedChildRef,
    SpawnedRefsCommand, ValidateTypeCommand, ValidateTypeOutcome,
};
