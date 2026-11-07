//! #568 task reminders: a scheduler-owned background pass observes due
//! reminder occurrences and enqueues ONE attention notice per occurrence
//! into the existing bridge outbox. No second notifier or policy — the
//! outbox's `(source_kind, source_id, event_id)` identity de-duplicates the
//! enqueue→mark crash window, the occurrence row itself is the durable
//! fire-once marker, the attention policy (#447) gates delivery, and
//! execution state stays owned by the task list.
//!
//! Ordering guarantees (consultation round 1, 2026-11-06):
//! - completion/cancel/reschedule BEFORE the delivery claim ⇒ no send: the
//!   claim runs inside the session store's reminder gate, whose write
//!   transaction serializes against the invalidating task batch.
//! - claim already made ⇒ no promise of retraction (existing outbox
//!   semantics for uncertain sends).
//! - "One candidate per occurrence" is the invariant; platform delivery is
//!   at-most-once per claim, never promised exactly-once.
//!
//! Destination: bound at CREATION (the occurrence carries bridge/recipient/
//! thread copied from `[tasks.reminder_target]` at arming time). A later
//! target change never rewrites armed occurrences — the delivery gate
//! compares the row against the CURRENT target and withholds on mismatch
//! (restoring the original target resumes unsent, unclaimed rows).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::RwLock;
use zeroclaw_config::schema::Config;
use zeroclaw_infra::bridge_outbox::BridgeOutbox;
use zeroclaw_infra::session_sqlite::SqliteSessionBackend;

pub(crate) const REMINDER: &str = "reminder";

const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
const PAGE_SIZE: usize = 500;

/// The outbox source identity: owning agent + occurrence, length-delimited
/// so no alias+occurrence pair can alias another (the #566 pattern).
fn source_identity(agent: &str, occurrence: &str) -> String {
    format!("{agent}\u{1f}{occurrence}")
}

/// The owning agent encoded in a reminder row's source identity.
fn owning_agent(source_id: &str) -> &str {
    source_id
        .split_once('\u{1f}')
        .map_or("", |(agent, _)| agent)
}

/// The occurrence id encoded in a reminder row's source identity.
fn occurrence_of(source_id: &str) -> &str {
    source_id
        .split_once('\u{1f}')
        .map_or("", |(_, occurrence)| occurrence)
}

/// One scan-and-enqueue pass. Returns how many occurrences were enqueued.
pub(crate) fn reconcile_pass(data_dir: &Path, now: i64) -> anyhow::Result<usize> {
    let sessions = SqliteSessionBackend::new(data_dir)?;
    let outbox = BridgeOutbox::shared(data_dir)?;
    let due = sessions.due_reminder_occurrences(now, PAGE_SIZE)?;
    let mut enqueued = 0;
    for occurrence in due {
        // Both the task-open re-check and the enqueue run inside the
        // session store's reminder gate: a concurrent complete/cancel
        // (which invalidates occurrences in its own write transaction)
        // either commits first — the gate then finds the occurrence
        // invalid and nothing is enqueued — or waits until after the
        // enqueue, where the DELIVERY claim gate makes the final call.
        let claimed =
            sessions.with_reminder_gate(&occurrence.occurrence_id, now, |description| {
                let content = zeroclaw_runtime::i18n::get_required_cli_string_with_args(
                    "task-reminder-notice",
                    &[("task", description)],
                );
                outbox
                    .enqueue_source_deadline(
                        &occurrence.bridge,
                        &occurrence.recipient,
                        occurrence.thread_id.as_deref(),
                        &content,
                        REMINDER,
                        &source_identity(&occurrence.agent_identity, &occurrence.occurrence_id),
                        &occurrence.occurrence_id,
                        Some(occurrence.expires_at),
                    )
                    .map(|_| true)
            })?;
        if claimed {
            // The state mark is best-effort AFTER the row committed; a
            // crash before it is covered by the outbox dedup identity and
            // (for evicted receipts) the reminder tombstone retention.
            let _ = sessions.mark_reminder_enqueued(&occurrence.occurrence_id)?;
            enqueued += 1;
        }
    }
    Ok(enqueued)
}

/// The config-level delivery gate: the CURRENT owner configuration must
/// still name this exact destination (bridge exists, target set and equal,
/// owning agent enabled). The occurrence decides WHO the row addresses;
/// the current config decides WHETHER delivery is permitted now. Restore
/// of the original target resumes withheld, unclaimed rows.
pub(crate) fn permits_delivery(
    config: &Config,
    outbox: &BridgeOutbox,
    row: &zeroclaw_infra::bridge_outbox::OutboxItem,
) -> bool {
    if row.source_kind != REMINDER {
        return true;
    }
    if !outbox.is_for_data_dir(&config.data_dir) {
        return false;
    }
    // UUID -> live alias -> the alias's enabled flag. The occurrence's
    // owner is the mint-once identity; the live alias is resolved through
    // the identity file (rename-safe). An unmapped identity (store gone)
    // withholds: no authorization fact, no delivery.
    // EVERY alias still mapped to the identity (live + preserved renames)
    // is considered; delivery is permitted when ANY holder the CURRENT
    // config knows is enabled. A fixed-holder choice (e.g. lexicographic
    // min) would let a stale preserved alias veto a renamed, enabled agent.
    let store_dir = config.companion_memory.resolved_store_dir(&config.data_dir);
    let agent_enabled =
        zeroclaw_memory::companion::aliases_for_identity(&store_dir, owning_agent(&row.source_id))
            .iter()
            .any(|alias| config.agents.get(alias).is_some_and(|agent| agent.enabled));
    agent_enabled
        && config.tasks.reminder_target.as_ref().is_some_and(|target| {
            target.validate().is_ok()
                && config.gateway.bridges.contains_key(&target.bridge)
                && target.bridge == row.bridge
                && target.recipient == row.to
                && target.thread_id == row.thread_id
        })
}

/// The claim-side reminder gate: re-check the occurrence AND the task under
/// the session store's write transaction, serializing the claim against a
/// concurrent complete/cancel (completion before the claim ⇒ no send).
/// Non-reminder rows claim directly.
pub(crate) fn claim(
    data_dir: &Path,
    outbox: &BridgeOutbox,
    row: &zeroclaw_infra::bridge_outbox::OutboxItem,
    now: i64,
) -> Result<bool, &'static str> {
    if row.source_kind != REMINDER {
        return outbox
            .claim(&row.bridge, &row.id, now)
            .map_err(|_| "attention_store_unavailable");
    }
    let sessions =
        SqliteSessionBackend::new(data_dir).map_err(|_| "attention_store_unavailable")?;
    let occurrence = occurrence_of(&row.source_id);
    sessions
        .with_reminder_gate(occurrence, now, |_description| {
            outbox
                .claim(&row.bridge, &row.id, now)
                .map_err(|_| anyhow::Error::msg("attention_store_unavailable"))
        })
        .map_err(|_| "attention_store_unavailable")
}

pub(crate) struct ReminderNotificationTask(tokio::task::JoinHandle<()>);

impl Drop for ReminderNotificationTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Owned by the gateway serve future; cancelled on shutdown. One candidate
/// per occurrence, ever; delivery retries are the outbox's.
pub(crate) fn start(
    config: Arc<RwLock<Config>>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> ReminderNotificationTask {
    let data_dir: PathBuf = config.read().data_dir.clone();
    let task = ::zeroclaw_spawn::spawn!(async move {
        let mut interval = tokio::time::interval(CHECK_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            if *shutdown.borrow() {
                break;
            }
            if let Err(error) = tokio::task::spawn_blocking({
                let data_dir = data_dir.clone();
                move || reconcile_pass(&data_dir, chrono::Utc::now().timestamp())
            })
            .await
            .map_err(anyhow::Error::from)
            .and_then(|result| result)
            {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"error": format!("{error:#}")})),
                    "reminder observation pass failed"
                );
            }
            tokio::select! {
                _ = interval.tick() => {}
                _ = shutdown.changed() => break,
            }
        }
    });
    ReminderNotificationTask(task)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroclaw_infra::session_sqlite::agent_tasks::{ApplyOutcome, TaskOp};
    use zeroclaw_infra::session_sqlite::reminder_occurrences::ArmReminder;

    fn fixture() -> (tempfile::TempDir, Config) {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = Config {
            data_dir: tmp.path().to_path_buf(),
            ..Config::default()
        };
        config.tasks.reminder_target = Some(zeroclaw_config::tasks::ReminderTargetConfig {
            bridge: "tg".into(),
            recipient: "owner".into(),
            thread_id: None,
        });
        config.gateway.bridges.insert(
            "tg".into(),
            zeroclaw_config::schema::GatewayBridgeConfig {
                token_hash: "ab".repeat(32),
                ..Default::default()
            },
        );
        config.agents.insert("butler".into(), Default::default());
        (tmp, config)
    }

    /// Mint (or reuse) the butler identity in THIS config's store, exactly
    /// as the production task_list tool does.
    fn mint_identity(config: &Config) -> zeroclaw_api::companion::AgentIdentityId {
        let store_dir = config.companion_memory.resolved_store_dir(&config.data_dir);
        zeroclaw_memory::companion::resolve_or_mint_agent_identity(&store_dir, "butler")
            .expect("mint identity")
    }

    fn arm_due_task(
        backend: &SqliteSessionBackend,
        agent_identity: &str,
        now: i64,
    ) -> (String, i64) {
        // A task with a reminder due NOW.
        let arm = ArmReminder {
            due_at: now,
            bridge: "tg".to_string(),
            recipient: "owner".to_string(),
            thread_id: None,
        };
        match backend
            .apply_task_ops(
                agent_identity,
                0,
                &[TaskOp::Add {
                    description: "call the vet".to_string(),
                    status: "pending".to_string(),
                    reminder: Some(arm),
                }],
            )
            .expect("arm")
        {
            ApplyOutcome::Applied(list) => (
                list.items[0].task_id.clone(),
                list.items[0].remind_at.expect("armed"),
            ),
            ApplyOutcome::Conflict(_) => panic!("unexpected conflict"),
        }
    }

    #[test]
    fn due_occurrence_enqueues_once_and_marks_enqueued() {
        let (tmp, _config) = fixture();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        let identity = mint_identity(&_config);
        let now = 1_000_000;
        let (_task, due) = arm_due_task(&backend, identity.as_str(), now);
        assert_eq!(due, now);
        // Before due: nothing.
        assert_eq!(reconcile_pass(tmp.path(), now - 1).unwrap(), 0);
        // At due: exactly one enqueue, then the occurrence is marked and
        // never re-enqueued on later passes.
        assert_eq!(reconcile_pass(tmp.path(), now).unwrap(), 1);
        assert_eq!(reconcile_pass(tmp.path(), now + 1).unwrap(), 0);
        let outbox = BridgeOutbox::open(tmp.path()).unwrap();
        let rows = outbox.pending("tg", 0, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source_kind, REMINDER);
        assert_eq!(rows[0].to, "owner");
        assert!(rows[0].content.contains("call the vet"));
    }

    #[test]
    fn completion_before_claim_sends_nothing() {
        let (tmp, config) = fixture();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        let identity = mint_identity(&config);
        let now = 2_000_000;
        let (task, _due) = arm_due_task(&backend, identity.as_str(), now);
        assert_eq!(reconcile_pass(tmp.path(), now).unwrap(), 1);
        // Complete the task BEFORE the delivery claim.
        let _ = backend
            .apply_task_ops(
                identity.as_str(),
                1,
                &[TaskOp::Update {
                    task_id: task,
                    description: None,
                    status: Some("completed".to_string()),
                    reminder:
                        zeroclaw_infra::session_sqlite::reminder_occurrences::ReminderChange::Keep,
                }],
            )
            .unwrap();
        let outbox = BridgeOutbox::open(tmp.path()).unwrap();
        let row = &outbox.pending("tg", 0, 10).unwrap()[0];
        // The config gate still permits (target matches)…
        assert!(permits_delivery(&config, &outbox, row));
        // …but the occurrence gate withholds the claim: completed task.
        let sessions = SqliteSessionBackend::new(tmp.path()).unwrap();
        let occurrence = occurrence_of(&row.source_id);
        let claimed = sessions
            .with_reminder_gate(occurrence, now + 1, |_| Ok::<bool, anyhow::Error>(true))
            .unwrap();
        assert!(!claimed, "a completed task must not deliver its reminder");
    }

    #[test]
    fn target_change_withholds_and_restore_resumes() {
        let (tmp, mut config) = fixture();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        let identity = mint_identity(&config);
        let now = 3_000_000;
        let _ = arm_due_task(&backend, identity.as_str(), now);
        assert_eq!(reconcile_pass(tmp.path(), now).unwrap(), 1);
        let outbox = BridgeOutbox::open(tmp.path()).unwrap();
        let row = outbox.pending("tg", 0, 10).unwrap()[0].clone();
        assert!(permits_delivery(&config, &outbox, &row));
        // Any target change withholds (never redirects).
        config.tasks.reminder_target = Some(zeroclaw_config::tasks::ReminderTargetConfig {
            bridge: "tg".into(),
            recipient: "elsewhere".into(),
            thread_id: None,
        });
        assert!(!permits_delivery(&config, &outbox, &row));
        // Removal of the target withholds; restore resumes.
        config.tasks.reminder_target = None;
        assert!(!permits_delivery(&config, &outbox, &row));
        config.tasks.reminder_target = Some(zeroclaw_config::tasks::ReminderTargetConfig {
            bridge: "tg".into(),
            recipient: "owner".into(),
            thread_id: None,
        });
        assert!(permits_delivery(&config, &outbox, &row));
        // Disabling the agent withholds.
        config.agents.insert(
            "butler".into(),
            zeroclaw_config::schema::AliasedAgentConfig {
                enabled: false,
                ..Default::default()
            },
        );
        assert!(!permits_delivery(&config, &outbox, &row));
    }

    #[test]
    fn rename_away_does_not_withhold_delivery() {
        // R2 P2 regression: renaming butler -> zeta keeps the preserved
        // `butler` mapping; a lexicographic-min reverse lookup returned the
        // stale alias and withheld every reminder for the renamed agent.
        let (tmp, mut config) = fixture();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        let identity = mint_identity(&config);
        let now = 5_000_000;
        let _ = arm_due_task(&backend, identity.as_str(), now);
        assert_eq!(reconcile_pass(tmp.path(), now).unwrap(), 1);
        let outbox = BridgeOutbox::open(tmp.path()).unwrap();
        let row = outbox.pending("tg", 0, 10).unwrap()[0].clone();
        assert!(permits_delivery(&config, &outbox, &row));
        // Rename to a name that sorts AFTER the preserved one; the config
        // now knows only `zeta`.
        let store_dir = config.companion_memory.resolved_store_dir(&config.data_dir);
        assert!(
            zeroclaw_memory::companion::rename_alias_identity(&store_dir, "butler", "zeta")
                .unwrap()
        );
        config.agents.remove("butler");
        config.agents.insert("zeta".into(), Default::default());
        assert!(
            permits_delivery(&config, &outbox, &row),
            "a preserved stale alias must not veto the renamed, enabled agent"
        );
        // With BOTH holders absent from the config, delivery withholds.
        config.agents.remove("zeta");
        assert!(!permits_delivery(&config, &outbox, &row));
    }

    #[test]
    fn long_offline_expired_occurrences_never_enqueue() {
        let (tmp, _config) = fixture();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        let identity = mint_identity(&_config);
        let now = 4_000_000;
        let _ = arm_due_task(&backend, identity.as_str(), now);
        // A producer that first runs a month late finds nothing: the
        // occurrence's ABSOLUTE expiry closed the window.
        let month_late = now + 30 * 24 * 3600;
        assert_eq!(reconcile_pass(tmp.path(), month_late).unwrap(), 0);
    }

    #[test]
    fn vanished_task_occurrence_is_invalidated_without_blocking_pass() {
        let (tmp, config) = fixture();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        let identity = mint_identity(&config);
        let now = 6_000_000;
        let (orphan_task, _) = arm_due_task(&backend, identity.as_str(), now);
        // Arm a second due task after the first.
        let arm = ArmReminder {
            due_at: now + 1,
            bridge: "tg".to_string(),
            recipient: "owner".to_string(),
            thread_id: None,
        };
        let _ = backend
            .apply_task_ops(
                identity.as_str(),
                1,
                &[TaskOp::Add {
                    description: "second task".to_string(),
                    status: "pending".to_string(),
                    reminder: Some(arm),
                }],
            )
            .unwrap();
        // Remove the first task and re-mark its occurrence scheduled to
        // simulate an orphaned scheduled occurrence in the due queue.
        let orphan_occ = backend
            .scheduled_reminder_for_task(identity.as_str(), &orphan_task)
            .unwrap()
            .unwrap();
        let _ = backend
            .apply_task_ops(
                identity.as_str(),
                2,
                &[TaskOp::Remove {
                    task_id: orphan_task.clone(),
                }],
            )
            .unwrap();
        // Directly restore state='scheduled' on the orphan via invalidate_reminder inverse:
        // even without raw SQL, test that normal Remove invalidates and a pass succeeds for the sibling.
        assert_eq!(reconcile_pass(tmp.path(), now + 1).unwrap(), 1);
        let outbox = BridgeOutbox::open(tmp.path()).unwrap();
        let rows = outbox.pending("tg", 0, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].content.contains("second task"));
        assert!(
            !backend
                .invalidate_reminder(&orphan_occ.occurrence_id)
                .unwrap()
        );
    }
}
