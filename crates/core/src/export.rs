use std::io::Write;

use crate::{Dialect, ResultSet, Value};

/// Writes a header row and one row per record; NULL becomes an empty field.
pub fn write_csv(rs: &ResultSet, out: impl Write) -> csv::Result<()> {
    let mut w = csv::Writer::from_writer(out);
    w.write_record(rs.columns.iter().map(|c| c.name.as_str()))?;
    for row in &rs.rows {
        w.write_record(row.iter().map(|v| if v.is_null() { String::new() } else { v.to_string() }))?;
    }
    w.flush()?;
    Ok(())
}

/// An array of objects keyed by column name. Repeated column names get a
/// `_2`, `_3`… suffix so no value is lost.
pub fn to_json(rs: &ResultSet) -> serde_json::Value {
    let mut keys: Vec<String> = Vec::with_capacity(rs.columns.len());
    for col in &rs.columns {
        let mut key = col.name.clone();
        let mut n = 2;
        while keys.contains(&key) {
            key = format!("{}_{n}", col.name);
            n += 1;
        }
        keys.push(key);
    }
    rs.rows
        .iter()
        .map(|row| keys.iter().cloned().zip(row.iter().map(value_to_json)).collect::<serde_json::Map<_, _>>().into())
        .collect::<Vec<serde_json::Value>>()
        .into()
}

fn value_to_json(v: &Value) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        Value::Null => J::Null,
        Value::Bool(b) => J::Bool(*b),
        Value::Int(i) => J::from(*i),
        Value::Float(f) => serde_json::Number::from_f64(*f).map_or_else(|| J::String(v.to_string()), J::Number),
        Value::Numeric(s) => s.parse::<serde_json::Number>().map_or_else(|_| J::String(s.clone()), J::Number),
        Value::Json(j) => j.clone(),
        Value::Array(items) => J::Array(items.iter().map(value_to_json).collect()),
        Value::Text(_) | Value::Bytes(_) => J::String(v.to_string()),
    }
}

/// The given rows of `rs`, in that order, as a result set of their own.
pub fn subset(rs: &ResultSet, rows: &[usize]) -> ResultSet {
    ResultSet {
        columns: rs.columns.clone(),
        rows: rows.iter().filter_map(|&r| rs.rows.get(r).cloned()).collect(),
        truncated: false,
    }
}

fn plain_cell(v: &Value) -> String {
    if v.is_null() { String::new() } else { v.to_string().replace(['\t', '\n', '\r'], " ") }
}

/// Tab-separated with a header row, the format spreadsheets expect on paste.
pub fn to_tsv(rs: &ResultSet) -> String {
    let mut out = rs.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join("\t");
    for row in &rs.rows {
        out.push('\n');
        out.push_str(&row.iter().map(plain_cell).collect::<Vec<_>>().join("\t"));
    }
    out
}

pub fn to_markdown(rs: &ResultSet) -> String {
    let esc = |s: String| s.replace('|', "\\|");
    let mut out = format!("| {} |\n", rs.columns.iter().map(|c| esc(c.name.clone())).collect::<Vec<_>>().join(" | "));
    out.push_str(&format!("|{}\n", " --- |".repeat(rs.columns.len())));
    for row in &rs.rows {
        let cells: Vec<String> =
            row.iter().map(|v| if v.is_null() { "NULL".into() } else { esc(plain_cell(v)) }).collect();
        out.push_str(&format!("| {} |\n", cells.join(" | ")));
    }
    out
}

/// A SQL literal for `v` in `dialect`.
pub fn sql_literal(dialect: Dialect, v: &Value) -> String {
    let quoted = |s: &str| format!("'{}'", s.replace('\'', "''"));
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) if f.is_finite() => v.to_string(),
        Value::Numeric(s) if s.parse::<f64>().is_ok_and(f64::is_finite) => s.clone(),
        Value::Bytes(b) => {
            let hex: String = b.iter().map(|byte| format!("{byte:02x}")).collect();
            match dialect {
                Dialect::Postgres => format!("'\\x{hex}'"),
                Dialect::Sqlite => format!("X'{hex}'"),
            }
        }
        other => quoted(&other.to_string()),
    }
}

/// One INSERT per row into `table` (unqualified, so it runs in either engine).
pub fn to_inserts(dialect: Dialect, table: &str, rs: &ResultSet) -> String {
    let cols: Vec<String> = rs.columns.iter().map(|c| dialect.quote_ident(&c.name)).collect();
    let target = dialect.quote_ident(table);
    rs.rows
        .iter()
        .map(|row| {
            let values: Vec<String> = row
                .iter()
                .zip(&rs.columns)
                .map(|(v, col)| match v {
                    // SQLite keeps booleans as 0/1; Postgres only accepts TRUE/FALSE.
                    Value::Int(i @ (0 | 1)) if col.type_name.to_ascii_lowercase().starts_with("bool") => {
                        sql_literal(dialect, &Value::Bool(*i == 1))
                    }
                    other => sql_literal(dialect, other),
                })
                .collect();
            format!("INSERT INTO {target} ({}) VALUES ({});", cols.join(", "), values.join(", "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}
