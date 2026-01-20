//! node:tls - TLS/SSL module for Node.js compatibility.
//!
//! This module provides TLS/SSL encrypted TCP connections using rustls.
//! It extends the net module with TLS functionality.
//!
//! ## Architecture
//!
//! - `tls.rs` - Rust implementation with `#[dive]` functions
//! - `tls.js` - JavaScript wrapper providing Node.js-compatible API
//!
//! ## Usage
//!
//! ```javascript
//! const tls = require('tls');
//!
//! // Connect to a TLS server
//! const socket = tls.connect({
//!   port: 443,
//!   host: 'example.com',
//!   rejectUnauthorized: true
//! }, () => {
//!   console.log('Connected!');
//!   socket.write('GET / HTTP/1.1\r\n\r\n');
//! });
//!
//! socket.on('data', (data) => console.log(data.toString()));
//! ```

use dashmap::DashMap;
use otter_macros::dive;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, Error as RustlsError, RootCertStore, SignatureScheme};
use serde::{Deserialize, Serialize};
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio_rustls::TlsConnector;

use std::io::Cursor;
use base64::Engine;

pub type Paw = u32;
const TLS_SOCKET_ID_BASE: u32 = 1_000_000_000;

/// Errors that can occur in TLS operations.
#[derive(Debug, Error)]
pub enum TlsError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    #[error("Resource not found: {0}")]
    NotFound(Paw),

    #[error("Invalid address: {0}")]
    InvalidAddress(String),

    #[error("TLS error: {0}")]
    Tls(String),

    #[error("Socket closed")]
    SocketClosed,

    #[error("Channel error: {0}")]
    Channel(String),

    #[error("Invalid certificate: {0}")]
    InvalidCertificate(String),

    #[error("Invalid private key: {0}")]
    InvalidPrivateKey(String),

    #[error("Root cert store error: {0}")]
    RootCertStore(String),
}

pub type TlsResult<T> = Result<T, TlsError>;

/// Events emitted by TLS operations for JavaScript consumption.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum TlsEvent {
    /// TLS handshake completed
    SecureConnect { socket_id: Paw },
    /// TLS handshake error
    SecureError { socket_id: Paw, error: String },
}

/// Command sent to a TLS socket's write task
enum TlsSocketCommand {
    Write(Vec<u8>, oneshot::Sender<io::Result<()>>),
    End,
    Destroy,
}

/// Internal representation of a TLS socket
struct TlsSocket {
    id: Paw,
    remote_addr: SocketAddr,
    local_addr: SocketAddr,
    command_tx: mpsc::UnboundedSender<TlsSocketCommand>,
    bytes_read: Arc<AtomicU32>,
    bytes_written: Arc<AtomicU32>,
}

/// TLS connection options
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TlsConnectOptions {
    pub port: u16,
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_reject_unauthorized")]
    pub reject_unauthorized: bool,
    pub ca: Option<String>,
    pub cert: Option<String>,
    pub key: Option<String>,
    pub servername: Option<String>,
}

fn default_host() -> String {
    "localhost".to_string()
}

fn default_reject_unauthorized() -> bool {
    true
}

/// Shared counter for tracking active TLS servers.
pub type ActiveTlsServerCount = Arc<AtomicU32>;

/// Global TLS manager - manages all TLS sockets.
pub struct TlsManager {
    /// Active sockets
    sockets: DashMap<Paw, Arc<TlsSocket>>,
    /// Next ID counter
    next_id: Arc<AtomicU32>,
    /// Event sender for JavaScript
    event_tx: mpsc::UnboundedSender<crate::net::NetEvent>,
    /// Active server count for keep-alive
    active_count: ActiveTlsServerCount,
}

#[derive(Debug)]
struct AcceptAllCertVerifier;

impl ServerCertVerifier for AcceptAllCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, RustlsError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::CryptoProvider::get_default()
            .map(|provider| provider.signature_verification_algorithms.supported_schemes())
            .unwrap_or_default()
    }
}

impl TlsManager {
    /// Create a new TlsManager with an event channel.
    pub fn new(event_tx: mpsc::UnboundedSender<crate::net::NetEvent>) -> Self {
        Self {
            sockets: DashMap::new(),
            // Keep TLS socket IDs in a separate range from net sockets.
            next_id: Arc::new(AtomicU32::new(TLS_SOCKET_ID_BASE)),
            event_tx,
            active_count: Arc::new(AtomicU32::new(0)),
        }
    }

    fn next_id(&self) -> Paw {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Get active server count
    pub fn active_count(&self) -> ActiveTlsServerCount {
        self.active_count.clone()
    }

    /// Build a TLS client configuration
    fn build_client_config(
        &self,
        reject_unauthorized: bool,
        ca: Option<&str>,
        cert: Option<&str>,
        key: Option<&str>,
    ) -> TlsResult<ClientConfig> {
        let client_auth = match (cert, key) {
            (Some(cert_pem), Some(key_pem)) => {
                let certs = parse_cert_chain(cert_pem)?;
                let key = parse_private_key(key_pem)?;
                Some((certs, key))
            }
            (Some(_), None) => {
                return Err(TlsError::InvalidPrivateKey(
                    "TLS key is required when cert is provided".to_string(),
                ));
            }
            (None, Some(_)) => {
                return Err(TlsError::InvalidCertificate(
                    "TLS cert is required when key is provided".to_string(),
                ));
            }
            _ => None,
        };

        if reject_unauthorized {
            let mut root_store = RootCertStore::empty();

            let native = rustls_native_certs::load_native_certs()
                .map_err(|e| TlsError::RootCertStore(e.to_string()))?;
            root_store.add_parsable_certificates(native.into_iter());

            if let Some(ca_pem) = ca {
                let certs = parse_cert_chain(ca_pem)?;
                root_store.add_parsable_certificates(certs);
            }

            let builder = ClientConfig::builder().with_root_certificates(root_store);
            return match client_auth {
                Some((certs, key)) => builder
                    .with_client_auth_cert(certs, key)
                    .map_err(|e| TlsError::InvalidPrivateKey(e.to_string())),
                None => Ok(builder.with_no_client_auth()),
            };
        }

        let builder = ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAllCertVerifier));

        match client_auth {
            Some((certs, key)) => builder
                .with_client_auth_cert(certs, key)
                .map_err(|e| TlsError::InvalidPrivateKey(e.to_string())),
            None => Ok(builder.with_no_client_auth()),
        }
    }

    /// Connect to a remote TLS server
    pub async fn connect(&self, options: TlsConnectOptions) -> TlsResult<Paw> {
        let TlsConnectOptions {
            port,
            host,
            reject_unauthorized,
            ca,
            cert,
            key,
            servername,
        } = options;

        let addr = format!("{}:{}", host, port);
        let stream = TcpStream::connect(&addr).await?;

        let peer_addr = stream.peer_addr()?;
        let local_addr = stream.local_addr()?;

        let config = self.build_client_config(reject_unauthorized, ca.as_deref(), cert.as_deref(), key.as_deref())?;
        let connector = TlsConnector::from(Arc::new(config));

        let server_name = servername.clone().unwrap_or_else(|| host.clone());
        let domain = ServerName::try_from(server_name)
            .map_err(|e| TlsError::Tls(format!("Invalid server name: {}", e)))?;

        let socket_id = self.next_id();
        let event_tx = self.event_tx.clone();

        let (command_tx, command_rx) = mpsc::unbounded_channel();

        let bytes_read = Arc::new(AtomicU32::new(0));
        let bytes_written = Arc::new(AtomicU32::new(0));

        let socket = Arc::new(TlsSocket {
            id: socket_id,
            remote_addr: peer_addr,
            local_addr,
            command_tx,
            bytes_read: bytes_read.clone(),
            bytes_written: bytes_written.clone(),
        });

        self.sockets.insert(socket_id, socket);

        tokio::spawn(async move {
            let tls_stream = match connector.connect(domain, stream).await {
                Ok(s) => s,
                Err(e) => {
                    let _ = event_tx.send(crate::net::NetEvent::SocketError {
                        socket_id,
                        error: e.to_string(),
                    });
                    return;
                }
            };

            let _ = event_tx.send(crate::net::NetEvent::SocketConnect { socket_id });

            Self::handle_tls_socket(
                socket_id,
                tls_stream,
                command_rx,
                event_tx,
                bytes_read,
                bytes_written,
            )
            .await;
        });

        Ok(socket_id)
    }

    /// Handle a TLS socket's read/write operations
    async fn handle_tls_socket(
        socket_id: Paw,
        stream: tokio_rustls::client::TlsStream<TcpStream>,
        mut command_rx: mpsc::UnboundedReceiver<TlsSocketCommand>,
        event_tx: mpsc::UnboundedSender<crate::net::NetEvent>,
        bytes_read: Arc<AtomicU32>,
        bytes_written: Arc<AtomicU32>,
    ) {
        let (mut reader, mut writer) = tokio::io::split(stream);
        let mut buf = vec![0u8; 64 * 1024];
        let mut had_error = false;

        loop {
            tokio::select! {
                result = reader.read(&mut buf) => {
                    match result {
                        Ok(0) => {
                            let _ = event_tx.send(crate::net::NetEvent::SocketEnd { socket_id });
                            break;
                        }
                        Ok(n) => {
                            bytes_read.fetch_add(n as u32, Ordering::Relaxed);
                            let _ = event_tx.send(crate::net::NetEvent::SocketData {
                                socket_id,
                                data: buf[..n].to_vec(),
                            });
                        }
                        Err(e) => {
                            had_error = true;
                            let _ = event_tx.send(crate::net::NetEvent::SocketError {
                                socket_id,
                                error: e.to_string(),
                            });
                            break;
                        }
                    }
                }
                cmd = command_rx.recv() => {
                    match cmd {
                        Some(TlsSocketCommand::Write(data, response_tx)) => {
                            let result = writer.write_all(&data).await;
                            if result.is_ok() {
                                let _ = writer.flush().await;
                                bytes_written.fetch_add(data.len() as u32, Ordering::Relaxed);
                            }
                            let _ = response_tx.send(result);
                        }
                        Some(TlsSocketCommand::End) => {
                            let _ = writer.shutdown().await;
                        }
                        Some(TlsSocketCommand::Destroy) | None => {
                            break;
                        }
                    }
                }
            }
        }

        let _ = event_tx.send(crate::net::NetEvent::SocketClose {
            socket_id,
            had_error,
        });
    }

    /// Write data to a TLS socket
    pub fn socket_write(&self, socket_id: Paw, data: Vec<u8>) -> TlsResult<()> {
        let socket = self
            .sockets
            .get(&socket_id)
            .ok_or(TlsError::NotFound(socket_id))?;

        let (tx, _rx) = oneshot::channel();
        socket
            .command_tx
            .send(TlsSocketCommand::Write(data, tx))
            .map_err(|_| TlsError::SocketClosed)?;

        Ok(())
    }

    /// End a TLS socket (half-close write side)
    pub fn socket_end(&self, socket_id: Paw) -> TlsResult<()> {
        let socket = self
            .sockets
            .get(&socket_id)
            .ok_or(TlsError::NotFound(socket_id))?;

        socket
            .command_tx
            .send(TlsSocketCommand::End)
            .map_err(|_| TlsError::SocketClosed)?;
        Ok(())
    }

    /// Destroy a TLS socket immediately
    pub fn socket_destroy(&self, socket_id: Paw) -> TlsResult<()> {
        let socket = self
            .sockets
            .get(&socket_id)
            .ok_or(TlsError::NotFound(socket_id))?;

        let _ = socket.command_tx.send(TlsSocketCommand::Destroy);
        self.sockets.remove(&socket_id);
        Ok(())
    }
}

// ============================================================================
// Global TlsManager instance (thread-local per runtime)
// ============================================================================

use std::cell::RefCell;

thread_local! {
    static TLS_MANAGER: RefCell<Option<Arc<TlsManager>>> = RefCell::new(None);
}

/// Initialize TLS manager for this thread/runtime.
/// Returns active server count for keep-alive tracking.
pub fn init_tls_manager(
    event_tx: mpsc::UnboundedSender<crate::net::NetEvent>,
) -> ActiveTlsServerCount {
    TLS_MANAGER.with(|m| {
        let manager = Arc::new(TlsManager::new(event_tx));
        let active_count = manager.active_count();
        *m.borrow_mut() = Some(manager);
        active_count
    })
}

/// Get TLS manager for this thread
pub fn get_manager() -> TlsResult<Arc<TlsManager>> {
    TLS_MANAGER.with(|m| {
        m.borrow()
            .clone()
            .ok_or_else(|| TlsError::Channel("TLS manager not initialized".to_string()))
    })
}

fn parse_cert_chain(pem: &str) -> TlsResult<Vec<CertificateDer<'static>>> {
    let mut reader = Cursor::new(pem.as_bytes());
    let mut certs = Vec::new();

    for cert in rustls_pemfile::certs(&mut reader) {
        certs.push(cert.map_err(|e| TlsError::InvalidCertificate(e.to_string()))?);
    }

    if certs.is_empty() {
        return Err(TlsError::InvalidCertificate(
            "No certificates found in PEM data".to_string(),
        ));
    }

    Ok(certs)
}

fn parse_private_key(pem: &str) -> TlsResult<PrivateKeyDer<'static>> {
    let mut reader = Cursor::new(pem.as_bytes());
    let key = rustls_pemfile::private_key(&mut reader)
        .map_err(|e| TlsError::InvalidPrivateKey(e.to_string()))?;

    key.ok_or_else(|| {
        TlsError::InvalidPrivateKey("No private key found in PEM data".to_string())
    })
}

// ============================================================================
// Dive Functions - Native ops callable from JavaScript
// ============================================================================

/// Connect to a remote TLS server.
/// Returns socket ID (paw).
#[dive(deep)]
pub async fn tls_connect(options: TlsConnectOptions) -> Result<Paw, TlsError> {
    let manager = get_manager()?;
    manager.connect(options).await
}

/// Write data to a TLS socket.
#[dive(swift)]
pub fn tls_socket_write(socket_id: Paw, data: String) -> Result<(), TlsError> {
    let manager = get_manager()?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&data)
        .map_err(|e| TlsError::Tls(format!("Invalid base64: {}", e)))?;
    manager.socket_write(socket_id, bytes)
}

/// Write raw string data to a TLS socket (UTF-8).
#[dive(swift)]
pub fn tls_socket_write_string(socket_id: Paw, data: String) -> Result<(), TlsError> {
    let manager = get_manager()?;
    manager.socket_write(socket_id, data.into_bytes())
}

/// End a TLS socket (half-close write side).
#[dive(swift)]
pub fn tls_socket_end(socket_id: Paw) -> Result<(), TlsError> {
    let manager = get_manager()?;
    manager.socket_end(socket_id)
}

/// Destroy a TLS socket immediately.
#[dive(swift)]
pub fn tls_socket_destroy(socket_id: Paw) -> Result<(), TlsError> {
    let manager = get_manager()?;
    manager.socket_destroy(socket_id)
}
