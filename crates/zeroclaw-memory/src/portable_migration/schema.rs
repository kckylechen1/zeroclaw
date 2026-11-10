//! Closed admission for the authentic standalone schema38 shapes.

use super::format::{self, Cell, LEGACY_REVISION, Manifest, SchemaObject, TableShape};
use crate::kernel_store::KernelStoreKind;
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension};
use std::collections::{BTreeMap, BTreeSet};

const AUTHORITY_NAMESPACES: &[&str] = &[
    "typed_record_kinds",
    "operator_maintenance_receipt",
    "memory_supersession_receipts",
    "agent_capability_audit",
];

pub(super) struct Inventory {
    pub schema: Manifest,
    pub counts: BTreeMap<String, u64>,
    pub projection: BTreeMap<String, TableShape>,
    pub schema_sha256: String,
}

pub(super) fn manifest(kind: KernelStoreKind) -> Result<Manifest> {
    let json = match kind {
        KernelStoreKind::Memory => include_str!("memory-schema-v38.json"),
        KernelStoreKind::Companion => include_str!("companion-schema-v38.json"),
    };
    let result: Manifest =
        serde_json::from_str(json).context("decode bundled legacy schema manifest")?;
    ensure!(
        result.source_revision == LEGACY_REVISION && result.user_version == 38,
        "bundled legacy schema provenance is invalid"
    );
    Ok(result)
}

fn vector_object(object: &SchemaObject) -> bool {
    object.table == "memories_vec" || object.table.starts_with("memories_vec_")
}

/// Check the closed DDL inventory before asking SQLite to connect virtual tables.
pub(super) fn verify_legacy_schema(
    conn: &Connection,
    actual_objects: &[SchemaObject],
    expected: &Manifest,
) -> Result<()> {
    if actual_objects != expected.objects {
        let actual: BTreeMap<_, _> = actual_objects
            .iter()
            .map(|object| ((&object.object_type, &object.name), object))
            .collect();
        let wanted: BTreeMap<_, _> = expected
            .objects
            .iter()
            .map(|object| ((&object.object_type, &object.name), object))
            .collect();
        let difference = actual
            .keys()
            .chain(wanted.keys())
            .find(|key| actual.get(*key) != wanted.get(*key));
        if let Some((kind, name)) = difference {
            bail!("legacy schema object is missing, unknown, or altered: {kind} {name}");
        }
        bail!("legacy schema object inventory differs");
    }
    // Only inspect table schemas after sqlite_schema has passed the exact DDL
    // allowlist. No source view, trigger, or table definition is executed here.
    ensure!(
        legacy_table_shapes_match(&format::table_shapes(conn)?, &expected.tables),
        "legacy table column/type/shape inventory differs"
    );
    Ok(())
}

// SQLite 3.54 marks these sqlite-vec 0.1.9 backing tables on a fresh reader.
// Only the PRAGMA kind changes; exact DDL admission precedes this comparison.
pub(super) fn legacy_table_shapes_match(
    actual: &BTreeMap<String, TableShape>,
    expected: &BTreeMap<String, TableShape>,
) -> bool {
    let authentic_vec_parent = expected.get("memories_vec").is_some_and(|parent| {
        parent.kind == "virtual" && actual.get("memories_vec") == Some(parent)
    });
    actual.len() == expected.len()
        && expected.iter().all(|(name, wanted)| {
            actual.get(name).is_some_and(|found| {
                found == wanted
                    || (authentic_vec_parent
                        && matches!(
                            name.as_str(),
                            "memories_vec_chunks" | "memories_vec_info" | "memories_vec_rowids"
                        )
                        && wanted.kind == "table"
                        && found.kind == "shadow"
                        && found.without_rowid == wanted.without_rowid
                        && found.strict == wanted.strict
                        && found.columns == wanted.columns)
            })
        })
}

pub(super) fn inspect(conn: &Connection, kind: KernelStoreKind) -> Result<Inventory> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    ensure!(
        version == 38,
        "kernel migration accepts only the recognized legacy schema38, found schema{version}"
    );
    let mut expected = manifest(kind)?;
    let actual_objects = format::objects(conn)?;
    let has_vectors = actual_objects
        .iter()
        .any(|object| object.object_type == "table" && object.name == "memories_vec");
    if !has_vectors {
        expected.objects.retain(|object| !vector_object(object));
        expected
            .tables
            .retain(|name, _| name != "memories_vec" && !name.starts_with("memories_vec_"));
    }
    verify_legacy_schema(conn, &actual_objects, &expected)?;
    verify_no_internal_rowid_references(conn, &expected)?;
    verify_identity(conn, kind, &expected)?;
    refuse_authority_history(conn)?;
    ensure!(
        conn.query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))? == "ok",
        "legacy SQLite integrity check failed"
    );
    let mut foreign_keys = conn.prepare("PRAGMA foreign_key_check")?;
    ensure!(
        foreign_keys.query([])?.next()?.is_none(),
        "legacy foreign-key violations are unsupported"
    );

    let portable = manifest(KernelStoreKind::Companion)?;
    let mut counts = BTreeMap::new();
    for name in expected
        .tables
        .keys()
        .filter(|name| !format::physical_projection(name))
    {
        let count: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM main.{}", format::identifier(name)),
            [],
            |row| row.get(0),
        )?;
        let count = u64::try_from(count).context("negative source row count")?;
        if !portable.tables.contains_key(name) && name != "sqlite_sequence" {
            ensure!(
                count == 0,
                "nonempty product table cannot migrate to PortableKernel: {name} ({count} rows)"
            );
        }
        counts.insert(name.clone(), count);
    }

    let mut projection = BTreeMap::new();
    for (name, shape) in &portable.tables {
        if format::projection(name) || name == "recall_cache" {
            continue;
        }
        let source_shape = expected
            .tables
            .get(name)
            .with_context(|| format!("portable source table is absent: {name}"))?;
        ensure!(
            source_shape == shape,
            "source portable table has a different shape: {name}"
        );
        projection.insert(name.clone(), shape.clone());
    }
    if has_vectors {
        projection.insert(
            "memories_vec".into(),
            expected.tables["memories_vec"].clone(),
        );
    }
    validate_sequence_history(conn, &expected, &portable, &mut projection)?;
    let schema_sha256 = super::snapshot::bytes_digest(&serde_json::to_vec(&actual_objects)?);
    Ok(Inventory {
        schema: expected,
        counts,
        projection,
        schema_sha256,
    })
}

fn verify_identity(conn: &Connection, kind: KernelStoreKind, manifest: &Manifest) -> Result<()> {
    let expected_profile = match kind {
        KernelStoreKind::Memory => "tachi_full",
        KernelStoreKind::Companion => "portable_kernel",
    };
    ensure!(
        manifest.profile == expected_profile,
        "bundled profile mismatch"
    );
    let profile = stamp(conn, "profile")?.context("legacy store profile stamp is missing")?;
    ensure!(
        profile == expected_profile,
        "legacy source profile is incompatible with the selected store"
    );
    if let Some(role) = stamp(conn, "role")? {
        ensure!(
            role == kind.role(),
            "legacy source role is foreign or a legacy alias; exact host role required"
        );
    }
    let unknown_identity: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM hard_state WHERE namespace='store_identity' AND key NOT IN ('profile','role'))",
        [], |row| row.get(0),
    )?;
    ensure!(
        !unknown_identity,
        "private, destination, or unknown store identity cannot be adopted by this migration"
    );
    let mut statement = conn.prepare(
        "SELECT key, value_json, version FROM hard_state WHERE namespace='migrations' ORDER BY key",
    )?;
    let mut cursor = statement.query([])?;
    let mut keys = Vec::new();
    while let Some(row) = cursor.next()? {
        let key = Cell::read(row.get_ref(0)?)?.text()?.to_owned();
        let json = Cell::read(row.get_ref(1)?)?.text()?.to_owned();
        let version = Cell::read(row.get_ref(2)?)?.integer()?;
        let value: serde_json::Value =
            serde_json::from_str(&json).context("malformed legacy migration sentinel")?;
        ensure!(
            value.as_object().is_some_and(|object| object.len() == 1
                && object
                    .get("ran_at")
                    .is_some_and(serde_json::Value::is_string))
                && version >= 1,
            "malformed legacy migration sentinel"
        );
        keys.push(key);
    }
    ensure!(
        keys == manifest.migration_keys,
        "legacy migration sentinel set is incomplete or unknown"
    );
    Ok(())
}

pub(super) fn stamp(conn: &Connection, key: &str) -> Result<Option<String>> {
    let value: Option<String> = conn
        .query_row(
            "SELECT value_json FROM hard_state WHERE namespace='store_identity' AND key=?1",
            [key],
            |row| row.get(0),
        )
        .optional()?;
    value
        .map(|value| -> Result<String> {
            let json: serde_json::Value =
                serde_json::from_str(&value).context("malformed store identity JSON")?;
            let fields = json
                .as_object()
                .context("store identity must be a JSON object")?;
            ensure!(
                fields.len() == 3
                    && ["value", "conferred_by", "stamped_at"]
                        .iter()
                        .all(|key| fields.get(*key).is_some_and(serde_json::Value::is_string)),
                "store identity stamp has a missing, unknown, or wrongly typed field"
            );
            json.get("value")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .context("store identity has no string value")
        })
        .transpose()
}

fn refuse_authority_history(conn: &Connection) -> Result<()> {
    let mut statement = conn.prepare("SELECT DISTINCT namespace FROM hard_state")?;
    let namespaces = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for namespace in namespaces {
        ensure!(
            !AUTHORITY_NAMESPACES.contains(&namespace.as_str()) && !protected_namespace(&namespace),
            "source contains protected kernel, Soul, or User Model authority history"
        );
    }
    let supersession_edge: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM memory_edges WHERE relation='supersedes')",
        [],
        |row| row.get(0),
    )?;
    let supersession_event: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM tachi_events WHERE event_type='memory.supersession.receipt.v1')", [], |row| row.get(0))?;
    ensure!(
        !supersession_edge && !supersession_event,
        "source contains canonical supersession authority history"
    );
    Ok(())
}

pub(super) fn protected_namespace(namespace: &str) -> bool {
    namespace == "soul"
        || namespace.starts_with("soul:")
        || namespace == "user_model"
        || namespace.starts_with("user_model:")
        || namespace == "agent_soul"
        || namespace.starts_with("agent_soul:")
        || namespace == "private_dyad"
        || namespace.starts_with("private_dyad:")
}

fn verify_no_internal_rowid_references(conn: &Connection, manifest: &Manifest) -> Result<()> {
    for (table, shape) in &manifest.tables {
        if format::projection(table) {
            continue;
        }
        ensure!(
            !shape
                .columns
                .iter()
                .any(|column| matches!(column.name.as_str(), "hard_state_rowid" | "memory_rowid")),
            "source contains an unsupported internal-rowid reference"
        );
        let mut statement =
            conn.prepare("SELECT [table], [to] FROM pragma_foreign_key_list(?1)")?;
        let references = statement
            .query_map([table], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (parent, column) in references {
            ensure!(
                !(matches!(parent.as_str(), "memories" | "hard_state")
                    && column.is_some_and(|column| matches!(
                        column.to_ascii_lowercase().as_str(),
                        "rowid" | "_rowid_" | "oid"
                    ))),
                "source foreign key references an internal rowid"
            );
        }
    }
    Ok(())
}

fn validate_sequence_history(
    conn: &Connection,
    source: &Manifest,
    portable: &Manifest,
    projection: &mut BTreeMap<String, TableShape>,
) -> Result<()> {
    let Some(shape) = source.tables.get("sqlite_sequence") else {
        return Ok(());
    };
    let allowed: BTreeSet<_> = portable
        .objects
        .iter()
        .filter(|object| {
            object.object_type == "table"
                && object
                    .sql
                    .as_ref()
                    .is_some_and(|sql| sql.contains("AUTOINCREMENT"))
        })
        .map(|object| object.name.as_str())
        .collect();
    let mut statement = conn.prepare("SELECT name, seq FROM sqlite_sequence")?;
    let mut cursor = statement.query([])?;
    let mut count = 0;
    while let Some(row) = cursor.next()? {
        let name = Cell::read(row.get_ref(0)?)?.text()?.to_owned();
        ensure!(
            allowed.contains(name.as_str()),
            "source contains product or unknown AUTOINCREMENT history"
        );
        ensure!(
            Cell::read(row.get_ref(1)?)?.integer()? >= 0,
            "source AUTOINCREMENT high watermark is invalid"
        );
        count += 1;
    }
    if count > 0 || portable.tables.contains_key("sqlite_sequence") {
        projection.insert("sqlite_sequence".into(), shape.clone());
    }
    Ok(())
}

/// Newly added kernel tables must be empty; a future schema must not silently
/// create host authority or data that this converter has not accounted for.
pub(super) fn verify_target_shape(conn: &Connection, source: &Inventory) -> Result<()> {
    let actual = format::table_shapes(conn)?;
    for (name, expected) in &source.projection {
        let shape = actual
            .get(name)
            .with_context(|| format!("target lost source table {name}"))?;
        ensure!(
            shape == expected,
            "target column/type/shape differs for {name}"
        );
    }
    for (name, shape) in &actual {
        if source.projection.contains_key(name)
            || format::projection(name)
            || name == "recall_cache"
        {
            continue;
        }
        ensure!(shape.kind == "table", "unsupported new target object");
        let count: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM {}", format::identifier(name)),
            [],
            |row| row.get(0),
        )?;
        ensure!(
            count == 0,
            "new target table unexpectedly contains data: {name}"
        );
    }
    Ok(())
}
