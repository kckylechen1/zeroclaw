//! Durable source handoff for authenticated bridges. The session backend owns
//! inputs/cursor; this module only owns scheduling in the current process.
use super::*;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use zeroclaw_api::bridge_intake::{BridgeInput, BridgeReceipt, BridgeSource};

const VERSION: u8 = 1;
const MAX_INPUT_BYTES: usize = 64 * 1024;
const MAX_SOURCES: usize = 1024;

type Reply = (Option<Value>, Option<TurnClaim>);

#[derive(Clone)]
pub(crate) struct Claim {
    source: BridgeSource,
    update_id: i64,
}

#[derive(Default)]
pub(crate) struct Scheduling {
    // This fact is created here: a pending DB row already has an in-process
    // TurnClaim. It vanishes on restart, allowing only still-pending recovery.
    scheduled: Mutex<HashSet<(String, i64)>>,
    sources: Mutex<HashMap<String, (BridgeSource, WsTurnScope)>>,
}

fn failure(id: Option<&str>, code: &str) -> Reply {
    let mut reply = json!({"type":"error", "code":code,
        "message":zeroclaw_runtime::i18n::get_required_cli_string("gateway-intake-unavailable")});
    stamp_request_id(&mut reply, id);
    (Some(reply), None)
}

fn source(
    state: &AppState,
    conversation: &Conversation<WsSession>,
    scope: &WsTurnScope,
    namespace: &str,
) -> Option<BridgeSource> {
    let mut parts = namespace.split(':');
    if parts.next()? != "telegram" || namespace.len() > 80 {
        return None;
    }
    for part in [parts.next()?, parts.next()?] {
        if part.is_empty()
            || !part.bytes().all(|b| b.is_ascii_digit())
            || part.parse::<i64>().ok()? <= 0
        {
            return None;
        }
    }
    if parts.next().is_some() {
        return None;
    }
    let (_, alias) = conversation.key().rsplit_once('\u{1f}')?;
    let subject = scope.auth_subject.as_ref()?;
    let config = state.config.read();
    if !config.agents.get(alias).is_some_and(|agent| agent.enabled) {
        return None;
    }
    let (name, _) = config.gateway.bridges.iter().find(|(_, bridge)| {
        bridge.allows_session(&scope.session_id)
            && zeroclaw_config::pairing::constant_time_eq(
                subject,
                &bridge.token_hash.to_ascii_lowercase(),
            )
    })?;
    Some(BridgeSource {
        key: format!("{name}:{namespace}"),
        session_key: scope.session_key.clone(),
        agent_alias: alias.to_string(),
    })
}

fn namespace(source: &BridgeSource) -> &str {
    source
        .key
        .rfind(":telegram:")
        .map_or("", |index| &source.key[index + 1..])
}

fn authorized(
    state: &AppState,
    conversation: &Conversation<WsSession>,
    scope: &WsTurnScope,
    expected: &BridgeSource,
) -> bool {
    source(state, conversation, scope, namespace(expected)).is_some_and(|current| {
        current.key == expected.key
            && current.session_key == expected.session_key
            && current.agent_alias == expected.agent_alias
    })
}

fn register(state: &AppState, source: &BridgeSource, scope: &WsTurnScope) -> std::io::Result<()> {
    let mut sources = state.ws_conversations.intake.sources.lock();
    if !sources.contains_key(&source.key) && sources.len() >= MAX_SOURCES {
        return Err(std::io::Error::other("bridge source capacity exhausted"));
    }
    sources.insert(source.key.clone(), (source.clone(), scope.clone()));
    Ok(())
}

pub(super) fn resume(
    state: &AppState,
    conversation: &Arc<Conversation<WsSession>>,
    scope: &WsTurnScope,
    frame: &Value,
) -> Reply {
    let Some(namespace) = frame["source"].as_str() else {
        return failure(None, "INVALID_SOURCE");
    };
    let Some(source) = source(state, conversation, scope, namespace) else {
        return failure(None, "SOURCE_UNAUTHORIZED");
    };
    let Some(backend) = &state.session_backend else {
        return failure(None, "DURABLE_INTAKE_UNAVAILABLE");
    };
    let Ok(snapshot) = backend.bridge_resume(&source) else {
        return failure(None, "SOURCE_UNAVAILABLE");
    };
    if register(state, &source, scope).is_err() {
        return failure(None, "SOURCE_CAPACITY");
    }
    let mut start = None;
    for input in &snapshot.inputs {
        if input.state == "pending" && input.update_id < snapshot.cursor {
            match schedule(state, conversation, scope, &source, input) {
                Ok(Some(claim)) => {
                    start = Some(claim);
                    break;
                }
                Ok(None) => {}
                Err(_) => return failure(None, "SOURCE_RECOVERY_FAILED"),
            }
        }
    }
    // Scheduling can reject missing attachment bytes. Report the owner's
    // current receipt rather than the pre-recovery snapshot.
    let snapshot = match backend.bridge_resume(&source) {
        Ok(snapshot) => snapshot,
        Err(_) => return (failure(None, "SOURCE_RECOVERY_FAILED").0, start),
    };
    let scheduled = state.ws_conversations.intake.scheduled.lock();
    let unknown: Vec<_> = snapshot
        .inputs
        .iter()
        .filter(|input| {
            matches!(
                input.state.as_str(),
                "running" | "steered" | "outcome_unknown" | "control"
            ) && !scheduled.contains(&(source.key.clone(), input.update_id))
        })
        .map(|input| input.request_id.clone())
        .collect();
    let receipts:Vec<_> = snapshot.inputs.iter().map(|input|json!({
        "update_id":input.update_id,"previous_cursor":input.previous_cursor,"id":input.request_id,"state":input.state
    })).collect();
    (
        Some(
            json!({"type":"source_ready","intake_version":VERSION,"source":namespace,
        "cursor":snapshot.cursor,"unknown":unknown,"receipts":receipts}),
        ),
        start,
    )
}

fn ack(namespace: &str, input: &BridgeInput, receipt: &BridgeReceipt, turn: Option<&str>) -> Value {
    let mut frame = json!({"type":"ack","id":input.request_id,
        "status":if receipt.duplicate {"duplicate"} else {"accepted"},
        "durable":true,"intake_version":VERSION,"state":receipt.state,
        "source":{"namespace":namespace,"update_id":input.update_id,"cursor":receipt.cursor}});
    if let Some(turn) = turn {
        frame["turn"] = turn.into();
    }
    frame
}

pub(super) fn receive(
    state: &AppState,
    conversation: &Arc<Conversation<WsSession>>,
    scope: &WsTurnScope,
    frame: &Value,
) -> Reply {
    let id = frame["id"].as_str();
    let metadata = &frame["source"];
    let Some(ns) = metadata["namespace"].as_str() else {
        return failure(id, "INVALID_SOURCE");
    };
    let Some(source) = source(state, conversation, scope, ns) else {
        return failure(id, "SOURCE_UNAUTHORIZED");
    };
    let (Some(update_id), Some(previous_cursor)) = (
        metadata["update_id"].as_i64(),
        metadata["previous_cursor"].as_i64(),
    ) else {
        return failure(id, "INVALID_SOURCE");
    };
    if previous_cursor < 0 || update_id < previous_cursor || update_id == i64::MAX {
        return failure(id, "INVALID_SOURCE_SEQUENCE");
    }
    let expected_id = format!(
        "tg:{}:{update_id}",
        ns.strip_prefix("telegram:").unwrap_or_default()
    );
    if id != Some(expected_id.as_str()) {
        return failure(id, "INVALID_SOURCE_ID");
    }
    let Some(backend) = &state.session_backend else {
        return failure(id, "DURABLE_INTAKE_UNAVAILABLE");
    };
    let (payload, initial_state) = if frame["type"] == "message" {
        let Some(content) = frame["content"].as_str() else {
            return failure(id, "INVALID_SOURCE_INPUT");
        };
        let attachments = frame
            .get("attachments")
            .cloned()
            .unwrap_or_else(|| json!([]));
        let Some(ids) = attachments.as_array() else {
            return failure(id, "INVALID_ATTACHMENTS");
        };
        if content.len() > MAX_INPUT_BYTES
            || ids.len() > crate::api_attachments::MAX_MESSAGE_ITEMS
            || ids.iter().any(|id| {
                id.as_str()
                    .is_none_or(|s| s.len() != 36 || uuid::Uuid::parse_str(s).is_err())
            })
            || (content.is_empty() && ids.is_empty())
            || ids
                .iter()
                .filter_map(Value::as_str)
                .collect::<HashSet<_>>()
                .len()
                != ids.len()
        {
            return failure(id, "INVALID_SOURCE_INPUT");
        }
        (
            json!({"content":content,"attachments":attachments}).to_string(),
            "pending",
        )
    } else {
        let Some(disposition) = frame["disposition"]
            .as_str()
            .filter(|s| matches!(*s, "ignored" | "rejected" | "control"))
        else {
            return failure(id, "INVALID_SOURCE_DISPOSITION");
        };
        (json!({"disposition":disposition}).to_string(), disposition)
    };
    let input = BridgeInput {
        update_id,
        previous_cursor,
        request_id: expected_id,
        payload,
        state: initial_state.to_string(),
    };
    let Ok(mut receipt) = backend.bridge_record(&source, &input) else {
        return failure(id, "SOURCE_NOT_RECORDED");
    };
    if register(state, &source, scope).is_err() {
        return failure(id, "SOURCE_CAPACITY");
    }
    // Any committed disposition can close a predecessor gap and make an
    // older pending message executable, even when this update is ignored.
    let start = match next_pending(state, conversation, scope, &source) {
        Ok(claim) => claim,
        Err(_) => return failure(id, "SOURCE_RECOVERY_FAILED"),
    };
    // Materialization may have rejected the whole input. Read its state from
    // the journal, never reconstruct acceptance from a socket write.
    match backend.bridge_receipt(&source, update_id) {
        Ok(Some(updated)) => {
            receipt.state = updated.state;
            receipt.cursor = updated.cursor;
        }
        _ => return (failure(id, "SOURCE_RECEIPT_UNAVAILABLE").0, start),
    }
    let turn = if start
        .as_ref()
        .is_some_and(|claim| claim.request_id.as_deref() == id)
    {
        Some("started")
    } else {
        None
    };
    (Some(ack(ns, &input, &receipt, turn)), start)
}

fn materialize(
    state: &AppState,
    source: &BridgeSource,
    scope: &WsTurnScope,
    payload: &str,
) -> std::io::Result<String> {
    let frame: Value = serde_json::from_str(payload).map_err(std::io::Error::other)?;
    let content = frame["content"]
        .as_str()
        .ok_or_else(|| std::io::Error::other("invalid persisted bridge input"))?;
    let ids: Vec<String> = frame["attachments"]
        .as_array()
        .ok_or_else(|| std::io::Error::other("invalid persisted attachment list"))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| std::io::Error::other("invalid attachment"))
        })
        .collect::<Result<_, _>>()?;
    if ids.is_empty() {
        return Ok(content.to_string());
    }
    let payload_scope = crate::api_attachments::Scope {
        subject: scope.auth_subject.clone().unwrap_or_default(),
        session: scope.session_id.clone(),
        agent: source.agent_alias.clone(),
    };
    if !crate::api_attachments::scope_authorized(state, &payload_scope) {
        return Err(std::io::Error::other("attachment scope no longer admitted"));
    }
    state
        .ws_conversations
        .attachments
        .materialize(&payload_scope, &ids, content)
        .map_err(|status| std::io::Error::other(status.to_string()))
}

fn schedule(
    state: &AppState,
    conversation: &Arc<Conversation<WsSession>>,
    scope: &WsTurnScope,
    source: &BridgeSource,
    input: &BridgeInput,
) -> std::io::Result<Option<TurnClaim>> {
    if !authorized(state, conversation, scope, source) {
        return Err(std::io::Error::other("source authority revoked"));
    }
    let backend = state
        .session_backend
        .as_ref()
        .ok_or_else(|| std::io::Error::other("durable backend unavailable"))?;
    let key = (source.key.clone(), input.update_id);
    if !state
        .ws_conversations
        .intake
        .scheduled
        .lock()
        .insert(key.clone())
    {
        return Ok(None);
    }
    if conversation.is_running() {
        state.ws_conversations.intake.scheduled.lock().remove(&key);
        return Ok(None);
    }
    let content = match materialize(state, source, scope, &input.payload) {
        Ok(content) => content,
        Err(_) => {
            state.ws_conversations.intake.scheduled.lock().remove(&key);
            backend.bridge_finish(source, input.update_id, "rejected")?;
            let (frame, _) = failure(Some(&input.request_id), "SOURCE_ATTACHMENT_UNAVAILABLE");
            if let Some(frame) = frame {
                conversation.publish(&frame);
            }
            return Ok(None);
        }
    };
    if !authorized(state, conversation, scope, source) {
        state.ws_conversations.intake.scheduled.lock().remove(&key);
        return Err(std::io::Error::other("source authority revoked"));
    }
    if let Some(mut claim) = conversation.start_if_idle(content) {
        claim.request_id = Some(input.request_id.clone());
        claim.intake = Some(Claim {
            source: source.clone(),
            update_id: input.update_id,
        });
        Ok(Some(claim))
    } else {
        state.ws_conversations.intake.scheduled.lock().remove(&key);
        Ok(None)
    }
}

pub(super) fn begin(
    state: &AppState,
    conversation: &Conversation<WsSession>,
    scope: &WsTurnScope,
    claim: Option<&Claim>,
) -> std::io::Result<bool> {
    let Some(claim) = claim else {
        return Ok(true);
    };
    let backend = state
        .session_backend
        .as_ref()
        .ok_or_else(|| std::io::Error::other("durable backend unavailable"))?;
    if !authorized(state, conversation, scope, &claim.source) {
        return Ok(false);
    }
    backend.bridge_claim(&claim.source, claim.update_id)
}

pub(super) fn finish(
    state: &AppState,
    conversation: &Conversation<WsSession>,
    claim: &Claim,
    outcome: &str,
) {
    if let Some(backend) = &state.session_backend
        && backend
            .bridge_finish(&claim.source, claim.update_id, outcome)
            .is_err()
    {
        let (frame, _) = failure(None, "SOURCE_SETTLEMENT_UNKNOWN");
        if let Some(frame) = frame {
            conversation.publish(&frame);
        }
    }
    release(state, claim);
}

pub(super) fn release(state: &AppState, claim: &Claim) {
    state
        .ws_conversations
        .intake
        .scheduled
        .lock()
        .remove(&(claim.source.key.clone(), claim.update_id));
}

fn next_pending(
    state: &AppState,
    conversation: &Arc<Conversation<WsSession>>,
    scope: &WsTurnScope,
    source: &BridgeSource,
) -> std::io::Result<Option<TurnClaim>> {
    if conversation.is_running() {
        return Ok(None);
    }
    let backend = state
        .session_backend
        .as_ref()
        .ok_or_else(|| std::io::Error::other("durable backend unavailable"))?;
    let snapshot = backend.bridge_resume(source)?;
    for input in snapshot
        .inputs
        .into_iter()
        .filter(|input| input.state == "pending" && input.update_id < snapshot.cursor)
    {
        if let Some(claim) = schedule(state, conversation, scope, source, &input)? {
            return Ok(Some(claim));
        }
    }
    Ok(None)
}

pub(super) fn resume_registered(
    state: &AppState,
    conversation: &Arc<Conversation<WsSession>>,
) -> Option<(WsTurnScope, TurnClaim)> {
    let sources: Vec<_> = state
        .ws_conversations
        .intake
        .sources
        .lock()
        .values()
        .filter(|(source, _)| {
            conversation.key() == format!("{}\u{1f}{}", source.session_key, source.agent_alias)
        })
        .cloned()
        .collect();
    for (source, scope) in sources {
        if !authorized(state, conversation, &scope, &source) {
            continue;
        }
        if let Ok(Some(claim)) = next_pending(state, conversation, &scope, &source) {
            return Some((scope, claim));
        }
    }
    None
}
