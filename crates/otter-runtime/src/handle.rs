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
//!
//! # See also
//!
//! - [Event loop](../../../docs/book/src/engine/event-loop.md)
//! - [Runtime architecture](../../../docs/book/src/engine/architecture.md)

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::time::Duration;

use tokio::sync::oneshot;

use crate::event_loop::{
    EventLoop, HostFuture, HostJoinHandle, HostOpCompletion, RuntimeLiveness, RuntimeWake,
    TimerRequest, TimerToken, TokioEventLoop,
};
use crate::{ExecutionResult, OtterError, Runtime, RuntimeConfig, SourceInput};

const DEFAULT_COMMAND_CAPACITY: usize = 64;

type RunReply = oneshot::Sender<Result<ExecutionResult, OtterError>>;
type CheckReply = oneshot::Sender<Result<(), OtterError>>;

type CommandId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct HostOpId(u64);

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
    next_host_op_id: AtomicU64,
    next_module_job_id: AtomicU64,
    shutdown: AtomicBool,
}

enum RuntimeMessage {
    Command(RuntimeCommand),
    HostOpCompleted {
        completion: HostOpCompletion,
        liveness: RuntimeLiveness,
    },
    TimerFired {
        token: TimerToken,
        liveness: RuntimeLiveness,
    },
    DynamicModuleReady(ModuleJobId),
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
            code: "TOKIO_RUNTIME_CREATE".to_string(),
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
            next_host_op_id: AtomicU64::new(1),
            next_module_job_id: AtomicU64::new(1),
            shutdown: AtomicBool::new(false),
        });
        let runner_counters = counters.clone();
        let runner = std::thread::Builder::new()
            .name("otter-isolate".to_string())
            .spawn(move || run_isolate(config, rx, runner_counters, interrupt_tx))
            .map_err(|e| OtterError::Internal {
                code: "ISOLATE_SPAWN".to_string(),
                message: e.to_string(),
            })?;
        let interrupt = interrupt_rx.recv().map_err(|_| OtterError::Internal {
            code: "ISOLATE_START".to_string(),
            message: "runtime isolate stopped before exposing its interrupt handle".to_string(),
        })?;
        let inner = Arc::new(RuntimeHandleInner {
            tx,
            runner: Mutex::new(Some(runner)),
            event_loop,
            interrupt,
            command_timeout,
            counters,
        });
        Ok(Self { inner })
    }

    /// Run a file through the isolate runner.
    ///
    /// # Errors
    /// See [`OtterError`].
    pub async fn run_file(&self, path: impl Into<PathBuf>) -> Result<ExecutionResult, OtterError> {
        let (reply, rx) = oneshot::channel();
        let id = self.next_command_id();
        self.submit(RuntimeMessage::Command(RuntimeCommand::RunFile {
            id,
            path: path.into(),
            reply,
        }))?;
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
        self.submit(RuntimeMessage::Command(RuntimeCommand::CheckFile {
            id,
            path: path.into(),
            reply,
        }))?;
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
        let (reply, rx) = oneshot::channel();
        let id = self.next_command_id();
        self.submit(RuntimeMessage::Command(RuntimeCommand::RunScript {
            id,
            source,
            specifier: specifier.into(),
            reply,
        }))?;
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
        let (reply, rx) = oneshot::channel();
        let id = self.next_command_id();
        self.submit(RuntimeMessage::Command(RuntimeCommand::RunModule {
            id,
            path: path.into(),
            reply,
        }))?;
        self.await_run_reply(rx).await
    }

    /// Evaluate a source bundle through the isolate runner.
    ///
    /// # Errors
    /// See [`OtterError`].
    pub async fn eval(&self, source: SourceInput) -> Result<ExecutionResult, OtterError> {
        let (reply, rx) = oneshot::channel();
        let id = self.next_command_id();
        self.submit(RuntimeMessage::Command(RuntimeCommand::Eval {
            id,
            source,
            reply,
        }))?;
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

    /// Spawn an owned host operation and post its completion back to
    /// the isolate inbox.
    pub fn spawn_host_op(&self, liveness: RuntimeLiveness, op: HostFuture) -> HostJoinHandle {
        let op_id = HostOpId(
            self.inner
                .counters
                .next_host_op_id
                .fetch_add(1, Ordering::Relaxed),
        );
        increment_liveness(
            liveness,
            &self.inner.counters.pending_ref_host_ops,
            &self.inner.counters.pending_unref_host_ops,
        );
        let tx = self.inner.tx.clone();
        self.inner.event_loop.spawn_host_op(Box::pin(async move {
            let mut completion = op.await;
            completion.id = op_id.0;
            let _ = tx.try_send(RuntimeMessage::HostOpCompleted {
                completion: completion.clone(),
                liveness,
            });
            completion
        }))
    }

    /// Schedule a timer wake through the runtime inbox.
    #[must_use]
    pub fn schedule_timer(&self, request: TimerRequest) -> TimerToken {
        increment_liveness(
            request.liveness,
            &self.inner.counters.pending_ref_timers,
            &self.inner.counters.pending_unref_timers,
        );
        let tx = self.inner.tx.clone();
        let liveness = request.liveness;
        self.inner
            .event_loop
            .schedule_timer_callback(request, move |token| {
                let _ = tx.try_send(RuntimeMessage::TimerFired { token, liveness });
            })
    }

    /// Cancel a pending timer.
    pub fn cancel_timer(&self, token: TimerToken) -> bool {
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
    pub fn complete_dynamic_module_job_for_tests(&self) {
        self.inner
            .counters
            .pending_dynamic_module_jobs
            .fetch_add(1, Ordering::Relaxed);
        let id = ModuleJobId(
            self.inner
                .counters
                .next_module_job_id
                .fetch_add(1, Ordering::Relaxed),
        );
        let _ = self
            .inner
            .tx
            .try_send(RuntimeMessage::DynamicModuleReady(id));
    }

    /// Emit a diagnostic wake through the event-loop abstraction.
    /// Wake the runtime and emit a diagnostic inbox item.
    pub fn wake_runtime(&self, origin: impl Into<String>) {
        let origin = origin.into();
        self.inner.event_loop.wake_runtime(RuntimeWake {
            origin: origin.clone(),
        });
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

    fn submit(&self, msg: RuntimeMessage) -> Result<(), OtterError> {
        if self.inner.counters.shutdown.load(Ordering::Relaxed) {
            return Err(OtterError::Internal {
                code: "RUNTIME_SHUTDOWN".to_string(),
                message: "runtime handle is shut down".to_string(),
            });
        }
        match self.inner.tx.try_send(msg) {
            Ok(()) => {
                self.inner
                    .counters
                    .queued_commands
                    .fetch_add(1, Ordering::Relaxed);
                self.inner
                    .counters
                    .submitted_commands
                    .fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(TrySendError::Full(_)) => {
                self.inner
                    .counters
                    .backpressure_rejections
                    .fetch_add(1, Ordering::Relaxed);
                Err(OtterError::Internal {
                    code: "RUNTIME_BACKPRESSURE".to_string(),
                    message: "runtime command queue is full".to_string(),
                })
            }
            Err(TrySendError::Disconnected(_)) => Err(OtterError::Internal {
                code: "RUNTIME_CLOSED".to_string(),
                message: "runtime isolate has stopped".to_string(),
            }),
        }
    }

    async fn await_run_reply(
        &self,
        rx: oneshot::Receiver<Result<ExecutionResult, OtterError>>,
    ) -> Result<ExecutionResult, OtterError> {
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
            Ok(Ok(result)) => {
                self.inner
                    .counters
                    .completed_commands
                    .fetch_add(1, Ordering::Relaxed);
                Ok(result)
            }
            Ok(Err(err)) => {
                self.inner
                    .counters
                    .failed_commands
                    .fetch_add(1, Ordering::Relaxed);
                Err(err)
            }
            Err(_) => Err(OtterError::Internal {
                code: "RUNTIME_REPLY_DROPPED".to_string(),
                message: "runtime isolate dropped command reply".to_string(),
            }),
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
                code: "RUNTIME_REPLY_DROPPED".to_string(),
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
) {
    let runtime = match Runtime::from_config(config) {
        Ok(runtime) => runtime,
        Err(_) => return,
    };
    let _ = interrupt_tx.send(runtime.interrupt_handle().raw_flag());
    let mut runner = IsolateRunner {
        runtime,
        rx,
        counters,
        shutdown: false,
    };
    runner.run_until_idle();
}

struct IsolateRunner {
    runtime: Runtime,
    rx: Receiver<RuntimeMessage>,
    counters: Arc<RuntimeCounters>,
    shutdown: bool,
}

enum TickOutcome {
    Processed,
    Idle,
    Shutdown,
}

impl IsolateRunner {
    fn poll_one_tick(&mut self) -> TickOutcome {
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
            if matches!(self.poll_one_tick(), TickOutcome::Idle) {
                match self.rx.recv() {
                    Ok(msg) => {
                        if matches!(self.process_message(msg), TickOutcome::Shutdown) {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
        }
    }

    #[allow(dead_code)]
    fn run_until_command(&mut self, id: CommandId) {
        while !self.shutdown {
            match self.rx.recv() {
                Ok(msg) => {
                    let command_done = matches!(
                        &msg,
                        RuntimeMessage::Command(command) if command.id() == id
                    );
                    if matches!(self.process_message(msg), TickOutcome::Shutdown) || command_done {
                        return;
                    }
                }
                Err(_) => return,
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
                if id == 0 {
                    return TickOutcome::Idle;
                }
                TickOutcome::Processed
            }
            RuntimeMessage::HostOpCompleted {
                completion,
                liveness,
            } => {
                decrement_liveness(
                    liveness,
                    &self.counters.pending_ref_host_ops,
                    &self.counters.pending_unref_host_ops,
                );
                match completion.result {
                    Ok(_) => {
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
                TickOutcome::Processed
            }
            RuntimeMessage::TimerFired {
                token: _token,
                liveness,
            } => {
                decrement_liveness(
                    liveness,
                    &self.counters.pending_ref_timers,
                    &self.counters.pending_unref_timers,
                );
                self.counters.fired_timers.fetch_add(1, Ordering::Relaxed);
                TickOutcome::Processed
            }
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
                send_check_reply(reply, self.runtime.check_file(path), &self.counters);
            }
            RuntimeCommand::RunFile { path, reply, .. } => {
                send_run_reply(reply, self.runtime.run_file(path), &self.counters);
            }
            RuntimeCommand::RunScript {
                source,
                specifier,
                reply,
                ..
            } => {
                send_run_reply(
                    reply,
                    self.runtime.run_script(source, &specifier),
                    &self.counters,
                );
            }
            RuntimeCommand::RunModule { path, reply, .. } => {
                send_run_reply(reply, self.runtime.run_module(path), &self.counters);
            }
            RuntimeCommand::Eval { source, reply, .. } => {
                send_run_reply(reply, self.runtime.eval(source), &self.counters);
            }
        }
        self.counters
            .running_command
            .store(false, Ordering::Relaxed);
        self.runtime.interrupt_handle().reset();
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

fn send_run_reply(
    reply: RunReply,
    result: Result<ExecutionResult, OtterError>,
    counters: &RuntimeCounters,
) {
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
