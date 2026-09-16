//! Unified work-slice budget and VM accounting snapshots.
//!
//! This module owns the VM-side data contract for BEAM-style runtime
//! accounting: work units, turn latency, allocation pressure, host-op enqueue
//! counts, and major call-shape counters. Observe mode records them without
//! changing execution; Reject mode turns a crossing into a structural error;
//! Yield mode closes the current slice and resumes the same isolate turn.
//!
//! # Contents
//! - [`WorkBudget`] — optional per-turn policy limits and enforcement mode.
//! - [`WorkBudgetExceededAction`] — outcome policy when a limit is crossed.
//! - [`WorkBudgetStats`] — aggregate counters exposed for diagnostics.
//! - [`WorkBudgetTelemetry`] — the shareable cell an embedder reads the
//!   counters from without owning the isolate.
//! - Static work-charge helpers for the interpreter dispatch loop.
//!
//! # Invariants
//! - Budget DTOs are owned, copyable data; no VM handles cross the boundary.
//! - Bytecode, generated backedges, native calls, GC, RegExp, and microtasks
//!   debit one monotonic work-unit counter.
//! - [`WorkBudgetExceededAction::Observe`] records an exceedance,
//!   [`WorkBudgetExceededAction::Reject`] returns a structural error, and
//!   [`WorkBudgetExceededAction::Yield`] rotates the owning isolate's work
//!   slice without exposing an ECMAScript completion.
//! - Work accounting is approximate and stable, not a wall-clock timer.
//! - Telemetry is published at slice boundaries and at every enforcement
//!   rejection, so a reader observes whole slices rather than a torn count.
//!
//! # See also
//! - [`crate::Interpreter`]
//! - [`crate::VmError`]

use otter_bytecode::Op;
use otter_gc::GcHeap;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Optional execution budget policy for one contiguous VM work slice.
///
/// Interpreter dispatch checks enforcing budgets before every instruction.
/// Native JIT loops decrement an inline fuel counter and re-enter the same
/// checkpoint in bounded batches. Microtasks, native calls, RegExp, and GC all
/// debit that same work counter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkBudget {
    /// Outcome policy when a configured limit is crossed.
    pub on_exceeded: WorkBudgetExceededAction,
    /// Maximum unified work units per contiguous isolate slice.
    pub max_work_units_per_turn: Option<u64>,
    /// Maximum GC-cell allocation bytes per isolate work slice.
    pub max_allocated_bytes_per_turn: Option<u64>,
    /// Maximum host operations enqueued per isolate work slice.
    pub max_host_ops_per_turn: Option<u64>,
    /// Maximum contiguous slice duration in nanoseconds.
    pub max_turn_nanos: Option<u64>,
    /// Maximum outstanding off-slot/external bytes at a checkpoint.
    pub max_external_bytes: Option<u64>,
}

impl WorkBudget {
    #[must_use]
    pub(crate) const fn enforces_on_exceedance(self) -> bool {
        !matches!(self.on_exceeded, WorkBudgetExceededAction::Observe)
    }

    #[must_use]
    pub(crate) const fn yields_on_exceedance(self) -> bool {
        matches!(self.on_exceeded, WorkBudgetExceededAction::Yield)
    }

    #[must_use]
    pub(crate) const fn needs_heap_checkpoint(self) -> bool {
        self.max_work_units_per_turn.is_some()
            || self.max_allocated_bytes_per_turn.is_some()
            || self.max_external_bytes.is_some()
    }
}

/// Result of a cooperative work-budget checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkBudgetCheckpoint {
    Continue,
    Yield,
}

/// What the VM does when an observed budget limit is crossed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkBudgetExceededAction {
    /// Record stats only. This preserves existing JS-visible behavior.
    #[default]
    Observe,
    /// Return a structural budget error at the next VM checkpoint.
    Reject,
    /// Cooperatively rotate the current isolate work slice, then resume the
    /// same ECMAScript turn before any later macrotask. A non-resettable
    /// external-memory overage still rejects rather than yield-loop forever.
    Yield,
}

/// Shareable cell carrying one isolate's budget counters off its own thread.
///
/// The interpreter owns the authoritative [`WorkBudgetStats`] and copies
/// them here at each slice boundary and at each enforcement rejection.
/// A holder of the cell — a runtime handle, an embedder, a telemetry
/// exporter — therefore reads whole turns, never a count torn out of the
/// middle of one. Cloning shares the same cell; two isolates never publish
/// into one.
#[derive(Clone, Debug, Default)]
pub struct WorkBudgetTelemetry(Arc<Mutex<WorkBudgetStats>>);

impl WorkBudgetTelemetry {
    /// Create an unpublished cell whose counters are all zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Read the counters published by the owning isolate's most recent
    /// boundary.
    #[must_use]
    pub fn snapshot(&self) -> WorkBudgetStats {
        *self.lock()
    }

    pub(crate) fn publish(&self, stats: WorkBudgetStats) {
        *self.lock() = stats;
    }

    /// A publisher that panicked mid-write would leave the cell poisoned;
    /// the counters are plain copyable data, so recovering the value is
    /// sound and keeps telemetry readable after an unrelated panic.
    fn lock(&self) -> std::sync::MutexGuard<'_, WorkBudgetStats> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Aggregate VM resource counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkBudgetStats {
    /// Isolate work slices started.
    pub turns_started: u64,
    /// Isolate work slices completed, yielded, or errored.
    pub turns_finished: u64,
    /// Total unified work units charged.
    pub work_units_executed: u64,
    /// RegExp backtrack points charged into `work_units_executed`.
    pub regex_backtrack_steps: u64,
    /// GC work units charged into `work_units_executed`.
    pub gc_work_units: u64,
    /// GC work charged since the active slice began. Used to make repeated
    /// heap checkpoints idempotent.
    pub current_turn_gc_work_units: u64,
    /// `work_units_executed` sampled when the active work slice began. The
    /// slice-local count is the delta against it ([`Self::current_turn_work_units`]),
    /// so the per-instruction charge updates exactly one counter.
    pub turn_start_work_units: u64,
    /// Largest completed work-slice unit count.
    pub max_turn_work_units: u64,
    /// GC-cell allocation bytes observed in the active slice.
    pub current_turn_allocated_bytes: u64,
    /// Largest completed slice GC-cell allocation byte count.
    pub max_turn_allocated_bytes: u64,
    /// Longest completed work-slice duration in nanoseconds.
    pub max_turn_nanos: u64,
    /// Times an observed work slice exceeded a configured limit.
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
    /// Largest completed work-slice host-op enqueue count.
    pub max_turn_host_ops: u64,
    /// Microtask drain calls entered.
    pub microtask_drains: u64,
    /// Microtasks executed by drain loops.
    pub microtasks_executed: u64,
    /// Object allocations observed across VM work slices.
    pub allocated_objects_observed: u64,
    /// GC-cell allocation bytes observed across VM work slices.
    pub allocated_bytes_observed: u64,
    /// Largest live heap byte count observed at a work-slice boundary.
    pub max_live_heap_bytes: u64,
    /// Largest tracked heap byte count observed at a work-slice boundary.
    pub max_tracked_heap_bytes: u64,
    /// Largest outstanding off-slot/external byte count observed at a
    /// work-slice boundary.
    pub max_external_bytes_observed: u64,
    /// Outstanding off-slot/external bytes observed at the latest work-slice
    /// boundary.
    pub current_external_bytes: u64,
    /// Deepest stack length observed at an instruction checkpoint.
    pub max_stack_depth_observed: u32,
    /// Cooperative yields caused by budget enforcement.
    ///
    pub forced_yields: u64,
    /// Hard budget rejections caused by budget enforcement.
    pub budget_rejections: u64,
}

impl WorkBudgetStats {
    /// Work units charged since the active isolate slice began.
    #[must_use]
    pub const fn current_turn_work_units(&self) -> u64 {
        self.work_units_executed
            .saturating_sub(self.turn_start_work_units)
    }

    pub(crate) fn begin_turn(&mut self) {
        self.turns_started = self.turns_started.saturating_add(1);
        self.turn_start_work_units = self.work_units_executed;
        self.current_turn_allocated_bytes = 0;
        self.current_turn_host_ops = 0;
        self.current_external_bytes = 0;
        self.current_turn_gc_work_units = 0;
    }

    pub(crate) fn finish_turn(&mut self, elapsed: Duration, budget: WorkBudget) {
        self.turns_finished = self.turns_finished.saturating_add(1);
        let turn_work_units = self.current_turn_work_units();
        self.max_turn_work_units = self.max_turn_work_units.max(turn_work_units);
        self.max_turn_allocated_bytes = self
            .max_turn_allocated_bytes
            .max(self.current_turn_allocated_bytes);
        self.max_turn_host_ops = self.max_turn_host_ops.max(self.current_turn_host_ops);
        let nanos = duration_nanos(elapsed);
        self.max_turn_nanos = self.max_turn_nanos.max(nanos);
        if budget_exceeded(
            turn_work_units,
            self.current_turn_allocated_bytes,
            self.current_turn_host_ops,
            nanos,
            self.current_external_bytes,
            budget,
        ) {
            self.budget_limit_observations = self.budget_limit_observations.saturating_add(1);
        }
        self.turn_start_work_units = self.work_units_executed;
        self.current_turn_allocated_bytes = 0;
        self.current_turn_host_ops = 0;
        self.current_external_bytes = 0;
        self.current_turn_gc_work_units = 0;
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
        let turn_gc_work = end.gc_work_total.saturating_sub(start.gc_work_total);
        let additional_gc_work = turn_gc_work.saturating_sub(self.current_turn_gc_work_units);
        self.current_turn_gc_work_units = turn_gc_work;
        self.record_gc_work(additional_gc_work);
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

    /// Meter one operation's work weight. Wrapping rather than
    /// saturating: this runs on every dispatched instruction, and exhausting a
    /// `u64` work count is unreachable, so the saturation test is pure hot
    /// path cost.
    #[inline]
    pub(crate) fn record_work(&mut self, units: u64) {
        self.work_units_executed = self.work_units_executed.wrapping_add(units);
    }

    /// Charge exact matcher work both to its diagnostic counter and to the
    /// shared work ledger used by the isolate work budget.
    pub(crate) fn record_regex_backtrack_steps(&mut self, steps: u64) {
        self.regex_backtrack_steps = self.regex_backtrack_steps.saturating_add(steps);
        self.record_work(steps);
    }

    pub(crate) fn record_bytecode_calls(&mut self, calls: u64) {
        self.bytecode_calls = self.bytecode_calls.saturating_add(calls);
    }

    pub(crate) fn record_native_call(&mut self) {
        self.native_calls = self.native_calls.saturating_add(1);
        self.record_work(8);
    }

    pub(crate) fn record_construct_call(&mut self) {
        self.construct_calls = self.construct_calls.saturating_add(1);
        self.record_work(8);
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
        self.record_work(8);
    }

    pub(crate) fn record_budget_rejection(&mut self) {
        self.budget_rejections = self.budget_rejections.saturating_add(1);
    }

    pub(crate) fn record_forced_yield(&mut self) {
        self.forced_yields = self.forced_yields.saturating_add(1);
    }

    pub(crate) fn record_gc_work(&mut self, units: u64) {
        self.gc_work_units = self.gc_work_units.saturating_add(units);
        self.record_work(units);
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
    gc_work_total: u64,
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
            // Full collections have no cumulative visited-slot counter yet;
            // give each cycle a fixed base cost. Minor collections expose the
            // exact slot walk, plus a fixed cycle cost for root processing.
            gc_work_total: stats
                .gc_cycles
                .saturating_mul(1_024)
                .saturating_add(stats.minor_gc_cycles.saturating_mul(64))
                .saturating_add(stats.minor_slots_scanned),
        }
    }
}

/// Static work charge for one executed opcode.
///
/// The interpreter never evaluates this at dispatch time: the charge is baked
/// into every execution record when its owning `CodeBlock` is built.
#[must_use]
pub(crate) const fn opcode_work_units(op: Op) -> u8 {
    match op {
        Op::Call
        | Op::CallWithThis
        | Op::CallForwardArguments
        | Op::CallMethodValue
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
    work_units: u64,
    allocated_bytes: u64,
    host_ops: u64,
    nanos: u64,
    external_bytes: u64,
    budget: WorkBudget,
) -> bool {
    budget
        .max_work_units_per_turn
        .is_some_and(|limit| work_units > limit)
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

pub(crate) fn external_budget_exceeded(external_bytes: u64, budget: WorkBudget) -> bool {
    budget
        .max_external_bytes
        .is_some_and(|limit| external_bytes > limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_opcodes_charge_more_than_register_ops() {
        assert!(opcode_work_units(Op::Call) > opcode_work_units(Op::LoadUndefined));
        assert!(opcode_work_units(Op::Eval) > opcode_work_units(Op::Call));
    }

    #[test]
    fn every_opcode_has_a_nonzero_static_work_charge() {
        for (op, _) in otter_bytecode::encoding::OP_BYTE_TABLE {
            assert_ne!(opcode_work_units(*op), 0, "missing work charge for {op:?}");
        }
    }

    #[test]
    fn observe_mode_records_exceedance_without_rejection() {
        let budget = WorkBudget {
            on_exceeded: WorkBudgetExceededAction::Observe,
            max_work_units_per_turn: Some(1),
            max_allocated_bytes_per_turn: None,
            max_host_ops_per_turn: None,
            max_turn_nanos: None,
            max_external_bytes: None,
        };
        let mut stats = WorkBudgetStats::default();
        stats.begin_turn();
        stats.record_work(2);
        stats.finish_turn(Duration::from_nanos(0), budget);
        assert_eq!(stats.budget_limit_observations, 1);
        assert_eq!(stats.turns_finished, 1);
    }
}
