// Derived from grok-build (Apache-2.0), revision
// 1adcd1f477870e4a97bacbd6be78c8a3bfbac46d, from
// `.../grok_build/task/coordinator.rs`.
// Copyright 2023-2026 SpaceXAI. Licensed under the Apache License, Version 2.0.
//
// This file was CHANGED by ZeroClaw Labs: the usage-accounting commands and
// their `PromptScope` state, and the workflow owner with its cancel-drain
// waiters, were removed; upstream's `tracing` calls were dropped when this
// crate had no logging dependency, and the child-runner-panicked error is
// restored here through `zeroclaw_log::record!` now that the wiring phase has
// taken that dependency; results speak ZeroClaw's `ChildOutcome`; the
// child-panic guard is this crate's `ChildRunFuture` instead of
// `futures::FutureExt::catch_unwind`; the completed-record eviction and the
// buffered-completion bound are single-sourced in `state`.
// See ../LICENSE and ../NOTICE.

//! The single-writer coordinator actor.
//!
//! The actor owns the command receiver, the pending/active/completed records,
//! the blocking waiters, the foreground deadlines, cancellation, and the
//! decision of who is told about a child's ending. There is deliberately no
//! shared mutable state in this module: every transition happens in one task,
//! in one order, or the "delivered exactly once" guarantees are not guarantees.

mod query;

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use futures_util::stream::{FuturesUnordered, StreamExt};
use tokio::sync::mpsc;

use crate::outcome::ChildOutcome;
use crate::persistence::{ChildPersistence, NoopPersistence};
use crate::state::{
    ActiveChild, BlockingWaiter, BufferedCompletion, ChildRecord, ChildRunFuture, CompletedChild,
    InternalEvent, ListRequest, MAX_PENDING_COMPLETIONS, PendingChild, ProgressFuture,
    ProgressTarget, ReplyFuture, active_summary, background_at_deadline, background_if_caller_gone,
    cap_completion_output, completed_snapshot, completion_summary, evict_completed, panicked_result,
    sleep_until,
};
use crate::types::{
    CancelOutcome, CancelTarget, ChildRequest, ChildResult, CoordinatorCommand, DescribeOutcome,
    OutstandingReply, RegistryCounts, ResumeLookup, ResumeSource, SpawnedChildRef,
    ValidateTypeOutcome,
};

use crate::state::{
    ChildCompletion, ChildControl, ChildReporter, ChildRunOutput, ChildRunRequest, ChildRunner,
    CompletionDisposition, CoordinatorConfig,
};

/// Channel-owned child lifecycle actor.
///
/// `P` is the durability seam ([`ChildPersistence`]), defaulted to
/// [`NoopPersistence`] so [`Coordinator::new`] keeps building the same
/// unpersisted actor this crate has always built; a host that wants writes
/// through to a store uses [`Coordinator::with_persistence`] instead.
pub struct Coordinator<R: ChildRunner, P: ChildPersistence = NoopPersistence> {
    commands: mpsc::UnboundedReceiver<CoordinatorCommand>,
    internal_tx: mpsc::UnboundedSender<InternalEvent<R::Control>>,
    internal_rx: mpsc::UnboundedReceiver<InternalEvent<R::Control>>,
    runner: R,
    config: CoordinatorConfig,
    persistence: P,
    pending: HashMap<String, PendingChild>,
    active: HashMap<String, ActiveChild<R::Control>>,
    completed: HashMap<String, CompletedChild>,
    completed_order: VecDeque<String>,
    waiters: HashMap<String, Vec<BlockingWaiter>>,
    pending_completions: Vec<BufferedCompletion>,
    runs: FuturesUnordered<ChildRunFuture<R::RunFuture>>,
    validations: FuturesUnordered<ReplyFuture<R::ValidateFuture, ValidateTypeOutcome>>,
    descriptions: FuturesUnordered<ReplyFuture<R::DescribeFuture, DescribeOutcome>>,
    progress: FuturesUnordered<ProgressFuture<<R::Control as ChildControl>::ProgressFuture>>,
    list_requests: HashMap<u64, ListRequest>,
    next_list_request_id: u64,
}

impl<R: ChildRunner> Coordinator<R> {
    /// Build a coordinator with no durability seam plugged in.
    ///
    /// Behaviourally identical to this crate before it took a persistence
    /// port at all — see [`Coordinator::with_persistence`] to plug one in.
    pub fn new(
        commands: mpsc::UnboundedReceiver<CoordinatorCommand>,
        runner: R,
        config: CoordinatorConfig,
    ) -> Self {
        Self::with_persistence(commands, runner, config, NoopPersistence)
    }
}

impl<R: ChildRunner, P: ChildPersistence> Coordinator<R, P> {
    /// Build a coordinator backed by `persistence`.
    ///
    /// See [`ChildPersistence`] for the two-moment write-through contract
    /// this actor calls it under; a write failure there is logged and never
    /// blocks, delays, or unwinds a spawn or a finish.
    pub fn with_persistence(
        commands: mpsc::UnboundedReceiver<CoordinatorCommand>,
        runner: R,
        config: CoordinatorConfig,
        persistence: P,
    ) -> Self {
        let (internal_tx, internal_rx) = mpsc::unbounded_channel();
        Self {
            commands,
            internal_tx,
            internal_rx,
            runner,
            config,
            persistence,
            pending: HashMap::new(),
            active: HashMap::new(),
            completed: HashMap::new(),
            completed_order: VecDeque::new(),
            waiters: HashMap::new(),
            pending_completions: Vec::new(),
            runs: FuturesUnordered::new(),
            validations: FuturesUnordered::new(),
            descriptions: FuturesUnordered::new(),
            progress: FuturesUnordered::new(),
            list_requests: HashMap::new(),
            next_list_request_id: 0,
        }
    }

    /// Run until the command channel closes AND every in-flight future has
    /// settled. Children still alive at that point are cancelled.
    pub async fn run(mut self) {
        let mut commands_open = true;
        loop {
            if !commands_open
                && self.runs.is_empty()
                && self.validations.is_empty()
                && self.descriptions.is_empty()
                && self.progress.is_empty()
            {
                break;
            }

            let deadline = self.next_deadline();
            tokio::select! {
                biased;
                Some(event) = self.internal_rx.recv() => self.handle_internal(event),
                Some((id, output)) = self.runs.next(), if !self.runs.is_empty() => {
                    match output {
                        Ok(output) => self.finish_child(&id, output),
                        Err(_panicked) => self.finish_panicked_child(&id),
                    }
                }
                Some((respond_to, outcome)) = self.validations.next(), if !self.validations.is_empty() => {
                    let _ = respond_to.send(outcome);
                }
                Some((respond_to, outcome)) = self.descriptions.next(), if !self.descriptions.is_empty() => {
                    let _ = respond_to.send(outcome);
                }
                Some((seed, target, progress)) = self.progress.next(), if !self.progress.is_empty() => {
                    self.finish_progress(seed, target, progress);
                }
                command = self.commands.recv(), if commands_open => {
                    match command {
                        Some(command) => {
                            self.reap_abandoned_callers();
                            self.handle_command(command);
                        }
                        None => commands_open = false,
                    }
                }
                _ = sleep_until(deadline), if deadline.is_some() => self.process_deadlines(),
            }
            evict_completed(&mut self.completed, &mut self.completed_order);
        }

        self.cancel_all_children();
    }

    fn handle_command(&mut self, command: CoordinatorCommand) {
        match command {
            CoordinatorCommand::Spawn(command) => self.handle_spawn(command),
            CoordinatorCommand::Query(query) => {
                self.handle_query(
                    query.child_id,
                    query.parent_session_id,
                    query.block,
                    query.timeout_ms,
                    query.respond_to,
                );
            }
            CoordinatorCommand::Cancel(request) => match request.target {
                CancelTarget::ChildId(id) => {
                    let outcome = self.cancel_one(&id, request.parent_session_id.as_deref(), true);
                    let _ = request.respond_to.send(outcome);
                }
                CancelTarget::ParentPromptId(prompt_id) => {
                    self.cancel_parent_prompt(&prompt_id, request.parent_session_id.as_deref());
                    let _ = request.respond_to.send(CancelOutcome::Cancelled);
                }
            },
            CoordinatorCommand::ListActive(request) => {
                let summaries = self
                    .active
                    .values()
                    .filter(|child| child.request.parent_session_id == request.parent_session_id)
                    .map(active_summary)
                    .collect();
                let _ = request.respond_to.send(summaries);
            }
            CoordinatorCommand::ListRunning(request) => {
                self.handle_list_running(request.parent_session_id, request.respond_to);
            }
            CoordinatorCommand::Completions(request) => {
                let (owned, foreign): (Vec<_>, Vec<_>) =
                    std::mem::take(&mut self.pending_completions)
                        .into_iter()
                        .partition(|completion| {
                            request
                                .parent_session_id
                                .as_ref()
                                .is_none_or(|id| completion.parent_session_id == *id)
                        });
                self.pending_completions = foreign;
                let completions = owned
                    .into_iter()
                    .map(|completion| completion.summary)
                    .filter(|summary| !request.suppress_ids.contains(&summary.child_id))
                    .collect();
                let _ = request.respond_to.send(completions);
            }
            CoordinatorCommand::DiscardSessionCompletions { parent_session_id } => {
                self.pending_completions
                    .retain(|completion| completion.parent_session_id != parent_session_id);
            }
            CoordinatorCommand::Outstanding(request) => {
                // Reap again here so a turn-end poll sees an abandoned caller
                // even if no other command woke the actor first.
                self.reap_abandoned_callers();
                let mut live_ids: Vec<_> = self
                    .pending
                    .values()
                    .filter(|child| {
                        child.request.parent_session_id == request.parent_session_id
                            && child.request.parent_prompt_id.as_deref() == Some(&request.prompt_id)
                            && !child.handle_only
                    })
                    .map(|child| child.request.child_id.clone())
                    .chain(
                        self.active
                            .values()
                            .filter(|child| {
                                child.request.parent_session_id == request.parent_session_id
                                    && child.request.parent_prompt_id.as_deref()
                                        == Some(&request.prompt_id)
                                    && !child.handle_only
                                    // A definition-declared background child is
                                    // background for accounting even while the
                                    // spawning caller blocks on it.
                                    && !child.definition_background
                            })
                            .map(|child| child.request.child_id.clone()),
                    )
                    .collect();
                live_ids.sort();
                let background_live = self.pending.values().any(|child| {
                    child.request.parent_session_id == request.parent_session_id
                        && child.request.parent_prompt_id.as_deref() == Some(&request.prompt_id)
                        && child.handle_only
                }) || self.active.values().any(|child| {
                    child.request.parent_session_id == request.parent_session_id
                        && child.request.parent_prompt_id.as_deref() == Some(&request.prompt_id)
                        && (child.handle_only || child.definition_background)
                });
                let _ = request.respond_to.send(OutstandingReply {
                    live_ids,
                    background_live,
                });
            }
            CoordinatorCommand::RegistryCounts(request) => {
                let _ = request.respond_to.send(RegistryCounts {
                    pending: self.pending.len(),
                    active: self.active.len(),
                    completed: self.completed.len(),
                });
            }
            CoordinatorCommand::Inspect(request) => {
                self.handle_inspect(request.child_id, request.parent_session_id, request.respond_to);
            }
            CoordinatorCommand::SpawnedRefs(request) => {
                let mut refs: Vec<_> = self
                    .active
                    .values()
                    .filter(|child| {
                        child.request.parent_session_id == request.parent_session_id
                            && child.request.parent_prompt_id.as_deref() == Some(&request.prompt_id)
                    })
                    .map(|child| SpawnedChildRef {
                        child_id: child.request.child_id.clone(),
                        child_session_id: child.child_session_id.clone(),
                        agent_type: child.request.agent_type.clone(),
                        description: child.request.description.clone(),
                        persona: child.persona.clone(),
                        resumed_from: child.resumed_from.clone(),
                    })
                    .chain(
                        self.completed
                            .values()
                            .filter(|child| {
                                child.request.parent_session_id == request.parent_session_id
                                    && child.request.parent_prompt_id.as_deref()
                                        == Some(&request.prompt_id)
                            })
                            .map(|child| SpawnedChildRef {
                                child_id: child.request.child_id.clone(),
                                child_session_id: child.child_session_id.clone(),
                                agent_type: child.request.agent_type.clone(),
                                description: child.request.description.clone(),
                                persona: child.persona.clone(),
                                resumed_from: child.resumed_from.clone(),
                            }),
                    )
                    .collect();
                refs.sort_by(|a, b| a.child_id.cmp(&b.child_id));
                let _ = request.respond_to.send(refs);
            }
            CoordinatorCommand::ValidateType(request) => {
                self.validations.push(ReplyFuture {
                    future: Box::pin(
                        self.runner
                            .validate_type(request.agent_type, request.parent_session_id),
                    ),
                    respond_to: Some(request.respond_to),
                });
            }
            CoordinatorCommand::DescribeType(request) => {
                self.descriptions.push(ReplyFuture {
                    future: Box::pin(self.runner.describe_type(
                        request.agent_type,
                        request.harness_agent_type,
                        request.parent_session_id,
                    )),
                    respond_to: Some(request.respond_to),
                });
            }
            CoordinatorCommand::LoopUnitActive(request) => {
                let is_active = self.pending.values().any(|child| {
                    child.request.overrides.loop_task_id.as_deref() == Some(&request.task_id)
                }) || self.active.values().any(|child| {
                    child.request.overrides.loop_task_id.as_deref() == Some(&request.task_id)
                });
                let _ = request.respond_to.send(is_active);
            }
        }
    }

    fn handle_spawn(&mut self, command: crate::types::SpawnCommand) {
        let mut request = *command.request;
        // A child spawned BY a child is re-parented onto the root parent: the
        // grandparent is the only session that can act on it, and the
        // intermediate child must not surface work the parent never asked for.
        if let Some((root_parent, loop_task_id)) = self
            .active
            .values()
            .find(|child| child.child_session_id == request.parent_session_id)
            .map(|child| {
                (
                    child.request.parent_session_id.clone(),
                    child.request.overrides.loop_task_id.clone(),
                )
            })
        {
            request.parent_session_id = root_parent;
            request.surface_completion = false;
            if request.overrides.loop_task_id.is_none() {
                request.overrides.loop_task_id = loop_task_id;
            }
        }
        let id = request.child_id.clone();
        if self.pending.contains_key(&id)
            || self.active.contains_key(&id)
            || self.completed.contains_key(&id)
        {
            let _ = command.result_tx.send(ChildResult {
                outcome: ChildOutcome::Failed,
                detail: Some(format!("child id '{id}' already exists")),
                child_id: id.clone(),
                child_session_id: id,
                ..Default::default()
            });
            return;
        }
        let cancellation = request.cancel_token.clone();
        let handle_only = request.run_in_background;
        let foreground_deadline = (!request.run_in_background && !request.await_to_completion)
            .then(|| tokio::time::Instant::now() + self.config.foreground_budget);
        self.pending.insert(
            id.clone(),
            PendingChild {
                request: request.clone(),
                started_at: std::time::Instant::now(),
                cancellation: cancellation.clone(),
                spawn_reply: Some(command.result_tx),
                foreground_deadline,
                handle_only,
                explicitly_killed: false,
            },
        );
        // The row must exist before the child can possibly finish: a crash
        // between "accepted into pending" and "actually run" must still leave
        // a Running row behind for the reaper. Persistence is an observer
        // here, not a gate — a write failure is logged loudly and the spawn
        // proceeds regardless.
        if let Err(error) = self.persistence.record_spawn(&request) {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "child_id": request.child_id,
                        "parent_session_id": request.parent_session_id,
                        "error": error.to_string(),
                    })),
                "coordinator: failed to persist a child's spawn row; the spawn \
                 proceeds unpersisted"
            );
        }
        self.running_count_changed();
        let reporter = ChildReporter {
            child_id: id.clone(),
            tx: self.internal_tx.clone(),
        };
        self.runs.push(ChildRunFuture {
            child_id: id,
            future: Box::pin(self.runner.run(ChildRunRequest {
                request,
                cancellation,
                reporter,
            })),
            finished: false,
        });
    }

    fn handle_internal(&mut self, event: InternalEvent<R::Control>) {
        match event {
            InternalEvent::Started {
                child_id,
                child,
                respond_to,
            } => {
                let Some(pending) = self.pending.remove(&child_id) else {
                    let _ = respond_to.send(false);
                    return;
                };
                if pending.cancellation.is_cancelled() {
                    // Cancel-at-promote: cancellation arrived while the runtime
                    // was being built. Refusing the promotion is what tells the
                    // runner to tear the half-built child down; promoting it
                    // would leave a live child nobody asked for.
                    self.pending.insert(child_id, pending);
                    let _ = respond_to.send(false);
                    return;
                }
                self.active.insert(
                    child_id,
                    ActiveChild {
                        request: pending.request,
                        started_at: pending.started_at,
                        cancellation: pending.cancellation,
                        spawn_reply: pending.spawn_reply,
                        foreground_deadline: pending.foreground_deadline,
                        handle_only: pending.handle_only,
                        definition_background: child.definition_background,
                        explicitly_killed: pending.explicitly_killed,
                        child_session_id: child.child_session_id,
                        persona: child.persona,
                        resumed_from: child.resumed_from,
                        child_cwd: child.child_cwd,
                        worktree_path: child.worktree_path,
                        effective_model_id: child.effective_model_id,
                        control: child.control,
                    },
                );
                let _ = respond_to.send(true);
            }
            InternalEvent::ResumeSource {
                source_id,
                parent_session_id,
                respond_to,
            } => {
                let source_is_active =
                    self.pending
                        .get(&source_id)
                        .is_some_and(|child| child.request.parent_session_id == parent_session_id)
                        || self.active.get(&source_id).is_some_and(|child| {
                            child.request.parent_session_id == parent_session_id
                        });
                let lookup = if source_is_active {
                    ResumeLookup::Active
                } else if let Some(child) = self.completed.get(&source_id)
                    && child.request.parent_session_id == parent_session_id
                {
                    ResumeLookup::Completed(ResumeSource {
                        child_id: child.request.child_id.clone(),
                        child_session_id: child.child_session_id.clone(),
                        child_cwd: child.child_cwd.clone(),
                        worktree_path: child.worktree_path.clone(),
                        snapshot_ref: child.snapshot_ref.clone(),
                        agent_type: child.request.agent_type.clone(),
                        persona: child.persona.clone(),
                        model_id: Some(child.effective_model_id.clone()),
                    })
                } else {
                    ResumeLookup::Missing
                };
                let _ = respond_to.send(lookup);
            }
        }
    }

    fn finish_child(&mut self, id: &str, output: ChildRunOutput<R::CompletionData>) {
        let record = if let Some(child) = self.active.remove(id) {
            ChildRecord::Active(child)
        } else if let Some(child) = self.pending.remove(id) {
            ChildRecord::Pending(child)
        } else {
            return;
        };

        let request = record.request().clone();
        let explicitly_killed = record.explicitly_killed();
        let (
            started_at,
            child_session_id,
            persona,
            resumed_from,
            child_cwd,
            worktree_path,
            effective_model_id,
            mut spawn_reply,
            mut handle_only,
        ) = match record {
            ChildRecord::Pending(child) => (
                child.started_at,
                output.result.child_session_id.clone(),
                child.request.overrides.persona.clone(),
                child.request.resume_from.clone(),
                child.request.cwd.clone().unwrap_or_default(),
                output.result.worktree_path.clone(),
                String::new(),
                child.spawn_reply,
                child.handle_only,
            ),
            ChildRecord::Active(child) => (
                child.started_at,
                child.child_session_id,
                child.persona,
                child.resumed_from,
                child.child_cwd,
                child.worktree_path,
                child.effective_model_id,
                child.spawn_reply,
                child.handle_only,
            ),
        };

        let persisted_output_ref = self.runner.persisted_output_ref(&output.completion_data);
        let mut completed = CompletedChild {
            request: request.clone(),
            started_at,
            child_session_id,
            persona,
            resumed_from,
            child_cwd,
            worktree_path,
            snapshot_ref: output.snapshot_ref,
            persisted_output_ref,
            effective_model_id,
            result: output.result.clone(),
        };
        let snapshot = completed_snapshot(&completed, None);

        let mut waiter_delivered = false;
        for waiter in self.waiters.remove(id).unwrap_or_default() {
            waiter_delivered |= waiter.respond_to.send(Some(snapshot.clone())).is_ok();
        }

        let mut foreground_delivered = false;
        if let Some(respond_to) = spawn_reply.take() {
            let sent = respond_to.send(output.result.clone()).is_ok();
            if !handle_only {
                foreground_delivered = sent;
                handle_only = !sent;
            }
        } else if !handle_only {
            handle_only = true;
        }

        if self.config.buffer_completions && request.surface_completion {
            let mut summary = completion_summary(&request, &output.result);
            if let Some(cap) = self.config.buffered_completion_output_cap {
                summary.output = cap_completion_output(&summary.output, cap);
            }
            self.pending_completions.push(BufferedCompletion {
                parent_session_id: request.parent_session_id.clone(),
                summary,
            });
            // Bound the buffer, oldest first: a session unloaded without a
            // discard cannot grow it without end.
            if self.pending_completions.len() > MAX_PENDING_COMPLETIONS {
                let excess = self.pending_completions.len() - MAX_PENDING_COMPLETIONS;
                self.pending_completions.drain(..excess);
            }
        }
        if completed.persisted_output_ref.is_some() {
            completed.result.output = Arc::from("");
        }

        let should_surface = request.surface_completion
            && handle_only
            && !matches!(output.result.outcome, ChildOutcome::Cancelled)
            && !waiter_delivered
            && !explicitly_killed;
        let disposition = CompletionDisposition {
            foreground_delivered,
            backgrounded: handle_only,
            waiter_delivered,
            explicitly_killed,
            should_surface,
        };
        // One write, after the disposition is known: `delivered` is true when
        // the coordinator's own foreground path already handed this result to
        // a parent, in-process — the spawn caller's inline reply or a
        // blocking waiter. See `ChildPersistence::record_finish`'s doc for why
        // this must be a single call, not a terminal write followed by a
        // separate delivered write. Persistence is an observer, not a gate —
        // a write failure is logged loudly and the actor carries on.
        let delivered = disposition.foreground_delivered || disposition.waiter_delivered;
        if let Err(error) = self.persistence.record_finish(id, &output.result, delivered) {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "child_id": id,
                        "delivered": delivered,
                        "error": error.to_string(),
                    })),
                "coordinator: failed to persist a child's finish row; the \
                 ending was still delivered to every in-process waiter and \
                 caller"
            );
        }
        self.completed.insert(id.to_owned(), completed);
        self.completed_order.push_back(id.to_owned());
        self.running_count_changed();
        self.runner.on_completed(ChildCompletion {
            request,
            result: output.result,
            completion_data: output.completion_data,
            disposition,
        });
    }

    /// A child whose runtime unwound is finished as a failure, through the
    /// ordinary path: the actor survives, and every waiter, spawn caller and
    /// buffer that would have been served by a normal ending is still served.
    fn finish_panicked_child(&mut self, id: &str) {
        let request = self
            .active
            .get(id)
            .map(|child| child.request.clone())
            .or_else(|| self.pending.get(id).map(|child| child.request.clone()));
        let Some(request) = request else {
            return;
        };
        // This is the one event where the actor knows something no one else
        // can reconstruct. Under `panic = "abort"` (this workspace's release
        // profiles) the underlying unwind never reaches this code at all —
        // see `ChildRunFuture`'s doc in `state.rs` — so in a release build
        // this call site never fires and the failure surfaces only as an
        // ordinary `ChildOutcome::Failed` completion. In dev and test builds,
        // where `catch_unwind` is live, this log is the only place the fact
        // of the panic — as opposed to just "the child failed" — is ever
        // recorded. That is why it is ERROR, not DEBUG: gating it behind
        // `debug_enabled()` would make the one build where this is
        // observable also the one build where it is easiest to miss.
        ::zeroclaw_log::record!(
            ERROR,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({
                    "child_id": request.child_id,
                    "agent_type": request.agent_type,
                    "parent_session_id": request.parent_session_id,
                })),
            "coordinator: child runtime panicked; reporting it as an ordinary \
             failed completion so every waiter, spawn caller, and buffer still \
             gets an ending"
        );
        self.finish_child(
            id,
            ChildRunOutput {
                result: panicked_result(&request),
                completion_data: R::CompletionData::default(),
                snapshot_ref: None,
            },
        );
    }

    fn cancel_one(
        &mut self,
        id: &str,
        parent_session_id: Option<&str>,
        explicit: bool,
    ) -> CancelOutcome {
        if let Some(child) = self.active.get_mut(id)
            && belongs_to_session(&child.request, parent_session_id)
        {
            child.explicitly_killed |= explicit;
            child.cancellation.cancel();
            child.control.cancel();
            return CancelOutcome::Cancelled;
        }
        if let Some(child) = self.pending.get_mut(id)
            && belongs_to_session(&child.request, parent_session_id)
        {
            child.explicitly_killed |= explicit;
            child.cancellation.cancel();
            return CancelOutcome::Cancelled;
        }
        if let Some(child) = self.completed.get(id)
            && belongs_to_session(&child.request, parent_session_id)
        {
            return CancelOutcome::AlreadyFinished {
                outcome: child.result.outcome,
            };
        }
        CancelOutcome::NotFound
    }

    fn cancel_parent_prompt(&mut self, parent_prompt_id: &str, parent_session_id: Option<&str>) {
        for child in self.active.values() {
            if child.request.parent_prompt_id.as_deref() == Some(parent_prompt_id)
                && belongs_to_session(&child.request, parent_session_id)
            {
                child.cancellation.cancel();
                child.control.cancel();
            }
        }
        for child in self.pending.values() {
            if child.request.parent_prompt_id.as_deref() == Some(parent_prompt_id)
                && belongs_to_session(&child.request, parent_session_id)
            {
                child.cancellation.cancel();
            }
        }
    }

    fn next_deadline(&self) -> Option<tokio::time::Instant> {
        self.pending
            .values()
            .filter_map(|child| child.foreground_deadline)
            .chain(
                self.active
                    .values()
                    .filter_map(|child| child.foreground_deadline),
            )
            .chain(
                self.waiters
                    .values()
                    .flatten()
                    .map(|waiter| waiter.deadline),
            )
            .min()
    }

    fn reap_abandoned_callers(&mut self) {
        for child in self.pending.values_mut() {
            background_if_caller_gone(child);
        }
        for child in self.active.values_mut() {
            background_if_caller_gone(child);
        }
    }

    fn process_deadlines(&mut self) {
        self.reap_abandoned_callers();
        let now = tokio::time::Instant::now();
        for child in self.pending.values_mut() {
            background_at_deadline(child, now, self.config.foreground_budget);
        }
        for child in self.active.values_mut() {
            background_at_deadline(child, now, self.config.foreground_budget);
        }

        let ids: Vec<_> = self.waiters.keys().cloned().collect();
        for id in ids {
            let waiters = self.waiters.remove(&id).unwrap_or_default();
            let (due, live): (Vec<_>, Vec<_>) = waiters
                .into_iter()
                .partition(|waiter| waiter.deadline <= now);
            if !live.is_empty() {
                self.waiters.insert(id.clone(), live);
            }
            for waiter in due {
                if waiter.respond_to.is_closed() {
                    continue;
                }
                if self.active.contains_key(&id) {
                    self.queue_active_progress(&id, ProgressTarget::Query(waiter.respond_to));
                } else {
                    let _ = waiter.respond_to.send(self.ready_snapshot(&id));
                }
            }
        }
    }

    fn running_count_changed(&self) {
        self.runner
            .running_count_changed(self.pending.len() + self.active.len());
    }

    fn cancel_all_children(&self) {
        for child in self.active.values() {
            child.cancellation.cancel();
            child.control.cancel();
        }
        for child in self.pending.values() {
            child.cancellation.cancel();
        }
    }

    /// Give every child still in `pending` or `active` at Drop a `record_finish`
    /// write, so none of them is left `Running` forever in a store that can
    /// only be reclaimed by a same-boot reaper keyed on a heartbeat this row
    /// never got.
    ///
    /// `Lost` is the only honest outcome here: its own definition is "the
    /// process that owned it went away before it reported anything", and Drop
    /// running IS the owner going away — there is no run future left to poll
    /// for a real ending, cancellation tokens fire but nothing consumes their
    /// effect once `run()` has already returned or the actor task is being
    /// torn down. `ChildResult::default()` was deliberately made `Lost` for
    /// exactly this no-information ending; this reuses it rather than
    /// inventing a second spelling of the same fact. `delivered` is always
    /// `false`: nobody in-process ever received this result, by construction
    /// — a child with a real ending already left `pending`/`active` through
    /// `finish_child`, which already made its own `record_finish` call, so
    /// there is no double write here for anything that actually finished.
    ///
    /// Persistence errors here are logged, never propagated: `Drop` must not
    /// panic, and a store that cannot be written at shutdown is no more fatal
    /// here than it is at spawn or finish time.
    fn record_abandoned_children(&mut self) {
        let ids: Vec<String> = self
            .pending
            .keys()
            .chain(self.active.keys())
            .cloned()
            .collect();
        for id in ids {
            let child_session_id = self
                .active
                .get(&id)
                .map(|child| child.child_session_id.clone())
                .unwrap_or_default();
            let result = ChildResult {
                outcome: ChildOutcome::Lost,
                detail: Some(
                    "coordinator dropped while the child was still pending or active"
                        .to_owned(),
                ),
                child_id: id.clone(),
                child_session_id,
                ..Default::default()
            };
            if let Err(error) = self.persistence.record_finish(&id, &result, false) {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "child_id": id,
                            "error": error.to_string(),
                        })),
                    "coordinator: failed to persist an abandoned child's Lost \
                     row during Drop"
                );
            }
        }
    }
}

fn belongs_to_session(request: &ChildRequest, parent_session_id: Option<&str>) -> bool {
    parent_session_id.is_none_or(|id| request.parent_session_id == id)
}

/// Dropping the coordinator cancels every child it was holding, and gives
/// every child still `pending` or `active` a last, honest `record_finish`.
///
/// The actor is the only thing that would ever have delivered their results,
/// so a child that outlives it is work nobody will ever read — and, without
/// the `record_finish` write, work whose persisted row would be stuck
/// `Running` forever (see [`Self::record_abandoned_children`]).
impl<R: ChildRunner, P: ChildPersistence> Drop for Coordinator<R, P> {
    fn drop(&mut self) {
        self.cancel_all_children();
        self.record_abandoned_children();
    }
}

#[cfg(test)]
#[path = "coordinator_tests.rs"]
mod tests;
