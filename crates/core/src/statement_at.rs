use crate::Dialect;
use crate::sql_split::{Span, split};

/// The statement Cmd+Enter runs for a cursor at byte offset `cursor`: the one
/// the cursor is inside, or else the closest one that starts before it.
pub fn statement_at(sql: &str, cursor: usize, dialect: Dialect) -> Option<Span> {
    let spans = split(sql, dialect);
    spans.iter().rev().find(|s| s.start <= cursor).or_else(|| spans.first()).cloned()
}
