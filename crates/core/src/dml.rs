//! Parameterised UPDATE / INSERT / DELETE for editing table data.
//!
//! Postgres binds every parameter as text, so each placeholder is cast to its
//! column's type (`CAST($1::text AS integer)`); SQLite relies on column
//! affinity instead.

use crate::{Dialect, Value};

/// A column together with its SQL type, used for the Postgres cast.
#[derive(Debug, Clone)]
pub struct TypedColumn {
    pub name: String,
    pub sql_type: String,
}

struct Builder {
    dialect: Dialect,
    params: Vec<Value>,
}

impl Builder {
    fn bind(&mut self, col: &TypedColumn, value: &Value) -> String {
        self.params.push(value.clone());
        let p = self.dialect.placeholder(self.params.len());
        match self.dialect {
            Dialect::Postgres if !col.sql_type.is_empty() => format!("CAST({p}::text AS {})", col.sql_type),
            _ => p,
        }
    }

    fn assignments(&mut self, cols: &[(TypedColumn, Value)], separator: &str) -> String {
        cols.iter()
            .map(|(c, v)| format!("{} = {}", self.dialect.quote_ident(&c.name), self.bind(c, v)))
            .collect::<Vec<_>>()
            .join(separator)
    }
}

pub fn update(
    dialect: Dialect,
    schema: &str,
    table: &str,
    set: &[(TypedColumn, Value)],
    key: &[(TypedColumn, Value)],
) -> (String, Vec<Value>) {
    let mut b = Builder { dialect, params: Vec::new() };
    let set_sql = b.assignments(set, ", ");
    let where_sql = b.assignments(key, " AND ");
    (format!("UPDATE {} SET {set_sql} WHERE {where_sql}", dialect.qualified(schema, table)), b.params)
}

/// Columns left out take their default.
pub fn insert(dialect: Dialect, schema: &str, table: &str, values: &[(TypedColumn, Value)]) -> (String, Vec<Value>) {
    let target = dialect.qualified(schema, table);
    if values.is_empty() {
        return (format!("INSERT INTO {target} DEFAULT VALUES"), Vec::new());
    }
    let mut b = Builder { dialect, params: Vec::new() };
    let cols: Vec<String> = values.iter().map(|(c, _)| dialect.quote_ident(&c.name)).collect();
    let vals: Vec<String> = values.iter().map(|(c, v)| b.bind(c, v)).collect();
    (format!("INSERT INTO {target} ({}) VALUES ({})", cols.join(", "), vals.join(", ")), b.params)
}

pub fn delete(dialect: Dialect, schema: &str, table: &str, key: &[(TypedColumn, Value)]) -> (String, Vec<Value>) {
    let mut b = Builder { dialect, params: Vec::new() };
    let where_sql = b.assignments(key, " AND ");
    (format!("DELETE FROM {} WHERE {where_sql}", dialect.qualified(schema, table)), b.params)
}
