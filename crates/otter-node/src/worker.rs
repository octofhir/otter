//! Web Worker API implementation for Otter.
//!
//! Provides Web-standard Worker API for running JavaScript in background threads.
//!
//! # Example
//!
//! ```javascript
//! const worker = new Worker('worker.js');
//! worker.onmessage = (e) => console.log('From worker:', e.data);
//! worker.postMessage({ task: 'compute' });
//! worker.terminate();
//! ```

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::JoinHandle;
use thiserror::Error;
use tokio::sync::mpsc;

/// Errors that can occur during Worker operations.
#[derive(Debug, Error)]
pub enum WorkerError {
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
}

/// Message sent between main thread and worker.
#[derive(Debug, Clone)]
pub enum WorkerMessage {
    /// JSON-serializable data message
    Data(serde_json::Value),
    /// Error message
    Error(String),
    /// Terminate signal
    Terminate,
}

/// Event from a worker.
#[derive(Debug, Clone)]
pub enum WorkerEvent {
    /// Worker received a message
    Message(serde_json::Value),
    /// Worker encountered an error
    Error(String),
    /// Worker exited normally
    Exit,
    /// Worker was terminated
    Terminated,
}

/// Internal worker handle
struct WorkerHandle {
    #[allow(dead_code)]
    id: u32,
    /// Channel to send messages to the worker
    message_tx: mpsc::UnboundedSender<WorkerMessage>,
    /// Worker's running state
    running: Arc<AtomicBool>,
    /// Thread handle
    #[allow(dead_code)]
    thread_handle: Option<JoinHandle<()>>,
}

/// Manager for Web Workers.
///
/// Handles creation, message passing, and termination of workers.
pub struct WorkerManager {
    workers: Mutex<HashMap<u32, WorkerHandle>>,
    next_id: AtomicU32,
    /// Events from workers to be polled by main thread
    events: Mutex<Vec<(u32, WorkerEvent)>>,
    /// Channel for workers to send events
    event_tx: mpsc::UnboundedSender<(u32, WorkerEvent)>,
    event_rx: Mutex<mpsc::UnboundedReceiver<(u32, WorkerEvent)>>,
}

impl WorkerManager {
    /// Create a new worker manager.
    pub fn new() -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        Self {
            workers: Mutex::new(HashMap::new()),
            next_id: AtomicU32::new(1),
            events: Mutex::new(Vec::new()),
            event_tx,
            event_rx: Mutex::new(event_rx),
        }
    }

    /// Create a new worker with inline script.
    ///
    /// Returns the worker ID.
    pub fn create(&self, script: String) -> Result<u32, WorkerError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let running = Arc::new(AtomicBool::new(true));
        let running_clone = running.clone();
        let event_tx = self.event_tx.clone();

        // Channel for messages to the worker
        let (message_tx, mut message_rx) = mpsc::unbounded_channel::<WorkerMessage>();

        // Channel for messages from the worker (to main thread)
        let event_tx_clone = event_tx.clone();

        // Spawn worker thread
        let handle = std::thread::Builder::new()
            .name(format!("otter-worker-{}", id))
            .spawn(move || {
                // Create a minimal JavaScript context for the worker
                // For now, we'll use a simple message loop
                // TODO: Integrate with JscContext when available in this crate

                // Simulate worker execution
                while running_clone.load(Ordering::SeqCst) {
                    // Try to receive a message
                    match message_rx.try_recv() {
                        Ok(WorkerMessage::Data(data)) => {
                            // Echo the message back (placeholder behavior)
                            // In a real implementation, this would execute the script
                            // and call onmessage handler
                            let _ = event_tx_clone.send((id, WorkerEvent::Message(data)));
                        }
                        Ok(WorkerMessage::Terminate) => {
                            running_clone.store(false, Ordering::SeqCst);
                            let _ = event_tx_clone.send((id, WorkerEvent::Terminated));
                            break;
                        }
                        Ok(WorkerMessage::Error(msg)) => {
                            let _ = event_tx_clone.send((id, WorkerEvent::Error(msg)));
                        }
                        Err(mpsc::error::TryRecvError::Empty) => {
                            // No message, sleep briefly
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                        Err(mpsc::error::TryRecvError::Disconnected) => {
                            break;
                        }
                    }
                }

                // Send exit event if not terminated
                if running_clone.load(Ordering::SeqCst) {
                    let _ = event_tx.send((id, WorkerEvent::Exit));
                }
            })
            .map_err(|e| WorkerError::CreationFailed(e.to_string()))?;

        // Store the worker handle
        let worker_handle = WorkerHandle {
            id,
            message_tx,
            running,
            thread_handle: Some(handle),
        };

        self.workers.lock().insert(id, worker_handle);

        // Store script for potential future use
        let _ = script; // Script will be used when we integrate with JscContext

        Ok(id)
    }

    /// Post a message to a worker.
    pub fn post_message(&self, id: u32, data: serde_json::Value) -> Result<(), WorkerError> {
        let workers = self.workers.lock();
        let worker = workers.get(&id).ok_or(WorkerError::NotFound(id))?;

        if !worker.running.load(Ordering::SeqCst) {
            return Err(WorkerError::Terminated);
        }

        worker
            .message_tx
            .send(WorkerMessage::Data(data))
            .map_err(|e| WorkerError::SendFailed(e.to_string()))
    }

    /// Terminate a worker.
    pub fn terminate(&self, id: u32) -> Result<(), WorkerError> {
        let workers = self.workers.lock();
        let worker = workers.get(&id).ok_or(WorkerError::NotFound(id))?;

        worker.running.store(false, Ordering::SeqCst);
        let _ = worker.message_tx.send(WorkerMessage::Terminate);

        Ok(())
    }

    /// Check if a worker is running.
    pub fn is_running(&self, id: u32) -> bool {
        self.workers
            .lock()
            .get(&id)
            .is_some_and(|w| w.running.load(Ordering::SeqCst))
    }

    /// Poll for worker events.
    ///
    /// Returns all pending events and clears the queue.
    pub fn poll_events(&self) -> Vec<(u32, WorkerEvent)> {
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

    /// Remove terminated workers from the manager.
    pub fn cleanup(&self) {
        let mut workers = self.workers.lock();
        workers.retain(|_, w| w.running.load(Ordering::SeqCst));
    }
}

impl Default for WorkerManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_worker_manager_creation() {
        let manager = WorkerManager::new();
        assert_eq!(manager.active_count(), 0);
    }

    #[test]
    fn test_create_worker() {
        let manager = WorkerManager::new();
        let id = manager.create("console.log('hello')".to_string()).unwrap();
        assert!(id > 0);
        assert!(manager.is_running(id));
    }

    #[test]
    fn test_terminate_worker() {
        let manager = WorkerManager::new();
        let id = manager.create("".to_string()).unwrap();
        assert!(manager.is_running(id));

        manager.terminate(id).unwrap();
        // Give thread time to process terminate
        std::thread::sleep(std::time::Duration::from_millis(50));

        assert!(!manager.is_running(id));
    }

    #[tokio::test]
    async fn test_post_message() {
        let manager = WorkerManager::new();
        let id = manager.create("".to_string()).unwrap();

        manager
            .post_message(id, serde_json::json!({"test": "data"}))
            .unwrap();

        // Give worker time to process
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let events = manager.poll_events();
        assert!(!events.is_empty());
        assert!(matches!(events[0].1, WorkerEvent::Message(_)));
    }

    #[test]
    fn test_post_message_to_terminated() {
        let manager = WorkerManager::new();
        let id = manager.create("".to_string()).unwrap();

        manager.terminate(id).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));

        let result = manager.post_message(id, serde_json::json!({}));
        assert!(result.is_err());
    }

    #[test]
    fn test_worker_not_found() {
        let manager = WorkerManager::new();
        let result = manager.post_message(999, serde_json::json!({}));
        assert!(matches!(result, Err(WorkerError::NotFound(999))));
    }

    #[test]
    fn test_cleanup() {
        let manager = WorkerManager::new();
        let id = manager.create("".to_string()).unwrap();

        manager.terminate(id).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));

        manager.cleanup();
        assert_eq!(manager.active_count(), 0);
    }
}
