//! PostgreSQL implementation of [`dbm_core::Connection`] on tokio-postgres.
//!
//! Row-returning reads run through a portal so that only `max_rows + 1` rows
//! are transferred. Portals need a transaction: outside a user transaction
//! the read gets a short one of its own; inside one (tracked from the
//! statements the user runs) the portal runs in the user's transaction,
//! which is left open.

mod decode;
mod introspect;

use std::sync::Arc;

use async_trait::async_trait;
use dbm_core::config::SslMode;
use dbm_core::driver::is_read_only_query;
use dbm_core::{
    Canceller, Column, ColumnOrigin, Connection, DbError, DbResult, Dialect, ExecOutcome, Relation, ResultSet,
    TableDetails, Value,
};
use postgres_native_tls::MakeTlsConnector;
use tokio::sync::Mutex;
use tokio_postgres::types::Type;
use tokio_postgres::{CancelToken, Client, Row, Statement};

pub struct PgConnection {
    session: Mutex<Session>,
    cancel: CancelToken,
    tls: MakeTlsConnector,
}

struct Session {
    client: Client,
    /// The user ran BEGIN and has not yet committed or rolled back.
    in_user_transaction: bool,
}

pub struct PgParams<'a> {
    pub host: &'a str,
    pub port: u16,
    pub database: &'a str,
    pub user: &'a str,
    pub password: Option<&'a str>,
    pub ssl_mode: SslMode,
}

pub async fn connect(p: PgParams<'_>) -> DbResult<PgConnection> {
    let mut config = tokio_postgres::Config::new();
    config
        .host(p.host)
        .port(p.port)
        .dbname(p.database)
        .user(p.user)
        .application_name("database-manager")
        .connect_timeout(std::time::Duration::from_secs(10))
        .ssl_mode(match p.ssl_mode {
            SslMode::Disable => tokio_postgres::config::SslMode::Disable,
            SslMode::Prefer => tokio_postgres::config::SslMode::Prefer,
            SslMode::Require => tokio_postgres::config::SslMode::Require,
        });
    if let Some(pw) = p.password {
        config.password(pw);
    }
    // libpq's prefer/require encrypt without verifying the certificate.
    let connector = native_tls::TlsConnector::builder()
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .build()
        .map_err(|e| DbError::new(e.to_string()))?;
    let tls = MakeTlsConnector::new(connector);
    let (client, connection) = config.connect(tls.clone()).await.map_err(pg_err)?;
    tokio::spawn(connection);
    Ok(PgConnection {
        cancel: client.cancel_token(),
        session: Mutex::new(Session { client, in_user_transaction: false }),
        tls,
    })
}

pub(crate) fn pg_err(e: tokio_postgres::Error) -> DbError {
    match e.as_db_error() {
        Some(db) => DbError {
            message: db.message().to_string(),
            detail: db.detail().or(db.hint()).map(str::to_string),
            code: Some(db.code().code().to_string()),
            // 1-based character index into the statement as sent.
            position: match db.position() {
                Some(tokio_postgres::error::ErrorPosition::Original(n)) => (*n as usize).checked_sub(1),
                _ => None,
            },
        },
        None => DbError::new(e.to_string()),
    }
}

/// Upper-cased leading words, skipping comments.
fn leading_words(sql: &str, n: usize) -> Vec<String> {
    let spans = dbm_core::sql_split::split(sql, Dialect::Postgres);
    let code = spans.first().map_or("", |s| &sql[s.clone()]);
    code.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|w| !w.is_empty())
        .take(n)
        .map(str::to_ascii_uppercase)
        .collect()
}

fn transaction_effect(sql: &str) -> Option<bool> {
    let words = leading_words(sql, 3);
    match words.first().map(String::as_str) {
        Some("BEGIN") | Some("START") => Some(true),
        Some("COMMIT") | Some("END") | Some("ABORT") => Some(false),
        Some("ROLLBACK") if !words.iter().any(|w| w == "TO") => Some(false),
        Some("PREPARE") if words.get(1).map(String::as_str) == Some("TRANSACTION") => Some(false),
        _ => None,
    }
}

fn type_display_name(ty: &Type) -> String {
    match ty.kind() {
        tokio_postgres::types::Kind::Array(elem) => format!("{}[]", elem.name()),
        _ => ty.name().to_string(),
    }
}

impl PgConnection {
    async fn resolve_origins(client: &Client, stmt: &Statement) -> DbResult<Vec<Option<ColumnOrigin>>> {
        let keys: Vec<Option<(u32, i16)>> = stmt
            .columns()
            .iter()
            .map(|c| match (c.table_oid(), c.column_id()) {
                (Some(oid), Some(att)) if oid != 0 && att > 0 => Some((oid, att)),
                _ => None,
            })
            .collect();
        let oids: Vec<u32> = keys.iter().flatten().map(|(oid, _)| *oid).collect();
        if oids.is_empty() {
            return Ok(vec![None; keys.len()]);
        }
        let rows = client
            .query(
                "SELECT a.attrelid, a.attnum, n.nspname::text, c.relname::text, a.attname::text \
                 FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE a.attrelid = ANY($1) AND a.attnum > 0",
                &[&oids],
            )
            .await
            .map_err(pg_err)?;
        let lookup: std::collections::HashMap<(u32, i16), ColumnOrigin> = rows
            .iter()
            .map(|r| ((r.get(0), r.get(1)), ColumnOrigin { schema: r.get(2), table: r.get(3), column: r.get(4) }))
            .collect();
        Ok(keys.into_iter().map(|k| k.and_then(|k| lookup.get(&k).cloned())).collect())
    }

    async fn run(&self, sql: &str, params: &[Value], max_rows: Option<usize>) -> DbResult<ExecOutcome> {
        let mut session = self.session.lock().await;
        let text_params: Vec<Option<String>> = params.iter().map(Value::to_param_text).collect();
        let param_refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
            text_params.iter().map(|p| p as &(dyn tokio_postgres::types::ToSql + Sync)).collect();
        let stmt = session.client.prepare_typed(sql, &vec![Type::TEXT; params.len()]).await.map_err(pg_err)?;

        if stmt.columns().is_empty() {
            let affected = session.client.execute(&stmt, &param_refs).await.map_err(pg_err)?;
            if let Some(open) = transaction_effect(sql) {
                session.in_user_transaction = open;
            }
            return Ok(ExecOutcome::Affected(affected));
        }

        let limit = max_rows.filter(|_| is_read_only_query(sql));
        let in_user_transaction = session.in_user_transaction;
        let (rows, truncated) = match limit {
            Some(max) => {
                // Inside the user's transaction this BEGIN only draws a
                // "transaction already in progress" warning.
                let tx = session.client.transaction().await.map_err(pg_err)?;
                let fetch = i32::try_from(max.saturating_add(1)).unwrap_or(i32::MAX);
                let fetched = async {
                    let portal = tx.bind(&stmt, &param_refs).await?;
                    tx.query_portal(&portal, fetch).await
                }
                .await;
                if in_user_transaction {
                    // Committing or dropping (which rolls back) would end the
                    // user's transaction, so skip the wrapper's cleanup on
                    // success and failure alike.
                    std::mem::forget(tx);
                } else if fetched.is_ok() {
                    tx.commit().await.map_err(pg_err)?;
                }
                let mut rows = fetched.map_err(pg_err)?;
                let truncated = rows.len() > max;
                rows.truncate(max);
                (rows, truncated)
            }
            None => (session.client.query(&stmt, &param_refs).await.map_err(pg_err)?, false),
        };

        let origins = Self::resolve_origins(&session.client, &stmt).await?;
        let columns = stmt
            .columns()
            .iter()
            .zip(origins)
            .map(|(c, origin)| Column { name: c.name().to_string(), type_name: type_display_name(c.type_()), origin })
            .collect();
        Ok(ExecOutcome::Rows(ResultSet { columns, rows: rows.iter().map(decode_row).collect(), truncated }))
    }
}

fn decode_row(row: &Row) -> Vec<Value> {
    (0..row.len())
        .map(|i| {
            let ty = row.columns()[i].type_();
            match row.try_get::<_, decode::Raw>(i) {
                Ok(decode::Raw(Some(bytes))) => decode::decode(ty, bytes),
                Ok(decode::Raw(None)) => Value::Null,
                Err(e) => Value::Text(format!("<decode error: {e}>")),
            }
        })
        .collect()
}

#[async_trait]
impl Connection for PgConnection {
    fn dialect(&self) -> Dialect {
        Dialect::Postgres
    }

    async fn execute(&self, sql: &str, params: &[Value], max_rows: Option<usize>) -> DbResult<ExecOutcome> {
        self.run(sql, params, max_rows).await
    }

    async fn schemas(&self) -> DbResult<Vec<String>> {
        introspect::schemas(&self.session.lock().await.client).await
    }

    async fn relations(&self, schema: &str) -> DbResult<Vec<Relation>> {
        introspect::relations(&self.session.lock().await.client, schema).await
    }

    async fn table_details(&self, schema: &str, name: &str) -> DbResult<TableDetails> {
        introspect::table_details(&self.session.lock().await.client, schema, name).await
    }

    async fn referencing_keys(&self, schema: &str, name: &str) -> DbResult<Vec<dbm_core::IncomingKey>> {
        introspect::referencing_keys(&self.session.lock().await.client, schema, name).await
    }

    async fn ddl(&self, schema: &str, name: &str) -> DbResult<String> {
        introspect::ddl(&self.session.lock().await.client, schema, name).await
    }

    fn canceller(&self) -> Arc<dyn Canceller> {
        Arc::new(PgCanceller { token: self.cancel.clone(), tls: self.tls.clone() })
    }

    async fn in_transaction(&self) -> bool {
        self.session.lock().await.in_user_transaction
    }
}

struct PgCanceller {
    token: CancelToken,
    tls: MakeTlsConnector,
}

#[async_trait]
impl Canceller for PgCanceller {
    async fn cancel(&self) -> DbResult<()> {
        self.token.cancel_query(self.tls.clone()).await.map_err(pg_err)
    }
}
