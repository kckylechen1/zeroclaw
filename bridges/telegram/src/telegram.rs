//! The slice of the Telegram Bot API the bridge uses, over plain HTTPS.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

/// Telegram's limit on one message's text.
pub const MESSAGE_LIMIT: usize = 4096;

#[derive(Debug, Clone, Deserialize)]
pub struct Update {
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<Message>,
    #[serde(default)]
    pub callback_query: Option<CallbackQuery>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub message_id: i64,
    #[serde(default)]
    pub from: Option<User>,
    pub chat: Chat,
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub id: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Chat {
    pub id: i64,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CallbackQuery {
    pub id: String,
    pub from: User,
    #[serde(default)]
    pub message: Option<Message>,
    #[serde(default)]
    pub data: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Reply<T> {
    ok: bool,
    #[serde(default = "Option::default")]
    result: Option<T>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Sent {
    message_id: i64,
}

/// A Bot API client for one bot token.
#[derive(Clone)]
pub struct Api {
    http: reqwest::Client,
    /// `<api>/bot<token>`; never logged.
    base: String,
}

impl Api {
    pub fn new(api_url: &str, token: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .context("building the Telegram HTTP client")?;
        Ok(Self {
            http,
            base: format!("{}/bot{token}", api_url.trim_end_matches('/')),
        })
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        body: Value,
        timeout: Duration,
    ) -> Result<T> {
        // `without_url` keeps the token, which is part of the URL, out of
        // error messages.
        let response = self
            .http
            .post(format!("{}/{method}", self.base))
            .timeout(timeout)
            .json(&body)
            .send()
            .await
            .map_err(reqwest::Error::without_url)
            .with_context(|| format!("Telegram {method} failed"))?;
        let reply: Reply<T> = response
            .json()
            .await
            .map_err(reqwest::Error::without_url)
            .with_context(|| format!("Telegram {method} returned an unreadable reply"))?;
        match reply {
            Reply {
                ok: true,
                result: Some(result),
                ..
            } => Ok(result),
            Reply { description, .. } => bail!(
                "Telegram {method} refused: {}",
                description.as_deref().unwrap_or("no description")
            ),
        }
    }

    /// Long-poll for updates after `offset`, waiting up to `wait`.
    pub async fn get_updates(&self, offset: i64, wait: Duration) -> Result<Vec<Update>> {
        let body = json!({
            "offset": offset,
            "timeout": wait.as_secs(),
            "allowed_updates": ["message", "callback_query"],
        });
        self.call("getUpdates", body, wait + Duration::from_secs(10))
            .await
    }

    /// Send plain text (no parse mode, so model output needs no escaping)
    /// and return the new message's id.
    pub async fn send_message(
        &self,
        chat_id: i64,
        text: &str,
        keyboard: Option<Value>,
    ) -> Result<i64> {
        let mut body = json!({ "chat_id": chat_id, "text": text });
        if let Some(keyboard) = keyboard {
            body["reply_markup"] = keyboard;
        }
        let sent: Sent = self.call("sendMessage", body, SHORT).await?;
        Ok(sent.message_id)
    }

    /// Replace a message's text. Without a keyboard, any inline keyboard on
    /// the message is removed.
    pub async fn edit_message_text(&self, chat_id: i64, message_id: i64, text: &str) -> Result<()> {
        let body = json!({ "chat_id": chat_id, "message_id": message_id, "text": text });
        self.call::<Value>("editMessageText", body, SHORT).await?;
        Ok(())
    }

    pub async fn send_typing(&self, chat_id: i64) -> Result<()> {
        let body = json!({ "chat_id": chat_id, "action": "typing" });
        self.call::<Value>("sendChatAction", body, SHORT).await?;
        Ok(())
    }

    pub async fn answer_callback_query(&self, id: &str, text: &str) -> Result<()> {
        let body = json!({ "callback_query_id": id, "text": text });
        self.call::<Value>("answerCallbackQuery", body, SHORT)
            .await?;
        Ok(())
    }
}

const SHORT: Duration = Duration::from_secs(20);

/// What an update means to a bridge that serves one owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inbound {
    /// Text the owner typed in their private chat with the bot.
    Text { chat_id: i64, text: String },
    /// The owner pressed an inline button.
    Callback {
        id: String,
        chat_id: i64,
        message_id: i64,
        message_text: String,
        data: String,
    },
    /// Anything else: other senders, groups, non-text messages.
    Ignored(&'static str),
}

/// Classify an update. Only the owner is served, and only in a private
/// chat; everyone else is ignored.
pub fn classify(update: &Update, owner_id: i64) -> Inbound {
    if let Some(query) = &update.callback_query {
        if query.from.id != owner_id {
            return Inbound::Ignored("callback from someone other than the owner");
        }
        let (Some(message), Some(data)) = (&query.message, &query.data) else {
            return Inbound::Ignored("callback without a message or data");
        };
        return Inbound::Callback {
            id: query.id.clone(),
            chat_id: message.chat.id,
            message_id: message.message_id,
            message_text: message.text.clone().unwrap_or_default(),
            data: data.clone(),
        };
    }
    let Some(message) = &update.message else {
        return Inbound::Ignored("update kind the bridge does not handle");
    };
    if message.chat.kind != "private" {
        return Inbound::Ignored("message outside a private chat");
    }
    if message.from.as_ref().map(|user| user.id) != Some(owner_id) {
        return Inbound::Ignored("message from someone other than the owner");
    }
    match &message.text {
        Some(text) if !text.trim().is_empty() => Inbound::Text {
            chat_id: message.chat.id,
            text: text.clone(),
        },
        _ => Inbound::Ignored("message without text"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(value: Value) -> Update {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn only_the_owner_in_a_private_chat_is_served() {
        let owner = update(json!({"update_id": 1, "message": {
            "message_id": 5, "from": {"id": 42}, "chat": {"id": 42, "type": "private"}, "text": "hi"
        }}));
        assert_eq!(
            classify(&owner, 42),
            Inbound::Text {
                chat_id: 42,
                text: "hi".into()
            }
        );
        let stranger = update(json!({"update_id": 2, "message": {
            "message_id": 6, "from": {"id": 7}, "chat": {"id": 7, "type": "private"}, "text": "hi"
        }}));
        assert!(matches!(classify(&stranger, 42), Inbound::Ignored(_)));
        let group = update(json!({"update_id": 3, "message": {
            "message_id": 7, "from": {"id": 42}, "chat": {"id": -100, "type": "group"}, "text": "hi"
        }}));
        assert!(matches!(classify(&group, 42), Inbound::Ignored(_)));
        let sticker = update(json!({"update_id": 4, "message": {
            "message_id": 8, "from": {"id": 42}, "chat": {"id": 42, "type": "private"}
        }}));
        assert!(matches!(classify(&sticker, 42), Inbound::Ignored(_)));
    }

    #[test]
    fn only_the_owners_button_presses_count() {
        let press = |from: i64| {
            update(json!({"update_id": 9, "callback_query": {
                "id": "cb1", "from": {"id": from}, "data": "ap:r1:y",
                "message": {"message_id": 3, "chat": {"id": 42, "type": "private"}, "text": "Allow?"}
            }}))
        };
        assert_eq!(
            classify(&press(42), 42),
            Inbound::Callback {
                id: "cb1".into(),
                chat_id: 42,
                message_id: 3,
                message_text: "Allow?".into(),
                data: "ap:r1:y".into(),
            }
        );
        assert!(matches!(classify(&press(7), 42), Inbound::Ignored(_)));
    }
}
