use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_cron_child_stays_tracked_after_its_parent_is_aborted() {
    let children = TaskTracker::new();
    let ended = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let nested_ended = Arc::clone(&ended);
    let (started, started_rx) = tokio::sync::oneshot::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let parent = CRON_CHILDREN
        .scope(children.clone(), async move {
            spawn_owned_cron_task(async move {
                let _nested = spawn_owned_cron_task(async move {
                    started.send(()).unwrap();
                    // Keep this child alive past its parent's destruction to
                    // prove tracking is inherited, not just abort requested.
                    let _ = release_rx.recv();
                    tokio::task::yield_now().await;
                    nested_ended.store(true, std::sync::atomic::Ordering::SeqCst);
                });
                std::future::pending::<()>().await;
            })
        })
        .await;
    started_rx.await.unwrap();
    parent.abort();
    assert!(parent.await.unwrap_err().is_cancelled());
    children.close();
    assert!(
        !children.is_empty(),
        "the nested child must retain the daemon's tracker"
    );
    release.send(()).unwrap();
    time::timeout(Duration::from_secs(2), children.wait())
        .await
        .unwrap();
    assert!(!ended.load(std::sync::atomic::Ordering::SeqCst));
}

#[cfg(unix)]
async fn shell_fixture(tmp: &TempDir, script: &str) -> (Config, CronJob) {
    let mut config = test_config(tmp).await;
    config
        .risk_profiles
        .get_mut(TEST_AGENT)
        .unwrap()
        .allowed_commands = vec!["sh".into()];
    std::fs::write(config.data_dir.join("fixture.sh"), script).unwrap();
    (config, test_job("sh ./fixture.sh"))
}

#[cfg(unix)]
async fn wait_for_marker(path: &std::path::Path) {
    time::timeout(Duration::from_secs(5), async {
        while !path.exists() {
            time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("shell fixture must start before cancellation");
}

#[cfg(unix)]
#[tokio::test]
async fn noisy_shell_finishes_with_bounded_stdout_and_stderr() {
    let tmp = TempDir::new().unwrap();
    let (config, job) = shell_fixture(
        &tmp,
        "printf 'stdout-start:'\nprintf '%65536s' x\nprintf 'stderr-start:' >&2\nprintf '%65536s' y >&2\nprintf done > completed\n",
    )
    .await;
    let (success, output) = run_job_command(&config, &test_security(&config), &job)
        .await
        .into_result();
    assert!(
        success,
        "capture overflow must not stop an authorized command"
    );
    assert!(config.data_dir.join("completed").exists());
    assert!(output.contains("stdout-start:"));
    assert!(output.contains("stderr-start:"));
    let marker = crate::i18n::get_required_cli_string("cron-shell-output-truncated");
    assert_eq!(output.matches(&marker).count(), 2);
    assert!(output.len() < 2 * crate::cron::store::MAX_CRON_OUTPUT_BYTES + 256);
}

#[cfg(unix)]
#[tokio::test]
async fn shell_timeout_persists_once_releases_claim_and_kills_descendants() {
    let tmp = TempDir::new().unwrap();
    let (mut config, _) = shell_fixture(
        &tmp,
        "printf 'attempt\\n' >> attempts\nprintf started\n(sleep 0.5; printf late > late-marker) &\nsleep 5\n",
    )
    .await;
    config.reliability.scheduler_retries = 2;
    let job = cron::add_shell_job_with_approval(
        &config,
        TEST_AGENT,
        None,
        Schedule::Cron {
            expr: "*/5 * * * *".into(),
            tz: None,
        },
        "sh ./fixture.sh",
        None,
        true,
    )
    .unwrap();
    assert!(cron::claim_job(&config, &job.id, Utc::now()).unwrap());
    let security = test_security(&config);
    let (_, success, output, _, _) = TEST_SHELL_JOB_TIMEOUT
        .scope(
            Duration::from_millis(100),
            execute_and_persist_job(
                &config,
                &security,
                TEST_AGENT,
                &job,
                &unique_component("shell-timeout"),
                None,
            ),
        )
        .await;
    assert!(!success);
    assert!(output.starts_with("job timed out after"));
    assert!(
        output.contains("started"),
        "retain bounded partial diagnostics"
    );
    assert_eq!(
        std::fs::read_to_string(config.data_dir.join("attempts")).unwrap(),
        "attempt\n"
    );
    let runs = cron::list_runs(&config, &job.id, 10).unwrap();
    assert_eq!(runs.len(), 1, "timeout must not replay the command");
    assert_eq!(runs[0].status, "error");
    assert!(cron::claim_job(&config, &job.id, Utc::now()).unwrap());
    time::sleep(Duration::from_millis(650)).await;
    assert!(
        !config.data_dir.join("late-marker").exists(),
        "descendant must be terminated with its shell"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn dropping_shell_run_kills_its_background_descendant() {
    let tmp = TempDir::new().unwrap();
    let (config, job) = shell_fixture(
        &tmp,
        "printf started > started\n(sleep 0.5; printf late > late-marker) &\nsleep 5\n",
    )
    .await;
    let run_config = config.clone();
    let run = zeroclaw_spawn::spawn!(async move { execute_job_now(&run_config, &job).await });
    wait_for_marker(&config.data_dir.join("started")).await;
    run.abort();
    assert!(run.await.unwrap_err().is_cancelled());
    time::sleep(Duration::from_millis(650)).await;
    assert!(!config.data_dir.join("late-marker").exists());
}

/// A real provider/agent/file_write round trip is the positive control. The
/// cancellation case stops the scheduler during startup catch-up, restarts it
/// against the same claimed job, then releases the old provider response. An
/// orphaned old agent would still execute file_write and create the end marker.
#[tokio::test]
async fn scheduler_cancel_and_restart_cannot_resume_the_old_agent() {
    use axum::{Json, Router, extract::State, routing::post};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;
    use zeroclaw_config::schema::{ModelProviderConfig, OllamaModelProviderConfig};

    #[derive(Clone)]
    struct Fixture {
        requests: Arc<AtomicUsize>,
        started: Arc<Notify>,
        release: Arc<Notify>,
    }
    async fn chat(State(fixture): State<Fixture>) -> Json<serde_json::Value> {
        let first = fixture.requests.fetch_add(1, Ordering::SeqCst) == 0;
        let message = if first {
            fixture.started.notify_one();
            fixture.release.notified().await;
            serde_json::json!({"role": "assistant", "content": null, "tool_calls": [{
                "id": "end-marker", "type": "function", "function": {
                    "name": "file_write", "arguments": serde_json::json!({
                        "path": "end-marker.txt", "content": "old agent continued"
                    }).to_string()
                }
            }]})
        } else {
            serde_json::json!({"role": "assistant", "content": "done"})
        };
        Json(serde_json::json!({"choices": [{"message": message}]}))
    }

    for cancel_old in [false, true] {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.reliability.scheduler_retries = 0;
        config.reliability.provider_retries = 0;
        config.memory.backend = "none".into();
        let profile = config.risk_profiles.get_mut(TEST_AGENT).unwrap();
        profile.allowed_tools = Some(vec!["file_write".into()]);
        profile.auto_approve = vec!["file_write".into()];
        let fixture = Fixture {
            requests: Arc::new(AtomicUsize::new(0)),
            started: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/v1/chat/completions", post(chat))
            .with_state(fixture.clone());
        let server = AbortOnDropHandle::new(zeroclaw_spawn::spawn!(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        config.providers.models.ollama.insert(
            "lifecycle".into(),
            OllamaModelProviderConfig {
                base: ModelProviderConfig {
                    uri: Some(format!("http://{addr}")),
                    model: Some("fixture-model".into()),
                    native_tools: Some(true),
                    timeout_secs: Some(10),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        config.agents.get_mut(TEST_AGENT).unwrap().model_provider = "ollama.lifecycle".into();
        let job = cron::add_agent_job(
            &config,
            TEST_AGENT,
            None,
            Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "Write the end marker",
            SessionTarget::Isolated,
            None,
            None,
            false,
            Some(vec!["file_write".into()]),
            false,
        )
        .unwrap();
        let marker = config
            .agent_workspace_dir(TEST_AGENT)
            .join("end-marker.txt");
        if !cancel_old {
            fixture.release.notify_one();
            let (success, output) =
                time::timeout(Duration::from_secs(15), execute_job_now(&config, &job))
                    .await
                    .unwrap();
            assert!(success, "positive control failed: {output}");
            assert!(
                marker.exists(),
                "positive control must actually execute file_write"
            );
        } else {
            let conn = rusqlite::Connection::open(config.data_dir.join("cron/jobs.db")).unwrap();
            conn.execute(
                "UPDATE cron_jobs SET next_run = '2000-01-01T00:00:00Z' WHERE id = ?1",
                [&job.id],
            )
            .unwrap();
            drop(conn);
            let cancel = CancellationToken::new();
            let identities = TaskIdentity::capture_all(&config);
            let old = {
                let config = config.clone();
                let cancel = cancel.clone();
                let identities = identities.clone();
                AbortOnDropHandle::new(zeroclaw_spawn::spawn!(run(
                    config, None, cancel, identities
                )))
            };
            time::timeout(Duration::from_secs(10), fixture.started.notified())
                .await
                .unwrap();
            cancel.cancel();
            time::timeout(Duration::from_secs(2), old)
                .await
                .expect("busy catch-up must observe cancellation")
                .unwrap()
                .unwrap();

            // A completed reload may recover the interrupted run's claim, but
            // only after the old child has actually been destroyed.
            let next_cancel = CancellationToken::new();
            let (tx, mut rx) = tokio::sync::broadcast::channel(8);
            let next = {
                let config = config.clone();
                let cancel = next_cancel.clone();
                AbortOnDropHandle::new(zeroclaw_spawn::spawn!(run(
                    config,
                    Some(tx),
                    cancel,
                    identities
                )))
            };
            let result = time::timeout(Duration::from_secs(10), rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(result["success"], true);
            assert_eq!(result["job_id"], job.id);
            next_cancel.cancel();
            time::timeout(Duration::from_secs(2), next)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            fixture.release.notify_one();
            time::sleep(Duration::from_millis(250)).await;
            assert!(
                !marker.exists(),
                "old agent executed a tool after scheduler restart"
            );
            assert_eq!(fixture.requests.load(Ordering::SeqCst), 2);
        }
        server.abort();
        let _ = server.await;
    }
}
