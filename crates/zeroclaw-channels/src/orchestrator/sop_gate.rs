//! Channel SOP-gate answer resolution. Extracted from orchestrator/mod.rs.

use std::sync::Arc;

use zeroclaw_api::channel::Channel;

use super::{
    AgentRouter, channel_key_for_message, finalize_gate_prompts, parse_gate_reference,
    text_gate_reply_matches_approval_route,
};

/// Resolve a SOP gate answered from a chat channel. Two answer forms converge
/// here, per the channel-agnostic gate-prompt seam:
///
/// - a component click: the channel's OWN interaction producer stamps the
///   internal `sop.gate:<choice>:<reference>` marker (unforgeable from message
///   text, same guarantee as the git producer's SOP-event marker);
/// - a plain `<choice> <reference>` text reply (the fallback prompt tells the
///   operator to send exactly this) — consumed ONLY when the reference matches a
///   run actually parked on a human AND the run's current policy can deliver its
///   approval prompt to this same channel route. Ordinary conversation and
///   unauthorised channel traffic never get swallowed.
///
/// Returns `true` when the message was consumed as a gate answer.
pub(crate) async fn dispatch_channel_sop_gate(
    router: &AgentRouter,
    msg: &zeroclaw_api::channel::ChannelMessage,
    config: &zeroclaw_config::schema::Config,
    gate_prompt_channels: &[Arc<dyn Channel>],
    gate_channel_route_keys: &[String],
) -> bool {
    const MARKER_PREFIX: &str = "sop.gate:";
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Form {
        Marker,
        Text,
    }
    let (form, choice, reference) = if let Some(rest) = msg
        .internal_sop_event
        .as_deref()
        .and_then(|s| s.strip_prefix(MARKER_PREFIX))
    {
        match rest.split_once(':') {
            // Any known gate-choice token is a valid marker; unknown tokens are
            // dropped, never coerced (the enum is the single vocabulary).
            Some((c, r))
                if !r.is_empty() && zeroclaw_api::channel::GateChoiceKind::from_id(c).is_some() =>
            {
                (Form::Marker, c.to_ascii_lowercase(), r.to_string())
            }
            _ => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_attrs(::serde_json::json!({"marker": rest})),
                    "dropping malformed or unknown channel SOP-gate marker"
                );
                return true;
            }
        }
    } else if msg.internal_sop_event.is_none() {
        // Text form: exactly two tokens, and the first must be a text-free
        // choice. Edit/Revise stay marker-only (they carry a text payload a
        // two-token reply cannot); approve/deny remain universally answerable.
        let mut words = msg.content.split_whitespace();
        match (words.next(), words.next(), words.next()) {
            (Some(c), Some(r), None)
                if zeroclaw_api::channel::GateChoiceKind::from_id(c)
                    .is_some_and(|k| !k.collects_text()) =>
            {
                (Form::Text, c.to_ascii_lowercase(), r.to_string())
            }
            _ => return false,
        }
    } else {
        return false;
    };

    let Some(engine) = router.sop_engine.as_ref() else {
        // A marker message exists only to answer a gate — consume it either way.
        return matches!(form, Form::Marker);
    };

    let (ref_run, ref_rev) = parse_gate_reference(&reference);
    let channel_key = channel_key_for_message(msg);
    let mut channel_route_keys = gate_channel_route_keys.to_vec();
    if !channel_route_keys
        .iter()
        .any(|route_key| route_key == &channel_key)
    {
        channel_route_keys.push(channel_key.clone());
    }

    // Resolve against runs actually parked on a human. Both marker and plain text
    // replies must carry the full run id minted in the prompt. For the TEXT form
    // a non-match means "not a gate answer" — fall through to the agent; a marker
    // non-match is consumed (stale buttons after the run ended). A matched run
    // whose CURRENT revision differs from the reference's is superseded only
    // after that replacement park is durable. While persistence retries, the
    // prior prompt stays visible and is not finalized as stale. Text replies
    // must first prove they came through a policy route that can present fallback
    // instructions.
    let resolved = {
        let Ok(guard) = engine.lock() else {
            return matches!(form, Form::Marker);
        };
        let mut candidates = guard.active_runs().values().filter(|r| {
            matches!(
                r.status,
                zeroclaw_runtime::sop::types::SopRunStatus::WaitingApproval
                    | zeroclaw_runtime::sop::types::SopRunStatus::PausedCheckpoint
            )
        });
        let matched: Vec<(String, u32, bool, bool)> = candidates
            .by_ref()
            .filter(|r| r.run_id == ref_run)
            .map(|r| {
                let text_admissible = matches!(form, Form::Marker)
                    || text_gate_reply_matches_approval_route(
                        &guard,
                        &r.run_id,
                        &channel_route_keys,
                        &msg.reply_target,
                    );
                let superseded = guard.is_gate_reference_superseded(&r.run_id, ref_rev);
                (r.run_id.clone(), r.revision, text_admissible, superseded)
            })
            .collect();
        match matched.as_slice() {
            [one] => Some(one.clone()),
            _ => None,
        }
    };
    if let Some((run_id, _, false, _)) = &resolved {
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(
                ::serde_json::json!({
                    "run_id": run_id,
                    "reference": reference,
                    "channel": channel_key,
                    "reply_target": msg.reply_target.as_str(),
                })
            ),
            "channel SOP-gate text reply did not match a gate approval route"
        );
        return false;
    }
    if let Some((run_id, current_rev, _, true)) = &resolved {
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(
                ::serde_json::json!({
                    "run_id": run_id,
                    "reference": reference,
                    "current_revision": current_rev,
                    "channel": msg.channel.as_str(),
                })
            ),
            "channel SOP-gate answer targeted a superseded prompt revision"
        );
        finalize_gate_prompts(
            gate_prompt_channels,
            &reference,
            "\u{1f501} This prompt was superseded by a newer draft \u{2014} \
             answer the latest prompt instead.",
        )
        .await;
        // Consumed for both forms: it named a real parked gate, just an old
        // presentation of it — never a message for the agent.
        return true;
    }
    let resolved_run_id = resolved.map(|(run_id, _, _, _)| run_id);
    let Some(run_id) = resolved_run_id else {
        return match form {
            Form::Marker => {
                ::zeroclaw_log::record!(
                    INFO,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_attrs(::serde_json::json!({
                            "reference": reference,
                            "channel": msg.channel.as_str(),
                        })),
                    "channel SOP-gate click did not match a parked run (stale or finished)"
                );
                // Name the state correctly on the prompt itself: this gate's
                // approval window has passed.
                finalize_gate_prompts(
                    gate_prompt_channels,
                    &reference,
                    "\u{23f0} The approval window for this gate has passed \
                     (the run already resolved or finished).",
                )
                .await;
                true
            }
            Form::Text => false,
        };
    };

    use zeroclaw_api::channel::GateChoiceKind;
    use zeroclaw_runtime::sop::approval::ApprovalDecision;
    // `choice` already passed `GateChoiceKind::from_id` at parse time; this
    // match is exhaustive over the enum, so a new choice is a compile error
    // here (not a silent fall-through to Deny).
    let decision = match GateChoiceKind::from_id(&choice) {
        Some(GateChoiceKind::Approve) => ApprovalDecision::Approve,
        Some(GateChoiceKind::Deny) | None => ApprovalDecision::Deny {
            reason: Some(format!("denied by {} via {channel_key}", msg.sender)),
        },
        // Edit / Revise carry their text in the marker message's content (the
        // connector puts the modal's typed field there). Empty text cannot
        // amend or steer anything — consume without resolving (the connector's
        // required-field modal makes this unreachable in practice).
        Some(kind @ (GateChoiceKind::Edit | GateChoiceKind::Revise)) => {
            let text = msg.content.trim().to_string();
            if text.is_empty() {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_attrs(::serde_json::json!({
                            "run_id": run_id,
                            "choice": choice,
                        })),
                    "channel SOP-gate edit/revise arrived without text; ignored"
                );
                return true;
            }
            if kind == GateChoiceKind::Edit {
                ApprovalDecision::Amend { text }
            } else {
                ApprovalDecision::Revise { guidance: text }
            }
        }
    };
    let is_edit = matches!(decision, ApprovalDecision::Amend { .. });
    let principal = zeroclaw_runtime::sop::approval::ApprovalPrincipal::channel(
        channel_key.clone(),
        Some(msg.sender.clone()),
    );
    let outcome = match engine.lock() {
        Ok(mut guard) => guard.resolve_via_broker(&run_id, decision, principal),
        Err(_) => return true,
    };
    match outcome {
        Ok(outcome) => {
            zeroclaw_runtime::sop::drive_resumed_broker_action(
                config,
                Arc::clone(engine),
                router.sop_audit.clone(),
                &outcome,
            );
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(::serde_json::json!({
                        "run_id": run_id,
                        "choice": choice,
                        "sender": msg.sender,
                        "channel": channel_key,
                        "outcome": outcome.label(),
                    })),
                "channel SOP-gate answer resolved"
            );
            // Finalize the prompt (strip buttons, show the decision in place)
            // ONLY on terminal outcomes. Non-terminal ones — pending quorum, a
            // failed slot re-acquire — leave the buttons alive so the decision
            // can be retried or CHANGED while the run is still parked.
            use zeroclaw_runtime::sop::approval::{BrokerOutcome, ResolveOutcome};
            let final_text = match &outcome {
                BrokerOutcome::Resolved(ResolveOutcome::Resumed(_)) if is_edit => Some(format!(
                    "\u{2705} Approved with edits by <@{}> \u{2014} run resumed with the \
                     amended text.",
                    msg.sender
                )),
                BrokerOutcome::Resolved(ResolveOutcome::Resumed(_)) => Some(format!(
                    "\u{2705} Approved by <@{}> \u{2014} run resumed.",
                    msg.sender
                )),
                BrokerOutcome::Resolved(ResolveOutcome::Denied) => Some(format!(
                    "\u{1f6ab} Denied by <@{}> \u{2014} run cancelled.",
                    msg.sender
                )),
                BrokerOutcome::Resolved(ResolveOutcome::Revised) => Some(format!(
                    "\u{1f501} Revision requested by <@{}> \u{2014} a new draft prompt is \
                     on its way.",
                    msg.sender
                )),
                BrokerOutcome::Resolved(ResolveOutcome::AlreadyResolved) => Some(
                    "\u{23f0} The approval window for this gate has passed \
                     (already resolved)."
                        .to_string(),
                ),
                _ => None,
            };
            // Finalize by the prompt's CANONICAL reference (revision-qualified
            // when > 0): the prompt registry is keyed by what was sent.
            let finalize_reference = if ref_rev == 0 {
                run_id.clone()
            } else {
                format!("{run_id}#{ref_rev}")
            };
            if let Some(text) = final_text {
                finalize_gate_prompts(gate_prompt_channels, &finalize_reference, &text).await;
            }
        }
        Err(e) => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "run_id": run_id,
                        "error": e.to_string(),
                    })),
                "channel SOP-gate resolution failed"
            );
        }
    }
    true
}
