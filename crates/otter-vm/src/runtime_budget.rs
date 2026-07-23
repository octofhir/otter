//! Runtime budget policy and resource-accounting snapshots.
//!
//! This module owns the VM-side data contract for BEAM-style runtime
//! accounting. The current slice is observational: it records reductions,
//! turn latency, allocation pressure, host-op enqueue counts, and major
//! call-shape counters without preempting execution.
//!
//! # Contents
//! - [`RuntimeBudget`] — optional per-turn policy limits.
//! - [`RuntimeBudgetExceededAction`] — outcome policy when a limit is crossed.
//! - [`RuntimeBudgetStats`] — aggregate counters exposed for diagnostics.
//! - Reduction cost helpers for the interpreter dispatch loop.
//!
//! # Invariants
//! - Budget DTOs are owned, copyable data; no VM handles cross the boundary.
//! - Exceeding a configured budget is counted but not enforced in this slice.
//! - Reduction accounting is approximate and stable, not a wall-clock timer.
//!
//! # See also
//! - [`crate::Interpreter`]
//! - [`crate::VmError`]

use otter_bytecode::Op;
use otter_gc::GcHeap;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Optional runtime budget policy for one contiguous VM turn.
///
/// The initial implementation records observations only. A future scheduler
/// slice will use the same DTO to yield or reject when limits are exceeded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeBudget {
    /// Outcome policy when a configured limit is crossed.
    pub on_exceeded: RuntimeBudgetExceededAction,
    /// Maximum reduction units per root VM turn.
    pub max_reductions_per_turn: Option<u64>,
    /// Maximum GC-cell allocation bytes per root VM turn.
    pub max_allocated_bytes_per_turn: Option<u64>,
    /// Maximum host operations enqueued per root VM turn.
    pub max_host_ops_per_turn: Option<u64>,
    /// Maximum contiguous turn duration in nanoseconds.
    pub max_turn_nanos: Option<u64>,
    /// Maximum outstanding off-slot/external bytes at a turn boundary.
    pub max_external_bytes: Option<u64>,
    /// Maximum microtasks drained in one drain pass.
    pub max_microtasks_per_drain: Option<u64>,
}

impl RuntimeBudget {
    #[must_use]
    pub(crate) const fn rejects_on_exceedance(self) -> bool {
        matches!(self.on_exceeded, RuntimeBudgetExceededAction::Reject)
    }

    #[must_use]
    pub(crate) const fn has_heap_checkpoint_limits(self) -> bool {
        self.max_allocated_bytes_per_turn.is_some() || self.max_external_bytes.is_some()
    }
}

/// What the VM does when an observed budget limit is crossed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuntimeBudgetExceededAction {
    /// Record stats only. This preserves existing JS-visible behavior.
    #[default]
    Observe,
    /// Return a structural budget error at the next VM checkpoint.
    Reject,
}

/// Aggregate VM resource counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeBudgetStats {
    /// Root VM turns started.
    pub turns_started: u64,
    /// Root VM turns completed or errored.
    pub turns_finished: u64,
    /// Total reduction units charged.
    pub reductions_executed: u64,
    /// `reductions_executed` sampled when the active root turn began. The
    /// turn-local count is the delta against it ([`Self::current_turn_reductions`]),
    /// so the per-instruction charge updates exactly one counter.
    pub turn_start_reductions: u64,
    /// Largest completed root-turn reduction count.
    pub max_turn_reductions: u64,
    /// GC-cell allocation bytes observed in the currently active root turn.
    pub current_turn_allocated_bytes: u64,
    /// Largest completed root-turn GC-cell allocation byte count.
    pub max_turn_allocated_bytes: u64,
    /// Longest completed root-turn duration in nanoseconds.
    pub max_turn_nanos: u64,
    /// Times an observed root turn exceeded a configured limit.
    pub budget_limit_observations: u64,
    /// Bytecode call frames entered.
    pub bytecode_calls: u64,
    /// Native calls invoked through the VM call glue.
    pub native_calls: u64,
    /// Constructor calls entered through `new` or synchronous construct.
    pub construct_calls: u64,
    /// Host operations enqueued from VM execution.
    pub host_ops_enqueued: u64,
    /// Host operations enqueued in the currently active root turn.
    pub current_turn_host_ops: u64,
    /// Largest completed root-turn host-op enqueue count.
    pub max_turn_host_ops: u64,
    /// Microtask drain calls entered.
    pub microtask_drains: u64,
    /// Microtasks executed by drain loops.
    pub microtasks_executed: u64,
    /// Object allocations observed across root VM turns.
    pub allocated_objects_observed: u64,
    /// GC-cell allocation bytes observed across root VM turns.
    pub allocated_bytes_observed: u64,
    /// Largest live heap byte count observed at a root-turn boundary.
    pub max_live_heap_bytes: u64,
    /// Largest tracked heap byte count observed at a root-turn boundary.
    pub max_tracked_heap_bytes: u64,
    /// Largest outstanding off-slot/external byte count observed at a
    /// root-turn boundary.
    pub max_external_bytes_observed: u64,
    /// Outstanding off-slot/external bytes observed at the latest root-turn
    /// boundary.
    pub current_external_bytes: u64,
    /// Deepest stack length observed at an instruction checkpoint.
    pub max_stack_depth_observed: u32,
    /// Cooperative yields caused by budget enforcement.
    ///
    /// This remains zero until the scheduler slice starts enforcing budgets.
    pub forced_yields: u64,
    /// Hard budget rejections caused by budget enforcement.
    ///
    /// This remains zero until the scheduler slice starts enforcing budgets.
    pub budget_rejections: u64,
}

impl RuntimeBudgetStats {
    /// Reduction units charged since the active root turn began.
    #[must_use]
    pub const fn current_turn_reductions(&self) -> u64 {
        self.reductions_executed
            .saturating_sub(self.turn_start_reductions)
    }

    pub(crate) fn begin_turn(&mut self) {
        self.turns_started = self.turns_started.saturating_add(1);
        self.turn_start_reductions = self.reductions_executed;
        self.current_turn_allocated_bytes = 0;
        self.current_turn_host_ops = 0;
        self.current_external_bytes = 0;
    }

    pub(crate) fn finish_turn(&mut self, elapsed: Duration, budget: RuntimeBudget) {
        self.turns_finished = self.turns_finished.saturating_add(1);
        let turn_reductions = self.current_turn_reductions();
        self.max_turn_reductions = self.max_turn_reductions.max(turn_reductions);
        self.max_turn_allocated_bytes = self
            .max_turn_allocated_bytes
            .max(self.current_turn_allocated_bytes);
        self.max_turn_host_ops = self.max_turn_host_ops.max(self.current_turn_host_ops);
        let nanos = duration_nanos(elapsed);
        self.max_turn_nanos = self.max_turn_nanos.max(nanos);
        if budget_exceeded(
            turn_reductions,
            self.current_turn_allocated_bytes,
            self.current_turn_host_ops,
            nanos,
            self.current_external_bytes,
            budget,
        ) {
            self.budget_limit_observations = self.budget_limit_observations.saturating_add(1);
        }
        self.turn_start_reductions = self.reductions_executed;
        self.current_turn_allocated_bytes = 0;
        self.current_turn_host_ops = 0;
        self.current_external_bytes = 0;
    }

    pub(crate) fn record_turn_heap_delta(
        &mut self,
        start: RuntimeHeapSnapshot,
        end: RuntimeHeapSnapshot,
    ) {
        self.observe_current_turn_heap_delta(start, end);
        self.allocated_objects_observed = self.allocated_objects_observed.saturating_add(
            end.allocated_objects_total
                .saturating_sub(start.allocated_objects_total),
        );
        self.allocated_bytes_observed = self
            .allocated_bytes_observed
            .saturating_add(self.current_turn_allocated_bytes);
    }

    pub(crate) fn observe_current_turn_heap_delta(
        &mut self,
        start: RuntimeHeapSnapshot,
        end: RuntimeHeapSnapshot,
    ) {
        self.current_turn_allocated_bytes = end
            .allocated_bytes_total
            .saturating_sub(start.allocated_bytes_total);
        self.max_live_heap_bytes = self.max_live_heap_bytes.max(end.live_bytes);
        self.max_tracked_heap_bytes = self.max_tracked_heap_bytes.max(end.tracked_heap_bytes);
        self.max_external_bytes_observed = self
            .max_external_bytes_observed
            .max(end.external_reserved_bytes);
        self.current_external_bytes = end.external_reserved_bytes;
    }

    #[inline]
    pub(crate) fn record_reductions(&mut self, units: u64) {
        self.reductions_executed = self.reductions_executed.saturating_add(units);
    }

    pub(crate) fn record_bytecode_calls(&mut self, calls: u64) {
        self.bytecode_calls = self.bytecode_calls.saturating_add(calls);
    }

    pub(crate) fn record_native_call(&mut self) {
        self.native_calls = self.native_calls.saturating_add(1);
    }

    pub(crate) fn record_construct_call(&mut self) {
        self.construct_calls = self.construct_calls.saturating_add(1);
    }

    pub(crate) fn record_host_op_enqueued(&mut self) {
        self.host_ops_enqueued = self.host_ops_enqueued.saturating_add(1);
        self.current_turn_host_ops = self.current_turn_host_ops.saturating_add(1);
    }

    pub(crate) fn record_microtask_drain_started(&mut self) {
        self.microtask_drains = self.microtask_drains.saturating_add(1);
    }

    pub(crate) fn record_microtask_executed(&mut self) {
        self.microtasks_executed = self.microtasks_executed.saturating_add(1);
    }

    pub(crate) fn record_budget_limit_observation(&mut self) {
        self.budget_limit_observations = self.budget_limit_observations.saturating_add(1);
    }

    pub(crate) fn record_budget_rejection(&mut self) {
        self.budget_rejections = self.budget_rejections.saturating_add(1);
    }
}

/// Heap snapshot used for turn-boundary allocation deltas.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RuntimeHeapSnapshot {
    allocated_objects_total: u64,
    allocated_bytes_total: u64,
    live_bytes: u64,
    tracked_heap_bytes: u64,
    external_reserved_bytes: u64,
}

impl RuntimeHeapSnapshot {
    pub(crate) fn from_heap(heap: &mut GcHeap) -> Self {
        let heap_stats = heap.stats();
        let stats = heap.gc_stats();
        let allocated_objects_total = stats
            .by_type
            .iter()
            .fold(0_u64, |acc, row| acc.saturating_add(row.alloc_count_total));
        Self {
            allocated_objects_total,
            allocated_bytes_total: stats.alloc_bytes_total,
            live_bytes: u64::try_from(stats.live_bytes).unwrap_or(u64::MAX),
            tracked_heap_bytes: heap_stats.tracked_bytes,
            external_reserved_bytes: heap_stats.reserved_bytes,
        }
    }
}

/// Reduction charge for one executed opcode.
///
/// The interpreter never evaluates this at dispatch time: the charge is baked
/// into every execution record when its owning `CodeBlock` is built.
#[must_use]
pub(crate) const fn opcode_reductions(op: Op) -> u8 {
    match op {
        Op::Call
        | Op::CallWithThis
        | Op::CallMethodValue
        | Op::MathCall
        | Op::CallSpread
        | Op::New
        | Op::NewSpread
        | Op::SuperConstructSpread
        | Op::Await
        | Op::Yield => 8,
        Op::LoadProperty
        | Op::StoreProperty
        | Op::LoadElement
        | Op::StoreElement
        | Op::HasProperty
        | Op::DeleteProperty
        | Op::GetIterator
        | Op::IteratorNext => 4,
        Op::Eval | Op::NewFunction => 16,
        _ => 1,
    }
}

fn duration_nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

pub(crate) fn budget_exceeded(
    reductions: u64,
    allocated_bytes: u64,
    host_ops: u64,
    nanos: u64,
    external_bytes: u64,
    budget: RuntimeBudget,
) -> bool {
    budget
        .max_reductions_per_turn
        .is_some_and(|limit| reductions > limit)
        || budget
            .max_allocated_bytes_per_turn
            .is_some_and(|limit| allocated_bytes > limit)
        || budget
            .max_host_ops_per_turn
            .is_some_and(|limit| host_ops > limit)
        || budget.max_turn_nanos.is_some_and(|limit| nanos > limit)
        || budget
            .max_external_bytes
            .is_some_and(|limit| external_bytes > limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_opcodes_charge_more_than_register_ops() {
        assert!(opcode_reductions(Op::Call) > opcode_reductions(Op::LoadUndefined));
        assert!(opcode_reductions(Op::Eval) > opcode_reductions(Op::Call));
    }

    #[test]
    fn budget_exceedance_is_observed_not_rejected() {
        let budget = RuntimeBudget {
            on_exceeded: RuntimeBudgetExceededAction::Observe,
            max_reductions_per_turn: Some(1),
            max_allocated_bytes_per_turn: None,
            max_host_ops_per_turn: None,
            max_turn_nanos: None,
            max_external_bytes: None,
            max_microtasks_per_drain: None,
        };
        let mut stats = RuntimeBudgetStats::default();
        stats.begin_turn();
        stats.record_reductions(2);
        stats.finish_turn(Duration::from_nanos(0), budget);
        assert_eq!(stats.budget_limit_observations, 1);
        assert_eq!(stats.turns_finished, 1);
    }
}
