//! Typed SQLite values shared with the independently compiled legacy fixture generator.

use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use rusqlite::types::{ToSql, ToSqlOutput, ValueRef};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub(super) const LEGACY_REVISION: &str = "eba0c48617e8820831bfd22f91d269c6d70f409f";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct SchemaObject {
    pub object_type: String,
    pub name: String,
    pub table: String,
    pub sql: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Column {
    pub name: String,
    pub declared_type: String,
    pub not_null: bool,
    pub default: Option<String>,
    pub primary_key: i64,
    pub hidden: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct TableShape {
    pub kind: String,
    pub without_rowid: bool,
    pub strict: bool,
    pub columns: Vec<Column>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Manifest {
    pub source_revision: String,
    pub user_version: i64,
    pub profile: String,
    pub objects: Vec<SchemaObject>,
    pub tables: BTreeMap<String, TableShape>,
    pub migration_keys: Vec<String>,
}

/// REAL stores its IEEE-754 bits; NULL and an empty BLOB remain distinct.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub(super) enum Cell {
    Null,
    Integer(i64),
    Real(u64),
    Text(String),
    Blob(Vec<u8>),
}

impl Cell {
    pub fn read(value: ValueRef<'_>) -> Result<Self> {
        Ok(match value {
            ValueRef::Null => Self::Null,
            ValueRef::Integer(value) => Self::Integer(value),
            ValueRef::Real(value) => Self::Real(value.to_bits()),
            ValueRef::Text(value) => Self::Text(
                std::str::from_utf8(value)
                    .context("non-UTF-8 SQLite TEXT is unsupported")?
                    .to_owned(),
            ),
            ValueRef::Blob(value) => Self::Blob(value.to_vec()),
        })
    }

    pub fn text(&self) -> Result<&str> {
        match self {
            Self::Text(value) => Ok(value),
            _ => bail!("expected SQLite TEXT"),
        }
    }

    pub fn integer(&self) -> Result<i64> {
        match self {
            Self::Integer(value) => Ok(*value),
            _ => bail!("expected SQLite INTEGER"),
        }
    }

    pub fn optional_text(&self) -> Result<Option<String>> {
        match self {
            Self::Null => Ok(None),
            Self::Text(value) => Ok(Some(value.clone())),
            _ => bail!("expected SQLite TEXT or NULL"),
        }
    }
}

impl ToSql for Cell {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Borrowed(match self {
            Self::Null => ValueRef::Null,
            Self::Integer(value) => ValueRef::Integer(*value),
            Self::Real(bits) => ValueRef::Real(f64::from_bits(*bits)),
            Self::Text(value) => ValueRef::Text(value.as_bytes()),
            Self::Blob(value) => ValueRef::Blob(value),
        }))
    }
}

pub(super) fn identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

pub(super) fn objects(conn: &Connection) -> Result<Vec<SchemaObject>> {
    let mut statement = conn
        .prepare("SELECT type, name, tbl_name, sql FROM main.sqlite_schema ORDER BY type, name")?;
    let result = statement
        .query_map([], |row| {
            Ok(SchemaObject {
                object_type: row.get(0)?,
                name: row.get(1)?,
                table: row.get(2)?,
                sql: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(result)
}

pub(super) fn table_shapes(conn: &Connection) -> Result<BTreeMap<String, TableShape>> {
    let mut statement = conn.prepare(
        "SELECT name, type, wr, strict FROM pragma_table_list \
         WHERE schema = 'main' AND name != 'sqlite_schema' ORDER BY name",
    )?;
    let tables = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, bool>(2)?,
                row.get::<_, bool>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut result = BTreeMap::new();
    for (name, kind, without_rowid, strict) in tables {
        let mut columns = conn.prepare(
            "SELECT name, type, [notnull], dflt_value, pk, hidden \
             FROM pragma_table_xinfo(?1) ORDER BY cid",
        )?;
        let columns = columns
            .query_map([&name], |row| {
                Ok(Column {
                    name: row.get(0)?,
                    declared_type: row.get(1)?,
                    not_null: row.get(2)?,
                    default: row.get(3)?,
                    primary_key: row.get(4)?,
                    hidden: row.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        result.insert(
            name,
            TableShape {
                kind,
                without_rowid,
                strict,
                columns,
            },
        );
    }
    Ok(result)
}

pub(super) fn projection(name: &str) -> bool {
    name == "memories_fts"
        || name.starts_with("memories_fts_")
        || name == "memories_symbolic_fts"
        || name.starts_with("memories_symbolic_fts_")
        || name == "memories_vec"
        || name.starts_with("memories_vec_")
}

pub(super) fn physical_projection(name: &str) -> bool {
    projection(name)
        && !matches!(
            name,
            "memories_fts" | "memories_symbolic_fts" | "memories_vec"
        )
}
