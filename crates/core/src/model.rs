use std::cmp::Ordering;
use std::fmt;
use std::time::Duration;

/// A single cell value, decoded into the closest engine-independent type.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// Arbitrary-precision number kept as its exact textual form.
    Numeric(String),
    Text(String),
    Bytes(Vec<u8>),
    Json(serde_json::Value),
    Array(Vec<Value>),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Ordering used by the results grid: NULLs first, numbers numerically,
    /// everything else by its display text.
    pub fn sort_cmp(&self, other: &Value) -> Ordering {
        match (self, other) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Null, _) => Ordering::Less,
            (_, Value::Null) => Ordering::Greater,
            (Value::Int(a), Value::Int(b)) => a.cmp(b),
            (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
            (a, b) => match (a.as_f64(), b.as_f64()) {
                (Some(x), Some(y)) => x.total_cmp(&y),
                _ => a.to_string().cmp(&b.to_string()),
            },
        }
    }

    fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Float(f) => Some(*f),
            Value::Numeric(s) => s.parse().ok(),
            _ => None,
        }
    }

    /// Text form accepted by Postgres' input functions, used when binding
    /// parameters as `text` and casting server-side.
    pub fn to_param_text(&self) -> Option<String> {
        match self {
            Value::Null => None,
            Value::Float(f) if f.is_infinite() => {
                Some(if *f > 0.0 { "Infinity" } else { "-Infinity" }.into())
            }
            other => Some(other.to_string()),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("NULL"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Float(x) if x.is_nan() => f.write_str("NaN"),
            Value::Float(x) => write!(f, "{x}"),
            Value::Numeric(s) | Value::Text(s) => f.write_str(s),
            Value::Bytes(b) => {
                f.write_str("\\x")?;
                b.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
            }
            Value::Json(j) => write!(f, "{j}"),
            Value::Array(items) => {
                f.write_str("{")?;
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    match v {
                        Value::Text(s) | Value::Numeric(s) if needs_array_quotes(s) => {
                            write!(f, "\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))?
                        }
                        other => write!(f, "{other}")?,
                    }
                }
                f.write_str("}")
            }
        }
    }
}

/// Postgres array-literal quoting rule, so the display form is also valid input.
fn needs_array_quotes(s: &str) -> bool {
    s.is_empty()
        || s.eq_ignore_ascii_case("NULL")
        || s.chars().any(|c| matches!(c, '{' | '}' | ',' | '"' | '\\') || c.is_whitespace())
}

/// The base-table column a result column was read from, when the engine reports it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ColumnOrigin {
    pub schema: String,
    pub table: String,
    pub column: String,
}

#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub type_name: String,
    pub origin: Option<ColumnOrigin>,
}

#[derive(Debug, Clone, Default)]
pub struct ResultSet {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Value>>,
    /// The row limit was reached and more rows exist.
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub enum ExecOutcome {
    Rows(ResultSet),
    Affected(u64),
}

#[derive(Debug, Clone)]
pub struct StatementResult {
    pub sql: String,
    pub outcome: Result<ExecOutcome, DbError>,
    pub elapsed: Duration,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct DbError {
    pub message: String,
    pub detail: Option<String>,
    pub code: Option<String>,
}

impl DbError {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), detail: None, code: None }
    }
}

pub type DbResult<T> = Result<T, DbError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelationKind {
    Table,
    View,
    MaterializedView,
}

#[derive(Debug, Clone)]
pub struct Relation {
    pub name: String,
    pub kind: RelationKind,
}

#[derive(Debug, Clone)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub default: Option<String>,
    /// 1-based position within the primary key.
    pub pk_position: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct IndexInfo {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
    pub primary: bool,
    /// Engine-provided CREATE INDEX statement, when available.
    pub definition: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ForeignKey {
    pub name: String,
    pub columns: Vec<String>,
    pub ref_schema: String,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
    /// SQL type of each referenced column; empty when the engine does not need casts.
    pub ref_column_types: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct TableDetails {
    pub schema: String,
    pub name: String,
    pub kind: RelationKind,
    pub columns: Vec<ColumnInfo>,
    pub indexes: Vec<IndexInfo>,
    pub foreign_keys: Vec<ForeignKey>,
}

impl TableDetails {
    pub fn foreign_key_for(&self, column: &str) -> Option<&ForeignKey> {
        self.foreign_keys.iter().find(|fk| fk.columns.iter().any(|c| c == column))
    }
}
