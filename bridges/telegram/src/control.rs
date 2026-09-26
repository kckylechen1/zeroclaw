//! The control socket: proactive messages from the gateway (cron output,
//! heartbeat alerts, the `notify` tool) delivered into Telegram.
//!
//! The gateway sends `deliver` frames from its outbox, oldest first, and
//! keeps each one until the bridge acknowledges it. The bridge acknowledges
//! only after Telegram accepted the message, so a crash or a failed send
//! means the message comes again on the next connection; ids already
//! delivered are remembered and acknowledged without sending twice.

use std::collections::{HashSet, VecDeque};
use std::time::Duration;

use serde_json::json;
use tokio::time::{sleep, timeout};
use zeroclaw_gateway_client::{Backoff, BridgeClient, Deliver, Rejected};
use zeroclaw_log::{Action, Event, EventOutcome, record};

use crate::render::split_message;
use crate::telegram::{Api, MESSAGE_LIMIT, Refused};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const BACKOFF_INITIAL: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Delivered ids remembered for deduplication.
const SEEN_CAPACITY: usize = 512;

/// Ids delivered recently, oldest evicted first.
#[derive(Debug, Default)]
pub struct Seen {
    order: VecDeque<String>,
    ids: HashSet<String>,
}

impl Seen {
    pub fn contains(&self, id: &str) -> bool {
        self.ids.contains(id)
    }

    pub fn insert(&mut self, id: &str) {
        if !self.ids.insert(id.to_string()) {
            return;
        }
        self.order.push_back(id.to_string());
        while self.order.len() > SEEN_CAPACITY {
            if let Some(old) = self.order.pop_front() {
                self.ids.remove(&old);
            }
        }
    }
}

/// What became of one `deliver`.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// Telegram accepted it.
    Sent,
    /// It can never be sent (not the owner's chat, a chat Telegram refuses);
    /// acknowledged so it does not block the queue.
    Dropped(String),
    /// A failure worth retrying: reconnect later and get it again.
    Retry(String),
}

/// Keep the control socket connected and deliver what arrives. Returns
/// only when the gateway refuses the token (for example a paired token
/// instead of a bridge token); the chat relay keeps running without it.
pub async fn run(api: Api, gateway: String, token: String, owner_id: i64) {
    let mut backoff = Backoff::new(BACKOFF_INITIAL, BACKOFF_MAX);
    let mut seen = Seen::default();
    let mut telegram_failed = false;
    loop {
        match timeout(CONNECT_TIMEOUT, BridgeClient::connect(&gateway, &token)).await {
            Ok(Ok(mut client)) => {
                record!(
                    INFO,
                    Event::new(module_path!(), Action::Connect)
                        .with_outcome(EventOutcome::Success)
                        .with_attrs(json!({ "gateway": &gateway, "bridge": client.bridge() })),
                    "control socket attached"
                );
                // After a Telegram failure keep backing off, or a Telegram
                // outage would be retried every second.
                if !telegram_failed {
                    backoff.reset();
                }
                let (reason, failed) = serve(&mut client, &api, owner_id, &mut seen).await;
                telegram_failed = failed;
                log_lost(&gateway, &reason);
            }
            Ok(Err(e)) => {
                if let Some(rejected) = e.downcast_ref::<Rejected>()
                    && crate::bridge::is_permanent(rejected.status)
                {
                    record!(
                        ERROR,
                        Event::new(module_path!(), Action::Connect)
                            .with_outcome(EventOutcome::Failure)
                            .with_attrs(json!({
                                "gateway": &gateway,
                                "status": rejected.status,
                                "reason": &rejected.reason,
                            })),
                        "the gateway refused the control socket; proactive messages \
                         (cron, notify) are off. Use a bridge token from \
                         `zeroclaw gateway bridge add`"
                    );
                    return;
                }
                log_lost(&gateway, &format!("{e:#}"));
            }
            Err(_) => log_lost(&gateway, "timed out"),
        }
        sleep(backoff.next_delay()).await;
    }
}

/// Deliver until the socket closes or Telegram fails. Returns why it ended
/// and whether Telegram was the reason.
async fn serve(
    client: &mut BridgeClient,
    api: &Api,
    owner_id: i64,
    seen: &mut Seen,
) -> (String, bool) {
    loop {
        let deliver = match client.next_deliver().await {
            Ok(Some(deliver)) => deliver,
            Ok(None) => return ("the gateway closed the control socket".into(), false),
            Err(e) => return (format!("{e:#}"), false),
        };
        if !seen.contains(&deliver.id) {
            match deliver_one(api, owner_id, &deliver).await {
                Outcome::Sent => {}
                Outcome::Dropped(reason) => {
                    record!(
                        WARN,
                        Event::new(module_path!(), Action::Skip)
                            .with_outcome(EventOutcome::Failure)
                            .with_attrs(json!({ "id": &deliver.id, "reason": reason })),
                        "dropped a proactive message that cannot be delivered"
                    );
                }
                Outcome::Retry(reason) => {
                    // Not acknowledged: the gateway sends it (and everything
                    // after it) again on the next connection, in order.
                    return (format!("Telegram send failed: {reason}"), true);
                }
            }
            seen.insert(&deliver.id);
        }
        if let Err(e) = client.ack(&deliver.id).await {
            return (format!("{e:#}"), false);
        }
    }
}

async fn deliver_one(api: &Api, owner_id: i64, deliver: &Deliver) -> Outcome {
    let Ok(chat_id) = deliver.to.trim().parse::<i64>() else {
        return Outcome::Dropped(format!("`to` is not a Telegram chat id: {:?}", deliver.to));
    };
    // The bridge serves only the owner's private chat, for inbound and
    // outbound alike.
    if chat_id != owner_id {
        return Outcome::Dropped("`to` is not the owner's chat".into());
    }
    let thread_id = deliver
        .thread_id
        .as_deref()
        .and_then(|thread| thread.trim().parse::<i64>().ok());
    for piece in split_message(&deliver.content, MESSAGE_LIMIT) {
        if let Err(e) = api.send_to_thread(chat_id, thread_id, &piece).await {
            let reason = format!("{e:#}");
            return match e.downcast_ref::<Refused>() {
                Some(refused) if refused.is_permanent() => Outcome::Dropped(reason),
                _ => Outcome::Retry(reason),
            };
        }
    }
    Outcome::Sent
}

fn log_lost(gateway: &str, reason: &str) {
    record!(
        WARN,
        Event::new(module_path!(), Action::Disconnect)
            .with_outcome(EventOutcome::Failure)
            .with_attrs(json!({ "gateway": gateway, "reason": reason })),
        "control socket lost; reconnecting"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seen_ids_are_bounded_and_evict_the_oldest() {
        let mut seen = Seen::default();
        for n in 0..=SEEN_CAPACITY {
            seen.insert(&format!("id{n}"));
        }
        seen.insert("id5");
        assert!(!seen.contains("id0"));
        assert!(seen.contains("id1"));
        assert!(seen.contains(&format!("id{SEEN_CAPACITY}")));
        assert_eq!(seen.order.len(), SEEN_CAPACITY);
    }

    #[test]
    fn only_client_refusals_other_than_rate_limits_are_permanent() {
        let refused = |code| Refused {
            method: "sendMessage".into(),
            code: Some(code),
            description: String::new(),
        };
        assert!(refused(400).is_permanent());
        assert!(refused(403).is_permanent());
        assert!(!refused(429).is_permanent());
        assert!(!refused(502).is_permanent());
    }
}
