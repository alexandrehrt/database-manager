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
                if same_value(&rs.rows[r][col], &value) {
                    self.updates.remove(&(r, col));
                } else {
                    self.updates.insert((r, col), value);
                }
            }
            RowRef::New(i) => self.inserts[i][col] = Some(value),
        }
    }
}

/// Whether an edited value is just the original again: typed text that
/// reads the same, or a checkbox matching SQLite's 0/1 booleans.
fn same_value(original: &Value, edited: &Value) -> bool {
    match (original, edited) {
        (Value::Null, Value::Null) => true,
        (Value::Null, _) | (_, Value::Null) => false,
        (Value::Json(j), Value::Text(t)) => serde_json::from_str::<serde_json::Value>(t).is_ok_and(|v| v == *j),
        (o, Value::Text(t)) => o.to_string() == *t,
        (Value::Int(i), Value::Bool(b)) | (Value::Bool(b), Value::Int(i)) => *i == *b as i64,
        (o, e) => o == e,
    }
}

/// Whether a column holds text, where an empty value is an empty string
/// rather than NULL. Untyped (SQLite expression) columns count as text.
pub fn is_texty(type_name: &str) -> bool {
    let t = type_name.to_lowercase();
    t.is_empty() || ["char", "text", "clob", "string", "name"].iter().any(|k| t.contains(k))
}

/// The value an edit buffer stands for: empty means NULL unless the column is
/// text and the cell wasn't NULL, so leaving a NULL cell empty keeps it NULL.
pub fn typed_value(text: String, type_name: &str, was_null: bool) -> Value {
    if text.is_empty() && (was_null || !is_texty(type_name)) { Value::Null } else { Value::Text(text) }
}

/// "Set to now" for a date / time column: the menu label and the current local
/// time as text the column's engine reads (Oracle via the session NLS formats).
pub fn now_value(type_name: &str, dialect: Dialect) -> Option<(&'static str, Value)> {
    let t = type_name.to_lowercase();
    let now = chrono::Local::now();
    let text = if t.contains("time zone") || t.contains("timestamptz") {
        now.format("%Y-%m-%d %H:%M:%S%.6f %:z").to_string()
    } else if t.contains("timestamp") || t.contains("datetime") {
        match dialect {
            Dialect::Sqlite => now.format("%Y-%m-%d %H:%M:%S").to_string(),
            _ => now.format("%Y-%m-%d %H:%M:%S%.6f").to_string(),
        }
    } else if t.starts_with("date") {
        // Oracle DATE carries a time of day.
        return Some(match dialect {
            Dialect::Oracle => ("Set to now", Value::Text(now.format("%Y-%m-%d %H:%M:%S").to_string())),
            _ => ("Set to today", Value::Text(now.format("%Y-%m-%d").to_string())),
        });
    } else if t.starts_with("time") {
        now.format("%H:%M:%S").to_string()
    } else {
        return None;
    };
    Some(("Set to now", Value::Text(text)))
}

/// A copy of fetched row `row` (with its pending edits) as a new row. Key
/// columns the database fills in itself (a default, or SQLite's rowid alias)
/// are left to their default so the copy doesn't collide.
pub fn duplicate(
    rs: &ResultSet,
    edits: &Edits,
    row: usize,
    target: &EditTarget,
    details: Option<&TableDetails>,
    dialect: Dialect,
) -> Vec<Option<Value>> {
    let rowid_alias = dialect == Dialect::Sqlite
        && target.key.len() == 1
        && target.columns[target.key[0]].sql_type.eq_ignore_ascii_case("integer");
    (0..rs.columns.len())
        .map(|c| {
            if target.key.contains(&c) {
                let has_default = details
                    .and_then(|d| d.columns.iter().find(|col| col.name == target.columns[c].name))
                    .is_some_and(|col| col.default.is_some());
                if has_default || rowid_alias {
                    return None;
                }
                // A single integer key without a default gets the next number after
                // the fetched and pending rows, so the copy doesn't collide on it.
                if target.key.len() == 1
                    && let Value::Int(_) = rs.rows[row][c]
                {
                    let fetched = rs.rows.iter().filter_map(|r| if let Value::Int(i) = r[c] { Some(i) } else { None });
                    let pending = edits.inserts.iter().filter_map(|r| match &r[c] {
                        Some(Value::Int(i)) => Some(*i),
                        _ => None,
                    });
                    return Some(Value::Int(fetched.chain(pending).max().unwrap_or(0) + 1));
                }
            }
            Some(edits.updates.get(&(row, c)).cloned().unwrap_or_else(|| rs.rows[row][c].clone()))
        })
        .collect()
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
