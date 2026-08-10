//! Channel system-prompt assembly and per-turn context preamble.
//!
//! Extracted from `orchestrator/mod.rs` so the cached system-prompt prefix
//! (byte-stable) and the volatile turn preamble can evolve independently of
//! the channel turn dispatch path.

use std::sync::Arc;
use zeroclaw_api::channel::Channel;

pub(crate) const CURRENT_DATE_HEADING: &str = "## Current Date\n\n";
pub(crate) const LEGACY_CURRENT_DATE_TIME_HEADING: &str = "## Current Date & Time\n\n";

pub(crate) fn channel_delivery_instructions(channel_name: &str) -> Option<&'static str> {
    match channel_name {
        "matrix" => Some(
            "When responding on Matrix:\n\
             - Use Markdown formatting (bold, italic, code blocks)\n\
             - Be concise and direct\n\
             - For media attachments use markers: [IMAGE:<path-or-url>], [DOCUMENT:<path-or-url>], [VIDEO:<path-or-url>], [AUDIO:<path-or-url>], or [VOICE:<path-or-url>]\n\
             - Local marker paths may be workspace-relative or absolute, but they must resolve inside the configured workspace directory.\n\
             - Copy paths from inbound messages or file tools exactly into markers. Do not add, remove, or rewrite path components.\n\
             - Remote media is also accepted via http:// or https:// URLs in the same marker form.\n\
             - Keep normal text outside markers and never wrap markers in code fences.\n\
             - When you receive a [Voice message], the user spoke to you. Respond naturally as in conversation.\n\
             - Your text reply will automatically be converted to audio and sent back as a voice message.\n",
        ),
        "discord" => Some(
            "When responding on Discord:\n\
             - Use Markdown formatting (bold, italic, code blocks)\n\
             - Be concise and direct\n\
             - For media attachments use markers: [IMAGE:<absolute-path>], [DOCUMENT:<absolute-path>], [VIDEO:<absolute-path>], [AUDIO:<absolute-path>], or [VOICE:<absolute-path>]\n\
             - Paths inside markers MUST be absolute (starting with /) and live inside the configured workspace directory. Never use relative paths.\n\
             - Remote media is also accepted via http:// or https:// URLs in the same marker form.\n\
             - For a rich embed, emit [EMBED:{...}] where {...} is a Discord embed JSON object (keys: title, description, url, color, timestamp, footer{text,icon_url}, image, thumbnail, author{name,url,icon_url}, fields[{name,value,inline}]). Any image/thumbnail/icon/url MUST be an http(s) URL; local paths are not embeddable. Keep the JSON on one line.\n\
             - To offer interactive buttons or a menu, emit one marker [COMPONENTS:{\"rows\":[[<component>, ...], ...]}] on a single line (up to 5 rows; a row holds up to 5 buttons OR exactly one select). Action button: {\"label\":\"Approve\",\"style\":\"primary|secondary|success|danger\",\"prompt\":\"<text run as a new turn when clicked>\"}; link button: {\"label\":\"Docs\",\"url\":\"https://...\"}; select: {\"select\":\"placeholder\",\"options\":[{\"label\":\"A\",\"value\":\"a\",\"prompt\":\"<run when chosen>\"}, ...]}. A button may instead carry a modal (a popup form) in place of prompt/url: {\"label\":\"Report\",\"style\":\"danger\",\"prompt\":\"<run on submit>\",\"modal\":{\"title\":\"Report\",\"fields\":[{\"id\":\"reason\",\"label\":\"Reason\",\"style\":\"short|paragraph\",\"required\":true,\"placeholder\":\"...\",\"min\":1,\"max\":500}]}} — clicking opens the form and the typed field values are appended to that button's prompt when submitted. Every action button and select option needs a prompt describing what should happen when it is clicked.\n\
             - Keep normal text outside markers and never wrap markers in code fences.\n",
        ),
        "whatsapp" | "whatsapp-web" => Some(
            "When responding on WhatsApp Web:\n\
             - Be concise and direct\n\
             - For media attachments use markers: [IMAGE:<path>], [DOCUMENT:<path>], [VIDEO:<path>], [AUDIO:<path>], or [VOICE:<path>]\n\
             - To send a native location pin, use marker: [LOCATION:<latitude>,<longitude>,<name>,<address>] where name and address are optional. Double-quote the name if it contains commas; the trailing address may contain commas without quoting.\n\
             - Marker paths must refer to local files inside the configured workspace directory. Absolute paths and workspace-relative paths are accepted when they stay inside that workspace.\n\
             - Do not use http://, https://, data:, file:, or any other URL scheme in WhatsApp Web media markers.\n\
             - Keep normal text outside markers and never wrap markers in code fences.\n",
        ),
        "lark" | "feishu" => Some(
            "When responding on Lark/Feishu:\n\
             - Be concise and direct\n\
             - Use Markdown formatting for readable answers\n\
             - If a tool can answer the task, use your tools instead of stopping at a plain chat reply\n\
             - Use tool results silently: answer with the outcome and do not narrate internal tool execution bookkeeping\n\
             - For media attachments use markers: [IMAGE:<path>], [DOCUMENT:<path>], [VIDEO:<path>], [AUDIO:<path>], or [VOICE:<path>]\n\
             - Marker paths must refer to local files inside the configured workspace directory. Absolute paths and workspace-relative paths are accepted when they stay inside that workspace.\n\
             - Do not use http://, https://, data:, file:, or any other URL scheme in Lark/Feishu media markers.\n\
             - Keep normal text outside markers and never wrap markers, tool output, or protocol markup in code fences.\n",
        ),
        "telegram" => Some(
            "When responding on Telegram:\n\
             - Include media markers for files or URLs that should be sent as attachments\n\
             - Use **bold** for key terms, section titles, and important info (renders as <b>)\n\
             - Use *italic* for emphasis (renders as <i>)\n\
             - Use `backticks` for inline code, commands, or technical terms\n\
             - Use triple backticks for code blocks\n\
             - Use emoji naturally to add personality — but don't overdo it\n\
             - Be concise and direct. Skip filler phrases like 'Great question!' or 'Certainly!'\n\
             - Structure longer answers with bold headers, not raw markdown ## headers\n\
             - For media attachments use markers: [IMAGE:<path-or-url>], [DOCUMENT:<path-or-url>], [VIDEO:<path-or-url>], [AUDIO:<path-or-url>], or [VOICE:<path-or-url>]\n\
             - Keep normal text outside markers and never wrap markers in code fences.\n\
             - When a question needs current, real-time, or external information \
               (prices, news, weather, web pages, lookups, etc.), use your tools — \
               e.g. web_search_tool and web_fetch — to obtain it before answering; \
               never guess or answer from memory alone when a tool can verify it.\n\
             - Present the final answer to the latest user message directly from the \
               tool results, without narrating delayed/internal tool-execution bookkeeping.",
        ),
        "qq" => Some(
            "When responding on QQ:\n\
             - Use Markdown formatting\n\
             - Be concise and direct\n\
             - For media attachments use markers: [IMAGE:<path-or-url>], [DOCUMENT:<path-or-url>], \
               [VIDEO:<path-or-url>], [VOICE:<path-or-url>]\n\
             - Voice supports .wav, .mp3, .silk formats only. Other audio formats use [DOCUMENT:]\n\
             - Keep normal text outside markers and never wrap markers in code fences.\n",
        ),
        "wechat" => Some(
            "When responding on WeChat:\n\
             - Be concise and direct\n\
             - For media attachments use markers: [IMAGE:<path-or-url>], [DOCUMENT:<path-or-url>], \
               [VIDEO:<path-or-url>], [AUDIO:<path-or-url>], or [VOICE:<path-or-url>]\n\
             - Keep normal text outside markers and never wrap markers in code fences.\n\
             - Use absolute local paths when sending generated files whenever possible.\n",
        ),
        "wecom_ws" => Some(
            "When responding on WeCom AI Bot WebSocket:\n\
             - Be concise and direct\n\
             - Use Markdown text; the channel sends progressive draft updates when enabled\n\
             - Do not use local attachment markers; outbound image payloads are not supported yet.\n",
        ),
        _ => None,
    }
}

pub(crate) fn build_channel_system_prompt_for_message(
    base_prompt: &str,
    msg: &zeroclaw_api::channel::ChannelMessage,
    target_channel: Option<&Arc<dyn Channel>>,
) -> String {
    let bot_mention = target_channel.and_then(|c| c.self_addressed_mention());
    build_channel_system_prompt(base_prompt, &msg.channel, bot_mention.as_deref())
}

/// Build the cached system-prompt prefix for a channel session.
///
/// **Byte-stability contract:** given identical `base_prompt`, `channel_name`,
/// and `bot_mention` arguments, this function MUST return byte-identical
/// output across consecutive calls — even across a second boundary, across
/// sender/reply_target/message_id changes, and across per-turn memory
/// recall. Provider-side prompt caching keys on this prefix, so any
/// per-turn data here invalidates the cache for every turn.
///
/// The volatile per-turn data (datetime, reply_target, sender, message_id,
/// cron_add delivery hint, and bot_mention for the current turn only)
/// lives in [`build_channel_turn_context_preamble`] and is prepended to
/// the outgoing user turn by the caller.
pub(crate) fn build_channel_system_prompt(
    base_prompt: &str,
    channel_name: &str,
    bot_mention: Option<&str>,
) -> String {
    let mut prompt = base_prompt.to_string();

    // Date refresh stays in the system prompt: the heading is date-only
    // (no seconds), so within a single day the rendered value is stable and
    // cache hits; it only changes once per day at midnight. Acceptable for
    // a 99%+ intra-session cache-hit rate.
    refresh_channel_prompt_date_section(&mut prompt);

    if let Some(instructions) = channel_delivery_instructions(channel_name) {
        if prompt.is_empty() {
            prompt = instructions.to_string();
        } else {
            prompt = format!("{prompt}\n\n{instructions}");
        }
    }

    if let Some(mention) = bot_mention {
        // Self-addressed mention handling is byte-stable: the mention
        // string is fixed per channel (set once at channel boot), so the
        // block content does not vary across turns.
        let block = format!(
            "\n\nYour addressable handle on this channel: {mention}. \
             When you see this exact string anywhere in an inbound message, \
             it refers to YOU, not another agent or user. This same format \
             is also what you should emit when you need to tag yourself or \
             address peers in outbound replies on this channel."
        );
        prompt.push_str(&block);
    }

    // Calibration note: static behavioral instruction that benefits from
    // the higher weight of the system prompt. Lifted out of the deleted
    // per-turn Channel context block so it survives the relocation.
    prompt.push_str(
        "\n\nCalibration note: agents in this system currently err on the side \
         of silence when a response would be appropriate, which users find \
         frustrating. Skew toward replying. Memory is supplementary context \
         that informs how you respond, not a gate on whether you respond.",
    );

    prompt
}

pub(crate) fn build_channel_system_prompt_for_message_with_signal(
    base_prompt: &str,
    msg: &zeroclaw_api::channel::ChannelMessage,
    target_channel: Option<&Arc<dyn Channel>>,
    native_tool_specs_present: bool,
) -> String {
    let prompt = build_channel_system_prompt_for_message(base_prompt, msg, target_channel);
    let want = if native_tool_specs_present {
        ::zeroclaw_runtime::agent::system_prompt::NATIVE_TOOLS_TASK_FRAMING
    } else {
        ::zeroclaw_runtime::agent::system_prompt::NO_TOOLS_TASK_FRAMING
    };
    if prompt.contains(::zeroclaw_runtime::agent::system_prompt::NATIVE_TOOLS_TASK_FRAMING) {
        prompt.replace(
            ::zeroclaw_runtime::agent::system_prompt::NATIVE_TOOLS_TASK_FRAMING,
            want,
        )
    } else if prompt.contains(::zeroclaw_runtime::agent::system_prompt::NO_TOOLS_TASK_FRAMING) {
        prompt.replace(
            ::zeroclaw_runtime::agent::system_prompt::NO_TOOLS_TASK_FRAMING,
            want,
        )
    } else {
        // Anchor absent (custom system_prompt_prefix or unusual config);
        // no-op. Preserves byte-stability for non-default startup prompts.
        prompt
    }
}

pub(crate) fn current_date_section() -> String {
    let now = chrono::Local::now();
    format!(
        "{CURRENT_DATE_HEADING}{} ({})",
        now.format("%Y-%m-%d"),
        now.format("%:z")
    )
}

pub(crate) fn refresh_channel_prompt_date_section(prompt: &mut String) {
    let runtime_start = prompt
        .find("\n## Runtime")
        .map(|i| i + 1)
        .unwrap_or(prompt.len());

    if let Some((start, heading_len)) = find_latest_date_heading_before(prompt, runtime_start) {
        let content_start = start + heading_len;
        let section_end = prompt[content_start..]
            .find("\n## ")
            .map(|i| content_start + i)
            .unwrap_or(prompt.len());
        prompt.replace_range(start..section_end, &current_date_section());
    }
}

pub(crate) fn find_latest_date_heading_before(
    prompt: &str,
    before: usize,
) -> Option<(usize, usize)> {
    let prefix = &prompt[..before];
    [CURRENT_DATE_HEADING, LEGACY_CURRENT_DATE_TIME_HEADING]
        .iter()
        .filter_map(|heading| prefix.rfind(heading).map(|start| (start, heading.len())))
        .max_by_key(|(start, _)| *start)
}

/// Build the volatile per-turn context that the model needs but the cached
/// system prompt must NOT contain. The caller prepends the returned string
/// to the current outgoing user turn; the cached conversation history copy
/// stays clean.
///
/// **Trust-boundary contract:** the caller MUST prepend this preamble to the
/// current outgoing user turn whenever `reply_target` is non-empty, without
/// inspecting user-controlled content. A user message that happens to start
/// with `[turn-context]` is not treated as proof that this preamble is
/// already present — the runtime preamble is authoritative, not
/// user-suppressible. (An earlier draft used a `starts_with("[turn-context]")`
/// guard on the outgoing user turn that let a malicious sender suppress the
/// `reply_target` / `sender` / delivery hint; this helper removes that
/// regression.)
///
/// Carries: channel/reply_target/sender/message_id, the wall-clock datetime,
/// the `cron_add` delivery hint (with the webhook `delivery.thread_id`
/// contract preserved), and (if set) the bot_mention handle.
pub(crate) fn build_channel_turn_context_preamble(
    msg: &zeroclaw_api::channel::ChannelMessage,
    target_channel: Option<&Arc<dyn Channel>>,
) -> String {
    if msg.reply_target.is_empty() {
        // CLI-style path: no channel recipient, no need to inject channel
        // context. Mirrors the CLI shape where no preamble is added.
        return String::new();
    }

    let now = chrono::Local::now();
    let channel_name = msg.channel.as_str();
    let reply_target = msg.reply_target.as_str();
    let sender = msg.sender.as_str();
    let message_id = msg.id.as_str();

    // Webhook contract: downstream services expect the *sender* as the
    // recipient and the thread/conversation identifier in `thread_id`.
    // Reusing `reply_target` as `to` for webhook would strip the thread
    // context and the receiver would discard the reply.
    let delivery_hint = if channel_name.eq_ignore_ascii_case("webhook") {
        format!(
            "delivery={{\"mode\":\"announce\",\"channel\":\"{channel_name}\",\
             \"to\":\"{sender}\",\"thread_id\":\"{reply_target}\"}}"
        )
    } else {
        format!(
            "delivery={{\"mode\":\"announce\",\"channel\":\"{channel_name}\",\
             \"to\":\"{reply_target}\"}}"
        )
    };

    let mut preamble = format!(
        "[turn-context] time={time} date={date} tz={tz} \
         channel={channel} reply_target={reply_target} sender={sender} \
         message_id={message_id}. The sender field is the platform-specific \
         user ID of the person who sent this message. Use it to distinguish \
         between different users. The message_id field identifies this \
         incoming message; pass it as the `message_id` argument when calling \
         the `reaction` tool. When scheduling delayed messages or reminders \
         via cron_add for this conversation, use {delivery_hint} so the \
         message reaches the user.\n\n",
        time = now.format("%H:%M:%S"),
        date = now.format("%Y-%m-%d"),
        tz = now.format("%Z"),
        channel = channel_name,
        reply_target = reply_target,
        sender = sender,
        message_id = message_id,
        delivery_hint = delivery_hint,
    );

    if let Some(channel) = target_channel
        && let Some(mention) = channel.self_addressed_mention()
    {
        preamble.push_str(&format!(
            "Your addressable handle on this channel: {mention}. \
             When you see this exact string anywhere in an inbound message, \
             it refers to YOU, not another agent or user. This same format \
             is also what you should emit when you need to tag yourself or \
             address peers in outbound replies on this channel.\n\n"
        ));
    }

    preamble
}

pub(crate) fn compose_outgoing_user_turn_with_context(
    preamble: &str,
    raw_user_content: &str,
) -> String {
    let mut parts: Vec<&str> = Vec::with_capacity(2);
    if !preamble.is_empty() {
        parts.push(preamble);
    }
    parts.push(raw_user_content);
    parts.join("\n\n")
}
