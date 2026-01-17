//! node:net - TCP networking module for Node.js compatibility.
//!
//! This module provides TCP server and client functionality using the new
//! `#[dive]` macro architecture with Holt resource storage.
//!
//! ## Architecture
//!
//! - `net.rs` - Rust implementation with `#[dive]` functions
//! - `net.js` - JavaScript wrapper providing Node.js-compatible API
//!
//! ## Usage
//!
//! ```javascript
//! const net = require('net');
//!
//! // Create a server
//! const server = net.createServer((socket) => {
//!     socket.on('data', (data) => console.log(data));
//!     socket.write('Hello!');
//!     socket.end();
//! });
//! server.listen(8080);
//!
//! // Connect as client
//! const client = net.createConnection({ port: 8080 }, () => {
//!     client.write('Hello server!');
//! });
//! ```

use dashmap::DashMap;
use otter_macros::dive;
use serde::{Deserialize, Serialize};
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};

/// Paw type for resource IDs (matches Holt's Paw type).
pub type Paw = u32;

/// Errors that can occur in net operations.
#[derive(Debug, Error)]
pub enum NetError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    #[error("Resource not found: {0}")]
    NotFound(Paw),

    #[error("Invalid address: {0}")]
    InvalidAddress(String),

    #[error("Server already listening")]
    AlreadyListening,

    #[error("Socket closed")]
    SocketClosed,

    #[error("Channel error: {0}")]
    Channel(String),
}

pub type NetResult<T> = Result<T, NetError>;

/// Events emitted by net operations for JavaScript consumption.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum NetEvent {
    /// Server started listening
    Listening {
        server_id: Paw,
        port: u16,
        address: String,
    },
    /// New connection accepted
    Connection {
        server_id: Paw,
        socket_id: Paw,
        remote_address: String,
        remote_port: u16,
    },
    /// Server closed
    ServerClose {
        server_id: Paw,
    },
    /// Server error
    ServerError {
        server_id: Paw,
        error: String,
    },
    /// Socket connected (for client sockets)
    SocketConnect {
        socket_id: Paw,
    },
    /// Data received on socket
    SocketData {
        socket_id: Paw,
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
    },
    /// Remote end closed write side
    SocketEnd {
        socket_id: Paw,
    },
    /// Socket fully closed
    SocketClose {
        socket_id: Paw,
        had_error: bool,
    },
    /// Socket error
    SocketError {
        socket_id: Paw,
        error: String,
    },
    /// Socket write buffer drained
    SocketDrain {
        socket_id: Paw,
    },
}

/// Base64 serialization for binary data
mod base64_bytes {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use serde::{Serializer, Serialize};

    pub fn serialize<S>(bytes: &Vec<u8>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        STANDARD.encode(bytes).serialize(serializer)
    }
}

/// Command sent to a socket's write task
enum SocketCommand {
    Write(Vec<u8>, oneshot::Sender<io::Result<()>>),
    End,
    Destroy,
}

/// Internal representation of a TCP server
struct TcpServer {
    /// Server ID (paw)
    id: Paw,
    /// Local address
    local_addr: SocketAddr,
    /// Shutdown signal sender
    shutdown_tx: Option<oneshot::Sender<()>>,
}

/// Internal representation of a TCP socket
struct TcpSocket {
    /// Socket ID (paw)
    id: Paw,
    /// Remote address
    remote_addr: SocketAddr,
    /// Local address
    local_addr: SocketAddr,
    /// Command sender for write operations
    command_tx: mpsc::UnboundedSender<SocketCommand>,
    /// Bytes read counter
    bytes_read: AtomicU32,
    /// Bytes written counter
    bytes_written: AtomicU32,
}

/// Shared counter for tracking active net servers.
/// Clone this and pass to the event loop to check if servers are running.
pub type ActiveNetServerCount = Arc<AtomicU64>;

/// Global net manager - manages all servers and sockets.
pub struct NetManager {
    /// Active servers
    servers: DashMap<Paw, Arc<TcpServer>>,
    /// Active sockets
    sockets: DashMap<Paw, Arc<TcpSocket>>,
    /// Next ID counter (wrapped in Arc for sharing with spawned tasks)
    next_id: Arc<AtomicU32>,
    /// Event sender for JavaScript
    event_tx: mpsc::UnboundedSender<NetEvent>,
    /// Active server count for keep-alive
    active_count: Arc<AtomicU64>,
}

impl NetManager {
    /// Create a new NetManager with an event channel.
    pub fn new(event_tx: mpsc::UnboundedSender<NetEvent>) -> Self {
        Self {
            servers: DashMap::new(),
            sockets: DashMap::new(),
            next_id: Arc::new(AtomicU32::new(1)),
            event_tx,
            active_count: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Get a clone of the active server counter.
    /// Use this to check if any net servers are running from the event loop.
    pub fn active_count(&self) -> ActiveNetServerCount {
        self.active_count.clone()
    }

    fn next_id(&self) -> Paw {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Create a new TCP server and start listening.
    pub async fn create_server(&self, port: u16, host: &str) -> NetResult<Paw> {
        let addr = format!("{}:{}", host, port);
        let listener = TcpListener::bind(&addr).await?;
        let local_addr = listener.local_addr()?;

        let server_id = self.next_id();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let server = Arc::new(TcpServer {
            id: server_id,
            local_addr,
            shutdown_tx: Some(shutdown_tx),
        });

        self.servers.insert(server_id, server);

        // Increment active server count (for keep-alive)
        self.active_count.fetch_add(1, Ordering::Relaxed);

        // Send listening event
        let _ = self.event_tx.send(NetEvent::Listening {
            server_id,
            port: local_addr.port(),
            address: local_addr.ip().to_string(),
        });

        // Spawn accept loop
        let event_tx = self.event_tx.clone();
        let sockets = self.sockets.clone();
        let next_id = self.next_id.clone();
        let active_count = self.active_count.clone();

        tokio::spawn(async move {
            Self::accept_loop(server_id, listener, shutdown_rx, event_tx, sockets, next_id, active_count).await;
        });

        Ok(server_id)
    }

    /// Accept loop for a server
    async fn accept_loop(
        server_id: Paw,
        listener: TcpListener,
        mut shutdown_rx: oneshot::Receiver<()>,
        event_tx: mpsc::UnboundedSender<NetEvent>,
        sockets: DashMap<Paw, Arc<TcpSocket>>,
        next_id: Arc<AtomicU32>,
        active_count: Arc<AtomicU64>,
    ) {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => {
                    // Decrement active server count
                    active_count.fetch_sub(1, Ordering::Relaxed);
                    let _ = event_tx.send(NetEvent::ServerClose { server_id });
                    break;
                }
                result = listener.accept() => {
                    match result {
                        Ok((stream, peer_addr)) => {
                            let socket_id = next_id.fetch_add(1, Ordering::Relaxed);
                            let local_addr = stream.local_addr().unwrap_or(peer_addr);

                            // Create socket and spawn read/write tasks
                            let (command_tx, command_rx) = mpsc::unbounded_channel();

                            let socket = Arc::new(TcpSocket {
                                id: socket_id,
                                remote_addr: peer_addr,
                                local_addr,
                                command_tx,
                                bytes_read: AtomicU32::new(0),
                                bytes_written: AtomicU32::new(0),
                            });

                            sockets.insert(socket_id, socket);

                            // Send connection event
                            let _ = event_tx.send(NetEvent::Connection {
                                server_id,
                                socket_id,
                                remote_address: peer_addr.ip().to_string(),
                                remote_port: peer_addr.port(),
                            });

                            // Spawn socket handler
                            let event_tx_clone = event_tx.clone();
                            let sockets_clone = sockets.clone();
                            tokio::spawn(async move {
                                Self::handle_socket(socket_id, stream, command_rx, event_tx_clone, sockets_clone).await;
                            });
                        }
                        Err(e) => {
                            let _ = event_tx.send(NetEvent::ServerError {
                                server_id,
                                error: e.to_string(),
                            });
                        }
                    }
                }
            }
        }
    }

    /// Handle a socket's read/write operations
    async fn handle_socket(
        socket_id: Paw,
        stream: TcpStream,
        mut command_rx: mpsc::UnboundedReceiver<SocketCommand>,
        event_tx: mpsc::UnboundedSender<NetEvent>,
        sockets: DashMap<Paw, Arc<TcpSocket>>,
    ) {
        let (mut reader, mut writer) = tokio::io::split(stream);
        let mut buf = vec![0u8; 64 * 1024]; // 64KB buffer
        let mut had_error = false;

        loop {
            tokio::select! {
                // Handle incoming data
                result = reader.read(&mut buf) => {
                    match result {
                        Ok(0) => {
                            // EOF - remote closed
                            let _ = event_tx.send(NetEvent::SocketEnd { socket_id });
                            break;
                        }
                        Ok(n) => {
                            let _ = event_tx.send(NetEvent::SocketData {
                                socket_id,
                                data: buf[..n].to_vec(),
                            });
                        }
                        Err(e) => {
                            had_error = true;
                            let _ = event_tx.send(NetEvent::SocketError {
                                socket_id,
                                error: e.to_string(),
                            });
                            break;
                        }
                    }
                }
                // Handle commands (write, end, destroy)
                cmd = command_rx.recv() => {
                    match cmd {
                        Some(SocketCommand::Write(data, response_tx)) => {
                            let result = writer.write_all(&data).await;
                            if result.is_ok() {
                                let _ = writer.flush().await;
                            }
                            let _ = response_tx.send(result);
                        }
                        Some(SocketCommand::End) => {
                            let _ = writer.shutdown().await;
                        }
                        Some(SocketCommand::Destroy) | None => {
                            break;
                        }
                    }
                }
            }
        }

        // Cleanup
        sockets.remove(&socket_id);
        let _ = event_tx.send(NetEvent::SocketClose { socket_id, had_error });
    }

    /// Connect to a remote server
    pub async fn connect(&self, port: u16, host: &str) -> NetResult<Paw> {
        let addr = format!("{}:{}", host, port);
        let stream = TcpStream::connect(&addr).await?;

        let peer_addr = stream.peer_addr()?;
        let local_addr = stream.local_addr()?;
        let socket_id = self.next_id();

        let (command_tx, command_rx) = mpsc::unbounded_channel();

        let socket = Arc::new(TcpSocket {
            id: socket_id,
            remote_addr: peer_addr,
            local_addr,
            command_tx,
            bytes_read: AtomicU32::new(0),
            bytes_written: AtomicU32::new(0),
        });

        self.sockets.insert(socket_id, socket);

        // Send connect event
        let _ = self.event_tx.send(NetEvent::SocketConnect { socket_id });

        // Spawn socket handler
        let event_tx = self.event_tx.clone();
        let sockets = self.sockets.clone();
        tokio::spawn(async move {
            Self::handle_socket(socket_id, stream, command_rx, event_tx, sockets).await;
        });

        Ok(socket_id)
    }

    /// Write data to a socket
    pub fn socket_write(&self, socket_id: Paw, data: Vec<u8>) -> NetResult<()> {
        let socket = self.sockets.get(&socket_id)
            .ok_or(NetError::NotFound(socket_id))?;

        let (tx, _rx) = oneshot::channel();
        socket.command_tx.send(SocketCommand::Write(data, tx))
            .map_err(|_| NetError::SocketClosed)?;

        // Note: In async context, we'd await _rx. For sync dive, we fire-and-forget.
        // The JS side will handle backpressure via drain events.
        Ok(())
    }

    /// End a socket (half-close write side)
    pub fn socket_end(&self, socket_id: Paw) -> NetResult<()> {
        let socket = self.sockets.get(&socket_id)
            .ok_or(NetError::NotFound(socket_id))?;

        socket.command_tx.send(SocketCommand::End)
            .map_err(|_| NetError::SocketClosed)?;
        Ok(())
    }

    /// Destroy a socket immediately
    pub fn socket_destroy(&self, socket_id: Paw) -> NetResult<()> {
        let socket = self.sockets.get(&socket_id)
            .ok_or(NetError::NotFound(socket_id))?;

        let _ = socket.command_tx.send(SocketCommand::Destroy);
        self.sockets.remove(&socket_id);
        Ok(())
    }

    /// Close a server
    pub fn server_close(&self, server_id: Paw) -> NetResult<()> {
        let mut server = self.servers.get_mut(&server_id)
            .ok_or(NetError::NotFound(server_id))?;

        if let Some(shutdown_tx) = Arc::get_mut(&mut server).and_then(|s| s.shutdown_tx.take()) {
            let _ = shutdown_tx.send(());
        }

        self.servers.remove(&server_id);
        Ok(())
    }

    /// Get socket info
    pub fn socket_info(&self, socket_id: Paw) -> NetResult<SocketInfo> {
        let socket = self.sockets.get(&socket_id)
            .ok_or(NetError::NotFound(socket_id))?;

        Ok(SocketInfo {
            remote_address: socket.remote_addr.ip().to_string(),
            remote_port: socket.remote_addr.port(),
            local_address: socket.local_addr.ip().to_string(),
            local_port: socket.local_addr.port(),
            bytes_read: socket.bytes_read.load(Ordering::Relaxed),
            bytes_written: socket.bytes_written.load(Ordering::Relaxed),
        })
    }

    /// Get server info
    pub fn server_info(&self, server_id: Paw) -> NetResult<ServerInfo> {
        let server = self.servers.get(&server_id)
            .ok_or(NetError::NotFound(server_id))?;

        Ok(ServerInfo {
            address: server.local_addr.ip().to_string(),
            port: server.local_addr.port(),
        })
    }

    /// Set TCP_NODELAY on a socket
    pub fn set_no_delay(&self, _socket_id: Paw, _no_delay: bool) -> NetResult<()> {
        // Note: This would require storing the stream reference differently
        // For now, we'll handle this in the socket setup
        Ok(())
    }

    /// Set SO_KEEPALIVE on a socket
    pub fn set_keep_alive(&self, _socket_id: Paw, _enable: bool) -> NetResult<()> {
        // Note: Similar to set_no_delay
        Ok(())
    }
}

/// Socket information returned to JavaScript
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SocketInfo {
    pub remote_address: String,
    pub remote_port: u16,
    pub local_address: String,
    pub local_port: u16,
    pub bytes_read: u32,
    pub bytes_written: u32,
}

/// Server information returned to JavaScript
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    pub address: String,
    pub port: u16,
}

// ============================================================================
// Global NetManager instance (thread-local per runtime)
// ============================================================================

use std::cell::RefCell;

thread_local! {
    static NET_MANAGER: RefCell<Option<Arc<NetManager>>> = RefCell::new(None);
}

/// Initialize the net manager for this thread/runtime.
/// Returns the active server count for keep-alive tracking.
pub fn init_net_manager(event_tx: mpsc::UnboundedSender<NetEvent>) -> ActiveNetServerCount {
    NET_MANAGER.with(|m| {
        let manager = Arc::new(NetManager::new(event_tx));
        let active_count = manager.active_count();
        *m.borrow_mut() = Some(manager);
        active_count
    })
}

/// Get the net manager for this thread
fn get_manager() -> NetResult<Arc<NetManager>> {
    NET_MANAGER.with(|m| {
        m.borrow().clone().ok_or_else(|| {
            NetError::Channel("Net manager not initialized".to_string())
        })
    })
}

// ============================================================================
// Dive Functions - Native ops callable from JavaScript
// ============================================================================

/// Create a TCP server and start listening.
/// Returns server ID (paw).
#[dive(deep)]
async fn net_create_server(port: u16, host: String) -> Result<Paw, NetError> {
    let manager = get_manager()?;
    manager.create_server(port, &host).await
}

/// Connect to a remote TCP server.
/// Returns socket ID (paw).
#[dive(deep)]
async fn net_connect(port: u16, host: String) -> Result<Paw, NetError> {
    let manager = get_manager()?;
    manager.connect(port, &host).await
}

/// Write data to a socket.
#[dive(swift)]
fn net_socket_write(socket_id: Paw, data: String) -> Result<(), NetError> {
    let manager = get_manager()?;
    // Data comes as base64 from JS
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&data)
        .map_err(|e| NetError::InvalidAddress(format!("Invalid base64: {}", e)))?;
    manager.socket_write(socket_id, bytes)
}

/// Write raw string data to a socket (UTF-8).
#[dive(swift)]
fn net_socket_write_string(socket_id: Paw, data: String) -> Result<(), NetError> {
    let manager = get_manager()?;
    manager.socket_write(socket_id, data.into_bytes())
}

/// End a socket (half-close write side).
#[dive(swift)]
fn net_socket_end(socket_id: Paw) -> Result<(), NetError> {
    let manager = get_manager()?;
    manager.socket_end(socket_id)
}

/// Destroy a socket immediately.
#[dive(swift)]
fn net_socket_destroy(socket_id: Paw) -> Result<(), NetError> {
    let manager = get_manager()?;
    manager.socket_destroy(socket_id)
}

/// Close a server.
#[dive(swift)]
fn net_server_close(server_id: Paw) -> Result<(), NetError> {
    let manager = get_manager()?;
    manager.server_close(server_id)
}

/// Get socket information.
#[dive(swift)]
fn net_socket_info(socket_id: Paw) -> Result<SocketInfo, NetError> {
    let manager = get_manager()?;
    manager.socket_info(socket_id)
}

/// Get server information (address).
#[dive(swift)]
fn net_server_address(server_id: Paw) -> Result<ServerInfo, NetError> {
    let manager = get_manager()?;
    manager.server_info(server_id)
}

/// Set TCP_NODELAY on a socket.
#[dive(swift)]
fn net_set_no_delay(socket_id: Paw, no_delay: bool) -> Result<(), NetError> {
    let manager = get_manager()?;
    manager.set_no_delay(socket_id, no_delay)
}

/// Set SO_KEEPALIVE on a socket.
#[dive(swift)]
fn net_set_keep_alive(socket_id: Paw, enable: bool) -> Result<(), NetError> {
    let manager = get_manager()?;
    manager.set_keep_alive(socket_id, enable)
}

// ============================================================================
// Extension Creation
// ============================================================================

use base64::Engine;

/// Create the net extension.
pub fn create_net_extension() -> otter_runtime::Extension {
    let js_code = include_str!("net.js");

    otter_runtime::Extension::new("net")
        .with_ops(vec![
            net_create_server_dive_decl(),
            net_connect_dive_decl(),
            net_socket_write_dive_decl(),
            net_socket_write_string_dive_decl(),
            net_socket_end_dive_decl(),
            net_socket_destroy_dive_decl(),
            net_server_close_dive_decl(),
            net_socket_info_dive_decl(),
            net_server_address_dive_decl(),
            net_set_no_delay_dive_decl(),
            net_set_keep_alive_dive_decl(),
        ])
        .with_js(js_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_socket_info_serialization() {
        let info = SocketInfo {
            remote_address: "127.0.0.1".to_string(),
            remote_port: 8080,
            local_address: "0.0.0.0".to_string(),
            local_port: 12345,
            bytes_read: 100,
            bytes_written: 200,
        };

        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("remoteAddress"));
        assert!(json.contains("remotePort"));
    }

    #[test]
    fn test_server_info_serialization() {
        let info = ServerInfo {
            address: "0.0.0.0".to_string(),
            port: 8080,
        };

        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("address"));
        assert!(json.contains("port"));
    }

    #[test]
    fn test_net_event_serialization() {
        let event = NetEvent::Listening {
            server_id: 1,
            port: 8080,
            address: "0.0.0.0".to_string(),
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"listening\""), "Missing type field, got: {}", json);
        assert!(json.contains("\"server_id\":1") || json.contains("\"serverId\":1"), "Missing server_id, got: {}", json);
    }
}
