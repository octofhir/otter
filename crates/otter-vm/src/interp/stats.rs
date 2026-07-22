//! Runtime-budget bookkeeping and JIT runtime counters.
//!
//! # Contents
//! Runtime budget turn begin/finish/checkpoint, bytecode/native/construct
//! call tallies, JIT stub and fast-path hit counters, microtask drain
//! stats, and property-IC capacity management.
#![allow(unused_imports)]
use crate::*;

impl Interpreter {
    /// Return the current observational runtime budget policy.
    #[must_use]
    pub fn runtime_budget(&self) -> RuntimeBudget {
        self.runtime_budget
    }

    /// Set the observational runtime budget policy.
    ///
    /// The current VM records exceedance observations but does not preempt,
    /// yield, or reject when limits are crossed.
    pub fn set_runtime_budget(&mut self, budget: RuntimeBudget) {
        self.runtime_budget = budget;
    }

    /// Return aggregate runtime budget/resource counters.
    #[must_use]
    pub fn runtime_budget_stats(&self) -> RuntimeBudgetStats {
        self.runtime_budget_stats
    }

    /// Reset aggregate runtime budget/resource counters.
    pub fn reset_runtime_budget_stats(&mut self) {
        self.runtime_budget_stats = RuntimeBudgetStats::default();
        self.runtime_budget_depth = 0;
        self.runtime_budget_turn_started_at = None;
        self.runtime_budget_heap_start = None;
    }

    pub(crate) fn begin_runtime_budget_turn(&mut self) {
        if self.runtime_budget_depth == 0 {
            self.runtime_budget_stats.begin_turn();
            self.runtime_budget_turn_started_at = Some(std::time::Instant::now());
            let heap = RuntimeHeapSnapshot::from_heap(&mut self.gc_heap);
            self.runtime_budget_heap_start = Some(heap);
        }
        self.runtime_budget_depth = self.runtime_budget_depth.saturating_add(1);
    }

    pub(crate) fn finish_runtime_budget_turn(&mut self) {
        self.runtime_budget_depth = self.runtime_budget_depth.saturating_sub(1);
        if self.runtime_budget_depth == 0
            && let Some(started_at) = self.runtime_budget_turn_started_at.take()
        {
            if let Some(start_heap) = self.runtime_budget_heap_start.take() {
                let end_heap = RuntimeHeapSnapshot::from_heap(&mut self.gc_heap);
                self.runtime_budget_stats
                    .record_turn_heap_delta(start_heap, end_heap);
            }
            self.runtime_budget_stats
                .finish_turn(started_at.elapsed(), self.runtime_budget);
        }
    }

    pub(crate) fn enforce_runtime_budget_checkpoint(&mut self) -> Result<(), VmError> {
        if !self.runtime_budget.rejects_on_exceedance() {
            return Ok(());
        }
        let Some(started_at) = self.runtime_budget_turn_started_at else {
            return Ok(());
        };
        if self.runtime_budget.has_heap_checkpoint_limits()
            && let Some(start_heap) = self.runtime_budget_heap_start
        {
            let end_heap = RuntimeHeapSnapshot::from_heap(&mut self.gc_heap);
            self.runtime_budget_stats
                .observe_current_turn_heap_delta(start_heap, end_heap);
        }
        let elapsed_nanos = u64::try_from(started_at.elapsed().as_nanos()).unwrap_or(u64::MAX);
        if runtime_budget::budget_exceeded(
            self.runtime_budget_stats.current_turn_reductions(),
            self.runtime_budget_stats.current_turn_allocated_bytes,
            self.runtime_budget_stats.current_turn_host_ops,
            elapsed_nanos,
            self.runtime_budget_stats.current_external_bytes,
            self.runtime_budget,
        ) {
            self.runtime_budget_stats.record_budget_rejection();
            return Err(self.err_budget(("runtime budget exceeded".to_string()).into()));
        }
        Ok(())
    }

    pub(crate) fn record_runtime_bytecode_call(&mut self) {
        self.record_runtime_bytecode_calls(1);
    }

    /// Reconcile a cold batch of bytecode call entries without one VM
    /// transition per generated call.
    pub(crate) fn record_runtime_bytecode_calls(&mut self, calls: u64) {
        self.runtime_budget_stats.record_bytecode_calls(calls);
    }

    pub(crate) fn record_runtime_native_call(&mut self) {
        self.runtime_budget_stats.record_native_call();
    }

    pub(crate) fn record_runtime_construct_call(&mut self) {
        self.runtime_budget_stats.record_construct_call();
    }

    pub(crate) fn record_runtime_host_op_enqueued(&mut self) {
        self.runtime_budget_stats.record_host_op_enqueued();
    }

    /// Poll interrupts and runtime budget from compiled loop backedges.
    ///
    /// Baseline code reaches this through a leaf VM-native runtime stub. The
    /// interpreter charges every opcode; compiled code has no per-op VM tick, so
    /// it charges one reduction per backedge and then reuses the same budget
    /// checkpoint. This keeps timeout/budget semantics independent of whether a
    /// hot loop has OSR'd into native code.
    pub fn jit_backedge_poll(&mut self) -> Result<(), VmError> {
        self.record_jit_runtime_stub_class(native_abi::STUB_JIT_BACKEDGE_POLL.class);
        // The interrupt flag is polled inline at every back-edge, so reaching
        // this re-entry with the flag set means a cancellation is pending.
        if self.interrupt.is_set() {
            return Err(VmError::Interrupted);
        }
        // Compiled code decremented the fuel counter inline for each back-edge
        // since the last checkpoint and re-entered when it hit zero. Account for
        // that whole batch of reductions in one step and re-arm the counter, then
        // run the (possibly early-returning) budget checkpoint.
        self.runtime_budget_stats
            .record_reductions(Self::JIT_BACKEDGE_POLL_BATCH);
        self.jit_backedge_fuel = Self::JIT_BACKEDGE_POLL_BATCH;
        self.enforce_runtime_budget_checkpoint()
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
        status: native_abi::RuntimeStubStatus,
    ) {
        match status {
            native_abi::RuntimeStubStatus::Ok => {
                self.jit_runtime_stats.alloc_value_stub_ok =
                    self.jit_runtime_stats.alloc_value_stub_ok.saturating_add(1);
            }
            native_abi::RuntimeStubStatus::Miss => {
                self.jit_runtime_stats.alloc_value_stub_miss = self
                    .jit_runtime_stats
                    .alloc_value_stub_miss
                    .saturating_add(1);
            }
            native_abi::RuntimeStubStatus::OutOfMemory => {
                self.jit_runtime_stats.alloc_value_stub_out_of_memory = self
                    .jit_runtime_stats
                    .alloc_value_stub_out_of_memory
                    .saturating_add(1);
            }
            native_abi::RuntimeStubStatus::Throw
            | native_abi::RuntimeStubStatus::Deopt
            | native_abi::RuntimeStubStatus::Interrupt => {
                self.jit_runtime_stats.alloc_value_stub_other = self
                    .jit_runtime_stats
                    .alloc_value_stub_other
                    .saturating_add(1);
            }
        }
    }

    pub(crate) fn record_runtime_microtask_drain_started(&mut self) {
        self.runtime_budget_stats.record_microtask_drain_started();
    }

    pub(crate) fn record_runtime_microtask_executed(&mut self) {
        self.runtime_budget_stats.record_microtask_executed();
    }

    pub(crate) fn observe_runtime_microtask_budget(&mut self, microtasks_this_drain: u64) -> bool {
        if self
            .runtime_budget
            .max_microtasks_per_drain
            .is_some_and(|limit| microtasks_this_drain > limit)
        {
            self.runtime_budget_stats.record_budget_limit_observation();
            true
        } else {
            false
        }
    }
}
