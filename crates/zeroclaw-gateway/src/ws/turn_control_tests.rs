use super::*;

/// Marker-only behavior in the real HTTP provider. No Conversation injection,
/// no fake gateway output: all frames pass through the runtime streaming path.
pub(super) async fn provider_response(request: &Value) -> Option<axum::response::Response> {
    let text = request["messages"].as_array()?.last()?["content"].to_string();
    if text.contains("s2-delete-marker") {
        let message = json!({"role":"assistant","content":"s2-deleted-output-marker "});
        return Some(if request["stream"] == true {
            ([("content-type", "text/event-stream")], format!("data: {}\n\ndata: [DONE]\n\n", json!({"id":"deletion-response","choices":[{"index":0,"delta":message,"finish_reason":"stop"}]}))).into_response()
        } else {
            axum::Json(json!({"id":"deletion-response","choices":[{"index":0,"message":message,"finish_reason":"stop"}]})).into_response()
        });
    }
    if text.contains("s2-held-provider") {
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    if !text.contains("s2-large-stream") || request["stream"] != true {
        return None;
    }
    let stream = futures_util::stream::unfold(0usize, |index| async move {
        if index > 512 {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
        let bytes = if index == 512 {
            "data: [DONE]\n\n".to_string()
        } else {
            format!(
                "data: {}\n\n",
                json!({"id":"s2-local-stream","choices":[{"index":0,"delta":{"content":format!("{} ", "x".repeat(8191))},"finish_reason":null}]})
            )
        };
        Some((Ok::<_, std::io::Error>(bytes), index + 1))
    });
    Some(
        (
            [("content-type", "text/event-stream")],
            axum::body::Body::from_stream(stream),
        )
            .into_response(),
    )
}

async fn negotiate(client: &mut DeviceWire) -> String {
    client
        .send(json!({"type":"turn_control","version":1,"op":"negotiate"}))
        .await;
    let ready = client.until("turn_ready").await;
    assert_eq!(ready["version"], 1);
    let epoch = ready["epoch"].as_str().unwrap().to_owned();
    assert!(uuid::Uuid::parse_str(&epoch).is_ok());
    epoch
}

fn request(epoch: &str, id: &str, content: &str) -> Value {
    json!({"type":"turn_control","version":1,"op":"message","epoch":epoch,"request_id":id,"content":content})
}

async fn terminal(client: &mut DeviceWire, id: &str, turn: &str) -> Vec<Value> {
    let mut events = Vec::new();
    tokio::time::timeout(LIMIT, async {
        loop {
            let frame = client.next_with_limit(32 * 1024 * 1024).await.unwrap();
            if frame["type"] != "turn_event" {
                assert!(
                    !matches!(frame["type"].as_str(), Some("turn_error" | "turn_gap")),
                    "{frame}"
                );
                continue;
            }
            assert_eq!(frame["request_id"], id);
            assert_eq!(frame["turn_id"], turn);
            assert_eq!(frame["seq"].as_u64(), Some(events.len() as u64 + 1));
            let finished = frame["terminal"] == true;
            events.push(frame);
            if finished {
                break;
            }
        }
    })
    .await
    .expect("turn terminal must be bounded");
    events
}

async fn wait_provider(fixture: &DeviceFixture, count: usize) {
    tokio::time::timeout(LIMIT, async {
        while fixture.seen.lock().len() < count {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn s2_wire_normal_turn_has_correlated_chunks_and_terminal() {
    let fixture = DeviceFixture::new(&["chat"]).await;
    let mut client = fixture.socket(DEVICE_TOKEN).await;
    let epoch = negotiate(&mut client).await;
    let id = uuid::Uuid::new_v4().to_string();
    client.send(request(&epoch, &id, "hello S2")).await;
    let ack = client.until("turn_ack").await;
    assert_eq!(ack["status"], "accepted");
    assert_eq!(ack["request_id"], id);
    assert_eq!(ack["epoch"], epoch);
    let turn = ack["turn_id"].as_str().unwrap();
    let events = terminal(&mut client, &id, turn).await;
    assert!(events.iter().any(|event| event["event"]["type"] == "chunk"));
    assert!(events.iter().any(|event| event["event"]["type"] == "done"));
    assert_eq!(events.last().unwrap()["event"]["outcome"], "done");
    assert_eq!(events.last().unwrap()["stream_complete"], true);
    assert_eq!(fixture.seen.lock().len(), 1);
}

#[tokio::test]
async fn s2_wire_busy_refuses_without_steering_and_late_cancel_is_scoped() {
    let fixture = DeviceFixture::new(&["chat", "cancel"]).await;
    let mut client = fixture.socket(DEVICE_TOKEN).await;
    let epoch = negotiate(&mut client).await;
    let first = uuid::Uuid::new_v4().to_string();
    client.send(request(&epoch, &first, "first")).await;
    let first_ack = client.until("turn_ack").await;
    let old_turn = first_ack["turn_id"].as_str().unwrap();
    terminal(&mut client, &first, old_turn).await;
    let second = uuid::Uuid::new_v4().to_string();
    client
        .send(request(&epoch, &second, "s2-held-provider"))
        .await;
    let second_ack = client.until("turn_ack").await;
    let new_turn = second_ack["turn_id"].as_str().unwrap();
    assert_ne!(old_turn, new_turn);
    wait_provider(&fixture, 2).await;
    let refused = uuid::Uuid::new_v4().to_string();
    client
        .send(request(&epoch, &refused, "must not steer"))
        .await;
    let busy = tokio::time::timeout(Duration::from_secs(1), client.until("turn_ack"))
        .await
        .unwrap();
    assert_eq!(busy["status"], "busy");
    assert_eq!(busy["request_id"], refused);
    client.send(json!({"type":"turn_control","version":1,"op":"cancel","epoch":epoch,"request_id":first,"turn_id":old_turn})).await;
    let cancel = client.until("turn_cancel_ack").await;
    assert_eq!(
        cancel["status"], "stale",
        "old turn must not be cancelled as current"
    );
    let events = terminal(&mut client, &second, new_turn).await;
    assert_eq!(events.last().unwrap()["event"]["outcome"], "done");
    assert_eq!(
        fixture.seen.lock().len(),
        2,
        "busy request must not become steering or queued input"
    );
}

#[tokio::test]
async fn s2_wire_lost_ack_reconnect_recovers_mapping_without_reexecution() {
    let fixture = DeviceFixture::new(&["chat"]).await;
    let mut lost = fixture.socket(DEVICE_TOKEN).await;
    let epoch = negotiate(&mut lost).await;
    let id = uuid::Uuid::new_v4().to_string();
    lost.send(request(&epoch, &id, "s2-held-provider")).await;
    wait_provider(&fixture, 1).await;
    // Deliberately never consume the ACK. Disconnect is not cancellation.
    drop(lost);
    let mut recovered = fixture.socket(DEVICE_TOKEN).await;
    assert_eq!(negotiate(&mut recovered).await, epoch);
    recovered
        .send(request(&epoch, &id, "s2-held-provider"))
        .await;
    let duplicate = recovered.until("turn_ack").await;
    assert_eq!(duplicate["status"], "duplicate");
    let turn = duplicate["turn_id"].as_str().unwrap().to_string();
    recovered.send(json!({"type":"turn_control","version":1,"op":"resume","epoch":epoch,"request_id":id,"after_seq":0})).await;
    let replay = recovered.until("turn_replay").await;
    assert_eq!(replay["turn_id"], turn);
    assert_eq!(replay["gap"], false);
    assert_eq!(fixture.seen.lock().len(), 1);
    // A second reconnect after settlement also retains the same identity.
    tokio::time::sleep(Duration::from_secs(3)).await;
    drop(recovered);
    let mut settled = fixture.socket(DEVICE_TOKEN).await;
    assert_eq!(negotiate(&mut settled).await, epoch);
    settled.send(json!({"type":"turn_control","version":1,"op":"resume","epoch":epoch,"request_id":id,"after_seq":0})).await;
    let replay = settled.until("turn_replay").await;
    assert_eq!(replay["request_id"], id);
    assert_eq!(replay["turn_id"], turn);
    assert_eq!(replay["terminal"], true);
    assert_eq!(replay["gap"], false);
    let events = replay["events"].as_array().unwrap();
    assert!(!events.is_empty());
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event["request_id"], id);
        assert_eq!(event["turn_id"], turn);
        assert_eq!(event["seq"], index as u64 + 1);
    }
    assert_eq!(events.last().unwrap()["terminal"], true);
    assert_eq!(fixture.seen.lock().len(), 1);
    settled
        .send(request(&epoch, &id, "different payload must not run"))
        .await;
    assert_eq!(settled.until("turn_ack").await["status"], "conflict");
    assert_eq!(fixture.seen.lock().len(), 1);
}

#[tokio::test]
async fn s2_wire_unknown_version_epoch_and_unnegotiated_controls_are_inert() {
    let fixture = DeviceFixture::new(&["chat"]).await;
    let mut client = fixture.socket(DEVICE_TOKEN).await;
    let id = uuid::Uuid::new_v4().to_string();
    client
        .send(request(
            &uuid::Uuid::new_v4().to_string(),
            &id,
            "not negotiated",
        ))
        .await;
    assert_eq!(
        client.until("turn_error").await["code"],
        "NEGOTIATION_REQUIRED"
    );
    client
        .send(json!({"type":"turn_control","version":99,"op":"negotiate"}))
        .await;
    assert_eq!(
        client.until("turn_error").await["code"],
        "UNSUPPORTED_VERSION"
    );
    let epoch = negotiate(&mut client).await;
    for _ in 0..2 {
        client.send(json!({"type":"turn_control","version":1,"op":"future_action","epoch":epoch,"request_id":id,"content":"no side effects"})).await;
        assert_eq!(
            client.until("turn_error").await["code"],
            "UNKNOWN_OPERATION"
        );
    }
    client
        .send(request(
            &uuid::Uuid::new_v4().to_string(),
            &id,
            "old process must not revive",
        ))
        .await;
    assert_eq!(client.until("turn_error").await["code"], "EPOCH_MISMATCH");
    assert!(fixture.seen.lock().is_empty());
    client
        .send(request(&epoch, &id, "valid after rejected frames"))
        .await;
    let ack = client.until("turn_ack").await;
    assert_eq!(ack["status"], "accepted");
    terminal(&mut client, &id, ack["turn_id"].as_str().unwrap()).await;
    assert_eq!(
        fixture.seen.lock().len(),
        1,
        "unknown frames must not reserve or poison the request ID"
    );
}

#[tokio::test]
async fn s2_wire_control_authority_and_live_revoke_preserve_s1() {
    let fixture = DeviceFixture::new(&["chat"]).await;
    let mut client = fixture.socket(DEVICE_TOKEN).await;
    let epoch = negotiate(&mut client).await;
    let id = uuid::Uuid::new_v4().to_string();
    client.send(request(&epoch, &id, "s2-held-provider")).await;
    let ack = client.until("turn_ack").await;
    client.send(json!({"type":"turn_control","version":1,"op":"cancel","epoch":epoch,"request_id":id,"turn_id":ack["turn_id"]})).await;
    let denied = client.next().await.unwrap();
    assert_eq!(denied["code"], "BRIDGE_OPERATION_DENIED");
    terminal(&mut client, &id, ack["turn_id"].as_str().unwrap()).await;
    fixture
        .state
        .config
        .write()
        .gateway
        .bridges
        .remove("device");
    client
        .send(request(
            &epoch,
            &uuid::Uuid::new_v4().to_string(),
            "revoked",
        ))
        .await;
    client.denied().await;
    assert_eq!(fixture.seen.lock().len(), 1);
}

#[tokio::test]
async fn s2_wire_negotiation_does_not_change_legacy_subscriber_contract() {
    let fixture = DeviceFixture::new(&["chat"]).await;
    let mut controlled = fixture.socket(DEVICE_TOKEN).await;
    let mut legacy = fixture.socket(OWNER_TOKEN).await;
    let epoch = negotiate(&mut controlled).await;
    let id = uuid::Uuid::new_v4().to_string();
    controlled
        .send(request(&epoch, &id, "coexisting subscribers"))
        .await;
    let ack = controlled.until("turn_ack").await;
    let mut chunk_seen = false;
    let mut done_seen = false;
    for _ in 0..64 {
        let frame = legacy.next().await.unwrap();
        assert!(
            !matches!(
                frame["type"].as_str(),
                Some("turn_ready" | "turn_event" | "turn_ack" | "turn_replay" | "turn_terminal")
            ),
            "extension leaked to unnegotiated subscriber: {frame}"
        );
        if frame["type"] == "chunk" {
            chunk_seen = true;
            assert!(frame.get("turn_id").is_none());
            assert!(frame.get("seq").is_none());
        }
        if frame["type"] == "done" {
            done_seen = true;
            break;
        }
    }
    assert!(
        chunk_seen && done_seen,
        "legacy output must contain both chunk and done"
    );
    terminal(&mut controlled, &id, ack["turn_id"].as_str().unwrap()).await;
    fixture
        .send_turn(
            &mut legacy,
            "legacy-after-negotiation",
            "legacy still works",
        )
        .await;
    assert_eq!(fixture.seen.lock().len(), 2);
}

#[tokio::test]
async fn s2_wire_slow_reader_has_explicit_recovery_gap_without_blocking_other_session() {
    let fixture = DeviceFixture::new(&["chat"]).await;
    fixture
        .state
        .config
        .write()
        .providers
        .models
        .custom
        .get_mut("test")
        .unwrap()
        .base
        .timeout_secs = Some(30);
    let mut slow = fixture.socket(DEVICE_TOKEN).await;
    let epoch = negotiate(&mut slow).await;
    let id = uuid::Uuid::new_v4().to_string();
    let socket = socket2::SockRef::from(&slow.0);
    socket.set_recv_buffer_size(1024).unwrap();
    assert!(
        socket.recv_buffer_size().unwrap() <= 65_536,
        "test requires a genuinely small receive buffer"
    );
    slow.send(request(&epoch, &id, "s2-large-stream")).await;
    wait_provider(&fixture, 1).await;
    // No more reads on this real TCP socket while 4 MiB of whitespace-ended
    // deltas traverse runtime/Conversation. The verified small receive buffer
    // ensures transport backpressure; whitespace makes sanitized chunks visible.
    let (status, mut independent) = DeviceWire::upgrade(
        fixture.address,
        "agent=device-agent&session_id=s2-independent-session",
        OWNER_TOKEN,
    )
    .await;
    assert_eq!(status, 101);
    assert_eq!(
        independent.until("session_start").await["session_id"],
        "s2-independent-session"
    );
    independent.send(json!({"type":"connect"})).await;
    independent.until("connected").await;
    tokio::time::timeout(Duration::from_secs(3), async {
        independent
            .send(json!({"type":"message","id":"independent-turn","content":"unrelated fast turn"}))
            .await;
        assert_eq!(independent.until("ack").await["status"], "accepted");
        assert_eq!(
            independent.until("done").await["full_response"],
            "device response"
        );
    })
    .await
    .expect("slow reader must not block unrelated Conversation");
    let mut recovery = fixture.socket(DEVICE_TOKEN).await;
    assert_eq!(negotiate(&mut recovery).await, epoch);
    let mut snapshot = Value::Null;
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            recovery.send(json!({"type":"turn_control","version":1,"op":"resume","epoch":epoch,"request_id":id,"after_seq":0})).await;
            // Live output can race the replay response. Bound each frame while
            // looking for the authoritative cache snapshot.
            loop {
                let frame = recovery.next_with_limit(32 * 1024 * 1024).await.unwrap();
                if frame["type"] == "turn_replay" { snapshot = frame; break; }
            }
            if snapshot["terminal"] == true { break; }
            // Keep this recovery reader draining while the intentionally slow
            // socket remains unread. Do not create an artificial second lagger.
            loop {
                let frame = recovery.next_with_limit(32 * 1024 * 1024).await.unwrap();
                assert_ne!(frame["type"], "turn_gap", "draining recovery client fell behind");
                if frame["type"] == "turn_event" && frame["terminal"] == true { break; }
            }
        }
    }).await.expect("bounded streamed turn must settle");
    assert_eq!(snapshot["request_id"], id);
    assert_eq!(
        snapshot["gap"], true,
        "overflow must not claim complete replay"
    );
    assert_eq!(snapshot["terminal"], true);
    assert_eq!(
        snapshot["events"].as_array().unwrap().last().unwrap()["stream_complete"],
        false,
        "terminal must not describe a truncated replay as complete"
    );
    assert!(
        snapshot["last_seq"].as_u64().unwrap()
            > snapshot["events"].as_array().unwrap().len() as u64
    );
    assert_eq!(
        fixture.seen.lock().len(),
        2,
        "recovery must never execute again"
    );
    drop(slow);
}

#[tokio::test]
async fn s2_wire_targeted_cancel_and_negotiated_legacy_bypass_are_bounded() {
    let fixture = DeviceFixture::new(&["chat", "cancel"]).await;
    let mut client = fixture.socket(DEVICE_TOKEN).await;
    let epoch = negotiate(&mut client).await;
    let id = uuid::Uuid::new_v4().to_string();
    client.send(request(&epoch, &id, "s2-held-provider")).await;
    let ack = client.until("turn_ack").await;
    wait_provider(&fixture, 1).await;
    client.send(json!({"type":"cancel"})).await;
    assert_eq!(
        client.until("turn_error").await["code"],
        "UNKNOWN_OPERATION"
    );
    client
        .send(json!({"type":"message","content":"legacy steering bypass"}))
        .await;
    assert_eq!(
        client.until("turn_error").await["code"],
        "UNKNOWN_OPERATION"
    );
    client.send(json!({"type":"turn_control","version":1,"op":"cancel","epoch":epoch,"request_id":id,"turn_id":ack["turn_id"]})).await;
    assert_eq!(client.until("turn_cancel_ack").await["status"], "requested");
    let events = terminal(&mut client, &id, ack["turn_id"].as_str().unwrap()).await;
    assert_eq!(events.last().unwrap()["event"]["outcome"], "aborted");
    assert_eq!(fixture.seen.lock().len(), 1);
}

#[tokio::test]
async fn s2_wire_request_recovery_is_bound_to_authenticated_principal() {
    let fixture = DeviceFixture::new(&["chat"]).await;
    let mut device = fixture.socket(DEVICE_TOKEN).await;
    let epoch = negotiate(&mut device).await;
    let id = uuid::Uuid::new_v4().to_string();
    device
        .send(request(&epoch, &id, "private correlation"))
        .await;
    let ack = device.until("turn_ack").await;
    terminal(&mut device, &id, ack["turn_id"].as_str().unwrap()).await;
    let mut owner = fixture.socket(OWNER_TOKEN).await;
    assert_eq!(negotiate(&mut owner).await, epoch);
    for _ in 0..2 {
        owner.send(json!({"type":"turn_control","version":1,"op":"resume","epoch":epoch,"request_id":id,"after_seq":0})).await;
        let reply = owner.until("turn_ack").await;
        assert_eq!(reply["status"], "unknown");
        assert!(reply["turn_id"].is_null());
    }
    assert_eq!(
        fixture.seen.lock().len(),
        1,
        "unknown recovery cannot repeat execution"
    );
}

#[tokio::test]
async fn s2_wire_concurrent_duplicate_has_one_admission_and_one_turn() {
    let fixture = DeviceFixture::new(&["chat"]).await;
    let mut first = fixture.socket(DEVICE_TOKEN).await;
    let mut second = fixture.socket(DEVICE_TOKEN).await;
    let epoch = negotiate(&mut first).await;
    assert_eq!(negotiate(&mut second).await, epoch);
    let id = uuid::Uuid::new_v4().to_string();
    tokio::join!(
        first.send(request(&epoch, &id, "s2-held-provider")),
        second.send(request(&epoch, &id, "s2-held-provider"))
    );
    let (a, b) = tokio::join!(first.until("turn_ack"), second.until("turn_ack"));
    let mut statuses = [a["status"].as_str().unwrap(), b["status"].as_str().unwrap()];
    statuses.sort_unstable();
    assert_eq!(statuses, ["accepted", "duplicate"]);
    assert_eq!(a["turn_id"], b["turn_id"]);
    assert_eq!(a["request_id"], id);
    assert_eq!(b["request_id"], id);
    let turn = a["turn_id"].as_str().unwrap();
    let (a_events, b_events) = tokio::join!(
        terminal(&mut first, &id, turn),
        terminal(&mut second, &id, turn)
    );
    assert_eq!(a_events, b_events);
    assert_eq!(fixture.seen.lock().len(), 1);
}

#[tokio::test]
async fn s2_wire_http_session_delete_erases_replay_without_reexecution() {
    let fixture = DeviceFixture::new(&["chat"]).await;
    let mut device = fixture.socket(DEVICE_TOKEN).await;
    let epoch = negotiate(&mut device).await;
    let id = uuid::Uuid::new_v4().to_string();
    device.send(request(&epoch, &id, "s2-delete-marker")).await;
    let ack = device.until("turn_ack").await;
    let events = terminal(&mut device, &id, ack["turn_id"].as_str().unwrap()).await;
    assert!(
        serde_json::to_string(&events)
            .unwrap()
            .contains("s2-deleted-output-marker")
    );
    drop(device);
    tokio::time::timeout(LIMIT, async {
        while fixture.state.ws_conversations.len() != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("socket teardown must release idle Agent");
    let (status, body) = device_http(
        fixture.address,
        "DELETE",
        "/api/sessions/device-session",
        OWNER_TOKEN,
        "",
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["deleted"],
        true
    );
    assert!(
        !fixture
            .state
            .session_backend
            .as_ref()
            .unwrap()
            .session_exists("gw_device-session")
    );
    let mut reopened = fixture.socket(DEVICE_TOKEN).await;
    assert_eq!(negotiate(&mut reopened).await, epoch);
    reopened.send(json!({"type":"turn_control","version":1,"op":"resume","epoch":epoch,"request_id":id,"after_seq":0})).await;
    let resumed = reopened.next().await.unwrap();
    assert_eq!(
        resumed["type"], "turn_ack",
        "deleted data must not be replayed"
    );
    assert_eq!(resumed["status"], "unknown");
    assert!(resumed["turn_id"].is_null());
    assert!(!resumed.to_string().contains("s2-deleted-output-marker"));
    reopened
        .send(request(&epoch, &id, "s2-delete-marker"))
        .await;
    let repeated = reopened.until("turn_ack").await;
    assert_eq!(
        repeated["status"], "unknown",
        "deleting history must not authorize effect replay"
    );
    assert_eq!(fixture.seen.lock().len(), 1);
}

#[tokio::test]
async fn s2_wire_negotiated_device_cannot_approve_owner_can_complete_same_prompt() {
    let fixture = DeviceFixture::new(&["chat", "answer", "cancel"]).await;
    let mut device = fixture.socket(DEVICE_TOKEN).await;
    let mut owner = fixture.socket(OWNER_TOKEN).await;
    let epoch = negotiate(&mut device).await;
    let id = uuid::Uuid::new_v4().to_string();
    device.send(request(&epoch, &id, "approval-probe")).await;
    let ack = device.until("turn_ack").await;
    assert_eq!(ack["status"], "accepted");
    let approval = device.until("approval_request").await;
    assert_eq!(
        owner.until("approval_request").await["request_id"],
        approval["request_id"]
    );
    for decision in ["approve", "always", "deny"] {
        device.send(json!({"type":"approval_response","request_id":approval["request_id"],"decision":decision})).await;
        assert_eq!(
            device.until("error").await["code"],
            "BRIDGE_OPERATION_DENIED"
        );
        assert!(!fixture.workspace.join("approved.txt").exists());
    }
    owner.send(json!({"type":"approval_response","request_id":approval["request_id"],"decision":"approve"})).await;
    owner.until("done").await;
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("approved.txt")).unwrap(),
        "owner-approved"
    );
    device.send(json!({"type":"turn_control","version":1,"op":"resume","epoch":epoch,"request_id":id,"after_seq":0})).await;
    let replay = device.until("turn_replay").await;
    assert_eq!(replay["turn_id"], ack["turn_id"]);
    assert_eq!(replay["gap"], false);
    assert_eq!(fixture.seen.lock().len(), 2);
}

#[tokio::test]
async fn s2_wire_delete_active_turn_suppresses_late_events_and_allows_fresh_request() {
    let fixture = DeviceFixture::new(&["chat"]).await;
    let mut active = fixture.socket(DEVICE_TOKEN).await;
    let epoch = negotiate(&mut active).await;
    let erased = uuid::Uuid::new_v4().to_string();
    active
        .send(request(&epoch, &erased, "s2-held-provider"))
        .await;
    let ack = active.until("turn_ack").await;
    assert_eq!(ack["status"], "accepted");
    wait_provider(&fixture, 1).await;
    let (status, _) = device_http(
        fixture.address,
        "DELETE",
        "/api/sessions/device-session",
        OWNER_TOKEN,
        "",
    )
    .await;
    assert_eq!(status, 200);
    // The provider was held before emitting any content. After a successful
    // deletion neither cancellation output nor late completion may leak through
    // this already-attached negotiated subscriber.
    for attempt in 0..2 {
        if attempt == 1 {
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
        active.send(json!({"type":"turn_control","version":1,"op":"resume","epoch":epoch,"request_id":erased,"after_seq":0})).await;
        let reply = tokio::time::timeout(LIMIT, async {
            loop {
                let frame = active.next().await.unwrap();
                assert!(
                    !(frame["type"] == "turn_event" && frame["request_id"] == erased),
                    "deleted active turn emitted late content: {frame}"
                );
                if frame["type"] == "turn_ack" {
                    break frame;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(reply["status"], "unknown");
        assert!(reply["turn_id"].is_null());
    }
    drop(active);
    tokio::time::timeout(LIMIT, async {
        while fixture.state.ws_conversations.len() != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let mut fresh = fixture.socket(DEVICE_TOKEN).await;
    assert_eq!(negotiate(&mut fresh).await, epoch);
    fresh
        .send(request(&epoch, &erased, "s2-held-provider"))
        .await;
    assert_eq!(fresh.until("turn_ack").await["status"], "unknown");
    let fresh_id = uuid::Uuid::new_v4().to_string();
    fresh
        .send(request(
            &epoch,
            &fresh_id,
            "new explicit request after deletion",
        ))
        .await;
    let fresh_ack = fresh.until("turn_ack").await;
    assert_eq!(fresh_ack["status"], "accepted");
    let events = terminal(
        &mut fresh,
        &fresh_id,
        fresh_ack["turn_id"].as_str().unwrap(),
    )
    .await;
    assert_eq!(events.last().unwrap()["event"]["outcome"], "done");
    assert_eq!(
        fixture.seen.lock().len(),
        2,
        "only the new explicit ID may run after deletion"
    );
}
