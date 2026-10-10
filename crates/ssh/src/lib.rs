//! SSH tunnels: a local port on 127.0.0.1 forwarded through a bastion host to
//! the database server, so every driver connects as if the server were local.
//!
//! Each connection accepted on the local port gets its own direct-tcpip
//! channel. If the SSH session drops (bastion restart, network change), the
//! next accepted connection reconnects it.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::client::{self, AuthResult, Handle};
use russh::keys::agent::client::AgentClient;
use russh::keys::{self, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

#[derive(Debug, Clone)]
pub enum Auth {
    Password(String),
    /// An OpenSSH private key file; the passphrase when it is encrypted.
    Key {
        path: PathBuf,
        passphrase: Option<String>,
    },
    /// Keys held by the running ssh-agent (`SSH_AUTH_SOCK`).
    Agent,
}

#[derive(Debug, Clone)]
pub struct TunnelParams {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: Auth,
    /// Require the bastion's key to be in ~/.ssh/known_hosts.
    pub verify_host_key: bool,
    /// The database server as seen from the bastion.
    pub target_host: String,
    pub target_port: u16,
}

/// An open tunnel. Dropping it stops accepting new connections; connections
/// already forwarded end when the driver closes them.
pub struct Tunnel {
    local_port: u16,
    accept: JoinHandle<()>,
}

impl Tunnel {
    pub fn local_port(&self) -> u16 {
        self.local_port
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

/// Connects and authenticates to the bastion and checks that it can reach
/// the target, so a failure names the hop that failed. Errors start with
/// "SSH" to tell them apart from the database's own.
pub async fn open(params: TunnelParams) -> Result<Tunnel, String> {
    let session = connect(&params).await?;
    // Probe the second hop now; otherwise the driver would only see a closed socket.
    session
        .channel_open_direct_tcpip(params.target_host.clone(), params.target_port.into(), "127.0.0.1", 0)
        .await
        .map_err(|e| {
            format!(
                "SSH: connected to {}, but it could not reach {}:{} ({e})",
                params.host, params.target_host, params.target_port
            )
        })?
        .close()
        .await
        .ok();
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.map_err(|e| format!("SSH: no local port: {e}"))?;
    let local_port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let session = Arc::new(tokio::sync::Mutex::new(Arc::new(session)));
    let accept = tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let (session, params) = (session.clone(), params.clone());
            tokio::spawn(async move {
                // The driver sees a dropped socket if forwarding fails; nothing else to report to.
                let _ = forward(socket, &session, &params).await;
            });
        }
    });
    Ok(Tunnel { local_port, accept })
}

type Session = Handle<Client>;

async fn forward(
    mut socket: TcpStream,
    session: &tokio::sync::Mutex<Arc<Session>>,
    params: &TunnelParams,
) -> Result<(), String> {
    let handle = {
        let mut current = session.lock().await;
        if current.is_closed() {
            *current = Arc::new(connect(params).await?);
        }
        current.clone()
    };
    let channel = handle
        .channel_open_direct_tcpip(params.target_host.clone(), params.target_port.into(), "127.0.0.1", 0)
        .await
        .map_err(|e| e.to_string())?;
    socket.set_nodelay(true).ok();
    let mut stream = channel.into_stream();
    tokio::io::copy_bidirectional(&mut socket, &mut stream).await.map_err(|e| e.to_string())?;
    Ok(())
}

struct Client {
    host: String,
    port: u16,
    verify: bool,
    /// Why the host key was rejected, for the error message.
    rejection: Arc<Mutex<Option<String>>>,
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        if !self.verify {
            return Ok(true);
        }
        let reason = match keys::check_known_hosts(&self.host, self.port, &key.public_key()) {
            Ok(true) => return Ok(true),
            Ok(false) => format!(
                "SSH: {} is not in ~/.ssh/known_hosts. Connect to it once with `ssh` to add it, \
                 or turn off the host key check.",
                self.host
            ),
            Err(keys::Error::KeyChanged { line }) => format!(
                "SSH: the host key of {} does not match ~/.ssh/known_hosts (line {line}). \
                 The server was reinstalled, or someone is intercepting the connection.",
                self.host
            ),
            Err(e) => format!("SSH: could not read ~/.ssh/known_hosts: {e}"),
        };
        *self.rejection.lock().unwrap() = Some(reason);
        Ok(false)
    }
}

async fn connect(p: &TunnelParams) -> Result<Session, String> {
    let config = Arc::new(client::Config {
        inactivity_timeout: None,
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3,
        nodelay: true,
        ..Default::default()
    });
    let rejection = Arc::new(Mutex::new(None));
    let handler =
        Client { host: p.host.clone(), port: p.port, verify: p.verify_host_key, rejection: rejection.clone() };
    let connecting = client::connect(config, (p.host.as_str(), p.port), handler);
    let mut session = match tokio::time::timeout(Duration::from_secs(10), connecting).await {
        Err(_) => return Err(format!("SSH: timed out connecting to {}:{}", p.host, p.port)),
        Ok(Err(e)) => {
            return Err(rejection
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| format!("SSH: cannot connect to {}:{}: {e}", p.host, p.port)));
        }
        Ok(Ok(s)) => s,
    };
    let result = match &p.auth {
        Auth::Password(password) => session.authenticate_password(&p.user, password).await.map_err(|e| e.to_string()),
        Auth::Key { path, passphrase } => {
            let key = load_key(path, passphrase.as_deref())?;
            let hash = if key.algorithm().is_rsa() {
                session.best_supported_rsa_hash().await.map_err(|e| e.to_string())?.flatten()
            } else {
                None
            };
            session
                .authenticate_publickey(&p.user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                .await
                .map_err(|e| e.to_string())
        }
        Auth::Agent => agent_auth(&mut session, &p.user).await,
    };
    match result {
        Ok(AuthResult::Success) => Ok(session),
        Ok(AuthResult::Failure { .. }) => Err(format!("SSH: {}@{} rejected the credentials", p.user, p.host)),
        Err(e) => Err(format!("SSH: authentication failed: {e}")),
    }
}

fn load_key(path: &std::path::Path, passphrase: Option<&str>) -> Result<keys::PrivateKey, String> {
    // "~/.ssh/id_ed25519" is how people write it.
    let path = match path.strip_prefix("~") {
        Ok(rest) => std::env::home_dir().map_or_else(|| path.to_path_buf(), |home| home.join(rest)),
        Err(_) => path.to_path_buf(),
    };
    keys::load_secret_key(&path, passphrase.filter(|p| !p.is_empty())).map_err(|e| match e {
        keys::Error::KeyIsEncrypted => format!("SSH: {} is encrypted; enter its passphrase", path.display()),
        keys::Error::IO(e) => format!("SSH: cannot read {}: {e}", path.display()),
        keys::Error::SshKey(russh::keys::ssh_key::Error::Crypto) if passphrase.is_some_and(|p| !p.is_empty()) => {
            format!("SSH: wrong passphrase for {}", path.display())
        }
        e => format!("SSH: cannot use {}: {e}", path.display()),
    })
}

/// Tries each key the agent holds until one is accepted.
async fn agent_auth(session: &mut Session, user: &str) -> Result<AuthResult, String> {
    let mut agent = agent_client().await?;
    let identities = agent.request_identities().await.map_err(|e| format!("ssh-agent: {e}"))?;
    if identities.is_empty() {
        return Err("ssh-agent holds no keys; add one with `ssh-add`".into());
    }
    let mut last = Err("no key was accepted".to_string());
    for identity in identities {
        let key = identity.public_key().into_owned();
        let hash = if key.algorithm().is_rsa() {
            session.best_supported_rsa_hash().await.map_err(|e| e.to_string())?.flatten()
        } else {
            None
        };
        last = session.authenticate_publickey_with(user, key, hash, &mut agent).await.map_err(|e| e.to_string());
        if matches!(last, Ok(AuthResult::Success)) {
            break;
        }
    }
    last
}

#[cfg(unix)]
async fn agent_client() -> Result<AgentClient<tokio::net::UnixStream>, String> {
    AgentClient::connect_env().await.map_err(|e| format!("cannot reach ssh-agent (is SSH_AUTH_SOCK set?): {e}"))
}

#[cfg(windows)]
async fn agent_client() -> Result<AgentClient<impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send>, String>
{
    AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent")
        .await
        .map_err(|e| format!("cannot reach the OpenSSH agent: {e}"))
}
