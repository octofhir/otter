//! node:dgram - UDP/datagram sockets module for Node.js compatibility.
//!
//! This module provides UDP socket functionality using the new
//! `#[dive]` macro architecture.
//!
//! ## Usage
//!
//! ```javascript
//! const dgram = require('dgram');
//!
//! // Create a UDP server
//! const server = dgram.createSocket('udp4');
//! server.on('message', (msg, rinfo) => {
//!     console.log(`server got: ${msg} from ${rinfo.address}:${rinfo.port}`);
//! });
//! server.bind(41234);
//!
//! // Create a UDP client
//! const client = dgram.createSocket('udp4');
//! client.send('Hello', 41234, 'localhost');
//! ```

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use thiserror::Error;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};

/// Paw type for resource IDs.
pub type Paw = u32;

/// Errors that can occur in dgram operations.
#[derive(Debug, Error)]
pub enum DgramError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    #[error("Socket not found: {0}")]
    NotFound(Paw),

    #[error("Invalid address: {0}")]
    InvalidAddress(String),

    #[error("Socket already bound")]
    AlreadyBound,

    #[error("Socket closed")]
    SocketClosed,

    #[error("Channel error: {0}")]
    Channel(String),
}

pub type DgramResult<T> = Result<T, DgramError>;

/// Events emitted by dgram operations for JavaScript consumption.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum DgramEvent {
    /// Socket started listening
    Listening {
        socket_id: Paw,
        port: u16,
        address: String,
        family: String,
    },
    /// Message received
    Message {
        socket_id: Paw,
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
        remote_address: String,
        remote_port: u16,
        remote_family: String,
        size: usize,
    },
    /// Socket closed
    Close { socket_id: Paw },
    /// Socket error
    Error { socket_id: Paw, error: String },
}

/// Base64 serialization for binary data
mod base64_bytes {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde::{Serialize, Serializer};

    pub fn serialize<S>(bytes: &Vec<u8>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        STANDARD.encode(bytes).serialize(serializer)
    }
}

/// Socket type (UDP4 or UDP6)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SocketType {
    Udp4,
    Udp6,
}

impl SocketType {
    pub fn family(&self) -> &'static str {
        match self {
            SocketType::Udp4 => "IPv4",
            SocketType::Udp6 => "IPv6",
        }
    }
}

/// Command sent to a socket's task
enum SocketCommand {
    Send {
        data: Vec<u8>,
        address: String,
        port: u16,
        reply: oneshot::Sender<io::Result<usize>>,
    },
    Close,
}

/// Internal representation of a UDP socket
struct UdpSocketState {
    /// Socket ID
    id: Paw,
    /// Socket type
    socket_type: SocketType,
    /// Local address (if bound)
    local_addr: Option<SocketAddr>,
    /// Command sender
    command_tx: mpsc::UnboundedSender<SocketCommand>,
    /// Messages sent counter
    bytes_sent: AtomicU32,
    /// Messages received counter
    bytes_received: AtomicU32,
}

/// Shared counter for tracking active dgram sockets.
pub type ActiveDgramSocketCount = Arc<AtomicU64>;

/// Global dgram manager - manages all UDP sockets.
pub struct DgramManager {
    /// Active sockets
    sockets: DashMap<Paw, Arc<UdpSocketState>>,
    /// Next ID counter
    next_id: Arc<AtomicU32>,
    /// Event sender for JavaScript
    event_tx: mpsc::UnboundedSender<DgramEvent>,
    /// Active socket count for keep-alive
    active_count: Arc<AtomicU64>,
}

impl DgramManager {
    /// Create a new DgramManager with an event channel.
    pub fn new(event_tx: mpsc::UnboundedSender<DgramEvent>) -> Self {
        Self {
            sockets: DashMap::new(),
            next_id: Arc::new(AtomicU32::new(1)),
            event_tx,
            active_count: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Get a clone of the active socket counter.
    pub fn active_count(&self) -> ActiveDgramSocketCount {
        self.active_count.clone()
    }

    fn next_id(&self) -> Paw {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Create a new UDP socket.
    pub fn create_socket(&self, socket_type: SocketType) -> Paw {
        let socket_id = self.next_id();
        let (command_tx, _command_rx) = mpsc::unbounded_channel();

        let socket = Arc::new(UdpSocketState {
            id: socket_id,
            socket_type,
            local_addr: None,
            command_tx,
            bytes_sent: AtomicU32::new(0),
            bytes_received: AtomicU32::new(0),
        });

        self.sockets.insert(socket_id, socket);
        socket_id
    }

    /// Bind a socket to an address and port.
    pub async fn bind(&self, socket_id: Paw, port: u16, address: &str) -> DgramResult<()> {
        let socket_state = self
            .sockets
            .get(&socket_id)
            .ok_or(DgramError::NotFound(socket_id))?;

        // Determine bind address based on socket type
        let bind_addr = if address.is_empty() || address == "0.0.0.0" || address == "::" {
            match socket_state.socket_type {
                SocketType::Udp4 => format!("0.0.0.0:{}", port),
                SocketType::Udp6 => format!("[::]:{}", port),
            }
        } else {
            format!("{}:{}", address, port)
        };

        let udp_socket = UdpSocket::bind(&bind_addr).await?;
        let local_addr = udp_socket.local_addr()?;

        // Increment active count
        self.active_count.fetch_add(1, Ordering::Relaxed);

        // Send listening event
        let _ = self.event_tx.send(DgramEvent::Listening {
            socket_id,
            port: local_addr.port(),
            address: local_addr.ip().to_string(),
            family: socket_state.socket_type.family().to_string(),
        });

        // Create a new command channel for the bound socket
        let (command_tx, command_rx) = mpsc::unbounded_channel();

        // Update socket state
        drop(socket_state);
        self.sockets.remove(&socket_id);

        let new_socket = Arc::new(UdpSocketState {
            id: socket_id,
            socket_type: SocketType::Udp4, // We'll fix this below
            local_addr: Some(local_addr),
            command_tx,
            bytes_sent: AtomicU32::new(0),
            bytes_received: AtomicU32::new(0),
        });
        self.sockets.insert(socket_id, new_socket);

        // Spawn receive loop
        let event_tx = self.event_tx.clone();
        let active_count = self.active_count.clone();
        let sockets = self.sockets.clone();

        tokio::spawn(async move {
            Self::socket_loop(
                socket_id,
                udp_socket,
                command_rx,
                event_tx,
                sockets,
                active_count,
            )
            .await;
        });

        Ok(())
    }

    /// Socket loop - handles receiving messages and sending commands.
    async fn socket_loop(
        socket_id: Paw,
        socket: UdpSocket,
        mut command_rx: mpsc::UnboundedReceiver<SocketCommand>,
        event_tx: mpsc::UnboundedSender<DgramEvent>,
        sockets: DashMap<Paw, Arc<UdpSocketState>>,
        active_count: Arc<AtomicU64>,
    ) {
        let socket = Arc::new(socket);
        let mut buf = vec![0u8; 65536]; // Max UDP packet size

        loop {
            tokio::select! {
                // Receive incoming messages
                result = socket.recv_from(&mut buf) => {
                    match result {
                        Ok((len, peer_addr)) => {
                            // Update bytes received
                            if let Some(state) = sockets.get(&socket_id) {
                                state.bytes_received.fetch_add(len as u32, Ordering::Relaxed);
                            }

                            let family = if peer_addr.is_ipv4() { "IPv4" } else { "IPv6" };

                            let _ = event_tx.send(DgramEvent::Message {
                                socket_id,
                                data: buf[..len].to_vec(),
                                remote_address: peer_addr.ip().to_string(),
                                remote_port: peer_addr.port(),
                                remote_family: family.to_string(),
                                size: len,
                            });
                        }
                        Err(e) => {
                            let _ = event_tx.send(DgramEvent::Error {
                                socket_id,
                                error: e.to_string(),
                            });
                            break;
                        }
                    }
                }
                // Handle commands
                cmd = command_rx.recv() => {
                    match cmd {
                        Some(SocketCommand::Send { data, address, port, reply }) => {
                            let target = format!("{}:{}", address, port);
                            let result = socket.send_to(&data, &target).await;
                            if let Ok(len) = &result {
                                if let Some(state) = sockets.get(&socket_id) {
                                    state.bytes_sent.fetch_add(*len as u32, Ordering::Relaxed);
                                }
                            }
                            let _ = reply.send(result);
                        }
                        Some(SocketCommand::Close) | None => {
                            break;
                        }
                    }
                }
            }
        }

        // Cleanup
        sockets.remove(&socket_id);
        active_count.fetch_sub(1, Ordering::Relaxed);
        let _ = event_tx.send(DgramEvent::Close { socket_id });
    }

    /// Send data to an address.
    pub async fn send(
        &self,
        socket_id: Paw,
        data: Vec<u8>,
        port: u16,
        address: &str,
    ) -> DgramResult<usize> {
        let socket_state = self
            .sockets
            .get(&socket_id)
            .ok_or(DgramError::NotFound(socket_id))?;

        let (reply_tx, reply_rx) = oneshot::channel();

        socket_state
            .command_tx
            .send(SocketCommand::Send {
                data,
                address: address.to_string(),
                port,
                reply: reply_tx,
            })
            .map_err(|e| DgramError::Channel(e.to_string()))?;

        reply_rx
            .await
            .map_err(|e| DgramError::Channel(e.to_string()))?
            .map_err(DgramError::Io)
    }

    /// Close a socket.
    pub fn close(&self, socket_id: Paw) -> DgramResult<()> {
        let socket_state = self
            .sockets
            .get(&socket_id)
            .ok_or(DgramError::NotFound(socket_id))?;

        let _ = socket_state.command_tx.send(SocketCommand::Close);
        Ok(())
    }

    /// Get socket address info.
    pub fn address(&self, socket_id: Paw) -> DgramResult<Option<(String, u16, String)>> {
        let socket_state = self
            .sockets
            .get(&socket_id)
            .ok_or(DgramError::NotFound(socket_id))?;

        Ok(socket_state.local_addr.map(|addr| {
            let family = if addr.is_ipv4() { "IPv4" } else { "IPv6" };
            (addr.ip().to_string(), addr.port(), family.to_string())
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_socket_type() {
        assert_eq!(SocketType::Udp4.family(), "IPv4");
        assert_eq!(SocketType::Udp6.family(), "IPv6");
    }

    #[test]
    fn test_dgram_event_serialization() {
        let event = DgramEvent::Listening {
            socket_id: 1,
            port: 8080,
            address: "0.0.0.0".to_string(),
            family: "IPv4".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("listening"));
        assert!(json.contains("8080"));
    }

    #[test]
    fn test_dgram_error_display() {
        let err = DgramError::NotFound(42);
        assert_eq!(err.to_string(), "Socket not found: 42");
    }

    #[tokio::test]
    async fn test_create_socket() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let manager = DgramManager::new(tx);

        let socket_id = manager.create_socket(SocketType::Udp4);
        assert!(socket_id > 0);
    }
}
