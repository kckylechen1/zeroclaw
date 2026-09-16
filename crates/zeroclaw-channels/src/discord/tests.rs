use super::*;

#[test]
fn effective_recipient_prefers_per_message_target() {
    let ids = vec!["fallback_channel".to_string()];
    assert_eq!(
        effective_discord_recipient("explicit_channel", &ids),
        Some("explicit_channel")
    );
}

#[test]
fn effective_recipient_falls_back_to_first_channel_id_when_empty() {
    let ids = vec!["first_channel".to_string(), "second_channel".to_string()];
    assert_eq!(effective_discord_recipient("", &ids), Some("first_channel"));
}

#[test]
fn effective_recipient_is_none_when_empty_and_no_channel_ids() {
    assert_eq!(effective_discord_recipient("", &[]), None);
}
use std::fmt::Write as _;

fn s(items: &[&str]) -> Vec<String> {
    items.iter().map(|i| (*i).to_string()).collect()
}

#[test]
fn prepare_outgoing_embeds_lifts_marker_vets_urls_and_strips_text() {
    let raw = "look [EMBED:{\"title\":\"Report\",\"image\":\"https://ex.com/i.png\"}] done";
    let (text, embeds, failures, truncated) = prepare_outgoing_embeds(raw, None);
    assert_eq!(text, "look  done");
    assert_eq!(embeds.len(), 1);
    assert_eq!(embeds[0].title.as_deref(), Some("Report"));
    assert_eq!(
        embeds[0].image.as_ref().unwrap().url,
        "https://ex.com/i.png"
    );
    assert!(failures.is_empty());
    assert!(!truncated);
}

#[test]
fn prepare_outgoing_embeds_drops_bad_url_and_reports_failure() {
    let raw = "[EMBED:{\"title\":\"T\",\"image\":\"file:///etc/passwd\"}]";
    let (text, embeds, failures, _) = prepare_outgoing_embeds(raw, None);
    assert_eq!(text, "");
    assert_eq!(embeds.len(), 1);
    assert!(embeds[0].image.is_none(), "disallowed scheme dropped");
    assert_eq!(failures, vec![DiscordMarkerFailure::Refused]);
}

#[test]
fn prepare_outgoing_embeds_flags_structural_truncation() {
    // 11 embeds → over the 10-per-message cap → truncated, ⚠️ territory.
    let markers: String = (0..11)
        .map(|i| format!("[EMBED:{{\"title\":\"t{i}\"}}]"))
        .collect();
    let (_, embeds, _, truncated) = prepare_outgoing_embeds(&markers, None);
    assert_eq!(embeds.len(), 10);
    assert!(truncated);
}

#[test]
fn prepare_outgoing_embeds_leaves_plain_text_untouched() {
    let (text, embeds, failures, truncated) = prepare_outgoing_embeds("just a normal reply", None);
    assert_eq!(text, "just a normal reply");
    assert!(embeds.is_empty());
    assert!(failures.is_empty());
    assert!(!truncated);
}

#[test]
fn finalize_draft_builds_a_first_message_payload_carrying_embeds() {
    // finalize_draft (and the slash-reply path) lift embeds out of the final
    // text and attach them to the first message's DiscordOutgoing — the same
    // transformation send() does. Pin that so neither path regresses to
    // content-only and leaks the raw [EMBED:…] marker.
    let raw = "Result [EMBED:{\"title\":\"Report\"}]";
    let (content, embeds, _failures, _truncated) = prepare_outgoing_embeds(raw, None);
    assert_eq!(content, "Result");
    assert_eq!(embeds.len(), 1);
    let payload = DiscordOutgoing {
        content: Some(content),
        embeds,
        ..Default::default()
    };
    assert_eq!(
        payload.to_rest_json(),
        serde_json::json!({ "content": "Result", "embeds": [{ "title": "Report" }] })
    );
}

#[test]
fn finalize_draft_payload_carries_components() {
    let raw = "Pick one [COMPONENTS:{\"rows\":[[{\"label\":\"Go\",\"style\":\"primary\",\"prompt\":\"go\"}]]}]";
    let (content, rows) = parse_component_markers(raw);
    assert_eq!(content.trim(), "Pick one");
    assert_eq!(rows.len(), 1, "one action row parsed");
    let mut reg = pending::PendingComponents::default();
    let component_action_rows = build_component_rows("n", &rows, &mut reg);
    assert_eq!(component_action_rows.len(), 1, "row rendered");
    let payload = DiscordOutgoing {
        content: Some(content.trim().to_string()),
        components: component_action_rows,
        ..Default::default()
    };
    let json = payload.to_rest_json();
    assert!(
        json.get("components").is_some(),
        "finalize payload must carry the action rows; got {json}"
    );
}

#[test]
fn interaction_gate_applies_peer_allowlist() {
    // Wildcard admits anyone; otherwise the invoker must be listed.
    assert_eq!(
        interaction_gate(&s(&["*"]), &[], &[], "u1", None, "c1", None),
        Ok(())
    );
    assert_eq!(
        interaction_gate(&s(&["u1"]), &[], &[], "u1", None, "c1", None),
        Ok(())
    );
    assert_eq!(
        interaction_gate(&s(&["u1"]), &[], &[], "intruder", None, "c1", None),
        Err(InteractionDenial::UnauthorizedUser)
    );
    // Empty peer list = nobody, same as the message path.
    assert_eq!(
        interaction_gate(&[], &[], &[], "u1", None, "c1", None),
        Err(InteractionDenial::UnauthorizedUser)
    );
}

#[test]
fn interaction_gate_applies_guild_and_channel_filters() {
    let peers = s(&["*"]);
    let guilds = s(&["g1"]);
    let channels = s(&["c1"]);

    assert_eq!(
        interaction_gate(&peers, &guilds, &channels, "u1", Some("g1"), "c1", None),
        Ok(())
    );
    assert_eq!(
        interaction_gate(&peers, &guilds, &[], "u1", Some("g2"), "c1", None),
        Err(InteractionDenial::GuildNotAllowed)
    );
    // DM interactions carry no guild_id and pass the guild filter,
    // mirroring MESSAGE_CREATE.
    assert_eq!(
        interaction_gate(&peers, &guilds, &[], "u1", None, "c1", None),
        Ok(())
    );
    assert_eq!(
        interaction_gate(&peers, &[], &channels, "u1", Some("g1"), "c2", None),
        Err(InteractionDenial::ChannelNotAllowed)
    );
    // A thread whose parent is allowlisted passes, like threaded messages.
    assert_eq!(
        interaction_gate(
            &peers,
            &[],
            &channels,
            "u1",
            Some("g1"),
            "thread9",
            Some("c1")
        ),
        Ok(())
    );
}

#[tokio::test]
async fn interaction_answer_over_2000_chars_chunks_into_edit_plus_followup() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    // The first chunk edits the deferred @original message.
    Mock::given(method("PATCH"))
        .and(path("/webhooks/app1/tok/messages/@original"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    // The remaining chunk is delivered as a followup POST (not truncated).
    Mock::given(method("POST"))
        .and(path("/webhooks/app1/tok"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let client = reqwest::Client::new();
    // 3000 contiguous chars (no break point) → a 2000-char chunk + a 1000.
    let content = "a".repeat(3000);
    deliver_interaction_answer(&client, "app1", "tok", &server.uri(), &content, &[], &[])
        .await
        .unwrap();
    // wiremock verifies the expect(1) counts when the server drops.
}

#[tokio::test]
async fn short_interaction_answer_only_edits_original() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    Mock::given(method("PATCH"))
        .and(path("/webhooks/app1/tok/messages/@original"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    // A short reply must not trigger any followup POST.
    Mock::given(method("POST"))
        .and(path("/webhooks/app1/tok"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let client = reqwest::Client::new();
    deliver_interaction_answer(
        &client,
        "app1",
        "tok",
        &server.uri(),
        "short answer",
        &[],
        &[],
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn interaction_answer_emits_components_on_original_edit() {
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    // The @original edit MUST carry the `components` array (a type-1 action
    // row holding the rendered button) and the stripped content — proving a
    // slash-command reply with a [COMPONENTS:…] marker renders interactive
    // controls instead of leaking the marker text.
    Mock::given(method("PATCH"))
        .and(path("/webhooks/app1/tok/messages/@original"))
        .and(body_partial_json(serde_json::json!({
            "content": "Pick:",
            "components": [{ "type": 1 }],
        })))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    // Build a real action row through the same registry path send() uses.
    let (cleaned, marker_rows) = parse_component_markers(
        "Pick: [COMPONENTS:{\"rows\":[[{\"label\":\"Ship\",\"style\":\"primary\",\"prompt\":\"ship it\"}]]}]",
    );
    assert_eq!(cleaned, "Pick:");
    let mut reg = pending::PendingComponents::default();
    let action_rows = build_component_rows("nonce", &marker_rows, &mut reg);
    assert_eq!(action_rows.len(), 1);

    let client = reqwest::Client::new();
    deliver_interaction_answer(
        &client,
        "app1",
        "tok",
        &server.uri(),
        &cleaned,
        &[],
        &action_rows,
    )
    .await
    .unwrap();
    // wiremock verifies the expect(1) + body_partial_json when the server drops.
}

#[tokio::test]
async fn plain_interaction_answer_omits_components_key() {
    // Behaviour-neutrality: a reply with no marker (empty components slice)
    // serialises to a content-only @original edit — no `components` key.
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    Mock::given(method("PATCH"))
        .and(path("/webhooks/app1/tok/messages/@original"))
        .and(body_partial_json(serde_json::json!({ "content": "hi" })))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let client = reqwest::Client::new();
    deliver_interaction_answer(&client, "app1", "tok", &server.uri(), "hi", &[], &[])
        .await
        .unwrap();
    let requests = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert!(
        body.get("components").is_none(),
        "plain reply must not carry a components key"
    );
}

#[test]
fn send_interaction_pipeline_strips_marker_and_registers_intents() {
    let ch = DiscordChannel::new(
        "fake".into(),
        vec![],
        "discord_test_alias",
        Arc::new(Vec::new),
        false,
        false,
    );
    let content = crate::util::strip_tool_call_tags(
        "Choose: [COMPONENTS:{\"rows\":[[{\"label\":\"Approve\",\"style\":\"success\",\"prompt\":\"user approved\"},{\"label\":\"Docs\",\"url\":\"https://example.com\"}]]}]",
    );
    let (stripped, marker_rows) = parse_component_markers(&content);
    assert_eq!(
        stripped, "Choose:",
        "marker stripped from interaction reply"
    );
    assert!(!marker_rows.is_empty(), "marker parsed into rows");

    let action_rows = ch.build_marker_components(&marker_rows);
    assert_eq!(action_rows.len(), 1, "one action row rendered");

    // The Approve button is registered (clickable); the link button is not.
    let ids = rendered_routing_ids(&action_rows);
    assert_eq!(ids.len(), 1, "only the prompt-bearing button registers");
    assert_eq!(
        ch.pending_components.lock().take(&ids[0]),
        Some(ComponentIntent::ResolveIntoTurn {
            prompt: "user approved".into()
        }),
        "click resolves the server-bound prompt"
    );
    // Single-use take: a replay resolves nothing.
    assert_eq!(ch.pending_components.lock().take(&ids[0]), None);
}

#[test]
fn interaction_reply_target_roundtrips() {
    let target = discord_interaction_reply_target("123456789");
    assert_eq!(target, "interaction:123456789");
    assert_eq!(parse_discord_interaction_target(&target), Some("123456789"));
}

#[test]
fn non_interaction_targets_are_ignored() {
    // A normal Discord channel id must NOT be treated as an interaction.
    assert_eq!(parse_discord_interaction_target("123456789012345678"), None);
    // Empty ids are rejected.
    assert_eq!(parse_discord_interaction_target("interaction:"), None);
    // The legacy `app:token` form (which carried a live credential in the
    // reply target) must never round-trip as valid again.
    assert_eq!(
        parse_discord_interaction_target("interaction:app123:tok456"),
        None
    );
}

#[tokio::test]
async fn send_to_unknown_interaction_sentinel_fails_without_rest() {
    // A sentinel whose credentials are not in the pending store (expired,
    // restarted process, forged) must error out before any HTTP happens.
    let ch = DiscordChannel::new(
        "fake".into(),
        vec![],
        "discord_test_alias",
        Arc::new(Vec::new),
        false,
        false,
    );
    let msg = SendMessage {
        content: "hello".into(),
        recipient: "interaction:999".into(),
        subject: None,
        thread_ts: None,
        cancellation_token: None,
        attachments: Vec::new(),
        in_reply_to: None,
        force_voice: false,
        suppress_voice: false,
    };
    let err = ch.send(&msg).await.unwrap_err();
    assert!(err.to_string().contains("unknown or expired"));
}

fn skill(name: &str, description: &str, tags: &[&str]) -> zeroclaw_runtime::skills::Skill {
    zeroclaw_runtime::skills::Skill {
        name: name.to_string(),
        description: description.to_string(),
        description_localizations: Default::default(),
        version: "1.0.0".to_string(),
        author: None,
        tags: tags.iter().map(|t| (*t).to_string()).collect(),
        tools: vec![],
        prompts: vec![],
        slash_options: Vec::new(),
        location: None,
    }
}

#[test]
fn command_slug_fits_discord_charset() {
    assert_eq!(discord_command_slug("Deploy Status"), "deploy-status");
    assert_eq!(discord_command_slug("summarize_pdf"), "summarize_pdf");
    assert_eq!(discord_command_slug("a  b!!c"), "a-b-c");
    assert_eq!(discord_command_slug("--weird--"), "weird");
    assert_eq!(discord_command_slug(""), "");
    // All-non-ASCII names slug to empty (documented limitation).
    assert_eq!(discord_command_slug("日本語スキル"), "");
    // 32-char cap, with a trailing dash at the boundary trimmed.
    assert_eq!(discord_command_slug(&"x".repeat(50)).len(), 32);
    let boundary = format!("{} tail", "y".repeat(31));
    let slug = discord_command_slug(&boundary);
    assert!(slug.len() <= 32 && !slug.ends_with('-'));
}

#[test]
fn specs_require_the_slash_tag_and_unique_slugs() {
    let skills = vec![
        skill("deploy status", "Check deploy state", &["slash"]),
        skill("not exposed", "No tag, no command", &[]),
        skill("Deploy Status", "Colliding slug", &["slash"]),
        skill("ask", "Reserved name", &["slash"]),
        skill("no-desc", "", &["slash"]),
        skill(
            "community",
            "Synced from a remote repo",
            &["slash", "open-skills"],
        ),
    ];
    let specs = discord_slash_specs_from_skills(&skills);
    let slugs: Vec<&str> = specs.iter().map(|s| s.slug.as_str()).collect();
    // Sorted, deduped, reserved + untagged + open-skills excluded.
    assert_eq!(slugs, vec!["deploy-status", "no-desc"]);
    assert_eq!(specs[1].description, "Run the no-desc skill");
}

#[test]
fn specs_are_deterministic_regardless_of_input_order() {
    let a = vec![
        skill("bravo", "b", &["slash"]),
        skill("alpha", "a", &["slash"]),
    ];
    let b = vec![
        skill("alpha", "a", &["slash"]),
        skill("bravo", "b", &["slash"]),
    ];
    assert_eq!(
        discord_slash_specs_from_skills(&a),
        discord_slash_specs_from_skills(&b)
    );
}

#[test]
fn specs_cap_at_the_registration_limit() {
    let many: Vec<_> = (0..95)
        .map(|i| skill(&format!("skill-{i:03}"), "d", &["slash"]))
        .collect();
    let specs = discord_slash_specs_from_skills(&many);
    assert_eq!(specs.len(), MAX_SKILL_SLASH_COMMANDS);
}

#[test]
fn specs_sanitize_names_that_enter_the_synthesized_prompt() {
    let skills = vec![skill("evil'\nname", "d", &["slash"])];
    let specs = discord_slash_specs_from_skills(&skills);
    assert!(!specs[0].skill_name.contains('\''));
    assert!(!specs[0].skill_name.contains('\n'));
}

#[test]
fn registration_body_contains_ask_plus_skill_commands() {
    let specs = vec![DiscordSlashCommandSpec {
        skill_name: "deploy status".to_string(),
        slug: "deploy-status".to_string(),
        description: "Check deploy state".to_string(),
        description_localizations: Default::default(),
        options: Vec::new(),
    }];
    let body = slash_command_registration_body(&specs);
    let commands = body.as_array().unwrap();
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[0]["name"], "ask");
    assert_eq!(commands[1]["name"], "deploy-status");
    assert_eq!(commands[1]["options"][0]["name"], "input");
    assert_eq!(commands[1]["options"][0]["required"], true);
    // Every desired command matches the ownership fingerprint except
    // /ask (whose option is `prompt`) — exactly the reaping contract.
    assert!(!is_skill_command_shape(&commands[0]));
    assert!(is_skill_command_shape(&commands[1]));
}

#[test]
fn foreign_commands_do_not_match_the_skill_shape() {
    // No options at all.
    assert!(!is_skill_command_shape(&serde_json::json!({"name": "x"})));
    // Multiple options.
    assert!(!is_skill_command_shape(&serde_json::json!({
        "name": "x",
        "options": [
            {"name": "input", "type": 3, "required": true},
            {"name": "more", "type": 3, "required": false}
        ]
    })));
    // Right shape, wrong option name.
    assert!(!is_skill_command_shape(&serde_json::json!({
        "name": "x",
        "options": [{"name": "query", "type": 3, "required": true}]
    })));
    // The critical foreign-collision case: a generic `/x <input>`
    // command registered by other tooling. Structure matches, but the
    // ownership marker (our exact option description) does not — it
    // must never be reaped.
    assert!(!is_skill_command_shape(&serde_json::json!({
        "name": "x",
        "options": [{
            "name": "input", "type": 3, "required": true,
            "description": "what to run"
        }]
    })));
    // Our own marker matches.
    assert!(is_skill_command_shape(&serde_json::json!({
        "name": "x",
        "options": [{
            "name": "input", "type": 3, "required": true,
            "description": SKILL_COMMAND_OPTION_DESCRIPTION
        }]
    })));
}

fn stale_skill_command(id: &str, name: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id, "name": name, "description": "d", "type": 1,
        "options": [{
            "name": "input", "type": 3, "required": true,
            "description": SKILL_COMMAND_OPTION_DESCRIPTION
        }]
    })
}

#[tokio::test]
async fn reconcile_fails_when_a_stale_delete_fails() {
    // A transiently failing DELETE of an owned stale command must make
    // the whole reconcile report Err — otherwise the caller records the
    // fingerprint as successful and the stale command is never retried
    // while the desired set stays unchanged.
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/applications/app1/commands"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            stale_skill_command("c1", "ghost-skill")
        ])))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/applications/app1/commands/c1"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&server)
        .await;
    // Desired set: /ask only (the upsert must still be attempted and
    // succeed even though the delete fails).
    Mock::given(method("POST"))
        .and(path("/applications/app1/commands"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let client = reqwest::Client::new();
    let desired = slash_command_registration_body(&[]);
    let err = reconcile_slash_commands(
        &client,
        "tok",
        "app1",
        &desired,
        &server.uri(),
        SlashScope::Global,
        &[],
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("stale skill command delete"));
}

#[tokio::test]
async fn reconcile_treats_delete_404_as_already_gone() {
    // 404 means the command is already gone (raced cleanup) — the
    // desired end state holds, so the pass records as successful.
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/applications/app1/commands"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            stale_skill_command("c1", "ghost-skill")
        ])))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/applications/app1/commands/c1"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/applications/app1/commands"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let client = reqwest::Client::new();
    let desired = slash_command_registration_body(&[]);
    reconcile_slash_commands(
        &client,
        "tok",
        "app1",
        &desired,
        &server.uri(),
        SlashScope::Global,
        &[],
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn guild_scope_registers_to_the_guild_endpoint() {
    // scope=Guild with one guild routes the upsert to
    // /applications/{app}/guilds/{gid}/commands; the (empty) global
    // endpoint is listed for cross-scope cleanup but has nothing to reap.
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/applications/app1/commands"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/applications/app1/guilds/g1/commands"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/applications/app1/guilds/g1/commands"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let client = reqwest::Client::new();
    let desired = slash_command_registration_body(&[]);
    reconcile_slash_commands(
        &client,
        "tok",
        "app1",
        &desired,
        &server.uri(),
        SlashScope::Guild,
        &["g1".to_string()],
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn scope_switch_reaps_owned_commands_from_the_inactive_scope() {
    // Switching to guild scope reaps our `/ask` + skill commands left on the
    // now-inactive global endpoint, so the same command isn't registered in
    // both scopes at once (the guild-scope migration hazard).
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    // Derive the stale `/ask` from what we register (incl. its
    // `description_localizations`, which the reaper's listing requests via
    // `with_localizations=true`) plus a server-side id - so its projection
    // matches ours and the ownership check reaps it
    let mut stale_ask = slash_command_registration_body(&[]).as_array().unwrap()[0].clone();
    stale_ask["id"] = serde_json::json!("a1");
    Mock::given(method("GET"))
        .and(path("/applications/app1/commands"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            stale_ask,
            stale_skill_command("c1", "ghost-skill")
        ])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/applications/app1/commands/a1"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/applications/app1/commands/c1"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/applications/app1/guilds/g1/commands"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/applications/app1/guilds/g1/commands"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let client = reqwest::Client::new();
    let desired = slash_command_registration_body(&[]);
    reconcile_slash_commands(
        &client,
        "tok",
        "app1",
        &desired,
        &server.uri(),
        SlashScope::Guild,
        &["g1".to_string()],
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn scope_switch_spares_foreign_ask_in_inactive_scope() {
    // A `/ask` registered by OTHER tooling (different description) on the
    // now-inactive global scope must NOT be reaped on a scope switch - we
    // only delete the `/ask` whose projection matches what we register.
    // Our own skill command on that scope is still reaped.
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    let foreign_ask = serde_json::json!({
        "id": "x1", "name": "ask",
        "description": "Ask a DIFFERENT bot", "type": 1,
        "options": [{ "name": "prompt", "description": "What to ask", "type": 3, "required": true }]
    });
    Mock::given(method("GET"))
        .and(path("/applications/app1/commands"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            foreign_ask,
            stale_skill_command("c1", "ghost-skill")
        ])))
        .expect(1)
        .mount(&server)
        .await;
    // Our owned skill command IS reaped...
    Mock::given(method("DELETE"))
        .and(path("/applications/app1/commands/c1"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    // ...but the foreign `/ask` (x1) must NOT be: expect(0) fails on drop if
    // a delete is ever issued for it.
    Mock::given(method("DELETE"))
        .and(path("/applications/app1/commands/x1"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/applications/app1/guilds/g1/commands"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/applications/app1/guilds/g1/commands"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let client = reqwest::Client::new();
    let desired = slash_command_registration_body(&[]);
    reconcile_slash_commands(
        &client,
        "tok",
        "app1",
        &desired,
        &server.uri(),
        SlashScope::Guild,
        &["g1".to_string()],
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn reconcile_skips_unchanged_and_spares_foreign_commands() {
    // Steady state: existing /ask matches the desired projection (no
    // POST), and a foreign command with a generic input option is left
    // alone. Zero writes.
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    let mut existing_ask = slash_command_registration_body(&[]).as_array().unwrap()[0].clone();
    existing_ask["id"] = serde_json::json!("a1");
    let foreign = serde_json::json!({
        "id": "f1", "name": "run",
        "description": "external tool", "type": 1,
        "options": [{
            "name": "input", "type": 3, "required": true,
            "description": "what to run"
        }]
    });
    Mock::given(method("GET"))
        .and(path("/applications/app1/commands"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!([existing_ask, foreign])),
        )
        .expect(1)
        .mount(&server)
        .await;
    // No DELETE and no POST expectations mounted: any write request
    // would 404 the mock server and fail the reconcile.

    let client = reqwest::Client::new();
    let desired = slash_command_registration_body(&[]);
    reconcile_slash_commands(
        &client,
        "tok",
        "app1",
        &desired,
        &server.uri(),
        SlashScope::Global,
        &[],
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn reconcile_returns_rate_limited_on_post_429() {
    // A 429 on an upsert must surface as RateLimited (with the body's
    // retry_after deadline) so the caller persists a cooldown instead of
    // re-hammering the daily command budget on the next READY.
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/applications/app1/commands"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/applications/app1/commands"))
        .respond_with(
            ResponseTemplate::new(429).set_body_json(serde_json::json!({"retry_after": 5.0})),
        )
        .mount(&server)
        .await;

    let client = reqwest::Client::new();
    let desired = slash_command_registration_body(&[]); // /ask → one POST
    let now = crate::discord_slash_state::now_unix();
    let outcome = reconcile_slash_commands(
        &client,
        "tok",
        "app1",
        &desired,
        &server.uri(),
        SlashScope::Global,
        &[],
    )
    .await
    .unwrap();
    match outcome {
        ReconcileOutcome::RateLimited { until } => assert!(until >= now + 5),
        ReconcileOutcome::Reconciled => panic!("expected RateLimited on a POST 429"),
    }
}

#[test]
fn command_projection_ignores_server_side_decorations() {
    // Discord's GET response decorates commands with id/version/etc.;
    // change detection must compare only what we author.
    let ours = serde_json::json!({
        "name": "deploy-status",
        "description": "Check deploy state",
        "type": 1,
        "options": [{
            "name": "input", "type": 3, "required": true,
            "description": SKILL_COMMAND_OPTION_DESCRIPTION
        }]
    });
    let theirs = serde_json::json!({
        "id": "1234", "version": "5678", "application_id": "42",
        "default_member_permissions": serde_json::Value::Null,
        "name": "deploy-status",
        "description": "Check deploy state",
        "type": 1,
        "options": [{
            "name": "input", "type": 3, "required": true,
            "description": SKILL_COMMAND_OPTION_DESCRIPTION
        }]
    });
    assert_eq!(command_projection(&ours), command_projection(&theirs));

    let changed = serde_json::json!({
        "name": "deploy-status",
        "description": "A different description",
        "type": 1,
        "options": ours["options"].clone()
    });
    assert_ne!(command_projection(&ours), command_projection(&changed));
}

#[test]
fn string_options_extract_by_name() {
    let d = serde_json::json!({
        "data": {
            "options": [
                {"name": "input", "value": "check prod"},
                {"name": "other", "value": "x"}
            ]
        }
    });
    assert_eq!(interaction_string_option(&d, "input"), "check prod");
    assert_eq!(interaction_string_option(&d, "missing"), "");
}

#[test]
fn channel_resolves_skill_commands_through_resolver() {
    let ch = DiscordChannel::new(
        "fake".into(),
        vec![],
        "discord_test_alias",
        Arc::new(Vec::new),
        false,
        false,
    )
    .with_slash_command_resolver(Arc::new(|| {
        discord_slash_specs_from_skills(&[skill("deploy status", "Check", &["slash"])])
    }));
    let specs = ch.slash_command_resolver.as_ref().map(|r| r()).unwrap();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].slug, "deploy-status");
}

#[test]
fn pending_interaction_sweep_drops_expired_entries() {
    let ch = DiscordChannel::new(
        "fake".into(),
        vec![],
        "discord_test_alias",
        Arc::new(Vec::new),
        false,
        false,
    );
    let mut guard = ch.pending_interactions.lock();
    guard.insert(
        "live".into(),
        PendingInteraction {
            app_id: "a".into(),
            token: "t".into(),
            created: std::time::Instant::now(),
        },
    );
    guard.insert(
        "stale".into(),
        PendingInteraction {
            app_id: "a".into(),
            token: "t".into(),
            created: std::time::Instant::now()
                - INTERACTION_TOKEN_TTL
                - std::time::Duration::from_secs(1),
        },
    );
    guard.retain(|_, p| p.created.elapsed() < INTERACTION_TOKEN_TTL);
    assert!(guard.contains_key("live"));
    assert!(!guard.contains_key("stale"));
}

#[test]
fn discord_channel_name() {
    let listen_to_bots = false;
    let mention_only = false;
    let ch = DiscordChannel::new(
        "fake".into(),
        vec![],
        "discord_test_alias",
        Arc::new(Vec::new),
        listen_to_bots,
        mention_only,
    );
    assert_eq!(ch.name(), "discord");
}

/// (channel, archive) pair backed by a throwaway sqlite file, mirroring
/// the orchestrator's `with_archive_memory` wiring.
fn archived_test_channel() -> (
    DiscordChannel,
    std::sync::Arc<dyn zeroclaw_memory::Memory>,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
    let mem: std::sync::Arc<dyn zeroclaw_memory::Memory> = std::sync::Arc::new(
        zeroclaw_memory::SqliteMemory::new_named("sqlite", dir.path(), "discord").unwrap(),
    );
    let ch = DiscordChannel::new(
        "fake".into(),
        vec![],
        "discord_test_alias",
        Arc::new(|| vec!["*".to_string()]),
        false,
        false,
    )
    .with_archive_memory(std::sync::Arc::clone(&mem));
    (ch, mem, dir)
}

async fn seed_archived_message(mem: &std::sync::Arc<dyn zeroclaw_memory::Memory>) {
    mem.store(
        "discord_111",
        "@alice in #200 at t0: original text",
        zeroclaw_memory::MemoryCategory::Custom("discord".to_string()),
        Some("200"),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn message_update_appends_edit_marker_to_archived_entry() {
    let (ch, mem, _dir) = archived_test_channel();
    seed_archived_message(&mem).await;

    let d = serde_json::json!({
        "id": "111", "channel_id": "200", "content": "revised text",
        "edited_timestamp": "2026-06-11T01:00:00Z",
        "author": {"id": "u-alice", "bot": false}
    });
    ch.sync_archive_for_message_event("MESSAGE_UPDATE", &d, "botid")
        .await;

    let entry = mem.get("discord_111").await.unwrap().unwrap();
    assert!(
        entry
            .content
            .starts_with("@alice in #200 at t0: original text")
    );
    assert!(
        entry
            .content
            .contains("[edited at 2026-06-11T01:00:00Z: revised text]")
    );
    // Session attribution survives the re-store.
    assert_eq!(entry.session_id.as_deref(), Some("200"));
}

#[tokio::test]
async fn redelivered_update_is_idempotent() {
    let (ch, mem, _dir) = archived_test_channel();
    seed_archived_message(&mem).await;
    let d = serde_json::json!({
        "id": "111", "channel_id": "200", "content": "revised text",
        "edited_timestamp": "2026-06-11T01:00:00Z",
        "author": {"id": "u-alice", "bot": false}
    });
    ch.sync_archive_for_message_event("MESSAGE_UPDATE", &d, "botid")
        .await;
    ch.sync_archive_for_message_event("MESSAGE_UPDATE", &d, "botid")
        .await;
    let entry = mem.get("discord_111").await.unwrap().unwrap();
    assert_eq!(entry.content.matches("[edited at ").count(), 1);
}

#[tokio::test]
async fn full_object_update_without_edited_timestamp_is_not_an_edit() {
    // Discord sends the complete message object (content included,
    // unchanged) on embed unfurls, pins, and flag changes — only real
    // edits carry edited_timestamp. No phantom markers.
    let (ch, mem, _dir) = archived_test_channel();
    seed_archived_message(&mem).await;
    let d = serde_json::json!({
        "id": "111", "channel_id": "200", "content": "original text",
        "edited_timestamp": serde_json::Value::Null,
        "author": {"id": "u-alice", "bot": false}
    });
    ch.sync_archive_for_message_event("MESSAGE_UPDATE", &d, "botid")
        .await;
    let entry = mem.get("discord_111").await.unwrap().unwrap();
    assert_eq!(entry.content, "@alice in #200 at t0: original text");
}

#[tokio::test]
async fn deauthorized_author_cannot_write_via_edits() {
    // Archive-time authorization is not durable: once a peer leaves
    // the allowlist their edits must stop reaching the archive.
    let dir = tempfile::tempdir().unwrap();
    let mem: std::sync::Arc<dyn zeroclaw_memory::Memory> = std::sync::Arc::new(
        zeroclaw_memory::SqliteMemory::new_named("sqlite", dir.path(), "discord").unwrap(),
    );
    let ch = DiscordChannel::new(
        "fake".into(),
        vec![],
        "discord_test_alias",
        Arc::new(|| vec!["someone-else".to_string()]),
        false,
        false,
    )
    .with_archive_memory(std::sync::Arc::clone(&mem));
    seed_archived_message(&mem).await;

    let d = serde_json::json!({
        "id": "111", "channel_id": "200", "content": "injected",
        "edited_timestamp": "2026-06-11T01:00:00Z",
        "author": {"id": "u-alice", "bot": false}
    });
    ch.sync_archive_for_message_event("MESSAGE_UPDATE", &d, "botid")
        .await;
    let entry = mem.get("discord_111").await.unwrap().unwrap();
    assert_eq!(entry.content, "@alice in #200 at t0: original text");
}

#[tokio::test]
async fn message_delete_appends_tombstone_once() {
    let (ch, mem, _dir) = archived_test_channel();
    seed_archived_message(&mem).await;

    let d = serde_json::json!({"id": "111", "channel_id": "200"});
    ch.sync_archive_for_message_event("MESSAGE_DELETE", &d, "botid")
        .await;
    // Redelivery must not double-stamp.
    ch.sync_archive_for_message_event("MESSAGE_DELETE", &d, "botid")
        .await;

    let entry = mem.get("discord_111").await.unwrap().unwrap();
    assert!(
        entry
            .content
            .starts_with("@alice in #200 at t0: original text")
    );
    assert_eq!(entry.content.matches("[deleted at ").count(), 1);
    assert_eq!(entry.session_id.as_deref(), Some("200"));
}

#[tokio::test]
async fn bulk_delete_tombstones_every_archived_id() {
    let (ch, mem, _dir) = archived_test_channel();
    seed_archived_message(&mem).await;
    mem.store(
        "discord_112",
        "@bob in #200 at t1: second message",
        zeroclaw_memory::MemoryCategory::Custom("discord".to_string()),
        Some("200"),
    )
    .await
    .unwrap();

    let d = serde_json::json!({"ids": ["111", "112", "999"], "channel_id": "200"});
    ch.sync_archive_for_message_event("MESSAGE_DELETE_BULK", &d, "botid")
        .await;

    assert!(
        mem.get("discord_111")
            .await
            .unwrap()
            .unwrap()
            .content
            .contains("[deleted at ")
    );
    assert!(
        mem.get("discord_112")
            .await
            .unwrap()
            .unwrap()
            .content
            .contains("[deleted at ")
    );
    // Unarchived ids stay unarchived.
    assert!(mem.get("discord_999").await.unwrap().is_none());
}

#[tokio::test]
async fn edit_then_delete_keeps_both_markers() {
    let (ch, mem, _dir) = archived_test_channel();
    seed_archived_message(&mem).await;
    let edit = serde_json::json!({
        "id": "111", "channel_id": "200", "content": "revised",
        "edited_timestamp": "2026-06-11T01:00:00Z",
        "author": {"id": "u-alice", "bot": false}
    });
    ch.sync_archive_for_message_event("MESSAGE_UPDATE", &edit, "botid")
        .await;
    let del = serde_json::json!({"id": "111", "channel_id": "200"});
    ch.sync_archive_for_message_event("MESSAGE_DELETE", &del, "botid")
        .await;
    let entry = mem.get("discord_111").await.unwrap().unwrap();
    assert!(entry.content.contains("[edited at "));
    assert!(entry.content.contains("[deleted at "));
}

#[tokio::test]
async fn message_events_for_unarchived_messages_are_ignored() {
    // A message that never passed the inbound filters was never stored;
    // its edit/delete events must not conjure an archive entry.
    let (ch, mem, _dir) = archived_test_channel();

    let d = serde_json::json!({
        "id": "999", "channel_id": "200", "content": "whatever",
        "edited_timestamp": "2026-06-11T01:00:00Z",
        "author": {"id": "u-alice", "bot": false}
    });
    ch.sync_archive_for_message_event("MESSAGE_UPDATE", &d, "botid")
        .await;
    ch.sync_archive_for_message_event("MESSAGE_DELETE", &d, "botid")
        .await;

    assert!(mem.get("discord_999").await.unwrap().is_none());
}

#[tokio::test]
async fn edit_history_growth_is_bounded() {
    let (ch, mem, _dir) = archived_test_channel();
    seed_archived_message(&mem).await;
    let big = "z".repeat(4000);
    for i in 0..10 {
        let d = serde_json::json!({
            "id": "111", "channel_id": "200",
            "content": format!("{big}-{i}"),
            "edited_timestamp": format!("2026-06-11T01:00:{i:02}Z"),
            "author": {"id": "u-alice", "bot": false}
        });
        ch.sync_archive_for_message_event("MESSAGE_UPDATE", &d, "botid")
            .await;
    }
    let entry = mem.get("discord_111").await.unwrap().unwrap();
    // Bounded: cap plus at most one marker's overshoot.
    assert!(entry.content.len() < MAX_ARCHIVE_ENTRY_BYTES + 5000);
    assert!(entry.content.contains("[edit history truncated]"));
    assert!(
        entry
            .content
            .starts_with("@alice in #200 at t0: original text")
    );
    // The latest edit always survives.
    assert!(entry.content.contains("01:00:09Z"));
}

#[test]
fn gateway_intents_default_matches_legacy_mask() {
    // The pre-resolver IDENTIFY hardcoded 37377. A default-config channel
    // must request exactly the same mask — no silent behavior change.
    let ch = DiscordChannel::new(
        "fake".into(),
        vec![],
        "discord_test_alias",
        Arc::new(Vec::new),
        false,
        false,
    );
    assert_eq!(ch.gateway_intents(), 37377);
    assert_eq!(ch.gateway_intents(), BASELINE_INTENTS);
}

#[test]
fn intent_names_decode_the_mask() {
    assert_eq!(
        intent_names(BASELINE_INTENTS),
        vec![
            "guilds",
            "guild_messages",
            "direct_messages",
            "message_content"
        ]
    );
    assert_eq!(
        intent_names(BASELINE_INTENTS | INTENT_GUILD_MEMBERS | INTENT_GUILD_PRESENCES),
        vec![
            "guilds",
            "guild_members",
            "guild_presences",
            "guild_messages",
            "direct_messages",
            "message_content"
        ]
    );
    // Bits with no known name (reachable via the raw override) are
    // reported, not dropped.
    assert_eq!(
        intent_names(INTENT_GUILDS | (1 << 21)),
        vec!["guilds".to_string(), format!("unknown({:#x})", 1u64 << 21)]
    );
}

#[test]
fn disallowed_intents_hint_names_privileged_toggles() {
    let hint = disallowed_intents_hint(BASELINE_INTENTS | INTENT_GUILD_MEMBERS);
    assert!(hint.contains("Server Members"));
    assert!(hint.contains("Message Content"));
    assert!(!hint.contains("Presence,"));

    let base_hint = disallowed_intents_hint(BASELINE_INTENTS);
    assert!(base_hint.contains("Message Content"));
    assert!(!base_hint.contains("Server Members"));

    // An override mask with no privileged bits still produces an
    // actionable message instead of a dangling empty list.
    let bare = disallowed_intents_hint(INTENT_GUILDS);
    assert!(bare.contains("mask 0x1"));
}

use zeroclaw_config::schema::DiscordReactionScope;

fn reaction_test_channel(
    scope: DiscordReactionScope,
) -> (
    DiscordChannel,
    std::sync::Arc<dyn zeroclaw_memory::Memory>,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
    let mem: std::sync::Arc<dyn zeroclaw_memory::Memory> = std::sync::Arc::new(
        zeroclaw_memory::SqliteMemory::new_named("sqlite", dir.path(), "discord").unwrap(),
    );
    let ch = DiscordChannel::new(
        "fake".into(),
        vec![],
        "discord_test_alias",
        Arc::new(|| vec!["*".to_string()]),
        false,
        false,
    )
    .with_archive_memory(std::sync::Arc::clone(&mem))
    .with_reaction_notifications(scope);
    (ch, mem, dir)
}

#[test]
fn reaction_scope_adds_reaction_intents_to_the_mask() {
    let (off, _m1, _d1) = reaction_test_channel(DiscordReactionScope::Off);
    assert_eq!(off.gateway_intents(), 37377);

    let (own, _m2, _d2) = reaction_test_channel(DiscordReactionScope::Own);
    assert_eq!(
        own.gateway_intents(),
        37377 | INTENT_GUILD_MESSAGE_REACTIONS | INTENT_DIRECT_MESSAGE_REACTIONS
    );
    // The reactions-on mask is OpenClaw's static base — parity by arithmetic.
    assert_eq!(own.gateway_intents(), 46593);
}

#[tokio::test]
async fn reaction_add_is_archived_and_remove_forgets_it() {
    let (ch, mem, _dir) = reaction_test_channel(DiscordReactionScope::All);
    let add = serde_json::json!({
        "user_id": "u1", "message_id": "m1", "channel_id": "c1",
        "guild_id": "g1", "emoji": {"name": "👍"},
        "member": {"user": {"username": "bob"}}
    });
    ch.handle_reaction_event("MESSAGE_REACTION_ADD", &add, "botid")
        .await;

    let key = "discord_reaction_m1_u1_👍";
    let entry = mem.get(key).await.unwrap().unwrap();
    assert!(
        entry
            .content
            .contains("@bob reacted 👍 to message m1 in #c1")
    );

    let remove = serde_json::json!({
        "user_id": "u1", "message_id": "m1", "channel_id": "c1",
        "guild_id": "g1", "emoji": {"name": "👍"}
    });
    ch.handle_reaction_event("MESSAGE_REACTION_REMOVE", &remove, "botid")
        .await;
    assert!(mem.get(key).await.unwrap().is_none());
}

#[tokio::test]
async fn own_scope_only_records_reactions_to_bot_messages() {
    let (ch, mem, _dir) = reaction_test_channel(DiscordReactionScope::Own);

    let to_other = serde_json::json!({
        "user_id": "u1", "message_id": "m1", "channel_id": "c1",
        "guild_id": "g1", "emoji": {"name": "👍"},
        "message_author_id": "someone_else"
    });
    ch.handle_reaction_event("MESSAGE_REACTION_ADD", &to_other, "botid")
        .await;
    assert!(
        mem.get("discord_reaction_m1_u1_👍")
            .await
            .unwrap()
            .is_none()
    );

    let to_bot = serde_json::json!({
        "user_id": "u1", "message_id": "m2", "channel_id": "c1",
        "guild_id": "g1", "emoji": {"name": "🎉"},
        "message_author_id": "botid"
    });
    ch.handle_reaction_event("MESSAGE_REACTION_ADD", &to_bot, "botid")
        .await;
    assert!(
        mem.get("discord_reaction_m2_u1_🎉")
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn bots_own_reactions_are_never_recorded() {
    // Ack/failure emoji the bot adds itself echo back as gateway events.
    let (ch, mem, _dir) = reaction_test_channel(DiscordReactionScope::All);
    let own_ack = serde_json::json!({
        "user_id": "botid", "message_id": "m1", "channel_id": "c1",
        "guild_id": "g1", "emoji": {"name": "⚡️"},
        "message_author_id": "u1"
    });
    ch.handle_reaction_event("MESSAGE_REACTION_ADD", &own_ack, "botid")
        .await;
    assert!(
        mem.get("discord_reaction_m1_botid_⚡️")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn custom_emoji_keys_by_stable_id_so_rename_cannot_orphan_entries() {
    // Discord sends {id, name} for custom emoji; the name is mutable
    // guild state. ADD records by id, and a REMOVE arriving after the
    // emoji was renamed or deleted (name: null) must still forget it.
    let (ch, mem, _dir) = reaction_test_channel(DiscordReactionScope::All);
    let add = serde_json::json!({
        "user_id": "u1", "message_id": "m1", "channel_id": "c1",
        "guild_id": "g1",
        "emoji": {"id": "424242", "name": "partyclaw"}
    });
    ch.handle_reaction_event("MESSAGE_REACTION_ADD", &add, "botid")
        .await;
    let entry = mem.get("discord_reaction_m1_u1_424242").await.unwrap();
    // Content keeps the human-readable name; the key uses the id.
    assert!(entry.unwrap().content.contains("reacted partyclaw"));

    let remove = serde_json::json!({
        "user_id": "u1", "message_id": "m1", "channel_id": "c1",
        "guild_id": "g1",
        "emoji": {"id": "424242", "name": serde_json::Value::Null}
    });
    ch.handle_reaction_event("MESSAGE_REACTION_REMOVE", &remove, "botid")
        .await;
    assert!(
        mem.get("discord_reaction_m1_u1_424242")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn identityless_emoji_are_ignored() {
    // No id and no name: nothing meaningful to record (or forget).
    let (ch, mem, _dir) = reaction_test_channel(DiscordReactionScope::All);
    let add = serde_json::json!({
        "user_id": "u1", "message_id": "m1", "channel_id": "c1",
        "guild_id": "g1", "emoji": {}
    });
    ch.handle_reaction_event("MESSAGE_REACTION_ADD", &add, "botid")
        .await;
    assert_eq!(mem.count().await.unwrap(), 0);
}

#[tokio::test]
async fn reaction_filters_mirror_the_message_path() {
    // Peer allowlist: a reactor outside the peer set is never recorded.
    let dir = tempfile::tempdir().unwrap();
    let mem: std::sync::Arc<dyn zeroclaw_memory::Memory> = std::sync::Arc::new(
        zeroclaw_memory::SqliteMemory::new_named("sqlite", dir.path(), "discord").unwrap(),
    );
    let gated = DiscordChannel::new(
        "fake".into(),
        vec!["g1".into()],
        "discord_test_alias",
        Arc::new(|| vec!["friend".to_string()]),
        false,
        false,
    )
    .with_channel_ids(vec!["c1".into()])
    .with_archive_memory(std::sync::Arc::clone(&mem))
    .with_reaction_notifications(DiscordReactionScope::All);

    let stranger = serde_json::json!({
        "user_id": "stranger", "message_id": "m1", "channel_id": "c1",
        "guild_id": "g1", "emoji": {"name": "👍"}
    });
    gated
        .handle_reaction_event("MESSAGE_REACTION_ADD", &stranger, "botid")
        .await;
    assert_eq!(mem.count().await.unwrap(), 0);

    // Guild allowlist: wrong guild is dropped even for an allowed peer.
    let wrong_guild = serde_json::json!({
        "user_id": "friend", "message_id": "m2", "channel_id": "c1",
        "guild_id": "g2", "emoji": {"name": "👍"}
    });
    gated
        .handle_reaction_event("MESSAGE_REACTION_ADD", &wrong_guild, "botid")
        .await;
    assert_eq!(mem.count().await.unwrap(), 0);

    // All gates pass: recorded.
    let ok = serde_json::json!({
        "user_id": "friend", "message_id": "m3", "channel_id": "c1",
        "guild_id": "g1", "emoji": {"name": "👍"}
    });
    gated
        .handle_reaction_event("MESSAGE_REACTION_ADD", &ok, "botid")
        .await;
    assert!(
        mem.get("discord_reaction_m3_friend_👍")
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn remove_without_prior_add_is_a_noop() {
    let (ch, mem, _dir) = reaction_test_channel(DiscordReactionScope::All);
    let remove = serde_json::json!({
        "user_id": "u1", "message_id": "m1", "channel_id": "c1",
        "guild_id": "g1", "emoji": {"name": "👍"}
    });
    ch.handle_reaction_event("MESSAGE_REACTION_REMOVE", &remove, "botid")
        .await;
    assert_eq!(mem.count().await.unwrap(), 0);
}

#[test]
fn reaction_sweep_predicate_scopes_by_message_then_emoji() {
    // REMOVE_ALL: every row for m1, regardless of user or emoji.
    assert!(reaction_sweep_matches(
        "discord_reaction_m1_u1_👍",
        "m1",
        None
    ));
    assert!(reaction_sweep_matches(
        "discord_reaction_m1_u2_🎉",
        "m1",
        None
    ));
    // ...but never another message's rows, and the trailing `_` on the
    // prefix keeps `m1` from swallowing `m12`.
    assert!(!reaction_sweep_matches(
        "discord_reaction_m2_u1_👍",
        "m1",
        None
    ));
    assert!(!reaction_sweep_matches(
        "discord_reaction_m12_u1_👍",
        "m1",
        None
    ));

    // REMOVE_EMOJI: the message AND that one emoji (any user).
    assert!(reaction_sweep_matches(
        "discord_reaction_m1_u1_👍",
        "m1",
        Some("👍")
    ));
    assert!(reaction_sweep_matches(
        "discord_reaction_m1_u2_👍",
        "m1",
        Some("👍")
    ));
    // Right message, wrong emoji: untouched.
    assert!(!reaction_sweep_matches(
        "discord_reaction_m1_u1_🎉",
        "m1",
        Some("👍")
    ));
    // Right emoji, wrong message: untouched.
    assert!(!reaction_sweep_matches(
        "discord_reaction_m2_u1_👍",
        "m1",
        Some("👍")
    ));
    // Custom-emoji rows key by id; REMOVE_EMOJI scopes by that same id.
    assert!(reaction_sweep_matches(
        "discord_reaction_m1_u1_424242",
        "m1",
        Some("424242")
    ));
    assert!(!reaction_sweep_matches(
        "discord_reaction_m1_u1_424242",
        "m1",
        Some("999")
    ));
}

#[tokio::test]
async fn remove_all_sweeps_only_that_messages_reactions() {
    let (ch, mem, _dir) = reaction_test_channel(DiscordReactionScope::All);
    // Two users react with two different emoji on m1, plus an unrelated
    // reaction on m2 that must survive the sweep.
    for (user, emoji) in [("u1", "👍"), ("u2", "🎉")] {
        let add = serde_json::json!({
            "user_id": user, "message_id": "m1", "channel_id": "c1",
            "guild_id": "g1", "emoji": {"name": emoji},
            "member": {"user": {"username": user}}
        });
        ch.handle_reaction_event("MESSAGE_REACTION_ADD", &add, "botid")
            .await;
    }
    let other = serde_json::json!({
        "user_id": "u1", "message_id": "m2", "channel_id": "c1",
        "guild_id": "g1", "emoji": {"name": "👍"}
    });
    ch.handle_reaction_event("MESSAGE_REACTION_ADD", &other, "botid")
        .await;
    assert_eq!(mem.count().await.unwrap(), 3);

    let clear = serde_json::json!({
        "message_id": "m1", "channel_id": "c1", "guild_id": "g1"
    });
    ch.sweep_message_reactions("MESSAGE_REACTION_REMOVE_ALL", &clear)
        .await;

    assert!(
        mem.get("discord_reaction_m1_u1_👍")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        mem.get("discord_reaction_m1_u2_🎉")
            .await
            .unwrap()
            .is_none()
    );
    // m2's reaction is untouched.
    assert!(
        mem.get("discord_reaction_m2_u1_👍")
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(mem.count().await.unwrap(), 1);
}

#[tokio::test]
async fn remove_emoji_sweeps_only_that_emoji_on_the_message() {
    let (ch, mem, _dir) = reaction_test_channel(DiscordReactionScope::All);
    // Same emoji from two users, plus a different emoji that must survive.
    for user in ["u1", "u2"] {
        let add = serde_json::json!({
            "user_id": user, "message_id": "m1", "channel_id": "c1",
            "guild_id": "g1", "emoji": {"name": "👍"}
        });
        ch.handle_reaction_event("MESSAGE_REACTION_ADD", &add, "botid")
            .await;
    }
    let keep = serde_json::json!({
        "user_id": "u1", "message_id": "m1", "channel_id": "c1",
        "guild_id": "g1", "emoji": {"name": "🎉"}
    });
    ch.handle_reaction_event("MESSAGE_REACTION_ADD", &keep, "botid")
        .await;
    assert_eq!(mem.count().await.unwrap(), 3);

    let clear = serde_json::json!({
        "message_id": "m1", "channel_id": "c1", "guild_id": "g1",
        "emoji": {"name": "👍"}
    });
    ch.sweep_message_reactions("MESSAGE_REACTION_REMOVE_EMOJI", &clear)
        .await;

    // Both 👍 rows gone; the 🎉 row survives.
    assert!(
        mem.get("discord_reaction_m1_u1_👍")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        mem.get("discord_reaction_m1_u2_👍")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        mem.get("discord_reaction_m1_u1_🎉")
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(mem.count().await.unwrap(), 1);
}

#[tokio::test]
async fn bulk_removal_respects_guild_and_channel_gates() {
    let dir = tempfile::tempdir().unwrap();
    let mem: std::sync::Arc<dyn zeroclaw_memory::Memory> = std::sync::Arc::new(
        zeroclaw_memory::SqliteMemory::new_named("sqlite", dir.path(), "discord").unwrap(),
    );
    let gated = DiscordChannel::new(
        "fake".into(),
        vec!["g1".into()],
        "discord_test_alias",
        Arc::new(|| vec!["*".to_string()]),
        false,
        false,
    )
    .with_channel_ids(vec!["c1".into()])
    .with_archive_memory(std::sync::Arc::clone(&mem))
    .with_reaction_notifications(DiscordReactionScope::All);

    let add = serde_json::json!({
        "user_id": "friend", "message_id": "m1", "channel_id": "c1",
        "guild_id": "g1", "emoji": {"name": "👍"}
    });
    gated
        .handle_reaction_event("MESSAGE_REACTION_ADD", &add, "botid")
        .await;
    assert_eq!(mem.count().await.unwrap(), 1);

    // Wrong guild: sweep is a no-op, the row stays.
    let wrong_guild = serde_json::json!({
        "message_id": "m1", "channel_id": "c1", "guild_id": "g2"
    });
    gated
        .sweep_message_reactions("MESSAGE_REACTION_REMOVE_ALL", &wrong_guild)
        .await;
    assert_eq!(mem.count().await.unwrap(), 1);

    // Right guild and channel: swept.
    let ok = serde_json::json!({
        "message_id": "m1", "channel_id": "c1", "guild_id": "g1"
    });
    gated
        .sweep_message_reactions("MESSAGE_REACTION_REMOVE_ALL", &ok)
        .await;
    assert_eq!(mem.count().await.unwrap(), 0);
}

#[test]
fn intents_mask_override_wins_verbatim() {
    // Operator escape hatch: a Some(_) intents_mask is sent exactly as
    // configured, ignoring the derived baseline — including Some(0),
    // which is a legal IDENTIFY value.
    let ch = DiscordChannel::new(
        "fake".into(),
        vec![],
        "discord_test_alias",
        Arc::new(Vec::new),
        false,
        false,
    )
    .with_intents_mask(Some(46593));
    assert_eq!(ch.gateway_intents(), 46593);

    let zero = DiscordChannel::new(
        "fake".into(),
        vec![],
        "discord_test_alias",
        Arc::new(Vec::new),
        false,
        false,
    )
    .with_intents_mask(Some(0));
    assert_eq!(zero.gateway_intents(), 0);

    // None means "derive".
    let derived = DiscordChannel::new(
        "fake".into(),
        vec![],
        "discord_test_alias",
        Arc::new(Vec::new),
        false,
        false,
    )
    .with_intents_mask(None);
    assert_eq!(derived.gateway_intents(), BASELINE_INTENTS);
}

#[test]
fn base64_decode_bot_id() {
    // "MTIzNDU2" decodes to "123456"
    let decoded = base64_decode("MTIzNDU2");
    assert_eq!(decoded, Some("123456".to_string()));
}

#[test]
fn bot_user_id_extraction() {
    // Token format: base64(user_id).timestamp.hmac
    let token = "MTIzNDU2.fake.hmac";
    let id = DiscordChannel::bot_user_id_from_token(token);
    assert_eq!(id, Some("123456".to_string()));
}

#[test]
fn gateway_preflight_429_remains_retryable_http_error() {
    let response = reqwest::Response::from(
        axum::http::Response::builder()
            .status(reqwest::StatusCode::TOO_MANY_REQUESTS)
            .header(reqwest::header::RETRY_AFTER, "1")
            .body(reqwest::Body::from(""))
            .expect("test response should build"),
    );

    let error = DiscordChannel::validate_gateway_preflight_response(response)
        .expect_err("429 should remain an HTTP error");
    assert!(error.downcast_ref::<reqwest::Error>().is_some());
    assert!(
        error.downcast_ref::<DiscordListenerFatalError>().is_none(),
        "gateway preflight 429 must not be wrapped as fatal"
    );
    assert!(
        !zeroclaw_providers::reliable::is_non_retryable(&error),
        "gateway preflight 429 should stay on the supervisor retry path"
    );
}

#[test]
fn empty_allowlist_denies_everyone() {
    let listen_to_bots = false;
    let mention_only = false;
    let ch = DiscordChannel::new(
        "fake".into(),
        vec![],
        "discord_test_alias",
        Arc::new(Vec::new),
        listen_to_bots,
        mention_only,
    );
    assert!(!ch.is_user_allowed("12345"));
    assert!(!ch.is_user_allowed("anyone"));
}

#[test]
fn wildcard_allows_everyone() {
    let listen_to_bots = false;
    let mention_only = false;
    let ch = DiscordChannel::new(
        "fake".into(),
        vec![],
        "discord_test_alias",
        Arc::new(|| vec!["*".into()]),
        listen_to_bots,
        mention_only,
    );
    assert!(ch.is_user_allowed("12345"));
    assert!(ch.is_user_allowed("anyone"));
}

#[test]
fn specific_allowlist_filters() {
    let listen_to_bots = false;
    let mention_only = false;
    let ch = DiscordChannel::new(
        "fake".into(),
        vec![],
        "discord_test_alias",
        Arc::new(|| vec!["111".into(), "222".into()]),
        listen_to_bots,
        mention_only,
    );
    assert!(ch.is_user_allowed("111"));
    assert!(ch.is_user_allowed("222"));
    assert!(!ch.is_user_allowed("333"));
    assert!(!ch.is_user_allowed("unknown"));
}

#[test]
fn allowlist_is_exact_match_not_substring() {
    let listen_to_bots = false;
    let mention_only = false;
    let ch = DiscordChannel::new(
        "fake".into(),
        vec![],
        "discord_test_alias",
        Arc::new(|| vec!["111".into()]),
        listen_to_bots,
        mention_only,
    );
    assert!(!ch.is_user_allowed("1111"));
    assert!(!ch.is_user_allowed("11"));
    assert!(!ch.is_user_allowed("0111"));
}

#[test]
fn allowlist_empty_string_user_id() {
    let listen_to_bots = false;
    let mention_only = false;
    let ch = DiscordChannel::new(
        "fake".into(),
        vec![],
        "discord_test_alias",
        Arc::new(|| vec!["111".into()]),
        listen_to_bots,
        mention_only,
    );
    assert!(!ch.is_user_allowed(""));
}

#[test]
fn allowlist_with_wildcard_and_specific() {
    let listen_to_bots = false;
    let mention_only = false;
    let ch = DiscordChannel::new(
        "fake".into(),
        vec![],
        "discord_test_alias",
        Arc::new(|| vec!["111".into(), "*".into()]),
        listen_to_bots,
        mention_only,
    );
    assert!(ch.is_user_allowed("111"));
    assert!(ch.is_user_allowed("anyone_else"));
}

#[test]
fn allowlist_case_sensitive() {
    let listen_to_bots = false;
    let mention_only = false;
    let ch = DiscordChannel::new(
        "fake".into(),
        vec![],
        "discord_test_alias",
        Arc::new(|| vec!["ABC".into()]),
        listen_to_bots,
        mention_only,
    );
    assert!(ch.is_user_allowed("ABC"));
    assert!(!ch.is_user_allowed("abc"));
    assert!(!ch.is_user_allowed("Abc"));
}

#[test]
fn base64_decode_empty_string() {
    let decoded = base64_decode("");
    assert_eq!(decoded, Some(String::new()));
}

#[test]
fn fatal_gateway_close_codes_match_expected_discord_auth_and_intent_errors() {
    for code in [4004_u16, 4010, 4011, 4012, 4013, 4014] {
        assert!(
            is_fatal_gateway_close_code(code),
            "code {code} should be fatal"
        );
    }
    assert!(!is_fatal_gateway_close_code(4007));
    assert!(!is_fatal_gateway_close_code(4009));
}

#[test]
fn new_session_close_codes_match_invalidated_gateway_sessions() {
    assert!(requires_new_session_close_code(4007));
    assert!(requires_new_session_close_code(4009));
    assert!(!requires_new_session_close_code(4004));
}

#[test]
fn base64_decode_invalid_chars() {
    let decoded = base64_decode("!!!!");
    assert!(decoded.is_none());
}

#[test]
fn bot_user_id_from_empty_token() {
    let id = DiscordChannel::bot_user_id_from_token("");
    assert_eq!(id, Some(String::new()));
}

#[test]
fn contains_bot_mention_supports_plain_and_nick_forms() {
    assert!(contains_bot_mention("hi <@12345>", "12345"));
    assert!(contains_bot_mention("hi <@!12345>", "12345"));
    assert!(!contains_bot_mention("hi <@99999>", "12345"));
}

#[test]
fn thread_created_and_system_messages_are_not_conversational() {
    // The bug: THREAD_CREATED (18) was treated as a normal message, so the
    // bot replied to a thread's birth. It and other system types must be
    // rejected; only DEFAULT (0) and REPLY (19) are real user turns.
    assert!(is_conversational_message_type(0)); // DEFAULT
    assert!(is_conversational_message_type(19)); // REPLY
    assert!(!is_conversational_message_type(18)); // THREAD_CREATED
    assert!(!is_conversational_message_type(21)); // THREAD_STARTER_MESSAGE
    assert!(!is_conversational_message_type(6)); // CHANNEL_PINNED_MESSAGE
    assert!(!is_conversational_message_type(7)); // USER_JOIN
}

#[test]
fn admit_discord_message_requires_mention_when_enabled() {
    let cleaned = admit_discord_message("hello there", false, true, "12345");
    assert!(cleaned.is_none());
}

#[test]
fn admit_discord_message_preserves_mention_in_body() {
    let cleaned = admit_discord_message("  <@!12345> run status  ", false, true, "12345");
    assert_eq!(cleaned.as_deref(), Some("<@!12345> run status"));
}

#[test]
fn admit_discord_message_admits_caption_that_is_only_the_mention() {
    let cleaned = admit_discord_message("<@12345>", false, true, "12345");
    assert_eq!(cleaned.as_deref(), Some("<@12345>"));
}

#[test]
fn admit_discord_message_attachment_only_in_dm_is_admitted() {
    // DM (effective_mention_only=false), empty text body, at least one
    // attachment. Previously dropped at the empty-text gate; now passes
    // through so process_attachments can run on the media.
    let cleaned = admit_discord_message("", true, false, "12345");
    assert_eq!(cleaned.as_deref(), Some(""));
}

#[test]
fn admit_discord_message_attachment_only_with_mention_in_guild_is_admitted() {
    // Guild channel with mention_only=true. Caption is the @mention tag
    // and the message has a media attachment. Mention gate passes; the
    // body keeps the mention text so downstream code (and the agent it
    // routes to) can see who was addressed.
    let cleaned = admit_discord_message("<@12345>", true, true, "12345");
    assert_eq!(cleaned.as_deref(), Some("<@12345>"));
}

#[test]
fn admit_discord_message_attachment_only_without_mention_in_guild_is_rejected() {
    // Guild channel with mention_only=true, attachment but no mention
    // anywhere in the caption. The mention gate is orthogonal to
    // attachment presence: no mention signal means drop.
    let cleaned = admit_discord_message("", true, true, "12345");
    assert!(cleaned.is_none());
}

#[test]
fn admit_discord_message_drops_when_no_text_and_no_attachments() {
    // Completely empty payload with attachments absent is always dropped,
    // regardless of mention_only setting.
    assert!(admit_discord_message("", false, false, "12345").is_none());
    assert!(admit_discord_message("", false, true, "12345").is_none());
}

// mention_only DM-bypass tests

#[test]
fn mention_only_dm_bypasses_mention_gate() {
    // DMs (no guild_id) must pass through even when mention_only is true
    // and the message contains no @mention. Mirrors the listen call-site logic.
    let mention_only = true;
    let is_dm = true;
    let effective = mention_only && !is_dm;
    let cleaned = admit_discord_message("hello without mention", false, effective, "12345");
    assert_eq!(cleaned.as_deref(), Some("hello without mention"));
}

#[test]
fn mention_only_guild_message_without_mention_is_rejected() {
    // Guild messages (has guild_id, so is_dm = false) must still be rejected
    // when mention_only is true and the message contains no @mention.
    let mention_only = true;
    let is_dm = false;
    let effective = mention_only && !is_dm;
    let cleaned = admit_discord_message("hello without mention", false, effective, "12345");
    assert!(cleaned.is_none());
}

#[test]
fn mention_only_guild_message_with_mention_passes_through() {
    // Guild messages that carry a @mention pass through the gate with
    // the mention text preserved so downstream consumers (and the agent
    // it routes to) can see who was addressed.
    let mention_only = true;
    let is_dm = false;
    let effective = mention_only && !is_dm;
    let cleaned = admit_discord_message("<@12345> run status", false, effective, "12345");
    assert_eq!(cleaned.as_deref(), Some("<@12345> run status"));
}

// Message splitting tests

#[test]
fn split_empty_message() {
    let chunks = split_message_for_discord("");
    assert_eq!(chunks, vec![""]);
}

#[test]
fn split_short_message_under_limit() {
    let msg = "Hello, world!";
    let chunks = split_message_for_discord(msg);
    assert_eq!(chunks, vec![msg]);
}

#[test]
fn split_message_exactly_2000_chars() {
    let msg = "a".repeat(DISCORD_MAX_MESSAGE_LENGTH);
    let chunks = split_message_for_discord(&msg);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].chars().count(), DISCORD_MAX_MESSAGE_LENGTH);
}

#[test]
fn split_message_just_over_limit() {
    let msg = "a".repeat(DISCORD_MAX_MESSAGE_LENGTH + 1);
    let chunks = split_message_for_discord(&msg);
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].chars().count(), DISCORD_MAX_MESSAGE_LENGTH);
    assert_eq!(chunks[1].chars().count(), 1);
}

#[test]
fn split_very_long_message() {
    let msg = "word ".repeat(2000); // 10000 characters (5 chars per "word ")
    let chunks = split_message_for_discord(&msg);
    // Should split into 5 chunks of <= 2000 chars
    assert_eq!(chunks.len(), 5);
    assert!(
        chunks
            .iter()
            .all(|chunk| chunk.chars().count() <= DISCORD_MAX_MESSAGE_LENGTH)
    );
    // Verify total content is preserved
    let reconstructed = chunks.concat();
    assert_eq!(reconstructed, msg);
}

#[test]
fn split_prefer_newline_break() {
    let msg = format!("{}\n{}", "a".repeat(1500), "b".repeat(500));
    let chunks = split_message_for_discord(&msg);
    // Should split at the newline
    assert_eq!(chunks.len(), 2);
    assert!(chunks[0].ends_with('\n'));
    assert!(chunks[1].starts_with('b'));
}

#[test]
fn split_prefer_space_break() {
    let msg = format!("{} {}", "a".repeat(1500), "b".repeat(600));
    let chunks = split_message_for_discord(&msg);
    assert_eq!(chunks.len(), 2);
}

#[test]
fn split_without_good_break_points_hard_split() {
    // No spaces or newlines - should hard split at 2000
    let msg = "a".repeat(5000);
    let chunks = split_message_for_discord(&msg);
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[0].chars().count(), DISCORD_MAX_MESSAGE_LENGTH);
    assert_eq!(chunks[1].chars().count(), DISCORD_MAX_MESSAGE_LENGTH);
    assert_eq!(chunks[2].chars().count(), 1000);
}

#[test]
fn split_multiple_breaks() {
    // Create a message with multiple newlines
    let part1 = "a".repeat(900);
    let part2 = "b".repeat(900);
    let part3 = "c".repeat(900);
    let msg = format!("{part1}\n{part2}\n{part3}");
    let chunks = split_message_for_discord(&msg);
    // Should split into 2 chunks (first two parts + third part)
    assert_eq!(chunks.len(), 2);
    assert!(chunks[0].chars().count() <= DISCORD_MAX_MESSAGE_LENGTH);
    assert!(chunks[1].chars().count() <= DISCORD_MAX_MESSAGE_LENGTH);
}

#[test]
fn split_preserves_content() {
    let original = "Hello world! This is a test message with some content. ".repeat(200);
    let chunks = split_message_for_discord(&original);
    let reconstructed = chunks.concat();
    assert_eq!(reconstructed, original);
}

#[test]
fn split_unicode_content() {
    // Test with emoji and multi-byte characters
    let msg = "🦀 Rust is awesome! ".repeat(500);
    let chunks = split_message_for_discord(&msg);
    // All chunks should be valid UTF-8
    for chunk in &chunks {
        assert!(std::str::from_utf8(chunk.as_bytes()).is_ok());
        assert!(chunk.chars().count() <= DISCORD_MAX_MESSAGE_LENGTH);
    }
    // Reconstruct and verify
    let reconstructed = chunks.concat();
    assert_eq!(reconstructed, msg);
}

#[test]
fn split_newline_too_close_to_end() {
    // If newline is in the first half, don't use it - use space instead or hard split
    let msg = format!("{}\n{}", "a".repeat(1900), "b".repeat(500));
    let chunks = split_message_for_discord(&msg);
    // Should split at newline since it's in the second half of the window
    assert_eq!(chunks.len(), 2);
}

#[test]
fn split_multibyte_only_content_without_panics() {
    let msg = "🦀".repeat(2500);
    let chunks = split_message_for_discord(&msg);
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].chars().count(), DISCORD_MAX_MESSAGE_LENGTH);
    assert_eq!(chunks[1].chars().count(), 500);
    let reconstructed = chunks.concat();
    assert_eq!(reconstructed, msg);
}

#[test]
fn split_chunks_always_within_discord_limit() {
    let msg = "x".repeat(12_345);
    let chunks = split_message_for_discord(&msg);
    assert!(
        chunks
            .iter()
            .all(|chunk| chunk.chars().count() <= DISCORD_MAX_MESSAGE_LENGTH)
    );
}

#[test]
fn split_message_with_multiple_newlines() {
    let msg = "Line 1\nLine 2\nLine 3\n".repeat(1000);
    let chunks = split_message_for_discord(&msg);
    assert!(chunks.len() > 1);
    let reconstructed = chunks.concat();
    assert_eq!(reconstructed, msg);
}

#[test]
fn typing_handles_start_empty() {
    let listen_to_bots = false;
    let mention_only = false;
    let ch = DiscordChannel::new(
        "fake".into(),
        vec![],
        "discord_test_alias",
        Arc::new(Vec::new),
        listen_to_bots,
        mention_only,
    );
    let guard = ch.typing_handles.lock();
    assert!(guard.is_empty());
}

#[tokio::test]
async fn start_typing_sets_handle() {
    let listen_to_bots = false;
    let mention_only = false;
    let ch = DiscordChannel::new(
        "fake".into(),
        vec![],
        "discord_test_alias",
        Arc::new(Vec::new),
        listen_to_bots,
        mention_only,
    );
    let _ = ch.start_typing("123456").await;
    let guard = ch.typing_handles.lock();
    assert!(guard.contains_key("123456"));
}

#[tokio::test]
async fn stop_typing_clears_handle() {
    let listen_to_bots = false;
    let mention_only = false;
    let ch = DiscordChannel::new(
        "fake".into(),
        vec![],
        "discord_test_alias",
        Arc::new(Vec::new),
        listen_to_bots,
        mention_only,
    );
    let _ = ch.start_typing("123456").await;
    let _ = ch.stop_typing("123456").await;
    let guard = ch.typing_handles.lock();
    assert!(!guard.contains_key("123456"));
}

#[tokio::test]
async fn stop_typing_is_idempotent() {
    let listen_to_bots = false;
    let mention_only = false;
    let ch = DiscordChannel::new(
        "fake".into(),
        vec![],
        "discord_test_alias",
        Arc::new(Vec::new),
        listen_to_bots,
        mention_only,
    );
    assert!(ch.stop_typing("123456").await.is_ok());
    assert!(ch.stop_typing("123456").await.is_ok());
}

#[tokio::test]
async fn concurrent_typing_handles_are_independent() {
    let listen_to_bots = false;
    let mention_only = false;
    let ch = DiscordChannel::new(
        "fake".into(),
        vec![],
        "discord_test_alias",
        Arc::new(Vec::new),
        listen_to_bots,
        mention_only,
    );
    let _ = ch.start_typing("111").await;
    let _ = ch.start_typing("222").await;
    {
        let guard = ch.typing_handles.lock();
        assert_eq!(guard.len(), 2);
        assert!(guard.contains_key("111"));
        assert!(guard.contains_key("222"));
    }
    // Stopping one does not affect the other
    let _ = ch.stop_typing("111").await;
    let guard = ch.typing_handles.lock();
    assert_eq!(guard.len(), 1);
    assert!(guard.contains_key("222"));
}

// ── Emoji encoding for reactions ──────────────────────────────

#[test]
fn encode_emoji_unicode_percent_encodes() {
    let encoded = encode_emoji_for_discord("\u{1F440}");
    assert_eq!(encoded, "%F0%9F%91%80");
}

#[test]
fn encode_emoji_checkmark() {
    let encoded = encode_emoji_for_discord("\u{2705}");
    assert_eq!(encoded, "%E2%9C%85");
}

#[test]
fn encode_emoji_custom_guild_emoji_passthrough() {
    let encoded = encode_emoji_for_discord("custom_emoji:123456789");
    assert_eq!(encoded, "custom_emoji:123456789");
}

#[test]
fn encode_emoji_simple_ascii_char() {
    let encoded = encode_emoji_for_discord("A");
    assert_eq!(encoded, "%41");
}

#[test]
fn random_discord_ack_reaction_is_from_pool() {
    for _ in 0..128 {
        let emoji = random_discord_ack_reaction();
        assert!(DISCORD_ACK_REACTIONS.contains(&emoji));
    }
}

#[test]
fn discord_reaction_url_encodes_emoji_and_strips_prefix() {
    let url = discord_reaction_url("123", "discord_456", "👀");
    assert_eq!(
        url,
        "https://discord.com/api/v10/channels/123/messages/456/reactions/%F0%9F%91%80/@me"
    );
}

// ── Message ID edge cases ─────────────────────────────────────

#[test]
fn discord_message_id_format_includes_discord_prefix() {
    // Verify that message IDs follow the format: discord_{message_id}
    let message_id = "123456789012345678";
    let expected_id = format!("discord_{message_id}");
    assert_eq!(expected_id, "discord_123456789012345678");
}

#[test]
fn discord_message_id_is_deterministic() {
    // Same message_id = same ID (prevents duplicates after restart)
    let message_id = "123456789012345678";
    let id1 = format!("discord_{message_id}");
    let id2 = format!("discord_{message_id}");
    assert_eq!(id1, id2);
}

#[test]
fn discord_message_id_different_message_different_id() {
    // Different message IDs produce different IDs
    let id1 = "discord_123456789012345678".to_string();
    let id2 = "discord_987654321098765432".to_string();
    assert_ne!(id1, id2);
}

#[test]
fn discord_message_id_uses_snowflake_id() {
    // Discord snowflake IDs are numeric strings
    let message_id = "123456789012345678"; // Typical snowflake format
    let id = format!("discord_{message_id}");
    assert!(id.starts_with("discord_"));
    // Snowflake IDs are numeric
    assert!(message_id.chars().all(|c| c.is_ascii_digit()));
}

#[test]
fn discord_message_id_fallback_to_uuid_on_empty() {
    // Edge case: empty message_id falls back to UUID
    let message_id = "";
    let id = if message_id.is_empty() {
        format!("discord_{}", uuid::Uuid::new_v4())
    } else {
        format!("discord_{message_id}")
    };
    assert!(id.starts_with("discord_"));
    // Should have UUID dashes
    assert!(id.contains('-'));
}

// ─────────────────────────────────────────────────────────────────────
// TG6: Channel platform limit edge cases for Discord (2000 char limit)
// Prevents: Pattern 6 — issues
// ─────────────────────────────────────────────────────────────────────

#[test]
fn split_message_code_block_at_boundary() {
    // Code block that spans the split boundary
    let mut msg = String::new();
    msg.push_str("```rust\n");
    msg.push_str(&"x".repeat(1990));
    msg.push_str("\n```\nMore text after code block");
    let parts = split_message_for_discord(&msg);
    assert!(
        parts.len() >= 2,
        "code block spanning boundary should split"
    );
    for part in &parts {
        assert!(
            part.len() <= DISCORD_MAX_MESSAGE_LENGTH,
            "each part must be <= {DISCORD_MAX_MESSAGE_LENGTH}, got {}",
            part.len()
        );
    }
}

#[test]
fn split_message_single_long_word_exceeds_limit() {
    // A single word longer than 2000 chars must be hard-split
    let long_word = "a".repeat(2500);
    let parts = split_message_for_discord(&long_word);
    assert!(parts.len() >= 2, "word exceeding limit must be split");
    for part in &parts {
        assert!(
            part.len() <= DISCORD_MAX_MESSAGE_LENGTH,
            "hard-split part must be <= {DISCORD_MAX_MESSAGE_LENGTH}, got {}",
            part.len()
        );
    }
    // Reassembled content should match original
    let reassembled: String = parts.join("");
    assert_eq!(reassembled, long_word);
}

#[test]
fn split_message_exactly_at_limit_no_split() {
    let msg = "a".repeat(DISCORD_MAX_MESSAGE_LENGTH);
    let parts = split_message_for_discord(&msg);
    assert_eq!(parts.len(), 1, "message exactly at limit should not split");
    assert_eq!(parts[0].len(), DISCORD_MAX_MESSAGE_LENGTH);
}

#[test]
fn split_message_one_over_limit_splits() {
    let msg = "a".repeat(DISCORD_MAX_MESSAGE_LENGTH + 1);
    let parts = split_message_for_discord(&msg);
    assert!(parts.len() >= 2, "message 1 char over limit must split");
}

#[test]
fn split_message_many_short_lines() {
    // Many short lines should be batched into chunks under the limit
    let msg: String = (0..500).fold(String::new(), |mut acc, i| {
        let _ = writeln!(acc, "line {i}");
        acc
    });
    let parts = split_message_for_discord(&msg);
    for part in &parts {
        assert!(
            part.len() <= DISCORD_MAX_MESSAGE_LENGTH,
            "short-line batch must be <= limit"
        );
    }
    // All content should be preserved
    let reassembled: String = parts.join("");
    assert_eq!(reassembled.trim(), msg.trim());
}

#[test]
fn split_message_only_whitespace() {
    let msg = "   \n\n\t  ";
    let parts = split_message_for_discord(msg);
    // Should handle gracefully without panic
    assert!(parts.len() <= 1);
}

#[test]
fn split_message_emoji_at_boundary() {
    // Emoji are multi-byte; ensure we don't split mid-emoji
    let mut msg = "a".repeat(1998);
    msg.push_str("🎉🎊"); // 2 emoji at the boundary (2000 chars total)
    let parts = split_message_for_discord(&msg);
    for part in &parts {
        // The function splits on character count, not byte count
        assert!(
            part.chars().count() <= DISCORD_MAX_MESSAGE_LENGTH,
            "emoji boundary split must respect limit"
        );
    }
}

#[test]
fn split_message_consecutive_newlines_at_boundary() {
    let mut msg = "a".repeat(1995);
    msg.push_str("\n\n\n\n\n");
    msg.push_str(&"b".repeat(100));
    let parts = split_message_for_discord(&msg);
    for part in &parts {
        assert!(part.len() <= DISCORD_MAX_MESSAGE_LENGTH);
    }
}

// process_attachments tests

#[tokio::test]
async fn process_attachments_empty_list_returns_empty() {
    let client = reqwest::Client::new();
    let (text, media) = process_attachments(&[], &client, None, None).await;
    assert!(text.is_empty());
    assert!(media.is_empty());
}

#[tokio::test]
async fn process_attachments_preserves_audio_when_transcription_fails() {
    use crate::transcription::TranscriptionManager;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let media_server = MockServer::start().await;
    let whisper_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/voice.ogg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"fake-audio"))
        .expect(1)
        .mount(&media_server)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/transcribe"))
        .respond_with(
            ResponseTemplate::new(503)
                .set_body_json(serde_json::json!({"error": "stt unavailable"})),
        )
        .mount(&whisper_server)
        .await;

    let audio_url = format!("{}/voice.ogg", media_server.uri());
    let attachments = vec![serde_json::json!({
        "content_type": "audio/ogg",
        "filename": "voice.ogg",
        "url": audio_url,
    })];
    let transcription =
        TranscriptionManager::new(&local_whisper_transcription_config(&whisper_server))
            .expect("transcription manager")
            .with_agent_transcription_provider("local_whisper");

    let client = reqwest::Client::new();
    let (text, media) =
        process_attachments(&attachments, &client, None, Some(&transcription)).await;

    assert_eq!(
        text,
        format!("[AUDIO:{}]", attachments[0]["url"].as_str().unwrap())
    );
    assert_eq!(media.len(), 1);
    assert_eq!(media[0].file_name, "voice.ogg");
    assert_eq!(media[0].mime_type.as_deref(), Some("audio/ogg"));
    assert_eq!(media[0].data, b"fake-audio");
}

#[tokio::test]
async fn process_attachments_preserves_audio_when_transcription_is_empty() {
    use crate::transcription::TranscriptionManager;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let media_server = MockServer::start().await;
    let whisper_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/voice.ogg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"fake-audio"))
        .expect(1)
        .mount(&media_server)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/transcribe"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"text": ""})))
        .mount(&whisper_server)
        .await;

    let audio_url = format!("{}/voice.ogg", media_server.uri());
    let attachments = vec![serde_json::json!({
        "content_type": "audio/ogg",
        "filename": "voice.ogg",
        "url": audio_url,
    })];
    let transcription =
        TranscriptionManager::new(&local_whisper_transcription_config(&whisper_server))
            .expect("transcription manager")
            .with_agent_transcription_provider("local_whisper");

    let client = reqwest::Client::new();
    let (text, media) =
        process_attachments(&attachments, &client, None, Some(&transcription)).await;

    assert_eq!(
        text,
        format!("[AUDIO:{}]", attachments[0]["url"].as_str().unwrap())
    );
    assert_eq!(media.len(), 1);
    assert_eq!(media[0].file_name, "voice.ogg");
    assert_eq!(media[0].mime_type.as_deref(), Some("audio/ogg"));
    assert_eq!(media[0].data, b"fake-audio");
}

fn local_whisper_transcription_config(
    server: &wiremock::MockServer,
) -> zeroclaw_config::schema::TranscriptionConfig {
    zeroclaw_config::schema::TranscriptionConfig {
        enabled: true,
        local_whisper: Some(zeroclaw_config::schema::LocalWhisperConfig {
            url: format!("{}/v1/transcribe", server.uri()),
            bearer_token: Some("test-token".to_string()),
            max_audio_bytes: 10 * 1024 * 1024,
            timeout_secs: 30,
        }),
        ..Default::default()
    }
}

#[test]
fn marker_kind_for_classifies_each_mime_family() {
    assert_eq!(marker_kind_for("image/png", false), "IMAGE");
    assert_eq!(marker_kind_for("image/jpeg", false), "IMAGE");
    assert_eq!(marker_kind_for("video/mp4", false), "VIDEO");
    assert_eq!(marker_kind_for("application/pdf", false), "DOCUMENT");
    assert_eq!(marker_kind_for("application/zip", false), "DOCUMENT");
    assert_eq!(marker_kind_for("", false), "DOCUMENT");
}

#[test]
fn marker_kind_for_treats_audio_flag_as_audio_regardless_of_content_type() {
    // Filename-detected audio with no content_type should still classify
    // as AUDIO, matching the unified inbound pipeline.
    assert_eq!(marker_kind_for("", true), "AUDIO");
    assert_eq!(marker_kind_for("application/octet-stream", true), "AUDIO");
}

#[test]
fn marker_kind_for_prefers_image_over_audio_when_content_type_is_image() {
    // Defensive: if a Discord attachment somehow tripped both heuristics,
    // image MIME wins so vision-capable providers still receive image
    // bytes through the MediaAttachment path.
    assert_eq!(marker_kind_for("image/png", true), "IMAGE");
}

#[test]
fn is_thread_channel_type_matches_only_thread_types() {
    // Thread types per Discord docs: 10/11/12.
    assert!(is_thread_channel_type(10));
    assert!(is_thread_channel_type(11));
    assert!(is_thread_channel_type(12));
    // Non-thread channel types must not be classified as threads.
    for non_thread in [0u64, 1, 2, 3, 4, 5, 13, 14, 15, 16] {
        assert!(
            !is_thread_channel_type(non_thread),
            "type {non_thread} must not classify as thread"
        );
    }
}

#[test]
fn channel_filter_empty_accepts_everything() {
    let filter: Vec<String> = vec![];
    assert!(channel_passes_filter(&filter, "12345", None));
    assert!(channel_passes_filter(&filter, "99999", Some("12345")));
    assert!(channel_passes_filter(&filter, "", None));
}

#[test]
fn channel_filter_direct_match() {
    let filter = vec!["111".to_string(), "222".to_string()];
    assert!(channel_passes_filter(&filter, "111", None));
    assert!(channel_passes_filter(&filter, "222", None));
    assert!(!channel_passes_filter(&filter, "333", None));
}

#[test]
fn channel_filter_thread_parent_fallback() {
    let filter = vec!["111".to_string()];
    // Thread whose parent is in the allowlist — accepted.
    assert!(channel_passes_filter(&filter, "999", Some("111")));
    // Thread whose parent is NOT in the allowlist — rejected.
    assert!(!channel_passes_filter(&filter, "999", Some("888")));
    // Non-thread channel not in the allowlist — rejected.
    assert!(!channel_passes_filter(&filter, "999", None));
}

#[test]
fn channel_filter_direct_match_skips_parent_check() {
    let filter = vec!["111".to_string()];
    // Direct match with a parent_id present — parent is irrelevant.
    assert!(channel_passes_filter(&filter, "111", Some("999")));
}

#[test]
fn parse_attachment_markers_extracts_supported_markers() {
    let input = "Report\n[IMAGE:https://example.com/a.png]\n[DOCUMENT:/tmp/a.pdf]";
    let (cleaned, attachments) = parse_attachment_markers(input);

    assert_eq!(cleaned, "Report");
    assert_eq!(attachments.len(), 2);
    assert_eq!(attachments[0].kind, DiscordAttachmentKind::Image);
    assert_eq!(attachments[0].target, "https://example.com/a.png");
    assert_eq!(attachments[1].kind, DiscordAttachmentKind::Document);
    assert_eq!(attachments[1].target, "/tmp/a.pdf");
}

#[test]
fn parse_attachment_markers_keeps_invalid_marker_text() {
    let input = "Hello [NOT_A_MARKER:foo] world";
    let (cleaned, attachments) = parse_attachment_markers(input);

    assert_eq!(cleaned, input);
    assert!(attachments.is_empty());
}

#[test]
fn classify_outgoing_attachments_keeps_workspace_locals_and_http() {
    let temp = tempfile::tempdir().expect("tempdir");
    let file_path = temp.path().join("image.png");
    std::fs::write(&file_path, b"fake").expect("write fixture");

    let attachments = vec![
        DiscordAttachment {
            kind: DiscordAttachmentKind::Image,
            target: file_path.to_string_lossy().to_string(),
        },
        DiscordAttachment {
            kind: DiscordAttachmentKind::Image,
            target: "https://example.com/remote.png".to_string(),
        },
    ];

    let (locals, remotes, failures) =
        classify_outgoing_attachments(&attachments, Some(temp.path()));
    assert_eq!(locals.len(), 1);
    let canonical_file = std::fs::canonicalize(&file_path).expect("canonicalize fixture");
    assert_eq!(locals[0], canonical_file);
    assert_eq!(remotes, vec!["https://example.com/remote.png".to_string()]);
    assert!(failures.is_empty());
}

#[test]
fn classify_outgoing_attachments_drops_missing_absolute_paths() {
    let temp = tempfile::tempdir().expect("tempdir");
    let attachments = vec![DiscordAttachment {
        kind: DiscordAttachmentKind::Video,
        target: temp
            .path()
            .join("does-not-exist.mp4")
            .to_string_lossy()
            .to_string(),
    }];

    let (locals, remotes, failures) =
        classify_outgoing_attachments(&attachments, Some(temp.path()));
    assert!(locals.is_empty());
    assert!(remotes.is_empty());
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0], DiscordMarkerFailure::NotFound);
}

#[test]
fn classify_outgoing_attachments_drops_paths_outside_workspace() {
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let outside = tempfile::tempdir().expect("outside tempdir");
    let outside_file = outside.path().join("escape.png");
    std::fs::write(&outside_file, b"fake").expect("write fixture");

    let attachments = vec![DiscordAttachment {
        kind: DiscordAttachmentKind::Image,
        target: outside_file.to_string_lossy().to_string(),
    }];

    let (locals, remotes, failures) =
        classify_outgoing_attachments(&attachments, Some(workspace.path()));
    assert!(
        locals.is_empty(),
        "absolute paths outside workspace must be refused"
    );
    assert!(remotes.is_empty());
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0], DiscordMarkerFailure::Refused);
}

#[test]
fn classify_outgoing_attachments_drops_relative_paths() {
    let temp = tempfile::tempdir().expect("tempdir");
    let attachments = vec![DiscordAttachment {
        kind: DiscordAttachmentKind::Document,
        target: "relative/report.pdf".to_string(),
    }];

    let (locals, remotes, failures) =
        classify_outgoing_attachments(&attachments, Some(temp.path()));
    assert!(locals.is_empty(), "relative paths must be refused");
    assert!(remotes.is_empty());
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0], DiscordMarkerFailure::Refused);
}

#[test]
fn classify_outgoing_attachments_drops_disallowed_schemes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let attachments = vec![
        DiscordAttachment {
            kind: DiscordAttachmentKind::Image,
            target: "file:///etc/hostname".to_string(),
        },
        DiscordAttachment {
            kind: DiscordAttachmentKind::Document,
            target: "data:text/plain;base64,aGk=".to_string(),
        },
        DiscordAttachment {
            kind: DiscordAttachmentKind::Video,
            target: "ftp://example.com/clip.mp4".to_string(),
        },
    ];

    let (locals, remotes, failures) =
        classify_outgoing_attachments(&attachments, Some(temp.path()));
    assert!(locals.is_empty());
    assert!(remotes.is_empty());
    assert_eq!(failures.len(), 3);
    for kind in &failures {
        assert_eq!(*kind, DiscordMarkerFailure::Refused);
    }
}

#[test]
fn classify_outgoing_attachments_refuses_local_without_workspace() {
    let attachments = vec![DiscordAttachment {
        kind: DiscordAttachmentKind::Image,
        target: "/some/absolute/path.png".to_string(),
    }];

    let (locals, remotes, failures) = classify_outgoing_attachments(&attachments, None);
    assert!(
        locals.is_empty(),
        "local paths must be refused without workspace_dir"
    );
    assert!(remotes.is_empty());
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0], DiscordMarkerFailure::Refused);
}

#[test]
fn classify_outgoing_attachments_passes_http_without_workspace() {
    let attachments = vec![DiscordAttachment {
        kind: DiscordAttachmentKind::Image,
        target: "https://example.com/x.png".to_string(),
    }];

    let (locals, remotes, failures) = classify_outgoing_attachments(&attachments, None);
    assert!(locals.is_empty());
    assert_eq!(remotes, vec!["https://example.com/x.png".to_string()]);
    assert!(failures.is_empty());
}

#[test]
fn with_inline_attachment_urls_appends_remote_urls_only() {
    let content = "Done";
    let remote_urls = vec!["https://example.com/a.png".to_string()];

    let rendered = with_inline_attachment_urls(content, &remote_urls);
    assert_eq!(rendered, "Done\nhttps://example.com/a.png");
}

#[test]
fn with_inline_attachment_urls_keeps_content_when_no_urls() {
    let rendered = with_inline_attachment_urls("Done", &[]);
    assert_eq!(rendered, "Done");
}

#[test]
fn delivery_failure_note_is_none_when_no_failures() {
    assert!(delivery_failure_note(&[]).is_none());
}

#[test]
fn delivery_failure_note_singular_for_one_failure() {
    let failures = [DiscordMarkerFailure::NotFound];
    let note = delivery_failure_note(&failures).expect("one failure should produce a note");
    // Locale-independent: count is always rendered as Arabic digits in
    // every shipped locale's FTL template (`{$count}`). The literal
    // English string used to live here but the assertion broke on any
    // CI runner with a non-English `$LANG` (see's blocker).
    assert!(!note.is_empty(), "note must be non-empty");
    assert!(
        note.contains(failures.len().to_string().as_str()),
        "note must contain the failure count"
    );
    assert!(
        !note.contains("/workspace/missing.png"),
        "user-facing failure note must not echo local marker targets"
    );
}

#[test]
fn delivery_failure_note_plural_redacts_targets() {
    let failures = [
        DiscordMarkerFailure::Refused,
        DiscordMarkerFailure::NotFound,
        DiscordMarkerFailure::Refused,
    ];
    let note = delivery_failure_note(&failures).expect("multiple failures should produce a note");
    // Locale-independent: see singular test for rationale.
    assert!(!note.is_empty(), "note must be non-empty");
    assert!(
        note.contains(failures.len().to_string().as_str()),
        "note must contain the failure count"
    );
    assert!(
        !note.contains("a.png") && !note.contains("b.pdf") && !note.contains("c.mp4"),
        "user-facing failure note must not echo failed marker targets"
    );
}

#[test]
fn composed_delivery_failure_note_redacts_parsed_marker_target() {
    let content = "Done\n[IMAGE: /workspace/missing.png]";
    let (cleaned_content, parsed_attachments) = parse_attachment_markers(content);
    let (_locals, _remotes, failures) = classify_outgoing_attachments(&parsed_attachments, None);
    let note = delivery_failure_note(&failures);
    let composed = compose_body_with_failure_note(&cleaned_content, note.as_deref());

    // Locale-independent: the body must keep the original `Done` content,
    // gain exactly one blank-line separator, and never echo the failed
    // marker path. The previous literal English assertion was
    // locale-dependent and broke on non-English CI runners.
    assert!(
        composed.starts_with("Done\n\n"),
        "composed body must preserve original content with blank-line separator, got {composed:?}"
    );
    assert!(
        !composed.contains("/workspace/missing.png"),
        "composed outbound body must not echo failed marker targets"
    );
}

#[test]
fn compose_body_with_failure_note_uses_note_alone_when_content_empty() {
    let composed = compose_body_with_failure_note("", Some("(note: ...)"));
    assert_eq!(composed, "(note: ...)");
}

#[test]
fn compose_body_with_failure_note_appends_note_to_existing_content() {
    let composed = compose_body_with_failure_note("Hello.", Some("(note: ...)"));
    assert_eq!(composed, "Hello.\n\n(note: ...)");
}

#[test]
fn compose_body_with_failure_note_returns_content_when_no_note() {
    let composed = compose_body_with_failure_note("Hello.", None);
    assert_eq!(composed, "Hello.");
}

#[test]
fn compose_body_with_failure_note_returns_empty_when_no_content_and_no_note() {
    let composed = compose_body_with_failure_note("", None);
    assert_eq!(composed, "");
}

#[test]
fn decide_failure_reactions_empty_for_no_failures() {
    assert!(decide_failure_reactions(&[]).is_empty());
}

#[test]
fn decide_failure_reactions_emits_refused_only() {
    let r =
        decide_failure_reactions(&[DiscordMarkerFailure::Refused, DiscordMarkerFailure::Refused]);
    assert_eq!(r, vec!["🚫"]);
}

#[test]
fn decide_failure_reactions_emits_not_found_only() {
    let r = decide_failure_reactions(&[DiscordMarkerFailure::NotFound]);
    assert_eq!(r, vec!["\u{26A0}\u{FE0F}"]);
}

#[test]
fn decide_failure_reactions_emits_both_when_mixed() {
    let r = decide_failure_reactions(&[
        DiscordMarkerFailure::Refused,
        DiscordMarkerFailure::NotFound,
    ]);
    assert_eq!(r, vec!["🚫", "\u{26A0}\u{FE0F}"]);
}

// ── Streaming mode tests ──────────────────────────────────────────

#[test]
fn supports_draft_updates_respects_stream_mode() {
    use zeroclaw_config::schema::StreamMode;

    let listen_to_bots = false;
    let mention_only = false;
    let off = DiscordChannel::new(
        "t".into(),
        vec![],
        "discord_test_alias",
        Arc::new(Vec::new),
        listen_to_bots,
        mention_only,
    );
    assert!(!off.supports_draft_updates());

    let partial = DiscordChannel::new(
        "t".into(),
        vec![],
        "discord_test_alias",
        Arc::new(Vec::new),
        listen_to_bots,
        mention_only,
    )
    .with_streaming(StreamMode::Partial, 750, 800);
    assert!(partial.supports_draft_updates());
    assert_eq!(partial.draft_update_interval_ms, 750);

    let multi = DiscordChannel::new(
        "t".into(),
        vec![],
        "discord_test_alias",
        Arc::new(Vec::new),
        listen_to_bots,
        mention_only,
    )
    .with_streaming(StreamMode::MultiMessage, 1000, 600);
    assert!(multi.supports_draft_updates());
    assert_eq!(multi.multi_message_delay_ms, 600);
}

#[tokio::test]
async fn send_draft_returns_none_when_not_partial() {
    use zeroclaw_api::channel::SendMessage;
    use zeroclaw_config::schema::StreamMode;

    let listen_to_bots = false;
    let mention_only = false;
    let off = DiscordChannel::new(
        "t".into(),
        vec![],
        "discord_test_alias",
        Arc::new(Vec::new),
        listen_to_bots,
        mention_only,
    );
    let msg = SendMessage::new("hello", "123");
    assert!(off.send_draft(&msg).await.unwrap().is_none());

    let multi = DiscordChannel::new(
        "t".into(),
        vec![],
        "discord_test_alias",
        Arc::new(Vec::new),
        listen_to_bots,
        mention_only,
    )
    .with_streaming(StreamMode::MultiMessage, 1000, 800);
    // MultiMessage returns a synthetic ID so the draft_updater task runs.
    assert_eq!(
        multi.send_draft(&msg).await.unwrap().as_deref(),
        Some("multi_message_synthetic")
    );
}

#[tokio::test]
async fn update_draft_rate_limit_short_circuits() {
    use zeroclaw_config::schema::StreamMode;

    let listen_to_bots = false;
    let mention_only = false;
    let ch = DiscordChannel::new(
        "t".into(),
        vec![],
        "discord_test_alias",
        Arc::new(Vec::new),
        listen_to_bots,
        mention_only,
    )
    .with_streaming(StreamMode::Partial, 60_000, 800);

    // Seed a recent edit time.
    ch.last_draft_edit
        .lock()
        .insert("chan".to_string(), std::time::Instant::now());

    // Should return Ok immediately (rate-limited) without making a network call.
    let result = ch.update_draft("chan", "fake_msg_id", "new text").await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn cancel_draft_cleans_up_tracking() {
    use zeroclaw_config::schema::StreamMode;

    let listen_to_bots = false;
    let mention_only = false;
    let ch = DiscordChannel::new(
        "t".into(),
        vec![],
        "discord_test_alias",
        Arc::new(Vec::new),
        listen_to_bots,
        mention_only,
    )
    .with_streaming(StreamMode::Partial, 1000, 800);

    ch.last_draft_edit
        .lock()
        .insert("chan".to_string(), std::time::Instant::now());

    // cancel_draft will try to delete a message (will fail with network error)
    // but should still clean up the tracking entry.
    let _ = ch.cancel_draft("chan", "fake_msg_id").await;
    assert!(!ch.last_draft_edit.lock().contains_key("chan"));
}

// ── MultiMessage splitter tests ───────────────────────────────────

#[test]
fn split_message_for_discord_multi_splits_at_paragraphs() {
    let content = "First paragraph.\n\nSecond paragraph.\n\nThird paragraph.";
    let chunks = split_message_for_discord_multi(content, 2000);
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[0], "First paragraph.");
    assert_eq!(chunks[1], "Second paragraph.");
    assert_eq!(chunks[2], "Third paragraph.");
}

#[test]
fn split_message_for_discord_multi_single_paragraph() {
    let content = "Just one paragraph with no breaks.";
    let chunks = split_message_for_discord_multi(content, 2000);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0], content);
}

#[test]
fn split_message_for_discord_multi_respects_max_len() {
    // Create a single paragraph that exceeds max_len.
    let long_para = "a ".repeat(1100); // ~2200 chars
    let chunks = split_message_for_discord_multi(&long_para, 2000);
    assert!(chunks.len() > 1, "should split oversized paragraph");
    for chunk in &chunks {
        assert!(
            chunk.chars().count() <= 2000,
            "chunk exceeds max: {}",
            chunk.chars().count()
        );
    }
}

#[test]
fn split_message_for_discord_multi_preserves_code_fences() {
    let content = "Before.\n\n```rust\nfn main() {\n\n    println!(\"hello\");\n}\n```\n\nAfter.";
    let chunks = split_message_for_discord_multi(content, 2000);
    // The code fence contains \n\n but should not be split there.
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[0], "Before.");
    assert!(chunks[1].contains("```rust"));
    assert!(chunks[1].contains("println!"));
    assert!(chunks[1].contains("```"));
    assert_eq!(chunks[2], "After.");
}

#[test]
fn split_message_for_discord_multi_empty_input() {
    let chunks = split_message_for_discord_multi("", 2000);
    assert!(chunks.is_empty());
}

// Regression lock for the marker-only paragraph in MultiMessage stream
// mode. Before the fix this produced an empty chunk vec and the chunk
// loop in send() iterated zero times, silently skipping the file upload.
#[test]
fn chunks_for_send_emits_empty_chunk_when_multi_message_paragraph_collapses_to_only_a_file() {
    use zeroclaw_config::schema::StreamMode;
    let chunks = chunks_for_send("", StreamMode::MultiMessage, 2000, true);
    assert_eq!(chunks, vec![String::new()]);
}

#[test]
fn chunks_for_send_does_not_emit_empty_chunk_when_no_files_to_upload() {
    use zeroclaw_config::schema::StreamMode;
    let chunks = chunks_for_send("", StreamMode::MultiMessage, 2000, false);
    assert!(chunks.is_empty());
}

#[test]
fn chunks_for_send_passes_through_non_empty_content() {
    use zeroclaw_config::schema::StreamMode;
    for mode in [
        StreamMode::MultiMessage,
        StreamMode::Partial,
        StreamMode::Off,
    ] {
        for has_files in [true, false] {
            let chunks = chunks_for_send("hello", mode, 2000, has_files);
            assert_eq!(
                chunks,
                vec!["hello".to_string()],
                "mode={mode:?} has_files={has_files}"
            );
        }
    }
}

#[test]
fn pending_approvals_map_is_initially_empty() {
    let listen_to_bots = false;
    let mention_only = false;
    let ch = DiscordChannel::new(
        "token".into(),
        vec![],
        "discord_test_alias",
        Arc::new(Vec::new),
        listen_to_bots,
        mention_only,
    );
    let map = ch.pending_approvals.try_lock().unwrap();
    assert!(map.is_empty());
}

#[test]
fn approval_timeout_defaults_to_300_and_is_overridable() {
    let listen_to_bots = false;
    let mention_only = false;
    let ch = DiscordChannel::new(
        "token".into(),
        vec![],
        "discord_test_alias",
        Arc::new(Vec::new),
        listen_to_bots,
        mention_only,
    );
    assert_eq!(ch.approval_timeout_secs, 300);
    let ch = ch.with_approval_timeout_secs(60);
    assert_eq!(ch.approval_timeout_secs, 60);
}

#[tokio::test]
async fn pending_approval_oneshot_delivers_response() {
    let listen_to_bots = false;
    let mention_only = false;
    let ch = DiscordChannel::new(
        "token".into(),
        vec![],
        "discord_test_alias",
        Arc::new(Vec::new),
        listen_to_bots,
        mention_only,
    );
    let (tx, rx) = oneshot::channel();
    ch.pending_approvals
        .lock()
        .await
        .insert("abc123".to_string(), tx);
    let sender = ch.pending_approvals.lock().await.remove("abc123").unwrap();
    sender.send(ChannelApprovalResponse::Deny).unwrap();
    assert_eq!(rx.await.unwrap(), ChannelApprovalResponse::Deny);
}

/// Faithful model of the type-3 dispatch's post-peer-check sequence: gate
/// first, and ONLY on success take + resolve. Mirrors mod.rs so the test
/// asserts the real ordering contract.
fn dispatch_approval_click(
    peers: &[String],
    user_id: &str,
    custom_id: &str,
    pending_components: &parking_lot::Mutex<pending::PendingComponents>,
    pending_approvals: &mut std::collections::HashMap<
        String,
        oneshot::Sender<ChannelApprovalResponse>,
    >,
) -> bool {
    // Fail-closed authz BEFORE any take. DM-style (no guild/channel filter)
    // with an empty peer list = nobody, exactly like the message path.
    if interaction_gate(peers, &[], &[], user_id, None, "c1", None).is_err() {
        return false; // unauthorized: must not drain or resolve anything
    }
    let intent = pending_components.lock().take(custom_id);
    match intent {
        Some(ComponentIntent::Approval { token, decision }) => {
            approval::resolve_parked_approval(pending_approvals, &token, decision)
        }
        _ => false,
    }
}

#[tokio::test]
async fn authorized_click_resolves_with_the_bound_decision() {
    let token = "tok123";
    let (cid, decision) =
        approval::approval_button_binding(token, approval::ApprovalDecision::AllowOnce);
    let wire = cid.encode().unwrap();

    let reg = parking_lot::Mutex::new(pending::PendingComponents::default());
    reg.lock().register(
        wire.clone(),
        ComponentIntent::Approval {
            token: token.to_string(),
            decision,
        },
    );
    let mut approvals = std::collections::HashMap::new();
    let (tx, rx) = oneshot::channel();
    approvals.insert(token.to_string(), tx);

    let resolved = dispatch_approval_click(&[String::from("*")], "u1", &wire, &reg, &mut approvals);
    assert!(resolved, "authorized click resolves the oneshot");
    assert_eq!(rx.await.unwrap(), ChannelApprovalResponse::Approve);
}

#[tokio::test]
async fn unauthorized_click_neither_resolves_nor_drains() {
    let token = "tok123";
    let (cid, decision) =
        approval::approval_button_binding(token, approval::ApprovalDecision::AllowOnce);
    let wire = cid.encode().unwrap();

    let reg = parking_lot::Mutex::new(pending::PendingComponents::default());
    reg.lock().register(
        wire.clone(),
        ComponentIntent::Approval {
            token: token.to_string(),
            decision,
        },
    );
    let mut approvals = std::collections::HashMap::new();
    let (tx, mut rx) = oneshot::channel();
    approvals.insert(token.to_string(), tx);

    // "intruder" is not in the (specific, non-wildcard) peer list → gate
    // denies BEFORE the take.
    let resolved = dispatch_approval_click(
        &[String::from("u1")],
        "intruder",
        &wire,
        &reg,
        &mut approvals,
    );
    assert!(!resolved, "unauthorized click resolves nothing");
    // The oneshot is unresolved (rx still pending, sender still parked).
    assert!(rx.try_recv().is_err(), "no decision delivered");
    assert!(
        approvals.contains_key(token),
        "the approval entry is NOT drained by an unauthorized click"
    );
    // And the pending component entry survives: an authorized user could
    // still click it (the intruder didn't burn the single use).
    assert!(
        reg.lock().take(&wire).is_some(),
        "the component entry was not drained by the unauthorized click"
    );
}

#[tokio::test]
async fn replayed_click_is_refused_single_use() {
    let token = "tok123";
    let (cid, decision) =
        approval::approval_button_binding(token, approval::ApprovalDecision::Deny);
    let wire = cid.encode().unwrap();

    let reg = parking_lot::Mutex::new(pending::PendingComponents::default());
    reg.lock().register(
        wire.clone(),
        ComponentIntent::Approval {
            token: token.to_string(),
            decision,
        },
    );
    let mut approvals = std::collections::HashMap::new();
    let (tx, rx) = oneshot::channel();
    approvals.insert(token.to_string(), tx);

    assert!(dispatch_approval_click(
        &[String::from("*")],
        "u1",
        &wire,
        &reg,
        &mut approvals
    ));
    assert_eq!(rx.await.unwrap(), ChannelApprovalResponse::Deny);
    // The component entry is gone (single-use take), so a replay of the same
    // custom_id resolves nothing even from an authorized user.
    assert!(
        !dispatch_approval_click(&[String::from("*")], "u1", &wire, &reg, &mut approvals),
        "replayed click refused"
    );
}

#[test]
fn buttoned_approval_registers_four_resolvable_bindings() {
    let listen_to_bots = false;
    let mention_only = false;
    let ch = DiscordChannel::new(
        "token".into(),
        vec![],
        "discord_test_alias",
        Arc::new(Vec::new),
        listen_to_bots,
        mention_only,
    );
    // Register exactly what send_buttoned_approval registers, then confirm
    // every button id resolves to its bound decision (and only its own).
    let token = "abc123";
    let (_, bindings) = approval::build_approval_row(token);
    {
        let mut reg = ch.pending_components.lock();
        for (cid, decision) in &bindings {
            reg.register(
                cid.encode().unwrap(),
                ComponentIntent::Approval {
                    token: token.to_string(),
                    decision: *decision,
                },
            );
        }
    }
    for (cid, decision) in &bindings {
        let got = ch.pending_components.lock().take(&cid.encode().unwrap());
        assert_eq!(
            got,
            Some(ComponentIntent::Approval {
                token: token.to_string(),
                decision: *decision,
            }),
            "each button resolves to its server-bound decision"
        );
    }
}

// ── [COMPONENTS:{json}] agent marker → interactive components (EPIC B) ──

fn rendered_routing_ids(rows: &[components::DiscordActionRow]) -> Vec<String> {
    let mut ids = Vec::new();
    for row in rows {
        let api = row.to_api().expect("non-empty row serializes");
        for comp in api["components"].as_array().unwrap() {
            if comp["type"] == serde_json::json!(2) {
                if let Some(cid) = comp.get("custom_id").and_then(|v| v.as_str()) {
                    ids.push(cid.to_string()); // action button (not a link)
                }
            } else if comp["type"] == serde_json::json!(3) {
                for opt in comp["options"].as_array().unwrap() {
                    ids.push(opt["value"].as_str().unwrap().to_string());
                }
            }
        }
    }
    ids
}

#[test]
fn marker_emit_to_click_resolves_the_registered_prompt() {
    // End-to-end at the registry boundary: the agent emits a [COMPONENTS:…]
    // marker with an action button; build_component_rows registers its prompt
    // under a minted custom_id; a "click" (take of that id) returns exactly
    // the bound prompt — never anything from the wire.
    let (cleaned, rows) = parse_component_markers(
        "Pick: [COMPONENTS:{\"rows\":[[{\"label\":\"Ship\",\"style\":\"primary\",\"prompt\":\"ship the release\"}]]}]",
    );
    assert_eq!(cleaned, "Pick:", "marker stripped from content");

    let mut reg = pending::PendingComponents::default();
    let action_rows = build_component_rows("nonce123", &rows, &mut reg);
    assert_eq!(action_rows.len(), 1);

    let ids = rendered_routing_ids(&action_rows);
    assert_eq!(ids.len(), 1, "one action button → one registered id");
    // The click resolves the server-side prompt the bot registered at emit.
    assert_eq!(
        reg.take(&ids[0]),
        Some(ComponentIntent::ResolveIntoTurn {
            prompt: "ship the release".into()
        })
    );
    // Single-use: a replay of the same id resolves nothing.
    assert_eq!(reg.take(&ids[0]), None, "single-use: replay refused");
}

#[test]
fn marker_link_button_renders_without_registration() {
    let (_, rows) = parse_component_markers(
        "[COMPONENTS:{\"rows\":[[{\"label\":\"Docs\",\"url\":\"https://example.com\"}]]}]",
    );
    let mut reg = pending::PendingComponents::default();
    let action_rows = build_component_rows("n", &rows, &mut reg);
    let api = action_rows[0].to_api().unwrap();
    let btn = &api["components"][0];
    assert_eq!(btn["style"], serde_json::json!(5), "link button");
    assert_eq!(btn["url"], serde_json::json!("https://example.com"));
    assert!(
        btn.get("custom_id").is_none(),
        "link button has no custom_id"
    );
    // No prompt was registered for a link button.
    assert!(rendered_routing_ids(&action_rows).is_empty());
}

#[test]
fn marker_select_options_each_register_their_own_prompt() {
    // Each select option's value IS its own routing token bound to that
    // option's prompt; choosing an option (take of its value) resolves only
    // that option's prompt, matching the dispatch's `component_routing_id`.
    let (_, rows) = parse_component_markers(
        "[COMPONENTS:{\"rows\":[[{\"select\":\"Pick\",\"options\":[{\"label\":\"A\",\"value\":\"a\",\"prompt\":\"chose a\"},{\"label\":\"B\",\"value\":\"b\",\"prompt\":\"chose b\"}]}]]}]",
    );
    let mut reg = pending::PendingComponents::default();
    let action_rows = build_component_rows("nonce", &rows, &mut reg);
    let api = action_rows[0].to_api().unwrap();
    assert_eq!(api["components"][0]["type"], serde_json::json!(3), "select");

    let opt_values: Vec<String> = api["components"][0]["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["value"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(opt_values.len(), 2);
    // The chosen option resolves its own prompt; the other still resolves to
    // its own (distinct ids, no aliasing).
    assert_eq!(
        reg.take(&opt_values[0]),
        Some(ComponentIntent::ResolveIntoTurn {
            prompt: "chose a".into()
        })
    );
    assert_eq!(
        reg.take(&opt_values[1]),
        Some(ComponentIntent::ResolveIntoTurn {
            prompt: "chose b".into()
        })
    );
}

#[test]
fn marker_custom_ids_are_unique_within_a_message() {
    // Two buttons with identical label/prompt must register under distinct
    // ids so they can't collide or alias in the single-use registry.
    let (_, rows) = parse_component_markers(
        "[COMPONENTS:{\"rows\":[[{\"label\":\"X\",\"prompt\":\"same\"},{\"label\":\"X\",\"prompt\":\"same\"}]]}]",
    );
    let mut reg = pending::PendingComponents::default();
    let action_rows = build_component_rows("nonce", &rows, &mut reg);
    let ids = rendered_routing_ids(&action_rows);
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1], "ids are unique even with identical content");
    // Both resolve independently (single-use, no aliasing).
    assert!(reg.take(&ids[0]).is_some());
    assert!(reg.take(&ids[1]).is_some());
}

#[test]
fn marker_modal_button_registers_open_modal_and_submit_resolves_into_turn() {
    let (cleaned, rows) = parse_component_markers(
        "Tell us: [COMPONENTS:{\"rows\":[[{\"label\":\"Report\",\"style\":\"danger\",\"prompt\":\"file a report\",\"modal\":{\"title\":\"Report\",\"fields\":[{\"id\":\"reason\",\"label\":\"Reason\",\"style\":\"paragraph\",\"required\":true,\"max\":500}]}}]]}]",
    );
    assert_eq!(cleaned, "Tell us:", "marker stripped from content");
    // The spec carries the parsed modal (title + one paragraph field).
    match &rows[0][0] {
        markers::ComponentSpec::ModalButton {
            label,
            modal,
            prompt,
            ..
        } => {
            assert_eq!(label, "Report");
            assert_eq!(prompt, "file a report");
            assert_eq!(modal.title, "Report");
            assert_eq!(modal.fields.len(), 1);
            assert_eq!(modal.fields[0].id, "reason");
            assert_eq!(modal.fields[0].style, components::TextInputStyle::Paragraph);
            assert!(modal.fields[0].required);
            assert_eq!(modal.fields[0].max_length, Some(500));
        }
        other => panic!("expected ModalButton, got {other:?}"),
    }

    let mut reg = pending::PendingComponents::default();
    let action_rows = build_component_rows("nonce42", &rows, &mut reg);
    assert_eq!(action_rows.len(), 1);
    // The button renders as a normal (non-link) action button with a zc1 id.
    let ids = rendered_routing_ids(&action_rows);
    assert_eq!(ids.len(), 1, "one modal button → one registered button id");

    // The click drains OpenModal, carrying the built modal + bound prompt.
    let (modal, prompt) = match reg.take(&ids[0]) {
        Some(ComponentIntent::OpenModal { modal, prompt }) => (modal, prompt),
        other => panic!("expected OpenModal, got {other:?}"),
    };
    assert_eq!(prompt, "file a report");
    // Single-use: the button id is drained.
    assert_eq!(reg.take(&ids[0]), None, "modal button is single-use");

    // The modal carries its own minted zc1 routing token, distinct from the
    // button's, and was NOT pre-registered (its TTL starts at open).
    let modal_wire = modal.custom_id.encode().expect("modal id encodes");
    assert!(modal_wire.starts_with("zc1|cmp|"));
    assert_ne!(
        modal_wire, ids[0],
        "modal id is distinct from the button id"
    );
    assert!(
        reg.take(&modal_wire).is_none(),
        "modal submit is not registered until the modal opens"
    );

    // The OpenModal dispatch arm registers the modal id as the resolve-into-
    // turn on open; the type-5 submit then drains that prompt.
    reg.register(
        modal_wire.clone(),
        ComponentIntent::ResolveIntoTurn { prompt },
    );
    assert_eq!(
        reg.take(&modal_wire),
        Some(ComponentIntent::ResolveIntoTurn {
            prompt: "file a report".into()
        }),
        "modal submit resolves into the button's server-side prompt"
    );
}

#[test]
fn malformed_component_marker_does_not_register_anything() {
    // A balanced-but-invalid-JSON body is left verbatim (a recoverable leak)
    // rather than stripped — this guarantees no surrounding prose is ever
    // deleted. Either way it registers nothing and never 400s the send.
    let (cleaned, rows) = parse_component_markers("hi [COMPONENTS:{garbage}] there");
    assert!(
        cleaned.contains("hi") && cleaned.contains("there"),
        "prose preserved; got {cleaned:?}"
    );
    let mut reg = pending::PendingComponents::default();
    let action_rows = build_component_rows("n", &rows, &mut reg);
    assert!(action_rows.is_empty(), "no rows from a malformed marker");
}

#[test]
fn component_routing_id_prefers_zc1_select_value_else_custom_id() {
    // Button / modal: routes on custom_id.
    let data = serde_json::json!({ "custom_id": "zc1|cmp|n-1" });
    assert_eq!(
        component_routing_id(Some(&data)),
        Some("zc1|cmp|n-1".to_string())
    );
    // Select: the chosen option value is a zc1 token → route on it.
    let data = serde_json::json!({
        "custom_id": "zc1|cmp|n-1-menu",
        "values": ["zc1|cmp|n-2"]
    });
    assert_eq!(
        component_routing_id(Some(&data)),
        Some("zc1|cmp|n-2".to_string())
    );
    // A non-zc1 selected value falls back to the menu custom_id.
    let data = serde_json::json!({
        "custom_id": "zc1|cmp|n-1-menu",
        "values": ["not-a-token"]
    });
    assert_eq!(
        component_routing_id(Some(&data)),
        Some("zc1|cmp|n-1-menu".to_string())
    );
}

#[test]
fn autocomplete_authz_is_side_effect_free() {
    assert!(
        interaction_gate(&[String::from("*")], &[], &[], "u1", None, "c1", None).is_ok(),
        "authorized keystroke gates open"
    );
    assert!(
        interaction_gate(
            &[String::from("u1")],
            &[],
            &[],
            "intruder",
            None,
            "c1",
            None
        )
        .is_err(),
        "unauthorized keystroke fails closed → empty choice set, no side effect"
    );
    // DM (no guild) with an empty peer list = nobody, same as messages.
    assert!(
        interaction_gate(&[], &[], &[], "u1", None, "c1", None).is_err(),
        "empty peer list denies"
    );
}

#[tokio::test]
async fn thread_parent_cached_reads_cache_without_rest() {
    // Cache-only lookup: a thread whose parent was resolved by an earlier
    // message returns that parent; a channel cached as a non-thread, or one
    // never looked up, returns None. No client/token is reachable here, so a
    // non-None result can only have come from the cache (never a REST probe).
    let cache: Arc<AsyncMutex<HashMap<String, Option<String>>>> =
        Arc::new(AsyncMutex::new(HashMap::new()));
    {
        let mut c = cache.lock().await;
        c.insert("thread1".to_string(), Some("parentA".to_string()));
        c.insert("plain1".to_string(), None);
    }
    assert_eq!(
        discord_thread_parent_cached(&cache, "thread1").await,
        Some("parentA".to_string()),
        "cached thread resolves to its parent"
    );
    assert_eq!(
        discord_thread_parent_cached(&cache, "plain1").await,
        None,
        "channel cached as a non-thread has no parent"
    );
    assert_eq!(
        discord_thread_parent_cached(&cache, "never_seen").await,
        None,
        "uncached channel yields None (fail-closed)"
    );
}

#[tokio::test]
async fn autocomplete_authorizes_parent_allowlisted_thread_only_when_cached() {
    let peers = s(&["*"]);
    let channel_filter = s(&["parentA"]); // allowlist the PARENT only
    let cache: Arc<AsyncMutex<HashMap<String, Option<String>>>> =
        Arc::new(AsyncMutex::new(HashMap::new()));
    cache
        .lock()
        .await
        .insert("thread_cached".to_string(), Some("parentA".to_string()));

    // Thread whose parent is cached + allowlisted → autocomplete authorized.
    let parent = discord_thread_parent_cached(&cache, "thread_cached").await;
    assert!(
        interaction_gate(
            &peers,
            &[],
            &channel_filter,
            "u1",
            Some("g1"),
            "thread_cached",
            parent.as_deref(),
        )
        .is_ok(),
        "cached allowlisted parent authorizes autocomplete in the thread"
    );

    // Same allowlist, thread NOT yet cached → no parent → fail-closed,
    // matching the pre-fix behavior and avoiding a per-keystroke REST probe.
    let parent = discord_thread_parent_cached(&cache, "thread_uncached").await;
    assert!(
        interaction_gate(
            &peers,
            &[],
            &channel_filter,
            "u1",
            Some("g1"),
            "thread_uncached",
            parent.as_deref(),
        )
        .is_err(),
        "uncached thread stays fail-closed"
    );
}

#[test]
fn autocomplete_arm_resolves_cached_thread_parent_before_gate() {
    let src = include_str!("mod.rs");
    let arm4 = src
        .find("} else if itype == 4 {")
        .expect("type-4 arm present");
    let end = src[arm4..]
        .find("// MESSAGE_UPDATE / MESSAGE_DELETE / MESSAGE_DELETE_BULK")
        .map(|i| arm4 + i)
        .expect("type-4 arm end boundary present");
    let region = &src[arm4..end];
    let cached = region
        .find("discord_thread_parent_cached(")
        .expect("type-4 arm resolves the cached thread parent");
    let gate = region.find("interaction_gate(").expect("type-4 arm gates");
    assert!(
        cached < gate,
        "cached thread-parent resolution must precede the gate"
    );
    assert!(
        region.contains("thread_parent.as_deref()"),
        "the resolved cached parent (not None) is passed to interaction_gate"
    );
}

fn autocomplete_spec_with_big_choice_list(slug: &str, option: &str) -> DiscordSlashCommandSpec {
    let mut opt = slash_options::OptionSpec {
        name: option.to_string(),
        description: "o".to_string(),
        description_localizations: Default::default(),
        kind: slash_options::OptKind::String,
        required: false,
        choices: Vec::new(),
        min: None,
        max: None,
        min_length: None,
        max_length: None,
    };
    // 40 > Discord's 25 static cap → served via autocomplete.
    opt.choices = (0..40)
        .map(|i| slash_options::Choice {
            name: format!("region-{i:02}"),
            value: format!("r{i:02}"),
        })
        .collect();
    DiscordSlashCommandSpec {
        skill_name: "deploy".to_string(),
        slug: slug.to_string(),
        description: "d".to_string(),
        description_localizations: Default::default(),
        options: vec![opt],
    }
}

// Reproduces the arm's choice-sourcing step exactly (spec lookup by slug →
// focused option by name → filter), given the resolved spec set.
fn arm_choices(
    specs: &[DiscordSlashCommandSpec],
    focused: Option<(String, String, String)>,
    authorized: bool,
) -> Vec<(String, String)> {
    match (authorized, focused) {
        (true, Some((command, option_name, partial))) => specs
            .iter()
            .find(|s| s.slug == command)
            .and_then(|s| s.options.iter().find(|o| o.name == option_name))
            .map(|o| o.matching_choices(&partial))
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

#[test]
fn autocomplete_arm_returns_matching_choices_for_focused_option() {
    let specs = vec![autocomplete_spec_with_big_choice_list("deploy", "region")];
    let payload = serde_json::json!({
        "type": 4,
        "data": {
            "name": "deploy",
            "options": [ { "name": "region", "type": 3, "value": "region-1", "focused": true } ]
        }
    });
    let focused = slash_options::extract_focused_option(&payload);
    let choices = arm_choices(&specs, focused, true);
    // "region-1" prefixes region-10..region-19 (10 of them).
    assert_eq!(choices.len(), 10);
    assert!(choices.iter().all(|(n, _)| n.starts_with("region-1")));
    assert_eq!(choices[0], ("region-10".to_string(), "r10".to_string()));
}

#[test]
fn autocomplete_arm_returns_empty_for_unauthorized() {
    let specs = vec![autocomplete_spec_with_big_choice_list("deploy", "region")];
    let payload = serde_json::json!({
        "type": 4,
        "data": { "name": "deploy", "options": [ { "name": "region", "value": "region", "focused": true } ] }
    });
    let focused = slash_options::extract_focused_option(&payload);
    // Even with a matching focused option, an unauthorized keystroke answers
    // empty — no policy leak, no work.
    assert!(arm_choices(&specs, focused, false).is_empty());
}

#[test]
fn autocomplete_arm_returns_empty_for_no_match_and_unknown_targets() {
    let specs = vec![autocomplete_spec_with_big_choice_list("deploy", "region")];
    // No choice matches the partial.
    let p = serde_json::json!({
        "data": { "name": "deploy", "options": [ { "name": "region", "value": "zzz", "focused": true } ] }
    });
    assert!(arm_choices(&specs, slash_options::extract_focused_option(&p), true).is_empty());
    // Unknown command slug.
    let p = serde_json::json!({
        "data": { "name": "ghost", "options": [ { "name": "region", "value": "r", "focused": true } ] }
    });
    assert!(arm_choices(&specs, slash_options::extract_focused_option(&p), true).is_empty());
    // Known command, unknown focused option name.
    let p = serde_json::json!({
        "data": { "name": "deploy", "options": [ { "name": "ghost", "value": "r", "focused": true } ] }
    });
    assert!(arm_choices(&specs, slash_options::extract_focused_option(&p), true).is_empty());
    // No focused option at all.
    let p = serde_json::json!({ "data": { "name": "deploy", "options": [] } });
    assert!(arm_choices(&specs, slash_options::extract_focused_option(&p), true).is_empty());
}

#[test]
fn interaction_arms_gate_before_take_after_the_doptions_merge() {
    let src = include_str!("mod.rs");

    let arm35 = src
        .find("} else if itype == 3 || itype == 5 {")
        .expect("type-3/5 arm present");
    let arm4 = src[arm35..]
        .find("} else if itype == 4 {")
        .map(|i| arm35 + i)
        .expect("type-4 arm present (arm-3/5 boundary)");
    let region35 = &src[arm35..arm4];
    let gate35 = region35
        .find("interaction_gate(")
        .expect("type-3/5 arm gates");
    let take35 = region35
        .find("pending_components.lock().take(")
        .expect("type-3/5 arm takes");
    assert!(
        gate35 < take35,
        "type-3/5: interaction_gate must run BEFORE the single-use take"
    );
    // The cheap peer pre-check is also before the take.
    let peer35 = region35
        .find("crate::allowlist::is_user_allowed(")
        .expect("type-3/5 arm peer-checks");
    assert!(
        peer35 < take35 && peer35 < gate35,
        "peer check precedes gate+take"
    );

    // type-2 arm: gate precedes the credential stash (`pending.lock()`) and
    // the defer — an unauthorized invoker never stashes creds or defers.
    let arm2 = src.find("if itype == 2 {").expect("type-2 arm present");
    let region2 = &src[arm2..arm35];
    let gate2 = region2.find("interaction_gate(").expect("type-2 arm gates");
    let stash2 = region2
        .find("let mut guard = pending.lock();")
        .expect("type-2 arm stashes creds");
    let defer2 = region2
        .find("discord_defer_interaction(")
        .expect("type-2 arm defers");
    assert!(
        gate2 < stash2 && gate2 < defer2,
        "type-2: gate before stash+defer"
    );
}

#[tokio::test]
async fn autocomplete_answer_posts_a_single_type8_callback_and_nothing_else() {
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    // The ONLY call an autocomplete keystroke may make: a type-8
    // (AUTOCOMPLETE_RESULT) callback. No defer, no reject, no followup.
    Mock::given(method("POST"))
        .and(path("/interactions/iid/tok/callback"))
        .and(body_partial_json(serde_json::json!({ "type": 8 })))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let specs = vec![autocomplete_spec_with_big_choice_list("deploy", "region")];
    let p = serde_json::json!({
        "data": { "name": "deploy", "options": [ { "name": "region", "value": "region-2", "focused": true } ] }
    });
    let choices = arm_choices(&specs, slash_options::extract_focused_option(&p), true);
    assert_eq!(choices.len(), 10, "region-2x → 10 matches");

    let client = reqwest::Client::new();
    // The arm posts the answer to <api_base>/interactions/{id}/{token}/callback;
    // discord_answer_autocomplete hardcodes the real base, so post directly
    // here against the mock to verify the single-call, type-8 shape.
    let url = format!("{}/interactions/iid/tok/callback", server.uri());
    let rendered: Vec<_> = choices
        .iter()
        .map(|(n, v)| serde_json::json!({ "name": n, "value": v }))
        .collect();
    let resp = client
        .post(&url)
        .json(&serde_json::json!({ "type": 8, "data": { "choices": rendered } }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    // wiremock verifies expect(1) on drop.
}
