#[cfg(test)]
use super::*;
use chrono::Duration as ChronoDuration;
use tempfile::TempDir;
use zeroclaw_config::schema::Config;

fn test_config(tmp: &TempDir) -> Config {
    let config = Config {
        data_dir: tmp.path().join("data"),
        config_path: tmp.path().join("config.toml"),
        ..Config::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    config
}

fn cron_dir(config: &Config) -> std::path::PathBuf {
    config.data_dir.join("cron")
}

fn cron_db(config: &Config) -> std::path::PathBuf {
    cron_dir(config).join("jobs.db")
}

async fn recv_log_event(
    rx: &mut tokio::sync::broadcast::Receiver<serde_json::Value>,
    message: &str,
    job_id: &str,
) -> serde_json::Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let step = remaining.min(std::time::Duration::from_millis(50));
        match tokio::time::timeout(step, rx.recv()).await {
            Ok(Ok(value))
                if value
                    .get("message")
                    .and_then(|v| v.as_str())
                    .is_some_and(|candidate| candidate == message)
                    && value
                        .get("attributes")
                        .and_then(|a| a.get("job_id"))
                        .and_then(|v| v.as_str())
                        .is_some_and(|id| id == job_id) =>
            {
                return value;
            }
            Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => break,
            Err(_elapsed) => {}
        }
    }
    panic!("did not find log event: {message} for job {job_id}");
}

#[test]
fn read_only_queries_on_empty_workspace_do_not_initialize_cron_db() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);

    assert!(list_jobs(&config).unwrap().is_empty());
    assert!(due_jobs(&config, Utc::now()).unwrap().is_empty());
    assert!(all_overdue_jobs(&config, Utc::now()).unwrap().is_empty());
    assert!(list_runs(&config, "missing", 10).unwrap().is_empty());

    let err = get_job(&config, "missing").unwrap_err();
    assert!(err.to_string().contains("not found"));

    assert!(
        !cron_dir(&config).exists(),
        "read-only queries should not create the cron directory"
    );
    assert!(
        !cron_db(&config).exists(),
        "read-only queries should not create jobs.db"
    );
}

#[test]
fn first_write_initializes_schema_and_follow_up_reads_work() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);

    let job = add_job(&config, "test-agent", "*/5 * * * *", "echo ok").unwrap();

    assert!(cron_db(&config).exists());
    assert_eq!(get_job(&config, &job.id).unwrap().id, job.id);
    assert_eq!(list_jobs(&config).unwrap().len(), 1);
}

/// Force a job's `next_run` into the past so it is selected by `due_jobs`
/// without waiting for its real schedule.
fn force_due(config: &Config, job_id: &str) {
    let past = (Utc::now() - ChronoDuration::hours(1)).to_rfc3339();
    with_initialized_connection(config, |conn| {
        conn.execute(
            "UPDATE cron_jobs SET next_run = ?1 WHERE id = ?2",
            params![past, job_id],
        )?;
        Ok(())
    })
    .unwrap();
}

#[test]
fn claim_job_is_atomic_and_blocks_second_claim() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let job = add_job(&config, "test-agent", "*/5 * * * *", "echo ok").unwrap();
    let now = Utc::now();

    assert!(
        claim_job(&config, &job.id, now).unwrap(),
        "first claim should win"
    );
    assert!(
        !claim_job(&config, &job.id, now).unwrap(),
        "second claim must fail while the job is locked"
    );

    release_job(&config, &job.id).unwrap();
    assert!(
        claim_job(&config, &job.id, now).unwrap(),
        "claim should win again after release"
    );
}

#[test]
fn due_jobs_skips_claimed_jobs() {
    // a job that is in flight must not be selected
    // again by the scheduler while its previous run is still running.
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let job = add_job(&config, "test-agent", "*/5 * * * *", "echo ok").unwrap();
    force_due(&config, &job.id);
    let now = Utc::now();

    assert_eq!(
        due_jobs(&config, now).unwrap().len(),
        1,
        "job is due before being claimed"
    );
    assert_eq!(all_overdue_jobs(&config, now).unwrap().len(), 1);

    assert!(claim_job(&config, &job.id, now).unwrap());

    assert!(
        due_jobs(&config, now).unwrap().is_empty(),
        "a claimed (in-flight) job must not be re-selected by due_jobs"
    );
    assert!(
        all_overdue_jobs(&config, now).unwrap().is_empty(),
        "a claimed (in-flight) job must not be re-selected by the catch-up path"
    );

    release_job(&config, &job.id).unwrap();
    assert_eq!(
        due_jobs(&config, now).unwrap().len(),
        1,
        "after release the job is due again until it is rescheduled"
    );
}

#[test]
fn clear_stale_locks_releases_in_flight_locks() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let job = add_job(&config, "test-agent", "*/5 * * * *", "echo ok").unwrap();
    force_due(&config, &job.id);
    let now = Utc::now();

    assert!(claim_job(&config, &job.id, now).unwrap());
    assert!(due_jobs(&config, now).unwrap().is_empty());

    assert_eq!(
        clear_stale_locks(&config).unwrap(),
        1,
        "the one in-flight lock should be cleared"
    );
    assert_eq!(
        due_jobs(&config, now).unwrap().len(),
        1,
        "after clearing the stale lock the job is eligible again"
    );
    assert_eq!(
        clear_stale_locks(&config).unwrap(),
        0,
        "clearing again when idle releases nothing"
    );
}

#[test]
fn clear_stale_locks_on_empty_workspace_does_not_create_db() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);

    assert_eq!(clear_stale_locks(&config).unwrap(), 0);
    assert!(
        !cron_db(&config).exists(),
        "clear_stale_locks must not create the cron DB on an empty workspace"
    );
}

#[test]
fn reclaim_stale_locks_only_clears_locks_older_than_ttl() {
    // A hung or crashed run leaves `locked_at` behind. The poll-loop
    // reclaim must free only locks past the TTL window and leave live
    // (in-flight) locks untouched.
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let fresh = add_job(&config, "test-agent", "*/5 * * * *", "echo fresh").unwrap();
    let stale = add_job(&config, "test-agent", "*/5 * * * *", "echo stale").unwrap();
    force_due(&config, &fresh.id);
    force_due(&config, &stale.id);

    let now = Utc::now();
    // The stale lock was taken an hour ago, the fresh one just now.
    let stale_locked_at = now - chrono::Duration::seconds(3600);
    assert!(claim_job(&config, &stale.id, stale_locked_at).unwrap());
    assert!(claim_job(&config, &fresh.id, now).unwrap());
    // Both are locked → neither is due.
    assert!(due_jobs(&config, now).unwrap().is_empty());

    // Reclaim with a 30-minute TTL: only the hour-old lock is touched.
    assert_eq!(
        reclaim_stale_locks(&config, now, std::time::Duration::from_secs(30 * 60)).unwrap(),
        1,
        "only the TTL-expired lock is reclaimed"
    );

    // The stale job is due again; the fresh one is still locked.
    let due = due_jobs(&config, now).unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].id, stale.id);
}

#[test]
fn reclaim_stale_locks_makes_expired_lease_runnable_again() {
    // Red test: an expired lease must be reclaimable by another run.
    // After reclaim, due_jobs selects the job again and a fresh claim
    // succeeds.
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let job = add_job(&config, "test-agent", "*/5 * * * *", "echo ok").unwrap();
    force_due(&config, &job.id);

    let now = Utc::now();
    // Simulate a run that claimed the lock 2 hours ago and then hung.
    let hung_locked_at = now - chrono::Duration::seconds(2 * 3600);
    assert!(claim_job(&config, &job.id, hung_locked_at).unwrap());
    assert!(due_jobs(&config, now).unwrap().is_empty());

    // Poll-loop reclaim frees the expired lease.
    assert_eq!(
        reclaim_stale_locks(&config, now, std::time::Duration::from_secs(30 * 60)).unwrap(),
        1
    );

    // A new run can now claim and release the job normally.
    assert!(
        claim_job(&config, &job.id, now).unwrap(),
        "after reclaim a fresh claim must succeed"
    );
    release_job(&config, &job.id).unwrap();
}

#[test]
fn empty_declarative_sync_on_empty_workspace_does_not_initialize_cron_db() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);

    sync_declarative_jobs(&config, &std::collections::HashMap::new()).unwrap();

    assert!(
        !cron_dir(&config).exists(),
        "empty declarative sync should not create the cron directory"
    );
    assert!(
        !cron_db(&config).exists(),
        "empty declarative sync should not create jobs.db"
    );
}

#[test]
fn read_existing_old_schema_db_migrates_before_querying_new_columns() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let cron_dir = cron_dir(&config);
    std::fs::create_dir_all(&cron_dir).unwrap();
    let db_path = cron_db(&config);
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch(
        "CREATE TABLE cron_jobs (
            id               TEXT PRIMARY KEY,
            expression       TEXT NOT NULL,
            command          TEXT NOT NULL,
            schedule         TEXT,
            job_type         TEXT NOT NULL DEFAULT 'shell',
            prompt           TEXT,
            name             TEXT,
            session_target   TEXT NOT NULL DEFAULT 'isolated',
            model            TEXT,
            enabled          INTEGER NOT NULL DEFAULT 1,
            delivery         TEXT,
            delete_after_run INTEGER NOT NULL DEFAULT 0,
            allowed_tools    TEXT,
            created_at       TEXT NOT NULL,
            next_run         TEXT NOT NULL,
            last_run         TEXT,
            last_status      TEXT,
            last_output      TEXT
        );",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO cron_jobs (
            id, expression, command, schedule, job_type, session_target,
            enabled, delete_after_run, created_at, next_run
         ) VALUES (?1, ?2, ?3, ?4, 'shell', 'isolated', 1, 0, ?5, ?6)",
        params![
            "legacy-schema",
            "*/5 * * * *",
            "echo legacy",
            Option::<String>::None,
            Utc::now().to_rfc3339(),
            (Utc::now() + ChronoDuration::minutes(5)).to_rfc3339(),
        ],
    )
    .unwrap();
    drop(conn);

    let jobs = list_jobs(&config).unwrap();

    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].id, "legacy-schema");
    assert_eq!(jobs[0].source, "imperative");
    assert!(jobs[0].uses_memory);

    let conn = Connection::open(&db_path).unwrap();
    let columns: Vec<String> = conn
        .prepare("PRAGMA table_info(cron_jobs)")
        .unwrap()
        .query_map([], |row| row.get(1))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(columns.iter().any(|name| name == "source"));
    assert!(columns.iter().any(|name| name == "uses_memory"));
}

#[test]
fn add_job_accepts_five_field_expression() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);

    let job = add_job(&config, "test-agent", "*/5 * * * *", "echo ok").unwrap();
    assert_eq!(job.expression, "*/5 * * * *");
    assert_eq!(job.command, "echo ok");
    assert!(matches!(job.schedule, Schedule::Cron { .. }));
}

#[test]
fn add_shell_job_marks_at_schedule_for_auto_delete() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);

    let one_shot = add_shell_job(
        &config,
        "default",
        None,
        Schedule::At {
            at: Utc::now() + ChronoDuration::minutes(10),
        },
        "echo once",
        None,
    )
    .unwrap();
    assert!(one_shot.delete_after_run);

    let recurring = add_shell_job(
        &config,
        "default",
        None,
        Schedule::Every { every_ms: 60_000 },
        "echo recurring",
        None,
    )
    .unwrap();
    assert!(!recurring.delete_after_run);
}

#[test]
fn add_shell_job_persists_delivery() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);

    let job = add_shell_job(
        &config,
        "default",
        Some("deliver-shell".into()),
        Schedule::Cron {
            expr: "*/5 * * * *".into(),
            tz: None,
        },
        "echo delivered",
        Some(DeliveryConfig {
            mode: "announce".into(),
            channel: Some("discord".into()),
            to: Some("1234567890".into()),
            thread_id: None,
            best_effort: true,
        }),
    )
    .unwrap();

    assert_eq!(job.delivery.mode, "announce");
    assert_eq!(job.delivery.channel.as_deref(), Some("discord"));
    assert_eq!(job.delivery.to.as_deref(), Some("1234567890"));

    let stored = get_job(&config, &job.id).unwrap();
    assert_eq!(stored.delivery.mode, "announce");
    assert_eq!(stored.delivery.channel.as_deref(), Some("discord"));
    assert_eq!(stored.delivery.to.as_deref(), Some("1234567890"));
}

#[test]
fn add_agent_job_rejects_invalid_announce_delivery() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);

    let err = add_agent_job(
        &config,
        "default",
        Some("deliver-agent".into()),
        Schedule::Cron {
            expr: "*/5 * * * *".into(),
            tz: None,
        },
        "summarize logs",
        SessionTarget::Isolated,
        None,
        Some(DeliveryConfig {
            mode: "announce".into(),
            channel: Some("discord".into()),
            to: None,
            thread_id: None,
            best_effort: true,
        }),
        false,
        None,
        true,
    )
    .unwrap_err();

    assert!(err.to_string().contains("delivery.to is required"));
}

#[test]
fn update_job_rejects_invalid_announce_delivery() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);

    let job = add_shell_job(
        &config,
        "default",
        Some("deliver-shell".into()),
        Schedule::Cron {
            expr: "*/5 * * * *".into(),
            tz: None,
        },
        "echo ok",
        None,
    )
    .unwrap();

    let err = update_job(
        &config,
        &job.id,
        CronJobPatch {
            delivery: Some(DeliveryConfig {
                mode: "announce".into(),
                channel: Some("discord".into()),
                to: None,
                thread_id: None,
                best_effort: true,
            }),
            ..CronJobPatch::default()
        },
    )
    .unwrap_err();

    assert!(err.to_string().contains("delivery.to is required"));
    let stored = get_job(&config, &job.id).unwrap();
    assert_ne!(stored.delivery.mode, "announce");
}

#[test]
fn add_shell_job_rejects_invalid_delivery_mode() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);

    let err = add_shell_job(
        &config,
        "default",
        Some("deliver-shell".into()),
        Schedule::Cron {
            expr: "*/5 * * * *".into(),
            tz: None,
        },
        "echo delivered",
        Some(DeliveryConfig {
            mode: "annouce".into(),
            channel: Some("discord".into()),
            to: Some("1234567890".into()),
            thread_id: None,
            best_effort: true,
        }),
    )
    .unwrap_err();

    assert!(err.to_string().contains("unsupported delivery mode"));
}

#[test]
fn add_list_remove_roundtrip() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);

    let job = add_job(&config, "test-agent", "*/10 * * * *", "echo roundtrip").unwrap();
    let listed = list_jobs(&config).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, job.id);

    remove_job(&config, &job.id).unwrap();
    assert!(list_jobs(&config).unwrap().is_empty());
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn remove_job_emits_structured_cron_delete_event() {
    let _writer_guard = zeroclaw_log::__private_test_writer_lock();
    let _hook_guard = zeroclaw_log::__private_test_hook_lock();
    zeroclaw_log::try_install_capture_subscriber();
    let mut rx = zeroclaw_log::subscribe_or_install();
    while rx.try_recv().is_ok() {}

    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let job = add_job(&config, "test-agent", "*/10 * * * *", "echo roundtrip").unwrap();

    remove_job(&config, &job.id).unwrap();

    let value = recv_log_event(&mut rx, "Removed cron job", &job.id).await;
    assert_eq!(value["event"]["category"], "cron");
    assert_eq!(value["event"]["action"], "delete");
    assert_eq!(value["event"]["outcome"], "success");
    assert_eq!(value["attributes"]["job_id"], job.id);
}

#[test]
fn due_jobs_filters_by_timestamp_and_enabled() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);

    let job = add_job(&config, "test-agent", "* * * * *", "echo due").unwrap();

    let before_next_run = job.next_run - ChronoDuration::milliseconds(1);
    let due_now = due_jobs(&config, before_next_run).unwrap();
    assert!(due_now.is_empty(), "new job should not be due immediately");

    let far_future = Utc::now() + ChronoDuration::days(365);
    let due_future = due_jobs(&config, far_future).unwrap();
    assert_eq!(due_future.len(), 1, "job should be due in far future");

    let _ = update_job(
        &config,
        &job.id,
        CronJobPatch {
            enabled: Some(false),
            ..CronJobPatch::default()
        },
    )
    .unwrap();
    let due_after_disable = due_jobs(&config, far_future).unwrap();
    assert!(due_after_disable.is_empty());
}

#[test]
fn due_jobs_respects_scheduler_max_tasks_limit() {
    let tmp = TempDir::new().unwrap();
    let mut config = test_config(&tmp);
    config.scheduler.max_tasks = 2;

    let _ = add_job(&config, "test-agent", "* * * * *", "echo due-1").unwrap();
    let _ = add_job(&config, "test-agent", "* * * * *", "echo due-2").unwrap();
    let _ = add_job(&config, "test-agent", "* * * * *", "echo due-3").unwrap();

    let far_future = Utc::now() + ChronoDuration::days(365);
    let due = due_jobs(&config, far_future).unwrap();
    assert_eq!(due.len(), 2);
}

#[test]
fn all_overdue_jobs_ignores_max_tasks_limit() {
    let tmp = TempDir::new().unwrap();
    let mut config = test_config(&tmp);
    config.scheduler.max_tasks = 2;

    let _ = add_job(&config, "test-agent", "* * * * *", "echo ov-1").unwrap();
    let _ = add_job(&config, "test-agent", "* * * * *", "echo ov-2").unwrap();
    let _ = add_job(&config, "test-agent", "* * * * *", "echo ov-3").unwrap();

    let far_future = Utc::now() + ChronoDuration::days(365);
    // due_jobs respects the limit
    let due = due_jobs(&config, far_future).unwrap();
    assert_eq!(due.len(), 2);
    // all_overdue_jobs returns everything
    let overdue = all_overdue_jobs(&config, far_future).unwrap();
    assert_eq!(overdue.len(), 3);
}

#[test]
fn all_overdue_jobs_excludes_disabled_jobs() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);

    let job = add_job(&config, "test-agent", "* * * * *", "echo disabled").unwrap();
    let _ = update_job(
        &config,
        &job.id,
        CronJobPatch {
            enabled: Some(false),
            ..CronJobPatch::default()
        },
    )
    .unwrap();

    let far_future = Utc::now() + ChronoDuration::days(365);
    let overdue = all_overdue_jobs(&config, far_future).unwrap();
    assert!(overdue.is_empty());
}

#[test]
fn add_agent_job_persists_allowed_tools() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);

    let job = add_agent_job(
        &config,
        "default",
        Some("agent".into()),
        Schedule::Every { every_ms: 60_000 },
        "do work",
        SessionTarget::Isolated,
        None,
        None,
        false,
        Some(vec!["file_read".into(), "web_search".into()]),
        true,
    )
    .unwrap();

    assert_eq!(
        job.allowed_tools,
        Some(vec!["file_read".into(), "web_search".into()])
    );

    let stored = get_job(&config, &job.id).unwrap();
    assert_eq!(stored.allowed_tools, job.allowed_tools);
}

#[test]
fn update_job_persists_allowed_tools_patch() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);

    let job = add_agent_job(
        &config,
        "default",
        Some("agent".into()),
        Schedule::Every { every_ms: 60_000 },
        "do work",
        SessionTarget::Isolated,
        None,
        None,
        false,
        None,
        true,
    )
    .unwrap();

    let updated = update_job(
        &config,
        &job.id,
        CronJobPatch {
            allowed_tools: Some(vec!["shell".into()]),
            ..CronJobPatch::default()
        },
    )
    .unwrap();

    assert_eq!(updated.allowed_tools, Some(vec!["shell".into()]));
    assert_eq!(
        get_job(&config, &job.id).unwrap().allowed_tools,
        Some(vec!["shell".into()])
    );
}

#[test]
fn update_job_empty_allowed_tools_patch_is_deny_all() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);

    let job = add_agent_job(
        &config,
        "default",
        Some("agent".into()),
        Schedule::Every { every_ms: 60_000 },
        "do work",
        SessionTarget::Isolated,
        None,
        None,
        false,
        Some(vec!["shell".into()]),
        true,
    )
    .unwrap();

    let updated = update_job(
        &config,
        &job.id,
        CronJobPatch {
            allowed_tools: Some(vec![]),
            ..CronJobPatch::default()
        },
    )
    .unwrap();

    assert_eq!(
        updated.allowed_tools,
        Some(vec![]),
        "explicit empty allowlist must persist as deny-all, not None"
    );
    assert_eq!(
        get_job(&config, &job.id).unwrap().allowed_tools,
        Some(vec![])
    );
}

#[test]
fn reschedule_after_run_persists_last_status_and_last_run() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);

    let job = add_job(&config, "test-agent", "*/15 * * * *", "echo run").unwrap();
    reschedule_after_run(&config, &job, false, "failed output").unwrap();

    let listed = list_jobs(&config).unwrap();
    let stored = listed.iter().find(|j| j.id == job.id).unwrap();
    assert_eq!(stored.last_status.as_deref(), Some("error"));
    assert!(stored.last_run.is_some());
    assert_eq!(stored.last_output.as_deref(), Some("failed output"));
}

#[test]
fn job_type_from_sql_reads_valid_value() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let now = Utc::now();

    with_initialized_connection(&config, |conn| {
        conn.execute(
            "INSERT INTO cron_jobs (id, expression, command, schedule, job_type, created_at, next_run)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                "job-type-valid",
                "*/5 * * * *",
                "echo ok",
                Option::<String>::None,
                "agent",
                now.to_rfc3339(),
                (now + ChronoDuration::minutes(5)).to_rfc3339(),
            ],
        )?;
        Ok(())
    })
    .unwrap();

    let job = get_job(&config, "job-type-valid").unwrap();
    assert_eq!(job.job_type, JobType::Agent);
}

#[test]
fn job_type_from_sql_rejects_invalid_value() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let now = Utc::now();

    with_initialized_connection(&config, |conn| {
        conn.execute(
            "INSERT INTO cron_jobs (id, expression, command, schedule, job_type, created_at, next_run)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                "job-type-invalid",
                "*/5 * * * *",
                "echo ok",
                Option::<String>::None,
                "unknown",
                now.to_rfc3339(),
                (now + ChronoDuration::minutes(5)).to_rfc3339(),
            ],
        )?;
        Ok(())
    })
    .unwrap();

    assert!(get_job(&config, "job-type-invalid").is_err());
}

#[test]
fn migration_falls_back_to_legacy_expression() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);

    with_initialized_connection(&config, |conn| {
        conn.execute(
            "INSERT INTO cron_jobs (id, expression, command, created_at, next_run)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                "legacy-id",
                "*/5 * * * *",
                "echo legacy",
                Utc::now().to_rfc3339(),
                (Utc::now() + ChronoDuration::minutes(5)).to_rfc3339(),
            ],
        )?;
        conn.execute(
            "UPDATE cron_jobs SET schedule = NULL WHERE id = 'legacy-id'",
            [],
        )?;
        Ok(())
    })
    .unwrap();

    let job = get_job(&config, "legacy-id").unwrap();
    assert!(matches!(job.schedule, Schedule::Cron { .. }));
}

#[test]
fn record_and_prune_runs() {
    let tmp = TempDir::new().unwrap();
    let mut config = test_config(&tmp);
    config.scheduler.max_run_history = 2;
    let job = add_job(&config, "test-agent", "*/5 * * * *", "echo ok").unwrap();
    let base = Utc::now();

    for idx in 0..3 {
        let start = base + ChronoDuration::seconds(idx);
        let end = start + ChronoDuration::milliseconds(100);
        record_run(&config, &job.id, start, end, "ok", Some("done"), 100).unwrap();
    }

    let runs = list_runs(&config, &job.id, 10).unwrap();
    assert_eq!(runs.len(), 2);
}

#[test]
fn remove_job_cascades_run_history() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let job = add_job(&config, "test-agent", "*/5 * * * *", "echo ok").unwrap();
    let start = Utc::now();
    record_run(
        &config,
        &job.id,
        start,
        start + ChronoDuration::milliseconds(5),
        "ok",
        Some("ok"),
        5,
    )
    .unwrap();

    remove_job(&config, &job.id).unwrap();
    let runs = list_runs(&config, &job.id, 10).unwrap();
    assert!(runs.is_empty());
}

#[test]
fn record_run_truncates_large_output() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let job = add_job(&config, "test-agent", "*/5 * * * *", "echo trunc").unwrap();
    let output = "x".repeat(MAX_CRON_OUTPUT_BYTES + 512);

    record_run(
        &config,
        &job.id,
        Utc::now(),
        Utc::now(),
        "ok",
        Some(&output),
        1,
    )
    .unwrap();

    let runs = list_runs(&config, &job.id, 1).unwrap();
    let stored = runs[0].output.as_deref().unwrap_or_default();
    assert!(stored.ends_with(TRUNCATED_OUTPUT_MARKER));
    assert!(stored.len() <= MAX_CRON_OUTPUT_BYTES);
}

#[test]
fn reschedule_after_run_disables_at_schedule_job() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let at = Utc::now() + ChronoDuration::minutes(10);
    let job = add_shell_job(
        &config,
        "test-agent",
        None,
        Schedule::At { at },
        "echo once",
        None,
    )
    .unwrap();

    reschedule_after_run(&config, &job, true, "done").unwrap();

    let stored = get_job(&config, &job.id).unwrap();
    assert!(
        !stored.enabled,
        "At schedule job should be disabled after reschedule"
    );
    assert_eq!(stored.last_status.as_deref(), Some("ok"));
}

#[test]
fn reschedule_after_run_disables_at_schedule_job_on_failure() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let at = Utc::now() + ChronoDuration::minutes(10);
    let job = add_shell_job(
        &config,
        "test-agent",
        None,
        Schedule::At { at },
        "echo once",
        None,
    )
    .unwrap();

    reschedule_after_run(&config, &job, false, "failed").unwrap();

    let stored = get_job(&config, &job.id).unwrap();
    assert!(
        !stored.enabled,
        "At schedule job should be disabled after reschedule even on failure"
    );
    assert_eq!(stored.last_status.as_deref(), Some("error"));
    assert_eq!(stored.last_output.as_deref(), Some("failed"));
}

#[test]
fn reschedule_after_run_truncates_last_output() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let job = add_job(&config, "test-agent", "*/5 * * * *", "echo trunc").unwrap();
    let output = "y".repeat(MAX_CRON_OUTPUT_BYTES + 1024);

    reschedule_after_run(&config, &job, false, &output).unwrap();

    let stored = get_job(&config, &job.id).unwrap();
    let last_output = stored.last_output.as_deref().unwrap_or_default();
    assert!(last_output.ends_with(TRUNCATED_OUTPUT_MARKER));
    assert!(last_output.len() <= MAX_CRON_OUTPUT_BYTES);
}

// ── Declarative cron job sync tests ──────────────────────────

fn make_shell_decl(
    id: &str,
    expr: &str,
    cmd: &str,
) -> (String, zeroclaw_config::schema::CronJobDecl) {
    (
        id.to_string(),
        zeroclaw_config::schema::CronJobDecl {
            name: Some(format!("decl-{id}")),
            job_type: "shell".to_string(),
            schedule: zeroclaw_config::schema::CronScheduleDecl::Cron {
                expr: expr.to_string(),
                tz: None,
            },
            command: Some(cmd.to_string()),
            prompt: None,
            enabled: true,
            model: None,
            allowed_tools: None,
            uses_memory: true,
            session_target: None,
            delivery: None,
        },
    )
}

fn make_agent_decl(
    id: &str,
    expr: &str,
    prompt: &str,
) -> (String, zeroclaw_config::schema::CronJobDecl) {
    (
        id.to_string(),
        zeroclaw_config::schema::CronJobDecl {
            name: Some(format!("decl-{id}")),
            job_type: "agent".to_string(),
            schedule: zeroclaw_config::schema::CronScheduleDecl::Cron {
                expr: expr.to_string(),
                tz: None,
            },
            command: None,
            prompt: Some(prompt.to_string()),
            enabled: true,
            model: None,
            allowed_tools: None,
            uses_memory: true,
            session_target: None,
            delivery: None,
        },
    )
}

fn decls_map(
    items: Vec<(String, zeroclaw_config::schema::CronJobDecl)>,
) -> std::collections::HashMap<String, zeroclaw_config::schema::CronJobDecl> {
    items.into_iter().collect()
}

/// Seed an enabled agent that claims `ids` via its `cron_jobs` list so
/// `sync_declarative_jobs` can resolve an owning agent for each entry.
fn seed_claiming_agent(config: &mut Config, ids: &[&str]) {
    config.agents.insert(
        "test-agent".to_string(),
        zeroclaw_config::schema::AliasedAgentConfig {
            enabled: true,
            cron_jobs: ids.iter().map(|s| (*s).to_string()).collect(),
            ..Default::default()
        },
    );
}

#[test]
fn sync_inserts_new_declarative_job() {
    let tmp = TempDir::new().unwrap();
    let mut config = test_config(&tmp);
    seed_claiming_agent(&mut config, &["daily-backup"]);

    let decls = decls_map(vec![make_shell_decl(
        "daily-backup",
        "0 2 * * *",
        "echo backup",
    )]);
    sync_declarative_jobs(&config, &decls).unwrap();

    let job = get_job(&config, "daily-backup").unwrap();
    assert_eq!(job.command, "echo backup");
    assert_eq!(job.source, "declarative");
    assert_eq!(job.name.as_deref(), Some("decl-daily-backup"));
}

#[test]
fn sync_updates_existing_declarative_job() {
    let tmp = TempDir::new().unwrap();
    let mut config = test_config(&tmp);
    seed_claiming_agent(&mut config, &["updatable"]);

    let decls = decls_map(vec![make_shell_decl("updatable", "0 2 * * *", "echo v1")]);
    sync_declarative_jobs(&config, &decls).unwrap();

    let job_v1 = get_job(&config, "updatable").unwrap();
    assert_eq!(job_v1.command, "echo v1");

    let decls_v2 = decls_map(vec![make_shell_decl("updatable", "0 3 * * *", "echo v2")]);
    sync_declarative_jobs(&config, &decls_v2).unwrap();

    let job_v2 = get_job(&config, "updatable").unwrap();
    assert_eq!(job_v2.command, "echo v2");
    assert_eq!(job_v2.expression, "0 3 * * *");
    assert_eq!(job_v2.source, "declarative");
}

#[test]
fn sync_does_not_delete_imperative_jobs() {
    let tmp = TempDir::new().unwrap();
    let mut config = test_config(&tmp);
    seed_claiming_agent(&mut config, &["my-decl"]);

    // Create an imperative job via the normal API.
    let imperative = add_job(&config, "test-agent", "*/10 * * * *", "echo imperative").unwrap();

    // Sync declarative jobs (none of which match the imperative job).
    let decls = decls_map(vec![make_shell_decl("my-decl", "0 2 * * *", "echo decl")]);
    sync_declarative_jobs(&config, &decls).unwrap();

    // Imperative job should still exist.
    let still_there = get_job(&config, &imperative.id).unwrap();
    assert_eq!(still_there.command, "echo imperative");
    assert_eq!(still_there.source, "imperative");

    // Declarative job should also exist.
    let decl_job = get_job(&config, "my-decl").unwrap();
    assert_eq!(decl_job.command, "echo decl");
}

#[test]
fn sync_removes_stale_declarative_jobs() {
    let tmp = TempDir::new().unwrap();
    let mut config = test_config(&tmp);
    seed_claiming_agent(&mut config, &["keeper", "stale"]);

    // Insert two declarative jobs.
    let decls = decls_map(vec![
        make_shell_decl("keeper", "0 2 * * *", "echo keep"),
        make_shell_decl("stale", "0 3 * * *", "echo stale"),
    ]);
    sync_declarative_jobs(&config, &decls).unwrap();

    // Now sync with only "keeper"; "stale" should be removed.
    let decls_v2 = decls_map(vec![make_shell_decl("keeper", "0 2 * * *", "echo keep")]);
    sync_declarative_jobs(&config, &decls_v2).unwrap();

    assert!(get_job(&config, "stale").is_err());
    assert!(get_job(&config, "keeper").is_ok());
}

#[test]
fn sync_empty_removes_all_declarative_jobs() {
    let tmp = TempDir::new().unwrap();
    let mut config = test_config(&tmp);
    seed_claiming_agent(&mut config, &["to-remove"]);

    let decls = decls_map(vec![make_shell_decl("to-remove", "0 2 * * *", "echo bye")]);
    sync_declarative_jobs(&config, &decls).unwrap();
    assert!(get_job(&config, "to-remove").is_ok());

    // Sync with empty map.
    sync_declarative_jobs(&config, &std::collections::HashMap::new()).unwrap();
    assert!(get_job(&config, "to-remove").is_err());
}

#[test]
fn sync_validates_shell_job_requires_command() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);

    let (id, mut decl) = make_shell_decl("bad", "0 2 * * *", "echo ok");
    decl.command = None;

    let decls = decls_map(vec![(id, decl)]);
    let result = sync_declarative_jobs(&config, &decls);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("command"));
}

#[test]
fn sync_validates_agent_job_requires_prompt() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);

    let (id, mut decl) = make_agent_decl("bad-agent", "0 2 * * *", "do stuff");
    decl.prompt = None;

    let decls = decls_map(vec![(id, decl)]);
    let result = sync_declarative_jobs(&config, &decls);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("prompt"));
}

#[test]
fn sync_agent_job_inserts_correctly() {
    let tmp = TempDir::new().unwrap();
    let mut config = test_config(&tmp);
    seed_claiming_agent(&mut config, &["agent-check"]);

    let decls = decls_map(vec![make_agent_decl(
        "agent-check",
        "*/15 * * * *",
        "check health",
    )]);
    sync_declarative_jobs(&config, &decls).unwrap();

    let job = get_job(&config, "agent-check").unwrap();
    assert_eq!(job.job_type, JobType::Agent);
    assert_eq!(job.prompt.as_deref(), Some("check health"));
    assert_eq!(job.source, "declarative");
}

#[test]
fn sync_every_schedule_works() {
    let tmp = TempDir::new().unwrap();
    let mut config = test_config(&tmp);
    seed_claiming_agent(&mut config, &["interval-job"]);

    let decl = zeroclaw_config::schema::CronJobDecl {
        name: None,
        job_type: "shell".to_string(),
        schedule: zeroclaw_config::schema::CronScheduleDecl::Every { every_ms: 60000 },
        command: Some("echo interval".to_string()),
        prompt: None,
        enabled: true,
        model: None,
        allowed_tools: None,
        uses_memory: true,
        session_target: None,
        delivery: None,
    };

    let mut decls = std::collections::HashMap::new();
    decls.insert("interval-job".to_string(), decl);
    sync_declarative_jobs(&config, &decls).unwrap();

    let job = get_job(&config, "interval-job").unwrap();
    assert!(matches!(job.schedule, Schedule::Every { every_ms: 60000 }));
    assert_eq!(job.command, "echo interval");
}

#[test]
fn declarative_config_parses_from_toml() {
    // Alias-keyed cron map: `[cron.<alias>]` syntax.
    let toml_str = r#"
[cron.daily-report]
name = "Daily Report"
job_type = "shell"
command = "echo report"
schedule = { kind = "cron", expr = "0 9 * * *" }

[cron.health-check]
job_type = "agent"
prompt = "Check server health"
schedule = { kind = "every", every_ms = 300000 }
    "#;

    #[derive(serde::Deserialize)]
    struct Wrap {
        cron: std::collections::HashMap<String, zeroclaw_config::schema::CronJobDecl>,
    }
    let parsed: Wrap = toml::from_str(toml_str).unwrap();
    assert_eq!(parsed.cron.len(), 2);

    let report = parsed.cron.get("daily-report").unwrap();
    assert_eq!(report.command.as_deref(), Some("echo report"));
    assert!(matches!(
        report.schedule,
        zeroclaw_config::schema::CronScheduleDecl::Cron { ref expr, .. } if expr == "0 9 * * *"
    ));

    let health = parsed.cron.get("health-check").unwrap();
    assert_eq!(health.job_type, "agent");
    assert_eq!(health.prompt.as_deref(), Some("Check server health"));
    assert!(matches!(
        health.schedule,
        zeroclaw_config::schema::CronScheduleDecl::Every { every_ms: 300_000 }
    ));
}

#[test]
fn skip_missed_run_advances_recurring_job_next_run() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);

    // Add a cron job that will be "overdue" — its next_run is set based
    // on the schedule from the current time, so we need to make it past.
    let job = add_job(&config, "test-agent", "* * * * *", "echo test").unwrap();

    // Force next_run into the past so the job appears overdue.
    let past = Utc::now() - ChronoDuration::hours(1);
    with_initialized_connection(&config, |conn| {
        conn.execute(
            "UPDATE cron_jobs SET next_run = ?1 WHERE id = ?2",
            params![past.to_rfc3339(), job.id],
        )
        .unwrap();
        Ok(())
    })
    .unwrap();

    // Verify it is overdue now.
    assert!(
        !all_overdue_jobs(&config, Utc::now()).unwrap().is_empty(),
        "job with past next_run must appear in overdue"
    );

    // Skip the missed run.
    let reloaded = get_job(&config, &job.id).unwrap();
    skip_missed_run(&config, &reloaded, Utc::now()).unwrap();

    // The job's next_run should now be in the future.
    let updated = get_job(&config, &job.id).unwrap();
    assert!(
        updated.next_run > Utc::now(),
        "skip_missed_run must advance next_run to the future"
    );
    assert!(updated.enabled, "recurring job must stay enabled");
}

#[test]
fn skip_missed_run_disables_overdue_oneshot_job() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);

    let run_at = Utc::now() - ChronoDuration::hours(2);
    let schedule = Schedule::At { at: run_at };
    let job = add_job_with_schedule(&config, "test-agent", &schedule, "echo once").unwrap();

    // The add_job_with_schedule should have set next_run = run_at,
    // so the job is overdue now.
    assert!(
        !all_overdue_jobs(&config, Utc::now()).unwrap().is_empty(),
        "one-shot job with past at-time must be overdue"
    );

    let reloaded = get_job(&config, &job.id).unwrap();
    skip_missed_run(&config, &reloaded, Utc::now()).unwrap();

    let updated = get_job(&config, &job.id).unwrap();
    assert!(
        !updated.enabled,
        "overdue one-shot job must be disabled after skip"
    );
    assert_eq!(
        updated.last_status.as_deref(),
        Some("skipped"),
        "one-shot job last_status must be 'skipped'"
    );
}

fn add_job_with_schedule(
    config: &Config,
    agent_alias: &str,
    schedule: &Schedule,
    command: &str,
) -> Result<CronJob> {
    let now = Utc::now();
    let job = CronJob {
        id: format!("test-job-{}", Uuid::new_v4()),
        expression: String::new(),
        schedule: schedule.clone(),
        command: command.to_string(),
        prompt: None,
        name: None,
        job_type: JobType::Shell,
        session_target: SessionTarget::Isolated,
        model: None,
        agent_alias: agent_alias.to_string(),
        enabled: true,
        delivery: DeliveryConfig::default(),
        delete_after_run: false,
        allowed_tools: None,
        uses_memory: false,
        source: "imperative".to_string(),
        created_at: now,
        next_run: next_run_for_schedule(schedule, now).unwrap_or(now),
        last_run: None,
        last_status: None,
        last_output: None,
    };
    let job_type_str: String = match &job.job_type {
        JobType::Shell => "shell".to_string(),
        JobType::Agent => "agent".to_string(),
    };
    let schedule_json = serde_json::to_string(&job.schedule).unwrap();
    let delivery_json = serde_json::to_string(&job.delivery).unwrap();
    let allowed_tools_json =
        crate::cron::store::encode_allowed_tools(job.allowed_tools.as_ref()).unwrap();
    with_initialized_connection(config, |conn| {
        conn.execute(
            "INSERT INTO cron_jobs
             (id, expression, command, schedule, job_type, prompt, name,
              session_target, model, enabled, delivery, delete_after_run,
              allowed_tools, next_run, last_run, last_status, last_output,
              uses_memory, source, created_at, agent_alias)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)",
            params![
                job.id,
                job.expression,
                job.command,
                schedule_json,
                job_type_str.to_string(),
                job.prompt,
                job.name,
                job.session_target.as_str(),
                job.model,
                if job.enabled { 1 } else { 0 },
                delivery_json,
                if job.delete_after_run { 1 } else { 0 },
                allowed_tools_json,
                job.next_run.to_rfc3339(),
                job.last_run.map(|t| t.to_rfc3339()),
                job.last_status,
                job.last_output,
                if job.uses_memory { 1 } else { 0 },
                job.source,
                job.created_at.to_rfc3339(),
                job.agent_alias,
            ],
        )
        .context("Failed to insert test cron job")?;
        Ok(())
    })?;
    Ok(job)
}

#[test]
fn resolve_job_id_or_name_scopes_name_to_owning_agent() {
    // Same job name under two agents. Resolving by name as agent-a must
    // return only agent-a's job — no false ambiguity from agent-b's
    // identically-named job, and no reaching across the agent boundary.
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let mine = add_shell_job(
        &config,
        "agent-a",
        Some("daily_sync".into()),
        Schedule::Cron {
            expr: "0 8 * * *".into(),
            tz: None,
        },
        "echo a",
        None,
    )
    .unwrap();
    add_shell_job(
        &config,
        "agent-b",
        Some("daily_sync".into()),
        Schedule::Cron {
            expr: "0 9 * * *".into(),
            tz: None,
        },
        "echo b",
        None,
    )
    .unwrap();

    let resolved = resolve_job_id_or_name(&config, "daily_sync", "agent-a").unwrap();
    assert_eq!(
        resolved, mine.id,
        "name must resolve to the caller's own job, not the other agent's"
    );
}

#[test]
fn resolve_job_id_or_name_cannot_reach_another_agents_job_by_name() {
    // Only agent-b owns `secret_job`; agent-a must not be able to resolve it.
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    add_shell_job(
        &config,
        "agent-b",
        Some("secret_job".into()),
        Schedule::Cron {
            expr: "0 8 * * *".into(),
            tz: None,
        },
        "echo b",
        None,
    )
    .unwrap();

    let err = resolve_job_id_or_name(&config, "secret_job", "agent-a").unwrap_err();
    assert!(
        err.to_string().contains("No cron job found"),
        "another agent's job must be unresolvable by name; got: {err}"
    );
}
