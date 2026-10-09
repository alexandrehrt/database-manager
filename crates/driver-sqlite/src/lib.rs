//! SQLite implementation of [`dbm_core::Connection`].
//!
//! rusqlite is synchronous, so every call runs on tokio's blocking pool
//! against a mutex-guarded connection. Cancellation uses SQLite's interrupt,
//! which is safe to trigger from any thread.

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use dbm_core::{
    Canceller, Column, ColumnInfo, ColumnOrigin, Connection, DbError, DbResult, Dialect, ExecOutcome, ForeignKey,
    IndexInfo, Relation, RelationKind, ResultSet, TableDetails, Value,
};
use rusqlite::types::ValueRef;
use rusqlite::{InterruptHandle, OptionalExtension, params};

pub struct SqliteConnection {
    conn: Arc<Mutex<rusqlite::Connection>>,
    interrupt: Arc<InterruptHandle>,
}

/// Opens (creating if needed) the database file with foreign keys enforced.
pub async fn connect(path: &Path) -> DbResult<SqliteConnection> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        let conn = rusqlite::Connection::open(&path).map_err(db_err)?;
        conn.execute_batch("PRAGMA foreign_keys = ON;").map_err(db_err)?;
        conn.busy_timeout(std::time::Duration::from_secs(5)).map_err(db_err)?;
        let interrupt = Arc::new(conn.get_interrupt_handle());
        Ok(SqliteConnection { conn: Arc::new(Mutex::new(conn)), interrupt })
    })
    .await
    .map_err(|e| DbError::new(e.to_string()))?
}

fn db_err(e: rusqlite::Error) -> DbError {
    let code = e.sqlite_error_code();
    let message = if code == Some(rusqlite::ErrorCode::OperationInterrupted) {
        "Query cancelled".to_string()
    } else {
        e.to_string()
    };
    let code = code.filter(|c| *c != rusqlite::ErrorCode::Unknown).map(|c| format!("{c:?}"));
    DbError { message, detail: None, code }
}

impl SqliteConnection {
    async fn blocking<T, F>(&self, f: F) -> DbResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&rusqlite::Connection) -> rusqlite::Result<T> + Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let guard = conn.lock().map_err(|_| DbError::new("SQLite connection poisoned by an earlier panic"))?;
            f(&guard).map_err(db_err)
        })
        .await
        .map_err(|e| DbError::new(e.to_string()))?
    }
}

fn to_sql_value(v: &Value) -> rusqlite::types::Value {
    use rusqlite::types::Value as S;
    match v {
        Value::Null => S::Null,
        Value::Bool(b) => S::Integer(*b as i64),
        Value::Int(i) => S::Integer(*i),
        Value::Float(f) => S::Real(*f),
        Value::Bytes(b) => S::Blob(b.clone()),
        other => S::Text(other.to_string()),
    }
}

fn from_sql_value(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::Int(i),
        ValueRef::Real(f) => Value::Float(f),
        ValueRef::Text(t) => Value::Text(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::Bytes(b.to_vec()),
    }
}

fn run_statement(
    conn: &rusqlite::Connection,
    sql: &str,
    params: &[Value],
    max_rows: Option<usize>,
) -> rusqlite::Result<ExecOutcome> {
    let mut stmt = conn.prepare(sql)?;
    for (i, p) in params.iter().enumerate() {
        stmt.raw_bind_parameter(i + 1, to_sql_value(p))?;
    }
    if stmt.column_count() == 0 {
        let before = conn.total_changes();
        stmt.raw_execute()?;
        return Ok(ExecOutcome::Affected(conn.total_changes() - before));
    }

    let columns: Vec<Column> = stmt
        .columns_with_metadata()
        .iter()
        .zip(stmt.columns())
        .map(|(meta, col)| Column {
            name: meta.name().to_string(),
            type_name: col.decl_type().unwrap_or_default().to_string(),
            origin: match (meta.database_name(), meta.table_name(), meta.origin_name()) {
                (Some(schema), Some(table), Some(column)) => Some(ColumnOrigin {
                    schema: schema.to_string(),
                    table: table.to_string(),
                    column: column.to_string(),
                }),
                _ => None,
            },
        })
        .collect();
    // Writing statements (INSERT … RETURNING) are always read to completion.
    let limit = if stmt.readonly() { max_rows } else { None };
    let width = columns.len();

    let mut rows_out = Vec::new();
    let mut truncated = false;
    let mut rows = stmt.raw_query();
    while let Some(row) = rows.next()? {
        if limit.is_some_and(|max| rows_out.len() >= max) {
            truncated = true;
            break;
        }
        rows_out.push((0..width).map(|i| row.get_ref(i).map(from_sql_value)).collect::<rusqlite::Result<_>>()?);
    }
    Ok(ExecOutcome::Rows(ResultSet { columns, rows: rows_out, truncated }))
}

fn table_details(conn: &rusqlite::Connection, schema: &str, name: &str) -> rusqlite::Result<TableDetails> {
    let kind = conn
        .query_row(
            &format!("SELECT type FROM {}.sqlite_schema WHERE name = ?1", Dialect::Sqlite.quote_ident(schema)),
            [name],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .map_or(RelationKind::Table, |t| if t == "view" { RelationKind::View } else { RelationKind::Table });

    let columns = conn
        .prepare("SELECT name, type, \"notnull\", dflt_value, pk FROM pragma_table_info(?1, ?2) ORDER BY cid")?
        .query_map(params![name, schema], |r| {
            let pk: u32 = r.get(4)?;
            Ok(ColumnInfo {
                name: r.get(0)?,
                data_type: r.get(1)?,
                nullable: !r.get::<_, bool>(2)?,
                default: r.get(3)?,
                pk_position: (pk > 0).then_some(pk),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut indexes = Vec::new();
    let index_rows = conn
        .prepare("SELECT name, \"unique\", origin FROM pragma_index_list(?1, ?2) ORDER BY name")?
        .query_map(params![name, schema], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?, r.get::<_, String>(2)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (index_name, unique, origin) in index_rows {
        let cols = conn
            .prepare("SELECT name FROM pragma_index_info(?1, ?2) ORDER BY seqno")?
            .query_map(params![index_name, schema], |r| r.get::<_, Option<String>>(0))?
            .map(|c| c.map(|c| c.unwrap_or_else(|| "<expression>".into())))
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let definition = conn
            .query_row(
                &format!(
                    "SELECT sql FROM {}.sqlite_schema WHERE type = 'index' AND name = ?1",
                    Dialect::Sqlite.quote_ident(schema)
                ),
                [&index_name],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        indexes.push(IndexInfo { name: index_name, columns: cols, unique, primary: origin == "pk", definition });
    }

    Ok(TableDetails {
        schema: schema.to_string(),
        name: name.to_string(),
        kind,
        columns,
        indexes,
        foreign_keys: foreign_keys(conn, schema, name)?,
    })
}

fn foreign_keys(conn: &rusqlite::Connection, schema: &str, table: &str) -> rusqlite::Result<Vec<ForeignKey>> {
    let rows = conn
        .prepare("SELECT id, \"table\", \"from\", \"to\" FROM pragma_foreign_key_list(?1, ?2) ORDER BY id, seq")?
        .query_map(params![table, schema], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<String>>(3)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut fks: Vec<(i64, ForeignKey, bool)> = Vec::new();
    for (id, ref_table, from, to) in rows {
        if fks.last().is_none_or(|(last, ..)| *last != id) {
            let fk = ForeignKey {
                name: format!("{table}_fk{id}"),
                columns: Vec::new(),
                ref_schema: schema.to_string(),
                ref_table,
                ref_columns: Vec::new(),
                ref_column_types: Vec::new(),
            };
            fks.push((id, fk, false));
        }
        let (_, fk, implicit) = fks.last_mut().expect("pushed above");
        fk.columns.push(from);
        match to {
            Some(to) => fk.ref_columns.push(to),
            None => *implicit = true,
        }
    }
    // `REFERENCES parent` without a column list targets the parent's primary key.
    for (_, fk, implicit) in &mut fks {
        if *implicit {
            fk.ref_columns = conn
                .prepare("SELECT name FROM pragma_table_info(?1, ?2) WHERE pk > 0 ORDER BY pk")?
                .query_map(params![fk.ref_table, schema], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
        }
    }
    Ok(fks.into_iter().map(|(_, fk, _)| fk).collect())
}

#[async_trait]
impl Connection for SqliteConnection {
    fn dialect(&self) -> Dialect {
        Dialect::Sqlite
    }

    async fn execute(&self, sql: &str, params: &[Value], max_rows: Option<usize>) -> DbResult<ExecOutcome> {
        let (sql, params) = (sql.to_string(), params.to_vec());
        self.blocking(move |c| run_statement(c, &sql, &params, max_rows)).await
    }

    async fn schemas(&self) -> DbResult<Vec<String>> {
        self.blocking(|c| {
            c.prepare("SELECT name FROM pragma_database_list WHERE name <> 'temp' ORDER BY seq")?
                .query_map([], |r| r.get(0))?
                .collect()
        })
        .await
    }

    async fn relations(&self, schema: &str) -> DbResult<Vec<Relation>> {
        let sql = format!(
            "SELECT name, type FROM {}.sqlite_schema WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' ORDER BY name",
            Dialect::Sqlite.quote_ident(schema)
        );
        self.blocking(move |c| {
            c.prepare(&sql)?
                .query_map([], |r| {
                    let kind: String = r.get(1)?;
                    Ok(Relation {
                        name: r.get(0)?,
                        kind: if kind == "view" { RelationKind::View } else { RelationKind::Table },
                    })
                })?
                .collect()
        })
        .await
    }

    async fn table_details(&self, schema: &str, name: &str) -> DbResult<TableDetails> {
        let (schema, name) = (schema.to_string(), name.to_string());
        self.blocking(move |c| table_details(c, &schema, &name)).await
    }

    /// SQLite keeps the original DDL text, so this returns it verbatim along
    /// with the table's indexes and triggers.
    async fn ddl(&self, schema: &str, name: &str) -> DbResult<String> {
        let sql = format!(
            "SELECT sql FROM {}.sqlite_schema WHERE tbl_name = ?1 AND sql IS NOT NULL \
             ORDER BY CASE type WHEN 'table' THEN 0 WHEN 'view' THEN 0 WHEN 'index' THEN 1 ELSE 2 END, name",
            Dialect::Sqlite.quote_ident(schema)
        );
        let name = name.to_string();
        self.blocking(move |c| {
            let parts: Vec<String> =
                c.prepare(&sql)?.query_map([&name], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            Ok(parts.iter().map(|s| format!("{s};")).collect::<Vec<_>>().join("\n\n"))
        })
        .await
    }

    fn canceller(&self) -> Arc<dyn Canceller> {
        Arc::new(SqliteCanceller(self.interrupt.clone()))
    }

    async fn in_transaction(&self) -> bool {
        self.blocking(|c| Ok(!c.is_autocommit())).await.unwrap_or(false)
    }
}

struct SqliteCanceller(Arc<InterruptHandle>);

#[async_trait]
impl Canceller for SqliteCanceller {
    async fn cancel(&self) -> DbResult<()> {
        self.0.interrupt();
        Ok(())
    }
}
