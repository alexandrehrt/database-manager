//! Runs all database work on a tokio runtime and reports back to the UI
//! thread through a channel, so the window never waits on the network.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Instant;

use dbm_core::config::{DataSourceConfig, DataSourceKind};
use dbm_core::{Connection, DbError, DbResult, Relation, StatementResult, TableDetails};
use eframe::egui;

use crate::persist;

pub type SharedConnection = Arc<dyn Connection>;

pub enum Event {
    Connected { source: String, result: DbResult<SharedConnection> },
    Tested { nonce: u64, result: DbResult<String> },
    Schemas { source: String, result: DbResult<Vec<String>> },
    Relations { source: String, schema: String, result: DbResult<Vec<Relation>> },
    Details { source: String, schema: String, table: String, result: DbResult<TableDetails> },
    StatementDone { console: u64, run: u64, index: usize, result: StatementResult },
    RunFinished { console: u64, run: u64 },
    Ddl { tab: u64, result: DbResult<String> },
}

pub struct Worker {
    rt: tokio::runtime::Runtime,
    tx: mpsc::Sender<Event>,
    rx: mpsc::Receiver<Event>,
    ctx: egui::Context,
    connections: HashMap<String, SharedConnection>,
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
        Self { rt, tx, rx, ctx, connections: HashMap::new() }
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

    /// `password` overrides the keychain; `None` reads the saved one.
    pub fn connect(&self, config: DataSourceConfig, password: Option<String>) {
        self.spawn(async move {
            let result = open(&config, password).await;
            Event::Connected { source: config.id, result }
        });
    }

    pub fn test(&self, nonce: u64, config: DataSourceConfig, password: Option<String>) {
        self.spawn(async move {
            let result = match open(&config, password).await {
                Ok(conn) => conn.schemas().await.map(|s| format!("Connected. {} schema(s) visible.", s.len())),
                Err(e) => Err(e),
            };
            Event::Tested { nonce, result }
        });
    }

    /// Executes `statements` in order, reporting each as it finishes. Stops
    /// after the first error or once `stop` is set.
    pub fn run_statements(
        &self,
        conn: SharedConnection,
        console: u64,
        run: u64,
        statements: Vec<String>,
        max_rows: usize,
        stop: Arc<AtomicBool>,
    ) {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        self.rt.spawn(async move {
            for (index, sql) in statements.into_iter().enumerate() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let started = Instant::now();
                let outcome = conn.execute(&sql, &[], Some(max_rows)).await;
                let failed = outcome.is_err();
                let result = StatementResult { sql, outcome, elapsed: started.elapsed() };
                let _ = tx.send(Event::StatementDone { console, run, index, result });
                ctx.request_repaint();
                if failed {
                    break;
                }
            }
            let _ = tx.send(Event::RunFinished { console, run });
            ctx.request_repaint();
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

async fn open(config: &DataSourceConfig, password: Option<String>) -> DbResult<SharedConnection> {
    match &config.kind {
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
