//! The single SQLite-backed [`TaskRegistry`] — EPIC A's durable index.

use std::path::Path;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};

use super::authority::is_authoritative;
use super::task_registry::{TaskKind, TaskRecord, TaskRegistry, TaskStatus};

mod goal;

const CONTROL_PLANE_SCHEMA_VERSION: i64 = 8;

pub struct SqliteTaskStore {
    conn: Mutex<Connection>,
}

/// A terminal task row plus the output/error `TaskRecord` does not carry.
pub struct TerminalTaskView {
    pub record: TaskRecord,
    pub output: Option<String>,
    pub error: Option<String>,
}

impl SqliteTaskStore {
    /// Open (creating if absent) the control-plane DB at `<data_dir>/control_plane.db`.
    /// Additive: a fresh install gets an empty DB and today's behavior is unchanged.
    pub fn new(data_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("create data dir {}", data_dir.display()))?;
        let db_path = data_dir.join("control_plane.db");
        let conn = Connection::open(&db_path)
            .with_context(|| format!("open control-plane DB: {}", db_path.display()))?;
        Self::init(conn)
    }

    /// In-memory store for unit tests.
    pub fn new_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory().context("open in-memory control-plane DB")?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;
             PRAGMA temp_store = MEMORY;
             PRAGMA foreign_keys = ON;",
        )
        .context("set control-plane PRAGMAs")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS tasks (
                 id              TEXT PRIMARY KEY,
                 kind            TEXT NOT NULL,
                 agent           TEXT NOT NULL,
                 status          TEXT NOT NULL,
                 owner_pid       INTEGER NOT NULL DEFAULT 0,
                 owner_boot_id   TEXT NOT NULL DEFAULT '',
                 heartbeat_at    TEXT,
                 depth           INTEGER NOT NULL DEFAULT 0,
                 parent_id       TEXT,
                 originator_route TEXT,
                 delivered       INTEGER NOT NULL DEFAULT 0,
                 idem_key        TEXT,
                 principal_id    TEXT,
                 started_at      TEXT NOT NULL,
                 finished_at     TEXT,
                 output          TEXT,
                 error           TEXT
             );
             CREATE INDEX IF NOT EXISTS idx_tasks_status ON tasks(status);
             CREATE INDEX IF NOT EXISTS idx_tasks_agent  ON tasks(agent);
             CREATE INDEX IF NOT EXISTS idx_tasks_agent_kind_started
                ON tasks(agent, kind, started_at DESC);",
        )
        .context("create control-plane base schema")?;
        migrate_schema(&conn).context("migrate control-plane schema")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Admin enumeration — count this agent's records (mirrors AcpSessionStore's
    /// `count_*_by_agent`; used by alias-delete cascades / observability).
    pub fn count_by_agent(&self, agent: &str) -> Result<u64> {
        let conn = self.conn.lock();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE agent = ?1",
                params![agent],
                |r| r.get(0),
            )
            .context("count tasks by agent")?;
        Ok(n as u64)
    }

    /// Admin enumeration — delete this agent's records (alias-delete cascade).
    pub fn delete_by_agent(&self, agent: &str) -> Result<u64> {
        let conn = self.conn.lock();
        let n = conn
            .execute("DELETE FROM tasks WHERE agent = ?1", params![agent])
            .context("delete tasks by agent")?;
        Ok(n as u64)
    }

    /// Synchronous counterpart to [`TaskRegistry::create`], for callers that
    /// cannot `.await` — namely `zeroclaw_coordinator::ChildPersistence`,
    /// whose `record_spawn` is a sync `&mut self` method (see
    /// `control_plane::subagent_persistence`). Locks the same `Mutex<Connection>`
    /// and calls the same [`insert_task_record`] the async `create` wraps —
    /// no SQL duplicated, no `block_on`.
    pub(crate) fn create_sync(&self, rec: TaskRecord) -> Result<()> {
        let conn = self.conn.lock();
        insert_task_record(&conn, rec)
    }

    /// Synchronous counterpart to [`TaskRegistry::finish_task`] — see
    /// `create_sync`'s doc for why this exists and what it does not do
    /// differently from the async path.
    pub(crate) fn finish_task_sync(
        &self,
        id: &str,
        status: TaskStatus,
        output: Option<&str>,
        error: Option<&str>,
        delivered: bool,
    ) -> Result<bool> {
        let conn = self.conn.lock();
        finish_task_record(&conn, id, status, output, error, delivered)
    }

    /// Terminal row plus its output/error, or `None` if missing or still running.
    pub fn get_terminal_with_result(&self, id: &str) -> Result<Option<TerminalTaskView>> {
        let conn = self.conn.lock();
        let row = conn
            .query_row("SELECT * FROM tasks WHERE id = ?1", params![id], |row| {
                let record = row_to_record(row)?;
                let output: Option<String> = row.get("output")?;
                let error: Option<String> = row.get("error")?;
                Ok((record, output, error))
            })
            .optional()
            .context("get terminal task")?;
        Ok(row.and_then(|(record, output, error)| {
            record.status.is_terminal().then_some(TerminalTaskView {
                record,
                output,
                error,
            })
        }))
    }

    /// Claim one finished child of `parent_id` by id. Same atomic
    /// `delivered = 1` semantics as [`TaskRegistry::claim_undelivered_children`].
    pub fn claim_child(
        &self,
        parent_id: &str,
        child_id: &str,
    ) -> Result<Option<zeroclaw_api::announce::Announcement>> {
        let conn = self.conn.lock();
        let sql = format!(
            "UPDATE tasks SET delivered = 1
              WHERE parent_id = ?1
                AND id = ?2
                AND delivered = 0
                AND status IN ({})
          RETURNING id, COALESCE(executor, agent) AS agent, status, output, error, finished_at",
            terminal_status_sql_list()
        );
        let mut stmt = conn
            .prepare_cached(&sql)
            .context("prepare claim one child")?;
        let row = stmt
            .query_row(params![parent_id, child_id], |row| {
                Ok((
                    row.get::<_, String>("id")?,
                    row.get::<_, String>("agent")?,
                    row.get::<_, String>("status")?,
                    row.get::<_, Option<String>>("output")?,
                    row.get::<_, Option<String>>("error")?,
                    row.get::<_, Option<String>>("finished_at")?,
                ))
            })
            .optional()
            .context("claim one child")?;
        Ok(
            row.map(|(task_id, agent, status, output, error, finished_at)| {
                let (outcome, degraded_detail) = resolve_claimed_outcome(&task_id, &status);
                zeroclaw_api::announce::Announcement {
                    task_id,
                    agent,
                    outcome,
                    output,
                    detail: degraded_detail.or(error),
                    finished_at,
                }
            }),
        )
    }
}

fn migrate_schema(conn: &Connection) -> Result<()> {
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .context("read control-plane schema version")?;
    goal::migrate_schema(conn, version)?;
    if version > CONTROL_PLANE_SCHEMA_VERSION {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(
                ::serde_json::json!({
                    "db_version": version,
                    "known_version": CONTROL_PLANE_SCHEMA_VERSION,
                })
            ),
            "control-plane DB was created by a newer schema version"
        );
    }
    Ok(())
}

fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    alter_sql: &str,
) -> Result<()> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .with_context(|| format!("inspect {table} columns"))?;
    let mut rows = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .with_context(|| format!("query {table} columns"))?;
    let exists = rows.any(|name| matches!(name, Ok(name) if name == column));
    if !exists {
        conn.execute_batch(alter_sql)
            .with_context(|| format!("add {table}.{column}"))?;
    }
    Ok(())
}

// ── serde<->TEXT helpers (reuse the snake_case derive, no hand-kept string tables) ──

fn kind_to_db(k: TaskKind) -> String {
    serde_json::to_value(k)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "delegate".into())
}

fn status_to_db(s: TaskStatus) -> String {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "running".into())
}

fn kind_from_db(s: &str) -> Result<TaskKind> {
    serde_json::from_value(serde_json::Value::String(s.to_owned()))
        .with_context(|| format!("unknown task kind {s:?}"))
}

fn status_from_db(s: &str) -> Result<TaskStatus> {
    serde_json::from_value(serde_json::Value::String(s.to_owned()))
        .with_context(|| format!("unknown task status {s:?}"))
}

/// A SQL `(...)` fragment listing every terminal status, spelled exactly as
/// `status_from_db` parses it back, for use after either `IN` or `NOT IN`.
/// Built from `TaskStatus::TERMINAL` via `status_to_db` (the same serde round
/// trip every other column uses) rather than a hand-typed string, so no SQL
/// filter in this file can drift from the on-disk spelling the way four
/// independently maintained copies once could — see
/// `every_sql_status_filter_is_built_from_terminal_status_sql_list_not_hand_typed`
/// below, which fails if any filter reverts to a hand-typed literal list.
/// Safe to inline directly into SQL text: every value comes from the enum,
/// never from untrusted input. The output is a pure function of
/// `TaskStatus::TERMINAL` (stable order, no randomness), so it is also safe
/// to use as a `prepare_cached` key: identical input always yields the
/// identical string, and rusqlite's statement cache keys on that exact
/// string (see `StatementCache::get`, which caches on `sql.trim()`).
fn terminal_status_sql_list() -> String {
    TaskStatus::TERMINAL
        .iter()
        .map(|&s| format!("'{}'", status_to_db(s)))
        .collect::<Vec<_>>()
        .join(",")
}

/// Resolve one claimed child's outcome and, when it had to be degraded, the
/// detail explaining why.
///
/// By the time a caller has a `status` string to hand this function, the
/// UPDATE that flagged the row delivered has already run (see the comment on
/// `claim_undelivered_children`). So there is no "reject and let it be
/// re-claimed" available here — every status this function is given must
/// leave with *some* outcome. Today, `claim_undelivered_children`'s WHERE
/// filter is derived from the same source this function parses against, so
/// every status reaching here is expected to map cleanly — this branch is a
/// deliberate defensive net against that guarantee ever being broken by a
/// future refactor (a hand-edited SQL string, a filter built a different
/// way), not a path this binary exercises in normal operation. A status that
/// does not map degrades to `Lost`, the outcome that exists for exactly "the
/// work may or may not have happened; go and check" — with a `Some` detail
/// carrying the raw status, so the degrade is visible to whoever reads the
/// announcement, not just to the log.
fn resolve_claimed_outcome(
    task_id: &str,
    status: &str,
) -> (zeroclaw_api::announce::AnnouncedOutcome, Option<String>) {
    use zeroclaw_api::announce::AnnouncedOutcome;

    let mapped = status_from_db(status)
        .ok()
        .and_then(super::task_registry::announced_outcome);
    if let Some(outcome) = mapped {
        return (outcome, None);
    }

    ::zeroclaw_log::record!(
        WARN,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
            .with_attrs(::serde_json::json!({ "task_id": task_id, "status": status })),
        "control-plane: claimed child has a status that does not map to an announced \
         outcome; already flagged delivered, so reporting it as lost rather than \
         dropping it"
    );
    (
        AnnouncedOutcome::Lost,
        Some(format!(
            "task ended with unrecognised status {status:?}; treated as lost"
        )),
    )
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRecord> {
    let kind_s: String = row.get("kind")?;
    let status_s: String = row.get("status")?;
    // serde parse failures map to a SQLite conversion error; callers SKIP such rows
    // (collect_skipping_bad_rows) rather than failing the whole query. The column index
    // (`0`) is a placeholder — rusqlite has no by-name conversion-error ctor and the
    // index is not surfaced to the skip path (review nit #4).
    let kind = kind_from_db(&kind_s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, e.into())
    })?;
    let status = status_from_db(&status_s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, e.into())
    })?;
    Ok(TaskRecord {
        id: row.get("id")?,
        kind,
        agent: row.get("agent")?,
        status,
        owner_pid: row.get::<_, i64>("owner_pid")? as u32,
        owner_boot_id: row.get("owner_boot_id")?,
        heartbeat_at: row.get("heartbeat_at")?,
        depth: row.get::<_, i64>("depth")? as u32,
        parent_id: row.get("parent_id")?,
        originator_route: row.get("originator_route")?,
        delivered: row.get::<_, i64>("delivered")? != 0,
        idem_key: row.get("idem_key")?,
        principal_id: row.get("principal_id")?,
        executor: row.get("executor")?,
        started_at: row.get("started_at")?,
        finished_at: row.get("finished_at")?,
    })
}

/// Collect query rows, SKIPPING (and logging) any single row that fails to convert —
/// one unrecognised/corrupt record (e.g. a forward-incompat `kind`/`status` written by a
/// newer binary) must not fail the whole enumeration and starve the reaper (finding #3).
fn collect_skipping_bad_rows<I>(rows: I) -> Vec<TaskRecord>
where
    I: Iterator<Item = rusqlite::Result<TaskRecord>>,
{
    let mut out = Vec::new();
    for r in rows {
        match r {
            Ok(rec) => out.push(rec),
            Err(e) => log_unreadable_task_row(e),
        }
    }
    out
}

fn log_unreadable_task_row(error: rusqlite::Error) {
    ::zeroclaw_log::record!(
        WARN,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
            .with_attrs(::serde_json::json!({ "error": format!("{error}") })),
        "control-plane: skipping unreadable task row"
    );
}

fn insert_task_record(conn: &Connection, rec: TaskRecord) -> Result<()> {
    // ON CONFLICT DO NOTHING, NOT INSERT OR REPLACE: re-registering an existing id
    // must be a true no-op, never clobber an already-recorded output/error/terminal
    // status back to NULL/running (review finding— the documented idempotency).
    conn.execute(
        "INSERT INTO tasks
            (id, kind, agent, status, owner_pid, owner_boot_id, heartbeat_at, depth,
             parent_id, originator_route, delivered, idem_key, principal_id, executor,
             started_at, finished_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)
         ON CONFLICT(id) DO NOTHING",
        params![
            rec.id,
            kind_to_db(rec.kind),
            rec.agent,
            status_to_db(rec.status),
            rec.owner_pid as i64,
            rec.owner_boot_id,
            rec.heartbeat_at,
            rec.depth as i64,
            rec.parent_id,
            rec.originator_route,
            rec.delivered as i64,
            rec.idem_key,
            rec.principal_id,
            rec.executor,
            rec.started_at,
            rec.finished_at,
        ],
    )
    .context("insert task record")?;
    Ok(())
}

fn update_task_status_record(
    conn: &Connection,
    id: &str,
    status: TaskStatus,
    output: Option<String>,
    error: Option<String>,
) -> Result<usize> {
    let finished_at = status
        .is_terminal()
        .then(|| chrono::Utc::now().to_rfc3339());
    let sql = format!(
        "UPDATE tasks
            SET status = ?1,
                output = COALESCE(?2, output),
                error  = COALESCE(?3, error),
                finished_at = COALESCE(?4, finished_at)
          WHERE id = ?5
            AND status NOT IN ({})",
        terminal_status_sql_list()
    );
    conn.execute(
        &sql,
        params![status_to_db(status), output, error, finished_at, id],
    )
    .context("update task status")
}

/// Move `id` straight from non-terminal to (terminal status, output, error,
/// delivered) in one UPDATE. See [`TaskRegistry::finish_task`] doc comment
/// for why this must be one statement, not two.
///
/// `output`/`error` use `COALESCE` the same way `update_task_status_record`
/// does, so passing `None` preserves whatever the row already carries rather
/// than clobbering it. `finished_at` is set unconditionally to "now" — safe
/// because the WHERE guard below only ever lets this branch run against a
/// row that was non-terminal a moment ago, so this is always the row's first
/// terminal transition.
fn finish_task_record(
    conn: &Connection,
    id: &str,
    status: TaskStatus,
    output: Option<&str>,
    error: Option<&str>,
    delivered: bool,
) -> Result<bool> {
    anyhow::ensure!(
        status.is_terminal(),
        "finish_task requires a terminal status, got {status:?}"
    );
    let finished_at = chrono::Utc::now().to_rfc3339();
    let sql = format!(
        "UPDATE tasks
            SET status = ?1,
                output = COALESCE(?2, output),
                error  = COALESCE(?3, error),
                finished_at = ?4,
                delivered = ?5
          WHERE id = ?6
            AND status NOT IN ({})",
        terminal_status_sql_list()
    );
    let n = conn
        .execute(
            &sql,
            params![
                status_to_db(status),
                output,
                error,
                finished_at,
                delivered as i64,
                id,
            ],
        )
        .context("finish task")?;
    Ok(n > 0)
}

fn claim_task_owner_record(
    conn: &Connection,
    id: &str,
    owner_pid: u32,
    owner_boot_id: &str,
) -> Result<usize> {
    let sql = format!(
        "UPDATE tasks
            SET owner_pid = ?1,
                owner_boot_id = ?2,
                heartbeat_at = NULL
          WHERE id = ?3
            AND status NOT IN ({})",
        terminal_status_sql_list()
    );
    conn.execute(&sql, params![owner_pid as i64, owner_boot_id, id])
        .context("claim task owner")
}

#[async_trait::async_trait]
impl TaskRegistry for SqliteTaskStore {
    async fn create(&self, rec: TaskRecord) -> Result<()> {
        let conn = self.conn.lock();
        insert_task_record(&conn, rec)?;
        Ok(())
    }

    async fn heartbeat(&self, id: &str, owner_boot_id: &str) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.conn.lock();
        // Only the heart-beating owner refreshes; prevents a stale boot from
        // resurrecting liveness it does not own.
        conn.execute(
            "UPDATE tasks SET heartbeat_at = ?1
             WHERE id = ?2 AND owner_boot_id = ?3",
            params![now, id, owner_boot_id],
        )
        .context("heartbeat task")?;
        Ok(())
    }

    async fn update_status(
        &self,
        id: &str,
        status: TaskStatus,
        output: Option<String>,
        error: Option<String>,
    ) -> Result<()> {
        let conn = self.conn.lock();
        update_task_status_record(&conn, id, status, output, error)?;
        Ok(())
    }

    async fn finish_task(
        &self,
        id: &str,
        status: TaskStatus,
        output: Option<&str>,
        error: Option<&str>,
        delivered: bool,
    ) -> Result<bool> {
        let conn = self.conn.lock();
        finish_task_record(&conn, id, status, output, error, delivered)
    }

    async fn claim_owner(&self, id: &str, owner_pid: u32, owner_boot_id: &str) -> Result<()> {
        let conn = self.conn.lock();
        claim_task_owner_record(&conn, id, owner_pid, owner_boot_id)?;
        Ok(())
    }

    async fn get(&self, id: &str) -> Result<Option<TaskRecord>> {
        let conn = self.conn.lock();
        let rec = conn
            .query_row(
                "SELECT * FROM tasks WHERE id = ?1",
                params![id],
                row_to_record,
            )
            .optional()
            .context("get task")?;
        Ok(rec)
    }

    async fn list_running(&self) -> Result<Vec<TaskRecord>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare("SELECT * FROM tasks WHERE status = 'running'")
            .context("prepare list_running")?;
        let rows = stmt
            .query_map([], row_to_record)
            .context("query list_running")?;
        Ok(collect_skipping_bad_rows(rows))
    }

    async fn list_by_agent(&self, agent: &str) -> Result<Vec<TaskRecord>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare("SELECT * FROM tasks WHERE agent = ?1 ORDER BY started_at DESC")
            .context("prepare list_by_agent")?;
        let rows = stmt
            .query_map(params![agent], row_to_record)
            .context("query list_by_agent")?;
        Ok(collect_skipping_bad_rows(rows))
    }

    async fn claim_undelivered_children(
        &self,
        parent_id: &str,
    ) -> Result<Vec<zeroclaw_api::announce::Announcement>> {
        use zeroclaw_api::announce::{AnnouncedOutcome, Announcement};

        let conn = self.conn.lock();
        // One statement claims and returns. Splitting the read from the flag
        // would let two wakers announce the same completion, and an announced
        // completion becomes a parent turn — so a double read is a double run.
        //
        // `output` and `error` are selected explicitly because `TaskRecord`
        // does not carry them: announcing a completion without its result
        // would tell the parent that something finished while withholding
        // what it produced.
        let sql = format!(
            "UPDATE tasks SET delivered = 1
              WHERE parent_id = ?1
                AND delivered = 0
                AND status IN ({})
          RETURNING id, COALESCE(executor, agent) AS agent, status, output, error, finished_at",
            terminal_status_sql_list()
        );
        let mut stmt = conn
            .prepare_cached(&sql)
            .context("prepare claim undelivered children")?;
        let rows = stmt
            .query_map(params![parent_id], |row| {
                // `id` is `TEXT PRIMARY KEY NOT NULL`, so it decodes whenever
                // the row itself is readable at all. Read it alone, first,
                // before touching any column a forward-incompat writer could
                // have gotten wrong — everything else is decoded inside the
                // nested closure below so a failure there comes back paired
                // with this id, instead of taking the whole row down with it.
                let task_id: String = row.get("id")?;
                let rest: rusqlite::Result<(
                    String,
                    String,
                    Option<String>,
                    Option<String>,
                    Option<String>,
                )> = (|| {
                    Ok((
                        row.get::<_, String>("agent")?,
                        row.get::<_, String>("status")?,
                        row.get::<_, Option<String>>("output")?,
                        row.get::<_, Option<String>>("error")?,
                        row.get::<_, Option<String>>("finished_at")?,
                    ))
                })();
                Ok((task_id, rest))
            })
            .context("claim undelivered children")?;

        // By the time a row reaches this loop, the UPDATE above has already
        // committed `delivered = 1` for it — this connection runs in
        // autocommit with no explicit transaction wrapping claim + decode. So
        // returning `Err` from here would not fail the claim; it would only
        // fail *reporting* a claim that already happened, and the row can
        // never be re-claimed. That makes every exit from this loop a
        // degrade-and-announce, never a silent drop — with exactly one
        // exception. A row whose `id` cannot be read at all (the closure
        // above never got past its first `?`) truly has nothing to name: it
        // is logged and skipped, because there is no id left to put in an
        // `Announcement`. Every other column failing to decode still leaves
        // the id in hand, so it is announced as `Lost` with a detail naming
        // the decode failure — the row is already consumed, and reporting
        // nothing would be indistinguishable to the parent from the child
        // still running.
        let mut claimed = Vec::new();
        for row in rows {
            let (task_id, rest) = match row {
                Ok(v) => v,
                Err(e) => {
                    log_unreadable_task_row(e);
                    continue;
                }
            };
            match rest {
                Ok((agent, status, output, error, finished_at)) => {
                    let (outcome, degraded_detail) = resolve_claimed_outcome(&task_id, &status);
                    claimed.push(Announcement {
                        task_id,
                        agent,
                        outcome,
                        output,
                        detail: degraded_detail.or(error),
                        finished_at,
                    });
                }
                Err(e) => {
                    // The id decoded but some other column did not — a
                    // different failure than `resolve_claimed_outcome`'s "the
                    // status decoded but does not map to a known outcome":
                    // here the row's shape did not match what this query
                    // expected at all (e.g. a column a different binary wrote
                    // with the wrong SQLite storage class). This row is
                    // already flagged delivered and can never be re-claimed,
                    // so this is the only chance left to tell the parent it
                    // ever existed.
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({
                                "task_id": task_id,
                                "error": format!("{e}"),
                            })),
                        "control-plane: claimed child's id decoded but the rest of its row did \
                         not; announcing it as lost rather than letting an already-delivered \
                         row vanish"
                    );
                    claimed.push(Announcement {
                        task_id: task_id.clone(),
                        agent: "unknown".to_string(),
                        outcome: AnnouncedOutcome::Lost,
                        output: None,
                        detail: Some(format!(
                            "task {task_id:?} was claimed but its row could not be fully \
                             decoded: {e}"
                        )),
                        finished_at: None,
                    });
                }
            }
        }
        Ok(claimed)
    }

    async fn unclaim_children(&self, ids: &[String]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let conn = self.conn.lock();
        // One id per statement, all inside one transaction, rather than the
        // obvious single set-membership UPDATE binding every id at once. That
        // is not a performance choice — a batch here is one turn's
        // announcements, a handful of rows — it is a deliberate concession to
        // this file's structural guard
        // `every_sql_status_filter_is_built_from_terminal_status_sql_list_not_hand_typed`,
        // which fails any set-membership clause in this file's production
        // source that is not immediately followed by
        // `terminal_status_sql_list()`'s `{}` placeholder. A list of ids bound
        // with `?` is not the hand-typed status literal that guard exists to
        // catch, but it does not match the shape either, and weakening a guard
        // to fit new code is how the drift it prevents gets back in. Do not
        // "optimise" this back into a set-membership clause without dealing
        // with that test first.
        //
        // The transaction is what keeps the batch all-or-nothing, which the
        // single statement would have given for free.
        //
        // `delivered = 1` in each WHERE is what makes this idempotent: a
        // repeat call matches nothing and reports zero rows changed, so a
        // retry cannot manufacture a second delivery of the same completion.
        //
        // No terminal-status filter on purpose — see the trait doc: these ids
        // came out of a claim, whose own WHERE clause admits terminal rows
        // only, so they are terminal by construction.
        let tx = conn
            .unchecked_transaction()
            .context("begin unclaim children")?;
        let mut changed = 0usize;
        {
            let mut stmt = tx
                .prepare_cached("UPDATE tasks SET delivered = 0 WHERE delivered = 1 AND id = ?1")
                .context("prepare unclaim children")?;
            for id in ids {
                changed += stmt.execute(params![id]).context("unclaim children")?;
            }
        }
        tx.commit().context("commit unclaim children")?;
        Ok(changed)
    }

    async fn reconcile_lost(&self, id: &str, now_boot_id: &str) -> Result<bool> {
        let conn = self.conn.lock();
        let rec = conn
            .query_row(
                "SELECT * FROM tasks WHERE id = ?1",
                params![id],
                row_to_record,
            )
            .optional()
            .context("reconcile: load task")?;
        let Some(rec) = rec else { return Ok(false) };
        // Never reclaim a terminal record, and never one a live owner still holds.
        if rec.status.is_terminal() || !is_authoritative(&rec, now_boot_id) {
            return Ok(false);
        }
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE tasks SET status = 'lost', finished_at = ?1
              WHERE id = ?2 AND status = 'running'",
            params![now, id],
        )
        .context("reconcile: mark lost")?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroclaw_api::announce::AnnouncedOutcome;

    fn rec(id: &str, agent: &str, owner_pid: u32, boot: &str) -> TaskRecord {
        TaskRecord {
            id: id.into(),
            kind: TaskKind::Delegate,
            agent: agent.into(),
            status: TaskStatus::Running,
            owner_pid,
            owner_boot_id: boot.into(),
            heartbeat_at: None,
            depth: 0,
            parent_id: None,
            originator_route: None,
            delivered: false,
            idem_key: None,
            principal_id: None,
            executor: None,
            started_at: "2026-06-18T00:00:00Z".into(),
            finished_at: None,
        }
    }

    #[tokio::test]
    async fn create_get_roundtrip() {
        let s = SqliteTaskStore::new_in_memory().unwrap();
        s.create(rec("a", "main", 1, "boot-1")).await.unwrap();
        let got = s.get("a").await.unwrap().unwrap();
        assert_eq!(got.id, "a");
        assert_eq!(got.kind, TaskKind::Delegate);
        assert_eq!(got.status, TaskStatus::Running);
        assert!(s.get("missing").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn update_status_sets_terminal_and_finished_at() {
        let s = SqliteTaskStore::new_in_memory().unwrap();
        s.create(rec("a", "main", 1, "boot-1")).await.unwrap();
        s.update_status("a", TaskStatus::Completed, Some("done".into()), None)
            .await
            .unwrap();
        let got = s.get("a").await.unwrap().unwrap();
        assert_eq!(got.status, TaskStatus::Completed);
        assert!(got.finished_at.is_some());
    }

    #[tokio::test]
    async fn list_running_and_by_agent() {
        let s = SqliteTaskStore::new_in_memory().unwrap();
        s.create(rec("a", "main", 1, "b")).await.unwrap();
        s.create(rec("b", "main", 1, "b")).await.unwrap();
        s.create(rec("c", "other", 1, "b")).await.unwrap();
        s.update_status("b", TaskStatus::Completed, None, None)
            .await
            .unwrap();
        assert_eq!(s.list_running().await.unwrap().len(), 2); // a + c
        assert_eq!(s.list_by_agent("main").await.unwrap().len(), 2); // a + b
        assert_eq!(s.count_by_agent("main").unwrap(), 2);
    }

    #[tokio::test]
    async fn finish_task_sets_terminal_output_error_and_finished_at() {
        let s = SqliteTaskStore::new_in_memory().unwrap();
        s.create(rec("a", "main", 1, "boot-1")).await.unwrap();
        let did = s
            .finish_task(
                "a",
                TaskStatus::Failed,
                Some("partial output"),
                Some("boom"),
                true,
            )
            .await
            .unwrap();
        assert!(did, "a non-terminal row must be finished");
        let got = s.get("a").await.unwrap().unwrap();
        assert_eq!(got.status, TaskStatus::Failed);
        assert!(got.finished_at.is_some());
        assert!(got.delivered);

        // TaskRecord doesn't carry output/error; read them straight off the row.
        let conn = s.conn.lock();
        let (output, error): (Option<String>, Option<String>) = conn
            .query_row("SELECT output, error FROM tasks WHERE id = 'a'", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        drop(conn);
        assert_eq!(
            output.as_deref(),
            Some("partial output"),
            "output must survive finish_task"
        );
        assert_eq!(
            error.as_deref(),
            Some("boom"),
            "error must survive finish_task"
        );
    }

    /// The whole point of this method: `finished` and `delivered` land in one
    /// write, so a child finished with `delivered = true` is never offered to
    /// `claim_undelivered_children` — there is no window where it was
    /// terminal-but-undelivered for a concurrent claim to catch.
    #[tokio::test]
    async fn finish_task_delivered_true_is_never_claimed() {
        let s = SqliteTaskStore::new_in_memory().unwrap();
        s.create(child_of("kid", "mum", TaskStatus::Running))
            .await
            .unwrap();
        s.finish_task("kid", TaskStatus::Completed, Some("done"), None, true)
            .await
            .unwrap();

        assert!(
            s.claim_undelivered_children("mum")
                .await
                .unwrap()
                .is_empty(),
            "a child finished with delivered = true must not be claimable"
        );
    }

    /// A child finished with `delivered = false` (the coordinator has not yet
    /// handed the result to a waiter) is exactly the row
    /// `claim_undelivered_children` exists to find, and only once.
    #[tokio::test]
    async fn finish_task_delivered_false_is_claimed_exactly_once() {
        let s = SqliteTaskStore::new_in_memory().unwrap();
        s.create(child_of("kid", "mum", TaskStatus::Running))
            .await
            .unwrap();
        s.finish_task("kid", TaskStatus::Completed, Some("done"), None, false)
            .await
            .unwrap();

        let first = s.claim_undelivered_children("mum").await.unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].task_id, "kid");
        assert_eq!(first[0].output.as_deref(), Some("done"));

        let second = s.claim_undelivered_children("mum").await.unwrap();
        assert!(
            second.is_empty(),
            "a second waker must not re-announce a completion already claimed"
        );
    }

    #[tokio::test]
    async fn finish_task_rejects_a_non_terminal_status() {
        let s = SqliteTaskStore::new_in_memory().unwrap();
        s.create(rec("a", "main", 1, "boot-1")).await.unwrap();

        let err = s
            .finish_task("a", TaskStatus::Running, None, None, true)
            .await
            .unwrap_err();
        assert!(
            format!("{err}").contains("terminal"),
            "error must say why: {err}"
        );

        // Rejected before any write: the row must be untouched.
        let got = s.get("a").await.unwrap().unwrap();
        assert_eq!(got.status, TaskStatus::Running);
        assert!(!got.delivered);
    }

    /// A terminal row is never re-finished by a second call — mirrors
    /// `update_status`'s existing "NOT IN terminal" guard. Decided behavior:
    /// no-op, reported via `Ok(false)`, not an error and not a silent
    /// overwrite of the first outcome.
    #[tokio::test]
    async fn finish_task_does_not_re_finish_an_already_terminal_row() {
        let s = SqliteTaskStore::new_in_memory().unwrap();
        s.create(rec("a", "main", 1, "boot-1")).await.unwrap();
        s.finish_task("a", TaskStatus::Completed, Some("first"), None, false)
            .await
            .unwrap();

        let did = s
            .finish_task("a", TaskStatus::Failed, Some("second"), Some("oops"), true)
            .await
            .unwrap();
        assert!(!did, "an already-terminal row must not be re-finished");

        let got = s.get("a").await.unwrap().unwrap();
        assert_eq!(got.status, TaskStatus::Completed, "first outcome sticks");
        assert!(
            !got.delivered,
            "the second call's delivered=true must not overwrite the first outcome"
        );
    }

    fn child_of(id: &str, parent: &str, status: TaskStatus) -> TaskRecord {
        TaskRecord {
            parent_id: Some(parent.into()),
            status,
            ..rec(id, "main", 1, "boot-1")
        }
    }

    /// The escape hatch for a claim that never reached the model: the rows go
    /// back, and the next claim finds them again.
    ///
    /// Without this, a turn that consumed its announcements and then failed
    /// before its provider call would have destroyed them — the flag says
    /// delivered, and nothing looks at a delivered row twice.
    #[tokio::test]
    async fn unclaimed_children_become_claimable_again() {
        let s = SqliteTaskStore::new_in_memory().unwrap();
        s.create(child_of("kid", "mum", TaskStatus::Completed))
            .await
            .unwrap();

        let claimed = s.claim_undelivered_children("mum").await.unwrap();
        assert_eq!(claimed.len(), 1);
        assert!(
            s.claim_undelivered_children("mum")
                .await
                .unwrap()
                .is_empty(),
            "the claim consumed it"
        );

        let returned = s.unclaim_children(&["kid".to_string()]).await.unwrap();
        assert_eq!(returned, 1, "one row went back");

        let reclaimed = s.claim_undelivered_children("mum").await.unwrap();
        assert_eq!(reclaimed.len(), 1, "the announcement is news again");
        assert_eq!(reclaimed[0].task_id, "kid");
    }

    /// A retry must not manufacture a second delivery: the second unclaim
    /// matches nothing, because the row is no longer flagged delivered.
    #[tokio::test]
    async fn unclaiming_twice_changes_nothing_the_second_time() {
        let s = SqliteTaskStore::new_in_memory().unwrap();
        s.create(child_of("kid", "mum", TaskStatus::Completed))
            .await
            .unwrap();
        s.claim_undelivered_children("mum").await.unwrap();

        let ids = vec!["kid".to_string()];
        assert_eq!(s.unclaim_children(&ids).await.unwrap(), 1);
        assert_eq!(
            s.unclaim_children(&ids).await.unwrap(),
            0,
            "idempotent: nothing left to return"
        );
        assert_eq!(
            s.claim_undelivered_children("mum").await.unwrap().len(),
            1,
            "and the row was returned exactly once, not twice"
        );
    }

    /// One failed turn must not resurrect announcements another turn has
    /// already shown its parent. Only the ids handed over come back.
    #[tokio::test]
    async fn unclaim_touches_only_the_ids_it_is_given() {
        let s = SqliteTaskStore::new_in_memory().unwrap();
        s.create(child_of("mine", "mum", TaskStatus::Completed))
            .await
            .unwrap();
        s.create(child_of("theirs", "dad", TaskStatus::Completed))
            .await
            .unwrap();
        s.claim_undelivered_children("mum").await.unwrap();
        s.claim_undelivered_children("dad").await.unwrap();

        let returned = s.unclaim_children(&["mine".to_string()]).await.unwrap();
        assert_eq!(returned, 1);

        assert_eq!(
            s.claim_undelivered_children("mum").await.unwrap().len(),
            1,
            "the returned announcement is claimable again"
        );
        assert!(
            s.claim_undelivered_children("dad")
                .await
                .unwrap()
                .is_empty(),
            "another parent's delivered announcement must stay delivered"
        );
    }

    /// A guard with nothing to return must not issue a statement that could
    /// match anything at all.
    #[tokio::test]
    async fn unclaiming_no_ids_is_a_no_op() {
        let s = SqliteTaskStore::new_in_memory().unwrap();
        s.create(child_of("kid", "mum", TaskStatus::Completed))
            .await
            .unwrap();
        s.claim_undelivered_children("mum").await.unwrap();

        assert_eq!(s.unclaim_children(&[]).await.unwrap(), 0);
        assert!(
            s.claim_undelivered_children("mum")
                .await
                .unwrap()
                .is_empty(),
            "an empty unclaim must not return anything"
        );
    }

    /// The invariant the whole announce path rests on: a completion is claimed
    /// once. An announced completion becomes a parent turn, so claiming twice
    /// means the parent acts on the same result twice.
    #[tokio::test]
    async fn a_completion_is_claimed_exactly_once() {
        let s = SqliteTaskStore::new_in_memory().unwrap();
        s.create(child_of("kid", "mum", TaskStatus::Completed))
            .await
            .unwrap();

        let first = s.claim_undelivered_children("mum").await.unwrap();
        assert_eq!(first.len(), 1, "the completion must be claimable once");
        assert_eq!(first[0].task_id, "kid");

        let second = s.claim_undelivered_children("mum").await.unwrap();
        assert!(
            second.is_empty(),
            "a second waker must not re-announce a delivered completion"
        );
    }

    /// The point of announcing at all. A parent told "your child finished"
    /// without being told what it produced has learned nothing it can act on —
    /// and `TaskRecord` drops these columns, so this is easy to get wrong.
    #[tokio::test]
    async fn an_announcement_carries_the_result_not_just_the_verdict() {
        let s = SqliteTaskStore::new_in_memory().unwrap();
        s.create(child_of("worker", "mum", TaskStatus::Running))
            .await
            .unwrap();
        s.update_status(
            "worker",
            TaskStatus::Completed,
            Some("the answer is 42".into()),
            None,
        )
        .await
        .unwrap();

        let claimed = s.claim_undelivered_children("mum").await.unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(
            claimed[0].output.as_deref(),
            Some("the answer is 42"),
            "the child's output must survive into the announcement"
        );
    }

    /// A failure must arrive with its reason attached, or the parent can only
    /// report that something went wrong.
    #[tokio::test]
    async fn a_failed_child_announces_why() {
        let s = SqliteTaskStore::new_in_memory().unwrap();
        s.create(child_of("worker", "mum", TaskStatus::Running))
            .await
            .unwrap();
        s.update_status(
            "worker",
            TaskStatus::Failed,
            None,
            Some("provider refused the request".into()),
        )
        .await
        .unwrap();

        let claimed = s.claim_undelivered_children("mum").await.unwrap();
        assert_eq!(claimed[0].outcome, AnnouncedOutcome::Failed);
        assert_eq!(
            claimed[0].detail.as_deref(),
            Some("provider refused the request")
        );
    }

    /// Failure, timeout, cancellation and loss are all news the parent needs.
    /// Announcing only success would leave a parent waiting forever on a child
    /// that died — silence is the one outcome that must never be reported.
    #[tokio::test]
    async fn every_terminal_outcome_is_announced_not_just_success() {
        let s = SqliteTaskStore::new_in_memory().unwrap();
        for (id, status) in [
            ("done", TaskStatus::Completed),
            ("broke", TaskStatus::Failed),
            ("stopped", TaskStatus::Cancelled),
            ("vanished", TaskStatus::Lost),
            ("slow", TaskStatus::TimedOut),
        ] {
            s.create(child_of(id, "mum", status)).await.unwrap();
        }

        let claimed = s.claim_undelivered_children("mum").await.unwrap();
        let mut ids: Vec<&str> = claimed.iter().map(|r| r.task_id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["broke", "done", "slow", "stopped", "vanished"]);

        let mut outcomes: Vec<&str> = claimed.iter().map(|r| r.outcome.as_str()).collect();
        outcomes.sort_unstable();
        assert_eq!(
            outcomes,
            vec!["cancelled", "completed", "failed", "lost", "timed_out"],
            "each status must map to its own outcome, not collapse"
        );
    }

    /// A child still working is not news yet.
    #[tokio::test]
    async fn a_running_child_is_not_announced() {
        let s = SqliteTaskStore::new_in_memory().unwrap();
        s.create(child_of("busy", "mum", TaskStatus::Running))
            .await
            .unwrap();
        s.create(child_of("resting", "mum", TaskStatus::Paused))
            .await
            .unwrap();

        assert!(
            s.claim_undelivered_children("mum")
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// One parent's results must never surface in another parent's turn.
    #[tokio::test]
    async fn claims_are_scoped_to_one_parent() {
        let s = SqliteTaskStore::new_in_memory().unwrap();
        s.create(child_of("mine", "mum", TaskStatus::Completed))
            .await
            .unwrap();
        s.create(child_of("theirs", "dad", TaskStatus::Completed))
            .await
            .unwrap();

        let mine = s.claim_undelivered_children("mum").await.unwrap();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].task_id, "mine");

        let theirs = s.claim_undelivered_children("dad").await.unwrap();
        assert_eq!(theirs.len(), 1);
        assert_eq!(theirs[0].task_id, "theirs");
    }

    /// Several children finishing together arrive as one batch, so a parent
    /// with ten workers wakes once rather than ten times.
    #[tokio::test]
    async fn simultaneous_completions_arrive_as_one_batch() {
        let s = SqliteTaskStore::new_in_memory().unwrap();
        for i in 0..10 {
            s.create(child_of(&format!("kid{i}"), "mum", TaskStatus::Completed))
                .await
                .unwrap();
        }
        assert_eq!(s.claim_undelivered_children("mum").await.unwrap().len(), 10);
        assert!(
            s.claim_undelivered_children("mum")
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// A parentless task belongs to nobody's conversation and must never be
    /// swept into one.
    #[tokio::test]
    async fn a_task_without_a_parent_is_never_claimed() {
        let s = SqliteTaskStore::new_in_memory().unwrap();
        let mut orphan = rec("solo", "main", 1, "boot-1");
        orphan.status = TaskStatus::Completed;
        s.create(orphan).await.unwrap();

        assert!(
            s.claim_undelivered_children("mum")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(s.claim_undelivered_children("").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn reconcile_lost_only_when_authoritative() {
        let s = SqliteTaskStore::new_in_memory().unwrap();
        // prior-boot orphan ⇒ reclaimable
        s.create(rec("orphan", "main", 999_999, "boot-OLD"))
            .await
            .unwrap();
        assert!(s.reconcile_lost("orphan", "boot-NEW").await.unwrap());
        assert_eq!(
            s.get("orphan").await.unwrap().unwrap().status,
            TaskStatus::Lost
        );

        // live same-boot owner ⇒ NOT reclaimable (split-brain guard)
        let me = std::process::id();
        s.create(rec("live", "main", me, "boot-NEW")).await.unwrap();
        assert!(!s.reconcile_lost("live", "boot-NEW").await.unwrap());
        assert_eq!(
            s.get("live").await.unwrap().unwrap().status,
            TaskStatus::Running
        );

        // already-terminal ⇒ no-op
        s.create(rec("done", "main", 0, "boot-OLD")).await.unwrap();
        s.update_status("done", TaskStatus::Completed, None, None)
            .await
            .unwrap();
        assert!(!s.reconcile_lost("done", "boot-NEW").await.unwrap());
    }

    #[tokio::test]
    async fn heartbeat_only_from_owner_boot() {
        let s = SqliteTaskStore::new_in_memory().unwrap();
        s.create(rec("a", "main", 1, "boot-1")).await.unwrap();
        s.heartbeat("a", "boot-OTHER").await.unwrap(); // wrong boot: no-op
        assert!(s.get("a").await.unwrap().unwrap().heartbeat_at.is_none());
        s.heartbeat("a", "boot-1").await.unwrap(); // owner: stamps
        assert!(s.get("a").await.unwrap().unwrap().heartbeat_at.is_some());
    }

    #[tokio::test]
    async fn claim_owner_updates_canonical_owner_fields_for_resumed_task() {
        let s = SqliteTaskStore::new_in_memory().unwrap();
        s.create(rec("a", "main", 1, "boot-old")).await.unwrap();

        s.claim_owner("a", 42, "boot-new").await.unwrap();

        let got = s.get("a").await.unwrap().unwrap();
        assert_eq!(got.owner_pid, 42);
        assert_eq!(got.owner_boot_id, "boot-new");
        assert!(got.heartbeat_at.is_none());
    }

    /// T2 — coverage. `TaskStatus::TERMINAL` is the single source that
    /// `is_terminal`, `announced_outcome`, and the SQL filter above all now
    /// derive from or are checked against. This is the test that must fail
    /// if someone adds a terminal variant to `TaskStatus` and forgets to
    /// teach `announced_outcome` about it — pre-fix, that was exactly how
    /// the SQL filter, the parser, and the outcome map could drift apart
    /// without anything noticing.
    #[test]
    fn every_terminal_status_announces_and_is_in_the_sql_filter() {
        let sql_list = terminal_status_sql_list();
        for status in TaskStatus::TERMINAL {
            assert!(
                crate::control_plane::task_registry::announced_outcome(status).is_some(),
                "{status:?} is in TaskStatus::TERMINAL but announced_outcome returns None"
            );
            let spelling = status_to_db(status);
            assert!(
                sql_list.contains(&format!("'{spelling}'")),
                "{status:?} (spelled {spelling:?}) is in TaskStatus::TERMINAL but missing \
                 from the derived SQL filter: {sql_list}"
            );
        }
    }

    /// Structural guard against a *third* copy of the terminal-status literal
    /// showing up in this file. `every_terminal_status_announces_and_is_in_the_sql_filter`
    /// above only checks that `terminal_status_sql_list()` itself is complete; it
    /// says nothing about whether some *other* line in this file went back to
    /// spelling the set out by hand instead of calling that function — which is
    /// exactly how `update_task_status_record` and `claim_task_owner_record` each
    /// grew their own hardcoded `NOT IN ('completed','failed','cancelled','lost',
    /// 'timed_out')` before this pass.
    ///
    /// This test re-reads this file's own production source (everything before
    /// `mod tests`, so this test's own text — which necessarily mentions the
    /// literal pattern — can't trip itself) and fails if any `IN (` / `NOT IN (`
    /// status filter is not immediately followed by the `{}` placeholder that
    /// `terminal_status_sql_list()` fills in at call time. Reverting either
    /// `update_task_status_record`'s or `claim_task_owner_record`'s `NOT IN ({})`
    /// back to a hand-typed list of quoted statuses turns this red; so does
    /// adding a brand-new hand-typed status filter anywhere else in the file.
    #[test]
    fn every_sql_status_filter_is_built_from_terminal_status_sql_list_not_hand_typed() {
        const SRC: &str = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/control_plane/task_store_sqlite.rs"
        ));
        let production_src = SRC.split("mod tests").next().unwrap_or(SRC);

        let offenders: Vec<String> = production_src
            .lines()
            .enumerate()
            .filter_map(|(i, line)| {
                let idx = line.find("IN (")?;
                let after_paren = &line[idx + "IN (".len()..];
                if after_paren.starts_with("{})") {
                    None
                } else {
                    Some(format!("line {}: {}", i + 1, line.trim()))
                }
            })
            .collect();

        assert!(
            offenders.is_empty(),
            "found a SQL status filter not built from terminal_status_sql_list() \
             (expected every `IN (`/`NOT IN (` here to be immediately followed by \
             the `{{}}` placeholder):\n{}",
            offenders.join("\n")
        );
    }

    /// A status this binary cannot map must still leave with an outcome —
    /// `Lost`, with a detail naming the raw status — never an `Err`, since by
    /// the time `resolve_claimed_outcome` is called the row is already
    /// flagged delivered and cannot be re-claimed if this failed instead.
    #[test]
    fn an_unmappable_status_degrades_to_lost_with_a_detail() {
        let (outcome, detail) = resolve_claimed_outcome("t1", "not_a_real_status");
        assert_eq!(outcome, AnnouncedOutcome::Lost);
        let detail = detail.expect("degrade must explain itself");
        assert!(
            detail.contains("not_a_real_status"),
            "detail must name the raw status: {detail}"
        );
    }

    /// T1 — partial-decode does not lose the batch, and does not lose the row.
    ///
    /// The UPDATE ... RETURNING that claims children runs as one statement
    /// against every matched row, so `delivered = 1` is set for all three
    /// children below in the same statement execution that yields their
    /// RETURNING rows — regardless of which of those rows this test can go
    /// on to decode on the Rust side. Corrupting `bad`'s `output` column to a
    /// BLOB (not a value `String`/`Option<String>` can convert from) makes
    /// `row.get::<_, Option<String>>("output")` fail inside the row-mapping
    /// closure, but only *after* `bad`'s `id` — a `TEXT PRIMARY KEY NOT
    /// NULL` column this corruption never touches — has already decoded.
    /// So `bad` is not the "row that cannot be read at all" case: its name
    /// is known, it is already consumed (`delivered = 1`), and it must come
    /// back announced as `Lost` rather than vanish, same as its siblings
    /// come back with their real outcomes.
    ///
    /// (A corrupted *status* column, by contrast, cannot reach this loop at
    /// all: the WHERE filter is derived from the exact same serde spellings
    /// `status_from_db` parses, so a status string that fails to parse can
    /// never match the filter in the first place — it is simply never
    /// selected, not degraded. That is a real, deliberate consequence of
    /// single-sourcing the terminal list, not an oversight; see
    /// `resolve_claimed_outcome`'s own direct test above for coverage of the
    /// degrade branch that guards against this filter and the parser ever
    /// drifting apart again.)
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // conn is dropped before .await; clippy false positive
    async fn a_corrupt_row_does_not_sink_its_siblings() {
        let s = SqliteTaskStore::new_in_memory().unwrap();
        s.create(child_of("good1", "mum", TaskStatus::Completed))
            .await
            .unwrap();
        s.create(child_of("bad", "mum", TaskStatus::Completed))
            .await
            .unwrap();
        s.create(child_of("good2", "mum", TaskStatus::Failed))
            .await
            .unwrap();

        {
            let conn = s.conn.lock();
            conn.execute("UPDATE tasks SET output = X'DEADBEEF' WHERE id = 'bad'", [])
                .expect("corrupt bad's output column with a BLOB");
        }

        let claimed = s.claim_undelivered_children("mum").await.unwrap();
        let mut ids: Vec<&str> = claimed.iter().map(|a| a.task_id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            vec!["bad", "good1", "good2"],
            "a row whose id decoded must still be announced, even when another column on \
             it did not — the claim must not crash, and the named row must not disappear"
        );

        let bad = claimed
            .iter()
            .find(|a| a.task_id == "bad")
            .expect("bad's id decoded, so it must be present");
        assert_eq!(
            bad.outcome,
            AnnouncedOutcome::Lost,
            "a row that could not be fully decoded has no reliable result to report"
        );
        assert!(
            bad.detail.as_deref().is_some_and(|d| !d.is_empty()),
            "the degrade must explain itself: {:?}",
            bad.detail
        );

        // The corrupt row was flagged delivered by the same UPDATE statement
        // that flagged its siblings, and it was just announced above — it
        // must not be silently retried, because a second claim announcing
        // the same completion twice would be a double-run, not a recovery.
        let conn = s.conn.lock();
        let bad_delivered: i64 = conn
            .query_row("SELECT delivered FROM tasks WHERE id = 'bad'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            bad_delivered, 1,
            "the corrupt row was consumed by the claim"
        );
        drop(conn);

        let second = s.claim_undelivered_children("mum").await.unwrap();
        assert!(
            second.is_empty(),
            "delivered-once must still hold: no row is re-offered on a second claim"
        );
    }

    /// Discriminating test for the id-first decode fix: corrupt a non-id
    /// column (`agent`, written as a BLOB — not an INTEGER, because `agent`
    /// has TEXT column affinity and SQLite would silently coerce an inserted
    /// integer literal into its text representation before storage, which
    /// would decode as `String` just fine and defeat the point; a BLOB has
    /// no such affinity conversion and `String`'s `FromSql` genuinely fails
    /// on it) and check every part of the contract this row must now
    /// satisfy. Pre-fix, this row's `id` was read inside the same fallible
    /// closure as `agent`, so the whole row's decode failed as one unit and
    /// was silently dropped — `continue`d out of the loop with no id ever
    /// recovered from the `Err`. Reverting the fix's id-first read (decoding
    /// the whole row at once, `continue`ing on any single column's failure)
    /// must turn this test — and only this test — red.
    #[tokio::test]
    async fn a_non_id_decode_failure_is_announced_lost_not_dropped() {
        let s = SqliteTaskStore::new_in_memory().unwrap();
        s.create(child_of("kid", "mum", TaskStatus::Completed))
            .await
            .unwrap();

        {
            let conn = s.conn.lock();
            conn.execute("UPDATE tasks SET agent = X'DEADBEEF' WHERE id = 'kid'", [])
                .expect("corrupt kid's agent column with a BLOB");
        }

        let claimed = s.claim_undelivered_children("mum").await.unwrap();
        let kid = claimed
            .iter()
            .find(|a| a.task_id == "kid")
            .expect("claim must return the id even though `agent` failed to decode");
        assert_eq!(
            kid.outcome,
            AnnouncedOutcome::Lost,
            "a row this binary could not fully decode has no reliable result"
        );
        assert!(
            kid.detail.as_deref().is_some_and(|d| !d.is_empty()),
            "the detail must name the decode failure: {:?}",
            kid.detail
        );

        let conn = s.conn.lock();
        let delivered: i64 = conn
            .query_row("SELECT delivered FROM tasks WHERE id = 'kid'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            delivered, 1,
            "the row was consumed by the UPDATE ... RETURNING regardless of decode outcome — \
             which is exactly why it must be announced, not silently skipped"
        );
    }

    /// T3 — record an empirical fact, do not assert one not observed.
    ///
    /// A single UPDATE statement (RETURNING or not) is atomic across every
    /// row it matches — that much is ordinary SQLite semantics and is not
    /// what this test probes. What this test probes is narrower and less
    /// obvious: if the *caller* does not step a RETURNING statement to
    /// completion (`SQLITE_DONE`) — which is exactly what the pre-fix
    /// `anyhow::bail!` did, by returning `Err` and letting the local `rows`
    /// variable drop mid-iteration — does rusqlite's `Rows::drop` (which
    /// calls `sqlite3_reset` on the still-active statement; see
    /// `rusqlite::row::Rows`'s `Drop` impl) roll back the writes that
    /// statement already made, including for rows already yielded via
    /// RETURNING before the abandonment? Or does each row's write commit as
    /// it is yielded, independent of whether later rows are ever stepped to?
    ///
    /// MEASURED, not assumed. Three undelivered terminal children, one
    /// `next()`, then the statement is dropped: all three come back
    /// `delivered = 1`. Two facts fall out of that single number.
    ///
    /// First, abandoning the iteration rolls back nothing. An earlier draft
    /// of this test asserted the opposite, reasoning from `Rows::drop`
    /// calling `sqlite3_reset` on a still-active statement; the run refuted
    /// it (`left: 3, right: 0`). Reset is not rollback.
    ///
    /// Second — and this is why one step flags three rows — SQLite runs the
    /// UPDATE to completion *before* yielding the first RETURNING row. The
    /// output is buffered; it is not a cursor that writes as you walk it.
    ///
    /// Together those settle how literally to read this function's
    /// pre-fix failure: every matched row was durably flagged delivered
    /// before the decode loop ever saw its first row, so a `bail!` anywhere
    /// in that loop lost the *entire batch* permanently — the rows can
    /// never be re-selected, and the parent never learns any of its
    /// children finished. Not starvation-by-retry, which would at least
    /// leave the data recoverable. Permanent, silent loss. Hence the fix:
    /// once the claim has run, drain it completely and announce everything,
    /// degrading what cannot be decoded rather than dropping it.
    #[tokio::test]
    async fn abandoning_a_returning_statement_still_commits_every_matched_row() {
        let s = SqliteTaskStore::new_in_memory().unwrap();
        for id in ["a", "b", "c"] {
            s.create(child_of(id, "mum", TaskStatus::Completed))
                .await
                .unwrap();
        }

        {
            let conn = s.conn.lock();
            let mut stmt = conn
                .prepare(
                    "UPDATE tasks SET delivered = 1
                      WHERE parent_id = 'mum' AND delivered = 0 AND status = 'completed'
                  RETURNING id",
                )
                .unwrap();
            let mut rows = stmt.query([]).unwrap();
            // Step exactly once, then let `rows` (and `stmt`) drop un-stepped
            // to SQLITE_DONE — reproducing what the pre-fix bail! did to the
            // production statement via early return.
            let _first: String = rows.next().unwrap().unwrap().get(0).unwrap();
        }

        let conn = s.conn.lock();
        let delivered: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE parent_id = 'mum' AND delivered = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            delivered, 3,
            "measured: stepping a RETURNING statement once and dropping it still leaves \
             every matched row flagged. The UPDATE completes before the first row is \
             yielded, and abandoning the iteration rolls back nothing — which is why a \
             bail! in the decode loop used to lose the whole claimed batch for good."
        );
    }

    #[tokio::test]
    async fn v7_store_migrates_null_executor_and_coalesce_claims_agent() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("control_plane.db");
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE tasks (
                     id              TEXT PRIMARY KEY,
                     kind            TEXT NOT NULL,
                     agent           TEXT NOT NULL,
                     status          TEXT NOT NULL,
                     owner_pid       INTEGER NOT NULL DEFAULT 0,
                     owner_boot_id   TEXT NOT NULL DEFAULT '',
                     heartbeat_at    TEXT,
                     depth           INTEGER NOT NULL DEFAULT 0,
                     parent_id       TEXT,
                     originator_route TEXT,
                     delivered       INTEGER NOT NULL DEFAULT 0,
                     idem_key        TEXT,
                     principal_id    TEXT,
                     started_at      TEXT NOT NULL,
                     finished_at     TEXT,
                     output          TEXT,
                     error           TEXT
                 );
                 INSERT INTO tasks (
                     id, kind, agent, status, owner_pid, owner_boot_id, depth,
                     parent_id, delivered, started_at, finished_at, output
                 ) VALUES (
                     'kid', 'delegate', 'parent-alias', 'completed', 1, 'boot-old', 0,
                     'mum', 0, '2026-06-18T00:00:00Z', '2026-06-18T00:00:01Z',
                     'old-row-output'
                 );
                 PRAGMA user_version = 7;",
            )
            .unwrap();
        }

        let s = SqliteTaskStore::new(dir.path()).unwrap();
        {
            let conn = s.conn.lock();
            let version: i64 = conn
                .query_row("PRAGMA user_version", [], |row| row.get(0))
                .unwrap();
            assert_eq!(version, CONTROL_PLANE_SCHEMA_VERSION);
        }

        let got = s
            .get("kid")
            .await
            .unwrap()
            .expect("migrated row must be readable");
        assert!(
            got.executor.is_none(),
            "pre-v8 rows have NULL executor after ALTER"
        );
        assert_eq!(got.agent, "parent-alias");
        assert_eq!(got.status, TaskStatus::Completed);

        let claimed = s.claim_undelivered_children("mum").await.unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(
            claimed[0].agent, "parent-alias",
            "COALESCE(executor, agent) must fall back to agent when executor is NULL"
        );
        assert_eq!(claimed[0].output.as_deref(), Some("old-row-output"));
    }
}
