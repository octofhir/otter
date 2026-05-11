//! Worker isolate handles and isolate-pool routing.
//!
//! The runtime worker model is isolate-per-worker: each worker owns a
//! separate runtime runner and therefore a separate VM, runtime state,
//! and GC heap. This module provides the host-facing handle shape while
//! keeping JS-visible `Worker`, message ports, and transferables for
//! later slices.
//!
//! # Contents
//!
//! - [`Worker`] — sendable handle to one worker isolate.
//! - [`WorkerBuilder`] — configuration for one worker.
//! - [`OtterPool`] — small round-robin isolate pool prototype.
//!
//! # Invariants
//!
//! - A worker is backed by its own [`crate::RuntimeHandle`]; no heap
//!   or VM state is shared between workers.
//! - Worker methods accept only owned public inputs and return
//!   [`crate::ExecutionResult`] / [`crate::OtterError`].
//! - Structured worker messages must use
//!   [`crate::StructuredCloneValue`], not `otter_vm::Value` or GC
//!   handles.
//!
//! # See also
//!
//! - [Event loop](../../../docs/book/src/engine/event-loop.md)
//! - [Runtime architecture](../../../docs/book/src/engine/architecture.md)

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use crate::module_loader;
use crate::{
    CapabilitySet, ExecutionResult, OtterError, RuntimeActivityStats, RuntimeBuilder,
    RuntimeHandle, SourceInput, StructuredCloneTransferList, StructuredCloneValue,
};

static NEXT_WORKER_ID: AtomicU64 = AtomicU64::new(1);

/// Stable host-side worker identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorkerId(u64);

impl WorkerId {
    /// Numeric worker id. Monotonic within this process.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Sendable handle to one worker isolate.
#[derive(Clone, Debug)]
pub struct Worker {
    id: WorkerId,
    handle: RuntimeHandle,
}

/// Worker shutdown / leak diagnostic snapshot.
#[derive(Debug, Clone)]
pub struct WorkerShutdownReport {
    /// Worker id.
    pub worker_id: WorkerId,
    /// Runtime handle references still pointing at this isolate.
    pub live_runtime_handles: usize,
    /// Commands queued from the handle side at report time.
    pub queued_messages: usize,
    /// Runtime activity at the time of the report.
    pub activity: RuntimeActivityStats,
    /// Transferable resources still owned by the worker boundary.
    ///
    /// This is always zero until message ports and ArrayBuffer
    /// transfer ownership land; keeping it in the report now fixes
    /// the public diagnostic shape.
    pub leaked_transferables: usize,
}

impl WorkerShutdownReport {
    /// `true` if any shutdown-relevant work/resource is still live.
    #[must_use]
    pub fn has_leaks(&self) -> bool {
        self.activity.queued_commands > 0
            || self.queued_messages > 0
            || self.live_runtime_handles > 1
            || self.activity.pending_ref_host_ops > 0
            || self.activity.pending_unref_host_ops > 0
            || self.activity.pending_ref_timers > 0
            || self.activity.pending_unref_timers > 0
            || self.activity.pending_dynamic_module_jobs > 0
            || self.leaked_transferables > 0
    }
}

impl Worker {
    /// Start configuring a worker isolate.
    #[must_use]
    pub fn builder() -> WorkerBuilder {
        WorkerBuilder::default()
    }

    /// Build a worker with default runtime configuration.
    ///
    /// # Errors
    /// Returns [`OtterError`] if the runtime isolate cannot start.
    pub fn new() -> Result<Self, OtterError> {
        Self::builder().build()
    }

    /// Host-side worker id.
    #[must_use]
    pub const fn id(&self) -> WorkerId {
        self.id
    }

    /// Run a file from disk on this worker isolate.
    ///
    /// # Errors
    /// See [`OtterError`].
    pub async fn run_file(&self, path: impl AsRef<Path>) -> Result<ExecutionResult, OtterError> {
        self.handle.run_file(path.as_ref().to_path_buf()).await
    }

    /// Run an ES module entry file on this worker isolate.
    ///
    /// # Errors
    /// See [`OtterError`].
    pub async fn run_module(&self, path: impl AsRef<Path>) -> Result<ExecutionResult, OtterError> {
        self.handle.run_module(path.as_ref().to_path_buf()).await
    }

    /// Run JavaScript source on this worker isolate.
    ///
    /// # Errors
    /// See [`OtterError`].
    pub async fn run_script(&self, source: &str) -> Result<ExecutionResult, OtterError> {
        self.handle
            .run_script(
                SourceInput::from_javascript(source),
                worker_specifier(self.id),
            )
            .await
    }

    /// Run TypeScript source on this worker isolate.
    ///
    /// # Errors
    /// See [`OtterError`].
    pub async fn run_typescript(&self, source: &str) -> Result<ExecutionResult, OtterError> {
        self.handle
            .run_script(
                SourceInput::from_typescript(source),
                worker_specifier(self.id),
            )
            .await
    }

    /// Evaluate JavaScript source on this worker isolate.
    ///
    /// # Errors
    /// See [`OtterError`].
    pub async fn eval(&self, source: &str) -> Result<ExecutionResult, OtterError> {
        self.handle.eval(SourceInput::from_javascript(source)).await
    }

    /// Validate that a message already crossed the structured-clone
    /// boundary. Full JS delivery lands with message ports in a later
    /// task-92 slice.
    #[must_use]
    pub fn accepts_message(&self, _message: &StructuredCloneValue) -> bool {
        true
    }

    /// Validate transfer-list metadata for a future message send.
    #[must_use]
    pub fn accepts_transfer_list(&self, transfers: &StructuredCloneTransferList) -> bool {
        transfers.validate().is_ok()
    }

    /// Cooperative cancellation for this worker isolate.
    pub fn interrupt(&self) {
        self.handle.interrupt();
    }

    /// Snapshot worker activity counters.
    #[must_use]
    pub fn activity_stats(&self) -> RuntimeActivityStats {
        self.handle.activity_stats()
    }

    /// Snapshot shutdown diagnostics without tearing down the worker.
    #[must_use]
    pub fn shutdown_report(&self) -> WorkerShutdownReport {
        let activity = self.activity_stats();
        WorkerShutdownReport {
            worker_id: self.id,
            live_runtime_handles: self.handle.live_handle_count(),
            queued_messages: activity.queued_commands,
            activity,
            leaked_transferables: 0,
        }
    }

    /// Drop down to the sendable runtime handle.
    #[must_use]
    pub fn handle(&self) -> &RuntimeHandle {
        &self.handle
    }
}

/// Builder for one worker isolate.
#[derive(Debug, Clone, Default)]
pub struct WorkerBuilder {
    runtime: RuntimeBuilder,
}

impl WorkerBuilder {
    /// Replace the capability set.
    #[must_use]
    pub fn capabilities(mut self, caps: CapabilitySet) -> Self {
        self.runtime = self.runtime.capabilities(caps);
        self
    }

    /// Hard heap cap. `0` disables the cap.
    #[must_use]
    pub fn max_heap_bytes(mut self, bytes: u64) -> Self {
        self.runtime = self.runtime.max_heap_bytes(bytes);
        self
    }

    /// Per-command timeout. `Duration::ZERO` disables the timeout.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.runtime = self.runtime.timeout(timeout);
        self
    }

    /// JS call-stack depth cap.
    #[must_use]
    pub fn max_stack_depth(mut self, depth: u32) -> Self {
        self.runtime = self.runtime.max_stack_depth(depth);
        self
    }

    /// Override the module-loader configuration.
    #[must_use]
    pub fn module_loader(mut self, loader: module_loader::LoaderConfig) -> Self {
        self.runtime = self.runtime.module_loader(loader);
        self
    }

    /// Construct a worker isolate.
    ///
    /// # Errors
    /// Returns [`OtterError`] when config validation or isolate
    /// startup fails.
    pub fn build(self) -> Result<Worker, OtterError> {
        let id = WorkerId(NEXT_WORKER_ID.fetch_add(1, Ordering::Relaxed));
        Ok(Worker {
            id,
            handle: self.runtime.build_handle()?,
        })
    }
}

/// Round-robin pool of independent worker isolates.
#[derive(Clone, Debug)]
pub struct OtterPool {
    workers: Arc<[Worker]>,
    next: Arc<AtomicUsize>,
}

impl OtterPool {
    /// Start configuring an isolate pool.
    #[must_use]
    pub fn builder() -> OtterPoolBuilder {
        OtterPoolBuilder::default()
    }

    /// Number of workers in the pool.
    #[must_use]
    pub fn len(&self) -> usize {
        self.workers.len()
    }

    /// `true` when the pool has no workers.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.workers.is_empty()
    }

    /// Borrow the worker slice for diagnostics/tests.
    #[must_use]
    pub fn workers(&self) -> &[Worker] {
        &self.workers
    }

    /// Snapshot shutdown diagnostics for every worker.
    #[must_use]
    pub fn shutdown_reports(&self) -> Vec<WorkerShutdownReport> {
        self.workers.iter().map(Worker::shutdown_report).collect()
    }

    /// Pick the next worker using round-robin routing.
    #[must_use]
    pub fn next_worker(&self) -> Worker {
        let idx = self.next.fetch_add(1, Ordering::Relaxed) % self.workers.len();
        self.workers[idx].clone()
    }

    /// Run JavaScript on the next worker.
    ///
    /// # Errors
    /// See [`OtterError`].
    pub async fn run_script(&self, source: &str) -> Result<ExecutionResult, OtterError> {
        self.next_worker().run_script(source).await
    }

    /// Run TypeScript on the next worker.
    ///
    /// # Errors
    /// See [`OtterError`].
    pub async fn run_typescript(&self, source: &str) -> Result<ExecutionResult, OtterError> {
        self.next_worker().run_typescript(source).await
    }

    /// Run a file on the next worker.
    ///
    /// # Errors
    /// See [`OtterError`].
    pub async fn run_file(&self, path: impl AsRef<Path>) -> Result<ExecutionResult, OtterError> {
        self.next_worker().run_file(path.as_ref()).await
    }
}

/// Builder for [`OtterPool`].
#[derive(Debug, Clone)]
pub struct OtterPoolBuilder {
    runtime: RuntimeBuilder,
    workers: usize,
}

impl Default for OtterPoolBuilder {
    fn default() -> Self {
        Self {
            runtime: RuntimeBuilder::default(),
            workers: 1,
        }
    }
}

impl OtterPoolBuilder {
    /// Number of worker isolates to spawn. Values below one are
    /// rejected at [`Self::build`].
    #[must_use]
    pub fn workers(mut self, workers: usize) -> Self {
        self.workers = workers;
        self
    }

    /// Replace the capability set for every worker.
    #[must_use]
    pub fn capabilities(mut self, caps: CapabilitySet) -> Self {
        self.runtime = self.runtime.capabilities(caps);
        self
    }

    /// Hard heap cap per worker. `0` disables each cap.
    #[must_use]
    pub fn max_heap_bytes(mut self, bytes: u64) -> Self {
        self.runtime = self.runtime.max_heap_bytes(bytes);
        self
    }

    /// Per-command timeout for every worker.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.runtime = self.runtime.timeout(timeout);
        self
    }

    /// JS call-stack depth cap for every worker.
    #[must_use]
    pub fn max_stack_depth(mut self, depth: u32) -> Self {
        self.runtime = self.runtime.max_stack_depth(depth);
        self
    }

    /// Override the module-loader configuration for every worker.
    #[must_use]
    pub fn module_loader(mut self, loader: module_loader::LoaderConfig) -> Self {
        self.runtime = self.runtime.module_loader(loader);
        self
    }

    /// Construct an isolate pool.
    ///
    /// # Errors
    /// Returns [`OtterError`] when config validation or isolate
    /// startup fails. A zero-worker pool is rejected as a config
    /// error because routing could not make progress.
    pub fn build(self) -> Result<OtterPool, OtterError> {
        if self.workers == 0 {
            return Err(OtterError::Config {
                reason: crate::ConfigError::ConflictingCapabilities {
                    message: "worker pool must contain at least one worker".to_string(),
                },
            });
        }

        let mut workers = Vec::with_capacity(self.workers);
        for _ in 0..self.workers {
            workers.push(
                WorkerBuilder {
                    runtime: self.runtime.clone(),
                }
                .build()?,
            );
        }

        Ok(OtterPool {
            workers: workers.into(),
            next: Arc::new(AtomicUsize::new(0)),
        })
    }
}

fn worker_specifier(id: WorkerId) -> String {
    format!("<worker:{}>", id.get())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_sync_static<T: Send + Sync + 'static>() {}

    #[test]
    fn worker_handles_are_send_sync_static() {
        assert_send_sync_static::<Worker>();
        assert_send_sync_static::<OtterPool>();
        assert_send_sync_static::<WorkerShutdownReport>();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_workers_run_concurrently_with_separate_globals() {
        let left = Worker::new().unwrap();
        let right = Worker::new().unwrap();

        let (left_result, right_result) = tokio::join!(
            left.run_script("globalThis.workerSlot = 7; workerSlot"),
            right.run_script("typeof globalThis.workerSlot"),
        );

        assert_eq!(left_result.unwrap().completion_string(), "7");
        assert_eq!(right_result.unwrap().completion_string(), "undefined");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pool_routes_round_robin_across_isolates() {
        let pool = OtterPool::builder().workers(2).build().unwrap();

        let first = pool.next_worker();
        let second = pool.next_worker();
        let third = pool.next_worker();

        assert_eq!(pool.len(), 2);
        assert_ne!(first.id(), second.id());
        assert_eq!(first.id(), third.id());

        first.run_script("globalThis.onlyFirst = 1").await.unwrap();
        let second_read = second
            .run_script("typeof globalThis.onlyFirst")
            .await
            .unwrap();

        assert_eq!(second_read.completion_string(), "undefined");
    }

    #[test]
    fn zero_worker_pool_is_rejected() {
        let err = OtterPool::builder().workers(0).build().unwrap_err();
        assert!(matches!(err, OtterError::Config { .. }));
    }

    #[test]
    fn worker_message_boundary_accepts_only_structured_clone_payload() {
        let worker = Worker::new().unwrap();
        let message = StructuredCloneValue::Object(vec![crate::StructuredCloneProperty {
            key: "ok".to_string(),
            value: StructuredCloneValue::Boolean(true),
        }]);
        let transfers = StructuredCloneTransferList::empty();

        assert!(worker.accepts_message(&message));
        assert!(worker.accepts_transfer_list(&transfers));
    }

    #[test]
    fn shutdown_report_marks_pending_timer_as_leak() {
        use crate::event_loop::TimerRequest;

        let worker = Worker::new().unwrap();
        let token = worker.handle().schedule_timer(TimerRequest {
            delay: Duration::from_secs(60),
            repeat: None,
        });

        let report = worker.shutdown_report();

        assert_eq!(report.worker_id, worker.id());
        assert_eq!(report.live_runtime_handles, 1);
        assert_eq!(report.queued_messages, report.activity.queued_commands);
        assert!(report.has_leaks());
        assert_eq!(report.activity.pending_ref_timers, 1);
        assert!(worker.handle().cancel_timer(token));
    }

    #[test]
    fn shutdown_report_tracks_live_handle_refs() {
        let worker = Worker::new().unwrap();
        let cloned = worker.clone();

        let report = worker.shutdown_report();

        assert_eq!(report.live_runtime_handles, 2);
        assert!(report.has_leaks());
        drop(cloned);
    }
}
