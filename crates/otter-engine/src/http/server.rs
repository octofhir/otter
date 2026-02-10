//! HTTP server manager
//!
//! Manages multiple HTTP servers with hyper.

use crate::http::request;
use crate::http::service::OtterHttpService;
use dashmap::{DashMap, DashSet};
use futures_util::{SinkExt, StreamExt};
use http::header::SEC_WEBSOCKET_KEY;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ServerBuilder;
use otter_vm_runtime::{ActiveServerCount, HttpEvent, WsEvent};
use rustls::ServerConfig as RustlsServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use serde_json::Value as JsonValue;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::{TcpListener, TcpSocket};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Message, Role, WebSocketConfig};

#[cfg(unix)]
use tokio::net::UnixListener;

/// Server information returned after creation
#[derive(Clone)]
pub struct ServerInfo {
    /// Unique server ID
    pub id: u64,
    /// Actual port (may differ from requested if 0 was passed)
    pub port: Option<u16>,
    /// Hostname
    pub hostname: Option<String>,
    /// Is TLS enabled
    pub is_tls: bool,
    /// Unix socket path (if used)
    pub unix: Option<String>,
}

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
    pub fn from_pem(cert_pem: &[u8], key_pem: &[u8]) -> Result<Self, String> {
        let certs: Vec<CertificateDer<'static>> =
            rustls_pemfile::certs(&mut std::io::BufReader::new(cert_pem))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("Failed to parse certificates: {}", e))?;

        if certs.is_empty() {
            return Err("No certificates found in PEM".into());
        }

        let key = rustls_pemfile::private_key(&mut std::io::BufReader::new(key_pem))
            .map_err(|e| format!("Failed to parse private key: {}", e))?
            .ok_or_else(|| "No private key found in PEM".to_string())?;

        Ok(Self {
            cert_chain: certs,
            key,
        })
    }

    /// Build rustls ServerConfig from this TLS configuration.
    pub fn server_config(&self, enable_http2: bool) -> Result<RustlsServerConfig, String> {
        let mut config = RustlsServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(self.cert_chain.clone(), self.key.clone_key())
            .map_err(|e| format!("Failed to build TLS config: {}", e))?;

        if enable_http2 {
            config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        } else {
            config.alpn_protocols = vec![b"http/1.1".to_vec()];
        }

        Ok(config)
    }
}

/// WebSocket server configuration (Bun-compatible defaults).
#[derive(Clone)]
pub struct WebSocketServerConfig {
    pub max_payload_length: usize,
    pub backpressure_limit: usize,
    pub close_on_backpressure_limit: bool,
    pub idle_timeout: Duration,
    pub publish_to_self: bool,
    pub send_pings: bool,
}

impl Default for WebSocketServerConfig {
    fn default() -> Self {
        Self {
            max_payload_length: 1024 * 1024 * 16,
            backpressure_limit: 1024 * 1024 * 16,
            close_on_backpressure_limit: false,
            idle_timeout: Duration::from_secs(120),
            publish_to_self: false,
            send_pings: true,
        }
    }
}

/// Server creation options.
#[derive(Clone)]
pub struct ServerOptions {
    pub port: Option<u16>,
    pub hostname: Option<String>,
    pub unix: Option<String>,
    pub tls: Option<TlsConfig>,
    pub http2: bool,
    pub h2c: bool,
    pub reuse_port: bool,
    pub ipv6_only: bool,
    pub idle_timeout: Option<Duration>,
    pub ws_config: WebSocketServerConfig,
    pub ws_enabled: bool,
}

enum WsOutgoing {
    Message { message: Message, size: usize },
    Close { code: Option<u16>, reason: String },
    Ping { data: Vec<u8> },
    Pong { data: Vec<u8> },
    Terminate,
}

struct WsConnection {
    server_id: u64,
    socket_id: u64,
    sender: mpsc::UnboundedSender<WsOutgoing>,
    buffered_amount: Arc<AtomicU64>,
    subscriptions: Mutex<HashSet<String>>,
    remote_addr: Option<String>,
    ws_config: WebSocketServerConfig,
}

/// Handle to a running server
struct ServerHandle {
    /// Shutdown signal sender
    shutdown_tx: mpsc::Sender<()>,
    /// Server task handle
    _task: JoinHandle<()>,
    /// Pending HTTP request count
    pending_requests: Arc<AtomicU64>,
    /// Pending websocket count
    pending_websockets: Arc<AtomicU64>,
    /// Server info
    info: ServerInfo,
    /// WebSocket configuration
    ws_config: WebSocketServerConfig,
}

/// HTTP server manager
pub struct HttpServerManager {
    /// Active servers by ID
    servers: DashMap<u64, ServerHandle>,
    /// Next server ID
    next_id: AtomicU64,
    /// Event sender for HTTP requests
    event_tx: mpsc::UnboundedSender<HttpEvent>,
    /// Event sender for websocket events
    ws_event_tx: mpsc::UnboundedSender<WsEvent>,
    /// Active websocket connections
    ws_connections: Arc<DashMap<u64, WsConnection>>,
    /// Topic subscribers
    ws_topics: Arc<DashMap<String, DashSet<u64>>>,
    /// Next websocket ID
    ws_next_id: AtomicU64,
    /// Active server count (shared with event loop)
    active_count: ActiveServerCount,
}

impl HttpServerManager {
    /// Create a new server manager
    pub fn new(
        event_tx: mpsc::UnboundedSender<HttpEvent>,
        ws_event_tx: mpsc::UnboundedSender<WsEvent>,
        active_count: ActiveServerCount,
    ) -> Self {
        Self {
            servers: DashMap::new(),
            next_id: AtomicU64::new(1),
            event_tx,
            ws_event_tx,
            ws_connections: Arc::new(DashMap::new()),
            ws_topics: Arc::new(DashMap::new()),
            ws_next_id: AtomicU64::new(1),
            active_count,
        }
    }

    /// Create a new HTTP server
    pub async fn create_server(&self, options: ServerOptions) -> Result<ServerInfo, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let is_tls = options.tls.is_some();

        let hostname = options
            .hostname
            .clone()
            .unwrap_or_else(|| "0.0.0.0".to_string());
        let port = options.port.unwrap_or(0);

        let ws_config = options.ws_config.clone();
        let ws_enabled = options.ws_enabled;

        let builder = build_server_builder(options.http2, options.h2c, is_tls);
        let tls_acceptor = match options.tls.as_ref() {
            Some(cfg) => Some(build_tls_acceptor(cfg, options.http2)?),
            None => None,
        };

        // Create shutdown channel
        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);

        // Clone for the task
        let event_tx = self.event_tx.clone();
        let active_count = Arc::clone(&self.active_count);

        // Increment active server count
        active_count.fetch_add(1, Ordering::SeqCst);

        // Spawn server task
        let pending_requests = Arc::new(AtomicU64::new(0));
        let pending_websockets = Arc::new(AtomicU64::new(0));

        let task_pending_requests = Arc::clone(&pending_requests);

        let server_info = if let Some(unix_path) = options.unix.clone() {
            #[cfg(unix)]
            {
                if is_tls {
                    return Err("TLS is not supported with unix sockets".to_string());
                }

                let listener = UnixListener::bind(&unix_path)
                    .map_err(|e| format!("Failed to bind unix socket: {}", e))?;

                let task = tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            accept_result = listener.accept() => {
                                match accept_result {
                                    Ok((stream, _addr)) => {
                                        let io = TokioIo::new(stream);
                                        let svc = OtterHttpService::new(
                                            id,
                                            event_tx.clone(),
                                            None,
                                            false,
                                            Arc::clone(&task_pending_requests),
                                        );

                                        let builder = builder.clone();
                                        tokio::spawn(async move {
                                            let result = if ws_enabled {
                                                builder.serve_connection_with_upgrades(io, svc).await
                                            } else {
                                                builder.serve_connection(io, svc).await
                                            };
                                            if let Err(e) = result {
                                                eprintln!("HTTP connection error: {}", e);
                                            }
                                        });
                                    }
                                    Err(e) => {
                                        eprintln!("Accept error: {}", e);
                                    }
                                }
                            }
                            _ = shutdown_rx.recv() => {
                                break;
                            }
                        }
                    }

                    active_count.fetch_sub(1, Ordering::SeqCst);
                });

                self.servers.insert(
                    id,
                    ServerHandle {
                        shutdown_tx,
                        _task: task,
                        pending_requests: Arc::clone(&pending_requests),
                        pending_websockets: Arc::clone(&pending_websockets),
                        info: ServerInfo {
                            id,
                            port: None,
                            hostname: None,
                            is_tls: false,
                            unix: Some(unix_path.clone()),
                        },
                        ws_config,
                    },
                );

                ServerInfo {
                    id,
                    port: None,
                    hostname: None,
                    is_tls: false,
                    unix: Some(unix_path),
                }
            }
            #[cfg(not(unix))]
            {
                return Err("Unix sockets are not supported on this platform".to_string());
            }
        } else {
            let addr: SocketAddr = format!("{}:{}", hostname, port)
                .parse()
                .map_err(|e| format!("Invalid address: {}", e))?;

            let listener = bind_tcp_listener(addr, options.reuse_port, options.ipv6_only).await?;
            let actual_addr = listener
                .local_addr()
                .map_err(|e| format!("Failed to get address: {}", e))?;

            let task = tokio::spawn(async move {
                loop {
                    tokio::select! {
                        accept_result = listener.accept() => {
                            match accept_result {
                                Ok((stream, remote_addr)) => {
                                    let event_tx = event_tx.clone();
                                    let pending_requests = Arc::clone(&task_pending_requests);
                                    let builder = builder.clone();
                                    let tls_acceptor = tls_acceptor.clone();
                                    tokio::spawn(async move {
                                        if let Some(acceptor) = tls_acceptor {
                                            match acceptor.accept(stream).await {
                                                Ok(tls_stream) => {
                                                    let io = TokioIo::new(tls_stream);
                                                    let svc = OtterHttpService::new(
                                                        id,
                                                        event_tx,
                                                        Some(remote_addr),
                                                        true,
                                                        pending_requests,
                                                    );
                                                    let result = if ws_enabled {
                                                        builder.serve_connection_with_upgrades(io, svc).await
                                                    } else {
                                                        builder.serve_connection(io, svc).await
                                                    };
                                                    if let Err(e) = result {
                                                        eprintln!("HTTP connection error: {}", e);
                                                    }
                                                }
                                                Err(e) => {
                                                    eprintln!("TLS accept error: {}", e);
                                                }
                                            }
                                        } else {
                                            let io = TokioIo::new(stream);
                                            let svc = OtterHttpService::new(
                                                id,
                                                event_tx,
                                                Some(remote_addr),
                                                false,
                                                pending_requests,
                                            );
                                            let result = if ws_enabled {
                                                builder.serve_connection_with_upgrades(io, svc).await
                                            } else {
                                                builder.serve_connection(io, svc).await
                                            };
                                            if let Err(e) = result {
                                                eprintln!("HTTP connection error: {}", e);
                                            }
                                        }
                                    });
                                }
                                Err(e) => {
                                    eprintln!("Accept error: {}", e);
                                }
                            }
                        }
                        _ = shutdown_rx.recv() => {
                            break;
                        }
                    }
                }

                active_count.fetch_sub(1, Ordering::SeqCst);
            });

            self.servers.insert(
                id,
                ServerHandle {
                    shutdown_tx,
                    _task: task,
                    pending_requests: Arc::clone(&pending_requests),
                    pending_websockets: Arc::clone(&pending_websockets),
                    info: ServerInfo {
                        id,
                        port: Some(actual_addr.port()),
                        hostname: Some(hostname.clone()),
                        is_tls,
                        unix: None,
                    },
                    ws_config,
                },
            );

            ServerInfo {
                id,
                port: Some(actual_addr.port()),
                hostname: Some(hostname),
                is_tls,
                unix: None,
            }
        };

        Ok(server_info)
    }

    /// Upgrade an HTTP request to a websocket connection.
    pub fn upgrade_websocket(
        &self,
        server_id: u64,
        request_id: u64,
        extra_headers: HashMap<String, String>,
        data: Option<JsonValue>,
    ) -> Result<bool, String> {
        let Some(req_ref) = request::get_request(request_id) else {
            return Ok(false);
        };
        let key = req_ref
            .headers
            .get(SEC_WEBSOCKET_KEY)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_string());
        drop(req_ref);

        let Some(key) = key else {
            return Ok(false);
        };

        let Some(handle) = self.servers.get(&server_id) else {
            return Ok(false);
        };
        let ws_config = handle.ws_config.clone();
        let pending_websockets = Arc::clone(&handle.pending_websockets);
        drop(handle);

        let Some(mut req) = request::remove_request(request_id) else {
            return Ok(false);
        };

        let Some(upgrade) = req.upgrade.take() else {
            return Ok(false);
        };

        let accept_key = derive_accept_key(key.as_bytes());

        let mut headers = HashMap::new();
        headers.insert("upgrade".to_string(), "websocket".to_string());
        headers.insert("connection".to_string(), "Upgrade".to_string());
        headers.insert("sec-websocket-accept".to_string(), accept_key);
        for (key, value) in extra_headers {
            headers.insert(key, value);
        }

        let response = request::HttpResponse {
            status: 101,
            headers,
            body: Vec::new(),
        };

        if req.response_tx.send(response).is_err() {
            return Ok(false);
        }

        let socket_id = self.ws_next_id.fetch_add(1, Ordering::Relaxed);
        let (ws_tx, ws_rx) = mpsc::unbounded_channel();
        let buffered_amount = Arc::new(AtomicU64::new(0));
        let remote_addr = req
            .peer_addr
            .map(|addr| format!("{}:{}", addr.ip(), addr.port()));

        self.ws_connections.insert(
            socket_id,
            WsConnection {
                server_id,
                socket_id,
                sender: ws_tx,
                buffered_amount: Arc::clone(&buffered_amount),
                subscriptions: Mutex::new(HashSet::new()),
                remote_addr: remote_addr.clone(),
                ws_config: ws_config.clone(),
            },
        );
        pending_websockets.fetch_add(1, Ordering::Relaxed);

        let ws_event_tx = self.ws_event_tx.clone();
        let ws_connections = Arc::clone(&self.ws_connections);
        let ws_topics = Arc::clone(&self.ws_topics);

        tokio::spawn(async move {
            match upgrade.await {
                Ok(upgraded) => {
                    let upgraded = TokioIo::new(upgraded);
                    let ws_stream = WebSocketStream::from_raw_socket(
                        upgraded,
                        Role::Server,
                        Some(ws_config_to_tungstenite(&ws_config)),
                    )
                    .await;

                    let _ = ws_event_tx.send(WsEvent::Open {
                        server_id,
                        socket_id,
                        data,
                        remote_addr,
                    });

                    run_ws_connection(
                        ws_stream,
                        ws_rx,
                        ws_event_tx.clone(),
                        server_id,
                        socket_id,
                        Arc::clone(&buffered_amount),
                        ws_config.clone(),
                    )
                    .await;
                }
                Err(e) => {
                    let _ = ws_event_tx.send(WsEvent::Error {
                        server_id,
                        socket_id,
                        message: format!("WebSocket upgrade failed: {}", e),
                    });
                }
            }

            ws_connections.remove(&socket_id);
            remove_from_all_topics(&ws_topics, socket_id);
            pending_websockets.fetch_sub(1, Ordering::Relaxed);
        });

        Ok(true)
    }

    /// Send a websocket message.
    pub fn ws_send(&self, socket_id: u64, data: Vec<u8>, is_text: bool) -> i64 {
        let Some(conn) = self.ws_connections.get(&socket_id) else {
            return 0;
        };
        let size = data.len();
        let config = conn.ws_config.clone();

        let current = conn.buffered_amount.load(Ordering::Relaxed);
        if current + size as u64 > config.backpressure_limit as u64 {
            if config.close_on_backpressure_limit {
                let _ = conn.sender.send(WsOutgoing::Close {
                    code: Some(1013),
                    reason: "Backpressure limit exceeded".to_string(),
                });
            }
            return -1;
        }

        conn.buffered_amount
            .fetch_add(size as u64, Ordering::Relaxed);
        let message = if is_text {
            match String::from_utf8(data) {
                Ok(text) => Message::Text(text.into()),
                Err(_) => {
                    conn.buffered_amount
                        .fetch_sub(size as u64, Ordering::Relaxed);
                    return 0;
                }
            }
        } else {
            Message::Binary(data.into())
        };

        if conn
            .sender
            .send(WsOutgoing::Message { message, size })
            .is_err()
        {
            conn.buffered_amount
                .fetch_sub(size as u64, Ordering::Relaxed);
            return 0;
        }

        size as i64
    }

    /// Close a websocket connection.
    pub fn ws_close(&self, socket_id: u64, code: Option<u16>, reason: Option<String>) -> bool {
        let Some(conn) = self.ws_connections.get(&socket_id) else {
            return false;
        };
        let _ = conn.sender.send(WsOutgoing::Close {
            code,
            reason: reason.unwrap_or_default(),
        });
        true
    }

    /// Terminate a websocket connection.
    pub fn ws_terminate(&self, socket_id: u64) -> bool {
        let Some(conn) = self.ws_connections.get(&socket_id) else {
            return false;
        };
        let _ = conn.sender.send(WsOutgoing::Terminate);
        true
    }

    /// Send a ping to a websocket connection.
    pub fn ws_ping(&self, socket_id: u64, data: Vec<u8>) -> i64 {
        let Some(conn) = self.ws_connections.get(&socket_id) else {
            return 0;
        };
        let size = data.len();
        if conn.sender.send(WsOutgoing::Ping { data }).is_err() {
            return 0;
        }
        size as i64
    }

    /// Send a pong to a websocket connection.
    pub fn ws_pong(&self, socket_id: u64, data: Vec<u8>) -> i64 {
        let Some(conn) = self.ws_connections.get(&socket_id) else {
            return 0;
        };
        let size = data.len();
        if conn.sender.send(WsOutgoing::Pong { data }).is_err() {
            return 0;
        }
        size as i64
    }

    /// Subscribe a websocket to a topic.
    pub fn ws_subscribe(&self, socket_id: u64, topic: &str) -> bool {
        let Some(conn) = self.ws_connections.get(&socket_id) else {
            return false;
        };
        let mut subs = conn.subscriptions.lock().unwrap();
        if subs.insert(topic.to_string()) {
            let key = topic_key(conn.server_id, topic);
            let entry = self.ws_topics.entry(key).or_insert_with(DashSet::new);
            entry.insert(socket_id);
        }
        true
    }

    /// Unsubscribe a websocket from a topic.
    pub fn ws_unsubscribe(&self, socket_id: u64, topic: &str) -> bool {
        let Some(conn) = self.ws_connections.get(&socket_id) else {
            return false;
        };
        let mut subs = conn.subscriptions.lock().unwrap();
        if subs.remove(topic) {
            let key = topic_key(conn.server_id, topic);
            if let Some(entry) = self.ws_topics.get(&key) {
                entry.remove(&socket_id);
                if entry.is_empty() {
                    drop(entry);
                    self.ws_topics.remove(&key);
                }
            }
        }
        true
    }

    /// Publish a message to all subscribers of a topic.
    pub fn ws_publish(
        &self,
        server_id: u64,
        topic: &str,
        data: Vec<u8>,
        is_text: bool,
        sender: Option<u64>,
    ) -> i64 {
        let key = topic_key(server_id, topic);
        let Some(subscribers) = self.ws_topics.get(&key) else {
            return 0;
        };

        let mut sent_any = false;
        let mut backpressure = false;
        let size = data.len() as i64;

        for socket_id in subscribers.iter() {
            let socket_id = *socket_id;
            if let Some(sender_id) = sender {
                if sender_id == socket_id {
                    let Some(conn) = self.ws_connections.get(&socket_id) else {
                        continue;
                    };
                    if !conn.ws_config.publish_to_self {
                        continue;
                    }
                }
            }

            let result = self.ws_send(socket_id, data.clone(), is_text);
            if result == -1 {
                backpressure = true;
            } else if result > 0 {
                sent_any = true;
            }
        }

        drop(subscribers);

        if backpressure {
            -1
        } else if sent_any {
            size
        } else {
            0
        }
    }

    /// Get buffered amount for a websocket connection.
    pub fn ws_buffered_amount(&self, socket_id: u64) -> u64 {
        self.ws_connections
            .get(&socket_id)
            .map(|conn| conn.buffered_amount.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Get ready state for a websocket connection.
    pub fn ws_ready_state(&self, socket_id: u64) -> u8 {
        if self.ws_connections.contains_key(&socket_id) {
            1
        } else {
            3
        }
    }

    /// Get subscriber count for a topic.
    pub fn ws_subscriber_count(&self, server_id: u64, topic: &str) -> usize {
        let key = topic_key(server_id, topic);
        self.ws_topics.get(&key).map(|set| set.len()).unwrap_or(0)
    }

    /// Stop a server by ID
    pub fn stop_server(&self, id: u64) -> bool {
        if let Some((_, handle)) = self.servers.remove(&id) {
            // Send shutdown signal (non-blocking)
            let _ = handle.shutdown_tx.try_send(());
            let sockets: Vec<u64> = self
                .ws_connections
                .iter()
                .filter(|entry| entry.value().server_id == id)
                .map(|entry| *entry.key())
                .collect();
            for socket_id in sockets {
                let _ = self.ws_terminate(socket_id);
            }
            true
        } else {
            false
        }
    }

    /// Get server info by ID.
    pub fn server_info(&self, id: u64) -> Option<ServerInfo> {
        self.servers.get(&id).map(|handle| handle.info.clone())
    }

    /// Get pending request count for a server.
    pub fn pending_requests(&self, id: u64) -> Option<u64> {
        self.servers
            .get(&id)
            .map(|handle| handle.pending_requests.load(Ordering::Relaxed))
    }

    /// Get pending websocket count for a server.
    pub fn pending_websockets(&self, id: u64) -> Option<u64> {
        self.servers
            .get(&id)
            .map(|handle| handle.pending_websockets.load(Ordering::Relaxed))
    }

    /// Get the number of active servers
    #[allow(dead_code)]
    pub fn server_count(&self) -> usize {
        self.servers.len()
    }
}

fn build_tls_acceptor(config: &TlsConfig, enable_http2: bool) -> Result<TlsAcceptor, String> {
    let server_config = config.server_config(enable_http2)?;
    Ok(TlsAcceptor::from(Arc::new(server_config)))
}

fn build_server_builder(
    enable_http2: bool,
    enable_h2c: bool,
    is_tls: bool,
) -> ServerBuilder<TokioExecutor> {
    let mut builder = ServerBuilder::new(TokioExecutor::new());

    let allow_http2 = if is_tls { enable_http2 } else { enable_h2c };
    if !allow_http2 {
        builder = builder.http1_only();
    }

    builder
}

async fn bind_tcp_listener(
    addr: SocketAddr,
    reuse_port: bool,
    ipv6_only: bool,
) -> Result<TcpListener, String> {
    let socket = if addr.is_ipv6() {
        TcpSocket::new_v6().map_err(|e| format!("Failed to create IPv6 socket: {}", e))?
    } else {
        TcpSocket::new_v4().map_err(|e| format!("Failed to create IPv4 socket: {}", e))?
    };

    socket
        .set_reuseaddr(true)
        .map_err(|e| format!("Failed to set reuseaddr: {}", e))?;

    if reuse_port {
        #[cfg(all(
            unix,
            not(target_os = "solaris"),
            not(target_os = "illumos"),
            not(target_os = "cygwin"),
        ))]
        {
            socket
                .set_reuseport(true)
                .map_err(|e| format!("Failed to set reuseport: {}", e))?;
        }
    }

    if ipv6_only && addr.is_ipv6() {
        let sock_ref = socket2::SockRef::from(&socket);
        sock_ref
            .set_only_v6(true)
            .map_err(|e| format!("Failed to set ipv6Only: {}", e))?;
    }

    socket
        .bind(addr)
        .map_err(|e| format!("Failed to bind: {}", e))?;

    socket
        .listen(1024)
        .map_err(|e| format!("Failed to listen: {}", e))
}

fn ws_config_to_tungstenite(config: &WebSocketServerConfig) -> WebSocketConfig {
    let mut ws_config = WebSocketConfig::default();
    ws_config.max_message_size = Some(config.max_payload_length);
    ws_config.max_frame_size = Some(config.max_payload_length);
    ws_config.max_write_buffer_size = config.backpressure_limit;
    ws_config
}

fn topic_key(server_id: u64, topic: &str) -> String {
    format!("{}::{}", server_id, topic)
}

async fn run_ws_connection(
    mut ws_stream: WebSocketStream<TokioIo<hyper::upgrade::Upgraded>>,
    mut outgoing_rx: mpsc::UnboundedReceiver<WsOutgoing>,
    ws_event_tx: mpsc::UnboundedSender<WsEvent>,
    server_id: u64,
    socket_id: u64,
    buffered_amount: Arc<AtomicU64>,
    ws_config: WebSocketServerConfig,
) {
    let mut close_sent = false;
    let mut close_info: Option<(u16, String)> = None;
    let mut had_buffer = false;

    loop {
        tokio::select! {
            incoming = ws_stream.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        let _ = ws_event_tx.send(WsEvent::Message {
                            server_id,
                            socket_id,
                            data: text.as_bytes().to_vec(),
                            is_text: true,
                        });
                    }
                    Some(Ok(Message::Binary(data))) => {
                        let _ = ws_event_tx.send(WsEvent::Message {
                            server_id,
                            socket_id,
                            data: data.to_vec(),
                            is_text: false,
                        });
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = ws_event_tx.send(WsEvent::Ping {
                            server_id,
                            socket_id,
                            data: data.to_vec(),
                        });
                    }
                    Some(Ok(Message::Pong(data))) => {
                        let _ = ws_event_tx.send(WsEvent::Pong {
                            server_id,
                            socket_id,
                            data: data.to_vec(),
                        });
                    }
                    Some(Ok(Message::Close(frame))) => {
                        let (code, reason) = frame
                            .map(|frame| (u16::from(frame.code), frame.reason.to_string()))
                            .unwrap_or((1000, String::new()));
                        let _ = ws_event_tx.send(WsEvent::Close {
                            server_id,
                            socket_id,
                            code,
                            reason: reason.clone(),
                        });
                        close_sent = true;
                        close_info = Some((code, reason));
                        break;
                    }
                    Some(Ok(Message::Frame(_))) => {}
                    Some(Err(err)) => {
                        let _ = ws_event_tx.send(WsEvent::Error {
                            server_id,
                            socket_id,
                            message: format!("WebSocket error: {}", err),
                        });
                        break;
                    }
                    None => {
                        break;
                    }
                }
            }
            outgoing = outgoing_rx.recv() => {
                let Some(outgoing) = outgoing else {
                    break;
                };
                match outgoing {
                    WsOutgoing::Message { message, size } => {
                        if ws_stream.send(message).await.is_err() {
                            let _ = ws_event_tx.send(WsEvent::Error {
                                server_id,
                                socket_id,
                                message: "WebSocket send failed".to_string(),
                            });
                            break;
                        }
                        let previous = buffered_amount.fetch_sub(size as u64, Ordering::Relaxed);
                        let new_amount = previous.saturating_sub(size as u64);
                        if new_amount == 0 {
                            if had_buffer {
                                let _ = ws_event_tx.send(WsEvent::Drain { server_id, socket_id });
                                had_buffer = false;
                            }
                        } else {
                            had_buffer = true;
                        }
                    }
                    WsOutgoing::Ping { data } => {
                        if ws_stream.send(Message::Ping(data.into())).await.is_err() {
                            break;
                        }
                    }
                    WsOutgoing::Pong { data } => {
                        if ws_stream.send(Message::Pong(data.into())).await.is_err() {
                            break;
                        }
                    }
                    WsOutgoing::Close { code, reason } => {
                        let code = code.unwrap_or(1000);
                        let frame = CloseFrame {
                            code: CloseCode::from(code),
                            reason: reason.clone().into(),
                        };
                        let _ = ws_stream.send(Message::Close(Some(frame))).await;
                        close_info = Some((code, reason));
                        break;
                    }
                    WsOutgoing::Terminate => {
                        close_info = Some((1006, "Terminated".to_string()));
                        break;
                    }
                }
            }
        }
    }

    if !close_sent {
        let (code, reason) = close_info.unwrap_or((1006, "Closed".to_string()));
        let _ = ws_event_tx.send(WsEvent::Close {
            server_id,
            socket_id,
            code,
            reason,
        });
    }

    if ws_config.send_pings {
        // No-op: placeholder for future periodic ping implementation.
    }
}

fn remove_from_all_topics(topics: &DashMap<String, DashSet<u64>>, socket_id: u64) {
    let keys: Vec<String> = topics.iter().map(|entry| entry.key().clone()).collect();
    for key in keys {
        if let Some(entry) = topics.get(&key) {
            entry.remove(&socket_id);
            if entry.is_empty() {
                drop(entry);
                topics.remove(&key);
            }
        }
    }
}
