//! Test doubles for the ExecutionSubAgent ports — STRICTLY test-only.
//!
//! The scripted controller stands in for the ACPX transport (its event
//! shapes follow the fixture harness pattern); the in-memory sink is a
//! fact LEDGER — structurally the second durable store the freeze
//! forbids if it existed in production, so it is `cfg(test)`-gated and
//! never constructible from production code (same law as the
//! tachi_bridge's in-memory double).

use async_trait::async_trait;
use parking_lot::Mutex;
use std::collections::VecDeque;
use zeroclaw_api::session_exec::{
    AuthorityConfirmationRef, InterventionRequestIdRef, SessionAdvertiseReceiptView,
    SessionAttachmentRef, SessionCanonicalStateV1, SessionConnectionFactV1, SessionEventIdRef,
    SessionEventKindV1, SessionEventReceiptView, SessionFactError,
    SessionInterventionDispositionV1, SessionInterventionKindV1, SessionInterventionRequestView,
    SessionReceiptAdmissionV1, SessionReconnectReceiptView, SessionStateView,
    SessionTerminalOutcomeV1,
};

use super::controller::{
    ControllerError, ControllerEvent, PromptReceipt, SessionCapabilities, SessionCollectView,
    SessionController, SessionEventPage, SessionHandle, SessionStartSpec, SessionStopReceipt,
};
use super::facts::{SessionBinding, SessionEventFact, SessionFactSink};

/// One scripted behavior the fixture controller performs.
#[derive(Clone, Debug)]
pub enum ScriptedStep {
    /// Emit these events on the next watch call.
    Emit(Vec<ControllerEvent>),
    /// Fail the next watch/prompt/stop/collect with `Unavailable`.
    TransportDown,
    /// Transport is back.
    TransportUp,
}

/// A scripted ACPX-shaped harness session. Deterministic; no threads.
#[derive(Default)]
pub struct ScriptedController {
    /// Capabilities the STARTED session will advertise (the declared
    /// attach set). Sessions started through `start` carry this set.
    pub declared: SessionCapabilities,
    pub started: Mutex<Vec<SessionHandle>>,
    pub prompts: Mutex<Vec<String>>,
    pub queue: Mutex<VecDeque<ScriptedStep>>,
    pub events: Mutex<Vec<ControllerEvent>>,
    pub next_seq: Mutex<u64>,
    pub unavailable: Mutex<bool>,
    pub started_count: Mutex<u32>,
    pub stop_requests: Mutex<Vec<bool>>,
    pub interrupt_requests: Mutex<u32>,
    /// When set, `stop` reports a confirmed cancel with this ref.
    pub stop_confirmation: Option<AuthorityConfirmationRef>,
    /// When set, `start` refuses with this typed error.
    pub start_refusal: Option<ControllerError>,
    pub collect_view: Mutex<Option<SessionCollectView>>,
}

impl ScriptedController {
    #[must_use]
    pub fn new(declared: SessionCapabilities) -> Self {
        Self {
            declared,
            ..Self::default()
        }
    }

    pub fn push(&self, step: ScriptedStep) {
        self.queue.lock().push_back(step);
    }

    fn unavailable(&self) -> bool {
        *self.unavailable.lock()
    }

    async fn drain_queue(&self) -> Result<(), ControllerError> {
        while let Some(step) = self.queue.lock().pop_front() {
            match step {
                ScriptedStep::Emit(new_events) => {
                    let mut events = self.events.lock();
                    for mut event in new_events {
                        let seq = *self.next_seq.lock() + 1;
                        *self.next_seq.lock() = seq;
                        event.seq = seq;
                        events.push(event);
                    }
                }
                ScriptedStep::TransportDown => {
                    *self.unavailable.lock() = true;
                    return Err(ControllerError::Unavailable);
                }
                ScriptedStep::TransportUp => {
                    *self.unavailable.lock() = false;
                }
            }
        }
        if self.unavailable() {
            return Err(ControllerError::Unavailable);
        }
        Ok(())
    }
}

#[async_trait]
impl SessionController for ScriptedController {
    async fn start(&self, spec: &SessionStartSpec) -> Result<SessionHandle, ControllerError> {
        self.drain_queue().await?;
        if spec.prompt.is_empty() {
            return Err(ControllerError::Refused("empty prompt".to_string()));
        }
        *self.started_count.lock() += 1;
        if let Some(refusal) = &self.start_refusal {
            return Err(refusal.clone());
        }
        if self.unavailable() {
            return Err(ControllerError::Unavailable);
        }
        // The TRANSPORT mints the remote session identity (consumer-side
        // specs cannot choose it): a per-start counter keeps minted ids
        // distinct so a caller cannot pre-aim a binding at someone else's
        // session id.
        let n = *self.started_count.lock();
        let handle = SessionHandle {
            remote_session: zeroclaw_api::session_exec::RemoteSessionRef::from_opaque(format!(
                "rs-fixture-{n}"
            )),
            capabilities: self.declared,
        };
        self.started.lock().push(handle.clone());
        // Seeding event: the session fact stream opens with accepted+started
        // (mirroring the real spine's attach→started sequence observed by
        // the consumer through watch).
        let mut events = self.events.lock();
        let seq = *self.next_seq.lock() + 1;
        *self.next_seq.lock() = seq;
        events.push(ControllerEvent {
            seq,
            event_id: SessionEventIdRef::from_opaque(format!("ev-{seq}")),
            kind: SessionEventKindV1::Accepted,
            outcome: None,
            summary: None,
        });
        Ok(handle)
    }

    async fn watch(
        &self,
        _handle: &SessionHandle,
        after_seq: u64,
        limit: usize,
    ) -> Result<SessionEventPage, ControllerError> {
        self.drain_queue().await?;
        let events = self.events.lock();
        let pending: Vec<ControllerEvent> = events
            .iter()
            .filter(|event| event.seq > after_seq)
            .take(limit.max(1))
            .cloned()
            .collect();
        let next_seq = pending
            .last()
            .map(|event| event.seq)
            .unwrap_or(after_seq)
            .max(after_seq);
        Ok(SessionEventPage {
            events: pending,
            next_seq,
        })
    }

    async fn prompt(
        &self,
        _handle: &SessionHandle,
        text: &str,
    ) -> Result<PromptReceipt, ControllerError> {
        if self.unavailable() {
            return Err(ControllerError::Unavailable);
        }
        self.prompts.lock().push(text.to_string());
        Ok(PromptReceipt {
            accepted: true,
            detail: None,
        })
    }

    async fn interrupt(&self, _handle: &SessionHandle) -> Result<(), ControllerError> {
        if self.unavailable() {
            return Err(ControllerError::Unavailable);
        }
        *self.interrupt_requests.lock() += 1;
        Ok(())
    }

    async fn stop(
        &self,
        _handle: &SessionHandle,
        graceful: bool,
    ) -> Result<SessionStopReceipt, ControllerError> {
        if self.unavailable() {
            return Err(ControllerError::Unavailable);
        }
        self.stop_requests.lock().push(graceful);
        // The fixture records a terminal cancelled fact ONLY when the stop
        // was confirmable (confirmation ref present); the spine's law that
        // a receipt is not a state is exercised at the sink layer.
        if let Some(confirmation) = self.stop_confirmation.clone() {
            let mut events = self.events.lock();
            let seq = *self.next_seq.lock() + 1;
            *self.next_seq.lock() = seq;
            events.push(ControllerEvent {
                seq,
                event_id: SessionEventIdRef::from_opaque(format!("ev-{seq}")),
                kind: SessionEventKindV1::Terminal,
                outcome: Some(SessionTerminalOutcomeV1::Cancelled {
                    confirmation: confirmation.clone(),
                }),
                summary: None,
            });
            Ok(SessionStopReceipt {
                confirmed: true,
                authority_confirmation_ref: Some(confirmation),
                detail: None,
            })
        } else {
            Ok(SessionStopReceipt {
                confirmed: false,
                authority_confirmation_ref: None,
                detail: Some("stop requested; no confirmation available".to_string()),
            })
        }
    }

    async fn collect(
        &self,
        _handle: &SessionHandle,
    ) -> Result<SessionCollectView, ControllerError> {
        if self.unavailable() {
            return Err(ControllerError::Unavailable);
        }
        let guarded = self.collect_view.lock();
        Ok(guarded.clone().unwrap_or_else(|| SessionCollectView {
            summary: Some("fixture terminal summary".to_string()),
            digest: "deadbeef".to_string(),
            evidence_refs: vec![],
        }))
    }

    async fn reattach(
        &self,
        _adapter_connection: &zeroclaw_api::session_exec::AdapterConnectionRef,
        remote_session: &zeroclaw_api::session_exec::RemoteSessionRef,
        _resume_from_revision: u64,
    ) -> Result<SessionHandle, ControllerError> {
        if self.unavailable() {
            return Err(ControllerError::Unavailable);
        }
        Ok(SessionHandle {
            remote_session: remote_session.clone(),
            capabilities: self.declared,
        })
    }
}

/// In-memory receipt ledger mirroring the spine's consumer-facing laws:
/// replay-idempotent events by id, monotone revisions, no state regression,
/// typed unsupported refusals with zero writes. TEST-ONLY (see module doc).
fn rank(kind: SessionEventKindV1) -> i64 {
    match kind {
        SessionEventKindV1::Accepted => 0,
        SessionEventKindV1::Started => 1,
        SessionEventKindV1::Progress | SessionEventKindV1::InputRequired => 2,
        SessionEventKindV1::Terminal => 3,
        SessionEventKindV1::Cleanup => 4,
    }
}

#[derive(Default)]
pub struct InMemoryFactSink {
    pub attachment: Mutex<Option<SessionAttachmentRef>>,
    pub attachments_created: Mutex<u32>,
    pub advertised: Mutex<Vec<Vec<String>>>,
    pub facts: Mutex<Vec<(SessionEventFact, SessionReceiptAdmissionV1)>>,
    pub seen_event_ids: Mutex<Vec<String>>,
    pub connection_facts: Mutex<Vec<SessionConnectionFactV1>>,
    pub results: Mutex<Vec<(String, SessionInterventionDispositionV1)>>,
    pub unavailable: Mutex<bool>,
    /// Intervention requests available for pickup.
    pub pending_requests: Mutex<Vec<SessionInterventionRequestView>>,
    /// Issued requests: (attachment, request_id, kind, reason).
    pub requests: Mutex<Vec<(String, String, String, String)>>,
    /// The canonical revision high-water (the stale-guard rank).
    pub canonical_revision: Mutex<u64>,
    /// The lifecycle rank high-water: a fact ranked below this is
    /// journaled stale WITHOUT advancing anything (rank+revision guard).
    pub reached_rank: Mutex<i64>,
    pub reconnections: Mutex<u32>,
}

impl InMemoryFactSink {
    fn unavailable(&self) -> Result<(), SessionFactError> {
        if *self.unavailable.lock() {
            Err(SessionFactError::Unavailable)
        } else {
            Ok(())
        }
    }
}

#[async_trait]
impl SessionFactSink for InMemoryFactSink {
    async fn attach(
        &self,
        binding: &SessionBinding,
        _capabilities: &[String],
    ) -> Result<SessionAttachmentRef, SessionFactError> {
        self.unavailable()?;
        let mut created = self.attachments_created.lock();
        let mut attachment = self.attachment.lock();
        match attachment.clone() {
            Some(existing) => Ok(existing),
            None => {
                *created += 1;
                let fresh = SessionAttachmentRef::from_opaque(format!("att-{}", *created));
                let _ = binding;
                *attachment = Some(fresh.clone());
                Ok(fresh)
            }
        }
    }

    async fn advertise_capabilities(
        &self,
        attachment: &SessionAttachmentRef,
        capabilities: &[String],
    ) -> Result<SessionAdvertiseReceiptView, SessionFactError> {
        self.unavailable()?;
        self.advertised.lock().push(capabilities.to_vec());
        let seq = self.advertised.lock().len() as u64;
        Ok(SessionAdvertiseReceiptView {
            attachment_ref: attachment.clone(),
            advertisement_seq: seq,
            capabilities: capabilities.to_vec(),
        })
    }

    async fn ingest_event(
        &self,
        attachment: &SessionAttachmentRef,
        fact: &SessionEventFact,
    ) -> Result<SessionEventReceiptView, SessionFactError> {
        self.unavailable()?;
        // Replay-idempotent by event id.
        let mut seen = self.seen_event_ids.lock();
        if seen.contains(&fact.event_id.as_str().to_string()) {
            return Ok(SessionEventReceiptView {
                attachment_ref: attachment.clone(),
                event_id: fact.event_id.clone(),
                admission: SessionReceiptAdmissionV1::Replayed,
                disposition: "journaled_replayed".to_string(),
                state: self.read_state(),
            });
        }
        // Rank+revision guard (mirrors the spine): a fact whose revision
        // is not fresher than the high-water, OR whose lifecycle rank is
        // below the reached phase, is journaled stale and advances
        // NOTHING (neither the projection nor the revision high-water).
        let fact_rank = rank(fact.kind);
        let mut revision = self.canonical_revision.lock();
        let mut reached = self.reached_rank.lock();
        let (admission, disposition) = if fact.source_revision <= *revision || fact_rank < *reached
        {
            (
                SessionReceiptAdmissionV1::Replayed,
                "journaled_stale".to_string(),
            )
        } else {
            *revision = fact.source_revision;
            *reached = (*reached).max(fact_rank);
            seen.push(fact.event_id.as_str().to_string());
            (SessionReceiptAdmissionV1::Created, "advanced".to_string())
        };
        drop(revision);
        drop(reached);
        self.facts.lock().push((fact.clone(), admission));
        Ok(SessionEventReceiptView {
            attachment_ref: attachment.clone(),
            event_id: fact.event_id.clone(),
            admission,
            disposition,
            state: self.read_state(),
        })
    }

    async fn request_intervention(
        &self,
        attachment: &SessionAttachmentRef,
        request_id: &InterventionRequestIdRef,
        kind: SessionInterventionKindV1,
        reason: &str,
    ) -> Result<(), SessionFactError> {
        self.unavailable()?;
        self.requests.lock().push((
            attachment.as_str().to_string(),
            request_id.as_str().to_string(),
            kind.as_str().to_string(),
            reason.to_string(),
        ));
        Ok(())
    }

    async fn get_intervention(
        &self,
        attachment: &SessionAttachmentRef,
        request_id: &InterventionRequestIdRef,
    ) -> Result<Option<SessionInterventionRequestView>, SessionFactError> {
        self.unavailable()?;
        Ok(self
            .pending_requests
            .lock()
            .iter()
            .find(|request| {
                request.attachment_ref == *attachment && request.request_id == *request_id
            })
            .cloned())
    }

    async fn record_intervention_result(
        &self,
        _attachment: &SessionAttachmentRef,
        request_id: &InterventionRequestIdRef,
        disposition: SessionInterventionDispositionV1,
        _authority_confirmation_ref: Option<&str>,
        _detail: Option<&str>,
    ) -> Result<(), SessionFactError> {
        self.unavailable()?;
        self.results
            .lock()
            .push((request_id.as_str().to_string(), disposition));
        Ok(())
    }

    async fn mark_connection(
        &self,
        _attachment: &SessionAttachmentRef,
        fact: SessionConnectionFactV1,
    ) -> Result<(), SessionFactError> {
        self.unavailable()?;
        self.connection_facts.lock().push(fact);
        Ok(())
    }

    async fn reconnect(
        &self,
        _binding: &SessionBinding,
    ) -> Result<SessionReconnectReceiptView, SessionFactError> {
        self.unavailable()?;
        *self.reconnections.lock() += 1;
        let attachment = self
            .attachment
            .lock()
            .clone()
            .ok_or_else(|| SessionFactError::Refused("not attached".to_string()))?;
        Ok(SessionReconnectReceiptView {
            attachment_ref: attachment,
            reconnected: true,
            resume_from_revision: *self.canonical_revision.lock(),
            state: self.read_state(),
        })
    }

    async fn get_state(
        &self,
        _attachment: &SessionAttachmentRef,
    ) -> Result<SessionStateView, SessionFactError> {
        self.unavailable()?;
        Ok(self.read_state())
    }
}

impl InMemoryFactSink {
    fn read_state(&self) -> SessionStateView {
        // Mirror the spine's projection: state derives from ingested
        // CREATED (advancing) facts only — stale/replayed entries never
        // move anything (the rank+revision guard lives in ingest_event).
        let facts = self.facts.lock();
        let mut canonical = SessionCanonicalStateV1::Accepted;
        let mut cleanup = false;
        let mut last: Option<String> = None;
        for (fact, admission) in facts.iter() {
            if *admission != SessionReceiptAdmissionV1::Created {
                continue;
            }
            match fact.kind {
                SessionEventKindV1::Accepted => canonical = SessionCanonicalStateV1::Accepted,
                SessionEventKindV1::Started => canonical = SessionCanonicalStateV1::Started,
                SessionEventKindV1::Progress => canonical = SessionCanonicalStateV1::Progressing,
                SessionEventKindV1::InputRequired => {
                    canonical = SessionCanonicalStateV1::InputRequired;
                }
                SessionEventKindV1::Terminal => {
                    if let Some(outcome) = fact.outcome.as_ref() {
                        canonical = match outcome {
                            SessionTerminalOutcomeV1::Completed => {
                                SessionCanonicalStateV1::Completed
                            }
                            SessionTerminalOutcomeV1::Failed => SessionCanonicalStateV1::Failed,
                            SessionTerminalOutcomeV1::Cancelled { .. } => {
                                SessionCanonicalStateV1::Cancelled
                            }
                        };
                    }
                }
                SessionEventKindV1::Cleanup => cleanup = true,
            }
            last = Some(fact.event_id.as_str().to_string());
        }
        SessionStateView {
            canonical_state: canonical,
            canonical_revision: *self.canonical_revision.lock(),
            cleanup_recorded: cleanup,
            conflicting_terminal: false,
            last_event_id: last,
        }
    }
}
