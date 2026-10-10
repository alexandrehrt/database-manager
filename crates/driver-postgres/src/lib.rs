//! PostgreSQL implementation of [`dbm_core::Connection`] on tokio-postgres.
//!
//! Row-returning reads run through a portal so that only `max_rows + 1` rows
//! are transferred. Portals need a transaction: outside a user transaction
//! the read gets a short one of its own; inside one (tracked from the
//! statements the user runs) the portal runs in the user's transaction,
//! which is left open.

mod decode;
mod introspect;

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use dbm_core::config::{SslMode, TlsFiles};
use dbm_core::driver::is_read_only_query;
use dbm_core::{
    Canceller, Column, ColumnOrigin, Connection, DbError, DbResult, Dialect, ExecOutcome, Relation, ResultSet,
    TableDetails, Value,
};
use postgres_native_tls::MakeTlsConnector;
use tokio::sync::Mutex;
use tokio_postgres::tls::MakeTlsConnect;
use tokio_postgres::types::Type;
use tokio_postgres::{CancelToken, Client, Row, Statement};

pub struct PgConnection {
    session: Mutex<Session>,
    cancel: CancelToken,
    tls: Tls,
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
    pub tls: &'a TlsFiles,
    /// The name the server certificate must carry when `host` is not it,
    /// e.g. 127.0.0.1 at the local end of an SSH tunnel.
    pub tls_host: Option<&'a str>,
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
            SslMode::Require | SslMode::VerifyCa | SslMode::VerifyFull => tokio_postgres::config::SslMode::Require,
        });
    if let Some(pw) = p.password {
        config.password(pw);
    }
    let tls =
        Tls { inner: MakeTlsConnector::new(tls_connector(p.ssl_mode, p.tls)?), domain: p.tls_host.map(Into::into) };
    let (client, connection) =
        config.connect(tls.clone()).await.map_err(|e| tls_hint(pg_err(e), p.ssl_mode, p.tls_host.unwrap_or(p.host)))?;
    tokio::spawn(connection);
    Ok(PgConnection {
        cancel: client.cancel_token(),
        session: Mutex::new(Session { client, in_user_transaction: false }),
        tls,
    })
}

fn read(path: &Path, what: &str) -> DbResult<Vec<u8>> {
    std::fs::read(path).map_err(|e| DbError::new(format!("Cannot read the {what} {}: {e}", path.display())))
}

/// libpq semantics: prefer / require encrypt without checking the
/// certificate; verify-ca checks the chain; verify-full also the host name.
/// A CA file replaces the system's trusted roots, as `sslrootcert` does.
fn tls_connector(mode: SslMode, files: &TlsFiles) -> DbResult<native_tls::TlsConnector> {
    let mut builder = native_tls::TlsConnector::builder();
    builder.danger_accept_invalid_certs(!mode.verifies()).danger_accept_invalid_hostnames(mode != SslMode::VerifyFull);
    if mode.verifies()
        && let Some(path) = &files.root_cert
    {
        let pem = read(path, "CA certificate")?;
        let certs = pem_blocks(&pem, "CERTIFICATE");
        if certs.is_empty() {
            return Err(DbError::new(format!("{} contains no PEM certificate", path.display())));
        }
        for block in certs {
            let cert = native_tls::Certificate::from_pem(&block)
                .map_err(|e| DbError::new(format!("Invalid certificate in {}: {e}", path.display())))?;
            builder.add_root_certificate(cert);
        }
        builder.disable_built_in_roots(true);
    }
    match (&files.client_cert, &files.client_key) {
        (Some(cert), Some(key)) => {
            let (cert_pem, key_pem) = (read(cert, "client certificate")?, read(key, "client key")?);
            if pem_blocks(&key_pem, "PRIVATE KEY").is_empty() {
                return Err(DbError::new(format!(
                    "{} is not a PKCS#8 key (BEGIN PRIVATE KEY). Convert it with: \
                     openssl pkcs8 -topk8 -nocrypt -in {0} -out client.pk8",
                    key.display()
                )));
            }
            let identity = native_tls::Identity::from_pkcs8(&cert_pem, &key_pem)
                .map_err(|e| DbError::new(format!("Cannot use the client certificate: {e}")))?;
            builder.identity(identity);
        }
        (None, None) => {}
        _ => return Err(DbError::new("A client certificate needs both the certificate and its key")),
    }
    builder.build().map_err(|e| DbError::new(e.to_string()))
}

/// The PEM blocks labelled `label` (e.g. every certificate of a CA bundle).
fn pem_blocks(pem: &[u8], label: &str) -> Vec<Vec<u8>> {
    let text = String::from_utf8_lossy(pem);
    let (begin, end) = (format!("-----BEGIN {label}-----"), format!("-----END {label}-----"));
    let mut blocks = Vec::new();
    let mut rest = text.as_ref();
    while let Some(start) = rest.find(&begin) {
        let Some(stop) = rest[start..].find(&end) else { break };
        let stop = start + stop + end.len();
        blocks.push(rest.as_bytes()[start..stop].to_vec());
        rest = &rest[stop..];
    }
    blocks
}

/// Adds what to do about a certificate the handshake rejected.
fn tls_hint(mut e: DbError, mode: SslMode, host: &str) -> DbError {
    if !mode.verifies() || e.code.is_some() {
        return e;
    }
    let lower = e.message.to_lowercase();
    if lower.contains("hostname") || lower.contains("host name") || lower.contains("not valid for") {
        e.detail = Some(format!(
            "The server certificate does not name {host}. Connect with the name it was issued for, or use verify-ca."
        ));
    } else if lower.contains("tls") || lower.contains("certificate") || lower.contains("trust") {
        e.detail = Some(
            "The server certificate is not signed by a trusted CA. Choose the CA certificate file the server's \
             certificate was issued from."
                .into(),
        );
    }
    e
}

/// Hands the TLS layer the host name to verify, which differs from the
/// address connected to when going through a tunnel.
#[derive(Clone)]
struct Tls {
    inner: MakeTlsConnector,
    domain: Option<String>,
}

impl<S> MakeTlsConnect<S> for Tls
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    type Stream = postgres_native_tls::TlsStream<S>;
    type TlsConnect = postgres_native_tls::TlsConnector;
    type Error = native_tls::Error;

    fn make_tls_connect(&mut self, domain: &str) -> Result<Self::TlsConnect, Self::Error> {
        let domain = self.domain.as_deref().unwrap_or(domain);
        MakeTlsConnect::<S>::make_tls_connect(&mut self.inner, domain)
    }
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
        // tokio-postgres' own text is generic ("error performing TLS handshake"); the cause says why.
        None => {
            let mut message = e.to_string();
            let mut source = std::error::Error::source(&e);
            while let Some(cause) = source {
                let text = cause.to_string();
                if !message.contains(&text) {
                    message = format!("{message}: {text}");
                }
                source = cause.source();
            }
            DbError::new(message)
        }
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
    tls: Tls,
}

#[async_trait]
impl Canceller for PgCanceller {
    async fn cancel(&self) -> DbResult<()> {
        self.token.cancel_query(self.tls.clone()).await.map_err(pg_err)
    }
}
