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
//! It has no tool frontend. [`ChildOutcome`] stays defined here rather than
//! becoming a re-export of `zeroclaw_api::announce::AnnouncedOutcome`, even
//! now that the wiring phase has taken the dependency: its variants are
//! ZeroClaw's five terminal outcomes, one-to-one with `AnnouncedOutcome`, and
//! the `From` conversions in [`outcome`] are the only place that vocabulary
//! edge is crossed — everything else in this crate still speaks
//! `ChildOutcome`. [`ChildPersistence`] defines the durability seam and
//! [`Coordinator`] calls it, but this crate still ships no implementation of
//! it: [`Coordinator::new`] defaults the seam to [`NoopPersistence`], so a
//! host that plugs nothing in through
//! [`Coordinator::with_persistence`](Coordinator::with_persistence) gets the
//! same in-memory-only behavior this crate has always had.

mod backend;
mod cancel;
mod coordinator;
mod outcome;
mod persistence;
mod state;
mod types;

pub use backend::{
    ChannelBackend, CoordinatorError, SPAWN_ADMISSION_TIMEOUT, SPAWN_ADMISSION_TIMEOUT_ENV_VAR,
    VALIDATE_TYPE_TIMEOUT, VALIDATE_TYPE_TIMEOUT_ENV_VAR, env_duration_or, spawn_admission_timeout,
    validate_type_timeout,
};
pub use cancel::CancelToken;
pub use coordinator::Coordinator;
pub use outcome::ChildOutcome;
pub use persistence::{ChildPersistence, NoopPersistence, PersistenceError};
pub use state::{
    ChildCompletion, ChildControl, ChildProgress, ChildReporter, ChildRunOutput, ChildRunRequest,
    ChildRunner, CompletionDisposition, CoordinatorConfig, LocalBoxFuture, MAX_COMPLETED_ENTRIES,
    MAX_PENDING_COMPLETIONS, SendBoxFuture, StartedChild, at_child_capacity, cap_completion_output,
    completion_summary, exceeds_spawn_depth,
};
pub use types::{
    ActiveChildSummary, CancelCommand, CancelOutcome, CancelTarget, ChildCompletionSummary,
    ChildInspection, ChildOverrides, ChildRequest, ChildResult, ChildSnapshot, ChildStatus,
    ChildTypeSummary, CommandSender, CompletionsCommand, CoordinatorCommand, DescribeOutcome,
    DescribeTypeCommand, InspectCommand, ListActiveCommand, ListRunningCommand,
    LoopUnitActiveCommand, OutstandingCommand, OutstandingReply, QueryCommand, RegistryCounts,
    RegistryCountsCommand, ResumeLookup, ResumeSource, SpawnAdmission, SpawnCommand, SpawnRefusal,
    SpawnedChildRef, SpawnedRefsCommand, ValidateTypeCommand, ValidateTypeOutcome,
};
