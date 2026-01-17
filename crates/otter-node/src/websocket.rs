//! WebSocket client API compatible with the Web standard.
//!
//! Provides:
//! - `WebSocket` - Client-side WebSocket connection
//! - Events: open, message, close, error

use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// WebSocket ready states (matching Web API).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ReadyState {
    Connecting = 0,
    Open = 1,
    Closing = 2,
    Closed = 3,
}

/// WebSocket events sent to JavaScript.
#[derive(Debug, Clone)]
pub enum WebSocketEvent {
    Open,
    Message(WebSocketMessage),
    Close { code: u16, reason: String },
    Error(String),
}

/// WebSocket message types.
#[derive(Debug, Clone)]
pub enum WebSocketMessage {
    Text(String),
    Binary(Vec<u8>),
}

/// Errors that can occur in WebSocket operations.
#[derive(Error, Debug)]
pub enum WebSocketError {
    #[error("Connection failed: {0}")]
    ConnectionFailed(String),

    #[error("Invalid URL: {0}")]
    InvalidUrl(String),

    #[error("Send failed: {0}")]
    SendFailed(String),

    #[error("WebSocket is not open")]
    NotOpen,

    #[error("Internal error: {0}")]
    Internal(String),
}

/// WebSocket connection handle.
///
/// This is the Rust-side handle for a WebSocket connection.
/// Events are sent through a channel to be consumed by JavaScript.
pub struct WebSocketHandle {
    id: u32,
    url: String,
    ready_state: Arc<Mutex<ReadyState>>,
    #[allow(dead_code)]
    event_tx: mpsc::UnboundedSender<(u32, WebSocketEvent)>,
    command_tx: mpsc::UnboundedSender<WebSocketCommand>,
}

/// Commands sent to the WebSocket task.
enum WebSocketCommand {
    Send(WebSocketMessage),
    Close(Option<u16>, Option<String>),
}

impl WebSocketHandle {
    /// Get the WebSocket ID.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Get the URL.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Get the current ready state.
    pub fn ready_state(&self) -> ReadyState {
        *self.ready_state.lock()
    }

    /// Send a text message.
    pub fn send_text(&self, data: &str) -> Result<(), WebSocketError> {
        if self.ready_state() != ReadyState::Open {
            return Err(WebSocketError::NotOpen);
        }
        self.command_tx
            .send(WebSocketCommand::Send(WebSocketMessage::Text(
                data.to_string(),
            )))
            .map_err(|e| WebSocketError::SendFailed(e.to_string()))
    }

    /// Send a binary message.
    pub fn send_binary(&self, data: Vec<u8>) -> Result<(), WebSocketError> {
        if self.ready_state() != ReadyState::Open {
            return Err(WebSocketError::NotOpen);
        }
        self.command_tx
            .send(WebSocketCommand::Send(WebSocketMessage::Binary(data)))
            .map_err(|e| WebSocketError::SendFailed(e.to_string()))
    }

    /// Close the connection.
    pub fn close(&self, code: Option<u16>, reason: Option<String>) -> Result<(), WebSocketError> {
        let state = self.ready_state();
        if state == ReadyState::Closed || state == ReadyState::Closing {
            return Ok(());
        }
        *self.ready_state.lock() = ReadyState::Closing;
        self.command_tx
            .send(WebSocketCommand::Close(code, reason))
            .map_err(|e| WebSocketError::Internal(e.to_string()))
    }
}

/// WebSocket connection manager.
///
/// Manages multiple WebSocket connections and routes events.
pub struct WebSocketManager {
    next_id: std::sync::atomic::AtomicU32,
    connections: Arc<Mutex<std::collections::HashMap<u32, WebSocketHandle>>>,
    event_tx: mpsc::UnboundedSender<(u32, WebSocketEvent)>,
    event_rx: Arc<Mutex<mpsc::UnboundedReceiver<(u32, WebSocketEvent)>>>,
}

impl Default for WebSocketManager {
    fn default() -> Self {
        Self::new()
    }
}

impl WebSocketManager {
    /// Create a new WebSocket manager.
    pub fn new() -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        Self {
            next_id: std::sync::atomic::AtomicU32::new(1),
            connections: Arc::new(Mutex::new(std::collections::HashMap::new())),
            event_tx,
            event_rx: Arc::new(Mutex::new(event_rx)),
        }
    }

    /// Connect to a WebSocket URL.
    pub fn connect(&self, url: &str) -> Result<u32, WebSocketError> {
        let url_parsed =
            url::Url::parse(url).map_err(|e| WebSocketError::InvalidUrl(e.to_string()))?;

        // Validate WebSocket URL scheme
        match url_parsed.scheme() {
            "ws" | "wss" => {}
            scheme => {
                return Err(WebSocketError::InvalidUrl(format!(
                    "Invalid scheme '{}', expected 'ws' or 'wss'",
                    scheme
                )));
            }
        }

        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let ready_state = Arc::new(Mutex::new(ReadyState::Connecting));
        let (command_tx, command_rx) = mpsc::unbounded_channel();

        let handle = WebSocketHandle {
            id,
            url: url.to_string(),
            ready_state: ready_state.clone(),
            event_tx: self.event_tx.clone(),
            command_tx,
        };

        self.connections.lock().insert(id, handle);

        // Spawn the WebSocket task
        let event_tx = self.event_tx.clone();
        let url_clone = url.to_string();
        let connections = self.connections.clone();

        tokio::spawn(async move {
            run_websocket(
                id,
                &url_clone,
                ready_state,
                event_tx,
                command_rx,
                connections,
            )
            .await;
        });

        Ok(id)
    }

    /// Send a text message to a WebSocket.
    pub fn send(&self, id: u32, data: &str) -> Result<(), WebSocketError> {
        let connections = self.connections.lock();
        let handle = connections
            .get(&id)
            .ok_or(WebSocketError::Internal("Connection not found".to_string()))?;
        handle.send_text(data)
    }

    /// Send binary data to a WebSocket.
    pub fn send_binary(&self, id: u32, data: Vec<u8>) -> Result<(), WebSocketError> {
        let connections = self.connections.lock();
        let handle = connections
            .get(&id)
            .ok_or(WebSocketError::Internal("Connection not found".to_string()))?;
        handle.send_binary(data)
    }

    /// Close a WebSocket connection.
    pub fn close(
        &self,
        id: u32,
        code: Option<u16>,
        reason: Option<String>,
    ) -> Result<(), WebSocketError> {
        let connections = self.connections.lock();
        let handle = connections
            .get(&id)
            .ok_or(WebSocketError::Internal("Connection not found".to_string()))?;
        handle.close(code, reason)
    }

    /// Get the ready state of a WebSocket.
    pub fn ready_state(&self, id: u32) -> Option<ReadyState> {
        self.connections.lock().get(&id).map(|h| h.ready_state())
    }

    /// Get the URL of a WebSocket.
    pub fn url(&self, id: u32) -> Option<String> {
        self.connections.lock().get(&id).map(|h| h.url.clone())
    }

    /// Poll for events (non-blocking).
    pub fn poll_events(&self) -> Vec<(u32, WebSocketEvent)> {
        let mut events = Vec::new();
        let mut rx = self.event_rx.lock();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    /// Remove a closed connection.
    pub fn remove(&self, id: u32) {
        self.connections.lock().remove(&id);
    }
}

/// Run the WebSocket connection task.
async fn run_websocket(
    id: u32,
    url: &str,
    ready_state: Arc<Mutex<ReadyState>>,
    event_tx: mpsc::UnboundedSender<(u32, WebSocketEvent)>,
    mut command_rx: mpsc::UnboundedReceiver<WebSocketCommand>,
    connections: Arc<Mutex<std::collections::HashMap<u32, WebSocketHandle>>>,
) {
    // Try to connect
    let ws_stream = match connect_async(url).await {
        Ok((stream, _response)) => stream,
        Err(e) => {
            *ready_state.lock() = ReadyState::Closed;
            let _ = event_tx.send((id, WebSocketEvent::Error(e.to_string())));
            let _ = event_tx.send((
                id,
                WebSocketEvent::Close {
                    code: 1006,
                    reason: "Connection failed".to_string(),
                },
            ));
            connections.lock().remove(&id);
            return;
        }
    };

    // Connection successful
    *ready_state.lock() = ReadyState::Open;
    let _ = event_tx.send((id, WebSocketEvent::Open));

    let (mut write, mut read) = ws_stream.split();

    loop {
        tokio::select! {
            // Handle incoming messages
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        let _ = event_tx.send((
                            id,
                            WebSocketEvent::Message(WebSocketMessage::Text(text.to_string())),
                        ));
                    }
                    Some(Ok(Message::Binary(data))) => {
                        let _ = event_tx.send((
                            id,
                            WebSocketEvent::Message(WebSocketMessage::Binary(data.to_vec())),
                        ));
                    }
                    Some(Ok(Message::Close(frame))) => {
                        let (code, reason) = frame
                            .map(|f| (f.code.into(), f.reason.to_string()))
                            .unwrap_or((1000, String::new()));
                        *ready_state.lock() = ReadyState::Closed;
                        let _ = event_tx.send((id, WebSocketEvent::Close { code, reason }));
                        connections.lock().remove(&id);
                        return;
                    }
                    Some(Ok(Message::Ping(data))) => {
                        // Respond with pong
                        let _ = write.send(Message::Pong(data)).await;
                    }
                    Some(Ok(Message::Pong(_))) => {
                        // Ignore pong
                    }
                    Some(Ok(Message::Frame(_))) => {
                        // Raw frames - ignore
                    }
                    Some(Err(e)) => {
                        *ready_state.lock() = ReadyState::Closed;
                        let _ = event_tx.send((id, WebSocketEvent::Error(e.to_string())));
                        let _ = event_tx.send((id, WebSocketEvent::Close {
                            code: 1006,
                            reason: "Error".to_string(),
                        }));
                        connections.lock().remove(&id);
                        return;
                    }
                    None => {
                        // Stream ended
                        *ready_state.lock() = ReadyState::Closed;
                        let _ = event_tx.send((id, WebSocketEvent::Close {
                            code: 1000,
                            reason: String::new(),
                        }));
                        connections.lock().remove(&id);
                        return;
                    }
                }
            }

            // Handle outgoing commands
            cmd = command_rx.recv() => {
                match cmd {
                    Some(WebSocketCommand::Send(WebSocketMessage::Text(text))) => {
                        if let Err(e) = write.send(Message::Text(text.into())).await {
                            let _ = event_tx.send((id, WebSocketEvent::Error(e.to_string())));
                        }
                    }
                    Some(WebSocketCommand::Send(WebSocketMessage::Binary(data))) => {
                        if let Err(e) = write.send(Message::Binary(data.into())).await {
                            let _ = event_tx.send((id, WebSocketEvent::Error(e.to_string())));
                        }
                    }
                    Some(WebSocketCommand::Close(code, reason)) => {
                        let close_frame = tokio_tungstenite::tungstenite::protocol::CloseFrame {
                            code: code.unwrap_or(1000).into(),
                            reason: reason.unwrap_or_default().into(),
                        };
                        let _ = write.send(Message::Close(Some(close_frame))).await;
                        *ready_state.lock() = ReadyState::Closed;
                        let _ = event_tx.send((id, WebSocketEvent::Close {
                            code: code.unwrap_or(1000),
                            reason: String::new(),
                        }));
                        connections.lock().remove(&id);
                        return;
                    }
                    None => {
                        // Command channel closed
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ready_state_values() {
        assert_eq!(ReadyState::Connecting as u8, 0);
        assert_eq!(ReadyState::Open as u8, 1);
        assert_eq!(ReadyState::Closing as u8, 2);
        assert_eq!(ReadyState::Closed as u8, 3);
    }

    #[test]
    fn test_manager_creation() {
        let manager = WebSocketManager::new();
        assert!(manager.poll_events().is_empty());
    }

    #[test]
    fn test_invalid_url() {
        let manager = WebSocketManager::new();
        let result = manager.connect("not-a-url");
        assert!(result.is_err());
    }

    #[test]
    fn test_invalid_scheme() {
        let manager = WebSocketManager::new();
        let result = manager.connect("http://example.com");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_valid_ws_url() {
        let manager = WebSocketManager::new();
        // This will create a connection attempt but fail since server doesn't exist
        let result = manager.connect("ws://localhost:65535/nonexistent");
        // Should succeed in creating the connection attempt
        assert!(result.is_ok());

        // Give the background task time to attempt connection and fail
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Check that we got a close or error event
        let events = manager.poll_events();
        // Connection should fail, generating events
        assert!(
            !events.is_empty() || manager.ready_state(result.unwrap()) == Some(ReadyState::Closed)
        );
    }
}
