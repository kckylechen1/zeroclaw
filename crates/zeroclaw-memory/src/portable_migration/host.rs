//! Admission of historical host records without granting new authority.

use super::{format::Cell, schema};
use crate::{kernel_store::KernelStoreKind, traits::MemoryCategory};
use anyhow::{Context, Result, bail, ensure};
use memcore::MemoryEntry;
use portable_kernel as memcore;
use rusqlite::Connection;
use serde::Deserialize;
use zeroclaw_api::companion::{AuthorityClass, CaptureOrigin, CaptureOutcome, SourcePartition};

fn metadata_string<'a>(entry: &'a MemoryEntry, name: &str) -> Result<&'a str> {
    entry
        .metadata
        .get(name)
        .and_then(serde_json::Value::as_str)
        .with_context(|| format!("host metadata lacks string {name}"))
}

#[derive(Deserialize)]
struct CaptureAttribution {
    outcome: CaptureOutcome,
    event_id: Option<String>,
    persisted_at: String,
    agent_identity_id: String,
    principal_id: String,
    session_id: String,
    turn_id: String,
    authority_class: AuthorityClass,
    origin: CaptureOrigin,
    partition: SourcePartition,
}

fn capture_attribution(entry: &MemoryEntry) -> Result<CaptureAttribution> {
    let capture: CaptureAttribution = serde_json::from_str(&entry.text)
        .context("companion body is not a recognized historical capture attribution")?;
    ensure!(
        matches!(
            capture.partition,
            SourcePartition::UserModel | SourcePartition::SharedLexicon
        ),
        "protected companion payload partition requires its owning migration"
    );
    ensure!(
        capture.partition.as_str() == metadata_string(entry, "partition")?
            && capture.outcome.as_str() == metadata_string(entry, "outcome")?
            && serde_json::to_value(capture.origin)? == entry.metadata["origin"]
            && capture.session_id == metadata_string(entry, "session_id")?
            && capture.turn_id == metadata_string(entry, "turn_id")?,
        "companion capture payload and host metadata attribution disagree"
    );
    ensure!(
        !capture.agent_identity_id.trim().is_empty()
            && !capture.principal_id.trim().is_empty()
            && !capture.session_id.trim().is_empty()
            && !capture.turn_id.trim().is_empty()
            && capture
                .event_id
                .as_ref()
                .is_some_and(|id| !id.trim().is_empty()),
        "companion capture attribution is incomplete"
    );
    chrono::DateTime::parse_from_rfc3339(&capture.persisted_at)
        .context("companion capture persisted_at is not a recognized timestamp")?;
    Ok(capture)
}

pub(super) fn verify_entry(entry: &MemoryEntry, kind: KernelStoreKind) -> Result<()> {
    ensure!(
        !entry.id.trim().is_empty()
            && !entry.id.starts_with("anchor:")
            && !entry.id.starts_with("wiki-rem:")
            && entry.id != "wiki-operation-log"
            && !entry.topic.eq_ignore_ascii_case("wiki_log"),
        "reserved kernel memory identity cannot be adopted by this migration"
    );
    match kind {
        KernelStoreKind::Memory => {
            ensure!(
                entry.source == "external:zeroclaw",
                "memory source is not the legacy ZeroClaw adapter"
            );
            let key = metadata_string(entry, "zeroclaw_key")?;
            let agent = metadata_string(entry, "zeroclaw_agent")?;
            let namespace = metadata_string(entry, "zeroclaw_namespace")?;
            let category = metadata_string(entry, "zeroclaw_category")?;
            ensure!(
                !schema::protected_namespace(namespace)
                    && !key.starts_with(crate::SOUL_KEY_PREFIX)
                    && !key.starts_with("user_model::"),
                "Soul and User Model authority records require their owning migration"
            );
            let category = match (entry.category.as_str(), category) {
                ("fact", "core") => MemoryCategory::Core,
                ("experience", "daily") => MemoryCategory::Daily,
                ("experience", "conversation") => MemoryCategory::Conversation,
                ("other", name) => MemoryCategory::Custom(name.to_owned()),
                _ => bail!("host category metadata and stored category disagree"),
            };
            let path = crate::tachi::TachiMemory::storage_path(
                Some(agent),
                Some(namespace),
                &category,
                key,
            );
            ensure!(
                entry.path == memcore::path_router::normalize_path(&path),
                "host metadata does not own the stored memory path"
            );
            let scope = if namespace == "default" {
                "general"
            } else if namespace == "user" || namespace.starts_with("user:") {
                "user"
            } else {
                "project"
            };
            ensure!(
                entry.scope == scope,
                "host namespace and stored scope disagree"
            );
        }
        KernelStoreKind::Companion => {
            ensure!(
                entry.source == "external:zeroclaw-companion-capture",
                "companion source is not a historical ZeroClaw capture"
            );
            let partition = metadata_string(entry, "partition")?;
            let class = metadata_string(entry, "object_class")?;
            ensure!(
                matches!(partition, "user_model" | "shared_lexicon"),
                "protected or unknown companion partition"
            );
            ensure!(
                !class.is_empty()
                    && class.len() <= 64
                    && class.bytes().all(|byte| byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'_' | b'.' | b'-'))
                    && !class.contains("head")
                    && !class.contains("revision")
                    && !class.contains("authority")
                    && !class.contains("soul"),
                "protected or unsupported companion authority object"
            );
            let prefix = format!("/companion/{partition}/{class}/");
            ensure!(
                entry.path.starts_with(&prefix)
                    && entry.path.len() > prefix.len()
                    && entry.path == memcore::path_router::normalize_path(&entry.path),
                "companion metadata does not own the stored memory path"
            );
            ensure!(
                entry.scope == "general" && entry.domain.as_deref() == Some("companion"),
                "companion scope or domain is foreign"
            );
            capture_attribution(entry)?;
        }
    }
    Ok(())
}

pub(super) fn verify_ledger_ownership(conn: &Connection, kind: KernelStoreKind) -> Result<()> {
    let destination_receipts: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM memory_outbox_destination_apply_receipts)",
        [],
        |row| row.get(0),
    )?;
    ensure!(
        !destination_receipts,
        "destination-apply authority history has no legacy ZeroClaw producer and requires its owning migration"
    );
    if kind == KernelStoreKind::Memory {
        let events: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM memory_outbox_events)",
            [],
            |row| row.get(0),
        )?;
        ensure!(
            !events,
            "generic ZeroClaw memory has no legacy outbox producer; foreign queue history cannot be adopted"
        );
    } else {
        let orphan: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM memory_outbox_events e LEFT JOIN memories m ON m.id=e.object_id WHERE m.id IS NULL)",
            [], |row| row.get(0))?;
        ensure!(
            !orphan,
            "companion outbox contains an object with no attributable memory record"
        );
    }
    Ok(())
}

pub(super) fn verify_events(conn: &Connection, entry: &MemoryEntry) -> Result<()> {
    let capture = capture_attribution(entry)?;
    let mut statement = conn.prepare("SELECT source_store,source_partition,object_class,authority_class,source_revision,payload_digest FROM memory_outbox_events WHERE object_id=?1")?;
    let mut rows = statement.query([&entry.id])?;
    while let Some(row) = rows.next()? {
        let source_store = Cell::read(row.get_ref(0)?)?;
        let partition = Cell::read(row.get_ref(1)?)?;
        let class = Cell::read(row.get_ref(2)?)?;
        let authority = Cell::read(row.get_ref(3)?)?;
        let revision = Cell::read(row.get_ref(4)?)?.integer()?;
        let digest = Cell::read(row.get_ref(5)?)?;
        let authority: AuthorityClass =
            serde_json::from_value(serde_json::Value::String(authority.text()?.to_owned()))
                .context("companion outbox authority is not a host authority class")?;
        ensure!(
            source_store.text()? == "companion"
                && partition.text()? == capture.partition.as_str()
                && class.text()? == metadata_string(entry, "object_class")?
                && authority == capture.authority_class,
            "companion outbox and capture ownership or authority disagree"
        );
        ensure!(
            revision > 0 && revision <= entry.revision,
            "companion outbox has a zero, negative, or future source revision"
        );
        let digest = digest.text()?;
        ensure!(
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')),
            "companion outbox payload digest is malformed"
        );
        if revision == entry.revision {
            ensure!(
                digest == memcore::outbox_payload_digest(entry)?,
                "current companion outbox digest disagrees with the stored memory payload"
            );
        }
        // Older revisions retain their historical digest. LocalWins may mint
        // event_id::local-wins without changing the capture's body event_id;
        // those protocol identities are intentionally not conflated here.
    }
    Ok(())
}
