use std::io::Write;

use crate::{ResultSet, Value};

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
        .map(|row| {
            keys.iter().cloned().zip(row.iter().map(value_to_json)).collect::<serde_json::Map<_, _>>().into()
        })
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
