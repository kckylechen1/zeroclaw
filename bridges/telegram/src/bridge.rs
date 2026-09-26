//! The bridge loop: Telegram updates in, gateway frames out, one chat
//! socket kept open for the owner's session. Proactive messages come in on
//! a separate control socket (see `control`).

use std::collections::VecDeque;
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::json;
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep, sleep_until, timeout};
use zeroclaw_gateway_client::{
    Backoff, Client, ConnectOptions, Decision, Frame, Rejected, new_request_id,
};
use zeroclaw_log::{Action, Event, EventOutcome, record};

use crate::render::{
    ApprovalKeys, EDIT_INTERVAL, decode_callback, edit_due, encode_callback, split_message,
};
use crate::telegram::{Api, Inbound, MESSAGE_LIMIT, Update, classify};

/// Telegram shows "typing" for about five seconds per action.
const TYPING_INTERVAL: Duration = Duration::from_secs(4);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const BACKOFF_INITIAL: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Replies the bridge writes itself. English only: the bridge carries no
/// Fluent catalogue (it does not link the runtime).
mod text {
    pub const STEERED: &str = "(added to the current turn)";
    pub const ABORTED: &str = "(cancelled)";
    pub const OFFLINE_QUEUED: &str =
        "(the gateway is unreachable; the message will be sent when it is back)";
    pub const OFFLINE: &str = "(the gateway is unreachable)";
    pub const EXPIRED: &str = "This request is no longer known";
    pub const APPROVE: &str = "Approve";
    pub const ALWAYS: &str = "Always";
    pub const DENY: &str = "Deny";
    pub const APPROVED: &str = "Approved";
    pub const ALWAYS_APPROVED: &str = "Approved (always)";
    pub const DENIED: &str = "Denied";

    pub fn error(message: &str) -> String {
        format!("Error: {message}")
    }

    pub fn approval(tool: &str, summary: &str) -> String {
        if summary.trim().is_empty() {
            format!("Allow {tool}?")
        } else {
            format!("Allow {tool}?\n\n{summary}")
        }
    }
}

/// Everything the bridge needs to run.
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    /// Bot API base URL, `https://api.telegram.org` in production.
    pub telegram_api: String,
    pub telegram_token: String,
    /// The one Telegram user served. In a private chat the chat id is the
    /// user id, so replies go to this id too.
    pub owner_id: i64,
    /// The gateway, agent, session and token to attach with.
    pub gateway: ConnectOptions,
    /// How long one `getUpdates` long poll waits.
    pub poll_wait: Duration,
}

/// Run the bridge until the Telegram poller stops or the gateway refuses
/// the bridge outright (bad token, unknown agent).
pub async fn run(config: BridgeConfig) -> Result<()> {
    let api = Api::new(&config.telegram_api, &config.telegram_token)?;
    let (tx, mut updates) = mpsc::channel(64);
    let poll_api = api.clone();
    let poll_wait = config.poll_wait;
    let poller = zeroclaw_spawn::spawn!(poll_updates(poll_api, poll_wait, tx));
    // Proactive messages arrive on the control socket, which needs the
    // bridge token; without a token only the chat relay runs.
    let control = match config.gateway.token.clone() {
        Some(token) => {
            let control_api = api.clone();
            let gateway = config.gateway.gateway.clone();
            let owner_id = config.owner_id;
            Some(zeroclaw_spawn::spawn!(crate::control::run(
                control_api,
                gateway,
                token,
                owner_id,
            )))
        }
        None => {
            record!(
                INFO,
                Event::new(module_path!(), Action::Skip),
                "no gateway token; proactive messages (cron, notify) are off"
            );
            None
        }
    };
    let mut bridge = Bridge {
        api,
        owner_id: config.owner_id,
        options: config.gateway,
        client: None,
        backoff: Backoff::new(BACKOFF_INITIAL, BACKOFF_MAX),
        reconnect_at: Instant::now(),
        unacked: VecDeque::new(),
        stream: None,
        typing_at: None,
        keys: ApprovalKeys::default(),
    };
    let result = bridge.run(&mut updates).await;
    poller.abort();
    if let Some(control) = control {
        control.abort();
    }
    result
}

/// Long-poll Telegram and hand updates to the bridge loop.
async fn poll_updates(api: Api, wait: Duration, tx: mpsc::Sender<Update>) {
    let mut offset = 0;
    let mut backoff = Backoff::new(BACKOFF_INITIAL, BACKOFF_MAX);
    while !tx.is_closed() {
        match api.get_updates(offset, wait).await {
            Ok(updates) => {
                backoff.reset();
                for update in updates {
                    offset = offset.max(update.update_id + 1);
                    if tx.send(update).await.is_err() {
                        return;
                    }
                }
            }
            Err(e) => {
                let delay = backoff.next_delay();
                record!(
                    WARN,
                    Event::new(module_path!(), Action::Receive)
                        .with_outcome(EventOutcome::Failure)
                        .with_attrs(json!({
                            "error": format!("{e:#}"),
                            "retry_in_ms": delay.as_millis() as u64,
                        })),
                    "Telegram getUpdates failed"
                );
                sleep(delay).await;
            }
        }
    }
}

/// The reply being streamed into Telegram messages.
#[derive(Debug, Default)]
struct Stream {
    text: String,
    /// Messages sent so far and the text each one shows.
    sent: Vec<(i64, String)>,
    last_edit: Option<Instant>,
    dirty: bool,
}

impl Stream {
    fn flush_at(&self) -> Option<Instant> {
        match self.last_edit {
            Some(last) if self.dirty => Some(last + EDIT_INTERVAL),
            _ => None,
        }
    }
}

struct Bridge {
    api: Api,
    owner_id: i64,
    options: ConnectOptions,
    client: Option<Client>,
    backoff: Backoff,
    reconnect_at: Instant,
    /// Messages sent (or waiting to be sent) without an `ack` yet, with
    /// their request ids. After a reconnect they go again under the same
    /// id, so the gateway runs each at most once.
    unacked: VecDeque<(String, String)>,
    stream: Option<Stream>,
    /// When to next show "typing"; set while a turn runs.
    typing_at: Option<Instant>,
    keys: ApprovalKeys,
}

async fn next_frame(client: &mut Option<Client>) -> Result<Option<Frame>> {
    match client {
        Some(client) => client.next_frame().await,
        None => std::future::pending().await,
    }
}

async fn sleep_until_some(at: Option<Instant>) {
    match at {
        Some(at) => sleep_until(at).await,
        None => std::future::pending().await,
    }
}

impl Bridge {
    async fn run(&mut self, updates: &mut mpsc::Receiver<Update>) -> Result<()> {
        loop {
            if self.client.is_none() && Instant::now() >= self.reconnect_at {
                self.connect().await?;
            }
            let flush_at = self.stream.as_ref().and_then(Stream::flush_at);
            let typing_at = self.typing_at;
            let reconnect_at = self.client.is_none().then_some(self.reconnect_at);
            tokio::select! {
                update = updates.recv() => {
                    let Some(update) = update else {
                        bail!("the Telegram poller stopped");
                    };
                    self.on_update(update).await;
                }
                frame = next_frame(&mut self.client) => match frame {
                    Ok(Some(frame)) => self.on_frame(frame).await,
                    Ok(None) => self.disconnected("the gateway closed the connection".into()),
                    Err(e) => self.disconnected(format!("{e:#}")),
                },
                () = sleep_until_some(flush_at) => self.flush().await,
                () = sleep_until_some(typing_at) => self.show_typing().await,
                () = sleep_until_some(reconnect_at) => {}
            }
        }
    }

    /// Attach to the session. Only a refusal the bridge cannot fix by
    /// waiting (bad token, unknown agent) is returned as an error.
    async fn connect(&mut self) -> Result<()> {
        let error = match timeout(CONNECT_TIMEOUT, Client::connect(&self.options)).await {
            Ok(Ok(client)) => {
                record!(
                    INFO,
                    Event::new(module_path!(), Action::Connect)
                        .with_outcome(EventOutcome::Success)
                        .with_attrs(json!({
                            "gateway": &self.options.gateway,
                            "agent": &self.options.agent,
                            "session": &client.session().session_id,
                            "resumed": client.session().resumed,
                            "unacked": self.unacked.len(),
                        })),
                    "attached to the gateway"
                );
                self.backoff.reset();
                self.client = Some(client);
                self.resend_unacked().await;
                return Ok(());
            }
            Ok(Err(e)) => e,
            Err(_) => anyhow::Error::msg("timed out"),
        };
        if let Some(rejected) = error.downcast_ref::<Rejected>()
            && is_permanent(rejected.status)
        {
            record!(
                ERROR,
                Event::new(module_path!(), Action::Connect)
                    .with_outcome(EventOutcome::Failure)
                    .with_attrs(json!({
                        "gateway": &self.options.gateway,
                        "status": rejected.status,
                        "reason": &rejected.reason,
                    })),
                "the gateway refused the bridge"
            );
            return Err(error);
        }
        self.disconnected(format!("{error:#}"));
        Ok(())
    }

    async fn resend_unacked(&mut self) {
        let pending: Vec<_> = self.unacked.iter().cloned().collect();
        for (id, content) in pending {
            let Some(client) = &mut self.client else {
                return;
            };
            if let Err(e) = client.send_message_with_id(&id, &content).await {
                self.disconnected(format!("{e:#}"));
                return;
            }
            self.typing_at.get_or_insert_with(Instant::now);
        }
    }

    fn disconnected(&mut self, reason: String) {
        let delay = self.backoff.next_delay();
        record!(
            WARN,
            Event::new(module_path!(), Action::Disconnect)
                .with_outcome(EventOutcome::Failure)
                .with_attrs(json!({
                    "gateway": &self.options.gateway,
                    "reason": reason,
                    "retry_in_ms": delay.as_millis() as u64,
                })),
            "gateway connection lost; reconnecting"
        );
        self.client = None;
        self.reconnect_at = Instant::now() + delay;
        // A turn in flight keeps running on the gateway; its frames are
        // not replayed, so the partial reply stays as last shown.
        self.stream = None;
        self.typing_at = None;
    }

    async fn on_update(&mut self, update: Update) {
        match classify(&update, self.owner_id) {
            Inbound::Ignored(reason) => {
                record!(
                    DEBUG,
                    Event::new(module_path!(), Action::Skip).with_attrs(json!({
                        "update_id": update.update_id,
                        "reason": reason,
                    })),
                    "ignored a Telegram update"
                );
            }
            Inbound::Text { text, .. } => self.on_text(text).await,
            Inbound::Callback {
                id,
                chat_id,
                message_id,
                message_text,
                data,
            } => {
                self.on_callback(&id, chat_id, message_id, &message_text, &data)
                    .await;
            }
        }
    }

    async fn on_text(&mut self, text: String) {
        match text.trim() {
            "/start" => return,
            "/cancel" => {
                let Some(client) = &mut self.client else {
                    self.reply(text::OFFLINE).await;
                    return;
                };
                if let Err(e) = client.cancel().await {
                    self.disconnected(format!("{e:#}"));
                }
                return;
            }
            _ => {}
        }
        let id = new_request_id();
        self.unacked.push_back((id.clone(), text.clone()));
        let Some(client) = &mut self.client else {
            self.reply(text::OFFLINE_QUEUED).await;
            return;
        };
        match client.send_message_with_id(&id, &text).await {
            Ok(()) => {
                self.typing_at.get_or_insert_with(Instant::now);
            }
            Err(e) => self.disconnected(format!("{e:#}")),
        }
    }

    async fn on_callback(
        &mut self,
        callback_id: &str,
        chat_id: i64,
        message_id: i64,
        message_text: &str,
        data: &str,
    ) {
        let Some((key, decision)) = decode_callback(data) else {
            self.answer_callback(callback_id, "").await;
            return;
        };
        let Some(request_id) = self.keys.resolve(key) else {
            self.answer_callback(callback_id, text::EXPIRED).await;
            return;
        };
        let Some(client) = &mut self.client else {
            self.answer_callback(callback_id, text::OFFLINE).await;
            return;
        };
        if let Err(e) = client.answer_approval(&request_id, decision).await {
            self.disconnected(format!("{e:#}"));
            self.answer_callback(callback_id, text::OFFLINE).await;
            return;
        }
        record!(
            INFO,
            Event::new(module_path!(), Action::Approve).with_attrs(json!({
                "request_id": &request_id,
                "decision": format!("{decision:?}"),
            })),
            "answered an approval from Telegram"
        );
        let label = match decision {
            Decision::Approve => text::APPROVED,
            Decision::Always => text::ALWAYS_APPROVED,
            Decision::Deny => text::DENIED,
        };
        self.answer_callback(callback_id, label).await;
        let shown = format!("{message_text}\n\n{label}");
        if let Err(e) = self
            .api
            .edit_message_text(chat_id, message_id, &shown)
            .await
        {
            log_send_failure("editMessageText", &e);
        }
    }

    async fn on_frame(&mut self, frame: Frame) {
        match frame {
            Frame::Ack {
                id, status, turn, ..
            } => {
                self.unacked.retain(|(pending, _)| *pending != id);
                if status == "duplicate" {
                    record!(
                        DEBUG,
                        Event::new(module_path!(), Action::Skip)
                            .with_attrs(json!({ "request_id": id })),
                        "the gateway already had this message"
                    );
                } else if turn.as_deref() == Some("steered") {
                    self.reply(text::STEERED).await;
                }
            }
            Frame::Chunk { content } => {
                // Chunks from a turn another client started land here too:
                // the session is shared, so the owner sees it.
                self.typing_at.get_or_insert_with(Instant::now);
                let stream = self.stream.get_or_insert_with(Stream::default);
                stream.text.push_str(&content);
                if stream.sent.is_empty()
                    || edit_due(
                        stream.last_edit.map(Instant::into_std),
                        std::time::Instant::now(),
                    )
                {
                    self.flush().await;
                } else {
                    stream.dirty = true;
                }
            }
            Frame::Thinking { .. } | Frame::ToolCall { .. } | Frame::ToolResult { .. } => {
                self.typing_at.get_or_insert_with(Instant::now);
            }
            Frame::ApprovalRequest {
                request_id,
                tool,
                arguments_summary,
                ..
            } => {
                self.ask_approval(&request_id, &tool, &arguments_summary)
                    .await
            }
            Frame::Done { full_response, .. } => {
                if !full_response.trim().is_empty() {
                    self.stream.get_or_insert_with(Stream::default).text = full_response;
                }
                self.end_turn().await;
            }
            Frame::Aborted { .. } => {
                self.end_turn().await;
                self.reply(text::ABORTED).await;
            }
            Frame::Error { message, .. } => {
                self.end_turn().await;
                self.reply(&text::error(&message)).await;
            }
            Frame::Other(_) => {}
        }
    }

    async fn end_turn(&mut self) {
        self.flush().await;
        self.stream = None;
        self.typing_at = None;
    }

    /// Bring Telegram up to date with the streamed text: edit the messages
    /// whose piece changed and send new ones past Telegram's size limit.
    async fn flush(&mut self) {
        let Some(stream) = &mut self.stream else {
            return;
        };
        stream.last_edit = Some(Instant::now());
        stream.dirty = false;
        for (index, piece) in split_message(&stream.text, MESSAGE_LIMIT)
            .into_iter()
            .enumerate()
        {
            match stream.sent.get_mut(index) {
                Some((_, shown)) if *shown == piece => {}
                Some((message_id, shown)) => {
                    match self
                        .api
                        .edit_message_text(self.owner_id, *message_id, &piece)
                        .await
                    {
                        Ok(()) => *shown = piece,
                        Err(e) => log_send_failure("editMessageText", &e),
                    }
                }
                None => match self.api.send_message(self.owner_id, &piece, None).await {
                    Ok(message_id) => stream.sent.push((message_id, piece)),
                    Err(e) => {
                        log_send_failure("sendMessage", &e);
                        return;
                    }
                },
            }
        }
    }

    async fn show_typing(&mut self) {
        self.typing_at = Some(Instant::now() + TYPING_INTERVAL);
        if let Err(e) = self.api.send_typing(self.owner_id).await {
            log_send_failure("sendChatAction", &e);
        }
    }

    async fn ask_approval(&mut self, request_id: &str, tool: &str, summary: &str) {
        let key = self.keys.key_for(request_id);
        let buttons: Vec<_> = [
            (Decision::Approve, text::APPROVE),
            (Decision::Always, text::ALWAYS),
            (Decision::Deny, text::DENY),
        ]
        .into_iter()
        .filter_map(|(decision, label)| {
            let data = encode_callback(&key, decision)?;
            Some(json!({ "text": label, "callback_data": data }))
        })
        .collect();
        let keyboard = json!({ "inline_keyboard": [buttons] });
        if let Err(e) = self
            .api
            .send_message(
                self.owner_id,
                &text::approval(tool, summary),
                Some(keyboard),
            )
            .await
        {
            log_send_failure("sendMessage", &e);
        }
    }

    async fn reply(&self, text: &str) {
        if let Err(e) = self.api.send_message(self.owner_id, text, None).await {
            log_send_failure("sendMessage", &e);
        }
    }

    async fn answer_callback(&self, id: &str, text: &str) {
        if let Err(e) = self.api.answer_callback_query(id, text).await {
            log_send_failure("answerCallbackQuery", &e);
        }
    }
}

/// A refusal that retrying will not fix. Timeouts and rate limits pass.
pub(crate) fn is_permanent(status: u16) -> bool {
    (400..500).contains(&status) && !matches!(status, 408 | 429)
}

fn log_send_failure(method: &str, error: &anyhow::Error) {
    record!(
        WARN,
        Event::new(module_path!(), Action::Send)
            .with_outcome(EventOutcome::Failure)
            .with_attrs(json!({ "method": method, "error": format!("{error:#}") })),
        "Telegram call failed"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_client_errors_stop_the_bridge() {
        assert!(is_permanent(401));
        assert!(is_permanent(400));
        assert!(!is_permanent(429));
        assert!(!is_permanent(408));
        assert!(!is_permanent(502));
    }

    #[test]
    fn a_stream_flushes_a_second_after_its_last_edit_when_dirty() {
        let now = Instant::now();
        let mut stream = Stream {
            last_edit: Some(now),
            ..Stream::default()
        };
        assert_eq!(stream.flush_at(), None);
        stream.dirty = true;
        assert_eq!(stream.flush_at(), Some(now + EDIT_INTERVAL));
    }
}
