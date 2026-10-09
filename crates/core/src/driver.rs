use std::sync::Arc;

use async_trait::async_trait;

use crate::{DbResult, Dialect, ExecOutcome, Relation, TableDetails, Value};

/// An open session to one data source. Calls are serialised by the
/// implementation; cancellation goes through [`Connection::canceller`] so it
/// can run while a statement is executing.
#[async_trait]
pub trait Connection: Send + Sync {
    fn dialect(&self) -> Dialect;

    /// Runs one statement. Row-returning statements stop after `max_rows`
    /// rows and set `truncated`. Postgres binds every parameter as `text`, so
    /// SQL that takes parameters must cast them.
    async fn execute(&self, sql: &str, params: &[Value], max_rows: Option<usize>) -> DbResult<ExecOutcome>;

    async fn schemas(&self) -> DbResult<Vec<String>>;

    async fn relations(&self, schema: &str) -> DbResult<Vec<Relation>>;

    async fn table_details(&self, schema: &str, name: &str) -> DbResult<TableDetails>;

    /// CREATE statement(s) for a table or view.
    async fn ddl(&self, schema: &str, name: &str) -> DbResult<String>;

    fn canceller(&self) -> Arc<dyn Canceller>;

    /// Whether an explicit transaction is open on this connection.
    async fn in_transaction(&self) -> bool;
}

#[async_trait]
pub trait Canceller: Send + Sync {
    async fn cancel(&self) -> DbResult<()>;
}

/// Whether re-running a statement with a larger row limit is free of side
/// effects, which is how "load more" is implemented.
pub fn is_read_only_query(sql: &str) -> bool {
    let first = first_keyword(sql).to_ascii_uppercase();
    matches!(first.as_str(), "SELECT" | "VALUES" | "TABLE" | "SHOW" | "EXPLAIN" | "PRAGMA")
        || (first == "WITH" && !contains_dml_keyword(sql))
}

fn first_keyword(sql: &str) -> &str {
    let mut rest = sql.trim_start();
    loop {
        if let Some(r) = rest.strip_prefix("--") {
            rest = r.split_once('\n').map_or("", |(_, tail)| tail).trim_start();
        } else if let Some(r) = rest.strip_prefix("/*") {
            rest = r.split_once("*/").map_or("", |(_, tail)| tail).trim_start();
        } else if let Some(r) = rest.strip_prefix('(') {
            rest = r.trim_start();
        } else {
            break;
        }
    }
    let end = rest.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(rest.len());
    &rest[..end]
}

fn contains_dml_keyword(sql: &str) -> bool {
    sql.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|w| ["INSERT", "UPDATE", "DELETE", "MERGE"].iter().any(|k| w.eq_ignore_ascii_case(k)))
}
