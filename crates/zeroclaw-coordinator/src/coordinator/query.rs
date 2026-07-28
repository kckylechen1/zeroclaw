// Derived from grok-build (Apache-2.0), revision
// 1adcd1f477870e4a97bacbd6be78c8a3bfbac46d, from
// `.../grok_build/task/coordinator/query.rs`.
// Copyright 2023-2026 SpaceXAI. Licensed under the Apache License, Version 2.0.
//
// This file was CHANGED by ZeroClaw Labs: the workflow-owner visibility filters
// (which hid workflow-owned children from queries) were removed along with the
// workflow owner itself, and the types were renamed onto this crate's
// vocabulary. See ../../LICENSE and ../../NOTICE.

//! Session-scoped query, inspection, and progress delivery.

use std::sync::Arc;

use tokio::sync::oneshot;

use super::{ChildControl, ChildRunner, Coordinator, belongs_to_session};
use crate::state::{
    BlockingWaiter, ChildProgress, CompletedChild, ListRequest, OUTPUT_UNAVAILABLE_PLACEHOLDER,
    ProgressFuture, ProgressTarget, RunningSeed, completed_inspection, completed_snapshot,
    pending_inspection, pending_snapshot, running_inspection, running_seed,
};
use crate::types::{ChildInspection, ChildSnapshot};

/// Default wait for a blocking query that names no timeout.
const DEFAULT_BLOCKING_QUERY_TIMEOUT_MS: u64 = 30_000;

impl<R: ChildRunner> Coordinator<R> {
    pub(super) fn handle_query(
        &mut self,
        id: String,
        parent_session_id: Option<String>,
        block: bool,
        timeout_ms: Option<u64>,
        respond_to: oneshot::Sender<Option<ChildSnapshot>>,
    ) {
        if let Some(child) = self
            .completed
            .get(&id)
            .filter(|child| belongs_to_session(&child.request, parent_session_id.as_deref()))
        {
            let _ = respond_to.send(Some(self.completed_snapshot_for_query(child)));
            return;
        }
        if self
            .active
            .get(&id)
            .is_some_and(|child| belongs_to_session(&child.request, parent_session_id.as_deref()))
        {
            if block {
                self.push_waiter(id, timeout_ms, respond_to);
            } else {
                self.queue_active_progress(&id, ProgressTarget::Query(respond_to));
            }
            return;
        }
        let pending_snapshot_now = self
            .pending
            .get(&id)
            .filter(|child| belongs_to_session(&child.request, parent_session_id.as_deref()))
            .map(pending_snapshot);
        if let Some(snapshot) = pending_snapshot_now {
            if block {
                // A child that has not finished initializing is still a child
                // to wait for, not a miss.
                self.push_waiter(id, timeout_ms, respond_to);
            } else {
                let _ = respond_to.send(Some(snapshot));
            }
            return;
        }
        let _ = respond_to.send(None);
    }

    fn push_waiter(
        &mut self,
        id: String,
        timeout_ms: Option<u64>,
        respond_to: oneshot::Sender<Option<ChildSnapshot>>,
    ) {
        self.waiters.entry(id).or_default().push(BlockingWaiter {
            deadline: tokio::time::Instant::now()
                + std::time::Duration::from_millis(
                    timeout_ms.unwrap_or(DEFAULT_BLOCKING_QUERY_TIMEOUT_MS),
                ),
            respond_to,
        });
    }

    pub(super) fn handle_inspect(
        &mut self,
        id: String,
        parent_session_id: Option<String>,
        respond_to: oneshot::Sender<Option<ChildInspection>>,
    ) {
        if let Some(child) = self
            .completed
            .get(&id)
            .filter(|child| belongs_to_session(&child.request, parent_session_id.as_deref()))
        {
            let _ = respond_to.send(Some(self.completed_inspection_for_query(child)));
        } else if let Some(child) = self
            .pending
            .get(&id)
            .filter(|child| belongs_to_session(&child.request, parent_session_id.as_deref()))
        {
            let _ = respond_to.send(Some(pending_inspection(child)));
        } else if self
            .active
            .get(&id)
            .is_some_and(|child| belongs_to_session(&child.request, parent_session_id.as_deref()))
        {
            self.queue_active_progress(&id, ProgressTarget::Inspect(respond_to));
        } else {
            let _ = respond_to.send(None);
        }
    }

    /// A finished child's output may have been handed to the host to persist;
    /// if the host cannot produce it again, say so rather than reporting empty
    /// output as the child's answer.
    fn persisted_output(&self, child: &CompletedChild) -> Option<Arc<str>> {
        child.persisted_output_ref.as_deref().map(|reference| {
            self.runner
                .load_persisted_output(reference)
                .unwrap_or_else(|| Arc::from(OUTPUT_UNAVAILABLE_PLACEHOLDER))
        })
    }

    fn completed_snapshot_for_query(&self, child: &CompletedChild) -> ChildSnapshot {
        let output = self.persisted_output(child);
        completed_snapshot(child, output.as_deref())
    }

    fn completed_inspection_for_query(&self, child: &CompletedChild) -> ChildInspection {
        let output = self.persisted_output(child);
        completed_inspection(child, output.as_deref())
    }

    pub(super) fn ready_snapshot(&self, id: &str) -> Option<ChildSnapshot> {
        self.completed
            .get(id)
            .map(|child| self.completed_snapshot_for_query(child))
            .or_else(|| self.pending.get(id).map(pending_snapshot))
    }

    pub(super) fn handle_list_running(
        &mut self,
        parent_session_id: String,
        respond_to: oneshot::Sender<Vec<ChildInspection>>,
    ) {
        let ids: Vec<_> = self
            .active
            .values()
            .filter(|child| child.request.parent_session_id == parent_session_id)
            .map(|child| child.request.child_id.clone())
            .collect();
        if ids.is_empty() {
            let _ = respond_to.send(Vec::new());
            return;
        }

        let request_id = self.next_list_request_id;
        self.next_list_request_id = self.next_list_request_id.wrapping_add(1);
        self.list_requests.insert(
            request_id,
            ListRequest {
                slots: vec![None; ids.len()],
                remaining: ids.len(),
                respond_to,
            },
        );
        for (index, id) in ids.into_iter().enumerate() {
            self.queue_active_progress(&id, ProgressTarget::List { request_id, index });
        }
    }

    /// Ask a live child for its progress. The reply is delivered later, from
    /// the actor loop, because reading progress is the runtime's work and the
    /// actor must not block on it.
    pub(super) fn queue_active_progress(&mut self, id: &str, target: ProgressTarget) {
        let Some(child) = self.active.get(id) else {
            match target {
                ProgressTarget::Query(tx) => {
                    let _ = tx.send(self.ready_snapshot(id));
                }
                ProgressTarget::Inspect(tx) => {
                    let value = self
                        .completed
                        .get(id)
                        .map(|child| self.completed_inspection_for_query(child));
                    let _ = tx.send(value);
                }
                ProgressTarget::List { request_id, index } => {
                    self.finish_list_slot(request_id, index, None);
                }
            }
            return;
        };
        self.progress.push(ProgressFuture {
            future: Box::pin(child.control.progress()),
            seed: Some(running_seed(child)),
            target: Some(target),
        });
    }

    pub(super) fn finish_progress(
        &mut self,
        seed: RunningSeed,
        target: ProgressTarget,
        progress: ChildProgress,
    ) {
        // The child may have finished while its progress was in flight; a
        // stale "running" answer would restart somebody's wait.
        let still_active = self.active.contains_key(&seed.child_id);
        if !still_active {
            match target {
                ProgressTarget::Query(respond_to) => {
                    let _ = respond_to.send(self.ready_snapshot(&seed.child_id));
                }
                ProgressTarget::Inspect(respond_to) => {
                    let value = self
                        .completed
                        .get(&seed.child_id)
                        .map(|child| self.completed_inspection_for_query(child));
                    let _ = respond_to.send(value);
                }
                ProgressTarget::List { request_id, index } => {
                    self.finish_list_slot(request_id, index, None);
                }
            }
            return;
        }
        let inspection = running_inspection(seed, progress);
        match target {
            ProgressTarget::Query(respond_to) => {
                let _ = respond_to.send(Some(inspection.snapshot));
            }
            ProgressTarget::Inspect(respond_to) => {
                let _ = respond_to.send(Some(inspection));
            }
            ProgressTarget::List { request_id, index } => {
                self.finish_list_slot(request_id, index, Some(inspection));
            }
        }
    }

    fn finish_list_slot(
        &mut self,
        request_id: u64,
        index: usize,
        inspection: Option<ChildInspection>,
    ) {
        let Some(request) = self.list_requests.get_mut(&request_id) else {
            return;
        };
        request.slots[index] = inspection;
        request.remaining = request.remaining.saturating_sub(1);
        if request.remaining != 0 {
            return;
        }
        let Some(request) = self.list_requests.remove(&request_id) else {
            return;
        };
        let values = request.slots.into_iter().flatten().collect();
        let _ = request.respond_to.send(values);
    }
}
