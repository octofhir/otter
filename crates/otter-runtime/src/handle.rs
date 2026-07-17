//! Sendable runtime command handle and isolate runner.
//!
//! [`RuntimeHandle`] is the Layer-B async boundary described in the mdBook
//! event-loop docs. It is
//! cloneable and `Send + Sync`; every command crosses an owned message
//! channel into a dedicated isolate runner thread that constructs and
//! owns the local [`crate::Runtime`].
//!
//! # Contents
//!
//! - [`RuntimeHandle`] — public command API.
//! - [`RuntimeActivityStats`] — cheap aggregate counters.
//! - isolate-runner message loop.
//!
//! # Invariants
//!
//! - VM and GC values never leave the isolate runner.
//! - Command replies carry only owned public data.
//! - Dropping a waiting future does not drop the isolate mid-turn; the
//!   runner observes the cancelled reply channel at the completion point.
//! - Public commands never execute recursively. Commands received while the
//!   current turn drains Ref'd work are deferred in FIFO order, and the shared
//!   queue bound accounts for both channel-resident and deferred commands.
//!
//! # See also
//!
//! - [Event loop](../../../docs/book/src/engine/event-loop.md)
//! - [Runtime architecture](../../../docs/book/src/engine/architecture.md)

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::time::Duration;

use tokio::sync::oneshot;

use crate::event_loop::{
    EventLoop, RuntimeLiveness, TimerRequest, TimerToken, TimerWake, TokioEventLoop,
};
use crate::host_services::{HttpsModuleFetchSink, HttpsModuleFetcherHandle};
use crate::runtime_activity::{
    RuntimeActivityAccounting, RuntimeKeepAlive, RuntimeTask, RuntimeTaskQueue, RuntimeTaskSpawner,
};
use crate::{
    DiagnosticCode, DynamicImportBegin, ExecutionAttempt, ExecutionResult, OtterError, Runtime,
    RuntimeConfig, SourceInput, TimerFireOutcome,
};
use otter_vm::{DynamicImportLoader, TimerScheduler};

use crate::promise_registry::{HostSettleOutcome, PromiseId};

const DEFAULT_COMMAND_CAPACITY: usize = 64;
const ISOLATE_THREAD_STACK_BYTES: usize = 16 * 1024 * 1024;

type RunReply = oneshot::Sender<ExecutionAttempt>;
type CheckReply = oneshot::Sender<Result<(), OtterError>>;

type CommandId = u64;

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ModuleJobId(u64);

/// Cheap activity counters exposed for tests and diagnostics.
#[derive(Debug, Clone, Default)]
pub struct RuntimeActivityStats {
    /// Commands currently queued from the handle side.
    pub queued_commands: usize,
    /// Commands accepted by the handle.
    pub submitted_commands: u64,
    /// Commands completed successfully.
    pub completed_commands: u64,
    /// Commands completed with an error.
    pub failed_commands: u64,
    /// Commands whose waiter timed out.
    pub timed_out_commands: u64,
    /// Commands whose waiter was dropped before the reply was sent.
    pub cancelled_waiters: u64,
    /// Commands rejected because the bounded queue was full.
    pub backpressure_rejections: u64,
    /// Interrupt requests sent to the isolate.
    pub interrupts: u64,
    /// Referenced host operations still pending.
    pub pending_ref_host_ops: usize,
    /// Unreferenced host operations still pending.
    pub pending_unref_host_ops: usize,
    /// Host operations completed successfully.
    pub completed_host_ops: u64,
    /// Host operations completed with an error.
    pub failed_host_ops: u64,
    /// Host operations cancelled before completion.
    pub cancelled_host_ops: u64,
    /// Referenced timers still pending.
    pub pending_ref_timers: usize,
    /// Unreferenced timers still pending.
    pub pending_unref_timers: usize,
    /// Timers that fired.
    pub fired_timers: u64,
    /// Timers cancelled before firing.
    pub cancelled_timers: u64,
    /// Dynamic module jobs still pending.
    pub pending_dynamic_module_jobs: usize,
    /// Dynamic module jobs completed.
    pub completed_dynamic_module_jobs: u64,
    /// Runtime diagnostics emitted.
    pub diagnostics: u64,
    /// Whether the isolate is currently running a command.
    pub running_command: bool,
    /// Whether VM microtasks were pending at the last runner safepoint.
    pub pending_microtasks: bool,
    /// VM microtask generation observed at the last runner safepoint.
    pub microtask_generation: u64,
    /// `true` after the handle has begun shutdown.
    pub shutdown: bool,
}

/// Cloneable, sendable runtime command API.
#[derive(Clone)]
pub struct RuntimeHandle {
    inner: Arc<RuntimeHandleInner>,
}

impl std::fmt::Debug for RuntimeHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeHandle")
            .field("activity_stats", &self.activity_stats())
            .finish_non_exhaustive()
    }
}

struct RuntimeHandleInner {
    tx: SyncSender<RuntimeMessage>,
    runner: Mutex<Option<std::thread::JoinHandle<()>>>,
    event_loop: TokioEventLoop,
    interrupt: otter_vm::InterruptFlag,
    command_timeout: Duration,
    command_capacity: usize,
    counters: Arc<RuntimeCounters>,
}

struct RuntimeCounters {
    queued_commands: AtomicUsize,
    submitted_commands: AtomicU64,
    completed_commands: AtomicU64,
    failed_commands: AtomicU64,
    timed_out_commands: AtomicU64,
    cancelled_waiters: AtomicU64,
    backpressure_rejections: AtomicU64,
    interrupts: AtomicU64,
    pending_ref_host_ops: AtomicUsize,
    pending_unref_host_ops: AtomicUsize,
    completed_host_ops: AtomicU64,
    failed_host_ops: AtomicU64,
    cancelled_host_ops: AtomicU64,
    pending_ref_timers: AtomicUsize,
    pending_unref_timers: AtomicUsize,
    fired_timers: AtomicU64,
    cancelled_timers: AtomicU64,
    pending_dynamic_module_jobs: AtomicUsize,
    completed_dynamic_module_jobs: AtomicU64,
    diagnostics: AtomicU64,
    running_command: AtomicBool,
    pending_microtasks: AtomicBool,
    microtask_generation: AtomicU64,
    next_command_id: AtomicU64,
    #[cfg(test)]
    next_module_job_id: AtomicU64,
    shutdown: AtomicBool,
}

struct InboxRuntimeTaskQueue {
    tx: SyncSender<RuntimeMessage>,
    counters: Arc<RuntimeCounters>,
}

impl RuntimeTaskQueue for InboxRuntimeTaskQueue {
    fn enqueue_boxed(
        &self,
        task: Box<dyn RuntimeTask>,
        liveness: RuntimeLiveness,
    ) -> Result<(), OtterError> {
        self.counters.retain_host_activity(liveness);
        match self
            .tx
            .try_send(RuntimeMessage::RuntimeTask { task, liveness })
        {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.counters.cancel_host_activity(liveness);
                self.counters
                    .backpressure_rejections
                    .fetch_add(1, Ordering::Relaxed);
                Err(OtterError::Internal {
                    code: DiagnosticCode::RuntimeBackpressure.as_str().to_string(),
                    message: "runtime inbox is full".to_string(),
                })
            }
            Err(TrySendError::Disconnected(_)) => {
                self.counters.cancel_host_activity(liveness);
                Err(OtterError::Internal {
                    code: DiagnosticCode::RuntimeShutdown.as_str().to_string(),
                    message: "runtime isolate is no longer accepting tasks".to_string(),
                })
            }
        }
    }
}

impl RuntimeActivityAccounting for RuntimeCounters {
    fn retain_host_activity(&self, liveness: RuntimeLiveness) {
        increment_liveness(
            liveness,
            &self.pending_ref_host_ops,
            &self.pending_unref_host_ops,
        );
    }

    fn complete_host_activity(&self, liveness: RuntimeLiveness) {
        decrement_liveness(
            liveness,
            &self.pending_ref_host_ops,
            &self.pending_unref_host_ops,
        );
        self.completed_host_ops.fetch_add(1, Ordering::Relaxed);
    }

    fn cancel_host_activity(&self, liveness: RuntimeLiveness) {
        decrement_liveness(
            liveness,
            &self.pending_ref_host_ops,
            &self.pending_unref_host_ops,
        );
        self.cancelled_host_ops.fetch_add(1, Ordering::Relaxed);
    }

    fn move_host_activity(&self, from: RuntimeLiveness, to: RuntimeLiveness) {
        decrement_liveness(
            from,
            &self.pending_ref_host_ops,
            &self.pending_unref_host_ops,
        );
        increment_liveness(to, &self.pending_ref_host_ops, &self.pending_unref_host_ops);
    }
}

enum RuntimeMessage {
    Command(RuntimeCommand),
    RuntimeTask {
        task: Box<dyn RuntimeTask>,
        liveness: RuntimeLiveness,
    },
    TimerFired {
        token: TimerToken,
        liveness: RuntimeLiveness,
        expects_js_callback: bool,
    },
    SettlePromise {
        id: PromiseId,
        outcome: HostSettleOutcome,
        liveness: RuntimeLiveness,
    },
    DynamicImportLoad {
        token: u64,
        specifier: String,
        referrer: String,
        liveness: RuntimeLiveness,
    },
    DynamicImportHttpsFetched {
        token: u64,
        target_url: String,
        result: Result<String, String>,
        liveness: RuntimeLiveness,
    },
    #[cfg(test)]
    DynamicModuleReady(ModuleJobId),
    #[cfg(test)]
    Diagnostic(RuntimeDiagnostic),
    Interrupt,
    Shutdown,
}

enum RuntimeCommand {
    CheckFile {
        id: CommandId,
        path: PathBuf,
        reply: CheckReply,
    },
    RunFile {
        id: CommandId,
        path: PathBuf,
        reply: RunReply,
    },
    RunScript {
        id: CommandId,
        source: SourceInput,
        specifier: String,
        reply: RunReply,
    },
    RunModule {
        id: CommandId,
        path: PathBuf,
        reply: RunReply,
    },
    Eval {
        id: CommandId,
        source: SourceInput,
        reply: RunReply,
    },
}

#[cfg(test)]
struct RuntimeDiagnostic {
    _origin: String,
    _message: String,
}

impl RuntimeHandle {
    /// Spawn an isolate runner with the default command capacity.
    ///
    /// # Errors
    /// Returns [`OtterError`] if the runtime config is invalid or the
    /// default Tokio runtime cannot be created.
    pub(crate) fn spawn(config: RuntimeConfig) -> Result<Self, OtterError> {
        Self::spawn_with_capacity(config, DEFAULT_COMMAND_CAPACITY)
    }

    /// Spawn an isolate runner with an explicit bounded queue size.
    ///
    /// # Errors
    /// Returns [`OtterError`] if the runtime config is invalid or the
    /// default Tokio runtime cannot be created.
    pub(crate) fn spawn_with_capacity(
        config: RuntimeConfig,
        capacity: usize,
    ) -> Result<Self, OtterError> {
        Runtime::validate_config(&config)?;
        let command_timeout = config.timeout();
        let event_loop = TokioEventLoop::current_or_owned().map_err(|e| OtterError::Internal {
            code: DiagnosticCode::TokioRuntimeCreate.as_str().to_string(),
            message: e.to_string(),
        })?;
        let (tx, rx) = sync_channel(capacity);
        let (interrupt_tx, interrupt_rx) = sync_channel(1);
        let counters = Arc::new(RuntimeCounters {
            queued_commands: AtomicUsize::new(0),
            submitted_commands: AtomicU64::new(0),
            completed_commands: AtomicU64::new(0),
            failed_commands: AtomicU64::new(0),
            timed_out_commands: AtomicU64::new(0),
            cancelled_waiters: AtomicU64::new(0),
            backpressure_rejections: AtomicU64::new(0),
            interrupts: AtomicU64::new(0),
            pending_ref_host_ops: AtomicUsize::new(0),
            pending_unref_host_ops: AtomicUsize::new(0),
            completed_host_ops: AtomicU64::new(0),
            failed_host_ops: AtomicU64::new(0),
            cancelled_host_ops: AtomicU64::new(0),
            pending_ref_timers: AtomicUsize::new(0),
            pending_unref_timers: AtomicUsize::new(0),
            fired_timers: AtomicU64::new(0),
            cancelled_timers: AtomicU64::new(0),
            pending_dynamic_module_jobs: AtomicUsize::new(0),
            completed_dynamic_module_jobs: AtomicU64::new(0),
            diagnostics: AtomicU64::new(0),
            running_command: AtomicBool::new(false),
            pending_microtasks: AtomicBool::new(false),
            microtask_generation: AtomicU64::new(0),
            next_command_id: AtomicU64::new(1),
            #[cfg(test)]
            next_module_job_id: AtomicU64::new(1),
            shutdown: AtomicBool::new(false),
        });
        let runner_counters = counters.clone();
        let scheduler_tx = tx.clone();
        let scheduler_event_loop = event_loop.clone();
        let runner = std::thread::Builder::new()
            .name("otter-isolate".to_string())
            .stack_size(ISOLATE_THREAD_STACK_BYTES)
            .spawn(move || {
                run_isolate(
                    config,
                    rx,
                    runner_counters,
                    interrupt_tx,
                    scheduler_tx,
                    scheduler_event_loop,
                )
            })
            .map_err(|e| OtterError::Internal {
                code: DiagnosticCode::IsolateSpawn.as_str().to_string(),
                message: e.to_string(),
            })?;
        let interrupt = interrupt_rx.recv().map_err(|_| OtterError::Internal {
            code: DiagnosticCode::IsolateStart.as_str().to_string(),
            message: "runtime isolate stopped before exposing its interrupt handle".to_string(),
        })?;
        let inner = Arc::new(RuntimeHandleInner {
            tx,
            runner: Mutex::new(Some(runner)),
            event_loop,
            interrupt,
            command_timeout,
            command_capacity: capacity,
            counters,
        });
        Ok(Self { inner })
    }

    /// Run a file through the isolate runner.
    ///
    /// # Errors
    /// See [`OtterError`].
    pub async fn run_file(&self, path: impl Into<PathBuf>) -> Result<ExecutionResult, OtterError> {
        self.run_file_with_diagnostics(path).await.into_result()
    }

    /// Run a file and retain partial JIT diagnostics on abrupt failure.
    pub async fn run_file_with_diagnostics(&self, path: impl Into<PathBuf>) -> ExecutionAttempt {
        let (reply, rx) = oneshot::channel();
        let id = self.next_command_id();
        if let Err(error) = self.submit(RuntimeCommand::RunFile {
            id,
            path: path.into(),
            reply,
        }) {
            return ExecutionAttempt::from_result(Err(error), None);
        }
        self.await_run_reply(rx).await
    }

    /// Parse and compile a file through the isolate runner without executing
    /// user code.
    ///
    /// # Errors
    /// See [`OtterError`].
    pub async fn check_file(&self, path: impl Into<PathBuf>) -> Result<(), OtterError> {
        let (reply, rx) = oneshot::channel();
        let id = self.next_command_id();
        self.submit(RuntimeCommand::CheckFile {
            id,
            path: path.into(),
            reply,
        })?;
        self.await_check_reply(rx).await
    }

    /// Run a JavaScript or TypeScript source bundle through the
    /// isolate runner.
    ///
    /// # Errors
    /// See [`OtterError`].
    pub async fn run_script(
        &self,
        source: SourceInput,
        specifier: impl Into<String>,
    ) -> Result<ExecutionResult, OtterError> {
        self.run_script_with_diagnostics(source, specifier)
            .await
            .into_result()
    }

    /// Run a source bundle and retain partial JIT diagnostics on failure.
    pub async fn run_script_with_diagnostics(
        &self,
        source: SourceInput,
        specifier: impl Into<String>,
    ) -> ExecutionAttempt {
        let (reply, rx) = oneshot::channel();
        let id = self.next_command_id();
        if let Err(error) = self.submit(RuntimeCommand::RunScript {
            id,
            source,
            specifier: specifier.into(),
            reply,
        }) {
            return ExecutionAttempt::from_result(Err(error), None);
        }
        self.await_run_reply(rx).await
    }

    /// Run a module entry file through the isolate runner.
    ///
    /// # Errors
    /// See [`OtterError`].
    pub async fn run_module(
        &self,
        path: impl Into<PathBuf>,
    ) -> Result<ExecutionResult, OtterError> {
        self.run_module_with_diagnostics(path).await.into_result()
    }

    /// Run a module and retain partial JIT diagnostics on abrupt failure.
    pub async fn run_module_with_diagnostics(&self, path: impl Into<PathBuf>) -> ExecutionAttempt {
        let (reply, rx) = oneshot::channel();
        let id = self.next_command_id();
        if let Err(error) = self.submit(RuntimeCommand::RunModule {
            id,
            path: path.into(),
            reply,
        }) {
            return ExecutionAttempt::from_result(Err(error), None);
        }
        self.await_run_reply(rx).await
    }

    /// Evaluate a source bundle through the isolate runner.
    ///
    /// # Errors
    /// See [`OtterError`].
    pub async fn eval(&self, source: SourceInput) -> Result<ExecutionResult, OtterError> {
        self.eval_with_diagnostics(source).await.into_result()
    }

    /// Evaluate a source bundle and retain partial JIT diagnostics on failure.
    pub async fn eval_with_diagnostics(&self, source: SourceInput) -> ExecutionAttempt {
        let (reply, rx) = oneshot::channel();
        let id = self.next_command_id();
        if let Err(error) = self.submit(RuntimeCommand::Eval { id, source, reply }) {
            return ExecutionAttempt::from_result(Err(error), None);
        }
        self.await_run_reply(rx).await
    }

    /// Request cooperative cancellation.
    pub fn interrupt(&self) {
        self.inner
            .counters
            .interrupts
            .fetch_add(1, Ordering::Relaxed);
        self.inner.interrupt.interrupt();
        let _ = self.inner.tx.try_send(RuntimeMessage::Interrupt);
    }

    /// Schedule a timer wake through the runtime inbox.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn schedule_timer(&self, request: TimerRequest) -> TimerToken {
        increment_liveness(
            RuntimeLiveness::Ref,
            &self.inner.counters.pending_ref_timers,
            &self.inner.counters.pending_unref_timers,
        );
        let wake = Arc::new(RuntimeTimerWake {
            tx: self.inner.tx.clone(),
            counters: self.inner.counters.clone(),
            liveness: RuntimeLiveness::Ref,
            repeat: request.repeat.is_some(),
            expects_js_callback: false,
        });
        self.inner.event_loop.schedule_timer(request, wake)
    }

    /// Settle a JS promise registered earlier via
    /// [`crate::Runtime::register_pending_promise`]. Posts a
    /// `SettlePromise` inbox message; the isolate runner picks it
    /// up on the next tick and resolves / rejects the matching
    /// [`otter_vm::JsPromiseHandle`] through the standard promise
    /// dispatch path so reactions land on the per-isolate
    /// microtask queue.
    ///
    /// The call accounts for one referenced host operation so the
    /// run-until-idle loop keeps the script alive until the
    /// settlement lands. Embedders that want fire-and-forget
    /// semantics should pass [`RuntimeLiveness::Unref`].
    ///
    /// A late or duplicate settlement (the host raced its own
    /// cancellation, or the matching script run has already
    /// returned and dropped its module) is a silent no-op.
    pub fn settle_promise(
        &self,
        id: PromiseId,
        outcome: HostSettleOutcome,
        liveness: RuntimeLiveness,
    ) {
        increment_liveness(
            liveness,
            &self.inner.counters.pending_ref_host_ops,
            &self.inner.counters.pending_unref_host_ops,
        );
        if self
            .inner
            .tx
            .try_send(RuntimeMessage::SettlePromise {
                id,
                outcome,
                liveness,
            })
            .is_err()
        {
            decrement_liveness(
                liveness,
                &self.inner.counters.pending_ref_host_ops,
                &self.inner.counters.pending_unref_host_ops,
            );
            self.inner
                .counters
                .failed_host_ops
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Retain one long-lived host resource in the runtime liveness counters.
    ///
    /// The returned guard must be closed when the host resource closes. Dropping
    /// the last guard also releases the hold, which covers error paths.
    #[must_use]
    pub fn retain_keep_alive(&self, liveness: RuntimeLiveness) -> RuntimeKeepAlive {
        let accounting: Arc<dyn RuntimeActivityAccounting> = self.inner.counters.clone();
        RuntimeKeepAlive::retain(accounting, liveness)
    }

    /// Enqueue an owned task to run on the isolate event-loop thread.
    ///
    /// The task is accounted as one host activity until the runner executes it.
    /// Feature crates should use this for cross-thread callbacks instead of
    /// calling into VM/JS from worker threads.
    ///
    /// # Errors
    /// Returns [`OtterError`] when the runtime inbox is full or shutting down.
    pub fn enqueue_runtime_task(
        &self,
        task: impl RuntimeTask,
        liveness: RuntimeLiveness,
    ) -> Result<(), OtterError> {
        self.task_spawner().enqueue(task, liveness)
    }

    /// Clone a sender for scheduling typed runtime tasks.
    #[must_use]
    pub fn task_spawner(&self) -> RuntimeTaskSpawner {
        RuntimeTaskSpawner::new(
            Arc::new(InboxRuntimeTaskQueue {
                tx: self.inner.tx.clone(),
                counters: self.inner.counters.clone(),
            }),
            self.inner.counters.clone(),
            Some(self.inner.event_loop.handle()),
        )
    }

    /// Cancel a pending timer.
    #[cfg(test)]
    pub(crate) fn cancel_timer(&self, token: TimerToken) -> bool {
        if !self.inner.event_loop.cancel_timer(token) {
            return false;
        }
        self.inner
            .counters
            .pending_ref_timers
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_sub(1))
            .or_else(|_| {
                self.inner.counters.pending_unref_timers.fetch_update(
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                    |v| v.checked_sub(1),
                )
            })
            .ok();
        self.inner
            .counters
            .cancelled_timers
            .fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Register and complete a synthetic dynamic module job.
    ///
    /// This keeps the task-85 inbox shape exercised until module
    /// graph loading itself grows asynchronous host work.
    #[doc(hidden)]
    #[cfg(test)]
    pub(crate) fn complete_dynamic_module_job_for_tests(&self) {
        self.inner
            .counters
            .pending_dynamic_module_jobs
            .fetch_add(1, Ordering::Relaxed);
        if self
            .inner
            .tx
            .try_send(RuntimeMessage::DynamicModuleReady(ModuleJobId(
                self.inner
                    .counters
                    .next_module_job_id
                    .fetch_add(1, Ordering::Relaxed),
            )))
            .is_err()
        {
            self.inner
                .counters
                .pending_dynamic_module_jobs
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_sub(1))
                .ok();
        }
    }

    /// Emit a diagnostic wake through the event-loop abstraction.
    /// Wake the runtime and emit a diagnostic inbox item.
    #[cfg(test)]
    pub(crate) fn wake_runtime(&self, origin: impl Into<String>) {
        let origin = origin.into();
        let _ = self
            .inner
            .tx
            .try_send(RuntimeMessage::Diagnostic(RuntimeDiagnostic {
                _origin: origin,
                _message: "runtime wake".to_string(),
            }));
    }

    fn next_command_id(&self) -> CommandId {
        self.inner
            .counters
            .next_command_id
            .fetch_add(1, Ordering::Relaxed)
    }

    /// Snapshot cheap activity counters.
    #[must_use]
    pub fn activity_stats(&self) -> RuntimeActivityStats {
        RuntimeActivityStats {
            queued_commands: self.inner.counters.queued_commands.load(Ordering::Relaxed),
            submitted_commands: self
                .inner
                .counters
                .submitted_commands
                .load(Ordering::Relaxed),
            completed_commands: self
                .inner
                .counters
                .completed_commands
                .load(Ordering::Relaxed),
            failed_commands: self.inner.counters.failed_commands.load(Ordering::Relaxed),
            timed_out_commands: self
                .inner
                .counters
                .timed_out_commands
                .load(Ordering::Relaxed),
            cancelled_waiters: self
                .inner
                .counters
                .cancelled_waiters
                .load(Ordering::Relaxed),
            backpressure_rejections: self
                .inner
                .counters
                .backpressure_rejections
                .load(Ordering::Relaxed),
            interrupts: self.inner.counters.interrupts.load(Ordering::Relaxed),
            pending_ref_host_ops: self
                .inner
                .counters
                .pending_ref_host_ops
                .load(Ordering::Relaxed),
            pending_unref_host_ops: self
                .inner
                .counters
                .pending_unref_host_ops
                .load(Ordering::Relaxed),
            completed_host_ops: self
                .inner
                .counters
                .completed_host_ops
                .load(Ordering::Relaxed),
            failed_host_ops: self.inner.counters.failed_host_ops.load(Ordering::Relaxed),
            cancelled_host_ops: self
                .inner
                .counters
                .cancelled_host_ops
                .load(Ordering::Relaxed),
            pending_ref_timers: self
                .inner
                .counters
                .pending_ref_timers
                .load(Ordering::Relaxed),
            pending_unref_timers: self
                .inner
                .counters
                .pending_unref_timers
                .load(Ordering::Relaxed),
            fired_timers: self.inner.counters.fired_timers.load(Ordering::Relaxed),
            cancelled_timers: self.inner.counters.cancelled_timers.load(Ordering::Relaxed),
            pending_dynamic_module_jobs: self
                .inner
                .counters
                .pending_dynamic_module_jobs
                .load(Ordering::Relaxed),
            completed_dynamic_module_jobs: self
                .inner
                .counters
                .completed_dynamic_module_jobs
                .load(Ordering::Relaxed),
            diagnostics: self.inner.counters.diagnostics.load(Ordering::Relaxed),
            running_command: self.inner.counters.running_command.load(Ordering::Relaxed),
            pending_microtasks: self
                .inner
                .counters
                .pending_microtasks
                .load(Ordering::Relaxed),
            microtask_generation: self
                .inner
                .counters
                .microtask_generation
                .load(Ordering::Relaxed),
            shutdown: self.inner.counters.shutdown.load(Ordering::Relaxed),
        }
    }

    /// Number of public handle clones still referencing this isolate.
    #[must_use]
    pub fn live_handle_count(&self) -> usize {
        Arc::strong_count(&self.inner)
    }

    pub(crate) fn block_on<F: std::future::Future>(&self, future: F) -> F::Output {
        self.inner.event_loop.block_on(future)
    }

    fn submit(&self, command: RuntimeCommand) -> Result<(), OtterError> {
        if self.inner.counters.shutdown.load(Ordering::Relaxed) {
            return Err(OtterError::Internal {
                code: DiagnosticCode::RuntimeShutdown.as_str().to_string(),
                message: "runtime handle is shut down".to_string(),
            });
        }
        if self
            .inner
            .counters
            .queued_commands
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |queued| {
                (queued < self.inner.command_capacity).then_some(queued + 1)
            })
            .is_err()
        {
            self.inner
                .counters
                .backpressure_rejections
                .fetch_add(1, Ordering::Relaxed);
            return Err(OtterError::Internal {
                code: DiagnosticCode::RuntimeBackpressure.as_str().to_string(),
                message: "runtime command queue is full".to_string(),
            });
        }
        match self.inner.tx.try_send(RuntimeMessage::Command(command)) {
            Ok(()) => {
                self.inner
                    .counters
                    .submitted_commands
                    .fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(TrySendError::Full(_)) => {
                self.inner
                    .counters
                    .queued_commands
                    .fetch_sub(1, Ordering::Relaxed);
                self.inner
                    .counters
                    .backpressure_rejections
                    .fetch_add(1, Ordering::Relaxed);
                Err(OtterError::Internal {
                    code: DiagnosticCode::RuntimeBackpressure.as_str().to_string(),
                    message: "runtime command queue is full".to_string(),
                })
            }
            Err(TrySendError::Disconnected(_)) => {
                self.inner
                    .counters
                    .queued_commands
                    .fetch_sub(1, Ordering::Relaxed);
                Err(OtterError::Internal {
                    code: DiagnosticCode::RuntimeClosed.as_str().to_string(),
                    message: "runtime isolate has stopped".to_string(),
                })
            }
        }
    }

    async fn await_run_reply(&self, rx: oneshot::Receiver<ExecutionAttempt>) -> ExecutionAttempt {
        let timeout = self.inner.command_timeout;
        let outcome = if timeout == Duration::ZERO {
            rx.await
        } else {
            match tokio::time::timeout(timeout, rx).await {
                Ok(outcome) => outcome,
                Err(_) => {
                    self.inner
                        .counters
                        .timed_out_commands
                        .fetch_add(1, Ordering::Relaxed);
                    self.inner
                        .counters
                        .failed_commands
                        .fetch_add(1, Ordering::Relaxed);
                    self.interrupt();
                    return ExecutionAttempt::from_result(
                        Err(OtterError::timeout_after(timeout)),
                        None,
                    );
                }
            }
        };
        match outcome {
            Ok(attempt) => {
                if attempt.result().is_ok() {
                    self.inner
                        .counters
                        .completed_commands
                        .fetch_add(1, Ordering::Relaxed);
                } else {
                    self.inner
                        .counters
                        .failed_commands
                        .fetch_add(1, Ordering::Relaxed);
                }
                attempt
            }
            Err(_) => ExecutionAttempt::from_result(
                Err(OtterError::Internal {
                    code: DiagnosticCode::RuntimeReplyDropped.as_str().to_string(),
                    message: "runtime isolate dropped command reply".to_string(),
                }),
                None,
            ),
        }
    }

    async fn await_check_reply(
        &self,
        rx: oneshot::Receiver<Result<(), OtterError>>,
    ) -> Result<(), OtterError> {
        let timeout = self.inner.command_timeout;
        let outcome = if timeout == Duration::ZERO {
            rx.await
        } else {
            match tokio::time::timeout(timeout, rx).await {
                Ok(outcome) => outcome,
                Err(_) => {
                    self.inner
                        .counters
                        .timed_out_commands
                        .fetch_add(1, Ordering::Relaxed);
                    self.inner
                        .counters
                        .failed_commands
                        .fetch_add(1, Ordering::Relaxed);
                    self.interrupt();
                    return Err(OtterError::timeout_after(timeout));
                }
            }
        };
        match outcome {
            Ok(Ok(())) => {
                self.inner
                    .counters
                    .completed_commands
                    .fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Ok(Err(err)) => {
                self.inner
                    .counters
                    .failed_commands
                    .fetch_add(1, Ordering::Relaxed);
                Err(err)
            }
            Err(_) => Err(OtterError::Internal {
                code: DiagnosticCode::RuntimeReplyDropped.as_str().to_string(),
                message: "runtime isolate dropped command reply".to_string(),
            }),
        }
    }
}

impl Drop for RuntimeHandleInner {
    fn drop(&mut self) {
        self.counters.shutdown.store(true, Ordering::Relaxed);
        let _ = self.tx.send(RuntimeMessage::Shutdown);
        if let Some(runner) = self.runner.lock().expect("runner mutex poisoned").take() {
            let _ = runner.join();
        }
    }
}

fn run_isolate(
    config: RuntimeConfig,
    rx: Receiver<RuntimeMessage>,
    counters: Arc<RuntimeCounters>,
    interrupt_tx: SyncSender<otter_vm::InterruptFlag>,
    scheduler_tx: SyncSender<RuntimeMessage>,
    event_loop: TokioEventLoop,
) {
    let runtime_task_spawner = RuntimeTaskSpawner::new(
        Arc::new(InboxRuntimeTaskQueue {
            tx: scheduler_tx.clone(),
            counters: counters.clone(),
        }),
        counters.clone(),
        Some(event_loop.handle()),
    );
    let mut runtime =
        match Runtime::from_config_with_task_spawner(config, Some(runtime_task_spawner)) {
            Ok(runtime) => runtime,
            Err(_) => return,
        };
    let https_module_fetcher = event_loop.https_module_fetcher();
    runtime.install_remote_module_fetch(event_loop.blocking_module_fetcher());
    let timer_scheduler = Arc::new(InboxTimerScheduler {
        tx: scheduler_tx.clone(),
        event_loop,
        counters: counters.clone(),
        next_immediate_token: AtomicU64::new(FIRST_IMMEDIATE_TOKEN),
    });
    runtime.install_timer_scheduler(timer_scheduler);
    runtime.install_host_completion_sink(Arc::new(
        crate::runtime_activity::SpawnerCompletionSink {
            spawner: runtime
                .runtime_task_spawner()
                .expect("isolate runner constructs the runtime with a task spawner"),
        },
    ));
    let dynamic_import_loader = Arc::new(InboxDynamicImportLoader {
        tx: scheduler_tx.clone(),
        counters: counters.clone(),
    });
    runtime.install_dynamic_import_loader(dynamic_import_loader);
    let _ = interrupt_tx.send(runtime.interrupt_handle().raw_flag());
    let mut runner = IsolateRunner {
        runtime,
        rx,
        tx: scheduler_tx,
        counters,
        https_module_fetcher,
        deferred_commands: VecDeque::new(),
        shutdown: false,
    };
    runner.run_until_idle();
}

/// Timer scheduler installed on the [`crate::Runtime`] inside the
/// isolate runner thread. Each `setTimeout` / `setInterval` native
/// call lands here, schedules a Tokio sleep through the event
/// loop, and posts back a [`RuntimeMessage::TimerFired`] when the
/// delay elapses so the runner re-enters the VM and runs the JS
/// callback.
///
/// The struct is `Send + Sync` because the
/// [`otter_vm::TimerSchedulerHandle`] alias requires both. The
/// fields satisfy that: `SyncSender` and `TokioEventLoop` are
/// `Clone + Send + Sync`; `RuntimeCounters` is wrapped in `Arc`.
/// No VM state crosses this boundary — the schedule callback only
/// ships the host-issued [`TimerToken`] back to the runner, which
/// is then resolved against the per-isolate
/// [`otter_vm::TimerCallbacks`] table.
struct InboxTimerScheduler {
    tx: SyncSender<RuntimeMessage>,
    event_loop: TokioEventLoop,
    counters: Arc<RuntimeCounters>,
    /// Monotonic counter for zero-delay tokens issued on the VM
    /// thread. Tokio's multi-thread runtime does not guarantee
    /// FIFO ordering across `sleep(Duration::ZERO)` spawns, so we
    /// short-circuit zero-delay timers by posting `TimerFired`
    /// straight to the inbox (which is FIFO). The counter starts
    /// at the high half of `u64` to keep these tokens disjoint
    /// from the Tokio-issued ones.
    next_immediate_token: AtomicU64,
}

const FIRST_IMMEDIATE_TOKEN: u64 = 1u64 << 63;

struct RuntimeTimerWake {
    tx: SyncSender<RuntimeMessage>,
    counters: Arc<RuntimeCounters>,
    liveness: RuntimeLiveness,
    repeat: bool,
    expects_js_callback: bool,
}

impl TimerWake for RuntimeTimerWake {
    fn timer_fired(&self, token: TimerToken) {
        if self
            .tx
            .try_send(RuntimeMessage::TimerFired {
                token,
                liveness: self.liveness,
                expects_js_callback: self.expects_js_callback,
            })
            .is_err()
            && !self.repeat
        {
            decrement_liveness(
                self.liveness,
                &self.counters.pending_ref_timers,
                &self.counters.pending_unref_timers,
            );
            self.counters
                .cancelled_timers
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

struct DynamicImportFetchWake {
    tx: SyncSender<RuntimeMessage>,
    counters: Arc<RuntimeCounters>,
    token: u64,
    target_url: String,
    liveness: RuntimeLiveness,
}

impl HttpsModuleFetchSink for DynamicImportFetchWake {
    fn fetched(&self, result: Result<String, String>) {
        if self
            .tx
            .try_send(RuntimeMessage::DynamicImportHttpsFetched {
                token: self.token,
                target_url: self.target_url.clone(),
                result,
                liveness: self.liveness,
            })
            .is_err()
        {
            decrement_liveness(
                self.liveness,
                &self.counters.pending_ref_host_ops,
                &self.counters.pending_unref_host_ops,
            );
            self.counters
                .failed_host_ops
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Dynamic-import scheduler installed on [`crate::Runtime`] from
/// inside the isolate runner. The VM-thread opcode hands us a
/// host-issued token + the resolved specifier; we post a
/// [`RuntimeMessage::DynamicImportLoad`] inbox message that, on
/// the next runner tick, drives the synchronous load + compile +
/// link + evaluate and settles the matching promise.
///
/// Both fields are `Send + Sync`: `SyncSender` clones, `Arc`
/// wraps the counters. The `String` payloads carry no VM state,
/// matching the Slice C `HostSettleOutcome` discipline.
struct InboxDynamicImportLoader {
    tx: SyncSender<RuntimeMessage>,
    counters: Arc<RuntimeCounters>,
}

impl DynamicImportLoader for InboxDynamicImportLoader {
    fn schedule(&self, token: u64, specifier: String, referrer: String) {
        let liveness = RuntimeLiveness::Ref;
        increment_liveness(
            liveness,
            &self.counters.pending_ref_host_ops,
            &self.counters.pending_unref_host_ops,
        );
        if self
            .tx
            .try_send(RuntimeMessage::DynamicImportLoad {
                token,
                specifier,
                referrer,
                liveness,
            })
            .is_err()
        {
            decrement_liveness(
                liveness,
                &self.counters.pending_ref_host_ops,
                &self.counters.pending_unref_host_ops,
            );
            self.counters
                .failed_host_ops
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl TimerScheduler for InboxTimerScheduler {
    fn schedule(&self, delay_ms: u64, repeat_ms: Option<u64>) -> u64 {
        let liveness = RuntimeLiveness::Ref;
        increment_liveness(
            liveness,
            &self.counters.pending_ref_timers,
            &self.counters.pending_unref_timers,
        );
        // Zero-delay one-shot timers route through the inbox
        // directly so multiple `setTimeout(..., 0)` calls fire in
        // FIFO scheduling order — Tokio's multi-thread spawn
        // scheduler does not guarantee that for `sleep(0)`.
        if delay_ms == 0 && repeat_ms.is_none() {
            let token = TimerToken(self.next_immediate_token.fetch_add(1, Ordering::Relaxed));
            if self
                .tx
                .try_send(RuntimeMessage::TimerFired {
                    token,
                    liveness,
                    expects_js_callback: true,
                })
                .is_err()
            {
                decrement_liveness(
                    liveness,
                    &self.counters.pending_ref_timers,
                    &self.counters.pending_unref_timers,
                );
                self.counters
                    .cancelled_timers
                    .fetch_add(1, Ordering::Relaxed);
            }
            return token.0;
        }
        let request = TimerRequest {
            delay: Duration::from_millis(delay_ms),
            repeat: repeat_ms.map(Duration::from_millis),
        };
        let wake = Arc::new(RuntimeTimerWake {
            tx: self.tx.clone(),
            counters: self.counters.clone(),
            liveness,
            repeat: repeat_ms.is_some(),
            expects_js_callback: true,
        });
        let token = self.event_loop.schedule_timer(request, wake);
        token.0
    }

    fn cancel(&self, token: u64) -> bool {
        // Immediate-token tokens have no Tokio handle to cancel;
        // the inbox already carries the `TimerFired` message, but
        // the per-isolate `TimerCallbacks` table was just emptied
        // by the JS-visible `clearTimeout` so the late fire is a
        // no-op. Decrement the liveness counter here so the
        // run-until-idle loop accounts for the cancellation.
        if token >= FIRST_IMMEDIATE_TOKEN {
            self.counters
                .pending_ref_timers
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_sub(1))
                .or_else(|_| {
                    self.counters.pending_unref_timers.fetch_update(
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                        |v| v.checked_sub(1),
                    )
                })
                .ok();
            self.counters
                .cancelled_timers
                .fetch_add(1, Ordering::Relaxed);
            return true;
        }
        let cancelled = self.event_loop.cancel_timer(TimerToken(token));
        if cancelled {
            self.counters
                .pending_ref_timers
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_sub(1))
                .or_else(|_| {
                    self.counters.pending_unref_timers.fetch_update(
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                        |v| v.checked_sub(1),
                    )
                })
                .ok();
            self.counters
                .cancelled_timers
                .fetch_add(1, Ordering::Relaxed);
        }
        cancelled
    }
}

struct IsolateRunner {
    runtime: Runtime,
    rx: Receiver<RuntimeMessage>,
    tx: SyncSender<RuntimeMessage>,
    counters: Arc<RuntimeCounters>,
    https_module_fetcher: HttpsModuleFetcherHandle,
    deferred_commands: VecDeque<RuntimeCommand>,
    shutdown: bool,
}

enum TickOutcome {
    Processed,
    Idle,
    Shutdown,
}

impl IsolateRunner {
    fn poll_one_tick(&mut self) -> TickOutcome {
        if let Some(command) = self.deferred_commands.pop_front() {
            return self.process_message(RuntimeMessage::Command(command));
        }
        let msg = match self.rx.try_recv() {
            Ok(msg) => msg,
            Err(std::sync::mpsc::TryRecvError::Empty) => return TickOutcome::Idle,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => return TickOutcome::Shutdown,
        };
        self.process_message(msg)
    }

    fn run_until_idle(&mut self) {
        loop {
            match self.poll_one_tick() {
                TickOutcome::Processed | TickOutcome::Idle => {}
                TickOutcome::Shutdown => return,
            }
            // Every poll consumes a message, so every outcome must be
            // honoured here — discarding a `Shutdown` consumed by this
            // poll would re-enter the blocking `recv()` while the
            // handle's `Drop` is parked in `join()` holding its `tx`
            // clone: the channel never disconnects and both threads
            // deadlock.
            match self.poll_one_tick() {
                TickOutcome::Processed => {}
                TickOutcome::Shutdown => return,
                TickOutcome::Idle => match self.rx.recv() {
                    Ok(msg) => {
                        if matches!(self.process_message(msg), TickOutcome::Shutdown) {
                            return;
                        }
                    }
                    Err(_) => return,
                },
            }
        }
    }

    fn shutdown(&mut self) {
        self.shutdown = true;
        self.counters.shutdown.store(true, Ordering::Relaxed);
    }

    fn process_message(&mut self, msg: RuntimeMessage) -> TickOutcome {
        match msg {
            RuntimeMessage::Command(command) => {
                self.counters
                    .queued_commands
                    .fetch_sub(1, Ordering::Relaxed);
                let id = command.id();
                self.run_command(command);
                self.record_microtask_snapshot();
                if self.shutdown {
                    return TickOutcome::Shutdown;
                }
                if id == 0 {
                    return TickOutcome::Idle;
                }
                TickOutcome::Processed
            }
            RuntimeMessage::RuntimeTask { task, liveness } => {
                let result = task.run(&mut self.runtime);
                decrement_liveness(
                    liveness,
                    &self.counters.pending_ref_host_ops,
                    &self.counters.pending_unref_host_ops,
                );
                match result {
                    Ok(()) => {
                        self.counters
                            .completed_host_ops
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => {
                        self.counters
                            .failed_host_ops
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }
                self.record_microtask_snapshot();
                TickOutcome::Processed
            }
            RuntimeMessage::TimerFired {
                token,
                liveness,
                expects_js_callback,
            } => {
                if !expects_js_callback {
                    self.counters.fired_timers.fetch_add(1, Ordering::Relaxed);
                    decrement_liveness(
                        liveness,
                        &self.counters.pending_ref_timers,
                        &self.counters.pending_unref_timers,
                    );
                    return TickOutcome::Processed;
                }
                // Drive the JS callback associated with `token`
                // through the runtime. A swallowed `Err` reply path
                // is intentional: the surrounding command (if any)
                // already returned its synchronous result, and the
                // diagnostic runs through the structured sink.
                match self.runtime.fire_timer(token.0) {
                    Ok(TimerFireOutcome::Missing) => {}
                    Ok(TimerFireOutcome::Fired { repeat }) => {
                        self.counters.fired_timers.fetch_add(1, Ordering::Relaxed);
                        if !repeat {
                            decrement_liveness(
                                liveness,
                                &self.counters.pending_ref_timers,
                                &self.counters.pending_unref_timers,
                            );
                        }
                    }
                    Err(_) => {
                        self.counters.fired_timers.fetch_add(1, Ordering::Relaxed);
                        decrement_liveness(
                            liveness,
                            &self.counters.pending_ref_timers,
                            &self.counters.pending_unref_timers,
                        );
                    }
                }
                self.record_microtask_snapshot();
                TickOutcome::Processed
            }
            RuntimeMessage::SettlePromise {
                id,
                outcome,
                liveness,
            } => {
                decrement_liveness(
                    liveness,
                    &self.counters.pending_ref_host_ops,
                    &self.counters.pending_unref_host_ops,
                );
                let _ = self.runtime.settle_pending_promise(id, outcome);
                self.record_microtask_snapshot();
                TickOutcome::Processed
            }
            RuntimeMessage::DynamicImportLoad {
                token,
                specifier,
                referrer,
                liveness,
            } => {
                match self
                    .runtime
                    .begin_dynamic_import(token, &specifier, &referrer)
                {
                    Ok(DynamicImportBegin::Settled) | Err(_) => {
                        decrement_liveness(
                            liveness,
                            &self.counters.pending_ref_host_ops,
                            &self.counters.pending_unref_host_ops,
                        );
                        self.record_microtask_snapshot();
                    }
                    Ok(DynamicImportBegin::FetchHttps { target_url }) => {
                        let sink = Arc::new(DynamicImportFetchWake {
                            tx: self.tx.clone(),
                            counters: self.counters.clone(),
                            token,
                            target_url: target_url.clone(),
                            liveness,
                        });
                        self.https_module_fetcher.fetch_utf8(target_url, sink);
                    }
                }
                TickOutcome::Processed
            }
            RuntimeMessage::DynamicImportHttpsFetched {
                token,
                target_url,
                result,
                liveness,
            } => {
                decrement_liveness(
                    liveness,
                    &self.counters.pending_ref_host_ops,
                    &self.counters.pending_unref_host_ops,
                );
                let _ = self
                    .runtime
                    .complete_dynamic_import_https(token, &target_url, result);
                self.record_microtask_snapshot();
                TickOutcome::Processed
            }
            #[cfg(test)]
            RuntimeMessage::DynamicModuleReady(_id) => {
                self.counters
                    .pending_dynamic_module_jobs
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_sub(1))
                    .ok();
                self.counters
                    .completed_dynamic_module_jobs
                    .fetch_add(1, Ordering::Relaxed);
                TickOutcome::Processed
            }
            #[cfg(test)]
            RuntimeMessage::Diagnostic(diagnostic) => {
                let _ = diagnostic;
                self.counters.diagnostics.fetch_add(1, Ordering::Relaxed);
                TickOutcome::Processed
            }
            RuntimeMessage::Interrupt => {
                self.runtime.interrupt_handle().interrupt();
                TickOutcome::Processed
            }
            RuntimeMessage::Shutdown => {
                self.shutdown();
                TickOutcome::Shutdown
            }
        }
    }

    fn run_command(&mut self, command: RuntimeCommand) {
        self.counters.running_command.store(true, Ordering::Relaxed);
        self.runtime.interrupt_handle().reset();
        match command {
            RuntimeCommand::CheckFile { path, reply, .. } => {
                // Compile-only, no event loop driving needed.
                send_check_reply(reply, self.runtime.check_file(path), &self.counters);
            }
            RuntimeCommand::RunFile { path, reply, .. } => {
                let result = self.runtime.run_file(path);
                let result = self.drive_event_loop_to_idle(result);
                let attempt = self.runtime.finish_jit_debug_attempt(result);
                send_run_reply(reply, attempt, &self.counters);
            }
            RuntimeCommand::RunScript {
                source,
                specifier,
                reply,
                ..
            } => {
                let result = self.runtime.run_script(source, &specifier);
                let result = self.drive_event_loop_to_idle(result);
                let attempt = self.runtime.finish_jit_debug_attempt(result);
                send_run_reply(reply, attempt, &self.counters);
            }
            RuntimeCommand::RunModule { path, reply, .. } => {
                let result = self.runtime.run_module(path);
                let result = self.drive_event_loop_to_idle(result);
                let attempt = self.runtime.finish_jit_debug_attempt(result);
                send_run_reply(reply, attempt, &self.counters);
            }
            RuntimeCommand::Eval { source, reply, .. } => {
                let result = self.runtime.eval(source);
                let result = self.drive_event_loop_to_idle(result);
                let attempt = self.runtime.finish_jit_debug_attempt(result);
                send_run_reply(reply, attempt, &self.counters);
            }
        }
        self.counters
            .running_command
            .store(false, Ordering::Relaxed);
        self.runtime.interrupt_handle().reset();
    }

    /// Drive the inbox until pending Ref'd timers / host ops drop
    /// to zero. Mirrors the Node / Deno run-loop semantics: a
    /// command's reply is held until the event loop is idle so
    /// `await otter.run_script(\"setTimeout(...)\")` observes the
    /// timer callback before resolving.
    ///
    /// On script error, the loop is short-circuited; pending
    /// timers are not run because the reply already carries the
    /// failure. Cancellation of leftover timers happens
    /// out-of-band when [`crate::Runtime`] drops.
    fn drive_event_loop_to_idle<T>(
        &mut self,
        initial: Result<T, OtterError>,
    ) -> Result<T, OtterError> {
        // Clippy `question_mark` suggests `as_ref()?` but the
        // function returns `Result<T, OtterError>` while `as_ref`
        // gives `Result<&T, &OtterError>`; rewriting would force
        // an extra clone path that does not pay back.
        #[allow(clippy::question_mark)]
        if initial.is_err() {
            return initial;
        }
        loop {
            let pending_ref_timers = self.counters.pending_ref_timers.load(Ordering::Relaxed);
            let pending_ref_host_ops = self.counters.pending_ref_host_ops.load(Ordering::Relaxed);
            if pending_ref_timers == 0 && pending_ref_host_ops == 0 {
                return initial;
            }
            // Block on the next inbox item. A later public command is deferred
            // until this command's Ref'd work finishes: recursively running it
            // would interleave isolate state and diagnostics batches.
            let msg = match self.rx.recv() {
                Ok(msg) => msg,
                Err(_) => return initial,
            };
            let msg = match msg {
                RuntimeMessage::Command(command) => {
                    self.deferred_commands.push_back(command);
                    continue;
                }
                other => other,
            };
            if matches!(self.process_message(msg), TickOutcome::Shutdown) {
                return initial;
            }
        }
    }

    fn record_microtask_snapshot(&self) {
        let stats = self.runtime.microtask_stats();
        self.counters
            .pending_microtasks
            .store(stats.pending, Ordering::Relaxed);
        self.counters
            .microtask_generation
            .store(stats.generation, Ordering::Relaxed);
    }
}

impl RuntimeCommand {
    fn id(&self) -> CommandId {
        match self {
            RuntimeCommand::CheckFile { id, .. }
            | RuntimeCommand::RunFile { id, .. }
            | RuntimeCommand::RunScript { id, .. }
            | RuntimeCommand::RunModule { id, .. }
            | RuntimeCommand::Eval { id, .. } => *id,
        }
    }
}

fn send_run_reply(reply: RunReply, result: ExecutionAttempt, counters: &RuntimeCounters) {
    if reply.send(result).is_err() {
        counters.cancelled_waiters.fetch_add(1, Ordering::Relaxed);
    }
}

fn send_check_reply(reply: CheckReply, result: Result<(), OtterError>, counters: &RuntimeCounters) {
    if reply.send(result).is_err() {
        counters.cancelled_waiters.fetch_add(1, Ordering::Relaxed);
    }
}

fn increment_liveness(
    liveness: RuntimeLiveness,
    ref_counter: &AtomicUsize,
    unref_counter: &AtomicUsize,
) {
    match liveness {
        RuntimeLiveness::Ref => {
            ref_counter.fetch_add(1, Ordering::Relaxed);
        }
        RuntimeLiveness::Unref => {
            unref_counter.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn decrement_liveness(
    liveness: RuntimeLiveness,
    ref_counter: &AtomicUsize,
    unref_counter: &AtomicUsize,
) {
    let counter = match liveness {
        RuntimeLiveness::Ref => ref_counter,
        RuntimeLiveness::Unref => unref_counter,
    };
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_sub(1));
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;

    #[test]
    fn dropping_last_handle_during_referenced_work_does_not_deadlock() {
        let handle = RuntimeHandle::spawn(RuntimeConfig::default()).expect("runtime handle");
        let _timer = handle.schedule_timer(TimerRequest {
            delay: Duration::from_secs(60),
            repeat: None,
        });
        let (reply, reply_rx) = oneshot::channel();
        let id = handle.next_command_id();
        handle
            .submit(RuntimeCommand::RunScript {
                id,
                source: SourceInput::from_javascript("1;"),
                specifier: "<shutdown-with-ref-work>".to_string(),
                reply,
            })
            .expect("submit command");

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !handle.activity_stats().running_command {
            assert!(
                std::time::Instant::now() < deadline,
                "command never entered the isolate"
            );
            std::thread::yield_now();
        }
        drop(reply_rx);

        let (dropped_tx, dropped_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(handle);
            let _ = dropped_tx.send(());
        });
        dropped_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("last handle drop must join the isolate without deadlocking");
    }
}
