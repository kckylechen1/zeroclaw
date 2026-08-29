//! Boot wiring for the control-plane — minted once per daemon run.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zeroclaw_coordinator::CommandSender;

use super::reaper;
use super::task_registry::TaskRegistry;
use super::task_store_sqlite::SqliteTaskStore;

/// The live control-plane, shared (cheaply, via `Arc`/clone) across producers and
/// the reaper.
#[derive(Clone)]
pub struct ControlPlaneHandle {
    pub store: Arc<dyn TaskRegistry>,
    pub boot_id: String,
    /// The same store as `store`, kept in its concrete form.
    ///
    /// `store` is `Arc<dyn TaskRegistry>` — every existing producer (the
    /// coordinator persistence path; the retired `delegate.rs` and
    /// `spawn_subagent.rs` were the tool-level writers) reaches it that way,
    /// and changing
    /// that field's type would force those call sites to bring the
    /// `TaskRegistry` trait into scope themselves (dyn-trait method calls
    /// resolve without an import; calls on a concrete type do not). The
    /// coordinator actor's persistence port ([`super::subagent_persistence::SubagentPersistence`])
    /// needs the concrete [`SqliteTaskStore`] instead, for its `create_sync`/
    /// `finish_task_sync` entry points (`&mut self` sync calls a
    /// single-writer actor can make without an executor) — so this field
    /// carries a second `Arc` over the *same* allocation (unsized coercion
    /// of `Arc<T>` to `Arc<dyn Trait>` shares the allocation and its strong
    /// count; it does not clone the store) purely for that constructor.
    pub sqlite_store: Arc<SqliteTaskStore>,
    /// The live coordinator actor's command channel, if one was wired in.
    ///
    /// `None` when no coordinator was ever started against this handle: a
    /// plain [`Self::start`]/[`Self::start_with_boot_id`] call, as used by
    /// this module's own tests and by any host that only needs the durable
    /// task store, leaves this `None` rather than starting an actor nobody
    /// asked for. Set by whoever calls
    /// [`super::coordinator_host::start`] against this handle's
    /// `sqlite_store`/`boot_id` and attaches the resulting
    /// [`CommandSender`] here — see that function's doc for why it is not
    /// done inside `start_with_boot_id` itself.
    pub commands: Option<CommandSender>,
}

impl ControlPlaneHandle {
    pub async fn start(data_dir: &Path) -> Result<Self> {
        let run_id = uuid::Uuid::new_v4().to_string();
        Self::start_with_boot_id(data_dir, run_id).await
    }

    /// As [`Self::start`] but with a caller-supplied `boot_id` — lets `DaemonRegistry`
    /// reuse a process-stable run-id across reloads instead of a fresh UUID.
    ///
    /// Does not start a coordinator actor: `commands` comes back `None`.
    /// `Coordinator::with_persistence` needs a `Config` this constructor
    /// does not take (it only takes `data_dir`), and this function must stay
    /// callable from every existing caller — this module's own tests among
    /// them — that has no `Config` in hand. A caller that wants a live actor
    /// runs [`super::coordinator_host::start`] against the returned handle's
    /// `sqlite_store` and `boot_id`, strictly after this function returns —
    /// see that function's doc for why the ordering matters.
    pub async fn start_with_boot_id(data_dir: &Path, boot_id: String) -> Result<Self> {
        let sqlite_store = Arc::new(SqliteTaskStore::new(data_dir)?);
        // Method-call syntax, not `Arc::clone(&sqlite_store)`: the latter's
        // generic `T` gets inferred from this statement's expected type
        // (`Arc<dyn TaskRegistry>`) before the argument is checked against
        // it, which fails to unify with `&Arc<SqliteTaskStore>`.
        // `.clone()` resolves against the receiver's concrete type first, so
        // the unsizing coercion to `Arc<dyn TaskRegistry>` applies cleanly
        // at this `let` binding instead.
        let store: Arc<dyn TaskRegistry> = sqlite_store.clone();
        let reclaimed = reaper::recovery_pass(store.as_ref(), &boot_id).await?;
        if reclaimed > 0 {
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(
                        ::serde_json::json!({ "reclaimed": reclaimed, "boot_id": boot_id })
                    ),
                "control-plane: reclaimed prior-boot orphan tasks at startup"
            );
        }
        Ok(Self {
            store,
            boot_id,
            sqlite_store,
            commands: None,
        })
    }

    pub fn spawn_reaper(&self, max_runtime_secs: i64, cancel: CancellationToken) -> JoinHandle<()> {
        // Hoist owned clones to locals so the spawn! future captures them by value
        // (not `&self`, which the macro would otherwise hold across the 'static boundary).
        let store = Arc::clone(&self.store);
        let boot_id = self.boot_id.clone();
        zeroclaw_spawn::spawn!(reaper::reaper_loop(
            store,
            boot_id,
            max_runtime_secs,
            cancel
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn start_in_tempdir_and_reap_handle() {
        let dir = tempfile::tempdir().unwrap();
        let h = ControlPlaneHandle::start(dir.path()).await.unwrap();
        assert!(!h.boot_id.is_empty());
        // a reaper spawns and stops cleanly on cancel
        let cancel = CancellationToken::new();
        let jh = h.spawn_reaper(600, cancel.clone());
        cancel.cancel();
        jh.await.unwrap();
    }

    #[tokio::test]
    async fn boot_id_distinguishes_runs_over_the_same_db() {
        use crate::control_plane::task_registry::{TaskKind, TaskRecord, TaskStatus};
        let dir = tempfile::tempdir().unwrap();
        // First "boot" registers a running task, then the daemon "dies".
        let h1 = ControlPlaneHandle::start_with_boot_id(dir.path(), "boot-1".into())
            .await
            .unwrap();
        h1.store
            .create(TaskRecord {
                id: "t".into(),
                kind: TaskKind::Delegate,
                agent: "main".into(),
                status: TaskStatus::Running,
                owner_pid: 999_999,
                owner_boot_id: "boot-1".into(),
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
            })
            .await
            .unwrap();
        // Second boot recovers the orphan at startup.
        let h2 = ControlPlaneHandle::start_with_boot_id(dir.path(), "boot-2".into())
            .await
            .unwrap();
        assert_eq!(
            h2.store.get("t").await.unwrap().unwrap().status,
            TaskStatus::Lost
        );
    }
}
