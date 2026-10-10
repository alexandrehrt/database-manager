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
    /// Colour tag shown on the connection's tabs, e.g. red for production.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<ConnColor>,
    /// Block statements and grid edits that change data or schema.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
    /// Reach the server through an SSH bastion. Ignored for SQLite.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<SshConfig>,
}

/// SSH tunnel settings. The password or key passphrase is kept in the OS
/// keychain like the database password.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: SshAuth,
    /// Require the bastion's host key to be in ~/.ssh/known_hosts.
    #[serde(default = "yes")]
    pub verify_host_key: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "lowercase")]
pub enum SshAuth {
    Password,
    /// A private key file; its passphrase, if any, is the stored secret.
    Key {
        path: PathBuf,
    },
    /// Keys held by the running ssh-agent.
    Agent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnColor {
    Red,
    Orange,
    Yellow,
    Green,
    Blue,
    Purple,
}

impl ConnColor {
    pub const ALL: [ConnColor; 6] =
        [ConnColor::Red, ConnColor::Orange, ConnColor::Yellow, ConnColor::Green, ConnColor::Blue, ConnColor::Purple];

    pub fn label(self) -> &'static str {
        match self {
            ConnColor::Red => "Red",
            ConnColor::Orange => "Orange",
            ConnColor::Yellow => "Yellow",
            ConnColor::Green => "Green",
            ConnColor::Blue => "Blue",
            ConnColor::Purple => "Purple",
        }
    }
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
        #[serde(default, skip_serializing_if = "TlsFiles::is_empty")]
        tls: TlsFiles,
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

/// libpq's `sslmode` except `allow`. `Require` encrypts without verifying
/// the server certificate, matching libpq.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SslMode {
    Disable,
    #[default]
    Prefer,
    Require,
    /// The certificate must chain to a trusted CA; the host name isn't checked.
    VerifyCa,
    /// As `VerifyCa`, and the certificate must name the host.
    VerifyFull,
}

impl SslMode {
    pub fn verifies(self) -> bool {
        matches!(self, SslMode::VerifyCa | SslMode::VerifyFull)
    }
}

/// PEM files for TLS: libpq's `sslrootcert`, `sslcert` and `sslkey`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TlsFiles {
    /// CA certificate(s) to trust instead of the system's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_cert: Option<PathBuf>,
    /// Client certificate and its private key, for servers that ask for one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_cert: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_key: Option<PathBuf>,
}

impl TlsFiles {
    pub fn is_empty(&self) -> bool {
        self.root_cert.is_none() && self.client_cert.is_none() && self.client_key.is_none()
    }
}
