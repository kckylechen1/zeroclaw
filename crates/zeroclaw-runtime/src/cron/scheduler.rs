use crate::cron::store::{
    RunCompletionAction, persist_manual_run_result, persist_run_completion_state,
    persist_run_result,
};
use crate::cron::{
    CronJob, DeliveryConfig, JobType, Schedule, SessionTarget, all_overdue_jobs, claim_job,
    clear_stale_locks, due_jobs, next_run_for_schedule, reclaim_stale_locks, release_job,
    skip_missed_run, sync_declarative_jobs,
};
use crate::security::SecurityPolicy;
use anyhow::Result;
use chrono::{DateTime, Utc};
use futures_util::{StreamExt, stream};
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::Command;
use tokio::time::{self, Duration};
use tokio_util::sync::CancellationToken;
use zeroclaw_config::schema::Config;
use zeroclaw_config::schema::{CronJobDecl, CronScheduleDecl};
use zeroclaw_log::Instrument;

const MIN_POLL_SECONDS: u64 = 5;
const SHELL_JOB_TIMEOUT_SECS: u64 = 120;
/// Cron-scoped wall-clock cap for agent jobs. A hung `agent::run` (e.g. an
/// MCP tool that never returns) is bounded by this deadline so it cannot
/// occupy a `max_concurrent` slot indefinitely. This is intentionally a
/// cron-local constant — NOT the delegate `agentic_timeout_secs` — because
/// the cron scheduler owns its own run lifecycle independent of delegate
/// spawn policy.
const CRON_AGENT_JOB_TIMEOUT_SECS: u64 = 30 * 60;
/// Stable prefix for agent-job timeout output. `execute_job_with_retry`
/// treats any output starting with this as non-retryable so a hung run
/// cannot occupy a slot for `(retries + 1)` full timeouts.
const CRON_AGENT_JOB_TIMEOUT_PREFIX: &str = "agent cron job timed out after ";
/// TTL for reclaiming `locked_at` during the poll loop. Matches the agent
/// job budget so a crash mid-run (without process exit) or a failed
/// `release_job` cannot wedge the scheduler until the next boot-time
/// `clear_stale_locks`. This reclaim runs between poll ticks — it does
/// **not** fire while `process_due_jobs` is still awaiting an in-flight
/// hang; that case is covered by the agent-run wall-clock timeout below.
const STALE_LOCK_TTL_SECS: u64 = CRON_AGENT_JOB_TIMEOUT_SECS;
/// Best-effort Isolated session purge after a failed/timed-out agent run.
/// Bounded so a stalled memory backend cannot delay `release_job`.
const ISOLATED_SESSION_PURGE_TIMEOUT: Duration = Duration::from_secs(30);
/// Announcement delivery is awaited by `persist_job_result` *before*
/// `execute_and_persist_job` calls `release_job`. A wedged channel send
/// must not hold `locked_at` indefinitely.
const CRON_DELIVERY_TIMEOUT: Duration = Duration::from_secs(30);
const SCHEDULER_COMPONENT: &str = "scheduler";
const CRON_AGENT_DEFAULT_EXCLUDED_TOOLS: &[&str] = &[
    "cron_add",
    "cron_update",
    "cron_remove",
    "cron_run",
    "schedule",
];

// Test-only seam: lets a scheduler test shorten the agent-job wall-clock
// timeout so hung-provider regressions run in milliseconds instead of the
// production 30 minutes.
#[cfg(test)]
tokio::task_local! {
    static TEST_AGENT_JOB_TIMEOUT: Duration;
}

fn agent_job_timeout() -> Duration {
    #[cfg(test)]
    if let Ok(d) = TEST_AGENT_JOB_TIMEOUT.try_with(|d| *d) {
        return d;
    }
    Duration::from_secs(CRON_AGENT_JOB_TIMEOUT_SECS)
}

/// Type alias for the optional broadcast sender used to push cron results
/// to connected dashboard/SSE clients.
pub type EventBroadcast = Option<tokio::sync::broadcast::Sender<serde_json::Value>>;

#[must_use]
pub fn is_no_reply_sentinel(output: &str) -> bool {
    let trimmed = output.trim();
    if trimmed.eq_ignore_ascii_case("NO_REPLY") {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();
    // Legacy form (`NO_REPLY: ...`) is documented as "treated as INFO".
    if lower.starts_with("no_reply:") {
        return true;
    }
    // Kinded form (`NO_REPLY[KIND]: ...`): only the informational kind is a
    // "nothing to report" sentinel. REFUSE / FAIL (and any other/unknown kind)
    // carry operator-visible meaning and must be delivered, not suppressed.
    if let Some(rest) = lower.strip_prefix("no_reply[") {
        if let Some((kind, _)) = rest.split_once(']') {
            return kind.trim() == "info";
        }
        // Malformed `NO_REPLY[...` with no closing bracket: not a clean
        // sentinel — deliver it rather than guess.
        return false;
    }
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnounceDecision {
    /// Send the output to the configured channel.
    Deliver,
    /// Suppress delivery: the output is a quiet `NO_REPLY` sentinel.
    SuppressNoReply,
}

impl AnnounceDecision {
    /// True when the announcement should actually be sent to the channel.
    #[must_use]
    pub fn should_deliver(self) -> bool {
        matches!(self, AnnounceDecision::Deliver)
    }
}

/// Decide whether an announce-mode output should be delivered or suppressed.
/// Suppresses only the *quiet* `NO_REPLY` forms (see [`is_no_reply_sentinel`]);
/// failure/refusal kinds and all real content are delivered.
#[must_use]
pub fn announce_delivery_decision(output: &str) -> AnnounceDecision {
    if is_no_reply_sentinel(output) {
        AnnounceDecision::SuppressNoReply
    } else {
        AnnounceDecision::Deliver
    }
}

#[derive(Clone, Copy)]
pub enum CronDeliveryContext {
    Scheduled,
    ToolManual,
    GatewayManual,
}

impl CronDeliveryContext {
    fn failure_message(self, best_effort: bool) -> &'static str {
        match (self, best_effort) {
            (Self::Scheduled, true) => "Cron delivery failed (best_effort)",
            (Self::Scheduled, false) => "Cron delivery failed",
            (Self::ToolManual, true) => "cron_run delivery failed (best_effort)",
            (Self::ToolManual, false) => "cron_run delivery failed",
            (Self::GatewayManual, true) => "manual cron trigger delivery failed (best_effort)",
            (Self::GatewayManual, false) => "manual cron trigger delivery failed",
        }
    }
}

pub struct ManualCronRunResult {
    pub job_id: String,
    pub success: bool,
    pub status: String,
    pub output: String,
    pub duration_ms: i64,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
}

pub struct CronDeliveryOutcome {
    /// Compatibility projection: true iff execution succeeded.
    pub success: bool,
    /// Compatibility projection: `"ok"`, `"degraded"`, or `"error"`.
    pub status: String,
    pub output: String,
    /// Component truth: did execution succeed?
    pub execution_success: bool,
    /// Component truth: delivery outcome string.
    pub delivery_status: &'static str,
}

pub async fn deliver_and_classify_run_result(
    config: &Config,
    job: &CronJob,
    execution_success: bool,
    mut output: String,
    context: CronDeliveryContext,
) -> CronDeliveryOutcome {
    // Bound delivery while the scheduler still holds the job claim. A
    // channel send that never returns would otherwise pin `locked_at`
    // indefinitely even after the agent run itself timed out. Spawn so a
    // DeliveryFn that blocks before its first await cannot starve the
    // parent's timer; abort on deadline (JoinHandle drop alone detaches).
    let delivery_result = {
        let d_config = config.clone();
        let d_job = job.clone();
        let d_output = output.clone();
        let handle = zeroclaw_spawn::spawn!(async move {
            deliver_if_configured(&d_config, &d_job, &d_output).await
        });
        let abort = handle.abort_handle();
        match time::timeout(CRON_DELIVERY_TIMEOUT, handle).await {
            Ok(Ok(res)) => res,
            Ok(Err(join_err)) => Err(anyhow::Error::msg(format!(
                "delivery task failed while the job lock was held: {join_err}"
            ))),
            Err(_elapsed) => {
                abort.abort();
                Err(anyhow::Error::msg(format!(
                    "delivery exceeded {:?} deadline while the job lock was held; abandoned",
                    CRON_DELIVERY_TIMEOUT
                )))
            }
        }
    };

    let (delivery_status, delivery_error) = match &delivery_result {
        Ok(()) => ("succeeded", None),
        Err(e) => ("failed", Some(e.to_string())),
    };

    // Delivery mode=none and NO_REPLY sentinel are already handled inside
    // `deliver_if_configured` returning Ok(()), but we need to distinguish
    // "not requested" and "suppressed" for the component truth.
    let delivery_status = if delivery_result.is_ok() {
        if job.delivery.mode.eq_ignore_ascii_case("announce") {
            // The announce path returned Ok — either delivered or suppressed
            // by NO_REPLY sentinel. We can't tell from here which, but both
            // are non-failure states. Use "succeeded" as the umbrella; the
            // sentinel log is already emitted inside deliver_if_configured.
            //
            // For the purpose of component truth, both "not_requested" and
            // "suppressed" collapse into the non-failure bucket. If we need
            // finer granularity later, deliver_if_configured can return an
            // enum.
            if announce_delivery_decision(&output).should_deliver() {
                "succeeded"
            } else {
                "suppressed"
            }
        } else {
            "not_requested"
        }
    } else {
        delivery_status
    };

    // Log delivery failures (the loudly-logged warn is the scheduler-side
    // half of the dangling-ref contract from cron add-time).
    if let Some(ref delivery_error) = delivery_error {
        let channel = job.delivery.channel.as_deref().unwrap_or("");
        let target = job.delivery.to.as_deref().unwrap_or("");
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({
                    "job_id": job.id,
                    "agent_alias": job.agent_alias,
                    "channel": channel,
                    "target": target,
                    "error": delivery_error
                })),
            context.failure_message(job.delivery.best_effort)
        );

        if output.trim().is_empty() {
            output = format!("delivery failed: {delivery_error}");
        } else {
            output.push_str("\n\ndelivery failed: ");
            output.push_str(delivery_error);
        }
    }

    // Compatibility projection — execution truth is never rewritten by
    // delivery outcome. A delivery failure on a successful execution may
    // surface as "degraded" (best_effort) or "error" (strict), but the
    // stored execution_success and delivery_status carry the component truth.
    let status = match (execution_success, delivery_status) {
        (true, "succeeded" | "not_requested" | "suppressed") => "ok",
        (true, "failed") => {
            if job.delivery.best_effort {
                "degraded"
            } else {
                // Strict delivery failure: the caller-facing result fails,
                // but execution_success in the outcome remains true so the
                // stored/persisted record preserves execution truth.
                "error"
            }
        }
        (true, _) => "ok", // unknown delivery states don't downgrade execution
        (false, _) => "error",
    };

    // Compatibility projection: strict delivery failure (best_effort=false)
    // surfaces as overall failure to callers, but execution_success and
    // delivery_status carry the component truth separately. For best_effort
    // delivery, a delivery failure does not negate execution success.
    let overall_success =
        execution_success && (delivery_status != "failed" || job.delivery.best_effort);

    CronDeliveryOutcome {
        success: overall_success,
        status: status.to_string(),
        output,
        execution_success,
        delivery_status,
    }
}

pub async fn run_manual_job(
    config: &Config,
    job: &CronJob,
    context: CronDeliveryContext,
    event_tx: &EventBroadcast,
) -> ManualCronRunResult {
    let started_at = Utc::now();
    let (success, output) = execute_job_now(config, job).await;
    let finished_at = Utc::now();
    let duration_ms = (finished_at - started_at).num_milliseconds();
    let outcome = deliver_and_classify_run_result(config, job, success, output, context).await;

    if let Err(e) = persist_manual_run_result(
        config,
        job,
        started_at,
        finished_at,
        &outcome.status,
        Some(&outcome.output),
        duration_ms,
        Some(if outcome.execution_success {
            "success"
        } else {
            "failed"
        }),
        Some(outcome.delivery_status),
    ) {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({"job_id": job.id, "error": format!("{}", e)})),
            "manual cron trigger: failed to persist run history"
        );
    }

    if let Some(tx) = event_tx {
        let _ = tx.send(serde_json::json!({
            "type": "cron_result",
            "job_id": job.id,
            "success": outcome.success,
            "execution_success": outcome.execution_success,
            "delivery_status": outcome.delivery_status,
            "output": &outcome.output,
            "manual": true,
            "timestamp": finished_at.to_rfc3339(),
        }));
    }

    ManualCronRunResult {
        job_id: job.id.clone(),
        success: outcome.success,
        status: outcome.status,
        output: outcome.output,
        duration_ms,
        started_at,
        finished_at,
    }
}

pub async fn run(
    config: Config,
    event_tx: EventBroadcast,
    cancel: CancellationToken,
) -> Result<()> {
    let poll_secs = config.reliability.scheduler_poll_secs.max(MIN_POLL_SECONDS);
    let mut interval = time::interval(Duration::from_secs(poll_secs));
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

    crate::health::mark_component_ok(SCHEDULER_COMPONENT);

    // ── Declarative job sync: reconcile config-defined jobs with the DB.
    let mut jobs_with_builtin = config.cron.clone();
    if let Some(ref schedule_cron) = config.backup.schedule_cron {
        let backup_job = CronJobDecl {
            name: Some("Scheduled backup".to_string()),
            job_type: "shell".to_string(),
            schedule: CronScheduleDecl::Cron {
                expr: schedule_cron.clone(),
                tz: config.backup.schedule_timezone.clone(),
            },
            command: Some("backup create".to_string()),
            prompt: None,
            enabled: true,
            model: None,
            allowed_tools: None,
            uses_memory: true,
            session_target: None,
            delivery: None,
        };
        ::zeroclaw_log::record!(
            DEBUG,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_attrs(::serde_json::json!({"schedule": schedule_cron})),
            "Synthesizing builtin backup cron job from config.backup.schedule_cron"
        );
        jobs_with_builtin.insert("__builtin_backup".to_string(), backup_job);
    }

    match sync_declarative_jobs(&config, &jobs_with_builtin) {
        Ok(()) => {
            if !jobs_with_builtin.is_empty() {
                ::zeroclaw_log::record!(
                    INFO,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_attrs(::serde_json::json!({"count": jobs_with_builtin.len()})),
                    "Synced declarative cron jobs from config"
                );
            }
        }
        Err(e) => ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
            "Failed to sync declarative cron jobs"
        ),
    }

    // ── Stale-lock recovery: any in-flight lock present at boot was left by a
    //    run that died with the previous process. Clear it so those jobs are
    //    eligible again instead of being wedged out of `due_jobs` forever.
    match clear_stale_locks(&config) {
        Ok(0) => {}
        Ok(cleared) => ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_attrs(::serde_json::json!({"cleared": cleared})),
            "Cleared stale cron in-flight locks at startup"
        ),
        Err(e) => ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
            "Failed to clear stale cron in-flight locks at startup"
        ),
    }

    if config.scheduler.catch_up_on_startup {
        catch_up_overdue_jobs(&config, &event_tx).await;
    } else {
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            "Scheduler startup: catch-up disabled by config"
        );
        skip_missed_jobs_on_startup(&config).await;
    }

    loop {
        tokio::select! {
            _ = interval.tick() => {
                // Keep scheduler liveness fresh even when there are no due jobs.
                crate::health::mark_component_ok(SCHEDULER_COMPONENT);

                // TTL reclaim: unlock jobs whose in-flight lock exceeded the
                // stale TTL so hung/crashed runs do not block max_concurrent
                // until the next process restart.
                match reclaim_stale_locks(
                    &config,
                    Utc::now(),
                    Duration::from_secs(STALE_LOCK_TTL_SECS),
                ) {
                    Ok(0) => {}
                    Ok(cleared) => ::zeroclaw_log::record!(
                        INFO,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_attrs(::serde_json::json!({
                                "cleared": cleared,
                                "ttl_secs": STALE_LOCK_TTL_SECS
                            })),
                        "Reclaimed TTL-expired cron in-flight locks"
                    ),
                    Err(e) => ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                        "Failed to reclaim TTL-expired cron in-flight locks"
                    ),
                }

                let jobs = match due_jobs(&config, Utc::now()) {
                    Ok(jobs) => jobs,
                    Err(e) => {
                        crate::health::mark_component_error(SCHEDULER_COMPONENT, e.to_string());
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                                .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                            "Scheduler query failed"
                        );
                        continue;
                    }
                };

                let jobs = claim_due_jobs(&config, jobs);
                process_due_jobs(&config, jobs, SCHEDULER_COMPONENT, &event_tx).await;
            }
            _ = cancel.cancelled() => {
                crate::health::mark_component_ok(SCHEDULER_COMPONENT);
                ::zeroclaw_log::record!(
                    INFO,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                    "Cron scheduler shutting down via cancellation token"
                );
                return Ok(());
            }
        }
    }
}

fn resolve_owning_agent<'a>(config: &'a Config, job: &CronJob) -> Option<&'a str> {
    if !job.agent_alias.is_empty()
        && let Some((alias, _)) = config
            .agents
            .iter()
            .find(|(alias, _)| alias.as_str() == job.agent_alias)
    {
        return Some(alias.as_str());
    }
    config.agent_for_cron_job(&job.id)
}

/// Fetch **all** overdue jobs (ignoring `max_tasks`) and execute them.
/// Called once at scheduler startup so that jobs missed during downtime
/// (e.g. late boot, daemon restart) are caught up immediately.
async fn catch_up_overdue_jobs(config: &Config, event_tx: &EventBroadcast) {
    let now = Utc::now();
    let jobs = match all_overdue_jobs(config, now) {
        Ok(jobs) => jobs,
        Err(e) => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "Startup catch-up query failed"
            );
            return;
        }
    };

    if jobs.is_empty() {
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            "Scheduler startup: no overdue jobs to catch up"
        );
        return;
    }

    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_attrs(::serde_json::json!({"count": jobs.len()})),
        "Scheduler startup: catching up overdue jobs"
    );

    let jobs = claim_due_jobs(config, jobs);
    process_due_jobs(config, jobs, SCHEDULER_COMPONENT, event_tx).await;

    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
        "Scheduler startup: catch-up complete"
    );
}

async fn skip_missed_jobs_on_startup(config: &Config) {
    let now = Utc::now();
    let jobs = match all_overdue_jobs(config, now) {
        Ok(jobs) => jobs,
        Err(e) => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "Scheduler startup skip: query failed",
            );
            return;
        }
    };

    if jobs.is_empty() {
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            "Scheduler startup skip: no overdue jobs to advance",
        );
        return;
    }

    let mut skipped_recurring: u64 = 0;
    let mut skipped_oneshot: u64 = 0;

    for job in &jobs {
        let is_oneshot = matches!(job.schedule, Schedule::At { .. });
        match skip_missed_run(config, job, now) {
            Ok(()) => {
                if is_oneshot {
                    skipped_oneshot += 1;
                } else {
                    skipped_recurring += 1;
                }
            }
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({
                            "job_id": job.id,
                            "error": format!("{}", e),
                        })),
                    "Scheduler startup skip: failed to advance job",
                );
            }
        }
    }

    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(
            ::serde_json::json!({
                "total": jobs.len(),
                "skipped_recurring": skipped_recurring,
                "skipped_oneshot": skipped_oneshot,
            })
        ),
        "Scheduler startup skip: advanced overdue jobs without executing",
    );
}

pub async fn execute_job_now(config: &Config, job: &CronJob) -> (bool, String) {
    use zeroclaw_log::Instrument;
    let Some(agent_alias) = resolve_owning_agent(config, job) else {
        return (
            false,
            format!(
                "cron job {id:?} has no owning agent; add the alias to an [agents.<x>].cron_jobs list",
                id = job.id
            ),
        );
    };
    let agent_alias = agent_alias.to_string();
    let security = match SecurityPolicy::for_agent(config, &agent_alias) {
        Ok(s) => s,
        Err(e) => return (false, format!("agent {agent_alias} risk profile: {e}")),
    };
    let span = zeroclaw_log::attribution_span!(job);
    Box::pin(execute_job_with_retry(config, &security, &agent_alias, job))
        .instrument(span)
        .await
}

fn cron_agent_run_security_policy(base: &SecurityPolicy, job: &CronJob) -> SecurityPolicy {
    let mut policy = base.clone();
    if !matches!(job.job_type, JobType::Agent) || job.allowed_tools.is_some() {
        return policy;
    }

    let excluded = policy.excluded_tools.get_or_insert_with(Vec::new);
    for tool in CRON_AGENT_DEFAULT_EXCLUDED_TOOLS {
        if !excluded.iter().any(|existing| existing == tool) {
            excluded.push((*tool).to_string());
        }
    }
    policy
}

fn cron_agent_session_path(target: &SessionTarget, run_session_id: &str) -> std::path::PathBuf {
    match target {
        SessionTarget::Main => std::path::PathBuf::from("main"),
        SessionTarget::Isolated => std::path::PathBuf::from(format!("cron-{run_session_id}")),
    }
}

async fn execute_job_with_retry(
    config: &Config,
    security: &SecurityPolicy,
    agent_alias: &str,
    job: &CronJob,
) -> (bool, String) {
    let mut last_output = String::new();
    let retries = config.reliability.scheduler_retries;
    let mut backoff_ms = config.reliability.provider_backoff_ms.max(200);

    for attempt in 0..=retries {
        let (success, output) = match job.job_type {
            JobType::Shell => run_job_command(config, security, job).await,
            JobType::Agent => Box::pin(run_agent_job(config, security, agent_alias, job)).await,
        };
        last_output = output;

        if success {
            return (true, last_output);
        }

        if last_output.starts_with("blocked by security policy:")
            || last_output.starts_with(CRON_AGENT_JOB_TIMEOUT_PREFIX)
        {
            // Deterministic policy violations and agent-run timeouts are not
            // retryable: a hung provider/tool will hang again, and retrying
            // would occupy the job's slot for `retries + 1` full timeouts.
            return (false, last_output);
        }

        if attempt < retries {
            let jitter_ms = u64::from(Utc::now().timestamp_subsec_millis() % 250);
            time::sleep(Duration::from_millis(backoff_ms + jitter_ms)).await;
            backoff_ms = (backoff_ms.saturating_mul(2)).min(30_000);
        }
    }

    (false, last_output)
}

fn claim_due_jobs(config: &Config, jobs: Vec<CronJob>) -> Vec<CronJob> {
    jobs.into_iter()
        .filter(|job| match claim_job(config, &job.id, Utc::now()) {
            Ok(true) => true,
            Ok(false) => {
                ::zeroclaw_log::record!(
                    DEBUG,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_attrs(::serde_json::json!({"job_id": job.id})),
                    "Cron job already in flight; skipping duplicate launch"
                );
                false
            }
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(
                            ::serde_json::json!({"job_id": job.id, "error": format!("{}", e)})
                        ),
                    "Cron job: failed to claim in-flight lock; skipping launch"
                );
                false
            }
        })
        .collect()
}

async fn process_due_jobs(
    config: &Config,
    jobs: Vec<CronJob>,
    component: &str,
    event_tx: &EventBroadcast,
) {
    // Refresh scheduler health on every successful poll cycle, including idle cycles.
    crate::health::mark_component_ok(component);

    let max_concurrent = config.scheduler.max_concurrent.max(1);
    let mut in_flight = stream::iter(jobs.into_iter().filter_map(|job| {
        let Some(agent_alias) = resolve_owning_agent(config, &job) else {
            ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"job_id": job.id})), "Cron job has no owning agent; add the alias to an [agents.<x>].cron_jobs list");
            let _ = release_job(config, &job.id);
            return None;
        };
        let agent_alias = agent_alias.to_owned();
        let security = match SecurityPolicy::for_agent(config, &agent_alias) {
            Ok(s) => Arc::new(s),
            Err(e) => {
                ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"job_id": job.id, "agent": agent_alias, "error": format!("{}", e)})), "Cron job: failed to build SecurityPolicy for owning agent");
                let _ = release_job(config, &job.id);
                return None;
            }
        };
        let config = config.clone();
        let component = component.to_owned();
        Some(async move {
            Box::pin(execute_and_persist_job(
                &config,
                security.as_ref(),
                &agent_alias,
                &job,
                &component,
            ))
            .await
        })
    }))
    .buffer_unordered(max_concurrent);

    while let Some((job_id, success, output, execution_success, delivery_status)) =
        in_flight.next().await
    {
        if !success {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"job_id": job_id, "output": output})),
                "Scheduler job '' failed: "
            );
        }
        // Broadcast cron result to dashboard/SSE clients.
        if let Some(tx) = event_tx {
            let _ = tx.send(serde_json::json!({
                "type": "cron_result",
                "job_id": job_id,
                "success": success,
                "execution_success": execution_success,
                "delivery_status": delivery_status,
                "output": output,
                "timestamp": chrono::Utc::now().to_rfc3339(),
            }));
        }
    }
}

async fn execute_and_persist_job(
    config: &Config,
    security: &SecurityPolicy,
    agent_alias: &str,
    job: &CronJob,
    component: &str,
) -> (String, bool, String, bool, &'static str) {
    crate::health::mark_component_ok(component);
    warn_if_high_frequency_agent_job(job);

    let started_at = Utc::now();
    let span = zeroclaw_log::attribution_span!(job);
    let (success, output) = Box::pin(execute_job_with_retry(config, security, agent_alias, job))
        .instrument(span)
        .await;
    let finished_at = Utc::now();
    let run_outcome = Box::pin(persist_job_result(
        config,
        job,
        success,
        &output,
        started_at,
        finished_at,
    ))
    .await;

    // Release the in-flight lock claimed during selection (`claim_due_jobs`) now
    // that the run (and its reschedule/disable/delete in `persist_job_result`) is
    // done. A deleted one-shot row simply releases nothing. If this fails the
    // lock is recovered by poll-loop `reclaim_stale_locks` (TTL) or by
    // `clear_stale_locks` at the next startup.
    if let Err(e) = release_job(config, &job.id) {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({"job_id": job.id, "error": format!("{}", e)})),
            "Cron job: failed to release in-flight lock after run"
        );
    }

    (
        job.id.clone(),
        run_outcome.projected_success,
        run_outcome.output,
        run_outcome.execution_success,
        run_outcome.delivery_status,
    )
}

async fn run_agent_job(
    config: &Config,
    security: &SecurityPolicy,
    agent_alias: &str,
    job: &CronJob,
) -> (bool, String) {
    run_agent_job_with_timeout(config, security, agent_alias, job, agent_job_timeout()).await
}

/// Best-effort purge of an Isolated cron run's per-run memory session.
/// Bounded so a stalled backend cannot delay persist/`release_job`.
async fn purge_isolated_session(
    config: &Config,
    job: &CronJob,
    agent_alias: &str,
    session_path: &std::path::Path,
) {
    if !matches!(job.session_target, SessionTarget::Isolated) {
        return;
    }
    let mem_session_key = zeroclaw_api::session_keys::sanitize_session_key(&format!(
        "cli:{}",
        session_path.display()
    ));
    let owned_config = config.clone();
    let owned_alias = agent_alias.to_string();
    let owned_api_key = config
        .model_provider_for_agent(agent_alias)
        .and_then(|e| e.api_key.as_deref().map(str::to_string));

    // Spawn so a purge that blocks before its first await cannot starve the
    // parent's timeout arm. Abort explicitly on deadline — dropping a
    // JoinHandle only detaches the task.
    let handle = zeroclaw_spawn::spawn!(async move {
        if let Ok(mem) = zeroclaw_memory::create_memory_for_agent(
            &owned_config,
            &owned_alias,
            owned_api_key.as_deref(),
        )
        .await
        {
            let _ = mem.purge_session(&mem_session_key).await;
        }
    });
    let abort = handle.abort_handle();
    if time::timeout(ISOLATED_SESSION_PURGE_TIMEOUT, handle)
        .await
        .is_err()
    {
        abort.abort();
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({
                    "job_id": job.id,
                    "agent": agent_alias,
                    "timeout_secs": ISOLATED_SESSION_PURGE_TIMEOUT.as_secs(),
                })),
            "Cron job: isolated-session purge exceeded its cleanup deadline; abandoning \
             best-effort cleanup so lock release is not delayed"
        );
    }
}

async fn run_agent_job_with_timeout(
    config: &Config,
    security: &SecurityPolicy,
    agent_alias: &str,
    job: &CronJob,
    timeout: Duration,
) -> (bool, String) {
    let subagent_ctx = match crate::subagent::SubAgentSpawn::for_agent(config, agent_alias)
        .and_then(|spawn| spawn.build(crate::subagent::SubAgentOverrides::default()))
    {
        Ok(ctx) => ctx,
        Err(e) => return (false, format!("subagent spawn failed: {e:#}")),
    };

    if !security.can_act() {
        return (
            false,
            "blocked by security policy: autonomy is read-only".to_string(),
        );
    }

    if security.is_rate_limited() {
        return (
            false,
            "blocked by security policy: rate limit exceeded".to_string(),
        );
    }

    if !security.record_action() {
        return (
            false,
            "blocked by security policy: action budget exhausted".to_string(),
        );
    }
    let name = job.name.clone().unwrap_or_else(|| "cron-job".to_string());
    let prompt = job.prompt.clone().unwrap_or_default();

    let prefixed_prompt = format!("[cron:{} {name}] {prompt}", job.id);
    let model_override = job.model.clone();

    let mut cron_config = config.clone();
    cron_config.memory.auto_save = false;

    // Assign a unique run ID for tracing. Isolated jobs also use it in the
    // session path so failed-run memory purge stays scoped per execution.
    // Main-target jobs reuse the stable `main` session path documented in
    // `session_target`.
    let run_session_id = uuid::Uuid::new_v4().to_string();
    let session_path = cron_agent_session_path(&job.session_target, &run_session_id);

    let subagent_span = zeroclaw_log::info_span!(
        "subagent",
        category = "cron",
        agent_alias = %agent_alias,
        cron_job_id = %job.id,
        run_id = %run_session_id,
        spawn_site = "cron",
    );

    let run_security = cron_agent_run_security_policy(subagent_ctx.policy.as_ref(), job);
    let run_overrides = crate::agent::loop_::AgentRunOverrides {
        security: Some(Arc::new(run_security)),
        memory: None,
        is_subagent: false,
        // `uses_memory = false` fully opts the job out of the engine's
        // memory-context injection (stateless digest jobs)...
        suppress_memory_inject: !job.uses_memory,
        // ...and makes the run memory-free end to end: the loop binds a
        // `NoneMemory` backend and drops the persistent memory tools, so a
        // `uses_memory = false` job can neither recall/store through a real
        // backend nor reach one via advertised memory tools
        memory_free: !job.uses_memory,
        // Cron runs are short-lived and one-shot — no cross-turn reuse
        // contract, so the per-call `connect_all` path inside
        // `agent::run` is the correct choice. The daemon heartbeat
        // worker is the only `mcp_registry` supplier.
        mcp_registry: None,
        // SA-10: a cron `JobType::Agent` run is a FRESH ROOT, not a
        // continuation of any interactive parent's ledger — a typed
        // root transition (the run mints its own lineage).
        lineage: None,
    };
    let run_result = match job.session_target {
        SessionTarget::Main | SessionTarget::Isolated => {
            // Supervise the wall-clock deadline from a spawned task so the
            // parent's timeout arm stays schedulable while `agent::run`
            // awaits. On timeout we abort the child (JoinHandle drop alone
            // only detaches) at its next await point, then continue into
            // purge → persist → `release_job`.
            //
            // Limitation: `AbortHandle` is cooperative. A section of
            // `agent::run` that blocks the worker without yielding (e.g.
            // synchronous backend init that `join()`s a thread) can still
            // starve the timeout arm when every worker is occupied. Poll-loop
            // TTL reclaim does not cover that live hang either — it only
            // runs between poll ticks, so it recovers release failures and
            // process-crash leftover locks, not an in-flight blocked worker.
            let run_alias = agent_alias.to_string();
            let run_temperature = config
                .model_provider_for_agent(agent_alias)
                .and_then(|e| e.temperature);
            let run_session_path = session_path.clone();
            let run_allowed_tools = job.allowed_tools.clone();
            let handle = zeroclaw_spawn::spawn!(
                async move {
                    Box::pin(crate::agent::run(
                        cron_config,
                        &run_alias,
                        Some(prefixed_prompt),
                        None,
                        model_override,
                        run_temperature,
                        vec![],
                        false,
                        Some(run_session_path),
                        run_allowed_tools,
                        zeroclaw_api::ingress::TurnOrigin::Cron,
                        run_overrides,
                    ))
                    .await
                }
                .instrument(subagent_span)
            );
            let abort = handle.abort_handle();

            match time::timeout(timeout, handle).await {
                Ok(Ok(result)) => result,
                Ok(Err(join_err)) => Err(anyhow::Error::msg(format!(
                    "agent cron job task failed: {join_err}"
                ))),
                Err(_elapsed) => {
                    abort.abort();
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({
                                "job_id": job.id,
                                "timeout_secs": timeout.as_secs(),
                            })),
                        "Cron job: agent run timed out"
                    );
                    // Child aborted at next await. Purge is independently
                    // bounded below.
                    purge_isolated_session(config, job, agent_alias, &session_path).await;
                    return (
                        false,
                        format!("{CRON_AGENT_JOB_TIMEOUT_PREFIX}{}s", timeout.as_secs()),
                    );
                }
            }
        }
    };

    match run_result {
        Ok(response) => (
            true,
            if response.trim().is_empty() {
                "agent job executed".to_string()
            } else {
                response
            },
        ),
        Err(e) => {
            purge_isolated_session(config, job, agent_alias, &session_path).await;
            (false, format!("agent job failed: {e}"))
        }
    }
}

/// Return value of [`persist_job_result`] — carries both the compatibility
/// projection and the component truth so the scheduled-path SSE broadcast
/// can include execution/delivery status (#64).
struct PersistedRunOutcome {
    /// Compatibility projection (surfaces delivery failure to callers when strict).
    projected_success: bool,
    output: String,
    /// Component truth: did execution succeed?
    execution_success: bool,
    /// Component truth: delivery outcome.
    delivery_status: &'static str,
}

async fn persist_job_result(
    config: &Config,
    job: &CronJob,
    success: bool,
    output: &str,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
) -> PersistedRunOutcome {
    let duration_ms = (finished_at - started_at).num_milliseconds();
    let outcome = deliver_and_classify_run_result(
        config,
        job,
        success,
        output.to_string(),
        CronDeliveryContext::Scheduled,
    )
    .await;

    let action = if is_one_shot_auto_delete(job) && outcome.execution_success {
        RunCompletionAction::Delete
    } else if matches!(job.schedule, Schedule::At { .. }) {
        RunCompletionAction::Disable
    } else {
        RunCompletionAction::Reschedule
    };

    let job_state_at = Utc::now();
    let exec_status_str = if outcome.execution_success {
        "success"
    } else {
        "failed"
    };
    if let Err(e) = persist_run_result(
        config,
        job,
        started_at,
        finished_at,
        job_state_at,
        &outcome.status,
        Some(&outcome.output),
        duration_ms,
        Some(exec_status_str),
        Some(outcome.delivery_status),
        action,
    ) {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({"e": e.to_string()})),
            "Failed to persist scheduler run result: "
        );

        if action == RunCompletionAction::Delete {
            // Best-effort fallback for the legacy behavior: a successful
            // auto-delete one-shot should not be picked up again if the
            // combined history+state transaction fails while inserting or
            // pruning the run row.
            if let Err(disable_err) = persist_run_completion_state(
                config,
                job,
                job_state_at,
                &outcome.status,
                Some(&outcome.output),
                RunCompletionAction::Disable,
            ) {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"disable_err": disable_err.to_string()})),
                    "Failed to disable one-shot cron job after history persistence failure: "
                );
            }
        } else {
            // For recurring jobs and non-delete one-shots, keep the scheduler
            // moving even if run-history persistence fails.
            if let Err(state_err) = persist_run_completion_state(
                config,
                job,
                job_state_at,
                &outcome.status,
                Some(&outcome.output),
                action,
            ) {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"state_err": state_err.to_string()})),
                    "Failed to update cron job state after history persistence failure: "
                );
            }
        }
    }

    PersistedRunOutcome {
        projected_success: outcome.success,
        output: outcome.output,
        execution_success: outcome.execution_success,
        delivery_status: outcome.delivery_status,
    }
}

fn is_one_shot_auto_delete(job: &CronJob) -> bool {
    job.delete_after_run && matches!(job.schedule, Schedule::At { .. })
}

fn is_high_frequency_agent_job(job: &CronJob) -> bool {
    if !matches!(job.job_type, JobType::Agent) {
        return false;
    }
    match &job.schedule {
        Schedule::Every { every_ms } => *every_ms < 5 * 60 * 1000,
        Schedule::Cron { .. } => {
            let now = Utc::now();
            next_run_for_schedule(&job.schedule, now)
                .and_then(|a| next_run_for_schedule(&job.schedule, a).map(|b| (a, b)))
                .map(|(a, b)| (b - a).num_minutes() < 5)
                .unwrap_or(false)
        }
        Schedule::At { .. } => false,
    }
}

fn warn_if_high_frequency_agent_job(job: &CronJob) {
    if is_high_frequency_agent_job(job) {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            &format!(
                "Cron agent job '{}' is scheduled more frequently than every 5 minutes",
                job.id
            )
        );
    }
}

async fn deliver_if_configured(config: &Config, job: &CronJob, output: &str) -> Result<()> {
    let delivery: &DeliveryConfig = &job.delivery;
    if !delivery.mode.eq_ignore_ascii_case("announce") {
        return Ok(());
    }

    if !announce_delivery_decision(output).should_deliver() {
        ::zeroclaw_log::record!(
            DEBUG,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Success)
                .with_attrs(::serde_json::json!({"job_id": job.id})),
            "Cron job returned NO_REPLY sentinel — skipping delivery"
        );
        return Ok(());
    }

    let channel = delivery.channel.as_deref().ok_or_else(|| {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({"field": "channel"})),
            "cron delivery announce refused: required field missing"
        );
        anyhow::Error::msg("delivery.channel is required for announce mode")
    })?;
    let target = delivery.to.as_deref().ok_or_else(|| {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({"field": "to"})),
            "cron delivery announce refused: required field missing"
        );
        anyhow::Error::msg("delivery.to is required for announce mode")
    })?;

    deliver_announcement(
        config,
        channel,
        target,
        delivery.thread_id.as_deref(),
        output,
    )
    .await
}

/// Delivery function type — takes owned values so the returned future is 'static.
/// The fourth `Option<String>` is the optional thread/conversation id propagated
/// to channels whose outbound `thread_id` is distinct from the recipient (webhook).
pub type DeliveryFn = Box<
    dyn Fn(
            Config,
            String,
            String,
            Option<String>,
            String,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>
        + Send
        + Sync,
>;

/// Global delivery function, injected by the binary crate at startup.
static DELIVERY_FN: std::sync::OnceLock<DeliveryFn> = std::sync::OnceLock::new();

/// Register the channel delivery function. Called once at startup by the binary.
pub fn register_delivery_fn(f: DeliveryFn) {
    let _ = DELIVERY_FN.set(f);
}

/// Deliver `output` to `target` on `channel`. A `channel` that names a
/// `[gateway.bridges.<name>]` entry is queued in the bridge outbox, which
/// the gateway drains over `/ws/bridge`; any other channel goes through the
/// delivery function the binary registered (the in-core channels).
pub async fn deliver_announcement(
    config: &Config,
    channel: &str,
    target: &str,
    thread_id: Option<&str>,
    output: &str,
) -> Result<()> {
    if config.gateway.bridges.contains_key(channel) {
        return enqueue_for_bridge(config, channel, target, thread_id, output);
    }
    if let Some(f) = DELIVERY_FN.get() {
        f(
            config.clone(),
            channel.to_string(),
            target.to_string(),
            thread_id.map(str::to_string),
            output.to_string(),
        )
        .await
    } else {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({"channel": channel, "target": target})),
            "Cron delivery skipped: no delivery handler registered \
             (register_delivery_fn was not called by the binary)"
        );
        Ok(())
    }
}

/// Queue a proactive message for a configured bridge. Delivery is
/// asynchronous: success means the message is durably queued, and the
/// bridge receives it the next time its control socket is connected.
pub fn enqueue_for_bridge(
    config: &Config,
    bridge: &str,
    target: &str,
    thread_id: Option<&str>,
    content: &str,
) -> Result<()> {
    let id = zeroclaw_infra::bridge_outbox::BridgeOutbox::shared(&config.data_dir)
        .and_then(|outbox| outbox.enqueue(bridge, target, thread_id, content))
        .inspect_err(|e| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Send)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "bridge": bridge,
                        "error": format!("{e:#}"),
                    })),
                "could not queue a message for a bridge"
            );
        })?;
    ::zeroclaw_log::record!(
        DEBUG,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Send)
            .with_outcome(::zeroclaw_log::EventOutcome::Success)
            .with_attrs(::serde_json::json!({"bridge": bridge, "id": id})),
        "queued a message for a bridge"
    );
    Ok(())
}

async fn run_job_command(
    config: &Config,
    security: &SecurityPolicy,
    job: &CronJob,
) -> (bool, String) {
    run_job_command_with_timeout(
        config,
        security,
        job,
        Duration::from_secs(SHELL_JOB_TIMEOUT_SECS),
    )
    .await
}

async fn run_job_command_with_timeout(
    config: &Config,
    security: &SecurityPolicy,
    job: &CronJob,
    timeout: Duration,
) -> (bool, String) {
    if !security.can_act() {
        return (
            false,
            "blocked by security policy: autonomy is read-only".to_string(),
        );
    }

    if security.is_rate_limited() {
        return (
            false,
            "blocked by security policy: rate limit exceeded".to_string(),
        );
    }

    // Unified command validation: allowlist + risk + path checks in one call.
    // Jobs created via the validated helpers were already checked at creation
    // time, but we re-validate at execution time to catch policy changes and
    // manually-edited job stores.
    let approved = false; // scheduler runs are never pre-approved
    if let Err(error) =
        crate::cron::validate_shell_command_with_security(security, &job.command, approved)
    {
        return (false, error.to_string());
    }

    if let Some(path) = security.forbidden_path_argument(&job.command) {
        return (
            false,
            format!("blocked by security policy: forbidden path argument: {path}"),
        );
    }

    if !security.record_action() {
        return (
            false,
            "blocked by security policy: action budget exhausted".to_string(),
        );
    }

    let child = match build_cron_shell_command(&job.command, &config.data_dir) {
        Ok(mut cmd) => match cmd.spawn() {
            Ok(child) => child,
            Err(e) => return (false, format!("spawn error: {e}")),
        },
        Err(e) => return (false, format!("shell setup error: {e}")),
    };

    match time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let combined = format!(
                "status={}\nstdout:\n{}\nstderr:\n{}",
                output.status,
                stdout.trim(),
                stderr.trim()
            );
            (output.status.success(), combined)
        }
        Ok(Err(e)) => (false, format!("spawn error: {e}")),
        Err(_) => (
            false,
            format!("job timed out after {}s", timeout.as_secs_f64()),
        ),
    }
}

fn build_cron_shell_command(
    command: &str,
    workspace_dir: &std::path::Path,
) -> anyhow::Result<Command> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(workspace_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    Ok(cmd)
}

#[cfg(test)]
mod tests;
