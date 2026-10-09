//! Pending edits to a result set and the DML that applies them.

use std::collections::{BTreeSet, HashMap};

use dbm_core::dml::{self, TypedColumn};
use dbm_core::{Dialect, RelationKind, ResultSet, TableDetails, Value};

/// A row of the grid: one fetched from the database, or one added locally.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RowRef {
    Existing(usize),
    New(usize),
}

#[derive(Default)]
pub struct Edits {
    /// New values of existing cells, by (data row, column).
    pub updates: HashMap<(usize, usize), Value>,
    /// Added rows; `None` leaves the column to its default.
    pub inserts: Vec<Vec<Option<Value>>>,
    pub deletes: BTreeSet<usize>,
}

impl Edits {
    /// Number of rows that will change.
    pub fn row_count(&self) -> usize {
        let updated: BTreeSet<usize> =
            self.updates.keys().map(|(r, _)| *r).filter(|r| !self.deletes.contains(r)).collect();
        updated.len() + self.inserts.len() + self.deletes.len()
    }

    pub fn set(&mut self, rs: &ResultSet, row: RowRef, col: usize, value: Value) {
        match row {
            RowRef::Existing(r) => {
                if rs.rows[r][col] == value {
                    self.updates.remove(&(r, col));
                } else {
                    self.updates.insert((r, col), value);
                }
            }
            RowRef::New(i) => self.inserts[i][col] = Some(value),
        }
    }
}

/// The single table a result set can be written back to.
pub struct EditTarget {
    pub schema: String,
    pub table: String,
    /// One per result column.
    pub columns: Vec<TypedColumn>,
    /// Result column indices of the primary key, in key order.
    pub key: Vec<usize>,
}

/// Why a result can't be edited, or how to write it back.
pub fn edit_target(rs: &ResultSet, tables: &HashMap<(String, String), TableDetails>) -> Result<EditTarget, String> {
    let origins: Vec<_> = rs
        .columns
        .iter()
        .map(|c| c.origin.as_ref())
        .collect::<Option<_>>()
        .ok_or_else(|| "Read-only: some columns are expressions, not table columns".to_string())?;
    let first = origins.first().ok_or_else(|| "Read-only: no columns".to_string())?;
    if origins.iter().any(|o| o.schema != first.schema || o.table != first.table) {
        return Err("Read-only: columns come from more than one table".into());
    }
    let mut seen = BTreeSet::new();
    if !origins.iter().all(|o| seen.insert(&o.column)) {
        return Err("Read-only: a column appears more than once".into());
    }
    let details = tables
        .get(&(first.schema.clone(), first.table.clone()))
        .ok_or_else(|| "Loading table metadata...".to_string())?;
    if details.kind != RelationKind::Table {
        return Err("Read-only: views can't be edited".into());
    }
    let mut pk: Vec<_> = details.columns.iter().filter_map(|c| c.pk_position.map(|p| (p, &c.name))).collect();
    if pk.is_empty() {
        return Err("Read-only: the table has no primary key".into());
    }
    pk.sort();
    let key = pk
        .iter()
        .map(|(_, name)| {
            origins
                .iter()
                .position(|o| &&o.column == name)
                .ok_or_else(|| format!("Read-only: primary key column {name} is not in the result"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let columns = origins
        .iter()
        .map(|o| TypedColumn {
            name: o.column.clone(),
            sql_type: details
                .columns
                .iter()
                .find(|c| c.name == o.column)
                .map(|c| c.data_type.clone())
                .unwrap_or_default(),
        })
        .collect();
    Ok(EditTarget { schema: first.schema.clone(), table: first.table.clone(), columns, key })
}

/// A statement with its parameters, and whether it must affect exactly one row.
pub type EditStatement = (String, Vec<Value>, bool);

/// Updates, then deletes, then inserts. Updates and deletes are keyed by the
/// row's original primary key, so editing the key itself works.
pub fn statements(dialect: Dialect, target: &EditTarget, rs: &ResultSet, edits: &Edits) -> Vec<EditStatement> {
    let key_of = |r: usize| -> Vec<(TypedColumn, Value)> {
        target.key.iter().map(|&c| (target.columns[c].clone(), rs.rows[r][c].clone())).collect()
    };
    let mut by_row: std::collections::BTreeMap<usize, Vec<(usize, &Value)>> = Default::default();
    for ((r, c), v) in &edits.updates {
        if !edits.deletes.contains(r) {
            by_row.entry(*r).or_default().push((*c, v));
        }
    }
    let mut out = Vec::new();
    for (r, mut cells) in by_row {
        cells.sort_by_key(|(c, _)| *c);
        let set: Vec<_> = cells.iter().map(|(c, v)| (target.columns[*c].clone(), (*v).clone())).collect();
        let (sql, params) = dml::update(dialect, &target.schema, &target.table, &set, &key_of(r));
        out.push((sql, params, true));
    }
    for &r in &edits.deletes {
        let (sql, params) = dml::delete(dialect, &target.schema, &target.table, &key_of(r));
        out.push((sql, params, true));
    }
    for row in &edits.inserts {
        let values: Vec<_> = row
            .iter()
            .enumerate()
            .filter_map(|(c, v)| v.as_ref().map(|v| (target.columns[c].clone(), v.clone())))
            .collect();
        let (sql, params) = dml::insert(dialect, &target.schema, &target.table, &values);
        out.push((sql, params, false));
    }
    out
}
