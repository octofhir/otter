//! HTTP/HTTPS server implementation for Otter.serve()
//!
//! High-performance, event-driven HTTP server.

use crate::http_service::OtterHttpService;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as HttpBuilder;
use parking_lot::Mutex;
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::collections::HashMap;
use std::io::BufReader;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use tokio_rustls::TlsAcceptor;

// Re-export HttpEvent from otter-runtime for convenience
pub use otter_runtime::HttpEvent;

/// Errors that can occur in the HTTP server.
#[derive(Debug, Error)]
pub enum HttpServerError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Address parse error: {0}")]
    AddrParse(#[from] std::net::AddrParseError),

    #[error("TLS error: {0}")]
    Tls(String),

    #[error("Server not found: {0}")]
    NotFound(u64),

    #[error("Server already stopped")]
    AlreadyStopped,
}

pub type HttpServerResult<T> = Result<T, HttpServerError>;

/// TLS configuration for HTTPS support.
pub struct TlsConfig {
    cert_chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
}

impl Clone for TlsConfig {
    fn clone(&self) -> Self {
        Self {
            cert_chain: self.cert_chain.clone(),
            key: self.key.clone_key(),
        }
    }
}

impl TlsConfig {
    /// Create TLS config from PEM-encoded certificate and private key.
    pub fn from_pem(cert_pem: &[u8], key_pem: &[u8]) -> HttpServerResult<Self> {
        let certs: Vec<CertificateDer<'static>> =
            rustls_pemfile::certs(&mut BufReader::new(cert_pem))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| {
                    HttpServerError::Tls(format!("Failed to parse certificates: {}", e))
                })?;

        if certs.is_empty() {
            return Err(HttpServerError::Tls("No certificates found in PEM".into()));
        }

        let key = rustls_pemfile::private_key(&mut BufReader::new(key_pem))
            .map_err(|e| HttpServerError::Tls(format!("Failed to parse private key: {}", e)))?
            .ok_or_else(|| HttpServerError::Tls("No private key found in PEM".into()))?;

        Ok(Self {
            cert_chain: certs,
            key,
        })
    }

    /// Build rustls ServerConfig from this TLS configuration.
    /// Includes ALPN protocols for HTTP/2 and HTTP/1.1 negotiation.
    pub fn server_config(&self) -> HttpServerResult<ServerConfig> {
        let mut config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(self.cert_chain.clone(), self.key.clone_key())
            .map_err(|e| HttpServerError::Tls(format!("Failed to build TLS config: {}", e)))?;

        // Set ALPN protocols for HTTP/2 and HTTP/1.1
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

        Ok(config)
    }
}

/// Information about a running HTTP server.
#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub port: u16,
    pub hostname: String,
    pub is_tls: bool,
}

/// An HTTP server instance.
pub struct HttpServer {
    pub id: u64,
    pub port: u16,
    pub hostname: String,
    pub is_tls: bool,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl HttpServer {
    /// Stop the server gracefully.
    pub fn stop(&mut self) -> HttpServerResult<()> {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
            Ok(())
        } else {
            Err(HttpServerError::AlreadyStopped)
        }
    }

    /// Get server info.
    pub fn info(&self) -> ServerInfo {
        ServerInfo {
            port: self.port,
            hostname: self.hostname.clone(),
            is_tls: self.is_tls,
        }
    }
}

/// Manager for multiple HTTP server instances.
pub struct HttpServerManager {
    servers: Mutex<HashMap<u64, HttpServer>>,
    next_id: AtomicU64,
    /// Shared counter for active servers (can be cloned and passed to event loop)
    active_count: Arc<AtomicU64>,
}

/// Shared counter for tracking active HTTP servers.
/// Clone this and pass to the event loop to check if servers are running.
pub type ActiveServerCount = Arc<AtomicU64>;

impl HttpServerManager {
    /// Create a new server manager.
    pub fn new() -> Self {
        Self {
            servers: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            active_count: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Get a clone of the active server counter.
    /// Use this to check if any servers are running from the event loop.
    pub fn active_count(&self) -> ActiveServerCount {
        self.active_count.clone()
    }

    /// Get the current number of active servers.
    pub fn active_server_count(&self) -> u64 {
        self.active_count.load(Ordering::Relaxed)
    }

    /// Create and start a new HTTP server.
    pub async fn create(
        &self,
        port: u16,
        hostname: &str,
        tls: Option<TlsConfig>,
        event_tx: UnboundedSender<HttpEvent>,
    ) -> HttpServerResult<u64> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let addr: SocketAddr = format!("{}:{}", hostname, port).parse()?;

        let listener = TcpListener::bind(addr).await?;
        let actual_port = listener.local_addr()?.port();
        let is_tls = tls.is_some();

        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

        // Prepare TLS acceptor if configured
        let tls_acceptor = if let Some(tls_config) = tls {
            let server_config = tls_config.server_config()?;
            Some(TlsAcceptor::from(Arc::new(server_config)))
        } else {
            None
        };

        // Spawn the server task
        let server_id = id;
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => {
                        tracing::debug!(server_id, "HTTP server shutdown signal received");
                        break;
                    }
                    result = listener.accept() => {
                        match result {
                            Ok((stream, peer_addr)) => {
                                let event_tx = event_tx.clone();
                                let tls_acceptor = tls_acceptor.clone();

                                tokio::spawn(async move {
                                    if let Err(e) = serve_connection(
                                        stream,
                                        peer_addr,
                                        server_id,
                                        event_tx,
                                        tls_acceptor,
                                    ).await {
                                        tracing::warn!(
                                            server_id,
                                            peer = %peer_addr,
                                            error = %e,
                                            "Connection error"
                                        );
                                    }
                                });
                            }
                            Err(e) => {
                                tracing::error!(server_id, error = %e, "Accept error");
                            }
                        }
                    }
                }
            }
        });

        // Store server info
        let server = HttpServer {
            id,
            port: actual_port,
            hostname: hostname.to_string(),
            is_tls,
            shutdown_tx: Some(shutdown_tx),
        };

        self.servers.lock().insert(id, server);
        self.active_count.fetch_add(1, Ordering::Relaxed);

        tracing::info!(
            server_id = id,
            port = actual_port,
            hostname,
            tls = is_tls,
            "HTTP server started"
        );

        Ok(id)
    }

    /// Get server info by ID.
    pub fn info(&self, server_id: u64) -> HttpServerResult<ServerInfo> {
        self.servers
            .lock()
            .get(&server_id)
            .map(|s| s.info())
            .ok_or(HttpServerError::NotFound(server_id))
    }

    /// Stop a server by ID.
    pub fn stop(&self, server_id: u64) -> HttpServerResult<()> {
        let mut servers = self.servers.lock();
        if let Some(mut server) = servers.remove(&server_id) {
            server.stop()?;
            self.active_count.fetch_sub(1, Ordering::Relaxed);
            tracing::info!(server_id, "HTTP server stopped");
            Ok(())
        } else {
            Err(HttpServerError::NotFound(server_id))
        }
    }

    /// Stop all servers.
    pub fn stop_all(&self) {
        let mut servers = self.servers.lock();
        let count = servers.len();
        for (id, mut server) in servers.drain() {
            let _ = server.stop();
            tracing::debug!(server_id = id, "HTTP server stopped");
        }
        // Reset counter (might not exactly match if some stops failed, but close enough)
        self.active_count.fetch_sub(count as u64, Ordering::Relaxed);
    }
}

impl Default for HttpServerManager {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for HttpServerManager {
    fn drop(&mut self) {
        self.stop_all();
    }
}

/// Serve a single connection (HTTP/1.1, HTTP/2, or HTTPS with ALPN).
async fn serve_connection(
    stream: tokio::net::TcpStream,
    _peer_addr: SocketAddr,
    server_id: u64,
    event_tx: UnboundedSender<HttpEvent>,
    tls_acceptor: Option<TlsAcceptor>,
) -> HttpServerResult<()> {
    let service = OtterHttpService::new(server_id, event_tx);

    // Auto builder supports both HTTP/1.1 and HTTP/2
    let builder = HttpBuilder::new(TokioExecutor::new());

    if let Some(acceptor) = tls_acceptor {
        // HTTPS connection with ALPN negotiation for HTTP/2
        let tls_stream = acceptor
            .accept(stream)
            .await
            .map_err(|e| HttpServerError::Tls(format!("TLS handshake failed: {}", e)))?;

        builder
            .serve_connection(TokioIo::new(tls_stream), service)
            .await
            .map_err(|e| HttpServerError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
    } else {
        // Plain HTTP connection (HTTP/1.1 with upgrade support, HTTP/2 via prior knowledge)
        builder
            .serve_connection(TokioIo::new(stream), service)
            .await
            .map_err(|e| HttpServerError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_server_manager_create() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let manager = HttpServerManager::new();

        let id = manager.create(0, "127.0.0.1", None, tx).await.unwrap();
        assert!(id > 0);

        let info = manager.info(id).unwrap();
        assert!(info.port > 0);
        assert_eq!(info.hostname, "127.0.0.1");
        assert!(!info.is_tls);

        manager.stop(id).unwrap();
    }

    #[test]
    fn test_tls_config_invalid() {
        let result = TlsConfig::from_pem(b"invalid", b"invalid");
        assert!(result.is_err());
    }
}
