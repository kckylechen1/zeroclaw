//! Fixture serialization; compiled only by the generator and migration tests.

use super::format::{Cell, TableShape, identifier};
use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct Fixture {
    pub source_revision: String,
    pub user_version: i64,
    pub tables: BTreeMap<String, TableData>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct TableData {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Cell>>,
}

pub(super) fn read_table(
    conn: &Connection,
    name: &str,
    shape: &TableShape,
    include_rowid: bool,
) -> Result<TableData> {
    let mut columns = Vec::new();
    if include_rowid && shape.kind == "table" && !shape.without_rowid {
        columns.push("rowid".to_owned());
    }
    columns.extend(
        shape
            .columns
            .iter()
            .filter(|column| column.hidden == 0)
            .map(|column| column.name.clone()),
    );
    let sql = format!(
        "SELECT {} FROM main.{}",
        columns
            .iter()
            .map(|name| identifier(name))
            .collect::<Vec<_>>()
            .join(","),
        identifier(name)
    );
    let mut statement = conn
        .prepare(&sql)
        .with_context(|| format!("read fixture table {name}"))?;
    let mut cursor = statement.query([])?;
    let mut rows = Vec::new();
    while let Some(row) = cursor.next()? {
        rows.push(
            (0..columns.len())
                .map(|index| Cell::read(row.get_ref(index)?))
                .collect::<Result<Vec<_>>>()?,
        );
    }
    Ok(TableData { columns, rows })
}
