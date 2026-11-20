//! Direct isolate control and completion delivery.
//!
//! # Contents
//! Resource accounts, cooperative interrupts, timeouts and host completion delivery.
//!
//! # Invariants
//! Execution remains pinned to the owning runtime.
//!
//! # See also
//! - [`crate::Runtime`] owns the isolate session.

use crate::*;

impl Runtime {
    /// Start configuring a new runtime.
    #[must_use]
    pub fn builder() -> RuntimeBuilder {
        RuntimeBuilder::default()
    }

    /// Clone the shared resource account used by this isolate and its children.
    #[must_use]
    pub fn resource_account(&self) -> ResourceAccount {
        self.config.resource_account.clone()
    }

    /// Capture deterministic current, peak, rejection, and limit counters.
    #[must_use]
    pub fn resource_snapshot(&self) -> ResourceSnapshot {
        self.config.resource_account.snapshot()
    }

    /// How many functions this runtime compiled on their first call.
    ///
    /// Scripts and modules compile only the code that runs as soon as it
    /// is created; every other function body compiles when it is first
    /// called, once.
    #[must_use]
    pub fn lazy_function_bodies_compiled(&self) -> u64 {
        self.interp.lazy_bodies_compiled()
    }

    /// Cooperative cancellation handle.
    #[must_use]
    pub fn interrupt_handle(&self) -> InterruptHandle {
        InterruptHandle(self.interp.interrupt_handle())
    }

    /// Clone a lifecycle handle that cancels only this isolate's Atomics waits.
    #[must_use]
    pub(crate) fn atomics_wait_agent_handle(&self) -> otter_vm::atomics_wait::WaitAgentHandle {
        self.interp.atomics_wait_agent_handle()
    }

    pub(crate) fn with_direct_timeout<T>(
        &mut self,
        run: impl FnOnce(&mut Self) -> Result<T, OtterError>,
    ) -> Result<T, OtterError> {
        if !self.enforce_direct_timeout || self.config.timeout == Duration::ZERO {
            return run(self);
        }
        let guard = DirectTimeoutGuard::start(self.config.timeout, self.interp.interrupt_handle())?;
        let result = run(self);
        guard.finish(result)
    }

    /// Clone the runtime event-loop task spawner, if this runtime has one.
    #[must_use]
    pub fn runtime_task_spawner(&self) -> Option<RuntimeTaskSpawner> {
        self.runtime_task_spawner.clone()
    }

    /// Configured per-`run_*` timeout. Direct runtimes interrupt executing
    /// JavaScript at VM safepoints; `Duration::ZERO` disables the watchdog.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.config.timeout
    }

    pub(crate) fn microtask_stats(&self) -> MicrotaskStats {
        let queue = self.interp.microtasks();
        MicrotaskStats {
            pending: queue.has_any_pending(),
            generation: queue.generation(),
        }
    }

    /// Install the host-side timer scheduler. The runtime calls
    /// this once during isolate-runner construction. Direct-mode
    /// embedders (Layer A `RuntimeBuilder::build`) never call this;
    /// scripts then receive a TypeError on `setTimeout` /
    /// `setInterval` instead of silently dropping the callback.
    pub fn install_timer_scheduler(&mut self, scheduler: otter_vm::TimerSchedulerHandle) {
        self.interp.set_timer_scheduler(scheduler);
    }

    /// Install the host completion sink backing async native methods.
    /// Wired by the isolate runner alongside the timer scheduler;
    /// direct-mode embedders never call this, and their async native
    /// methods reject with a TypeError unless the future is
    /// immediately ready.
    pub fn install_host_completion_sink(
        &mut self,
        sink: std::sync::Arc<dyn otter_vm::host_completion::HostCompletionSink>,
    ) {
        self.interp.set_host_completion_sink(sink);
    }

    /// Install or replace the Rust-side Promise rejection observer.
    pub fn install_promise_rejection_hook(&mut self, hook: impl PromiseRejectionHook) {
        self.interp
            .set_promise_rejection_hook(PromiseRejectionHookHandle::new(hook));
    }

    /// Run a host completion job posted by an async native future.
    ///
    /// Layer B calls this from its isolate inbox. A Layer A embedder can
    /// install a custom [`HostCompletionSink`] whose `complete` method posts
    /// the owned job into the embedder's event queue, then call this method on
    /// the runtime's owning thread when that event arrives. The job must never
    /// run on an executor worker: it may allocate, resolve persistent roots,
    /// and settle JavaScript promises in this isolate.
    pub fn run_host_completion(
        &mut self,
        job: otter_vm::host_completion::HostCompletionJob,
    ) -> Result<(), OtterError> {
        match job.run(&mut self.interp) {
            Ok(()) => Ok(()),
            Err(error) if self.absorb_termination(&error) => Ok(()),
            Err(error) => Err(map_vm_error(error)),
        }
    }

    /// Suppress a host completion after process exit while releasing any
    /// persistent roots owned by its job.
    pub(crate) fn cancel_host_completion(
        &mut self,
        job: otter_vm::host_completion::HostCompletionJob,
    ) {
        job.cancel(&mut self.interp);
    }
}
