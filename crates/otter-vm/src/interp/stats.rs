//! Work-budget bookkeeping and JIT runtime counters.
//!
//! # Contents
//! Work-slice begin/finish/checkpoint, bytecode/native/construct
//! call tallies, JIT stub and fast-path hit counters, microtask drain
//! stats, and property-IC capacity management.
//!
//! # Invariants
//! - Every enforcing checkpoint either continues, rejects, or rotates exactly
//!   one isolate-owned work slice.
//! - A cooperative yield never exposes a JavaScript completion and never lets
//!   a later macrotask overtake the current turn.
#![allow(unused_imports)]
use crate::work_budget::WorkBudgetCheckpoint;
use crate::*;

impl Interpreter {
    /// Return the current per-slice work budget policy.
    #[must_use]
    pub fn work_budget(&self) -> WorkBudget {
        self.work_budget
    }

    /// Set the per-slice work budget policy.
    ///
    /// Observe records crossings, Reject returns [`VmError::BudgetExceeded`],
    /// and Yield rotates the owning isolate's slice before resuming the same
    /// ECMAScript turn.
    pub fn set_work_budget(&mut self, budget: WorkBudget) {
        self.work_budget = budget;
    }

    /// Return aggregate work-budget/resource counters.
    #[must_use]
    pub fn work_budget_stats(&self) -> WorkBudgetStats {
        self.work_budget_stats
    }

    /// Return the shareable cell this isolate publishes its budget counters
    /// to.
    #[must_use]
    pub fn work_budget_telemetry(&self) -> WorkBudgetTelemetry {
        self.work_budget_telemetry.clone()
    }

    /// Publish this isolate's budget counters into `telemetry` from now on.
    ///
    /// The runtime installs one cell per isolate before the first turn, so a
    /// worker never publishes into its parent's counters.
    pub fn set_work_budget_telemetry(&mut self, telemetry: WorkBudgetTelemetry) {
        self.work_budget_telemetry = telemetry;
        self.publish_work_budget_telemetry();
    }

    pub(crate) fn publish_work_budget_telemetry(&self) {
        self.work_budget_telemetry.publish(self.work_budget_stats);
    }

    /// Reset aggregate work-budget/resource counters.
    pub fn reset_work_budget_stats(&mut self) {
        self.work_budget_stats = WorkBudgetStats::default();
        self.work_budget_depth = 0;
        self.work_budget_slice_started_at = None;
        self.work_budget_heap_start = None;
        self.publish_work_budget_telemetry();
    }

    pub(crate) fn begin_work_budget_turn(&mut self) {
        if self.work_budget_depth == 0 {
            self.work_budget_stats.begin_turn();
            self.work_budget_slice_started_at = Some(std::time::Instant::now());
            let heap = RuntimeHeapSnapshot::from_heap(&mut self.gc_heap);
            self.work_budget_heap_start = Some(heap);
        }
        self.work_budget_depth = self.work_budget_depth.saturating_add(1);
    }

    pub(crate) fn finish_work_budget_turn(&mut self) {
        self.work_budget_depth = self.work_budget_depth.saturating_sub(1);
        if self.work_budget_depth == 0
            && let Some(started_at) = self.work_budget_slice_started_at.take()
        {
            if let Some(start_heap) = self.work_budget_heap_start.take() {
                let end_heap = RuntimeHeapSnapshot::from_heap(&mut self.gc_heap);
                self.work_budget_stats
                    .record_turn_heap_delta(start_heap, end_heap);
            }
            self.work_budget_stats
                .finish_turn(started_at.elapsed(), self.work_budget);
            self.publish_work_budget_telemetry();
        }
    }

    fn poll_work_budget_checkpoint(&mut self) -> Result<WorkBudgetCheckpoint, VmError> {
        if !self.work_budget.enforces_on_exceedance() {
            return Ok(WorkBudgetCheckpoint::Continue);
        }
        let Some(started_at) = self.work_budget_slice_started_at else {
            return Ok(WorkBudgetCheckpoint::Continue);
        };
        if self.work_budget.needs_heap_checkpoint()
            && let Some(start_heap) = self.work_budget_heap_start
        {
            let end_heap = RuntimeHeapSnapshot::from_heap(&mut self.gc_heap);
            self.work_budget_stats
                .observe_current_turn_heap_delta(start_heap, end_heap);
        }
        let elapsed_nanos = u64::try_from(started_at.elapsed().as_nanos()).unwrap_or(u64::MAX);
        if work_budget::budget_exceeded(
            self.work_budget_stats.current_turn_work_units(),
            self.work_budget_stats.current_turn_allocated_bytes,
            self.work_budget_stats.current_turn_host_ops,
            elapsed_nanos,
            self.work_budget_stats.current_external_bytes,
            self.work_budget,
        ) {
            // Outstanding external memory is not reset by rotating a CPU work
            // slice, so yielding on that limit would livelock forever.
            if self.work_budget.yields_on_exceedance()
                && !work_budget::external_budget_exceeded(
                    self.work_budget_stats.current_external_bytes,
                    self.work_budget,
                )
            {
                return Ok(WorkBudgetCheckpoint::Yield);
            }
            self.work_budget_stats.record_budget_rejection();
            self.publish_work_budget_telemetry();
            return Err(self.err_budget(("work budget exceeded".to_string()).into()));
        }
        Ok(WorkBudgetCheckpoint::Continue)
    }

    fn rotate_work_budget_slice(&mut self) {
        let Some(started_at) = self.work_budget_slice_started_at.take() else {
            return;
        };
        if let Some(start_heap) = self.work_budget_heap_start.take() {
            let end_heap = RuntimeHeapSnapshot::from_heap(&mut self.gc_heap);
            self.work_budget_stats
                .record_turn_heap_delta(start_heap, end_heap);
        }
        self.work_budget_stats.record_forced_yield();
        self.work_budget_stats
            .finish_turn(started_at.elapsed(), self.work_budget);
        self.publish_work_budget_telemetry();

        // The isolate retains ownership of the current ECMAScript turn. This
        // gives peer OS threads a scheduling point without letting a later
        // macrotask on this isolate overtake its microtask checkpoint.
        std::thread::yield_now();

        self.work_budget_stats.begin_turn();
        self.work_budget_slice_started_at = Some(std::time::Instant::now());
        self.work_budget_heap_start = Some(RuntimeHeapSnapshot::from_heap(&mut self.gc_heap));
    }

    pub(crate) fn enforce_work_budget_checkpoint(&mut self) -> Result<(), VmError> {
        if self.poll_work_budget_checkpoint()? == WorkBudgetCheckpoint::Yield {
            self.rotate_work_budget_slice();
        }
        Ok(())
    }

    pub(crate) fn record_runtime_bytecode_call(&mut self) {
        self.record_runtime_bytecode_calls(1);
    }

    /// Reconcile a cold batch of bytecode call entries without one VM
    /// transition per generated call.
    pub(crate) fn record_runtime_bytecode_calls(&mut self, calls: u64) {
        self.work_budget_stats.record_bytecode_calls(calls);
    }

    pub(crate) fn record_runtime_native_call(&mut self) -> Result<(), VmError> {
        self.work_budget_stats.record_native_call();
        self.enforce_work_budget_checkpoint()
    }

    /// Charge one completed RegExp engine attempt to the current root turn.
    /// The matcher itself is synchronous, so this checkpoint runs immediately
    /// after it returns and before any result becomes JavaScript-visible.
    pub(crate) fn charge_regex_backtrack_steps(&mut self, steps: u64) -> Result<(), VmError> {
        self.work_budget_stats.record_regex_backtrack_steps(steps);
        self.enforce_work_budget_checkpoint()
    }

    pub(crate) fn record_runtime_construct_call(&mut self) -> Result<(), VmError> {
        self.work_budget_stats.record_construct_call();
        self.enforce_work_budget_checkpoint()
    }

    pub(crate) fn record_runtime_host_op_enqueued(&mut self) {
        self.work_budget_stats.record_host_op_enqueued();
    }

    /// Poll interrupts and the work budget from compiled loop backedges.
    ///
    /// Baseline code reaches this through a leaf VM-native runtime stub. The
    /// interpreter charges every opcode; compiled code has no per-op VM tick, so
    /// it charges a bounded backedge batch and then reuses the same budget
    /// checkpoint. This keeps timeout/budget semantics independent of whether a
    /// hot loop has OSR'd into native code.
    pub(crate) fn jit_backedge_poll(
        &mut self,
        context: &crate::execution_context::ExecutionContext,
    ) -> Result<WorkBudgetCheckpoint, VmError> {
        self.record_jit_runtime_stub_class(native_abi::STUB_JIT_BACKEDGE_POLL.class);
        // A compiled loop is the one place a long-running program may spend
        // all its time without an interpreter entry; promote the callee its
        // generated calls reported hot here.
        self.promote_hot_generated_callee(context);
        // The interrupt flag is polled inline at every back-edge, so reaching
        // this re-entry with the flag set means a cancellation is pending.
        if self.interrupt.is_set() {
            return Err(VmError::Interrupted);
        }
        // Compiled code decremented the fuel counter inline for each back-edge
        // since the last checkpoint and re-entered when it hit zero. Account for
        // that whole batch of work in one step and re-arm the counter, then
        // run the (possibly early-returning) budget checkpoint.
        self.work_budget_stats
            .record_work(Self::JIT_BACKEDGE_POLL_BATCH);
        self.jit_backedge_fuel = Self::JIT_BACKEDGE_POLL_BATCH;
        let checkpoint = self.poll_work_budget_checkpoint()?;
        if checkpoint == WorkBudgetCheckpoint::Yield {
            self.rotate_work_budget_slice();
        }
        Ok(checkpoint)
    }

    /// Address of the inline back-edge fuel counter, handed to compiled code so
    /// it can decrement the countdown without a VM re-entry.
    pub fn jit_backedge_fuel_ptr(&mut self) -> *mut u64 {
        &mut self.jit_backedge_fuel
    }

    /// Address of the cooperative interrupt flag's backing byte, polled inline at
    /// each back-edge.
    #[must_use]
    pub fn jit_interrupt_flag_ptr(&self) -> *const u8 {
        self.interrupt.as_ptr()
    }

    pub(crate) fn record_jit_runtime_property_stub(&mut self) {
        self.record_jit_runtime_stub_class(native_abi::RuntimeStubClass::Reentrant);
        self.jit_runtime_stats.runtime_property_stubs = self
            .jit_runtime_stats
            .runtime_property_stubs
            .saturating_add(1);
    }

    pub(crate) fn record_jit_runtime_stub_class(&mut self, class: native_abi::RuntimeStubClass) {
        self.work_budget_stats
            .record_work(u64::from(class.work_units()));
        self.jit_runtime_stats.runtime_stub_transitions = self
            .jit_runtime_stats
            .runtime_stub_transitions
            .saturating_add(1);
        match class {
            native_abi::RuntimeStubClass::LeafNoAlloc => {
                self.jit_runtime_stats.leaf_stub_transitions = self
                    .jit_runtime_stats
                    .leaf_stub_transitions
                    .saturating_add(1);
            }
            native_abi::RuntimeStubClass::Alloc => {
                self.jit_runtime_stats.alloc_stub_transitions = self
                    .jit_runtime_stats
                    .alloc_stub_transitions
                    .saturating_add(1);
            }
            native_abi::RuntimeStubClass::Reentrant => {
                self.jit_runtime_stats.reentrant_stub_transitions = self
                    .jit_runtime_stats
                    .reentrant_stub_transitions
                    .saturating_add(1);
            }
        }
    }

    pub(crate) fn record_jit_alloc_value_stub_status(
        &mut self,
        status: native_abi::NativeResultStatus,
    ) {
        match status {
            native_abi::NativeResultStatus::Success => {
                self.jit_runtime_stats.alloc_value_stub_ok =
                    self.jit_runtime_stats.alloc_value_stub_ok.saturating_add(1);
            }
            native_abi::NativeResultStatus::SideExit => {
                self.jit_runtime_stats.alloc_value_stub_miss = self
                    .jit_runtime_stats
                    .alloc_value_stub_miss
                    .saturating_add(1);
            }
            native_abi::NativeResultStatus::OutOfMemory => {
                self.jit_runtime_stats.alloc_value_stub_out_of_memory = self
                    .jit_runtime_stats
                    .alloc_value_stub_out_of_memory
                    .saturating_add(1);
            }
            native_abi::NativeResultStatus::Throw
            | native_abi::NativeResultStatus::Continue
            | native_abi::NativeResultStatus::Yield
            | native_abi::NativeResultStatus::Fatal => {
                self.jit_runtime_stats.alloc_value_stub_other = self
                    .jit_runtime_stats
                    .alloc_value_stub_other
                    .saturating_add(1);
            }
        }
    }

    pub(crate) fn record_runtime_microtask_drain_started(&mut self) {
        self.work_budget_stats.record_microtask_drain_started();
    }

    pub(crate) fn record_runtime_microtask_executed(&mut self) {
        self.work_budget_stats.record_microtask_executed();
    }
}
