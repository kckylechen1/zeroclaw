use crate::cron::{
    CronJob, CronJobPatch, CronRun, DeliveryConfig, JobType, Schedule, SessionTarget,
    next_run_for_schedule, schedule_cron_expression, validate_delivery_config, validate_schedule,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::types::{FromSqlResult, ValueRef};
use rusqlite::{Connection, OpenFlags, params};
use uuid::Uuid;
use zeroclaw_config::schema::Config;

const MAX_CRON_OUTPUT_BYTES: usize = 16 * 1024;
const TRUNCATED_OUTPUT_MARKER: &str = "\n...[truncated]";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RunCompletionAction {
    Reschedule,
    Disable,
    Delete,
}

#[cfg(test)]
static WRITE_CONNECTION_COUNTS_FOR_TESTS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, usize>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[cfg(test)]
pub(crate) fn reset_write_connection_count_for_tests(config: &Config) {
    let mut counts = WRITE_CONNECTION_COUNTS_FOR_TESTS
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    counts.insert(cron_db_path(config), 0);
}

#[cfg(test)]
pub(crate) fn write_connection_count_for_tests(config: &Config) -> usize {
    let counts = WRITE_CONNECTION_COUNTS_FOR_TESTS
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    counts.get(&cron_db_path(config)).copied().unwrap_or(0)
}

impl rusqlite::types::FromSql for JobType {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let text = value.as_str()?;
        JobType::try_from(text).map_err(|e| rusqlite::types::FromSqlError::Other(e.into()))
    }
}

#[cfg(test)]
pub fn add_job(
    config: &Config,
    agent_alias: &str,
    expression: &str,
    command: &str,
) -> Result<CronJob> {
    let schedule = Schedule::Cron {
        expr: expression.to_string(),
        tz: None,
    };
    add_shell_job(config, agent_alias, None, schedule, command, None)
}

pub fn add_shell_job(
    config: &Config,
    agent_alias: &str,
    name: Option<String>,
    schedule: Schedule,
    command: &str,
    delivery: Option<DeliveryConfig>,
) -> Result<CronJob> {
    let now = Utc::now();
    validate_schedule(&schedule, now)?;
    validate_delivery_config(delivery.as_ref())?;
    let next_run = next_run_for_schedule(&schedule, now)?;
    let id = Uuid::new_v4().to_string();
    let expression = schedule_cron_expression(&schedule).unwrap_or_default();
    let schedule_json = serde_json::to_string(&schedule)?;
    let delivery = delivery.unwrap_or_default();

    let delete_after_run = matches!(schedule, Schedule::At { .. });
    let agent_alias = agent_alias.trim();
    if agent_alias.is_empty() {
        anyhow::bail!("agent_alias is required; cron jobs must name an owning agent");
    }

    with_initialized_connection(config, |conn| {
        conn.execute(
            "INSERT INTO cron_jobs (
                id, expression, command, schedule, job_type, prompt, name, session_target, model,
                enabled, delivery, delete_after_run, agent_alias, created_at, next_run
             ) VALUES (?1, ?2, ?3, ?4, 'shell', NULL, ?5, 'isolated', NULL, 1, ?6, ?7, ?8, ?9, ?10)",
            params![
                id,
                expression,
                command,
                schedule_json,
                name,
                serde_json::to_string(&delivery)?,
                if delete_after_run { 1 } else { 0 },
                agent_alias,
                now.to_rfc3339(),
                next_run.to_rfc3339(),
            ],
        )
        .context("Failed to insert cron shell job")?;
        Ok(())
    })?;

    get_job(config, &id)
}

#[allow(clippy::too_many_arguments)]
pub fn add_agent_job(
    config: &Config,
    agent_alias: &str,
    name: Option<String>,
    schedule: Schedule,
    prompt: &str,
    session_target: SessionTarget,
    model: Option<String>,
    delivery: Option<DeliveryConfig>,
    delete_after_run: bool,
    allowed_tools: Option<Vec<String>>,
    uses_memory: bool,
) -> Result<CronJob> {
    let now = Utc::now();
    validate_schedule(&schedule, now)?;
    validate_delivery_config(delivery.as_ref())?;
    let next_run = next_run_for_schedule(&schedule, now)?;
    let id = Uuid::new_v4().to_string();
    let expression = schedule_cron_expression(&schedule).unwrap_or_default();
    let schedule_json = serde_json::to_string(&schedule)?;
    let delivery = delivery.unwrap_or_default();
    let agent_alias = agent_alias.trim();
    if agent_alias.is_empty() {
        anyhow::bail!("agent_alias is required; cron jobs must name an owning agent");
    }

    with_initialized_connection(config, |conn| {
        conn.execute(
            "INSERT INTO cron_jobs (
                id, expression, command, schedule, job_type, prompt, name, session_target, model,
                enabled, delivery, delete_after_run, allowed_tools, agent_alias, created_at, next_run,
                uses_memory
             ) VALUES (?1, ?2, '', ?3, 'agent', ?4, ?5, ?6, ?7, 1, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                id,
                expression,
                schedule_json,
                prompt,
                name,
                session_target.as_str(),
                model,
                serde_json::to_string(&delivery)?,
                if delete_after_run { 1 } else { 0 },
                encode_allowed_tools(allowed_tools.as_ref())?,
                agent_alias,
                now.to_rfc3339(),
                next_run.to_rfc3339(),
                if uses_memory { 1 } else { 0 },
            ],
        )
        .context("Failed to insert cron agent job")?;
        Ok(())
    })?;

    get_job(config, &id)
}

pub fn list_jobs(config: &Config) -> Result<Vec<CronJob>> {
    let Some(jobs) = with_read_connection(config, |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, expression, command, schedule, job_type, prompt, name, session_target, model,
                    enabled, delivery, delete_after_run, created_at, next_run, last_run, last_status, last_output,
                    allowed_tools, source, uses_memory, agent_alias
             FROM cron_jobs ORDER BY next_run ASC",
        )?;

        let rows = stmt.query_map([], map_cron_job_row)?;

        let mut jobs = Vec::new();
        for row in rows {
            jobs.push(row?);
        }
        Ok(jobs)
    })?
    else {
        return Ok(Vec::new());
    };

    Ok(jobs)
}

pub fn get_job(config: &Config, job_id: &str) -> Result<CronJob> {
    let Some(job) = with_read_connection(config, |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, expression, command, schedule, job_type, prompt, name, session_target, model,
                    enabled, delivery, delete_after_run, created_at, next_run, last_run, last_status, last_output,
                    allowed_tools, source, uses_memory, agent_alias
             FROM cron_jobs WHERE id = ?1",
        )?;

        let mut rows = stmt.query(params![job_id])?;
        if let Some(row) = rows.next()? {
            map_cron_job_row(row).map_err(Into::into)
        } else {
            anyhow::bail!("Cron job '{job_id}' not found")
        }
    })?
    else {
        anyhow::bail!("Cron job '{job_id}' not found")
    };

    Ok(job)
}

pub fn resolve_job_id_or_name(
    config: &Config,
    id_or_name: &str,
    agent_alias: &str,
) -> Result<String> {
    // Fast path: try exact ID lookup first.
    if let Ok(job) = get_job(config, id_or_name) {
        return Ok(job.id);
    }

    // Fallback: search by name within the requesting agent's own jobs.
    let jobs = list_jobs_by_agent(config, agent_alias)?;
    let lower = id_or_name.to_lowercase();
    let matches: Vec<&CronJob> = jobs
        .iter()
        .filter(|j| j.name.as_deref().is_some_and(|n| n.to_lowercase() == lower))
        .collect();

    match matches.len() {
        0 => anyhow::bail!("No cron job found with id or name '{id_or_name}'"),
        1 => Ok(matches[0].id.clone()),
        n => anyhow::bail!(
            "Ambiguous name '{id_or_name}': matched {n} jobs — use the job ID instead"
        ),
    }
}

pub fn remove_job(config: &Config, id: &str) -> Result<()> {
    let changed = with_initialized_connection(config, |conn| {
        conn.execute("DELETE FROM cron_jobs WHERE id = ?1", params![id])
            .context("Failed to delete cron job")
    })?;

    if changed == 0 {
        anyhow::bail!("Cron job '{id}' not found");
    }

    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Delete)
            .with_category(::zeroclaw_log::EventCategory::Cron)
            .with_outcome(::zeroclaw_log::EventOutcome::Success)
            .with_attrs(::serde_json::json!({"job_id": id})),
        "Removed cron job"
    );
    Ok(())
}

/// Cron jobs owned by `agent_alias`, for the agent-deletion export-then-delete
/// archive
pub fn list_jobs_by_agent(config: &Config, agent_alias: &str) -> Result<Vec<CronJob>> {
    let Some(jobs) = with_read_connection(config, |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, expression, command, schedule, job_type, prompt, name, session_target, model,
                    enabled, delivery, delete_after_run, created_at, next_run, last_run, last_status, last_output,
                    allowed_tools, source, uses_memory, agent_alias
             FROM cron_jobs WHERE agent_alias = ?1 ORDER BY next_run ASC",
        )?;
        let rows = stmt.query_map(params![agent_alias], map_cron_job_row)?;
        let mut jobs = Vec::new();
        for row in rows {
            jobs.push(row?);
        }
        Ok(jobs)
    })?
    else {
        return Ok(Vec::new());
    };
    Ok(jobs)
}

/// Delete every cron job owned by `agent_alias`, returning the row count
/// (`cron_runs` cascade via their `job_id` FK). A job whose owning agent is gone
/// can never run, so the agent-deletion cascade removes it
pub fn remove_jobs_by_agent(config: &Config, agent_alias: &str) -> Result<usize> {
    let changed = with_initialized_connection(config, |conn| {
        conn.execute(
            "DELETE FROM cron_jobs WHERE agent_alias = ?1",
            params![agent_alias],
        )
        .context("Failed to delete cron jobs for agent")
    })?;
    Ok(changed)
}

/// Re-point every cron job owned by `from` to `to`, returning the row count.
/// Called by the agent-rename cascade the job keeps running, just
/// under the renamed owner. `agent_alias` is plain TEXT (not a UUID), so this
/// is a direct column update.
pub fn rename_jobs_by_agent(config: &Config, from: &str, to: &str) -> Result<usize> {
    let changed = with_initialized_connection(config, |conn| {
        conn.execute(
            "UPDATE cron_jobs SET agent_alias = ?2 WHERE agent_alias = ?1",
            params![from, to],
        )
        .context("Failed to rename cron job owner")
    })?;
    Ok(changed)
}

pub fn due_jobs(config: &Config, now: DateTime<Utc>) -> Result<Vec<CronJob>> {
    let lim = i64::try_from(config.scheduler.max_tasks.max(1))
        .context("Scheduler max_tasks overflows i64")?;
    let Some(jobs) = with_read_connection(config, |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, expression, command, schedule, job_type, prompt, name, session_target, model,
                    enabled, delivery, delete_after_run, created_at, next_run, last_run, last_status, last_output,
                    allowed_tools, source, uses_memory, agent_alias
             FROM cron_jobs
             WHERE enabled = 1 AND next_run <= ?1 AND locked_at IS NULL
             ORDER BY next_run ASC
             LIMIT ?2",
        )?;

        let rows = stmt.query_map(params![now.to_rfc3339(), lim], map_cron_job_row)?;

        let mut jobs = Vec::new();
        for row in rows {
            match row {
                Ok(job) => jobs.push(job),
                Err(e) => ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                    "Skipping cron job with unparseable row data"
                ),
            }
        }
        Ok(jobs)
    })?
    else {
        return Ok(Vec::new());
    };

    Ok(jobs)
}

pub fn all_overdue_jobs(config: &Config, now: DateTime<Utc>) -> Result<Vec<CronJob>> {
    let Some(jobs) = with_read_connection(config, |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, expression, command, schedule, job_type, prompt, name, session_target, model,
                    enabled, delivery, delete_after_run, created_at, next_run, last_run, last_status, last_output,
                    allowed_tools, source, uses_memory, agent_alias
             FROM cron_jobs
             WHERE enabled = 1 AND next_run <= ?1 AND locked_at IS NULL
             ORDER BY next_run ASC",
        )?;

        let rows = stmt.query_map(params![now.to_rfc3339()], map_cron_job_row)?;

        let mut jobs = Vec::new();
        for row in rows {
            match row {
                Ok(job) => jobs.push(job),
                Err(e) => ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                    "Skipping cron job with unparseable row data"
                ),
            }
        }
        Ok(jobs)
    })?
    else {
        return Ok(Vec::new());
    };

    Ok(jobs)
}

pub fn update_job(config: &Config, job_id: &str, patch: CronJobPatch) -> Result<CronJob> {
    let mut job = get_job(config, job_id)?;
    let mut schedule_changed = false;

    if let Some(schedule) = patch.schedule {
        validate_schedule(&schedule, Utc::now())?;
        job.schedule = schedule;
        job.expression = schedule_cron_expression(&job.schedule).unwrap_or_default();
        schedule_changed = true;
    }
    if let Some(command) = patch.command {
        job.command = command;
    }
    if let Some(prompt) = patch.prompt {
        job.prompt = Some(prompt);
    }
    if let Some(name) = patch.name {
        job.name = Some(name);
    }
    if let Some(enabled) = patch.enabled {
        job.enabled = enabled;
    }
    if let Some(delivery) = patch.delivery {
        // Match add_*_job: announce delivery must include channel + to.
        validate_delivery_config(Some(&delivery))?;
        job.delivery = delivery;
    }
    if let Some(model) = patch.model {
        job.model = Some(model);
    }
    if let Some(target) = patch.session_target {
        job.session_target = target;
    }
    if let Some(delete_after_run) = patch.delete_after_run {
        job.delete_after_run = delete_after_run;
    }
    if let Some(allowed_tools) = patch.allowed_tools {
        // Explicit empty list means deny-all (empty allowlist), matching
        // risk-profile / filter_by_allowed_tools semantics. Use `None` (omit
        // the field) for "unset / default scheduler exclusions".
        // Fail-closed for Hyperion: never treat [] as unrestricted.
        job.allowed_tools = Some(allowed_tools);
    }
    if let Some(uses_memory) = patch.uses_memory {
        job.uses_memory = uses_memory;
    }

    if schedule_changed {
        job.next_run = next_run_for_schedule(&job.schedule, Utc::now())?;
    }

    with_initialized_connection(config, |conn| {
        conn.execute(
            "UPDATE cron_jobs
             SET expression = ?1, command = ?2, schedule = ?3, job_type = ?4, prompt = ?5, name = ?6,
                 session_target = ?7, model = ?8, enabled = ?9, delivery = ?10, delete_after_run = ?11,
                 allowed_tools = ?12, next_run = ?13, uses_memory = ?14
             WHERE id = ?15",
            params![
                job.expression,
                job.command,
                serde_json::to_string(&job.schedule)?,
                <JobType as Into<&str>>::into(job.job_type).to_string(),
                job.prompt,
                job.name,
                job.session_target.as_str(),
                job.model,
                if job.enabled { 1 } else { 0 },
                serde_json::to_string(&job.delivery)?,
                if job.delete_after_run { 1 } else { 0 },
                encode_allowed_tools(job.allowed_tools.as_ref())?,
                job.next_run.to_rfc3339(),
                if job.uses_memory { 1 } else { 0 },
                job.id,
            ],
        )
        .context("Failed to update cron job")?;
        Ok(())
    })?;

    get_job(config, job_id)
}

pub fn record_last_run(
    config: &Config,
    job_id: &str,
    finished_at: DateTime<Utc>,
    success: bool,
    output: &str,
) -> Result<()> {
    let status = if success { "ok" } else { "error" };
    record_last_run_with_status(config, job_id, finished_at, status, output)
}

pub fn record_last_run_with_status(
    config: &Config,
    job_id: &str,
    finished_at: DateTime<Utc>,
    status: &str,
    output: &str,
) -> Result<()> {
    let bounded_output = truncate_cron_output(output);
    with_initialized_connection(config, |conn| {
        apply_last_run_state(conn, job_id, finished_at, status, &bounded_output)
    })
}

pub fn reschedule_after_run(
    config: &Config,
    job: &CronJob,
    success: bool,
    output: &str,
) -> Result<()> {
    let status = if success { "ok" } else { "error" };
    reschedule_after_run_with_status(config, job, status, output)
}

pub fn reschedule_after_run_with_status(
    config: &Config,
    job: &CronJob,
    status: &str,
    output: &str,
) -> Result<()> {
    let now = Utc::now();
    let bounded_output = truncate_cron_output(output);

    // One-shot `At` schedules have no future occurrence — record the run
    // result and disable the job so it won't be picked up again.
    if matches!(job.schedule, Schedule::At { .. }) {
        with_initialized_connection(config, |conn| {
            conn.execute(
                "UPDATE cron_jobs
                 SET enabled = 0, last_run = ?1, last_status = ?2, last_output = ?3
                 WHERE id = ?4",
                params![now.to_rfc3339(), status, bounded_output, job.id],
            )
            .context("Failed to disable completed one-shot cron job")?;
            Ok(())
        })
    } else {
        let next_run = next_run_for_schedule(&job.schedule, now)?;
        with_initialized_connection(config, |conn| {
            conn.execute(
                "UPDATE cron_jobs
                 SET next_run = ?1, last_run = ?2, last_status = ?3, last_output = ?4
                 WHERE id = ?5",
                params![
                    next_run.to_rfc3339(),
                    now.to_rfc3339(),
                    status,
                    bounded_output,
                    job.id
                ],
            )
            .context("Failed to update cron job run state")?;
            Ok(())
        })
    }
}

pub fn skip_missed_run(config: &Config, job: &CronJob, now: DateTime<Utc>) -> Result<()> {
    if matches!(job.schedule, Schedule::At { .. }) {
        // One-shot job whose scheduled moment has already passed —
        // disable it so it won't execute late.
        let bounded_output = truncate_cron_output("skipped — catch_up_on_startup disabled");
        with_initialized_connection(config, |conn| {
            conn.execute(
                "UPDATE cron_jobs
                 SET enabled = 0, last_run = ?1, last_status = 'skipped', last_output = ?2
                 WHERE id = ?3",
                params![now.to_rfc3339(), bounded_output, job.id],
            )
            .context("Failed to disable overdue one-shot cron job on startup skip")?;
            Ok(())
        })
    } else {
        // Recurring job — advance next_run to the next future occurrence.
        let next_run = next_run_for_schedule(&job.schedule, now)?;
        with_initialized_connection(config, |conn| {
            conn.execute(
                "UPDATE cron_jobs SET next_run = ?1 WHERE id = ?2",
                params![next_run.to_rfc3339(), job.id],
            )
            .context("Failed to advance next_run on startup skip")?;
            Ok(())
        })
    }
}

pub fn claim_job(config: &Config, job_id: &str, now: DateTime<Utc>) -> Result<bool> {
    with_initialized_connection(config, |conn| {
        let claimed = conn
            .execute(
                "UPDATE cron_jobs SET locked_at = ?1 WHERE id = ?2 AND locked_at IS NULL",
                params![now.to_rfc3339(), job_id],
            )
            .context("Failed to claim cron job for execution")?;
        Ok(claimed == 1)
    })
}

pub fn release_job(config: &Config, job_id: &str) -> Result<()> {
    with_initialized_connection(config, |conn| {
        conn.execute(
            "UPDATE cron_jobs SET locked_at = NULL WHERE id = ?1",
            params![job_id],
        )
        .context("Failed to release cron job lock")?;
        Ok(())
    })
}

pub fn clear_stale_locks(config: &Config) -> Result<usize> {
    clear_stale_locks_before(config, None)
}

/// Clear in-flight cron locks.
///
/// When `older_than` is `None`, every lock is cleared (startup recovery after
/// process death). When `Some(cutoff)`, only locks with `locked_at < cutoff`
/// are reclaimed — used by the poll loop for TTL-based recovery so a hung
/// agent job cannot hold a `max_concurrent` slot until the next daemon
/// restart.
pub fn clear_stale_locks_before(
    config: &Config,
    older_than: Option<DateTime<Utc>>,
) -> Result<usize> {
    let cleared = with_read_connection(config, |conn| match older_than {
        None => conn
            .execute(
                "UPDATE cron_jobs SET locked_at = NULL WHERE locked_at IS NOT NULL",
                [],
            )
            .context("Failed to clear stale cron job locks"),
        Some(cutoff) => conn
            .execute(
                "UPDATE cron_jobs SET locked_at = NULL \
                     WHERE locked_at IS NOT NULL AND locked_at < ?1",
                params![cutoff.to_rfc3339()],
            )
            .context("Failed to reclaim TTL-expired cron job locks"),
    })?;
    Ok(cleared.unwrap_or(0))
}

/// Reclaim locks whose `locked_at` is older than `now - ttl`.
///
/// This is the poll-loop counterpart to boot-time [`clear_stale_locks`]: a
/// hung or leaked claim is freed once it exceeds the agent job wall-clock
/// budget, so the next poll can re-queue the job instead of wedging it out
/// of `due_jobs` until process restart.
pub fn reclaim_stale_locks(
    config: &Config,
    now: DateTime<Utc>,
    ttl: std::time::Duration,
) -> Result<usize> {
    let ttl = chrono::Duration::from_std(ttl).unwrap_or_else(|_| chrono::Duration::seconds(1800));
    let cutoff = now - ttl;
    clear_stale_locks_before(config, Some(cutoff))
}

pub fn record_run(
    config: &Config,
    job_id: &str,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    status: &str,
    output: Option<&str>,
    duration_ms: i64,
) -> Result<()> {
    let bounded_output = output.map(truncate_cron_output);
    with_initialized_connection(config, |conn| {
        // Wrap INSERT + pruning DELETE in an explicit transaction so that
        // if the DELETE fails, the INSERT is rolled back and the run table
        // cannot grow unboundedly.
        let tx = conn.unchecked_transaction()?;

        insert_run_and_prune(
            &tx,
            config,
            job_id,
            started_at,
            finished_at,
            status,
            bounded_output.as_deref(),
            duration_ms,
            None,
            None,
        )?;

        tx.commit()
            .context("Failed to commit cron run transaction")?;
        Ok(())
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn persist_manual_run_result(
    config: &Config,
    job: &CronJob,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    status: &str,
    output: Option<&str>,
    duration_ms: i64,
    execution_status: Option<&str>,
    delivery_status: Option<&str>,
) -> Result<()> {
    let bounded_output = output.map(truncate_cron_output);

    with_initialized_connection(config, |conn| {
        let tx = conn.unchecked_transaction()?;

        insert_run_and_prune(
            &tx,
            config,
            &job.id,
            started_at,
            finished_at,
            status,
            bounded_output.as_deref(),
            duration_ms,
            execution_status,
            delivery_status,
        )?;

        apply_last_run_state(
            &tx,
            &job.id,
            finished_at,
            status,
            bounded_output.as_deref().unwrap_or(""),
        )?;

        tx.commit()
            .context("Failed to commit manual cron run result transaction")?;
        Ok(())
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn persist_run_result(
    config: &Config,
    job: &CronJob,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    job_state_at: DateTime<Utc>,
    status: &str,
    output: Option<&str>,
    duration_ms: i64,
    execution_status: Option<&str>,
    delivery_status: Option<&str>,
    action: RunCompletionAction,
) -> Result<()> {
    let bounded_output = output.map(truncate_cron_output);

    with_initialized_connection(config, |conn| {
        let tx = conn.unchecked_transaction()?;

        insert_run_and_prune(
            &tx,
            config,
            &job.id,
            started_at,
            finished_at,
            status,
            bounded_output.as_deref(),
            duration_ms,
            execution_status,
            delivery_status,
        )?;

        apply_run_completion_state(
            &tx,
            job,
            job_state_at,
            status,
            bounded_output.as_deref(),
            action,
        )?;

        tx.commit()
            .context("Failed to commit cron run result transaction")?;
        Ok(())
    })
}

pub(crate) fn persist_run_completion_state(
    config: &Config,
    job: &CronJob,
    job_state_at: DateTime<Utc>,
    status: &str,
    output: Option<&str>,
    action: RunCompletionAction,
) -> Result<()> {
    with_initialized_connection(config, |conn| {
        apply_run_completion_state(conn, job, job_state_at, status, output, action)
    })
}

#[allow(clippy::too_many_arguments)]
fn insert_run_and_prune(
    conn: &Connection,
    config: &Config,
    job_id: &str,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    status: &str,
    output: Option<&str>,
    duration_ms: i64,
    execution_status: Option<&str>,
    delivery_status: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO cron_runs (job_id, started_at, finished_at, status, output, duration_ms, execution_status, delivery_status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            job_id,
            started_at.to_rfc3339(),
            finished_at.to_rfc3339(),
            status,
            output,
            duration_ms,
            execution_status,
            delivery_status,
        ],
    )
    .context("Failed to insert cron run")?;

    let keep = i64::from(config.scheduler.max_run_history.max(1));
    conn.execute(
        "DELETE FROM cron_runs
         WHERE job_id = ?1
           AND id NOT IN (
             SELECT id FROM cron_runs
             WHERE job_id = ?1
             ORDER BY started_at DESC, id DESC
             LIMIT ?2
           )",
        params![job_id, keep],
    )
    .context("Failed to prune cron run history")?;

    Ok(())
}

fn apply_last_run_state(
    conn: &Connection,
    job_id: &str,
    finished_at: DateTime<Utc>,
    status: &str,
    output: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE cron_jobs
         SET last_run = ?1, last_status = ?2, last_output = ?3
         WHERE id = ?4",
        params![finished_at.to_rfc3339(), status, output, job_id],
    )
    .context("Failed to update cron last run fields")?;
    Ok(())
}

fn truncate_cron_output(output: &str) -> String {
    if output.len() <= MAX_CRON_OUTPUT_BYTES {
        return output.to_string();
    }

    if MAX_CRON_OUTPUT_BYTES <= TRUNCATED_OUTPUT_MARKER.len() {
        return TRUNCATED_OUTPUT_MARKER.to_string();
    }

    let mut cutoff = MAX_CRON_OUTPUT_BYTES - TRUNCATED_OUTPUT_MARKER.len();
    while cutoff > 0 && !output.is_char_boundary(cutoff) {
        cutoff -= 1;
    }

    let mut truncated = output[..cutoff].to_string();
    truncated.push_str(TRUNCATED_OUTPUT_MARKER);
    truncated
}

pub fn list_runs(config: &Config, job_id: &str, limit: usize) -> Result<Vec<CronRun>> {
    let Some(runs) = with_read_connection(config, |conn| {
        let lim = i64::try_from(limit.max(1)).context("Run history limit overflow")?;
        let mut stmt = conn.prepare(
            "SELECT id, job_id, started_at, finished_at, status, output, duration_ms,
                    execution_status, delivery_status
             FROM cron_runs
             WHERE job_id = ?1
             ORDER BY started_at DESC, id DESC
             LIMIT ?2",
        )?;

        let rows = stmt.query_map(params![job_id, lim], |row| {
            Ok(CronRun {
                id: row.get(0)?,
                job_id: row.get(1)?,
                started_at: parse_rfc3339(&row.get::<_, String>(2)?)
                    .map_err(sql_conversion_error)?,
                finished_at: parse_rfc3339(&row.get::<_, String>(3)?)
                    .map_err(sql_conversion_error)?,
                status: row.get(4)?,
                output: row.get(5)?,
                duration_ms: row.get(6)?,
                execution_status: row.get(7)?,
                delivery_status: row.get(8)?,
            })
        })?;

        let mut runs = Vec::new();
        for row in rows {
            runs.push(row?);
        }
        Ok(runs)
    })?
    else {
        return Ok(Vec::new());
    };

    Ok(runs)
}

fn parse_rfc3339(raw: &str) -> Result<DateTime<Utc>> {
    let parsed = DateTime::parse_from_rfc3339(raw)
        .with_context(|| format!("Invalid RFC3339 timestamp in cron DB: {raw}"))?;
    Ok(parsed.with_timezone(&Utc))
}

fn sql_conversion_error(err: anyhow::Error) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(err.into())
}

fn map_cron_job_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CronJob> {
    let expression: String = row.get(1)?;
    let schedule_raw: Option<String> = row.get(3)?;
    let schedule =
        decode_schedule(schedule_raw.as_deref(), &expression).map_err(sql_conversion_error)?;

    let delivery_raw: Option<String> = row.get(10)?;
    let delivery = decode_delivery(delivery_raw.as_deref()).map_err(sql_conversion_error)?;

    let next_run_raw: String = row.get(13)?;
    let last_run_raw: Option<String> = row.get(14)?;
    let created_at_raw: String = row.get(12)?;
    let allowed_tools_raw: Option<String> = row.get(17)?;
    let source: Option<String> = row.get(18)?;
    let uses_memory: Option<i64> = row.get(19)?;
    let agent_alias: Option<String> = row.get(20)?;

    Ok(CronJob {
        id: row.get(0)?,
        expression,
        schedule,
        command: row.get(2)?,
        job_type: row.get(4)?,
        prompt: row.get(5)?,
        name: row.get(6)?,
        session_target: SessionTarget::parse(&row.get::<_, String>(7)?),
        model: row.get(8)?,
        agent_alias: agent_alias
            .map(|s| s.trim().to_string())
            .unwrap_or_default(),
        enabled: row.get::<_, i64>(9)? != 0,
        delivery,
        delete_after_run: row.get::<_, i64>(11)? != 0,
        source: source.unwrap_or_else(|| "imperative".to_string()),
        uses_memory: uses_memory != Some(0),
        created_at: parse_rfc3339(&created_at_raw).map_err(sql_conversion_error)?,
        next_run: parse_rfc3339(&next_run_raw).map_err(sql_conversion_error)?,
        last_run: match last_run_raw {
            Some(raw) => Some(parse_rfc3339(&raw).map_err(sql_conversion_error)?),
            None => None,
        },
        last_status: row.get(15)?,
        last_output: row.get(16)?,
        allowed_tools: decode_allowed_tools(allowed_tools_raw.as_deref())
            .map_err(sql_conversion_error)?,
    })
}

fn decode_schedule(schedule_raw: Option<&str>, expression: &str) -> Result<Schedule> {
    if let Some(raw) = schedule_raw {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return serde_json::from_str(trimmed)
                .with_context(|| format!("Failed to parse cron schedule JSON: {trimmed}"));
        }
    }

    if expression.trim().is_empty() {
        anyhow::bail!("Missing schedule and legacy expression for cron job")
    }

    Ok(Schedule::Cron {
        expr: expression.to_string(),
        tz: None,
    })
}

fn decode_delivery(delivery_raw: Option<&str>) -> Result<DeliveryConfig> {
    if let Some(raw) = delivery_raw {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return serde_json::from_str(trimmed)
                .with_context(|| format!("Failed to parse cron delivery JSON: {trimmed}"));
        }
    }
    Ok(DeliveryConfig::default())
}

fn encode_allowed_tools(allowed_tools: Option<&Vec<String>>) -> Result<Option<String>> {
    allowed_tools
        .map(serde_json::to_string)
        .transpose()
        .context("Failed to serialize cron allowed_tools")
}

fn decode_allowed_tools(raw: Option<&str>) -> Result<Option<Vec<String>>> {
    if let Some(raw) = raw {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return serde_json::from_str(trimmed)
                .map(Some)
                .with_context(|| format!("Failed to parse cron allowed_tools JSON: {trimmed}"));
        }
    }
    Ok(None)
}

pub fn sync_declarative_jobs(
    config: &Config,
    decls: &std::collections::HashMap<String, zeroclaw_config::schema::CronJobDecl>,
) -> Result<()> {
    use zeroclaw_config::schema::CronScheduleDecl;

    if decls.is_empty() {
        // If no declarative jobs are defined, clean up previously synced
        // declarative jobs only when cron storage already exists. A fresh
        // workspace with nothing to sync should stay DB-free on daemon start.
        let _ = with_existing_initialized_connection(config, |conn| {
            let deleted = conn
                .execute("DELETE FROM cron_jobs WHERE source = 'declarative'", [])
                .context("Failed to remove stale declarative cron jobs")?;
            if deleted > 0 {
                ::zeroclaw_log::record!(
                    INFO,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_attrs(::serde_json::json!({"count": deleted})),
                    "Removed declarative cron jobs no longer in config"
                );
            }
            Ok(())
        })?;
        return Ok(());
    }

    // Validate declarations before touching the DB.
    for (id, decl) in decls {
        validate_decl(id, decl)?;
    }

    let now = Utc::now();

    with_initialized_connection(config, |conn| {
        // Collect IDs of all declarative jobs currently defined in config.
        let config_ids: std::collections::HashSet<&str> =
            decls.keys().map(String::as_str).collect();

        // Remove declarative jobs no longer in config.
        {
            let mut stmt = conn.prepare("SELECT id FROM cron_jobs WHERE source = 'declarative'")?;
            let db_ids: Vec<String> = stmt
                .query_map([], |row| row.get(0))?
                .filter_map(|r| r.ok())
                .collect();

            for db_id in &db_ids {
                if !config_ids.contains(db_id.as_str()) {
                    conn.execute("DELETE FROM cron_jobs WHERE id = ?1", params![db_id])
                        .with_context(|| {
                            format!("Failed to remove stale declarative cron job '{db_id}'")
                        })?;
                    ::zeroclaw_log::record!(
                        INFO,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_attrs(::serde_json::json!({"job_id": db_id})),
                        "Removed declarative cron job no longer in config"
                    );
                }
            }
        }

        for (id, decl) in decls {
            let schedule = convert_schedule_decl(&decl.schedule)?;
            let expression = schedule_cron_expression(&schedule).unwrap_or_default();
            let schedule_json = serde_json::to_string(&schedule)?;
            let job_type = &decl.job_type;
            let session_target = decl.session_target.as_deref().unwrap_or("isolated");
            let delivery = match &decl.delivery {
                Some(d) => convert_delivery_decl(d),
                None => DeliveryConfig::default(),
            };
            let delivery_json = serde_json::to_string(&delivery)?;
            let allowed_tools_json = encode_allowed_tools(decl.allowed_tools.as_ref())?;
            let command = decl.command.as_deref().unwrap_or("");
            let delete_after_run = matches!(decl.schedule, CronScheduleDecl::At { .. });

            // Check if job already exists.
            let exists: bool = conn
                .prepare("SELECT COUNT(*) FROM cron_jobs WHERE id = ?1")?
                .query_row(params![id], |row| row.get::<_, i64>(0))
                .map(|c| c > 0)
                .unwrap_or(false);

            if exists {
                // Update existing declarative job — preserve runtime state
                // (next_run, last_run, last_status, last_output, created_at).
                // Only update the schedule's next_run if the schedule itself changed.
                let current_schedule_raw: Option<String> = conn
                    .prepare("SELECT schedule FROM cron_jobs WHERE id = ?1")?
                    .query_row(params![id], |row| row.get(0))
                    .ok();

                let schedule_changed = current_schedule_raw.as_deref() != Some(&schedule_json);

                if schedule_changed {
                    let next_run = next_run_for_schedule(&schedule, now)?;
                    conn.execute(
                        "UPDATE cron_jobs
                         SET expression = ?1, command = ?2, schedule = ?3, job_type = ?4,
                             prompt = ?5, name = ?6, session_target = ?7, model = ?8,
                             enabled = ?9, delivery = ?10, delete_after_run = ?11,
                             allowed_tools = ?12, source = 'declarative', next_run = ?13,
                             uses_memory = ?14
                         WHERE id = ?15",
                        params![
                            expression,
                            command,
                            schedule_json,
                            job_type,
                            decl.prompt,
                            decl.name,
                            session_target,
                            decl.model,
                            i32::from(decl.enabled),
                            delivery_json,
                            i32::from(delete_after_run),
                            allowed_tools_json,
                            next_run.to_rfc3339(),
                            i32::from(decl.uses_memory),
                            id,
                        ],
                    )
                    .with_context(|| format!("Failed to update declarative cron job '{id}'"))?;
                } else {
                    conn.execute(
                        "UPDATE cron_jobs
                         SET expression = ?1, command = ?2, schedule = ?3, job_type = ?4,
                             prompt = ?5, name = ?6, session_target = ?7, model = ?8,
                             enabled = ?9, delivery = ?10, delete_after_run = ?11,
                             allowed_tools = ?12, source = 'declarative',
                             uses_memory = ?13
                         WHERE id = ?14",
                        params![
                            expression,
                            command,
                            schedule_json,
                            job_type,
                            decl.prompt,
                            decl.name,
                            session_target,
                            decl.model,
                            i32::from(decl.enabled),
                            delivery_json,
                            i32::from(delete_after_run),
                            allowed_tools_json,
                            i32::from(decl.uses_memory),
                            id,
                        ],
                    )
                    .with_context(|| format!("Failed to update declarative cron job '{id}'"))?;
                }

                ::zeroclaw_log::record!(
                    DEBUG,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_attrs(::serde_json::json!({"job_id": id})),
                    "Updated declarative cron job"
                );
            } else {
                // Reverse-resolve the owning agent from
                // `[agents.<x>].cron_jobs` membership. Orphan declarative
                // entries that no agent claims are skipped with a warning
                // rather than silently bound to a magic alias.
                let Some(agent_alias) = config.agent_for_cron_job(id) else {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({"job_id": id})),
                        "Skipping declarative cron job: no [agents.<x>].cron_jobs entry claims this id"
                    );
                    continue;
                };
                let next_run = next_run_for_schedule(&schedule, now)?;
                conn.execute(
                    "INSERT INTO cron_jobs (
                        id, expression, command, schedule, job_type, prompt, name,
                        session_target, model, enabled, delivery, delete_after_run,
                        allowed_tools, source, uses_memory, agent_alias, created_at, next_run
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 'declarative', ?14, ?15, ?16, ?17)",
                    params![
                        id,
                        expression,
                        command,
                        schedule_json,
                        job_type,
                        decl.prompt,
                        decl.name,
                        session_target,
                        decl.model,
                        i32::from(decl.enabled),
                        delivery_json,
                        i32::from(delete_after_run),
                        allowed_tools_json,
                        i32::from(decl.uses_memory),
                        agent_alias,
                        now.to_rfc3339(),
                        next_run.to_rfc3339(),
                    ],
                )
                .with_context(|| {
                    format!("Failed to insert declarative cron job '{id}'")
                })?;

                ::zeroclaw_log::record!(
                    INFO,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_attrs(::serde_json::json!({"job_id": id})),
                    "Inserted declarative cron job from config"
                );
            }
        }

        Ok(())
    })
}

/// Validate a declarative cron job definition.
fn validate_decl(id: &str, decl: &zeroclaw_config::schema::CronJobDecl) -> Result<()> {
    if id.trim().is_empty() {
        anyhow::bail!("Declarative cron job has empty id");
    }

    match decl.job_type.to_lowercase().as_str() {
        "shell" => {
            if decl.command.as_deref().is_none_or(|c| c.trim().is_empty()) {
                anyhow::bail!(
                    "Declarative cron job '{id}': shell job requires a non-empty 'command'"
                );
            }
        }
        "agent" => {
            if decl.prompt.as_deref().is_none_or(|p| p.trim().is_empty()) {
                anyhow::bail!(
                    "Declarative cron job '{id}': agent job requires a non-empty 'prompt'"
                );
            }
        }
        other => {
            anyhow::bail!(
                "Declarative cron job '{id}': invalid job_type '{other}', expected 'shell' or 'agent'"
            );
        }
    }

    Ok(())
}

/// Convert a `CronScheduleDecl` to the runtime `Schedule` type.
fn convert_schedule_decl(decl: &zeroclaw_config::schema::CronScheduleDecl) -> Result<Schedule> {
    use zeroclaw_config::schema::CronScheduleDecl;
    match decl {
        CronScheduleDecl::Cron { expr, tz } => Ok(Schedule::Cron {
            expr: expr.clone(),
            tz: tz.clone(),
        }),
        CronScheduleDecl::Every { every_ms } => Ok(Schedule::Every {
            every_ms: *every_ms,
        }),
        CronScheduleDecl::At { at } => {
            let parsed = DateTime::parse_from_rfc3339(at)
                .with_context(|| {
                    format!("Invalid RFC3339 timestamp in declarative cron 'at': {at}")
                })?
                .with_timezone(&Utc);
            Ok(Schedule::At { at: parsed })
        }
    }
}

/// Convert a `DeliveryConfigDecl` to the runtime `DeliveryConfig`.
fn convert_delivery_decl(decl: &zeroclaw_config::schema::DeliveryConfigDecl) -> DeliveryConfig {
    DeliveryConfig {
        mode: decl.mode.clone(),
        channel: decl.channel.clone(),
        to: decl.to.clone(),
        thread_id: decl.thread_id.clone(),
        best_effort: decl.best_effort,
    }
}

fn add_column_if_missing(conn: &Connection, name: &str, sql_type: &str) -> Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(cron_jobs)")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let col_name: String = row.get(1)?;
        if col_name == name {
            return Ok(());
        }
    }
    // Drop the statement/rows before executing ALTER to release any locks
    drop(rows);
    drop(stmt);

    // Tolerate "duplicate column name" errors to handle the race where
    // another process adds the column between our PRAGMA check and ALTER.
    match conn.execute(
        &format!("ALTER TABLE cron_jobs ADD COLUMN {name} {sql_type}"),
        [],
    ) {
        Ok(_) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(err, Some(ref msg)))
            if msg.contains("duplicate column name") =>
        {
            ::zeroclaw_log::record!(
                DEBUG,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(::serde_json::json!({"error": format!("{}", err), "name": name})),
                "Column cron_jobs. already exists (concurrent migration)"
            );
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("Failed to add cron_jobs.{name}")),
    }
}

/// Like [`add_column_if_missing`] but for the `cron_runs` table (#64).
fn add_run_column_if_missing(conn: &Connection, name: &str, sql_type: &str) -> Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(cron_runs)")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let col_name: String = row.get(1)?;
        if col_name == name {
            return Ok(());
        }
    }
    drop(rows);
    drop(stmt);

    match conn.execute(
        &format!("ALTER TABLE cron_runs ADD COLUMN {name} {sql_type}"),
        [],
    ) {
        Ok(_) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(_err, Some(ref msg)))
            if msg.contains("duplicate column name") =>
        {
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("Failed to add cron_runs.{name}")),
    }
}

fn cron_db_path(config: &Config) -> std::path::PathBuf {
    config.data_dir.join("cron").join("jobs.db")
}

// Read paths must not create the cron directory or jobs.db. If the DB already
// exists, however, reads still need the lightweight schema/migration ensure
// step before selecting columns added by newer releases.
fn with_read_connection<T>(
    config: &Config,
    f: impl FnOnce(&Connection) -> Result<T>,
) -> Result<Option<T>> {
    with_existing_initialized_connection(config, f)
}

fn with_existing_initialized_connection<T>(
    config: &Config,
    f: impl FnOnce(&Connection) -> Result<T>,
) -> Result<Option<T>> {
    let db_path = cron_db_path(config);
    if !db_path.exists() {
        return Ok(None);
    }

    let conn = Connection::open_with_flags(
        &db_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| {
        format!(
            "Failed to open existing cron DB: {}",
            db_path.display().to_string()
        )
    })?;

    initialize_schema(&conn)?;

    f(&conn).map(Some)
}

fn with_initialized_connection<T>(
    config: &Config,
    f: impl FnOnce(&Connection) -> Result<T>,
) -> Result<T> {
    let db_path = cron_db_path(config);
    #[cfg(test)]
    {
        let mut counts = WRITE_CONNECTION_COUNTS_FOR_TESTS
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(count) = counts.get_mut(&db_path) {
            *count += 1;
        }
    }

    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "Failed to create cron directory: {}",
                parent.display().to_string()
            )
        })?;
    }

    let conn = Connection::open(&db_path)
        .with_context(|| format!("Failed to open cron DB: {}", db_path.display().to_string()))?;

    initialize_schema(&conn)?;

    f(&conn)
}

fn apply_run_completion_state(
    conn: &Connection,
    job: &CronJob,
    job_state_at: DateTime<Utc>,
    status: &str,
    output: Option<&str>,
    action: RunCompletionAction,
) -> Result<()> {
    let bounded_output = output.map(truncate_cron_output);

    match action {
        RunCompletionAction::Reschedule => {
            let next_run = next_run_for_schedule(&job.schedule, job_state_at)?;
            let changed = conn
                .execute(
                    "UPDATE cron_jobs
                     SET next_run = ?1, last_run = ?2, last_status = ?3, last_output = ?4
                     WHERE id = ?5",
                    params![
                        next_run.to_rfc3339(),
                        job_state_at.to_rfc3339(),
                        status,
                        bounded_output.as_deref(),
                        job.id,
                    ],
                )
                .context("Failed to update cron job run state")?;
            if changed == 0 {
                anyhow::bail!("Cron job '{}' not found", job.id);
            }
        }
        RunCompletionAction::Disable => {
            let changed = conn
                .execute(
                    "UPDATE cron_jobs
                     SET enabled = 0, last_run = ?1, last_status = ?2, last_output = ?3
                     WHERE id = ?4",
                    params![
                        job_state_at.to_rfc3339(),
                        status,
                        bounded_output.as_deref(),
                        job.id,
                    ],
                )
                .context("Failed to disable completed one-shot cron job")?;
            if changed == 0 {
                anyhow::bail!("Cron job '{}' not found", job.id);
            }
        }
        RunCompletionAction::Delete => {
            let changed = conn
                .execute("DELETE FROM cron_jobs WHERE id = ?1", params![job.id])
                .context("Failed to delete completed one-shot cron job")?;
            if changed == 0 {
                anyhow::bail!("Cron job '{}' not found", job.id);
            }
        }
    }

    Ok(())
}

fn initialize_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS cron_jobs (
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
        );
        CREATE INDEX IF NOT EXISTS idx_cron_jobs_next_run ON cron_jobs(next_run);

        CREATE TABLE IF NOT EXISTS cron_runs (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            job_id      TEXT NOT NULL,
            started_at  TEXT NOT NULL,
            finished_at TEXT NOT NULL,
            status      TEXT NOT NULL,
            output      TEXT,
            duration_ms INTEGER,
            FOREIGN KEY (job_id) REFERENCES cron_jobs(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_cron_runs_job_id ON cron_runs(job_id);
        CREATE INDEX IF NOT EXISTS idx_cron_runs_started_at ON cron_runs(started_at);
        CREATE INDEX IF NOT EXISTS idx_cron_runs_job_started ON cron_runs(job_id, started_at);",
    )
    .context("Failed to initialize cron schema")?;

    add_column_if_missing(conn, "schedule", "TEXT")?;
    add_column_if_missing(conn, "job_type", "TEXT NOT NULL DEFAULT 'shell'")?;
    add_column_if_missing(conn, "prompt", "TEXT")?;
    add_column_if_missing(conn, "name", "TEXT")?;
    add_column_if_missing(conn, "session_target", "TEXT NOT NULL DEFAULT 'isolated'")?;
    add_column_if_missing(conn, "model", "TEXT")?;
    add_column_if_missing(conn, "enabled", "INTEGER NOT NULL DEFAULT 1")?;
    add_column_if_missing(conn, "delivery", "TEXT")?;
    add_column_if_missing(conn, "delete_after_run", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(conn, "allowed_tools", "TEXT")?;
    add_column_if_missing(conn, "source", "TEXT DEFAULT 'imperative'")?;
    add_column_if_missing(conn, "uses_memory", "INTEGER NOT NULL DEFAULT 1")?;
    // Rows written before the column existed get an empty alias; the
    // scheduler treats those as orphans (skip with warning) rather than
    // coercing them to a magic alias.
    add_column_if_missing(conn, "agent_alias", "TEXT NOT NULL DEFAULT ''")?;
    // In-flight execution lock: RFC3339 timestamp of when a run claimed this job,
    // or NULL when idle. `due_jobs`/`all_overdue_jobs` skip locked rows so a job that
    // runs longer than the poll interval cannot be launched again while still in
    // flight (see `claim_job`/`release_job` and
    add_column_if_missing(conn, "locked_at", "TEXT")?;

    // Component-level outcome decomposition (#64). Legacy cron_runs rows
    // written before these columns existed carry NULL — readers map NULL
    // to None (reported as "legacy/unknown") rather than guessing.
    add_run_column_if_missing(conn, "execution_status", "TEXT")?;
    add_run_column_if_missing(conn, "delivery_status", "TEXT")?;

    Ok(())
}

#[cfg(test)]
mod tests;
