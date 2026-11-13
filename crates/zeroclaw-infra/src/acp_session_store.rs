//! Historical ACP session reads, complete archives and controlled lifecycle cleanup.
//! New ACP editor traffic uses the gateway session owner; this store has no
//! transcript, token, plan or event writers.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use zeroclaw_api::model_provider::{ChatMessage, ConversationMessage, ToolCall, ToolResultMessage};
use zeroclaw_api::plan::PlanEntry;

pub struct AcpSessionStore {
    conn: Mutex<Connection>,
}

pub struct AcpSessionData {
    pub session_uuid: String,
    pub agent_alias: String,
    pub workspace_dir: String,
    pub token_count: u64,
    pub created_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
    pub messages: Vec<ConversationMessage>,
}

pub enum AcpSessionRestore {
    Missing,
    Killed,
    Restorable(AcpSessionData),
}

/// Lightweight summary for the ACP session picker. Avoids loading the full
/// message history just to render a one-line label per session.
pub struct AcpSessionSummary {
    pub session_uuid: String,
    pub agent_alias: String,
    pub workspace_dir: String,
    pub token_count: u64,
    pub created_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
    pub message_count: usize,
}

impl AcpSessionStore {
    pub fn new(workspace_dir: &Path) -> Result<Self> {
        let sessions_dir = workspace_dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir).context("Failed to create sessions directory")?;
        let db_path = sessions_dir.join("acp-sessions.db");

        let conn = Connection::open(&db_path)
            .with_context(|| format!("Failed to open ACP session DB: {}", db_path.display()))?;

        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;
             PRAGMA foreign_keys = ON;
             PRAGMA temp_store = MEMORY;",
        )
        .context("Failed to configure ACP session DB pragmas")?;

        // Schema is create-if-missing: ACP sessions are long-lived user data
        // and must survive daemon restarts. Never drop existing tables here.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS acp_sessions (
                 id            INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_uuid  TEXT NOT NULL UNIQUE,
                 agent_alias   TEXT NOT NULL,
                 workspace_dir TEXT NOT NULL,
                 token_count   INTEGER NOT NULL DEFAULT 0,
                 killed_at     TEXT,
                 created_at    TEXT NOT NULL,
                 last_activity TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_acp_sessions_uuid  ON acp_sessions(session_uuid);
             CREATE INDEX IF NOT EXISTS idx_acp_sessions_alias ON acp_sessions(agent_alias);

             CREATE TABLE IF NOT EXISTS acp_messages (
                 id                INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id        INTEGER NOT NULL REFERENCES acp_sessions(id) ON DELETE CASCADE,
                 role              TEXT NOT NULL,
                 content           TEXT NOT NULL,
                 reasoning_content TEXT,
                 created_at        TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_acp_messages_session ON acp_messages(session_id, id);

             CREATE TABLE IF NOT EXISTS acp_tool_calls (
                 id           INTEGER PRIMARY KEY AUTOINCREMENT,
                 message_id   INTEGER NOT NULL REFERENCES acp_messages(id) ON DELETE CASCADE,
                 tool_call_id TEXT NOT NULL,
                 tool_name    TEXT NOT NULL,
                 event_kind   TEXT NOT NULL,
                 payload      TEXT NOT NULL,
                 outcome      TEXT,
                 created_at   TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_acp_tool_calls_message ON acp_tool_calls(message_id, id);
             CREATE INDEX IF NOT EXISTS idx_acp_tool_calls_lookup  ON acp_tool_calls(tool_call_id);

             CREATE TABLE IF NOT EXISTS acp_session_events (
                 id         INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id INTEGER NOT NULL REFERENCES acp_sessions(id) ON DELETE CASCADE,
                 action     TEXT NOT NULL,
                 outcome    TEXT NOT NULL,
                 payload    TEXT,
                 created_at TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_acp_session_events_session ON acp_session_events(session_id, id);",
        )
        .context("Failed to create ACP session schema")?;

        Self::ensure_killed_at_column(&conn)
            .context("Failed to migrate ACP session killed marker")?;

        Self::ensure_plan_json_column(&conn)
            .context("Failed to migrate ACP session plan column")?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn ensure_killed_at_column(conn: &Connection) -> Result<()> {
        let mut stmt = conn
            .prepare("PRAGMA table_info(acp_sessions)")
            .context("Failed to inspect ACP session schema")?;
        let mut rows = stmt
            .query([])
            .context("Failed to read ACP session schema")?;
        while let Some(row) = rows
            .next()
            .context("Failed to read ACP session schema row")?
        {
            let column: String = row
                .get(1)
                .context("Failed to read ACP session column name")?;
            if column == "killed_at" {
                return Ok(());
            }
        }
        drop(rows);
        drop(stmt);

        match conn.execute("ALTER TABLE acp_sessions ADD COLUMN killed_at TEXT", []) {
            Ok(_) => Ok(()),
            Err(rusqlite::Error::SqliteFailure(_, Some(ref msg)))
                if msg.contains("duplicate column name") =>
            {
                Ok(())
            }
            Err(e) => Err(e).context("Failed to add ACP session killed marker"),
        }
    }

    /// Idempotent migration adding the `plan_json` column that stores the
    /// session's latest TodoWrite plan as a JSON array of `PlanEntry`.
    /// Existing user databases predate this column; add it if absent so
    /// the plan survives daemon restarts (durable, like the transcript).
    fn ensure_plan_json_column(conn: &Connection) -> Result<()> {
        let mut stmt = conn
            .prepare("PRAGMA table_info(acp_sessions)")
            .context("Failed to inspect ACP session schema")?;
        let mut rows = stmt
            .query([])
            .context("Failed to read ACP session schema")?;
        while let Some(row) = rows
            .next()
            .context("Failed to read ACP session schema row")?
        {
            let column: String = row
                .get(1)
                .context("Failed to read ACP session column name")?;
            if column == "plan_json" {
                return Ok(());
            }
        }
        drop(rows);
        drop(stmt);

        match conn.execute("ALTER TABLE acp_sessions ADD COLUMN plan_json TEXT", []) {
            Ok(_) => Ok(()),
            Err(rusqlite::Error::SqliteFailure(_, Some(ref msg)))
                if msg.contains("duplicate column name") =>
            {
                Ok(())
            }
            Err(e) => Err(e).context("Failed to add ACP session plan column"),
        }
    }

    /// Load session metadata and full message history for restore.
    /// Returns `None` if the session_uuid is not found.
    pub fn load_session(&self, session_uuid: &str) -> Result<Option<AcpSessionData>> {
        let conn = self.conn.lock();

        let row = conn.query_row(
            "SELECT id, agent_alias, workspace_dir, token_count, created_at, last_activity
             FROM acp_sessions WHERE session_uuid = ?1",
            params![session_uuid],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            },
        );

        let (session_id, agent_alias, workspace_dir, token_count, created_at_s, last_activity_s) =
            match row {
                Ok(r) => r,
                Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
                Err(e) => return Err(e).context("Failed to query ACP session"),
            };

        let created_at = parse_ts(&created_at_s, "created_at", session_uuid);
        let last_activity = parse_ts(&last_activity_s, "last_activity", session_uuid);

        let messages = Self::load_messages(&conn, session_id)?;

        Ok(Some(AcpSessionData {
            session_uuid: session_uuid.to_string(),
            agent_alias,
            workspace_dir,
            token_count: token_count.max(0) as u64,
            created_at,
            last_activity,
            messages,
        }))
    }

    /// Load only durable ACP rows that are allowed to become live sessions.
    /// Killed rows keep their transcript for history/export but are terminal
    /// for runtime restore paths.
    pub fn load_session_for_restore(&self, session_uuid: &str) -> Result<AcpSessionRestore> {
        let conn = self.conn.lock();

        let row = conn.query_row(
            "SELECT id, agent_alias, workspace_dir, token_count, created_at, last_activity, killed_at
             FROM acp_sessions WHERE session_uuid = ?1",
            params![session_uuid],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            },
        );

        let (
            session_id,
            agent_alias,
            workspace_dir,
            token_count,
            created_at_s,
            last_activity_s,
            killed_at,
        ) = match row {
            Ok(r) => r,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(AcpSessionRestore::Missing),
            Err(e) => return Err(e).context("Failed to query ACP session for restore"),
        };

        if killed_at.is_some() {
            return Ok(AcpSessionRestore::Killed);
        }

        let created_at = parse_ts(&created_at_s, "created_at", session_uuid);
        let last_activity = parse_ts(&last_activity_s, "last_activity", session_uuid);
        let messages = Self::load_messages(&conn, session_id)?;

        Ok(AcpSessionRestore::Restorable(AcpSessionData {
            session_uuid: session_uuid.to_string(),
            agent_alias,
            workspace_dir,
            token_count: token_count.max(0) as u64,
            created_at,
            last_activity,
            messages,
        }))
    }

    /// List restorable sessions as lightweight summaries, ordered by most recent
    /// activity first. This is the picker-facing read: it avoids the full
    /// message-history hydration that `load_session` performs. Killed rows keep
    /// history/export data but are terminal and must not be offered for restore.
    pub fn list_sessions(&self) -> Result<Vec<AcpSessionSummary>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT s.session_uuid,
                        s.agent_alias,
                        s.workspace_dir,
                        s.token_count,
                        s.created_at,
                        s.last_activity,
                        (SELECT COUNT(*) FROM acp_messages m WHERE m.session_id = s.id) AS message_count
                 FROM acp_sessions s
                 WHERE s.killed_at IS NULL
                 ORDER BY s.last_activity DESC",
            )
            .context("Failed to prepare ACP session list query")?;

        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            })
            .context("Failed to query ACP sessions")?;

        let mut out = Vec::new();
        for row in rows {
            let (
                session_uuid,
                agent_alias,
                workspace_dir,
                token_count,
                created_s,
                activity_s,
                msg_count,
            ) = row.context("Failed to read ACP session row")?;
            out.push(AcpSessionSummary {
                created_at: parse_ts(&created_s, "created_at", &session_uuid),
                last_activity: parse_ts(&activity_s, "last_activity", &session_uuid),
                session_uuid,
                agent_alias,
                workspace_dir,
                token_count: token_count.max(0) as u64,
                message_count: msg_count.max(0) as usize,
            });
        }
        Ok(out)
    }

    fn load_messages(conn: &Connection, session_id: i64) -> Result<Vec<ConversationMessage>> {
        // Pull all message rows.
        let mut msg_stmt = conn
            .prepare(
                "SELECT id, role, content, reasoning_content
                 FROM acp_messages WHERE session_id = ?1 ORDER BY id ASC",
            )
            .context("Failed to prepare message query")?;

        let msg_rows: Vec<(i64, String, String, Option<String>)> = msg_stmt
            .query_map(params![session_id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to read message rows")?;

        // For each message row, pull its tool_calls (event_kind='in') and
        // tool_results (event_kind='out') in id order.
        let mut tc_stmt = conn
            .prepare(
                "SELECT tool_call_id, tool_name, event_kind, payload
                 FROM acp_tool_calls WHERE message_id = ?1 ORDER BY id ASC",
            )
            .context("Failed to prepare tool_call query")?;

        let mut out = Vec::with_capacity(msg_rows.len());
        for (msg_id, role, content, reasoning_content) in msg_rows {
            // Split this message's tool_calls into ins and outs preserving order.
            let mut ins: Vec<ToolCall> = Vec::new();
            let mut outs: Vec<ToolResultMessage> = Vec::new();
            let rows = tc_stmt
                .query_map(params![msg_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()
                .context("Failed to read tool_call rows")?;
            for (tool_call_id, tool_name, event_kind, payload) in rows {
                match event_kind.as_str() {
                    "in" => ins.push(ToolCall {
                        id: tool_call_id,
                        name: tool_name,
                        arguments: payload,
                        extra_content: None,
                    }),
                    "out" => outs.push(ToolResultMessage {
                        tool_call_id,
                        content: payload,
                        // Carry the producing tool name (looked up from the
                        // matching 'in' row on write) so a resumed session
                        // stays provenance-aware for media-marker
                        // canonicalization
                        tool_name,
                    }),
                    other => {
                        ::zeroclaw_log::record!(
                            ERROR,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Read,
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "session_id": session_id,
                                "message_id": msg_id,
                                "event_kind": other,
                            })),
                            "unknown event_kind in acp_tool_calls"
                        );
                        return Err(anyhow::Error::msg(format!(
                            "unknown event_kind '{other}' in acp_tool_calls for message_id {msg_id}"
                        )));
                    }
                }
            }

            if ins.is_empty() && outs.is_empty() {
                // Pure chat message.
                out.push(ConversationMessage::Chat(ChatMessage { role, content }));
            } else {
                if !ins.is_empty() {
                    // Assistant turn that issued tool calls. The text may be empty.
                    out.push(ConversationMessage::AssistantToolCalls {
                        text: if content.is_empty() {
                            None
                        } else {
                            Some(content)
                        },
                        tool_calls: ins,
                        reasoning_content,
                    });
                }
                if !outs.is_empty() {
                    out.push(ConversationMessage::ToolResults(outs));
                }
            }
        }

        Ok(out)
    }

    /// Load the session's stored plan. Returns an empty vec when the
    /// session has no plan (NULL or absent). Malformed JSON is treated
    /// as an empty plan rather than a hard error, so a corrupt plan
    /// column never blocks session restore.
    pub fn get_plan(&self, session_uuid: &str) -> Result<Vec<PlanEntry>> {
        let conn = self.conn.lock();
        let plan_json: Option<String> = conn
            .query_row(
                "SELECT plan_json FROM acp_sessions WHERE session_uuid = ?1",
                params![session_uuid],
                |row| row.get(0),
            )
            .optional()
            .context("Failed to query plan_json")?
            .flatten();
        Ok(plan_json
            .and_then(|s| serde_json::from_str::<Vec<PlanEntry>>(&s).ok())
            .unwrap_or_default())
    }

    /// Delete a session and all its child rows (messages, tool calls, events
    /// cascade via FK). Returns `true` if the session existed.
    pub fn delete_session(&self, session_uuid: &str) -> Result<bool> {
        let conn = self.conn.lock();
        let rows = conn
            .execute(
                "DELETE FROM acp_sessions WHERE session_uuid = ?1",
                params![session_uuid],
            )
            .context("Failed to delete ACP session")?;
        Ok(rows > 0)
    }

    // ── per-agent cascade (agent deletion,───────────────────────────

    /// Count *live* ACP sessions for `agent_alias` — rows not yet killed
    /// (`killed_at IS NULL`). A non-zero count is a HARD blocker for deleting the
    /// agent: the operator must end the sessions first.
    pub fn count_live_sessions_by_agent(&self, agent_alias: &str) -> Result<usize> {
        let conn = self.conn.lock();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM acp_sessions WHERE agent_alias = ?1 AND killed_at IS NULL",
                params![agent_alias],
                |row| row.get(0),
            )
            .context("Failed to count live ACP sessions for agent")?;
        Ok(n.max(0) as usize)
    }

    /// Summaries of every ACP session (live or killed) attributed to
    /// `agent_alias`, for the export-then-delete archive.
    pub fn list_sessions_by_agent(&self, agent_alias: &str) -> Result<Vec<AcpSessionSummary>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT s.session_uuid,
                        s.agent_alias,
                        s.workspace_dir,
                        s.token_count,
                        s.created_at,
                        s.last_activity,
                        (SELECT COUNT(*) FROM acp_messages m WHERE m.session_id = s.id) AS message_count
                 FROM acp_sessions s
                 WHERE s.agent_alias = ?1
                 ORDER BY s.last_activity DESC",
            )
            .context("Failed to prepare ACP per-agent session query")?;

        let rows = stmt
            .query_map(params![agent_alias], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            })
            .context("Failed to query ACP sessions for agent")?;

        let mut out = Vec::new();
        for row in rows {
            let (
                session_uuid,
                agent_alias,
                workspace_dir,
                token_count,
                created_s,
                activity_s,
                msg_count,
            ) = row.context("Failed to read ACP session row")?;
            out.push(AcpSessionSummary {
                created_at: parse_ts(&created_s, "created_at", &session_uuid),
                last_activity: parse_ts(&activity_s, "last_activity", &session_uuid),
                session_uuid,
                agent_alias,
                workspace_dir,
                token_count: token_count.max(0) as u64,
                message_count: msg_count.max(0) as usize,
            });
        }
        Ok(out)
    }

    /// Delete every ACP session (live or killed) for `agent_alias`, returning the
    /// row count. Child tables (`acp_messages`/`acp_tool_calls`/`acp_session_events`)
    /// cascade via their `ON DELETE CASCADE` FKs (`foreign_keys = ON`).
    pub fn delete_sessions_by_agent(&self, agent_alias: &str) -> Result<usize> {
        let conn = self.conn.lock();
        let rows = conn
            .execute(
                "DELETE FROM acp_sessions WHERE agent_alias = ?1",
                params![agent_alias],
            )
            .context("Failed to delete ACP sessions for agent")?;
        Ok(rows)
    }

    /// Archive every persisted column and child row before deleting an agent's
    /// terminal sessions. The immediate transaction prevents a second store
    /// handle from adding data between export and the cascading DELETE.
    pub fn archive_and_delete_sessions_by_agent(
        &self,
        agent_alias: &str,
        archive_path: &Path,
    ) -> Result<usize> {
        use crate::owned_archive::{sqlite_rows, write_verified_json};
        let mut conn = self.conn.lock();
        let transaction =
            conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let live: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM acp_sessions WHERE agent_alias = ?1 AND killed_at IS NULL",
            [agent_alias],
            |row| row.get(0),
        )?;
        anyhow::ensure!(
            live == 0,
            "live ACP sessions remain; original ACP data retained"
        );
        let sessions = sqlite_rows(
            &transaction,
            "SELECT * FROM acp_sessions WHERE agent_alias = ?1 ORDER BY id",
            agent_alias,
        )?;
        let messages = sqlite_rows(
            &transaction,
            "SELECT m.* FROM acp_messages m JOIN acp_sessions s ON s.id = m.session_id
             WHERE s.agent_alias = ?1 ORDER BY m.id",
            agent_alias,
        )?;
        let tool_calls = sqlite_rows(
            &transaction,
            "SELECT t.* FROM acp_tool_calls t JOIN acp_messages m ON m.id = t.message_id
             JOIN acp_sessions s ON s.id = m.session_id WHERE s.agent_alias = ?1 ORDER BY t.id",
            agent_alias,
        )?;
        let events = sqlite_rows(
            &transaction,
            "SELECT e.* FROM acp_session_events e JOIN acp_sessions s ON s.id = e.session_id
             WHERE s.agent_alias = ?1 ORDER BY e.id",
            agent_alias,
        )?;
        let snapshot = serde_json::json!({
            "format": "zeroclaw-agent-archive", "version": 1, "store": "acp", "agent_alias": agent_alias,
            "tables": {"acp_sessions": sessions, "acp_messages": messages,
                "acp_tool_calls": tool_calls, "acp_session_events": events},
        });
        write_verified_json(archive_path, &snapshot).context("archive complete ACP history")?;
        let removed = transaction.execute(
            "DELETE FROM acp_sessions WHERE agent_alias = ?1",
            [agent_alias],
        )?;
        transaction.commit()?;
        Ok(removed)
    }

    /// Re-point every ACP session (live or killed) from `from` to `to`,
    /// returning the row count. The agent-rename cascadekeeps the
    /// session and its transcript; only the owning alias moves. Unlike delete,
    /// a live session (`killed_at IS NULL`) is no obstacle to rename.
    pub fn rename_sessions_by_agent(&self, from: &str, to: &str) -> Result<usize> {
        let conn = self.conn.lock();
        let rows = conn
            .execute(
                "UPDATE acp_sessions SET agent_alias = ?2 WHERE agent_alias = ?1",
                params![from, to],
            )
            .context("Failed to rename ACP session owner")?;
        Ok(rows)
    }

    /// Return whether this durable ACP session has been intentionally killed.
    /// Missing rows are not killed; callers can then use normal load handling
    /// to distinguish SESSION_NOT_FOUND from a terminal killed session.
    pub fn is_session_killed(&self, session_uuid: &str) -> Result<bool> {
        let conn = self.conn.lock();
        let row = conn.query_row(
            "SELECT CASE WHEN killed_at IS NULL THEN 0 ELSE 1 END
             FROM acp_sessions WHERE session_uuid = ?1",
            params![session_uuid],
            |row| row.get::<_, i64>(0),
        );
        match row {
            Ok(killed) => Ok(killed != 0),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
            Err(e) => Err(e).context("Failed to query ACP session killed marker"),
        }
    }
}

fn parse_ts(s: &str, field: &'static str, session_uuid: &str) -> DateTime<Utc> {
    s.parse::<DateTime<Utc>>().unwrap_or_else(|e| {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(
                ::serde_json::json!({
                    "session_uuid": session_uuid,
                    "field": field,
                    "error": e.to_string(),
                })
            ),
            "Failed to parse session timestamp"
        );
        Utc::now()
    })
}

#[cfg(test)]
mod archive_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn open_store() -> (TempDir, AcpSessionStore) {
        let tmp = TempDir::new().unwrap();
        let store = AcpSessionStore::new(tmp.path()).unwrap();
        (tmp, store)
    }

    #[test]
    fn new_creates_all_four_tables() {
        let (_tmp, store) = open_store();
        let conn = store.conn.lock();
        for table in [
            "acp_sessions",
            "acp_messages",
            "acp_tool_calls",
            "acp_session_events",
        ] {
            let name: String = conn
                .query_row(
                    "SELECT name FROM sqlite_master WHERE type='table' AND name = ?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap_or_else(|_| panic!("table {table} should exist"));
            assert_eq!(name, table);
        }
    }

    #[test]
    fn opens_in_wal_mode_to_avoid_blocking_runtime_threads() {
        let (_tmp, store) = open_store();
        let conn = store.conn.lock();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal", "ACP session DB must use WAL");
    }

    #[test]
    fn load_existing_session_metadata() {
        let (_tmp, store) = open_store();
        store
            .conn
            .lock()
            .execute_batch(
                "INSERT INTO acp_sessions
                   (session_uuid, agent_alias, workspace_dir, created_at, last_activity)
                 VALUES ('sess-abc', 'personal_code', '/home/user/project',
                         '2026-01-01T00:00:00Z', '2026-01-01T00:00:01Z');",
            )
            .unwrap();

        let data = store.load_session("sess-abc").unwrap().unwrap();
        assert_eq!(data.session_uuid, "sess-abc");
        assert_eq!(data.agent_alias, "personal_code");
        assert_eq!(data.workspace_dir, "/home/user/project");
        assert_eq!(data.token_count, 0);
        assert!(data.messages.is_empty());
    }

    #[test]
    fn load_nonexistent_session_returns_none() {
        let (_tmp, store) = open_store();
        assert!(store.load_session("nonexistent").unwrap().is_none());
    }

    #[test]
    fn get_plan_reads_existing_plan_and_empty_states() {
        use zeroclaw_api::plan::{PlanEntry, PlanPriority, PlanStatus};
        let (_tmp, store) = open_store();
        store
            .conn
            .lock()
            .execute_batch(
                r#"INSERT INTO acp_sessions
                   (session_uuid, agent_alias, workspace_dir, created_at, last_activity, plan_json)
                 VALUES
                   ('sess-plan', 'alpha', '/tmp/proj',
                    '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z',
                    '[{"content":"A","status":"completed","priority":"high"},
                      {"content":"B","status":"in_progress","priority":"medium","activeForm":"Doing B"}]'),
                   ('sess-no-plan', 'alpha', '/tmp/proj',
                    '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', NULL),
                   ('sess-cleared-plan', 'alpha', '/tmp/proj',
                    '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', '[]');"#,
            )
            .unwrap();

        let plan = vec![
            PlanEntry {
                content: "A".to_string(),
                status: PlanStatus::Completed,
                priority: PlanPriority::High,
                active_form: None,
            },
            PlanEntry {
                content: "B".to_string(),
                status: PlanStatus::InProgress,
                priority: PlanPriority::Medium,
                active_form: Some("Doing B".to_string()),
            },
        ];
        assert_eq!(store.get_plan("sess-plan").unwrap(), plan);
        assert!(store.get_plan("sess-no-plan").unwrap().is_empty());
        assert!(store.get_plan("sess-cleared-plan").unwrap().is_empty());
    }

    #[test]
    fn get_plan_empty_for_unknown_session() {
        let (_tmp, store) = open_store();
        assert!(store.get_plan("nope").unwrap().is_empty());
    }

    #[test]
    fn load_existing_chat_messages() {
        let (_tmp, store) = open_store();
        store
            .conn
            .lock()
            .execute_batch(
                "INSERT INTO acp_sessions
                   (id, session_uuid, agent_alias, workspace_dir, created_at, last_activity)
                 VALUES (1, 'sess-msgs', 'alpha', '/tmp/proj',
                         '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');
                 INSERT INTO acp_messages (session_id, role, content, created_at)
                 VALUES (1, 'user', 'hello', '2026-01-01T00:00:00Z'),
                        (1, 'assistant', 'hi', '2026-01-01T00:00:00Z');",
            )
            .unwrap();

        let data = store.load_session("sess-msgs").unwrap().unwrap();
        assert_eq!(data.messages.len(), 2);
        assert!(matches!(
            &data.messages[0],
            ConversationMessage::Chat(m) if m.role == "user" && m.content == "hello"
        ));
        assert!(matches!(
            &data.messages[1],
            ConversationMessage::Chat(m) if m.role == "assistant" && m.content == "hi"
        ));
    }

    #[test]
    fn load_existing_assistant_tool_calls_and_results() {
        let (_tmp, store) = open_store();
        // Both tool events belong to the same historical assistant message.
        store
            .conn
            .lock()
            .execute_batch(
                r#"INSERT INTO acp_sessions
                   (id, session_uuid, agent_alias, workspace_dir, created_at, last_activity)
                 VALUES (1, 'sess-variants', 'alpha', '/tmp/proj',
                         '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');
                 INSERT INTO acp_messages
                   (id, session_id, role, content, reasoning_content, created_at)
                 VALUES (1, 1, 'user', 'task', NULL, '2026-01-01T00:00:00Z'),
                        (2, 1, 'assistant', 'calling shell', 'think think', '2026-01-01T00:00:00Z'),
                        (3, 1, 'assistant', 'done', NULL, '2026-01-01T00:00:00Z');
                 INSERT INTO acp_tool_calls
                   (message_id, tool_call_id, tool_name, event_kind, payload, outcome, created_at)
                 VALUES (2, 'tc-1', 'shell', 'in', '{"command":"ls"}', NULL, '2026-01-01T00:00:00Z'),
                        (2, 'tc-1', 'shell', 'out', 'file.txt' || char(10), 'unknown', '2026-01-01T00:00:00Z');"#,
            )
            .unwrap();

        let data = store.load_session("sess-variants").unwrap().unwrap();
        assert_eq!(data.messages.len(), 4);

        // Historical AssistantToolCalls preserve text + tool_calls + reasoning.
        match &data.messages[1] {
            ConversationMessage::AssistantToolCalls {
                text,
                tool_calls,
                reasoning_content,
            } => {
                assert_eq!(text.as_deref(), Some("calling shell"));
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].id, "tc-1");
                assert_eq!(tool_calls[0].name, "shell");
                assert_eq!(tool_calls[0].arguments, r#"{"command":"ls"}"#);
                assert_eq!(reasoning_content.as_deref(), Some("think think"));
            }
            other => panic!("expected AssistantToolCalls, got {other:?}"),
        }

        // Historical ToolResults preserve tool_call_id + content + provenance.
        match &data.messages[2] {
            ConversationMessage::ToolResults(results) => {
                assert_eq!(results.len(), 1);
                assert_eq!(results[0].tool_call_id, "tc-1");
                assert_eq!(results[0].content, "file.txt\n");
                assert_eq!(results[0].tool_name, "shell");
            }
            other => panic!("expected ToolResults, got {other:?}"),
        }
    }

    #[test]
    fn delete_session_cascades_to_children() {
        let (_tmp, store) = open_store();
        store
            .conn
            .lock()
            .execute_batch(
                "INSERT INTO acp_sessions
                   (id, session_uuid, agent_alias, workspace_dir, created_at, last_activity)
                 VALUES (1, 'sess-del', 'alpha', '/tmp/proj',
                         '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');
                 INSERT INTO acp_messages (id, session_id, role, content, created_at)
                 VALUES (1, 1, 'assistant', 'calling', '2026-01-01T00:00:00Z');
                 INSERT INTO acp_tool_calls
                   (message_id, tool_call_id, tool_name, event_kind, payload, outcome, created_at)
                 VALUES (1, 'tc-1', 'shell', 'in', '{}', NULL, '2026-01-01T00:00:00Z'),
                        (1, 'tc-1', 'shell', 'out', 'ok', 'unknown', '2026-01-01T00:00:00Z');
                 INSERT INTO acp_session_events (session_id, action, outcome, created_at)
                 VALUES (1, 'disconnect', 'success', '2026-01-01T00:00:00Z');",
            )
            .unwrap();

        assert!(store.delete_session("sess-del").unwrap());

        let conn = store.conn.lock();
        for table in ["acp_messages", "acp_tool_calls", "acp_session_events"] {
            let count: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 0, "cascade should empty {table}");
        }
    }

    #[test]
    fn delete_nonexistent_session_returns_false() {
        let (_tmp, store) = open_store();
        assert!(!store.delete_session("ghost").unwrap());
    }

    #[test]
    fn killed_fixture_preserves_history_and_remains_terminal() {
        let (tmp, store) = open_store();
        store
            .conn
            .lock()
            .execute_batch(
                "INSERT INTO acp_sessions
                   (id, session_uuid, agent_alias, workspace_dir, created_at, last_activity)
                 VALUES (1, 'sess-kill', 'alpha', '/tmp/proj',
                         '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');
                 INSERT INTO acp_messages (session_id, role, content, created_at)
                 VALUES (1, 'user', 'keep this', '2026-01-01T00:00:00Z');",
            )
            .unwrap();

        assert!(!store.is_session_killed("sess-kill").unwrap());
        assert!(matches!(
            store.load_session_for_restore("sess-kill").unwrap(),
            AcpSessionRestore::Restorable(_)
        ));
        store.conn.lock().execute(
            "UPDATE acp_sessions SET killed_at = '2026-01-02T00:00:00Z' WHERE session_uuid = 'sess-kill'", [],
        ).unwrap();
        assert!(store.is_session_killed("sess-kill").unwrap());
        assert!(matches!(
            store.load_session_for_restore("sess-kill").unwrap(),
            AcpSessionRestore::Killed
        ));

        let data = store.load_session("sess-kill").unwrap().unwrap();
        assert_eq!(
            data.messages.len(),
            1,
            "kill marker must not delete durable transcript history"
        );

        drop(store);
        let reopened = AcpSessionStore::new(tmp.path()).unwrap();
        assert!(
            reopened.is_session_killed("sess-kill").unwrap(),
            "kill marker must survive store reopen"
        );
        assert!(
            reopened.load_session("sess-kill").unwrap().is_some(),
            "durable history remains loadable after reopen"
        );
        assert!(matches!(
            reopened.load_session_for_restore("sess-kill").unwrap(),
            AcpSessionRestore::Killed
        ));
    }

    #[test]
    fn missing_session_is_not_killed() {
        let (_tmp, store) = open_store();
        assert!(!store.is_session_killed("ghost").unwrap());
        assert!(matches!(
            store.load_session_for_restore("ghost").unwrap(),
            AcpSessionRestore::Missing
        ));
    }

    #[test]
    fn load_existing_token_count() {
        let (_tmp, store) = open_store();
        store
            .conn
            .lock()
            .execute_batch(
                "INSERT INTO acp_sessions
                   (session_uuid, agent_alias, workspace_dir, token_count, created_at, last_activity)
                 VALUES ('sess-tok', 'alpha', '/tmp/proj', 152306,
                         '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');",
            )
            .unwrap();
        assert_eq!(
            store.load_session("sess-tok").unwrap().unwrap().token_count,
            152_306,
            "historical token count must remain readable"
        );
    }

    #[test]
    fn list_sessions_returns_summaries_ordered_by_recent_activity() {
        let (_tmp, store) = open_store();
        store
            .conn
            .lock()
            .execute_batch(
                "INSERT INTO acp_sessions
                   (id, session_uuid, agent_alias, workspace_dir, token_count, created_at, last_activity)
                 VALUES (1, 'sess-old', 'alpha', '/tmp/a', 0,
                         '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z'),
                        (2, 'sess-new', 'beta', '/tmp/b', 1234,
                         '2026-01-01T00:00:01Z', '2026-01-01T00:00:01Z');
                 INSERT INTO acp_messages (session_id, role, content, created_at)
                 VALUES (2, 'user', 'hi', '2026-01-01T00:00:01Z');",
            )
            .unwrap();

        let list = store.list_sessions().unwrap();
        assert_eq!(list.len(), 2);
        // Most recent activity first.
        assert_eq!(list[0].session_uuid, "sess-new");
        assert_eq!(list[0].agent_alias, "beta");
        assert_eq!(list[0].workspace_dir, "/tmp/b");
        assert_eq!(list[0].message_count, 1);
        assert_eq!(list[0].token_count, 1234);
        assert_eq!(list[1].session_uuid, "sess-old");
        assert_eq!(list[1].message_count, 0);
    }

    #[test]
    fn list_sessions_empty_when_no_sessions() {
        let (_tmp, store) = open_store();
        assert!(store.list_sessions().unwrap().is_empty());
    }

    #[test]
    fn list_sessions_omits_killed_sessions() {
        let (_tmp, store) = open_store();
        store
            .conn
            .lock()
            .execute_batch(
                "INSERT INTO acp_sessions
                   (session_uuid, agent_alias, workspace_dir, created_at, last_activity, killed_at)
                 VALUES ('sess-live', 'alpha', '/tmp/live',
                         '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', NULL),
                        ('sess-killed', 'alpha', '/tmp/killed',
                         '2026-01-01T00:00:01Z', '2026-01-01T00:00:02Z', '2026-01-01T00:00:02Z');",
            )
            .unwrap();

        let list = store.list_sessions().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].session_uuid, "sess-live");
    }

    #[test]
    fn per_agent_cascade_counts_live_and_deletes_only_that_agent() {
        let (_tmp, store) = open_store();
        store
            .conn
            .lock()
            .execute_batch(
                "INSERT INTO acp_sessions
                   (session_uuid, agent_alias, workspace_dir, created_at, last_activity, killed_at)
                 VALUES ('a-live', 'alpha', '/ws/a1',
                         '2001-01-01T00:00:00Z', '2001-01-01T00:00:00Z', NULL),
                        ('a-killed', 'alpha', '/ws/a2',
                         '2026-01-01T00:00:00Z', '2026-01-01T00:00:01Z', '2026-01-01T00:00:01Z'),
                        ('b-live', 'beta', '/ws/b1',
                         '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', NULL);",
            )
            .unwrap();

        // Only the killed marker changes the HARD-refuse signal, never age.
        assert_eq!(store.count_live_sessions_by_agent("alpha").unwrap(), 1);
        assert_eq!(store.count_live_sessions_by_agent("beta").unwrap(), 1);
        assert_eq!(store.count_live_sessions_by_agent("ghost").unwrap(), 0);

        // list_by_agent returns all (live + killed) for export.
        assert_eq!(store.list_sessions_by_agent("alpha").unwrap().len(), 2);

        // delete_by_agent removes exactly that agent's sessions.
        assert_eq!(store.delete_sessions_by_agent("alpha").unwrap(), 2);
        assert!(store.list_sessions_by_agent("alpha").unwrap().is_empty());
        assert_eq!(store.list_sessions_by_agent("beta").unwrap().len(), 1);
    }

    #[test]
    fn rename_sessions_by_agent_repoints_live_and_killed() {
        let (_tmp, store) = open_store();
        store
            .conn
            .lock()
            .execute_batch(
                "INSERT INTO acp_sessions
                   (session_uuid, agent_alias, workspace_dir, created_at, last_activity, killed_at)
                 VALUES ('a-live', 'alpha', '/ws/a1',
                         '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', NULL),
                        ('a-killed', 'alpha', '/ws/a2',
                         '2026-01-01T00:00:00Z', '2026-01-01T00:00:01Z', '2026-01-01T00:00:01Z'),
                        ('b-live', 'beta', '/ws/b1',
                         '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', NULL);",
            )
            .unwrap();

        // Rename re-points BOTH live and killed sessions; unlike delete, a live
        // session is no obstacle.
        assert_eq!(store.rename_sessions_by_agent("alpha", "gamma").unwrap(), 2);
        assert!(store.list_sessions_by_agent("alpha").unwrap().is_empty());
        assert_eq!(store.list_sessions_by_agent("gamma").unwrap().len(), 2);
        // the live session followed the rename
        assert_eq!(store.count_live_sessions_by_agent("gamma").unwrap(), 1);
        // beta untouched
        assert_eq!(store.list_sessions_by_agent("beta").unwrap().len(), 1);
        // unknown source → 0
        assert_eq!(store.rename_sessions_by_agent("ghost", "x").unwrap(), 0);
    }
}
