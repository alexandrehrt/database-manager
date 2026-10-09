//! Runs all database work on a tokio runtime and reports back to the UI
//! thread through a channel, so the window never waits on the network.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Instant;

use dbm_core::config::{DataSourceConfig, DataSourceKind};
use dbm_core::{Connection, DbError, DbResult, IncomingKey, Relation, StatementResult, TableDetails, Value};
use eframe::egui;

use crate::persist;

pub type SharedConnection = Arc<dyn Connection>;

pub enum Event {
    Incoming {
        source: String,
        schema: String,
        table: String,
        result: DbResult<Vec<IncomingKey>>,
    },
    Connected {
        source: String,
        result: DbResult<SharedConnection>,
    },
    Tested {
        nonce: u64,
        result: DbResult<String>,
    },
    Schemas {
        source: String,
        result: DbResult<Vec<String>>,
    },
    Relations {
        source: String,
        schema: String,
        result: DbResult<Vec<Relation>>,
    },
    Details {
        source: String,
        schema: String,
        table: String,
        result: DbResult<TableDetails>,
    },
    ConsoleConnected {
        console: u64,
        result: DbResult<SharedConnection>,
    },
    /// `in_transaction` is the console connection's state after the statement.
    StatementDone {
        console: u64,
        run: u64,
        index: usize,
        result: StatementResult,
        in_transaction: bool,
    },
    RunFinished {
        console: u64,
        run: u64,
        in_transaction: bool,
    },
    /// A COMMIT / ROLLBACK issued from the console toolbar.
    ControlDone {
        console: u64,
        sql: &'static str,
        result: DbResult<()>,
        in_transaction: bool,
    },
    /// Grid edits of result tab `tab` were applied (or failed and were rolled back).
    EditsSubmitted {
        console: u64,
        tab: usize,
        result: Result<(), String>,
        in_transaction: bool,
    },
    Ddl {
        tab: u64,
        result: DbResult<String>,
    },
}

pub struct Worker {
    rt: tokio::runtime::Runtime,
    tx: mpsc::Sender<Event>,
    rx: mpsc::Receiver<Event>,
    ctx: egui::Context,
    /// Per data source, used by the explorer and metadata lookups.
    connections: HashMap<String, SharedConnection>,
    /// Per console, so each console's transaction is its own.
    console_connections: HashMap<u64, SharedConnection>,
}

impl Worker {
    pub fn new(ctx: egui::Context) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("db-worker")
            .build()
            .expect("failed to start tokio runtime");
        let (tx, rx) = mpsc::channel();
        Self { rt, tx, rx, ctx, connections: HashMap::new(), console_connections: HashMap::new() }
    }

    pub fn try_recv(&self) -> Option<Event> {
        self.rx.try_recv().ok()
    }

    pub fn spawn(&self, fut: impl Future<Output = Event> + Send + 'static) {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        self.rt.spawn(async move {
            // The receiver only disappears when the app is closing.
            let _ = tx.send(fut.await);
            ctx.request_repaint();
        });
    }

    pub fn connection(&self, source: &str) -> Option<SharedConnection> {
        self.connections.get(source).cloned()
    }

    pub fn register(&mut self, source: String, conn: SharedConnection) {
        self.connections.insert(source, conn);
    }

    pub fn disconnect(&mut self, source: &str) {
        self.connections.remove(source);
    }

    pub fn console_connection(&self, console: u64) -> Option<SharedConnection> {
        self.console_connections.get(&console).cloned()
    }

    pub fn register_console(&mut self, console: u64, conn: SharedConnection) {
        self.console_connections.insert(console, conn);
    }

    /// Dropping the last handle closes the connection, which rolls back any
    /// open transaction on the server.
    pub fn close_console(&mut self, console: u64) {
        self.console_connections.remove(&console);
    }

    pub fn connect_console(&self, console: u64, config: DataSourceConfig, password: Option<String>) {
        self.spawn(async move { Event::ConsoleConnected { console, result: open(&config, password).await } });
    }

    /// `password` overrides the keychain; `None` reads the saved one.
    pub fn connect(&self, config: DataSourceConfig, password: Option<String>) {
        self.spawn(async move {
            let result = open(&config, password).await;
            Event::Connected { source: config.id, result }
        });
    }

    pub fn test(&self, nonce: u64, config: DataSourceConfig, password: Option<String>) {
        self.spawn(async move {
            let started = std::time::Instant::now();
            let result = match open(&config, password).await {
                Ok(conn) => {
                    let ms = started.elapsed().as_millis();
                    Ok(format!("{} · {ms} ms", server_version(conn.as_ref()).await))
                }
                Err(e) => Err(e),
            };
            Event::Tested { nonce, result }
        });
    }

    /// Executes `statements` in order, reporting each as it finishes. Stops
    /// after the first error or once `stop` is set. With `begin_first`
    /// (manual transaction mode) a transaction is opened first unless one
    /// already is.
    #[allow(clippy::too_many_arguments)]
    pub fn run_statements(
        &self,
        conn: SharedConnection,
        console: u64,
        run: u64,
        statements: Vec<(String, Vec<Value>)>,
        max_rows: usize,
        stop: Arc<AtomicBool>,
        begin_first: bool,
    ) {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        self.rt.spawn(async move {
            if begin_first
                && !conn.in_transaction().await
                && let Err(e) = conn.execute("BEGIN", &[], None).await
            {
                let result = StatementResult { sql: "BEGIN".into(), outcome: Err(e), elapsed: Default::default() };
                let in_transaction = conn.in_transaction().await;
                let _ = tx.send(Event::StatementDone { console, run, index: 0, result, in_transaction });
                let _ = tx.send(Event::RunFinished { console, run, in_transaction });
                ctx.request_repaint();
                return;
            }
            for (index, (sql, params)) in statements.into_iter().enumerate() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let started = Instant::now();
                let outcome = conn.execute(&sql, &params, Some(max_rows)).await;
                let failed = outcome.is_err();
                let result = StatementResult { sql, outcome, elapsed: started.elapsed() };
                let in_transaction = conn.in_transaction().await;
                let _ = tx.send(Event::StatementDone { console, run, index, result, in_transaction });
                ctx.request_repaint();
                if failed {
                    break;
                }
            }
            let in_transaction = conn.in_transaction().await;
            let _ = tx.send(Event::RunFinished { console, run, in_transaction });
            ctx.request_repaint();
        });
    }

    /// Runs COMMIT or ROLLBACK without producing a results tab.
    pub fn control(&self, conn: SharedConnection, console: u64, sql: &'static str) {
        self.spawn(async move {
            let result = conn.execute(sql, &[], None).await.map(|_| ());
            Event::ControlDone { console, sql, result, in_transaction: conn.in_transaction().await }
        });
    }

    /// Applies grid edits. Outside a transaction they get one of their own, so
    /// either all apply or none; inside the user's transaction they join it
    /// and the user commits.
    pub fn submit_edits(
        &self,
        conn: SharedConnection,
        console: u64,
        tab: usize,
        statements: Vec<(String, Vec<Value>, bool)>,
    ) {
        self.spawn(async move {
            let own_transaction = !conn.in_transaction().await;
            let result = apply_edits(conn.as_ref(), statements, own_transaction).await;
            Event::EditsSubmitted { console, tab, result, in_transaction: conn.in_transaction().await }
        });
    }

    pub fn incoming(&self, conn: SharedConnection, source: String, schema: String, table: String) {
        self.spawn(async move {
            let result = conn.referencing_keys(&schema, &table).await;
            Event::Incoming { source, schema, table, result }
        });
    }

    pub fn details(&self, conn: SharedConnection, source: String, schema: String, table: String) {
        self.spawn(async move {
            let result = conn.table_details(&schema, &table).await;
            Event::Details { source, schema, table, result }
        });
    }

    pub fn cancel(&self, conn: SharedConnection) {
        let canceller = conn.canceller();
        self.rt.spawn(async move {
            // A failed cancel request leaves the query running; nothing else to do.
            let _ = canceller.cancel().await;
        });
    }

    /// Runs `f` against the source's open connection, if any.
    pub fn with_connection<F, Fut>(&self, source: &str, f: F)
    where
        F: FnOnce(SharedConnection) -> Fut,
        Fut: Future<Output = Event> + Send + 'static,
    {
        if let Some(conn) = self.connection(source) {
            self.spawn(f(conn));
        }
    }
}

async fn apply_edits(
    conn: &dyn Connection,
    statements: Vec<(String, Vec<Value>, bool)>,
    own_transaction: bool,
) -> Result<(), String> {
    if own_transaction {
        conn.execute("BEGIN", &[], None).await.map_err(|e| format!("BEGIN failed: {}", e.message))?;
    }
    for (sql, params, expect_one) in statements {
        // The reason first, then the statement: the first line is what the banner shows.
        let failure = match conn.execute(&sql, &params, None).await {
            Ok(dbm_core::ExecOutcome::Affected(n)) if expect_one && n != 1 => {
                Some(format!("affected {n} rows instead of 1; the row may have been changed or deleted meanwhile"))
            }
            Ok(_) => None,
            Err(e) => Some(match e.detail {
                Some(detail) => format!("{} ({detail})", e.message),
                None => e.message,
            }),
        };
        if let Some(reason) = failure {
            if own_transaction {
                let _ = conn.execute("ROLLBACK", &[], None).await;
                return Err(format!("Nothing was saved: {reason}\n\n{sql}"));
            }
            return Err(format!(
                "{reason}\nThe console's transaction is still open; roll back or fix and retry.\n\n{sql}"
            ));
        }
    }
    if own_transaction {
        conn.execute("COMMIT", &[], None).await.map_err(|e| format!("COMMIT failed: {}", e.message))?;
    }
    Ok(())
}

/// "PostgreSQL 16.4", "SQLite 3.46.0", "Oracle 23.5.0.24.07"; the engine name alone if unknown.
async fn server_version(conn: &dyn Connection) -> String {
    let (engine, sql) = match conn.dialect() {
        dbm_core::Dialect::Postgres => ("PostgreSQL", "SHOW server_version"),
        dbm_core::Dialect::Sqlite => ("SQLite", "SELECT sqlite_version()"),
        dbm_core::Dialect::Oracle => ("Oracle", "SELECT version_full FROM product_component_version WHERE ROWNUM = 1"),
    };
    let version = match conn.execute(sql, &[], Some(1)).await {
        Ok(dbm_core::ExecOutcome::Rows(rs)) => rs.rows.first().and_then(|r| r.first()).map(|v| v.to_string()),
        _ => None,
    };
    // Postgres appends the build (e.g. "16.4 (Debian 16.4-1)").
    match version.as_deref().and_then(|v| v.split_whitespace().next()) {
        Some(v) => format!("{engine} {v}"),
        None => engine.to_string(),
    }
}

/// The given password, else the one saved in the keychain.
async fn password_for(config: &DataSourceConfig, password: Option<String>) -> DbResult<Option<String>> {
    if password.is_some() {
        return Ok(password);
    }
    let id = config.id.clone();
    tokio::task::spawn_blocking(move || persist::load_password(&id)).await.map_err(|e| DbError::new(e.to_string()))
}

async fn open(config: &DataSourceConfig, password: Option<String>) -> DbResult<SharedConnection> {
    let conn = open_engine(config, password).await?;
    if config.read_only {
        // The engine enforces it too where it can; Oracle relies on the app's checks.
        let sql = match conn.dialect() {
            dbm_core::Dialect::Postgres => Some("SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY"),
            dbm_core::Dialect::Sqlite => Some("PRAGMA query_only = ON"),
            dbm_core::Dialect::Oracle => None,
        };
        if let Some(sql) = sql {
            conn.execute(sql, &[], None).await?;
        }
    }
    Ok(conn)
}

async fn open_engine(config: &DataSourceConfig, password: Option<String>) -> DbResult<SharedConnection> {
    match &config.kind {
        DataSourceKind::Oracle { host, port, service, user, client_dir } => {
            let password = password_for(config, password).await?;
            let conn = dbm_driver_oracle::connect(dbm_driver_oracle::OracleParams {
                host,
                port: *port,
                service,
                user,
                password: password.as_deref(),
                client_dir: client_dir.as_deref(),
            })
            .await?;
            Ok(Arc::new(conn))
        }
        DataSourceKind::Sqlite { path } => Ok(Arc::new(dbm_driver_sqlite::connect(path).await?)),
        DataSourceKind::Postgres { host, port, database, user, ssl_mode } => {
            let password = match password {
                Some(p) => Some(p),
                None => {
                    let id = config.id.clone();
                    tokio::task::spawn_blocking(move || persist::load_password(&id))
                        .await
                        .map_err(|e| DbError::new(e.to_string()))?
                }
            };
            let conn = dbm_driver_postgres::connect(dbm_driver_postgres::PgParams {
                host,
                port: *port,
                database,
                user,
                password: password.as_deref(),
                ssl_mode: *ssl_mode,
            })
            .await?;
            Ok(Arc::new(conn))
        }
    }
}
