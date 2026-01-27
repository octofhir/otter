//! Worker Threads
//!
//! Implements Web Workers API for multi-threaded JavaScript execution.
//! Each worker runs in its own OS thread with its own event loop.
//!
//! ## API
//! - `new Worker(script)` - Create a new worker from a script
//! - `worker.postMessage(data)` - Send data to the worker
//! - `worker.onmessage` - Receive messages from the worker
//! - `worker.terminate()` - Stop the worker

use otter_vm_core::{MemoryManager, StructuredCloneError, Value, structured_clone};
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

/// Unique worker ID counter
static NEXT_WORKER_ID: AtomicU64 = AtomicU64::new(1);

/// Message sent between main thread and worker
#[derive(Debug)]
pub enum WorkerMessage {
    /// Data message (postMessage)
    Data(Value),
    /// Error from worker
    Error(String),
    /// Worker is terminating
    Terminate,
}

/// Message handler callback type
pub type MessageHandler = Box<dyn Fn(Value) + Send + 'static>;

/// Error handler callback type
pub type ErrorHandler = Box<dyn Fn(String) + Send + 'static>;

/// A Web Worker handle
///
/// Workers run JavaScript in a separate thread with their own event loop.
/// Communication happens via message passing using `postMessage`.
pub struct Worker {
    /// Unique worker ID
    id: u64,
    /// Channel to send messages to the worker
    tx: Sender<WorkerMessage>,
    /// Channel to receive messages from the worker
    rx: Mutex<Receiver<WorkerMessage>>,
    /// Thread handle
    handle: Mutex<Option<JoinHandle<()>>>,
    /// Whether the worker is terminated
    terminated: AtomicBool,
    /// Message handler (onmessage)
    on_message: Mutex<Option<MessageHandler>>,
    /// Error handler (onerror)
    on_error: Mutex<Option<ErrorHandler>>,
    /// Destination memory manager for messages sent to the worker
    target_memory_manager: Arc<MemoryManager>,
}

impl Worker {
    /// Create a new worker
    ///
    /// The `script` is the source code to execute in the worker.
    /// The `executor` is called with the worker context to run the script.
    pub fn new<F>(
        executor: F,
        worker_mm: Arc<MemoryManager>,
        main_mm: Arc<MemoryManager>,
    ) -> Arc<Self>
    where
        F: FnOnce(WorkerContext) + Send + 'static,
    {
        let id = NEXT_WORKER_ID.fetch_add(1, Ordering::Relaxed);

        // Create bidirectional channels
        let (main_tx, worker_rx) = mpsc::channel();
        let (worker_tx, main_rx) = mpsc::channel();

        let worker = Arc::new(Self {
            id,
            tx: main_tx,
            rx: Mutex::new(main_rx),
            handle: Mutex::new(None),
            terminated: AtomicBool::new(false),
            on_message: Mutex::new(None),
            on_error: Mutex::new(None),
            target_memory_manager: Arc::clone(&worker_mm),
        });

        // Create worker context
        let ctx = WorkerContext {
            id,
            tx: worker_tx,
            rx: worker_rx,
            terminated: Arc::new(AtomicBool::new(false)),
            target_memory_manager: main_mm,
        };

        // Spawn worker thread
        let handle = thread::Builder::new()
            .name(format!("worker-{}", id))
            .spawn(move || {
                executor(ctx);
            })
            .expect("Failed to spawn worker thread");

        *worker.handle.lock() = Some(handle);

        worker
    }

    /// Get the worker ID
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Send a message to the worker (postMessage)
    ///
    /// The value is cloned using the structured clone algorithm.
    /// SharedArrayBuffer is shared (not cloned).
    pub fn post_message(&self, value: Value) -> Result<(), WorkerError> {
        if self.terminated.load(Ordering::Acquire) {
            return Err(WorkerError::Terminated);
        }

        // Clone the value using structured clone
        let cloned = structured_clone(&value, self.target_memory_manager.clone())
            .map_err(WorkerError::CloneError)?;

        self.tx
            .send(WorkerMessage::Data(cloned))
            .map_err(|_| WorkerError::ChannelClosed)
    }

    /// Set the message handler (onmessage)
    pub fn set_on_message<F>(&self, handler: F)
    where
        F: Fn(Value) + Send + 'static,
    {
        *self.on_message.lock() = Some(Box::new(handler));
    }

    /// Set the error handler (onerror)
    pub fn set_on_error<F>(&self, handler: F)
    where
        F: Fn(String) + Send + 'static,
    {
        *self.on_error.lock() = Some(Box::new(handler));
    }

    /// Receive a message from the worker (non-blocking)
    pub fn try_recv(&self) -> Option<WorkerMessage> {
        self.rx.lock().try_recv().ok()
    }

    /// Process pending messages from worker
    pub fn process_messages(&self) {
        while let Some(msg) = self.try_recv() {
            match msg {
                WorkerMessage::Data(value) => {
                    if let Some(handler) = self.on_message.lock().as_ref() {
                        handler(value);
                    }
                }
                WorkerMessage::Error(err) => {
                    if let Some(handler) = self.on_error.lock().as_ref() {
                        handler(err);
                    }
                }
                WorkerMessage::Terminate => {
                    self.terminated.store(true, Ordering::Release);
                }
            }
        }
    }

    /// Terminate the worker
    pub fn terminate(&self) {
        if self.terminated.swap(true, Ordering::AcqRel) {
            return; // Already terminated
        }

        // Send terminate message
        let _ = self.tx.send(WorkerMessage::Terminate);

        // Wait for thread to finish (with timeout)
        if let Some(handle) = self.handle.lock().take() {
            let _ = handle.join();
        }
    }

    /// Check if worker is terminated
    pub fn is_terminated(&self) -> bool {
        self.terminated.load(Ordering::Acquire)
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.terminate();
    }
}

/// Context available inside a worker thread
pub struct WorkerContext {
    /// Worker ID
    id: u64,
    /// Channel to send messages to main thread
    tx: Sender<WorkerMessage>,
    /// Channel to receive messages from main thread
    rx: Receiver<WorkerMessage>,
    /// Termination flag
    terminated: Arc<AtomicBool>,
    /// Destination memory manager for messages sent to the main thread
    target_memory_manager: Arc<MemoryManager>,
}

impl WorkerContext {
    /// Get the worker ID
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Send a message to the main thread (postMessage)
    pub fn post_message(&self, value: Value) -> Result<(), WorkerError> {
        if self.terminated.load(Ordering::Acquire) {
            return Err(WorkerError::Terminated);
        }

        // Clone the value using structured clone
        let cloned = structured_clone(&value, self.target_memory_manager.clone())
            .map_err(WorkerError::CloneError)?;

        self.tx
            .send(WorkerMessage::Data(cloned))
            .map_err(|_| WorkerError::ChannelClosed)
    }

    /// Send an error to the main thread
    pub fn post_error(&self, error: String) {
        let _ = self.tx.send(WorkerMessage::Error(error));
    }

    /// Receive a message from the main thread (blocking)
    pub fn recv(&self) -> Option<WorkerMessage> {
        if self.terminated.load(Ordering::Acquire) {
            return None;
        }
        self.rx.recv().ok()
    }

    /// Receive a message from the main thread (non-blocking)
    pub fn try_recv(&self) -> Option<WorkerMessage> {
        self.rx.try_recv().ok()
    }

    /// Check if termination was requested
    pub fn is_terminated(&self) -> bool {
        self.terminated.load(Ordering::Acquire)
    }

    /// Mark self as terminated (called when receiving Terminate message)
    pub fn mark_terminated(&self) {
        self.terminated.store(true, Ordering::Release);
    }
}

/// Worker error types
#[derive(Debug, Clone)]
pub enum WorkerError {
    /// Worker has been terminated
    Terminated,
    /// Message channel is closed
    ChannelClosed,
    /// Value cannot be cloned
    CloneError(StructuredCloneError),
}

impl std::fmt::Display for WorkerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Terminated => write!(f, "Worker has been terminated"),
            Self::ChannelClosed => write!(f, "Message channel is closed"),
            Self::CloneError(e) => write!(f, "Clone error: {}", e),
        }
    }
}

impl std::error::Error for WorkerError {}

/// Worker pool for managing multiple workers
pub struct WorkerPool {
    /// Active workers
    workers: Mutex<Vec<Arc<Worker>>>,
}

impl WorkerPool {
    /// Create a new worker pool
    pub fn new() -> Self {
        Self {
            workers: Mutex::new(Vec::new()),
        }
    }

    /// Spawn a new worker
    pub fn spawn<F>(
        &self,
        executor: F,
        worker_mm: Arc<MemoryManager>,
        main_mm: Arc<MemoryManager>,
    ) -> Arc<Worker>
    where
        F: FnOnce(WorkerContext) + Send + 'static,
    {
        let worker = Worker::new(executor, worker_mm, main_mm);
        self.workers.lock().push(Arc::clone(&worker));
        worker
    }

    /// Process messages from all workers
    pub fn process_all_messages(&self) {
        for worker in self.workers.lock().iter() {
            worker.process_messages();
        }
    }

    /// Remove terminated workers
    pub fn cleanup(&self) {
        self.workers.lock().retain(|w| !w.is_terminated());
    }

    /// Terminate all workers
    pub fn terminate_all(&self) {
        for worker in self.workers.lock().iter() {
            worker.terminate();
        }
        self.workers.lock().clear();
    }

    /// Get number of active workers
    pub fn len(&self) -> usize {
        self.workers.lock().len()
    }

    /// Check if pool is empty
    pub fn is_empty(&self) -> bool {
        self.workers.lock().is_empty()
    }
}

impl Default for WorkerPool {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        self.terminate_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    #[test]
    fn test_worker_creation() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = Arc::clone(&counter);

        let main_mm = Arc::new(MemoryManager::test());
        let worker_mm = Arc::new(MemoryManager::test());
        let worker = Worker::new(
            move |_ctx| {
                counter_clone.fetch_add(1, Ordering::Relaxed);
            },
            worker_mm,
            main_mm,
        );

        // Give the worker time to run
        thread::sleep(Duration::from_millis(50));
        worker.terminate();

        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_worker_post_message() {
        let received = Arc::new(Mutex::new(None));
        let received_clone = Arc::clone(&received);

        let main_mm = Arc::new(MemoryManager::test());
        let worker_mm = Arc::new(MemoryManager::test());
        let worker = Worker::new(
            move |ctx| {
                // Wait for message
                if let Some(WorkerMessage::Data(value)) = ctx.recv() {
                    *received_clone.lock() = Some(value);
                }
            },
            worker_mm,
            main_mm,
        );

        // Send message to worker
        worker.post_message(Value::int32(42)).unwrap();

        // Give worker time to process
        thread::sleep(Duration::from_millis(50));
        worker.terminate();

        let value = received.lock().take();
        assert!(value.is_some());
        assert_eq!(value.unwrap().as_number(), Some(42.0));
    }

    #[test]
    fn test_worker_bidirectional() {
        let main_mm = Arc::new(MemoryManager::test());
        let worker_mm = Arc::new(MemoryManager::test());
        let worker = Worker::new(
            |ctx| {
                // Echo messages back
                while let Some(msg) = ctx.recv() {
                    match msg {
                        WorkerMessage::Data(value) => {
                            let _ = ctx.post_message(value);
                        }
                        WorkerMessage::Terminate => {
                            ctx.mark_terminated();
                            break;
                        }
                        _ => {}
                    }
                }
            },
            worker_mm,
            main_mm,
        );

        let received = Arc::new(Mutex::new(Vec::new()));
        let received_clone = Arc::clone(&received);

        worker.set_on_message(move |value| {
            received_clone.lock().push(value);
        });

        // Send messages
        worker.post_message(Value::int32(1)).unwrap();
        worker.post_message(Value::int32(2)).unwrap();
        worker.post_message(Value::int32(3)).unwrap();

        // Give worker time to process and echo
        thread::sleep(Duration::from_millis(100));

        // Process responses
        worker.process_messages();

        worker.terminate();

        let values = received.lock();
        assert_eq!(values.len(), 3);
    }

    #[test]
    fn test_worker_terminate() {
        let terminated = Arc::new(AtomicBool::new(false));
        let terminated_clone = Arc::clone(&terminated);

        let main_mm = Arc::new(MemoryManager::test());
        let worker_mm = Arc::new(MemoryManager::test());
        let worker = Worker::new(
            move |ctx| {
                while let Some(msg) = ctx.recv() {
                    if matches!(msg, WorkerMessage::Terminate) {
                        terminated_clone.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            },
            worker_mm,
            main_mm,
        );

        worker.terminate();
        thread::sleep(Duration::from_millis(50));

        assert!(terminated.load(Ordering::Relaxed));
        assert!(worker.is_terminated());
    }

    #[test]
    fn test_worker_pool() {
        let pool = WorkerPool::new();
        let counter = Arc::new(AtomicUsize::new(0));

        let main_mm = Arc::new(MemoryManager::test());
        for _ in 0..3 {
            let counter_clone = Arc::clone(&counter);
            let worker_mm = Arc::new(MemoryManager::test());
            pool.spawn(
                move |_ctx| {
                    counter_clone.fetch_add(1, Ordering::Relaxed);
                },
                worker_mm,
                main_mm.clone(),
            );
        }

        assert_eq!(pool.len(), 3);

        // Give workers time to run
        thread::sleep(Duration::from_millis(50));
        pool.terminate_all();

        assert_eq!(counter.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn test_shared_array_buffer_between_workers() {
        use otter_vm_core::SharedArrayBuffer;

        let sab = Arc::new(SharedArrayBuffer::new(4));
        let sab_clone = Arc::clone(&sab);

        let main_mm = Arc::new(MemoryManager::test());
        let worker_mm = Arc::new(MemoryManager::test());
        let worker = Worker::new(
            move |ctx| {
                // Wait for SharedArrayBuffer
                if let Some(WorkerMessage::Data(value)) = ctx.recv()
                    && let Some(received_sab) = value.as_shared_array_buffer()
                {
                    // Modify the shared buffer
                    received_sab.set(0, 42);
                }
            },
            worker_mm,
            main_mm,
        );

        // Send SharedArrayBuffer to worker
        worker
            .post_message(Value::shared_array_buffer(sab_clone))
            .unwrap();

        // Give worker time to process
        thread::sleep(Duration::from_millis(100));
        worker.terminate();

        // Check that main thread sees the modification
        assert_eq!(sab.get(0), Some(42));
    }
}
