use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::Dialect;

/// A saved connection. Passwords are not part of it; the app keeps them in
/// the OS keychain under [`DataSourceConfig::id`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DataSourceConfig {
    pub id: String,
    pub name: String,
    pub kind: DataSourceKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum DataSourceKind {
    Postgres {
        host: String,
        port: u16,
        database: String,
        user: String,
        #[serde(default)]
        ssl_mode: SslMode,
    },
    Sqlite {
        path: PathBuf,
    },
    /// Needs Oracle Instant Client; `client_dir` points at it when it isn't
    /// on the system library path.
    Oracle {
        host: String,
        port: u16,
        service: String,
        user: String,
        #[serde(default)]
        client_dir: Option<PathBuf>,
    },
}

impl DataSourceKind {
    pub fn dialect(&self) -> Dialect {
        match self {
            DataSourceKind::Postgres { .. } => Dialect::Postgres,
            DataSourceKind::Sqlite { .. } => Dialect::Sqlite,
            DataSourceKind::Oracle { .. } => Dialect::Oracle,
        }
    }
}

/// Subset of libpq's `sslmode`. `Require` encrypts without verifying the
/// server certificate, matching libpq.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SslMode {
    Disable,
    #[default]
    Prefer,
    Require,
}
