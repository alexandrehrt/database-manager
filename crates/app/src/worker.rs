//! Runs all database work on a tokio runtime and reports back to the UI
//! thread through a channel, so the window never waits on the network.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::mpsc;

use dbm_core::config::{DataSourceConfig, DataSourceKind};
use dbm_core::{Connection, DbError, DbResult, Relation, TableDetails};
use eframe::egui;

use crate::persist;

pub type SharedConnection = Arc<dyn Connection>;

pub enum Event {
    Connected { source: String, result: DbResult<SharedConnection> },
    Tested { nonce: u64, result: DbResult<String> },
    Schemas { source: String, result: DbResult<Vec<String>> },
    Relations { source: String, schema: String, result: DbResult<Vec<Relation>> },
    Details { source: String, schema: String, table: String, result: DbResult<TableDetails> },
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
