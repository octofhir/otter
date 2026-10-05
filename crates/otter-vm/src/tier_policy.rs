//! Deterministic native-tier admission from actually entered source opcodes.
//!
//! # Contents
//! - [`TierWorkModel`] converts immutable compiler geometry to opcode work.
//! - [`TierPolicy`] owns feedback origins, compiler attempts and retraining.
//! - [`crate::native_abi::SourceWork`] is the one shared execution scalar.
//!
//! # Invariants
//! - Interpreter dispatch and Template source paths charge each entered opcode
//!   attempt once. Entries, backedges and static spans never create work.
//! - Static geometry predicts compiler work and code-memory cost only.
//! - Wall-clock durations are diagnostics and never enter policy.
//! - Feedback changes restart the stable work origin; wakeup targets name that
//!   origin plus required work, preserving already observed work.
//! - Retraining additionally requires interpreted work and a complete fresh
//!   activation or loop iteration of its exact generation.
//! - Code objects retain the same scalar allocation through active retirement.
//!
//! # See also
//! - `benchmarks/tiering/README.md` for calibrated integer-work provenance.
//! - [`crate::executable::CodeBlock`] for executable and feedback ownership.

use crate::{Interpreter, executable::CodeBlock, native_abi::SourceWork};
use rustc_hash::FxHashMap;
use std::sync::Arc;

/// Per-isolate cap on requested executable mappings and owned generation
/// payloads, including permanent entry cells; not a total RSS bound.
///
/// The checked-in tier census peaked at 21.4 MiB of emitted
/// code in one workload. Three times that observed high-water mark leaves room
/// for live replacement generations while bounding retained mappings and their generation payloads.
pub(crate) const JIT_CODE_RESOURCE_LIMIT_BYTES: u64 = 64 * 1024 * 1024;

/// Property programs retained per site. The tier census observed 22,857
/// compiled property sites: 93.7% monomorphic and 99.6% at three ways or less;
/// the fourth way covered the remaining observed tail before its memory cost
/// exceeded another guard's measured hit contribution.
pub(crate) const PROFILED_PROPERTY_PIC_CAPACITY: usize = 4;

/// Ordinary call targets retained per site. The generated-plan census reached
/// four targets, but that stream excludes non-generated polymorphic ordinary
/// sites. The holdout regressed when those slots were truncated to four;
/// the measured eight-record layout is therefore retained until an uncensored
/// runtime target-cardinality histogram justifies a smaller allocation.
pub(crate) const PROFILED_CALL_TARGET_CAPACITY: usize = 8;

/// Native tier whose compiler work is being budgeted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum CostedTier {
    Template,
    Optimizing,
}

impl CostedTier {
    const fn index(self) -> usize {
        match self {
            Self::Template => 0,
            Self::Optimizing => 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TierWorkReason {
    Affordable,
    InsufficientWork,
    CodeMemoryBudget,
}

/// Pure scalar input; all execution evidence is actual opcode attempts.
/// Available code bytes are canonical retained-mapping and host-account headroom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TierWorkInput {
    pub(crate) tier: CostedTier,
    pub(crate) observed_work: u64,
    pub(crate) bytecode_instructions: u64,
    pub(crate) register_count: u64,
    pub(crate) parameter_count: u64,
    pub(crate) available_code_bytes: u64,
    pub(crate) previous_compile_attempts: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TierWorkDecision {
    reason: TierWorkReason,
    pub(crate) observed_work: u64,
    pub(crate) required_work: u64,
    pub(crate) estimated_code_bytes: u64,
    /// Absolute target in the canonical source scalar, or unreachable.
    pub(crate) work_target: Option<u64>,
}

impl TierWorkDecision {
    #[must_use]
    pub(crate) const fn should_compile(self) -> bool {
        matches!(self.reason, TierWorkReason::Affordable)
    }
    pub(crate) const fn resource_blocked(self) -> bool {
        matches!(self.reason, TierWorkReason::CodeMemoryBudget)
    }
    #[cfg(test)]
    pub(crate) const fn reason(self) -> TierWorkReason {
        self.reason
    }
}

/// Fixed integer source-work costs, reproduced by the checked-in fit script.
/// Conservative historical per-op calibration is converted once, rounding
/// compiler costs upward. No duration or entry/loop model exists at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TierWorkModel;

impl TierWorkModel {
    pub(crate) const fn calibrated() -> Self {
        Self
    }

    pub(crate) fn minimum_required_work(self, input: TierWorkInput) -> u64 {
        self.decide(input).required_work
    }

    pub(crate) fn decide(self, input: TierWorkInput) -> TierWorkDecision {
        let (base, instruction, register, parameter, code_base, code_instruction, memory_divisor) =
            match input.tier {
                CostedTier::Template => (334u64, 23u64, 4u64, 7u64, 560u64, 56u64, 3u64),
                CostedTier::Optimizing => (750u64, 88u64, 2u64, 4u64, 320u64, 38u64, 5u64),
            };
        let compile_work = base
            .saturating_add(instruction.saturating_mul(input.bytecode_instructions))
            .saturating_add(register.saturating_mul(input.register_count))
            .saturating_add(parameter.saturating_mul(input.parameter_count));
        let estimated_code_bytes = code_base
            .saturating_add(code_instruction.saturating_mul(input.bytecode_instructions))
            .saturating_add(16u64.saturating_mul(input.register_count));
        let memory_work = estimated_code_bytes / memory_divisor
            + u64::from(estimated_code_bytes % memory_divisor != 0);
        let cost = compile_work
            .saturating_mul(input.previous_compile_attempts.saturating_add(1))
            .saturating_add(memory_work);
        let required_work = cost.saturating_add(1);
        let reason = if estimated_code_bytes > input.available_code_bytes {
            TierWorkReason::CodeMemoryBudget
        } else if input.observed_work > cost {
            TierWorkReason::Affordable
        } else {
            TierWorkReason::InsufficientWork
        };
        TierWorkDecision {
            reason,
            observed_work: input.observed_work,
            required_work,
            estimated_code_bytes,
            work_target: (reason != TierWorkReason::CodeMemoryBudget)
                .then(|| cost.checked_add(1))
                .flatten(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Optimizer admission from the function's canonical source-work budget.
pub enum OptimizingDecision {
    /// Some work is visible, but work or resource admission remains unfunded.
    Cold,
    /// No source attempts are visible in the current feedback-work epoch.
    Warming,
    /// Changed feedback has not yet earned a funded stable-work window.
    FeedbackUnstable,
    /// Exact source work and physical headroom permit a compiler invocation.
    Promote,
}

#[derive(Debug, Default)]
struct FunctionTierState {
    /// Bound once at source admission or a source-owned deopt. A policy token
    /// may precede available executable storage; it cannot fund compilation
    /// until the canonical source has been bound.
    work: Option<Arc<SourceWork>>,
    last_feedback_epoch: Option<u32>,
    stable_work_origin: u64,
    observed_feedback_change: bool,
    compile_attempts: [u64; 2],
    /// Exact resource demand learned only from a compiled mapping's refused
    /// admission. Same-epoch retries require this physical headroom.
    refused_code_bytes: [u64; 2],
    retraining_generation: u64,
    retraining: Option<RetrainingState>,
}

#[derive(Debug)]
struct RetrainingState {
    remaining_work: u64,
    completed_path: bool,
}

/// Evidence carried by one interpreter activation, independently of recursion.
/// No GC values or physical frame addresses enter the policy.
#[derive(Debug, Clone)]
pub(crate) struct RetrainingActivation {
    pub(crate) generation: u64,
    fresh: bool,
    seen_loop_headers: Vec<u32>,
}

impl RetrainingActivation {
    pub(crate) fn new(generation: u64, fresh: bool) -> Self {
        Self {
            generation,
            fresh,
            seen_loop_headers: Vec::new(),
        }
    }

    pub(crate) fn is_fresh(&self) -> bool {
        self.fresh
    }

    /// The first backedge may finish only a deopt suffix. Returning to the
    /// same header again proves a whole interpreted iteration of this frame.
    pub(crate) fn observe_backedge(&mut self, header: u32) -> bool {
        let completed = self.seen_loop_headers.contains(&header);
        if !completed {
            self.seen_loop_headers.push(header);
        }
        completed
    }
}

#[derive(Debug, Default)]
pub(crate) struct TierPolicy {
    functions: FxHashMap<u32, FunctionTierState>,
    retraining_functions: usize,
}

impl TierPolicy {
    pub(crate) fn bind_source(&mut self, function: &CodeBlock) {
        let state = self.functions.entry(function.id).or_default();
        if let Some(work) = &state.work {
            assert!(
                Arc::ptr_eq(work, function.source_work()),
                "one function owns one work scalar"
            );
        } else {
            state.work = Some(Arc::clone(function.source_work()));
        }
    }

    pub(crate) fn begin_retraining(&mut self, function_id: u32, work: u64) {
        let state = self.functions.entry(function_id).or_default();
        if state.retraining.is_none() {
            self.retraining_functions += 1;
        }
        state.retraining_generation = state
            .retraining_generation
            .checked_add(1)
            .expect("function retraining generation overflow");
        state.retraining = Some(RetrainingState {
            remaining_work: work.max(1),
            completed_path: false,
        });
        state.last_feedback_epoch = None;
        state.observed_feedback_change = false;
    }
    #[inline]
    pub(crate) fn has_retraining(&self) -> bool {
        self.retraining_functions != 0
    }
    #[inline]
    pub(crate) fn retraining_generation(&self, fid: u32) -> Option<u64> {
        if !self.has_retraining() {
            return None;
        }
        let state = self.functions.get(&fid)?;
        state
            .retraining
            .as_ref()
            .map(|_| state.retraining_generation)
    }
    pub(crate) fn note_interpreted_work(&mut self, fid: u32, generation: u64) {
        self.update_retraining(fid, generation, false);
    }
    pub(crate) fn note_completed_interpreted_path(&mut self, fid: u32, generation: u64) {
        self.update_retraining(fid, generation, true);
    }
    fn update_retraining(&mut self, fid: u32, generation: u64, completed: bool) {
        let Some(state) = self.functions.get_mut(&fid) else {
            return;
        };
        if state.retraining_generation != generation {
            return;
        }
        let Some(retraining) = state.retraining.as_mut() else {
            return;
        };
        if completed {
            retraining.completed_path = true;
        } else {
            retraining.remaining_work = retraining.remaining_work.saturating_sub(1);
        }
        if retraining.remaining_work == 0 && retraining.completed_path {
            state.retraining = None;
            self.retraining_functions -= 1;
        }
    }

    pub(crate) fn observe_template_generation(&mut self, function: &CodeBlock) {
        self.bind_source(function);
        let state = self.functions.get_mut(&function.id).expect("bound source");
        if state.last_feedback_epoch.is_none() {
            state.last_feedback_epoch = Some(function.feedback_epoch());
            state.stable_work_origin = function.source_work().total();
        }
    }

    pub(crate) fn evict_function_range(&mut self, start: u32, end: u32) {
        self.retraining_functions -= self
            .functions
            .iter()
            .filter(|(fid, state)| **fid >= start && **fid < end && state.retraining.is_some())
            .count();
        self.functions
            .retain(|fid, _| !(*fid >= start && *fid < end));
    }
    pub(crate) fn record_compile_attempt(&mut self, fid: u32, tier: CostedTier) {
        let attempts = &mut self.functions.entry(fid).or_default().compile_attempts[tier.index()];
        *attempts = attempts.saturating_add(1);
    }
    pub(crate) fn record_resource_refusal(
        &mut self,
        function: &CodeBlock,
        tier: CostedTier,
        retained_bytes: u64,
    ) {
        self.bind_source(function);
        let state = self.functions.get_mut(&function.id).expect("bound source");
        state.refused_code_bytes[tier.index()] =
            state.refused_code_bytes[tier.index()].max(retained_bytes);
    }

    pub(crate) fn compile_attempts(&self, fid: u32, tier: CostedTier) -> u64 {
        self.functions
            .get(&fid)
            .map_or(0, |state| state.compile_attempts[tier.index()])
    }

    pub(crate) fn decide(
        &mut self,
        function: &CodeBlock,
        tier: CostedTier,
        available_code_bytes: u64,
    ) -> TierWorkDecision {
        self.bind_source(function);
        let state = self.functions.get_mut(&function.id).expect("bound source");
        let total = function.source_work().total();
        let epoch = function.feedback_epoch();
        match state.last_feedback_epoch {
            None => {
                state.last_feedback_epoch = Some(epoch);
                state.stable_work_origin = total;
            }
            Some(previous) if previous == epoch => {}
            Some(_) => {
                state.last_feedback_epoch = Some(epoch);
                state.stable_work_origin = total;
                state.observed_feedback_change = true;
                state.refused_code_bytes = [0; 2];
            }
        }
        let origin = match tier {
            CostedTier::Template => 0,
            CostedTier::Optimizing => state.stable_work_origin,
        };
        let mut decision = TierWorkModel::calibrated().decide(TierWorkInput {
            tier,
            observed_work: total.saturating_sub(origin),
            bytecode_instructions: function.code.len() as u64,
            register_count: u64::from(function.register_count),
            parameter_count: u64::from(function.param_count),
            available_code_bytes,
            previous_compile_attempts: state.compile_attempts[tier.index()],
        });
        if state.refused_code_bytes[tier.index()] > available_code_bytes {
            decision.reason = TierWorkReason::CodeMemoryBudget;
            decision.work_target = None;
        }
        decision.work_target = decision
            .work_target
            .and_then(|required| origin.checked_add(required));
        if state.retraining.is_some()
            || (decision.work_target.is_none()
                && decision.reason != TierWorkReason::CodeMemoryBudget)
        {
            decision.reason = TierWorkReason::InsufficientWork;
        }
        decision
    }

    pub(crate) fn optimizing_decision(
        &mut self,
        function: &CodeBlock,
        available_code_bytes: u64,
    ) -> OptimizingDecision {
        let decision = self.decide(function, CostedTier::Optimizing, available_code_bytes);
        let state = self.functions.get(&function.id).expect("bound source");
        if decision.should_compile() {
            OptimizingDecision::Promote
        } else if state.observed_feedback_change {
            OptimizingDecision::FeedbackUnstable
        } else if decision.observed_work == 0 {
            OptimizingDecision::Warming
        } else {
            OptimizingDecision::Cold
        }
    }
}

impl Interpreter {
    #[must_use]
    pub(crate) fn optimizing_tier_decision_for(
        &mut self,
        function: &CodeBlock,
        available_code_bytes: u64,
    ) -> OptimizingDecision {
        self.optimizing_tier_policy
            .optimizing_decision(function, available_code_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retraining_loop_completion_survives_alternating_nested_headers() {
        let mut activation = RetrainingActivation::new(1, false);
        assert!(!activation.observe_backedge(10));
        assert!(!activation.observe_backedge(20));
        assert!(activation.observe_backedge(10));
        assert!(activation.observe_backedge(20));
    }

    #[test]
    fn retraining_requires_exact_work_and_completed_path() {
        let mut policy = TierPolicy::default();
        policy.begin_retraining(7, 3);
        let generation = policy.retraining_generation(7).unwrap();
        for _ in 0..3 {
            policy.note_interpreted_work(7, generation);
        }
        assert_eq!(
            policy.retraining_generation(7),
            Some(generation),
            "a suffix alone is insufficient"
        );
        policy.note_completed_interpreted_path(7, generation);
        assert!(!policy.has_retraining());

        policy.begin_retraining(7, 3);
        let generation = policy.retraining_generation(7).unwrap();
        policy.note_completed_interpreted_path(7, generation);
        for _ in 0..2 {
            policy.note_interpreted_work(7, generation);
        }
        assert!(
            policy.has_retraining(),
            "completion cannot erase unpaid work"
        );
        policy.note_interpreted_work(7, generation);
        assert!(!policy.has_retraining());
    }

    #[test]
    fn new_retraining_generation_rejects_older_recursive_evidence_and_evicts() {
        let mut policy = TierPolicy::default();
        policy.begin_retraining(7, 1);
        let old = policy.retraining_generation(7).unwrap();
        policy.begin_retraining(7, 1);
        let current = policy.retraining_generation(7).unwrap();
        assert_ne!(old, current);
        policy.note_completed_interpreted_path(7, old);
        policy.note_interpreted_work(7, old);
        assert_eq!(policy.retraining_generation(7), Some(current));
        policy.note_interpreted_work(7, current);
        assert!(policy.has_retraining());
        policy.note_completed_interpreted_path(7, current);
        assert!(!policy.has_retraining());

        policy.begin_retraining(7, 1);
        policy.begin_retraining(8, 1);
        policy.evict_function_range(7, 8);
        assert!(policy.has_retraining());
        assert!(policy.retraining_generation(7).is_none());
        policy.evict_function_range(8, 9);
        assert!(!policy.has_retraining());
    }

    fn input(tier: CostedTier) -> TierWorkInput {
        TierWorkInput {
            tier,
            observed_work: 0,
            bytecode_instructions: 32,
            register_count: 8,
            parameter_count: 2,
            available_code_bytes: JIT_CODE_RESOURCE_LIMIT_BYTES,
            previous_compile_attempts: 0,
        }
    }
    #[test]
    fn work_admission_has_exact_monotone_boundary() {
        for tier in [CostedTier::Template, CostedTier::Optimizing] {
            let mut value = input(tier);
            let target = TierWorkModel::calibrated().minimum_required_work(value);
            value.observed_work = target - 1;
            assert!(!TierWorkModel::calibrated().decide(value).should_compile());
            for work in [target, target + 1, u64::MAX] {
                value.observed_work = work;
                assert!(TierWorkModel::calibrated().decide(value).should_compile());
            }
        }
    }
    #[test]
    fn work_geometry_never_creates_execution_and_resource_cap_remains_hard() {
        let mut value = input(CostedTier::Template);
        for instructions in [1, 32, 1_000_000] {
            value.bytecode_instructions = instructions;
            assert!(!TierWorkModel::calibrated().decide(value).should_compile());
        }
        value.observed_work = u64::MAX;
        value.available_code_bytes = 0;
        assert_eq!(
            TierWorkModel::calibrated().decide(value).reason(),
            TierWorkReason::CodeMemoryBudget
        );
        value.available_code_bytes = JIT_CODE_RESOURCE_LIMIT_BYTES;
        value.previous_compile_attempts = u64::MAX;
        assert!(
            !TierWorkModel::calibrated().decide(value).should_compile(),
            "saturated cost cannot be funded"
        );
    }
    #[test]
    fn feedback_origin_and_wakeup_preserve_already_observed_work() {
        use otter_bytecode::Op;
        let function = CodeBlock::jit_test_stub(
            7,
            0,
            1,
            &[crate::jit::JitTestInstruction::new(
                Op::ReturnUndefined,
                0,
                0,
                vec![],
            )],
            &[],
        );
        let mut policy = TierPolicy::default();
        function.source_work().charge(100);
        let first = policy.decide(
            &function,
            CostedTier::Optimizing,
            JIT_CODE_RESOURCE_LIMIT_BYTES,
        );
        assert_eq!(first.observed_work, 0);
        let target = first.work_target.unwrap();
        function.source_work().charge(first.required_work / 2);
        let halfway = policy.decide(
            &function,
            CostedTier::Optimizing,
            JIT_CODE_RESOURCE_LIMIT_BYTES,
        );
        assert_eq!(halfway.work_target, Some(target));
        assert!(halfway.observed_work > 0);
        function.bump_feedback_epoch();
        let changed = policy.decide(
            &function,
            CostedTier::Optimizing,
            JIT_CODE_RESOURCE_LIMIT_BYTES,
        );
        assert_eq!(changed.observed_work, 0);
        assert_eq!(
            changed.work_target,
            function
                .source_work()
                .total()
                .checked_add(changed.required_work)
        );
    }
    #[test]
    fn source_work_resource_requirement_is_exact_epoch_scoped_and_headroom_reversible() {
        let function = CodeBlock::jit_test_stub(7, 0, 1, &[], &[]);
        let mut policy = TierPolicy::default();
        function.source_work().charge(1_000_000);
        assert!(
            policy
                .decide(&function, CostedTier::Template, 2048)
                .should_compile()
        );
        policy.record_compile_attempt(7, CostedTier::Template);
        policy.record_resource_refusal(&function, CostedTier::Template, 4096);
        let blocked = policy.decide(&function, CostedTier::Template, 2048);
        assert!(blocked.resource_blocked());
        assert_eq!(blocked.work_target, None);
        assert!(
            policy
                .decide(&function, CostedTier::Template, 4096)
                .should_compile(),
            "unchanged work can compile after real headroom is restored"
        );
        function.bump_feedback_epoch();
        assert!(
            policy
                .decide(&function, CostedTier::Template, 2048)
                .should_compile(),
            "changed feedback can produce a different finalized mapping"
        );
        assert_eq!(policy.compile_attempts(7, CostedTier::Template), 1);
    }

    #[test]
    fn compiler_attempts_share_function_ownership_and_eviction() {
        let mut policy = TierPolicy::default();
        let initial =
            TierWorkModel::calibrated().minimum_required_work(input(CostedTier::Template));
        for attempt in 1..=4 {
            policy.record_compile_attempt(7, CostedTier::Template);
            assert_eq!(policy.compile_attempts(7, CostedTier::Template), attempt);
            assert_eq!(policy.compile_attempts(7, CostedTier::Optimizing), 0);
            let mut value = input(CostedTier::Template);
            value.previous_compile_attempts = attempt;
            assert!(TierWorkModel::calibrated().minimum_required_work(value) > initial);
        }
        policy.record_compile_attempt(8, CostedTier::Template);
        policy.evict_function_range(7, 8);
        assert_eq!(policy.compile_attempts(7, CostedTier::Template), 0);
        assert_eq!(policy.compile_attempts(8, CostedTier::Template), 1);
    }
}
