//! Shared WebSocket conversations (#376).
//!
//! Every `/ws/chat` socket that opens the same session key attaches to one
//! [`Conversation`]: one agent, one in-memory history, one approval map, one
//! running turn at a time. Sockets are subscribers. A turn runs in its own
//! task and publishes its frames to every subscriber, so:
//!
//! - two sockets on one session see the same ordered history and events;
//! - a message sent while a turn is running steers that turn instead of
//!   starting a second one;
//! - an approval raised during a turn can be answered from any subscriber,
//!   and only the first answer counts;
//! - closing a socket only unsubscribes. The turn keeps running; it stops
//!   only on an explicit cancel (a `cancel` frame or the REST abort route).
//!
//! The hub is generic over the agent state so its bookkeeping is testable
//! without building a real agent.

use crate::ws_approval::{PendingApprovals, new_pending_approvals};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use tokio::sync::{OnceCell, broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use zeroclaw_api::channel::ChannelApprovalResponse;
use zeroclaw_infra::session_backend::RequestReceipt;

/// A serialized JSON frame, shared by every subscriber.
pub(crate) type Frame = Arc<str>;

/// Frames buffered per conversation for slow subscribers. A subscriber that
/// falls further behind skips frames (logged); the `done` frame still carries
/// the full response.
const FRAME_BUFFER: usize = 1024;

/// Steering messages queued for a running turn.
const STEERING_BUFFER: usize = 32;

/// Client request ids a conversation remembers when the session store keeps
/// no receipts. Mirrors the SQLite backend's per-session bound.
const RECENT_REQUESTS: usize = 256;

/// Recently accepted client request ids and their last state, in arrival
/// order. The in-memory stand-in for the session store's receipts: it
/// catches a retry on a live conversation, not one after a restart.
#[derive(Default)]
struct RecentRequests {
    entries: std::collections::VecDeque<(String, String)>,
}

impl RecentRequests {
    fn record(&mut self, request_id: &str, state: &str) -> RequestReceipt {
        if let Some((_, known)) = self.entries.iter().find(|(id, _)| id == request_id) {
            return RequestReceipt::Duplicate {
                state: known.clone(),
            };
        }
        if self.entries.len() == RECENT_REQUESTS {
            self.entries.pop_front();
        }
        self.entries
            .push_back((request_id.to_string(), state.to_string()));
        RequestReceipt::Recorded
    }

    fn set_state(&mut self, request_id: &str, state: &str) {
        if let Some((_, known)) = self.entries.iter_mut().find(|(id, _)| id == request_id) {
            *known = state.to_string();
        }
    }
}

/// Publishes frames to every subscriber of one conversation.
#[derive(Clone)]
pub(crate) struct FrameSink {
    frames: broadcast::Sender<Frame>,
}

impl FrameSink {
    pub(crate) fn publish(&self, frame: &serde_json::Value) {
        let _ = self.frames.send(Arc::from(frame.to_string()));
    }

    /// Whether any socket is currently subscribed.
    pub(crate) fn has_subscribers(&self) -> bool {
        self.frames.receiver_count() > 0
    }
}

/// What the agent factory gets to wire the agent into its conversation.
pub(crate) struct Seed {
    pub(crate) pending_approvals: PendingApprovals,
    pub(crate) frames: FrameSink,
}

/// The turn currently running in a conversation.
struct ActiveTurn {
    generation: u64,
    cancel: CancellationToken,
    steering: mpsc::Sender<String>,
}

/// What happened to a message submitted to a conversation.
pub(crate) enum Submitted {
    /// No turn was running; the caller must run this one.
    Start(TurnClaim),
    /// A turn is running and the message was queued as steering for it.
    Steered,
    /// A turn is running and its steering queue is full.
    SteeringFull,
    /// A turn is running but no longer accepts steering.
    SteeringClosed,
}

/// The right to run one turn. Created only by [`Conversation::submit`].
pub(crate) struct TurnClaim {
    /// The message that starts the turn.
    pub(crate) input: String,
    /// The client's id for that message, if it sent one.
    pub(crate) request_id: Option<String>,
    pub(crate) generation: u64,
    pub(crate) cancel: CancellationToken,
    pub(crate) steering: mpsc::Receiver<String>,
}

pub(crate) struct Conversation<A> {
    key: String,
    /// The agent state. A turn holds this lock for its whole duration.
    pub(crate) agent: tokio::sync::Mutex<A>,
    frames: FrameSink,
    pub(crate) pending_approvals: PendingApprovals,
    turn: parking_lot::Mutex<Option<ActiveTurn>>,
    next_generation: AtomicU64,
    subscribers: AtomicUsize,
    requests: parking_lot::Mutex<RecentRequests>,
}

impl<A> Conversation<A> {
    fn new(key: String, agent: A, seed: Seed) -> Self {
        let Seed {
            pending_approvals,
            frames,
        } = seed;
        Self {
            key,
            agent: tokio::sync::Mutex::new(agent),
            frames,
            pending_approvals,
            turn: parking_lot::Mutex::new(None),
            next_generation: AtomicU64::new(1),
            subscribers: AtomicUsize::new(0),
            requests: parking_lot::Mutex::default(),
        }
    }

    pub(crate) fn key(&self) -> &str {
        &self.key
    }

    /// Send a frame to every current subscriber.
    pub(crate) fn publish(&self, frame: &serde_json::Value) {
        self.frames.publish(frame);
    }

    /// Start a turn if none is running, otherwise steer the running one.
    /// On [`Submitted::Start`] the claim carries `content` back as the turn's
    /// input; otherwise it went to the running turn or was dropped.
    pub(crate) fn submit(&self, content: String) -> Submitted {
        let mut turn = self.turn.lock();
        if let Some(active) = turn.as_ref() {
            return match active.steering.try_send(content) {
                Ok(()) => Submitted::Steered,
                Err(mpsc::error::TrySendError::Full(_)) => Submitted::SteeringFull,
                Err(mpsc::error::TrySendError::Closed(_)) => Submitted::SteeringClosed,
            };
        }
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        let cancel = CancellationToken::new();
        let (steering_tx, steering_rx) = mpsc::channel(STEERING_BUFFER);
        *turn = Some(ActiveTurn {
            generation,
            cancel: cancel.clone(),
            steering: steering_tx,
        });
        Submitted::Start(TurnClaim {
            input: content,
            request_id: None,
            generation,
            cancel,
            steering: steering_rx,
        })
    }

    /// Release the turn slot, but only for the turn that holds it.
    pub(crate) fn finish_turn(&self, generation: u64) {
        let mut turn = self.turn.lock();
        if turn.as_ref().is_some_and(|t| t.generation == generation) {
            *turn = None;
        }
    }

    /// Cancel the running turn. Returns `false` when none is running.
    pub(crate) fn cancel_current(&self) -> bool {
        match self.turn.lock().as_ref() {
            Some(active) => {
                active.cancel.cancel();
                true
            }
            None => false,
        }
    }

    pub(crate) fn is_running(&self) -> bool {
        self.turn.lock().is_some()
    }

    /// Deliver an operator decision. Only the first answer for a request
    /// counts; later or unknown ones return `false`.
    pub(crate) fn resolve_approval(
        &self,
        request_id: &str,
        decision: ChannelApprovalResponse,
    ) -> bool {
        match self.pending_approvals.lock().remove(request_id) {
            Some(tx) => tx.send(decision).is_ok(),
            None => false,
        }
    }

    /// Drop every pending approval so a waiting turn resolves as unreachable
    /// instead of waiting for its timeout.
    pub(crate) fn drain_approvals(&self) {
        let drained: Vec<_> = self.pending_approvals.lock().drain().collect();
        drop(drained);
    }

    /// Remember a client request id in memory. For sessions whose store
    /// keeps no receipts; see [`RecentRequests`].
    pub(crate) fn record_request(&self, request_id: &str, state: &str) -> RequestReceipt {
        self.requests.lock().record(request_id, state)
    }

    pub(crate) fn set_request_state(&self, request_id: &str, state: &str) {
        self.requests.lock().set_state(request_id, state);
    }

    pub(crate) fn subscriber_count(&self) -> usize {
        self.subscribers.load(Ordering::Acquire)
    }
}

type Slot<A> = Arc<OnceCell<Arc<Conversation<A>>>>;

/// Live conversations keyed by session key.
pub struct ConversationHub<A> {
    slots: parking_lot::Mutex<HashMap<String, Slot<A>>>,
}

impl<A> Default for ConversationHub<A> {
    fn default() -> Self {
        Self {
            slots: parking_lot::Mutex::new(HashMap::new()),
        }
    }
}

impl<A> ConversationHub<A> {
    /// Attach to the conversation for `key`, building it with `create` if no
    /// socket holds it. `create` returns the agent state plus a value for
    /// the caller; the caller gets that value back only when this call built
    /// the conversation. A failed `create` leaves no entry behind.
    pub(crate) async fn attach<F, Fut, X, E>(
        self: &Arc<Self>,
        key: &str,
        mut create: F,
    ) -> Result<(Subscription<A>, Option<X>), E>
    where
        F: FnMut(Seed) -> Fut,
        Fut: std::future::Future<Output = Result<(A, X), E>>,
    {
        loop {
            let slot = Arc::clone(
                self.slots
                    .lock()
                    .entry(key.to_string())
                    .or_insert_with(|| Arc::new(OnceCell::new())),
            );
            let mut built = None;
            let result = slot
                .get_or_try_init(|| async {
                    let pending_approvals = new_pending_approvals();
                    let (frames, _) = broadcast::channel(FRAME_BUFFER);
                    let frames = FrameSink { frames };
                    let (agent, extra) = create(Seed {
                        pending_approvals: pending_approvals.clone(),
                        frames: frames.clone(),
                    })
                    .await?;
                    built = Some(extra);
                    Ok(Arc::new(Conversation::new(
                        key.to_string(),
                        agent,
                        Seed {
                            pending_approvals,
                            frames,
                        },
                    )))
                })
                .await;
            let conversation = match result {
                Ok(conversation) => Arc::clone(conversation),
                Err(err) => {
                    self.remove_empty_slot(key, &slot);
                    return Err(err);
                }
            };

            // Count the subscriber under the map lock, and only while the map
            // still points at this conversation: an idle conversation can be
            // released between the build above and here. A vacant key takes
            // it back; a newer conversation under the key wins, and this
            // attach joins that one instead.
            let mut slots = self.slots.lock();
            match slots.get(key) {
                Some(held) if Arc::ptr_eq(held, &slot) => {}
                Some(_) => continue,
                None => {
                    slots.insert(key.to_string(), Arc::clone(&slot));
                }
            }
            conversation.subscribers.fetch_add(1, Ordering::AcqRel);
            let frames = conversation.frames.frames.subscribe();
            drop(slots);
            return Ok((
                Subscription {
                    hub: Arc::clone(self),
                    conversation,
                    frames,
                },
                built,
            ));
        }
    }

    /// Forget `conversation` once no socket is attached and no turn runs.
    pub(crate) fn release_if_unused(&self, conversation: &Arc<Conversation<A>>) {
        let mut slots = self.slots.lock();
        if conversation.subscriber_count() > 0 || conversation.is_running() {
            return;
        }
        let same = slots
            .get(conversation.key())
            .and_then(|slot| slot.get())
            .is_some_and(|held| Arc::ptr_eq(held, conversation));
        if same {
            slots.remove(conversation.key());
        }
    }

    /// Drop a slot whose build failed. An attach still waiting on it builds
    /// in it and puts it back (see [`Self::attach`]).
    fn remove_empty_slot(&self, key: &str, slot: &Slot<A>) {
        let mut slots = self.slots.lock();
        if slot.get().is_none() && slots.get(key).is_some_and(|held| Arc::ptr_eq(held, slot)) {
            slots.remove(key);
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.slots.lock().len()
    }
}

/// One socket's attachment to a conversation. Dropping it unsubscribes.
pub(crate) struct Subscription<A> {
    hub: Arc<ConversationHub<A>>,
    pub(crate) conversation: Arc<Conversation<A>>,
    pub(crate) frames: broadcast::Receiver<Frame>,
}

impl<A> Drop for Subscription<A> {
    fn drop(&mut self) {
        let remaining = self.conversation.subscribers.fetch_sub(1, Ordering::AcqRel) - 1;
        if remaining == 0 && self.conversation.is_running() {
            // Nobody is left to answer: a pending approval would otherwise
            // hold the turn until its timeout. The turn itself keeps going.
            self.conversation.drain_approvals();
        }
        self.hub.release_if_unused(&self.conversation);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn attach(hub: &Arc<ConversationHub<()>>, key: &str) -> (Subscription<()>, bool) {
        let (sub, built) = hub
            .attach(key, |_| async { Ok::<_, ()>(((), ())) })
            .await
            .unwrap();
        (sub, built.is_some())
    }

    #[tokio::test]
    async fn sockets_on_one_key_share_a_conversation_and_its_frames() {
        let hub = Arc::new(ConversationHub::<()>::default());
        let (mut a, created_a) = attach(&hub, "gw_s1").await;
        let (mut b, created_b) = attach(&hub, "gw_s1").await;
        let (other, _) = attach(&hub, "gw_s2").await;
        assert!(created_a && !created_b);
        assert!(Arc::ptr_eq(&a.conversation, &b.conversation));
        assert!(!Arc::ptr_eq(&a.conversation, &other.conversation));

        a.conversation
            .publish(&serde_json::json!({"type": "chunk", "content": "hi"}));
        for sub in [&mut a, &mut b] {
            let frame = sub.frames.recv().await.unwrap();
            assert_eq!(&*frame, r#"{"content":"hi","type":"chunk"}"#);
        }
    }

    #[tokio::test]
    async fn a_message_during_a_turn_steers_it_instead_of_starting_another() {
        let hub = Arc::new(ConversationHub::<()>::default());
        let (a, _) = attach(&hub, "gw_s1").await;
        let conv = &a.conversation;
        let Submitted::Start(mut claim) = conv.submit("first".into()) else {
            panic!("idle conversation must start a turn")
        };
        assert!(matches!(conv.submit("more".into()), Submitted::Steered));
        assert_eq!(claim.steering.recv().await.as_deref(), Some("more"));

        conv.finish_turn(claim.generation);
        let Submitted::Start(next) = conv.submit("second".into()) else {
            panic!("finished turn must free the slot")
        };
        assert!(next.generation > claim.generation);
    }

    #[tokio::test]
    async fn an_old_generation_cannot_release_or_cancel_a_newer_turn() {
        let hub = Arc::new(ConversationHub::<()>::default());
        let (a, _) = attach(&hub, "gw_s1").await;
        let conv = &a.conversation;
        let Submitted::Start(old) = conv.submit("one".into()) else {
            panic!()
        };
        conv.finish_turn(old.generation);
        let Submitted::Start(new) = conv.submit("two".into()) else {
            panic!()
        };
        conv.finish_turn(old.generation);
        assert!(conv.is_running(), "stale finish must not free the new turn");
        assert!(!old.cancel.is_cancelled());
        assert!(conv.cancel_current());
        assert!(new.cancel.is_cancelled());
        assert!(!old.cancel.is_cancelled());
        conv.finish_turn(new.generation);
        assert!(!conv.cancel_current());
    }

    #[tokio::test]
    async fn an_approval_resolves_once_from_any_subscriber() {
        let hub = Arc::new(ConversationHub::<()>::default());
        let (a, _) = attach(&hub, "gw_s1").await;
        let (b, _) = attach(&hub, "gw_s1").await;
        let (tx, rx) = tokio::sync::oneshot::channel();
        a.conversation
            .pending_approvals
            .lock()
            .insert("req-1".into(), tx);
        assert!(
            b.conversation
                .resolve_approval("req-1", ChannelApprovalResponse::Approve)
        );
        assert!(
            !a.conversation
                .resolve_approval("req-1", ChannelApprovalResponse::Deny)
        );
        assert!(
            !a.conversation
                .resolve_approval("unknown", ChannelApprovalResponse::Deny)
        );
        assert!(matches!(rx.await, Ok(ChannelApprovalResponse::Approve)));
    }

    #[tokio::test]
    async fn closing_the_last_socket_keeps_the_turn_but_drops_its_approvals() {
        let hub = Arc::new(ConversationHub::<()>::default());
        let (a, _) = attach(&hub, "gw_s1").await;
        let conv = Arc::clone(&a.conversation);
        let Submitted::Start(claim) = conv.submit("go".into()) else {
            panic!()
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        conv.pending_approvals.lock().insert("req-1".into(), tx);

        drop(a);
        assert!(!claim.cancel.is_cancelled(), "detach must not cancel");
        assert!(rx.await.is_err(), "unanswerable approval is dropped");
        assert_eq!(hub.len(), 1, "a running turn keeps its conversation");

        // A socket reconnecting mid-turn finds the same conversation.
        let (again, created) = attach(&hub, "gw_s1").await;
        assert!(!created && Arc::ptr_eq(&again.conversation, &conv));
        drop(again);

        conv.finish_turn(claim.generation);
        hub.release_if_unused(&conv);
        assert_eq!(hub.len(), 0);
    }

    #[tokio::test]
    async fn idle_conversation_is_released_when_its_last_socket_closes() {
        let hub = Arc::new(ConversationHub::<()>::default());
        let (a, _) = attach(&hub, "gw_s1").await;
        let (b, _) = attach(&hub, "gw_s1").await;
        drop(a);
        assert_eq!(hub.len(), 1);
        drop(b);
        assert_eq!(hub.len(), 0);
    }

    #[tokio::test]
    async fn failed_creation_leaves_no_entry_and_can_be_retried() {
        let hub = Arc::new(ConversationHub::<()>::default());
        let err = hub
            .attach("gw_s1", |_| async { Err::<((), ()), _>("boom") })
            .await
            .err();
        assert_eq!(err, Some("boom"));
        assert_eq!(hub.len(), 0);
        let (_a, created) = attach(&hub, "gw_s1").await;
        assert!(created);
    }

    #[tokio::test]
    async fn a_failed_build_keeps_the_slot_for_an_attach_waiting_on_it() {
        let hub = Arc::new(ConversationHub::<()>::default());
        let (fail_tx, fail_rx) = tokio::sync::oneshot::channel::<()>();
        let mut fail_rx = Some(fail_rx);
        let failing = hub.attach("gw_s1", |_| {
            let rx = fail_rx.take();
            async move {
                if let Some(rx) = rx {
                    let _ = rx.await;
                }
                Err::<((), ()), _>("boom")
            }
        });
        let waiting = async {
            tokio::task::yield_now().await;
            let _ = fail_tx.send(());
            hub.attach("gw_s1", |_| async { Ok::<_, &str>(((), ())) })
                .await
        };
        let (failed, joined) = tokio::join!(failing, waiting);
        assert!(failed.is_err());
        let (b, built) = joined.unwrap();
        assert!(built.is_some());

        // The waiting attach's conversation is the one the key maps to.
        let (c, created) = attach(&hub, "gw_s1").await;
        assert!(!created && Arc::ptr_eq(&b.conversation, &c.conversation));
        assert_eq!(hub.len(), 1);
    }

    #[tokio::test]
    async fn remembered_requests_report_duplicates_and_stay_bounded() {
        let hub = Arc::new(ConversationHub::<()>::default());
        let (a, _) = attach(&hub, "gw_s1").await;
        let conv = &a.conversation;
        assert_eq!(
            conv.record_request("r1", "accepted"),
            RequestReceipt::Recorded
        );
        conv.set_request_state("r1", "done");
        assert_eq!(
            conv.record_request("r1", "accepted"),
            RequestReceipt::Duplicate {
                state: "done".into()
            }
        );
        for i in 2..=RECENT_REQUESTS + 1 {
            conv.record_request(&format!("r{i}"), "accepted");
        }
        assert_eq!(
            conv.record_request("r1", "accepted"),
            RequestReceipt::Recorded
        );
    }
}
