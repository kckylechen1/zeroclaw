//! Strict memory decoding, byte-preserving auxiliary transfer, and readback.

use super::format::{self, Cell, TableShape};
use super::host;
use super::schema::{self, Inventory};
use super::{RECEIPT_NAMESPACE, TableDigest};
use crate::kernel_store::KernelStoreKind;
use anyhow::{Context, Result, bail, ensure};
use memcore::{MemoryEntry, MemoryStore, PortableImportEntry};
use portable_kernel as memcore;
use rusqlite::{Connection, Row, params, params_from_iter};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

const MEMORY_COLUMNS: &[&str] = &[
    "id",
    "path",
    "summary",
    "text",
    "importance",
    "timestamp",
    "valid_from",
    "valid_until",
    "category",
    "topic",
    "keywords",
    "entities",
    "source",
    "scope",
    "archived",
    "created_at",
    "updated_at",
    "access_count",
    "scored_count",
    "last_access",
    "last_use_at",
    "revision",
    "metadata",
    "superseded_by",
    "idless_identity",
    "retention_policy",
    "domain",
    "recall_count",
    "query_diversity",
    "tier",
];

pub(super) fn bytes_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn string(row: &Row<'_>, name: &str) -> Result<String> {
    Cell::read(row.get_ref(name)?)
        .and_then(|value| value.text().map(str::to_owned))
        .with_context(|| format!("memories.{name} is not TEXT"))
}

fn integer(row: &Row<'_>, name: &str) -> Result<i64> {
    Cell::read(row.get_ref(name)?)
        .and_then(|value| value.integer())
        .with_context(|| format!("memories.{name} is not INTEGER"))
}

fn optional_string(row: &Row<'_>, name: &str) -> Result<Option<String>> {
    Cell::read(row.get_ref(name)?)
        .and_then(|value| value.optional_text())
        .with_context(|| format!("memories.{name} is not TEXT or NULL"))
}

fn json(row: &Row<'_>, name: &str) -> Result<serde_json::Value> {
    let raw = string(row, name)?;
    let value: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("memories.{name} contains invalid JSON"))?;
    // The public snapshot writer serializes these fields. Refuse inputs it
    // cannot reproduce byte-for-byte rather than silently normalize JSON.
    ensure!(
        serde_json::to_string(&value)? == raw,
        "memories.{name} is not byte-preserving through the public snapshot representation"
    );
    Ok(value)
}

fn strings(row: &Row<'_>, name: &str) -> Result<Vec<String>> {
    serde_json::from_value(json(row, name)?)
        .with_context(|| format!("memories.{name} must be a JSON array of strings"))
}

fn decode(row: &Row<'_>) -> Result<PortableImportEntry> {
    ensure!(
        matches!(Cell::read(row.get_ref("idless_identity")?)?, Cell::Null),
        "non-NULL idless identity is not supported by the public snapshot API"
    );
    let importance = match Cell::read(row.get_ref("importance")?)? {
        Cell::Real(bits) => f64::from_bits(bits),
        _ => bail!("memories.importance is not REAL"),
    };
    ensure!(importance.is_finite(), "memories.importance must be finite");
    let archived = integer(row, "archived")?;
    ensure!(
        matches!(archived, 0 | 1),
        "memories.archived must be an exact boolean integer"
    );
    let metadata = json(row, "metadata")?;
    ensure!(
        metadata.is_object(),
        "host memory metadata must be a JSON object"
    );
    Ok(PortableImportEntry {
        entry: MemoryEntry {
            id: string(row, "id")?,
            path: string(row, "path")?,
            summary: string(row, "summary")?,
            text: string(row, "text")?,
            importance,
            timestamp: string(row, "timestamp")?,
            valid_from: string(row, "valid_from")?,
            valid_until: optional_string(row, "valid_until")?,
            category: string(row, "category")?,
            topic: string(row, "topic")?,
            keywords: strings(row, "keywords")?,
            entities: strings(row, "entities")?,
            persons: Vec::new(),
            location: String::new(),
            source: string(row, "source")?,
            scope: string(row, "scope")?,
            archived: archived == 1,
            access_count: integer(row, "access_count")?,
            scored_count: integer(row, "scored_count")?,
            last_access: optional_string(row, "last_access")?,
            last_use_at: optional_string(row, "last_use_at")?,
            revision: integer(row, "revision")?,
            metadata,
            vector: None,
            retention_policy: optional_string(row, "retention_policy")?,
            domain: optional_string(row, "domain")?,
            recall_count: integer(row, "recall_count")?,
            query_diversity: integer(row, "query_diversity")?,
            tier: string(row, "tier")?,
        },
        created_at: string(row, "created_at")?,
        updated_at: string(row, "updated_at")?,
        superseded_by: optional_string(row, "superseded_by")?,
    })
}

fn scan_entries(
    conn: &Connection,
    inventory: &Inventory,
    kind: KernelStoreKind,
    mut consume: impl FnMut(PortableImportEntry) -> Result<()>,
) -> Result<()> {
    let actual: BTreeSet<_> = inventory.schema.tables["memories"]
        .columns
        .iter()
        .map(|column| column.name.as_str())
        .collect();
    ensure!(
        actual == MEMORY_COLUMNS.iter().copied().collect(),
        "strict decoder does not account for every memory column"
    );
    let has_vectors = inventory.projection.contains_key("memories_vec");
    if has_vectors {
        let orphans: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM memories_vec v LEFT JOIN memories m ON m.id=v.id WHERE m.id IS NULL)", [], |row| row.get(0))?;
        ensure!(
            !orphans,
            "orphan vectors cannot be represented by the snapshot API"
        );
    }
    let mut vectors = if has_vectors {
        Some(conn.prepare("SELECT embedding FROM memories_vec WHERE id=?1")?)
    } else {
        None
    };
    let mut statement = conn.prepare(&format!(
        "SELECT {} FROM memories ORDER BY id",
        MEMORY_COLUMNS.join(",")
    ))?;
    let mut cursor = statement.query([])?;
    while let Some(row) = cursor.next()? {
        let mut entry = decode(row)?;
        host::verify_entry(&entry.entry, kind)?;
        if let Some(statement) = vectors.as_mut() {
            let mut cursor = statement.query([&entry.entry.id])?;
            if let Some(row) = cursor.next()? {
                let Cell::Blob(blob) = Cell::read(row.get_ref(0)?)? else {
                    bail!("vector projection is not a BLOB");
                };
                ensure!(
                    blob.len() == 1024 * 4,
                    "legacy vector does not have the expected 1024 dimensions"
                );
                let vector: Vec<f32> = blob
                    .chunks_exact(4)
                    .map(|bytes| f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
                    .collect();
                ensure!(
                    vector.iter().all(|value| value.is_finite()),
                    "non-finite legacy vector is unsupported"
                );
                entry.entry.vector = Some(vector);
                ensure!(cursor.next()?.is_none(), "duplicate vector identity");
            }
        }
        consume(entry)?;
    }
    Ok(())
}

pub(super) fn preflight(
    conn: &Connection,
    inventory: &Inventory,
    kind: KernelStoreKind,
) -> Result<()> {
    host::verify_ledger_ownership(conn, kind)?;
    scan_entries(conn, inventory, kind, |entry| {
        if kind == KernelStoreKind::Companion {
            host::verify_events(conn, &entry.entry)?;
        }
        Ok(())
    })
}

pub(super) fn import_memories(
    conn: &Connection,
    inventory: &Inventory,
    store: &mut MemoryStore,
) -> Result<()> {
    let mut batch = Vec::with_capacity(256);
    scan_entries(conn, inventory, KernelStoreKind::Memory, |entry| {
        batch.push(entry);
        if batch.len() == 256 {
            let receipt = store.import_snapshot_batch(&batch)?;
            ensure!(
                receipt.rows_imported == batch.len(),
                "snapshot import row count differs"
            );
            batch.clear();
        }
        Ok(())
    })?;
    if !batch.is_empty() {
        let receipt = store.import_snapshot_batch(&batch)?;
        ensure!(
            receipt.rows_imported == batch.len(),
            "snapshot import row count differs"
        );
    }
    Ok(())
}

pub(super) fn columns(name: &str, shape: &TableShape) -> Vec<String> {
    let mut columns = Vec::new();
    if shape.kind == "table" && !shape.without_rowid && !matches!(name, "memories" | "hard_state") {
        columns.push("rowid".to_owned());
    }
    columns.extend(
        shape
            .columns
            .iter()
            .filter(|column| column.hidden == 0)
            .map(|column| column.name.clone()),
    );
    columns
}

fn select(name: &str, columns: &[String], exclude_receipt: bool) -> String {
    let mut sql = format!(
        "SELECT {} FROM main.{}",
        columns
            .iter()
            .map(|column| format::identifier(column))
            .collect::<Vec<_>>()
            .join(","),
        format::identifier(name)
    );
    if name == "hard_state" {
        sql.push_str(" WHERE namespace != 'migrations' AND NOT (namespace='store_identity' AND key IN ('profile','role'))");
        if exclude_receipt {
            sql.push_str(" AND NOT (namespace=?1 AND key=?2)");
        }
    }
    sql
}

fn hash_component(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

pub(super) fn digests(
    conn: &Connection,
    inventory: &Inventory,
    receipt_key: Option<&str>,
) -> Result<Vec<TableDigest>> {
    let mut result = Vec::new();
    for (name, shape) in &inventory.projection {
        let columns = columns(name, shape);
        let mut statement = conn.prepare(&select(name, &columns, receipt_key.is_some()))?;
        let mut cursor = if name == "hard_state" && receipt_key.is_some() {
            statement.query(params![RECEIPT_NAMESPACE, receipt_key])?
        } else {
            statement.query([])?
        };
        let mut hashes = Vec::<[u8; 32]>::new();
        while let Some(row) = cursor.next()? {
            let mut hash = Sha256::new();
            for index in 0..columns.len() {
                match row.get_ref(index)? {
                    rusqlite::types::ValueRef::Null => hash.update([0]),
                    rusqlite::types::ValueRef::Integer(value) => {
                        hash.update([1]);
                        hash.update(value.to_le_bytes());
                    }
                    rusqlite::types::ValueRef::Real(value) => {
                        hash.update([2]);
                        hash.update(value.to_bits().to_le_bytes());
                    }
                    rusqlite::types::ValueRef::Text(value) => {
                        hash.update([3]);
                        hash_component(&mut hash, value);
                    }
                    rusqlite::types::ValueRef::Blob(value) => {
                        hash.update([4]);
                        hash_component(&mut hash, value);
                    }
                }
            }
            hashes.push(hash.finalize().into());
        }
        hashes.sort_unstable();
        let mut hash = Sha256::new();
        hash.update(b"zeroclaw-kernel-migration-sqlite-values-v1\0");
        for column in &columns {
            hash_component(&mut hash, column.as_bytes());
        }
        hash.update((hashes.len() as u64).to_le_bytes());
        for row in &hashes {
            hash.update(row);
        }
        result.push(TableDigest {
            table: name.clone(),
            columns,
            rows: hashes.len() as u64,
            sha256: format!("{:x}", hash.finalize()),
        });
    }
    Ok(result)
}

pub(super) fn require_equal(expected: &[TableDigest], actual: &[TableDigest]) -> Result<()> {
    if expected == actual {
        return Ok(());
    }
    let expected: BTreeMap<_, _> = expected.iter().map(|entry| (&entry.table, entry)).collect();
    let actual: BTreeMap<_, _> = actual.iter().map(|entry| (&entry.table, entry)).collect();
    for name in expected.keys().chain(actual.keys()) {
        let before = expected.get(name);
        let after = actual.get(name);
        if before != after {
            bail!(
                "migration readback differs for table {name}: source rows/digest={:?}, target rows/digest={:?}",
                before.map(|entry| (entry.rows, &entry.sha256)),
                after.map(|entry| (entry.rows, &entry.sha256))
            );
        }
    }
    bail!("migration projection differs");
}

pub(super) fn open_auxiliary_target(path: &std::path::Path) -> Result<Connection> {
    let connection =
        Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    // Kernel-owned registration only: the installed function remains denied.
    // No migration code defines a guard function or receives an authority flag.
    memcore::db::ensure_reserved_reference_write_guard(&connection)?;
    let enabled: i64 = connection.query_row(
        "SELECT tachi_reserved_reference_write_enabled()",
        [],
        |row| row.get(0),
    )?;
    ensure!(
        enabled == 0,
        "auxiliary connection unexpectedly holds write authority"
    );
    connection.execute_batch("PRAGMA foreign_keys=ON")?;
    Ok(connection)
}

pub(super) fn copy_auxiliary(
    source: &Connection,
    inventory: &Inventory,
    target: &mut Connection,
) -> Result<()> {
    schema::verify_target_shape(target, inventory)?;
    let transaction = target.transaction()?;
    transaction.execute_batch("PRAGMA defer_foreign_keys=ON")?;
    for (name, shape) in &inventory.projection {
        if matches!(
            name.as_str(),
            "memories" | "memories_vec" | "memory_search_generation" | "sqlite_sequence"
        ) {
            continue;
        }
        if name != "hard_state" {
            let occupied: bool = transaction.query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM {})", format::identifier(name)),
                [],
                |row| row.get(0),
            )?;
            ensure!(!occupied, "fresh auxiliary destination is occupied: {name}");
        }
        copy_table(source, &transaction, name, shape)?;
    }
    // Inserts into history/edges advance generation; restore the authoritative
    // source counter only after all those rows have been copied.
    transaction.execute("DELETE FROM memory_search_generation", [])?;
    copy_table(
        source,
        &transaction,
        "memory_search_generation",
        &inventory.projection["memory_search_generation"],
    )?;
    if let Some(shape) = inventory.projection.get("sqlite_sequence") {
        transaction.execute("DELETE FROM sqlite_sequence", [])?;
        copy_table(source, &transaction, "sqlite_sequence", shape)?;
    }
    transaction.execute("DELETE FROM recall_cache", [])?;
    transaction.commit()?;
    Ok(())
}

fn copy_table(
    source: &Connection,
    target: &Connection,
    name: &str,
    shape: &TableShape,
) -> Result<()> {
    let columns = columns(name, shape);
    let mut read = source.prepare(&select(name, &columns, false))?;
    let mut rows = read.query([])?;
    let Some(first) = rows.next()? else {
        return Ok(());
    };
    // Preparing INSERT can resolve a protected trigger even for zero rows.
    // Empty tables never prepare write statements.
    let sql = format!(
        "INSERT INTO main.{} ({}) VALUES ({})",
        format::identifier(name),
        columns
            .iter()
            .map(|column| format::identifier(column))
            .collect::<Vec<_>>()
            .join(","),
        vec!["?"; columns.len()].join(",")
    );
    let mut insert = target
        .prepare(&sql)
        .with_context(|| format!("prepare auxiliary copy for {name}"))?;
    let values = (0..columns.len())
        .map(|index| Cell::read(first.get_ref(index)?))
        .collect::<Result<Vec<_>>>()?;
    insert
        .execute(params_from_iter(&values))
        .with_context(|| format!("copy auxiliary row into {name}"))?;
    while let Some(row) = rows.next()? {
        let values = (0..columns.len())
            .map(|index| Cell::read(row.get_ref(index)?))
            .collect::<Result<Vec<_>>>()?;
        insert
            .execute(params_from_iter(&values))
            .with_context(|| format!("copy auxiliary row into {name}"))?;
    }
    Ok(())
}
