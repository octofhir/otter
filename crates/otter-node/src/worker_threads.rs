//! Node.js worker_threads API implementation for Otter.
//!
//! Provides Node.js-compatible worker threads API for running JavaScript in parallel threads.
//!
//! # Example
//!
//! ```javascript
//! const { Worker, isMainThread, parentPort, workerData } = require('worker_threads');
//!
//! if (isMainThread) {
//!     const worker = new Worker('./worker.js', { workerData: { num: 42 } });
//!     worker.on('message', (msg) => console.log('From worker:', msg));
//!     worker.postMessage('hello');
//! } else {
//!     console.log('Worker data:', workerData);
//!     parentPort.on('message', (msg) => {
//!         parentPort.postMessage(`Received: ${msg}`);
//!     });
//! }
//! ```

use otter_runtime::{
    JscConfig, JscRuntime, needs_transpilation, set_tokio_handle, transpile_typescript,
};
use parking_lot::Mutex;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::thread::JoinHandle;
use thiserror::Error;
use tokio::sync::mpsc;

/// Active worker count for event loop integration.
/// The runtime uses this to keep the event loop alive while workers are running.
pub type ActiveWorkerCount = Arc<AtomicU32>;

/// Errors that can occur during worker thread operations.
#[derive(Debug, Error)]
pub enum WorkerThreadError {
    #[error("Worker not found: {0}")]
    NotFound(u32),

    #[error("Worker already terminated")]
    Terminated,

    #[error("Failed to send message: {0}")]
    SendFailed(String),

    #[error("Failed to create worker: {0}")]
    CreationFailed(String),

    #[error("Invalid script: {0}")]
    InvalidScript(String),

    #[error("Message port not found: {0}")]
    PortNotFound(u64),

    #[error("Message port already closed")]
    PortClosed,

    #[error("Broadcast channel not found: {0}")]
    BroadcastChannelNotFound(u64),
}

/// Resource limits for worker threads.
#[derive(Debug, Clone, Default)]
pub struct ResourceLimits {
    /// Maximum size of the young generation heap in MB.
    pub max_young_generation_size_mb: Option<f64>,
    /// Maximum size of the old generation heap in MB.
    pub max_old_generation_size_mb: Option<f64>,
    /// Size of pre-allocated code range in MB.
    pub code_range_size_mb: Option<f64>,
    /// Stack size in MB.
    pub stack_size_mb: Option<f64>,
}

/// Options for creating a worker thread.
#[derive(Debug, Clone)]
pub struct WorkerThreadOptions {
    /// The script to execute (filename or code if eval is true).
    pub filename: String,
    /// Data to pass to the worker.
    pub worker_data: Value,
    /// If true, interpret filename as code to execute.
    pub eval: bool,
    /// Environment variables for the worker.
    pub env: Option<HashMap<String, String>>,
    /// Name for the worker (useful for debugging).
    pub name: Option<String>,
    /// Resource limits for the worker.
    pub resource_limits: Option<ResourceLimits>,
    /// Whether to track async resources (for async_hooks).
    pub track_unmanaged_fds: bool,
    /// Arguments to pass to worker's process.argv.
    pub argv: Vec<String>,
    /// Arguments to pass to worker's process.execArgv.
    pub exec_argv: Vec<String>,
    /// Whether stdin should be available.
    pub stdin: bool,
    /// Whether stdout should be piped.
    pub stdout: bool,
    /// Whether stderr should be piped.
    pub stderr: bool,
}

impl Default for WorkerThreadOptions {
    fn default() -> Self {
        Self {
            filename: String::new(),
            worker_data: Value::Null,
            eval: false,
            env: None,
            name: None,
            resource_limits: None,
            track_unmanaged_fds: true,
            argv: Vec::new(),
            exec_argv: Vec::new(),
            stdin: false,
            stdout: false,
            stderr: false,
        }
    }
}

/// Internal message between main thread and worker.
#[derive(Debug, Clone)]
pub enum WorkerThreadMessage {
    /// JSON-serializable data message.
    Data(Value),
    /// Error message.
    Error(String),
    /// Terminate signal.
    Terminate,
}

/// Events emitted by worker threads.
#[derive(Debug, Clone)]
pub enum WorkerThreadEvent {
    /// Worker has started executing code.
    Online { worker_id: u32 },
    /// Message received from worker.
    Message { worker_id: u32, data: Value },
    /// Message deserialization failed.
    MessageError { worker_id: u32, error: String },
    /// Uncaught exception in worker.
    Error { worker_id: u32, error: String },
    /// Worker stopped (with exit code).
    Exit { worker_id: u32, code: i32 },
    /// MessagePort received a message.
    PortMessage { port_id: u64, data: Value },
    /// MessagePort received an error.
    PortMessageError { port_id: u64, error: String },
    /// MessagePort was closed.
    PortClose { port_id: u64 },
    /// BroadcastChannel received a message.
    BroadcastMessage {
        channel_id: u64,
        name: String,
        data: Value,
    },
    /// BroadcastChannel received an error.
    BroadcastMessageError {
        channel_id: u64,
        name: String,
        error: String,
    },
}

/// Internal worker handle.
struct WorkerThreadHandle {
    id: u32,
    /// Channel to send messages to the worker.
    message_tx: mpsc::UnboundedSender<WorkerThreadMessage>,
    /// Worker's running state.
    running: Arc<AtomicBool>,
    /// Thread handle.
    #[allow(dead_code)]
    thread_handle: Option<JoinHandle<()>>,
    /// Whether this worker keeps the event loop alive.
    referenced: AtomicBool,
    /// Worker options.
    #[allow(dead_code)]
    options: WorkerThreadOptions,
}

/// A message port for bidirectional communication.
struct MessagePortHandle {
    id: u64,
    /// Paired port ID (the other end of the channel).
    pair_id: u64,
    /// Queue of pending messages.
    messages: Mutex<Vec<Value>>,
    /// Whether the port has been started (receiving messages).
    started: AtomicBool,
    /// Whether the port is closed.
    closed: AtomicBool,
    /// Whether this port keeps the event loop alive.
    referenced: AtomicBool,
}

/// A broadcast channel for one-to-many communication.
struct BroadcastChannelHandle {
    id: u64,
    /// Channel name.
    name: String,
    /// Whether the channel is closed.
    closed: AtomicBool,
    /// Whether this channel keeps the event loop alive.
    referenced: AtomicBool,
}

/// Manager for Node.js worker threads.
///
/// Handles creation, message passing, and termination of worker threads,
/// as well as MessageChannel/MessagePort and BroadcastChannel management.
pub struct WorkerThreadManager {
    /// Active workers.
    workers: Mutex<HashMap<u32, WorkerThreadHandle>>,
    /// Next worker ID.
    next_worker_id: AtomicU32,
    /// Message ports.
    ports: Mutex<HashMap<u64, MessagePortHandle>>,
    /// Next port ID.
    next_port_id: AtomicU64,
    /// Broadcast channels by name (for routing messages).
    broadcast_channels: Mutex<HashMap<String, Vec<u64>>>,
    /// Broadcast channel handles.
    broadcast_handles: Mutex<HashMap<u64, BroadcastChannelHandle>>,
    /// Next broadcast channel ID.
    next_broadcast_id: AtomicU64,
    /// Shared environment data across all workers.
    env_data: Mutex<HashMap<String, Value>>,
    /// Events from workers to be polled by main thread.
    events: Mutex<Vec<WorkerThreadEvent>>,
    /// Channel for workers to send events.
    event_tx: mpsc::UnboundedSender<WorkerThreadEvent>,
    /// Receiver for worker events.
    event_rx: Mutex<mpsc::UnboundedReceiver<WorkerThreadEvent>>,
    /// Current thread ID (main thread is 0).
    current_thread_id: AtomicU32,
    /// Set of objects marked as untransferable.
    untransferable: Mutex<std::collections::HashSet<u64>>,
    /// Next untransferable ID.
    next_untransferable_id: AtomicU64,
    /// Active worker count (shared with event loop).
    active_worker_count: ActiveWorkerCount,
}

impl WorkerThreadManager {
    /// Create a new worker thread manager.
    pub fn new() -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        Self {
            workers: Mutex::new(HashMap::new()),
            next_worker_id: AtomicU32::new(1),
            ports: Mutex::new(HashMap::new()),
            next_port_id: AtomicU64::new(1),
            broadcast_channels: Mutex::new(HashMap::new()),
            broadcast_handles: Mutex::new(HashMap::new()),
            next_broadcast_id: AtomicU64::new(1),
            env_data: Mutex::new(HashMap::new()),
            events: Mutex::new(Vec::new()),
            event_tx,
            event_rx: Mutex::new(event_rx),
            current_thread_id: AtomicU32::new(0),
            untransferable: Mutex::new(std::collections::HashSet::new()),
            next_untransferable_id: AtomicU64::new(1),
            active_worker_count: Arc::new(AtomicU32::new(0)),
        }
    }

    /// Get a reference to the active worker count.
    /// This is used by the event loop to keep alive while workers are running.
    pub fn active_count_ref(&self) -> ActiveWorkerCount {
        self.active_worker_count.clone()
    }

    /// Check if this is the main thread.
    pub fn is_main_thread(&self) -> bool {
        self.current_thread_id.load(Ordering::SeqCst) == 0
    }

    /// Get the current thread ID.
    pub fn thread_id(&self) -> u32 {
        self.current_thread_id.load(Ordering::SeqCst)
    }

    /// Create a new worker thread.
    ///
    /// Returns the worker ID.
    pub fn create(&self, options: WorkerThreadOptions) -> Result<u32, WorkerThreadError> {
        let id = self.next_worker_id.fetch_add(1, Ordering::SeqCst);
        let running = Arc::new(AtomicBool::new(true));
        let running_clone = running.clone();
        let event_tx = self.event_tx.clone();

        // Channel for messages to the worker
        let (message_tx, message_rx) = mpsc::unbounded_channel::<WorkerThreadMessage>();

        // Clone for thread
        let event_tx_clone = event_tx.clone();
        let worker_data = options.worker_data.clone();
        let filename = options.filename.clone();
        let eval_mode = options.eval;
        let active_count = self.active_worker_count.clone();

        // Increment active worker count
        active_count.fetch_add(1, Ordering::SeqCst);

        // Spawn worker thread
        let thread_name = options
            .name
            .clone()
            .unwrap_or_else(|| format!("otter-worker-thread-{}", id));

        let handle = std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                // Run the worker in a tokio runtime
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create tokio runtime for worker");

                rt.block_on(async move {
                    run_worker_thread(
                        id,
                        filename,
                        eval_mode,
                        worker_data,
                        running_clone,
                        message_rx,
                        event_tx_clone,
                    )
                    .await;
                });

                // Decrement active worker count
                active_count.fetch_sub(1, Ordering::SeqCst);

                // Send exit event
                let _ = event_tx.send(WorkerThreadEvent::Exit {
                    worker_id: id,
                    code: 0,
                });
            })
            .map_err(|e| WorkerThreadError::CreationFailed(e.to_string()))?;

        // Store the worker handle
        let worker_handle = WorkerThreadHandle {
            id,
            message_tx,
            running,
            thread_handle: Some(handle),
            referenced: AtomicBool::new(true),
            options,
        };

        self.workers.lock().insert(id, worker_handle);

        Ok(id)
    }

    /// Post a message to a worker.
    pub fn post_message(
        &self,
        id: u32,
        data: Value,
        _transfer_list: Option<Vec<Value>>,
    ) -> Result<(), WorkerThreadError> {
        let workers = self.workers.lock();
        let worker = workers.get(&id).ok_or(WorkerThreadError::NotFound(id))?;

        if !worker.running.load(Ordering::SeqCst) {
            return Err(WorkerThreadError::Terminated);
        }

        worker
            .message_tx
            .send(WorkerThreadMessage::Data(data))
            .map_err(|e| WorkerThreadError::SendFailed(e.to_string()))
    }

    /// Terminate a worker.
    ///
    /// Returns a future that resolves when the worker has terminated.
    pub fn terminate(&self, id: u32) -> Result<(), WorkerThreadError> {
        let workers = self.workers.lock();
        let worker = workers.get(&id).ok_or(WorkerThreadError::NotFound(id))?;

        worker.running.store(false, Ordering::SeqCst);
        let _ = worker.message_tx.send(WorkerThreadMessage::Terminate);

        Ok(())
    }

    /// Mark a worker as referenced (keeps event loop alive).
    pub fn ref_worker(&self, id: u32) {
        if let Some(worker) = self.workers.lock().get(&id) {
            worker.referenced.store(true, Ordering::SeqCst);
        }
    }

    /// Mark a worker as unreferenced (allows event loop to exit).
    pub fn unref_worker(&self, id: u32) {
        if let Some(worker) = self.workers.lock().get(&id) {
            worker.referenced.store(false, Ordering::SeqCst);
        }
    }

    /// Check if a worker is running.
    pub fn is_running(&self, id: u32) -> bool {
        self.workers
            .lock()
            .get(&id)
            .is_some_and(|w| w.running.load(Ordering::SeqCst))
    }

    /// Get the resource limits for a worker.
    pub fn get_resource_limits(&self, id: u32) -> Option<ResourceLimits> {
        self.workers
            .lock()
            .get(&id)
            .and_then(|w| w.options.resource_limits.clone())
    }

    // ========== MessageChannel / MessagePort ==========

    /// Create a new MessageChannel (returns two linked port IDs).
    pub fn create_message_channel(&self) -> (u64, u64) {
        let port1_id = self.next_port_id.fetch_add(1, Ordering::SeqCst);
        let port2_id = self.next_port_id.fetch_add(1, Ordering::SeqCst);

        let port1 = MessagePortHandle {
            id: port1_id,
            pair_id: port2_id,
            messages: Mutex::new(Vec::new()),
            started: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            referenced: AtomicBool::new(true),
        };

        let port2 = MessagePortHandle {
            id: port2_id,
            pair_id: port1_id,
            messages: Mutex::new(Vec::new()),
            started: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            referenced: AtomicBool::new(true),
        };

        let mut ports = self.ports.lock();
        ports.insert(port1_id, port1);
        ports.insert(port2_id, port2);

        (port1_id, port2_id)
    }

    /// Post a message to a MessagePort.
    pub fn port_post_message(
        &self,
        port_id: u64,
        data: Value,
        _transfer_list: Option<Vec<Value>>,
    ) -> Result<(), WorkerThreadError> {
        let ports = self.ports.lock();
        let port = ports
            .get(&port_id)
            .ok_or(WorkerThreadError::PortNotFound(port_id))?;

        if port.closed.load(Ordering::SeqCst) {
            return Err(WorkerThreadError::PortClosed);
        }

        // Get the paired port
        let pair_id = port.pair_id;
        if let Some(pair_port) = ports.get(&pair_id) {
            if pair_port.closed.load(Ordering::SeqCst) {
                return Ok(()); // Silently drop if pair is closed
            }

            // Queue message on the paired port
            pair_port.messages.lock().push(data.clone());

            // If port is started, emit event immediately
            if pair_port.started.load(Ordering::SeqCst) {
                self.events.lock().push(WorkerThreadEvent::PortMessage {
                    port_id: pair_id,
                    data,
                });
            }
        }

        Ok(())
    }

    /// Start receiving messages on a port.
    pub fn port_start(&self, port_id: u64) -> Result<(), WorkerThreadError> {
        let ports = self.ports.lock();
        let port = ports
            .get(&port_id)
            .ok_or(WorkerThreadError::PortNotFound(port_id))?;

        port.started.store(true, Ordering::SeqCst);

        // Emit events for any queued messages
        let messages: Vec<Value> = std::mem::take(&mut *port.messages.lock());
        let mut events = self.events.lock();
        for data in messages {
            events.push(WorkerThreadEvent::PortMessage { port_id, data });
        }

        Ok(())
    }

    /// Close a MessagePort.
    pub fn port_close(&self, port_id: u64) -> Result<(), WorkerThreadError> {
        let ports = self.ports.lock();
        let port = ports
            .get(&port_id)
            .ok_or(WorkerThreadError::PortNotFound(port_id))?;

        port.closed.store(true, Ordering::SeqCst);

        // Emit close event
        self.events
            .lock()
            .push(WorkerThreadEvent::PortClose { port_id });

        Ok(())
    }

    /// Mark a port as referenced.
    pub fn port_ref(&self, port_id: u64) {
        if let Some(port) = self.ports.lock().get(&port_id) {
            port.referenced.store(true, Ordering::SeqCst);
        }
    }

    /// Mark a port as unreferenced.
    pub fn port_unref(&self, port_id: u64) {
        if let Some(port) = self.ports.lock().get(&port_id) {
            port.referenced.store(false, Ordering::SeqCst);
        }
    }

    /// Check if a port has a reference.
    pub fn port_has_ref(&self, port_id: u64) -> bool {
        self.ports
            .lock()
            .get(&port_id)
            .is_some_and(|p| p.referenced.load(Ordering::SeqCst))
    }

    /// Receive a single message synchronously from a port.
    pub fn receive_message_on_port(&self, port_id: u64) -> Option<Value> {
        let ports = self.ports.lock();
        let port = ports.get(&port_id)?;

        if port.closed.load(Ordering::SeqCst) {
            return None;
        }

        let mut messages = port.messages.lock();
        if messages.is_empty() {
            None
        } else {
            Some(messages.remove(0))
        }
    }

    // ========== BroadcastChannel ==========

    /// Create a new BroadcastChannel.
    pub fn create_broadcast_channel(&self, name: String) -> u64 {
        let id = self.next_broadcast_id.fetch_add(1, Ordering::SeqCst);

        let handle = BroadcastChannelHandle {
            id,
            name: name.clone(),
            closed: AtomicBool::new(false),
            referenced: AtomicBool::new(true),
        };

        self.broadcast_handles.lock().insert(id, handle);

        // Add to channel list by name
        self.broadcast_channels
            .lock()
            .entry(name)
            .or_default()
            .push(id);

        id
    }

    /// Post a message to a BroadcastChannel.
    pub fn broadcast_post_message(
        &self,
        channel_id: u64,
        data: Value,
    ) -> Result<(), WorkerThreadError> {
        let handles = self.broadcast_handles.lock();
        let handle = handles
            .get(&channel_id)
            .ok_or(WorkerThreadError::BroadcastChannelNotFound(channel_id))?;

        if handle.closed.load(Ordering::SeqCst) {
            return Err(WorkerThreadError::PortClosed);
        }

        let name = handle.name.clone();
        drop(handles);

        // Get all channels with the same name
        let channels = self.broadcast_channels.lock();
        if let Some(channel_ids) = channels.get(&name) {
            let mut events = self.events.lock();
            for &id in channel_ids {
                if id != channel_id {
                    // Don't send to self
                    events.push(WorkerThreadEvent::BroadcastMessage {
                        channel_id: id,
                        name: name.clone(),
                        data: data.clone(),
                    });
                }
            }
        }

        Ok(())
    }

    /// Close a BroadcastChannel.
    pub fn broadcast_close(&self, channel_id: u64) -> Result<(), WorkerThreadError> {
        let handles = self.broadcast_handles.lock();
        let handle = handles
            .get(&channel_id)
            .ok_or(WorkerThreadError::BroadcastChannelNotFound(channel_id))?;

        handle.closed.store(true, Ordering::SeqCst);

        let name = handle.name.clone();
        drop(handles);

        // Remove from channel list
        let mut channels = self.broadcast_channels.lock();
        if let Some(ids) = channels.get_mut(&name) {
            ids.retain(|&id| id != channel_id);
            if ids.is_empty() {
                channels.remove(&name);
            }
        }

        Ok(())
    }

    /// Mark a broadcast channel as referenced.
    pub fn broadcast_ref(&self, channel_id: u64) {
        if let Some(handle) = self.broadcast_handles.lock().get(&channel_id) {
            handle.referenced.store(true, Ordering::SeqCst);
        }
    }

    /// Mark a broadcast channel as unreferenced.
    pub fn broadcast_unref(&self, channel_id: u64) {
        if let Some(handle) = self.broadcast_handles.lock().get(&channel_id) {
            handle.referenced.store(false, Ordering::SeqCst);
        }
    }

    // ========== Environment Data ==========

    /// Get environment data by key.
    pub fn get_environment_data(&self, key: &str) -> Option<Value> {
        self.env_data.lock().get(key).cloned()
    }

    /// Set environment data (available to all workers).
    pub fn set_environment_data(&self, key: String, value: Value) {
        self.env_data.lock().insert(key, value);
    }

    // ========== Untransferable ==========

    /// Mark an object as untransferable (returns a unique ID for tracking).
    pub fn mark_as_untransferable(&self) -> u64 {
        let id = self.next_untransferable_id.fetch_add(1, Ordering::SeqCst);
        self.untransferable.lock().insert(id);
        id
    }

    /// Check if an object is marked as untransferable.
    pub fn is_marked_as_untransferable(&self, id: u64) -> bool {
        self.untransferable.lock().contains(&id)
    }

    // ========== Event Polling ==========

    /// Poll for worker thread events.
    ///
    /// Returns all pending events and clears the queue.
    pub fn poll_events(&self) -> Vec<WorkerThreadEvent> {
        // Drain events from channel
        let mut rx = self.event_rx.lock();
        let mut events = self.events.lock();

        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }

        std::mem::take(&mut *events)
    }

    /// Get the number of active workers.
    pub fn active_count(&self) -> usize {
        self.workers
            .lock()
            .values()
            .filter(|w| w.running.load(Ordering::SeqCst))
            .count()
    }

    /// Get the number of referenced workers (keeping event loop alive).
    pub fn referenced_count(&self) -> usize {
        self.workers
            .lock()
            .values()
            .filter(|w| w.running.load(Ordering::SeqCst) && w.referenced.load(Ordering::SeqCst))
            .count()
    }

    /// Remove terminated workers from the manager.
    pub fn cleanup(&self) {
        let mut workers = self.workers.lock();
        workers.retain(|_, w| w.running.load(Ordering::SeqCst));
    }
}

impl Default for WorkerThreadManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Run a worker thread with its own JS context.
async fn run_worker_thread(
    worker_id: u32,
    filename: String,
    eval_mode: bool,
    worker_data: Value,
    running: Arc<AtomicBool>,
    mut message_rx: mpsc::UnboundedReceiver<WorkerThreadMessage>,
    event_tx: mpsc::UnboundedSender<WorkerThreadEvent>,
) {
    // Set tokio handle for this thread
    set_tokio_handle(tokio::runtime::Handle::current());

    // Create a new JS runtime for this worker
    let runtime = match JscRuntime::new(JscConfig::default()) {
        Ok(r) => r,
        Err(e) => {
            let _ = event_tx.send(WorkerThreadEvent::Error {
                worker_id,
                error: format!("Failed to create runtime: {}", e),
            });
            return;
        }
    };

    // Register essential extensions for workers
    if let Err(e) = register_worker_extensions(&runtime) {
        let _ = event_tx.send(WorkerThreadEvent::Error {
            worker_id,
            error: format!("Failed to register extensions: {}", e),
        });
        return;
    }

    // Set up worker-specific globals
    let worker_data_json = serde_json::to_string(&worker_data).unwrap_or("null".to_string());
    let setup_code = format!(
        r#"
        globalThis.__otter_worker_thread_id = {worker_id};
        globalThis.__otter_is_main_thread = false;
        globalThis.__otter_worker_data = {worker_data_json};

        // Message queue from main thread
        globalThis.__otter_worker_messages = [];

        // parentPort mock for receiving messages
        globalThis.__otter_parent_port_handlers = [];

        // Function to receive messages from main thread (called from Rust)
        globalThis.__otter_worker_receive_message = function(data) {{
            for (const handler of globalThis.__otter_parent_port_handlers) {{
                try {{
                    handler({{ data }});
                }} catch (e) {{
                    console.error('Worker message handler error:', e);
                }}
            }}
        }};
        "#
    );

    if let Err(e) = runtime.eval(&setup_code) {
        let _ = event_tx.send(WorkerThreadEvent::Error {
            worker_id,
            error: format!("Failed to setup worker globals: {}", e),
        });
        return;
    }

    // Send online event
    let _ = event_tx.send(WorkerThreadEvent::Online { worker_id });

    // Get the script to execute
    let script = if eval_mode {
        filename.clone()
    } else {
        // Read from file
        let path = PathBuf::from(&filename);
        match std::fs::read_to_string(&path) {
            Ok(source) => {
                // Transpile if TypeScript
                if needs_transpilation(&filename) {
                    match transpile_typescript(&source) {
                        Ok(result) => result.code,
                        Err(e) => {
                            let _ = event_tx.send(WorkerThreadEvent::Error {
                                worker_id,
                                error: format!("Failed to transpile: {}", e),
                            });
                            return;
                        }
                    }
                } else {
                    source
                }
            }
            Err(e) => {
                let _ = event_tx.send(WorkerThreadEvent::Error {
                    worker_id,
                    error: format!("Failed to read file '{}': {}", filename, e),
                });
                return;
            }
        }
    };

    // Execute the worker script
    if let Err(e) = runtime.eval(&script) {
        let _ = event_tx.send(WorkerThreadEvent::Error {
            worker_id,
            error: format!("Worker script error: {}", e),
        });
        // Continue running to handle messages even if initial script fails
    }

    // Message loop
    let event_tx_for_messages = event_tx.clone();
    while running.load(Ordering::SeqCst) {
        // Poll the JS event loop
        if let Err(e) = runtime.poll_event_loop() {
            let _ = event_tx.send(WorkerThreadEvent::Error {
                worker_id,
                error: format!("Event loop error: {}", e),
            });
        }

        // Check for messages from main thread
        match message_rx.try_recv() {
            Ok(WorkerThreadMessage::Data(data)) => {
                // Call the worker's message handler
                let data_json = serde_json::to_string(&data).unwrap_or("null".to_string());
                let call_code =
                    format!("globalThis.__otter_worker_receive_message({});", data_json);
                if let Err(e) = runtime.eval(&call_code) {
                    let _ = event_tx_for_messages.send(WorkerThreadEvent::Error {
                        worker_id,
                        error: format!("Error calling message handler: {}", e),
                    });
                }
            }
            Ok(WorkerThreadMessage::Terminate) => {
                running.store(false, Ordering::SeqCst);
                break;
            }
            Ok(WorkerThreadMessage::Error(msg)) => {
                let _ = event_tx_for_messages.send(WorkerThreadEvent::Error {
                    worker_id,
                    error: msg,
                });
            }
            Err(mpsc::error::TryRecvError::Empty) => {
                // No message, brief sleep
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            Err(mpsc::error::TryRecvError::Disconnected) => {
                break;
            }
        }
    }
}

/// Register essential extensions for worker threads.
fn register_worker_extensions(runtime: &JscRuntime) -> Result<(), String> {
    use crate::ext;

    // Register essential Node.js compatibility extensions
    runtime
        .register_extension(ext::path())
        .map_err(|e| e.to_string())?;
    runtime
        .register_extension(ext::buffer())
        .map_err(|e| e.to_string())?;
    runtime
        .register_extension(ext::events())
        .map_err(|e| e.to_string())?;
    runtime
        .register_extension(ext::util())
        .map_err(|e| e.to_string())?;
    runtime
        .register_extension(ext::url())
        .map_err(|e| e.to_string())?;
    runtime
        .register_extension(ext::crypto())
        .map_err(|e| e.to_string())?;
    runtime
        .register_extension(ext::timers())
        .map_err(|e| e.to_string())?;
    runtime
        .register_extension(ext::string_decoder())
        .map_err(|e| e.to_string())?;
    runtime
        .register_extension(ext::querystring())
        .map_err(|e| e.to_string())?;
    runtime
        .register_extension(ext::assert())
        .map_err(|e| e.to_string())?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_worker_thread_manager_creation() {
        let manager = WorkerThreadManager::new();
        assert_eq!(manager.active_count(), 0);
        assert!(manager.is_main_thread());
        assert_eq!(manager.thread_id(), 0);
    }

    #[test]
    fn test_message_channel() {
        let manager = WorkerThreadManager::new();
        let (port1, port2) = manager.create_message_channel();

        assert!(port1 != port2);
        assert!(manager.port_has_ref(port1));
        assert!(manager.port_has_ref(port2));
    }

    #[test]
    fn test_port_message() {
        let manager = WorkerThreadManager::new();
        let (port1, port2) = manager.create_message_channel();

        // Start port2 to receive messages
        manager.port_start(port2).unwrap();

        // Send message from port1 to port2
        manager
            .port_post_message(port1, serde_json::json!({"test": "data"}), None)
            .unwrap();

        // Poll events
        let events = manager.poll_events();
        assert!(!events.is_empty());
        assert!(matches!(
            events[0],
            WorkerThreadEvent::PortMessage { port_id, .. } if port_id == port2
        ));
    }

    #[test]
    fn test_receive_message_on_port() {
        let manager = WorkerThreadManager::new();
        let (port1, port2) = manager.create_message_channel();

        // Send message without starting port
        manager
            .port_post_message(port1, serde_json::json!({"test": "sync"}), None)
            .unwrap();

        // Receive synchronously
        let msg = manager.receive_message_on_port(port2);
        assert!(msg.is_some());
        assert_eq!(msg.unwrap()["test"], "sync");

        // Queue should be empty now
        let msg2 = manager.receive_message_on_port(port2);
        assert!(msg2.is_none());
    }

    #[test]
    fn test_broadcast_channel() {
        let manager = WorkerThreadManager::new();
        let ch1 = manager.create_broadcast_channel("test".to_string());
        let ch2 = manager.create_broadcast_channel("test".to_string());

        assert!(ch1 != ch2);

        // Post message from ch1 should be received by ch2
        manager
            .broadcast_post_message(ch1, serde_json::json!({"broadcast": true}))
            .unwrap();

        let events = manager.poll_events();
        assert!(!events.is_empty());
        assert!(matches!(
            &events[0],
            WorkerThreadEvent::BroadcastMessage { channel_id, name, .. }
            if *channel_id == ch2 && name == "test"
        ));
    }

    #[test]
    fn test_environment_data() {
        let manager = WorkerThreadManager::new();

        assert!(manager.get_environment_data("key").is_none());

        manager.set_environment_data("key".to_string(), serde_json::json!("value"));
        assert_eq!(
            manager.get_environment_data("key"),
            Some(serde_json::json!("value"))
        );
    }

    #[test]
    fn test_untransferable() {
        let manager = WorkerThreadManager::new();

        let id = manager.mark_as_untransferable();
        assert!(manager.is_marked_as_untransferable(id));
        assert!(!manager.is_marked_as_untransferable(id + 1));
    }
}
