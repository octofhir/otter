//! Thread-safe engine for JavaScript execution
//!
//! The Engine provides a pool of worker threads for executing JavaScript code.
//! Jobs are submitted via a thread-safe `EngineHandle` and executed on worker threads.
//!
//! # Example
//!
//! ```no_run
//! use otter_runtime::Engine;
//!
//! #[tokio::main]
//! async fn main() {
//!     let engine = Engine::new().unwrap();
//!     let handle = engine.handle();
//!
//!     // Evaluate JavaScript from any thread
//!     let result = handle.eval("1 + 1").await.unwrap();
//!     assert_eq!(result, serde_json::json!(2));
//!
//!     engine.shutdown().await;
//! }
//! ```

use crate::error::{JscError, JscResult};
use crate::extension::Extension;
use crate::worker::{HttpEvent, Job, NetEvent, run_worker_with_events};
use crossbeam_channel::{Sender, bounded, unbounded};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use tokio::sync::oneshot;

/// Statistics about engine operation
///
/// All counters are atomic and can be read at any time without locking.
#[derive(Debug, Default)]
pub struct EngineStats {
    /// Total number of jobs submitted to the engine
    pub jobs_submitted: AtomicU64,
    /// Total number of jobs completed (successfully or with error)
    pub jobs_completed: AtomicU64,
    /// Number of jobs that failed with an error
    pub jobs_failed: AtomicU64,
}

impl EngineStats {
    /// Create new empty stats
    pub fn new() -> Self {
        Self::default()
    }

    /// Get snapshot of current stats
    pub fn snapshot(&self) -> EngineStatsSnapshot {
        EngineStatsSnapshot {
            jobs_submitted: self.jobs_submitted.load(Ordering::Relaxed),
            jobs_completed: self.jobs_completed.load(Ordering::Relaxed),
            jobs_failed: self.jobs_failed.load(Ordering::Relaxed),
        }
    }

    /// Get the number of jobs currently in flight
    pub fn jobs_in_flight(&self) -> u64 {
        let submitted = self.jobs_submitted.load(Ordering::Relaxed);
        let completed = self.jobs_completed.load(Ordering::Relaxed);
        submitted.saturating_sub(completed)
    }
}

/// A point-in-time snapshot of engine statistics
#[derive(Debug, Clone, Copy)]
pub struct EngineStatsSnapshot {
    pub jobs_submitted: u64,
    pub jobs_completed: u64,
    pub jobs_failed: u64,
}

impl EngineStatsSnapshot {
    /// Get the success rate as a percentage (0.0 - 100.0)
    pub fn success_rate(&self) -> f64 {
        if self.jobs_completed == 0 {
            100.0
        } else {
            let succeeded = self.jobs_completed - self.jobs_failed;
            (succeeded as f64 / self.jobs_completed as f64) * 100.0
        }
    }
}

/// Builder for creating an Engine with custom configuration
pub struct EngineBuilder {
    pool_size: usize,
    queue_capacity: usize,
    extensions: Vec<Extension>,
    tokio_handle: tokio::runtime::Handle,
    /// Enable HTTP event handling with a dedicated channel
    enable_http_events: bool,
}

impl Default for EngineBuilder {
    fn default() -> Self {
        // Initialize JSC options for server-side performance (once globally)
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| {
            // JIT Configuration for Server Workloads
            // Enable OSR and lower thresholds for faster tier-up
            // SAFETY: Setting environment variables at startup is safe if no other threads are running yet
            unsafe {
                if std::env::var("JSC_useOSR").is_err() {
                    std::env::set_var("JSC_useOSR", "1");
                }
                if std::env::var("JSC_thresholdForJITAfterWarmUp").is_err() {
                    std::env::set_var("JSC_thresholdForJITAfterWarmUp", "10");
                }
                if std::env::var("JSC_thresholdForOptimizeAfterWarmUp").is_err() {
                    std::env::set_var("JSC_thresholdForOptimizeAfterWarmUp", "100");
                }
            }
        });

        // Get current Tokio handle - panics if not in a Tokio context
        let tokio_handle = tokio::runtime::Handle::current();

        Self {
            pool_size: num_cpus::get().max(1),
            queue_capacity: 1024,
            extensions: Vec::new(),
            tokio_handle,
            enable_http_events: false,
        }
    }
}

impl EngineBuilder {
    /// Set the number of worker threads
    ///
    /// Default is the number of CPU cores.
    pub fn pool_size(mut self, size: usize) -> Self {
        self.pool_size = size.max(1);
        self
    }

    /// Set the job queue capacity (backpressure threshold)
    ///
    /// When the queue is full, `try_eval` returns an error.
    /// Default is 1024.
    pub fn queue_capacity(mut self, capacity: usize) -> Self {
        self.queue_capacity = capacity.max(1);
        self
    }

    /// Register an extension to be available in all contexts
    pub fn extension(mut self, ext: Extension) -> Self {
        self.extensions.push(ext);
        self
    }

    /// Set the Tokio runtime handle for async operations in workers
    ///
    /// By default, the builder captures the current Tokio handle via `Handle::current()`.
    /// Use this method to override with a different handle if needed.
    /// The handle is required for async operations like fetch() to work in worker threads.
    pub fn tokio_handle(mut self, handle: tokio::runtime::Handle) -> Self {
        self.tokio_handle = handle;
        self
    }

    /// Enable HTTP event handling for event-driven HTTP server dispatch
    ///
    /// When enabled, the engine creates an HTTP event channel that workers
    /// listen to using crossbeam Select. This enables instant dispatch of
    /// HTTP requests without polling.
    ///
    /// After building, use `Engine::http_event_sender()` to get the sender.
    pub fn enable_http_events(mut self) -> Self {
        self.enable_http_events = true;
        self
    }

    /// Build the engine and start worker threads
    pub fn build(self) -> JscResult<Engine> {
        Engine::new_with_config(self)
    }
}

/// JavaScript execution engine with a pool of runtime threads
///
/// The Engine manages worker threads that execute JavaScript code.
/// Use `handle()` to get a thread-safe handle for submitting jobs.
pub struct Engine {
    job_tx: Sender<Job>,
    workers: Vec<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
    stats: Arc<EngineStats>,
    /// HTTP event sender for event-driven HTTP server dispatch
    http_event_tx: Option<Sender<HttpEvent>>,
    /// Net event sender for TCP server/socket events
    net_event_tx: Option<Sender<NetEvent>>,
}

impl Engine {
    /// Create a new engine with default configuration
    pub fn new() -> JscResult<Self> {
        Self::builder().build()
    }

    /// Create a builder for custom configuration
    pub fn builder() -> EngineBuilder {
        EngineBuilder::default()
    }

    fn new_with_config(config: EngineBuilder) -> JscResult<Self> {
        let (job_tx, job_rx) = bounded::<Job>(config.queue_capacity);
        let shutdown = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(EngineStats::new());

        // Create HTTP event channel if enabled
        let (http_event_tx, http_event_rx) = if config.enable_http_events {
            let (tx, rx) = unbounded::<HttpEvent>();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };

        // Always create net event channel (for node:net support)
        let (net_event_tx, net_event_rx) = {
            let (tx, rx) = unbounded::<NetEvent>();
            (Some(tx), Some(rx))
        };

        let mut workers = Vec::with_capacity(config.pool_size);

        for i in 0..config.pool_size {
            let rx = job_rx.clone();
            let http_rx = http_event_rx.clone();
            let net_rx = net_event_rx.clone();
            let extensions = config.extensions.clone();
            let shutdown_flag = shutdown.clone();
            let worker_stats = stats.clone();
            let tokio_handle = config.tokio_handle.clone();

            let handle = std::thread::Builder::new()
                .name(format!("otter-worker-{}", i))
                .spawn(move || {
                    run_worker_with_events(
                        rx,
                        http_rx,
                        net_rx,
                        extensions,
                        shutdown_flag,
                        worker_stats,
                        &tokio_handle,
                    );
                })
                .map_err(|e| JscError::internal(format!("Failed to spawn worker: {}", e)))?;

            workers.push(handle);
        }

        Ok(Self {
            job_tx,
            workers,
            shutdown,
            stats,
            http_event_tx,
            net_event_tx,
        })
    }

    /// Get a thread-safe handle for submitting jobs
    ///
    /// The handle can be cloned and shared across threads.
    pub fn handle(&self) -> EngineHandle {
        EngineHandle {
            job_tx: self.job_tx.clone(),
            stats: self.stats.clone(),
        }
    }

    /// Get the engine statistics
    pub fn stats(&self) -> &EngineStats {
        &self.stats
    }

    /// Get the HTTP event sender for event-driven HTTP server dispatch
    ///
    /// Returns `None` if HTTP events were not enabled via `enable_http_events()`.
    /// Clone the sender to share it with HTTP server components.
    pub fn http_event_sender(&self) -> Option<Sender<HttpEvent>> {
        self.http_event_tx.clone()
    }

    /// Get the Net event sender for TCP server/socket dispatch
    ///
    /// Always available - net events are always enabled.
    /// Clone the sender to share it with net module components.
    pub fn net_event_sender(&self) -> Option<Sender<NetEvent>> {
        self.net_event_tx.clone()
    }

    /// Shutdown the engine and wait for all workers to finish
    pub async fn shutdown(self) {
        self.shutdown.store(true, Ordering::SeqCst);

        // Send shutdown signal to all workers
        for _ in &self.workers {
            let _ = self.job_tx.send(Job::Shutdown);
        }

        // Wait for workers to finish (use spawn_blocking to avoid blocking async runtime)
        let workers = self.workers;
        tokio::task::spawn_blocking(move || {
            for worker in workers {
                let _ = worker.join();
            }
        })
        .await
        .ok();
    }

    /// Check if the engine is still running
    pub fn is_running(&self) -> bool {
        !self.shutdown.load(Ordering::SeqCst)
    }

    /// Get the number of worker threads
    pub fn pool_size(&self) -> usize {
        self.workers.len()
    }
}

/// Thread-safe handle for submitting JavaScript execution jobs
///
/// This handle is `Send + Sync + Clone` and can be freely shared
/// across threads. All JavaScript execution happens on dedicated
/// worker threads.
#[derive(Clone)]
pub struct EngineHandle {
    job_tx: Sender<Job>,
    stats: Arc<EngineStats>,
}

// SAFETY: EngineHandle only holds a crossbeam Sender and Arc which are already Send + Sync
unsafe impl Send for EngineHandle {}
unsafe impl Sync for EngineHandle {}

impl EngineHandle {
    /// Evaluate JavaScript code and return the result as JSON
    ///
    /// The script is executed on a worker thread. The returned future
    /// resolves when execution completes.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # async fn example(handle: otter_runtime::EngineHandle) {
    /// let result = handle.eval("1 + 1").await.unwrap();
    /// assert_eq!(result, serde_json::json!(2));
    /// # }
    /// ```
    pub async fn eval(&self, script: impl Into<String>) -> JscResult<serde_json::Value> {
        let (tx, rx) = oneshot::channel();
        self.stats.jobs_submitted.fetch_add(1, Ordering::Relaxed);
        self.job_tx
            .send(Job::Eval {
                script: script.into(),
                source_url: None,
                response: tx,
            })
            .map_err(|_| JscError::internal("Engine shut down"))?;

        rx.await
            .map_err(|_| JscError::internal("Worker dropped response"))?
    }

    /// Evaluate JavaScript code with a source URL for error messages
    ///
    /// The source URL appears in stack traces and error messages.
    pub async fn eval_with_source(
        &self,
        script: impl Into<String>,
        source_url: impl Into<String>,
    ) -> JscResult<serde_json::Value> {
        let (tx, rx) = oneshot::channel();
        self.stats.jobs_submitted.fetch_add(1, Ordering::Relaxed);
        self.job_tx
            .send(Job::Eval {
                script: script.into(),
                source_url: Some(source_url.into()),
                response: tx,
            })
            .map_err(|_| JscError::internal("Engine shut down"))?;

        rx.await
            .map_err(|_| JscError::internal("Worker dropped response"))?
    }

    /// Evaluate TypeScript code (transpiled via SWC, then executed)
    ///
    /// The code is transpiled to JavaScript before execution.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # async fn example(handle: otter_runtime::EngineHandle) {
    /// let result = handle
    ///     .eval_typescript("const x: number = 42; x * 2")
    ///     .await
    ///     .unwrap();
    /// assert_eq!(result, serde_json::json!(84));
    /// # }
    /// ```
    pub async fn eval_typescript(&self, code: impl Into<String>) -> JscResult<serde_json::Value> {
        let (tx, rx) = oneshot::channel();
        self.stats.jobs_submitted.fetch_add(1, Ordering::Relaxed);
        self.job_tx
            .send(Job::EvalTypeScript {
                code: code.into(),
                source_url: None,
                response: tx,
            })
            .map_err(|_| JscError::internal("Engine shut down"))?;

        rx.await
            .map_err(|_| JscError::internal("Worker dropped response"))?
    }

    /// Evaluate TypeScript code with a source URL
    pub async fn eval_typescript_with_source(
        &self,
        code: impl Into<String>,
        source_url: impl Into<String>,
    ) -> JscResult<serde_json::Value> {
        let (tx, rx) = oneshot::channel();
        self.stats.jobs_submitted.fetch_add(1, Ordering::Relaxed);
        self.job_tx
            .send(Job::EvalTypeScript {
                code: code.into(),
                source_url: Some(source_url.into()),
                response: tx,
            })
            .map_err(|_| JscError::internal("Engine shut down"))?;

        rx.await
            .map_err(|_| JscError::internal("Worker dropped response"))?
    }

    /// Call a global function with arguments
    ///
    /// The function must be defined in the global scope.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # async fn example(handle: otter_runtime::EngineHandle) {
    /// // First define a function
    /// handle.eval("function add(a, b) { return a + b; }").await.unwrap();
    ///
    /// // Then call it
    /// let result = handle
    ///     .call("add", vec![serde_json::json!(1), serde_json::json!(2)])
    ///     .await
    ///     .unwrap();
    /// assert_eq!(result, serde_json::json!(3));
    /// # }
    /// ```
    pub async fn call(
        &self,
        function: impl Into<String>,
        args: Vec<serde_json::Value>,
    ) -> JscResult<serde_json::Value> {
        let (tx, rx) = oneshot::channel();
        self.stats.jobs_submitted.fetch_add(1, Ordering::Relaxed);
        self.job_tx
            .send(Job::Call {
                function: function.into(),
                args,
                response: tx,
            })
            .map_err(|_| JscError::internal("Engine shut down"))?;

        rx.await
            .map_err(|_| JscError::internal("Worker dropped response"))?
    }

    /// Try to submit a job without blocking (returns error if queue is full)
    ///
    /// This is useful for implementing backpressure. The returned receiver
    /// can be awaited to get the result.
    pub fn try_eval(
        &self,
        script: impl Into<String>,
    ) -> JscResult<oneshot::Receiver<JscResult<serde_json::Value>>> {
        let (tx, rx) = oneshot::channel();
        self.stats.jobs_submitted.fetch_add(1, Ordering::Relaxed);
        self.job_tx
            .try_send(Job::Eval {
                script: script.into(),
                source_url: None,
                response: tx,
            })
            .map_err(|e| match e {
                crossbeam_channel::TrySendError::Full(_) => JscError::Core(
                    otter_jsc_core::JscError::ResourceLimit("Job queue full".into()),
                ),
                crossbeam_channel::TrySendError::Disconnected(_) => {
                    JscError::internal("Engine shut down")
                }
            })?;

        Ok(rx)
    }

    /// Get access to the engine statistics
    pub fn stats(&self) -> &EngineStats {
        &self.stats
    }

    /// Check if the underlying channel is still connected
    pub fn is_connected(&self) -> bool {
        !self.job_tx.is_full() || self.job_tx.capacity().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_engine_builder_defaults() {
        let builder = EngineBuilder::default();
        assert!(builder.pool_size >= 1);
        assert_eq!(builder.queue_capacity, 1024);
        assert!(builder.extensions.is_empty());
    }

    #[tokio::test]
    async fn test_engine_builder_config() {
        let builder = EngineBuilder::default().pool_size(2).queue_capacity(100);

        assert_eq!(builder.pool_size, 2);
        assert_eq!(builder.queue_capacity, 100);
    }

    #[tokio::test]
    async fn test_engine_builder_min_values() {
        let builder = EngineBuilder::default().pool_size(0).queue_capacity(0);

        assert_eq!(builder.pool_size, 1);
        assert_eq!(builder.queue_capacity, 1);
    }

    #[test]
    fn test_handle_send_sync() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}
        fn assert_clone<T: Clone>() {}

        assert_send::<EngineHandle>();
        assert_sync::<EngineHandle>();
        assert_clone::<EngineHandle>();
    }
}
