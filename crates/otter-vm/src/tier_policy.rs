//! Measured, deterministic native-tier cost policy.
//!
//! # Contents
//! - [`TierCostModel`] evaluates promotion and replacement from scalar costs.
//! - [`TierCostInput`] is the pure, synthetic-testable policy input.
//! - [`TierPolicy`] retains the execution point of the latest material
//!   feedback change for each function.
//! - [`Interpreter::optimizing_tier_decision_for`] applies the model to a live
//!   function without compiling or installing code.
//!
//! # Invariants
//! - A compile is admitted only when expected saved execution time is greater
//!   than predicted compile time, cumulative recompilation cost, and the code
//!   memory charge.
//! - Decisions are monotonic in executions while all other inputs are fixed.
//! - Feedback changes restart the stable execution observation; there is no
//!   sample-count stability threshold.
//! - Coefficients come from the checked-in Step 6 census and derivation script.
//!   They are engine policy, never environment or embedder knobs.
//! - The executable-memory resource limit is a separately named hard cap, not
//!   a profitability threshold.
//!
//! # See also
//! - `benchmarks/tiering/README.md`
//! - [`crate::executable::CodeBlock::feedback_epoch`]

use rustc_hash::FxHashMap;

use crate::Interpreter;

/// Hard per-isolate executable-code resource cap.
///
/// The complete pre-policy V8-v7/Octane census peaked at 21.4 MiB of emitted
/// code in one workload. Three times that observed high-water mark leaves room
/// for live replacement generations while bounding executable mappings.
pub(crate) const JIT_CODE_RESOURCE_LIMIT_BYTES: u64 = 64 * 1024 * 1024;

/// Property programs retained per site. The Step 6 census observed 22,857
/// compiled property sites: 93.7% monomorphic and 99.6% at three ways or less;
/// the fourth way covered the remaining observed tail before its memory cost
/// exceeded another guard's measured hit contribution.
pub(crate) const PROFILED_PROPERTY_PIC_CAPACITY: usize = 4;

/// Ordinary call targets retained per site. The generated-plan census reached
/// four targets, but that stream excludes non-generated polymorphic ordinary
/// sites. The V8-v7 holdout regressed when those slots were truncated to four;
/// the measured eight-record layout is therefore retained until an uncensored
/// runtime target-cardinality histogram justifies a smaller allocation.
pub(crate) const PROFILED_CALL_TARGET_CAPACITY: usize = 8;

/// Native tier whose next code object is being costed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum CostedTier {
    Template,
    Optimizing,
}

/// Dynamic observation that would trigger compilation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TierTrigger {
    FunctionEntry,
    /// A compiled caller already observed this target and can replace a Rust
    /// call transition with generated linkage once the target owns entry code.
    DirectCallTarget,
    LoopBackedge {
        /// Static instruction span between the loop header and latch. This is
        /// the generated work one successful OSR iteration avoids.
        span_instructions: u64,
    },
}

/// Why the pure model admitted or deferred a compile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TierCostReason {
    Profitable,
    InsufficientPayoff,
    CodeMemoryBudget,
}

/// Complete scalar input to one deterministic policy decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TierCostInput {
    pub(crate) tier: CostedTier,
    pub(crate) trigger: TierTrigger,
    /// Stable executions already observed; also the conservative estimate of
    /// remaining executions.
    pub(crate) executions: u64,
    /// Exits charged to the current generation when considering replacement.
    pub(crate) exits: u64,
    pub(crate) bytecode_instructions: u64,
    pub(crate) register_count: u64,
    pub(crate) parameter_count: u64,
    pub(crate) resident_code_bytes: u64,
    /// Actual duration of earlier compiles for this function. This makes a
    /// repeatedly rebuilt body progressively harder to justify without a
    /// fixed reoptimization count.
    pub(crate) cumulative_compile_ns: u64,
}

/// Auditable arithmetic behind one decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TierCostDecision {
    reason: TierCostReason,
    pub(crate) expected_remaining_executions: u64,
    pub(crate) estimated_saved_ns: u64,
    pub(crate) estimated_compile_ns: u64,
    pub(crate) estimated_code_bytes: u64,
    pub(crate) code_memory_charge_ns: u64,
}

impl TierCostDecision {
    #[must_use]
    pub(crate) const fn should_compile(self) -> bool {
        matches!(self.reason, TierCostReason::Profitable)
    }

    #[must_use]
    #[cfg(test)]
    pub(crate) const fn reason(self) -> TierCostReason {
        self.reason
    }
}

/// Integer coefficient set fitted by `benchmarks/tiering/fit_cost_model.py`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TierCostModel {
    template_compile_base_ns: u64,
    template_compile_per_instruction_ns: u64,
    optimizing_compile_base_ns: u64,
    optimizing_compile_per_instruction_ns: u64,
    compile_per_register_ns: u64,
    compile_per_parameter_ns: u64,
    template_code_base_bytes: u64,
    template_code_per_instruction_bytes: u64,
    optimizing_code_base_bytes: u64,
    optimizing_code_per_instruction_bytes: u64,
    code_per_register_bytes: u64,
    template_entry_saved_per_instruction_ns: u64,
    template_entry_transition_cost_ns: u64,
    optimizing_entry_saved_per_instruction_ns: u64,
    optimizing_entry_transition_cost_ns: u64,
    direct_call_target_saved_ns: u64,
    template_backedge_saved_per_instruction_ns: u64,
    optimizing_backedge_saved_per_instruction_ns: u64,
    exit_penalty_ns: u64,
    code_memory_charge_per_byte_ns: u64,
    entry_continuation_multiplier: u64,
    loop_continuation_multiplier: u64,
}

impl TierCostModel {
    /// Current coefficient set, generated from the checked-in census.
    #[must_use]
    pub(crate) const fn calibrated() -> Self {
        Self {
            template_compile_base_ns: 4_000,
            template_compile_per_instruction_ns: 270,
            optimizing_compile_base_ns: 15_000,
            optimizing_compile_per_instruction_ns: 1_750,
            compile_per_register_ns: 40,
            compile_per_parameter_ns: 80,
            template_code_base_bytes: 560,
            template_code_per_instruction_bytes: 56,
            optimizing_code_base_bytes: 320,
            optimizing_code_per_instruction_bytes: 38,
            code_per_register_bytes: 16,
            template_entry_saved_per_instruction_ns: 12,
            template_entry_transition_cost_ns: 96,
            optimizing_entry_saved_per_instruction_ns: 20,
            optimizing_entry_transition_cost_ns: 128,
            direct_call_target_saved_ns: 1_450,
            template_backedge_saved_per_instruction_ns: 103,
            optimizing_backedge_saved_per_instruction_ns: 108,
            exit_penalty_ns: 192,
            code_memory_charge_per_byte_ns: 4,
            entry_continuation_multiplier: 1,
            loop_continuation_multiplier: 1,
        }
    }

    const fn continuation_multiplier(self, trigger: TierTrigger) -> u64 {
        match trigger {
            TierTrigger::FunctionEntry => self.entry_continuation_multiplier,
            TierTrigger::DirectCallTarget => self.entry_continuation_multiplier,
            TierTrigger::LoopBackedge { .. } => self.loop_continuation_multiplier,
        }
    }

    const fn saved_per_execution(
        self,
        tier: CostedTier,
        trigger: TierTrigger,
        bytecode_instructions: u64,
    ) -> u64 {
        match (tier, trigger) {
            (CostedTier::Template, TierTrigger::FunctionEntry) => self
                .template_entry_saved_per_instruction_ns
                .saturating_mul(bytecode_instructions)
                .saturating_sub(self.template_entry_transition_cost_ns),
            (CostedTier::Optimizing, TierTrigger::FunctionEntry) => self
                .optimizing_entry_saved_per_instruction_ns
                .saturating_mul(bytecode_instructions)
                .saturating_sub(self.optimizing_entry_transition_cost_ns),
            (_, TierTrigger::DirectCallTarget) => self.direct_call_target_saved_ns,
            (CostedTier::Template, TierTrigger::LoopBackedge { span_instructions }) => self
                .template_backedge_saved_per_instruction_ns
                .saturating_mul(span_instructions),
            (CostedTier::Optimizing, TierTrigger::LoopBackedge { span_instructions }) => self
                .optimizing_backedge_saved_per_instruction_ns
                .saturating_mul(span_instructions),
        }
    }

    /// Smallest execution count that can have positive payoff for these static
    /// inputs. Generated linkage uses this only as a cold-policy wakeup point;
    /// the VM re-evaluates the complete live decision before compiling.
    #[must_use]
    pub(crate) fn minimum_profitable_executions(self, mut input: TierCostInput) -> u64 {
        input.executions = 0;
        input.exits = 0;
        let decision = self.decide(input);
        let saved = self
            .saved_per_execution(input.tier, input.trigger, input.bytecode_instructions)
            .saturating_mul(self.continuation_multiplier(input.trigger));
        decision
            .estimated_compile_ns
            .saturating_add(input.cumulative_compile_ns)
            .saturating_add(decision.code_memory_charge_ns)
            .checked_div(saved)
            .unwrap_or(u64::MAX)
            .saturating_add(1)
    }

    /// Evaluate promotion or generation replacement with saturating integer
    /// arithmetic. `exits` adds recovered deopt round-trip cost; it cannot make
    /// an otherwise profitable decision less profitable.
    #[must_use]
    pub(crate) fn decide(self, input: TierCostInput) -> TierCostDecision {
        let (compile_base, compile_per_instruction, code_base, code_per_instruction) =
            match input.tier {
                CostedTier::Template => (
                    self.template_compile_base_ns,
                    self.template_compile_per_instruction_ns,
                    self.template_code_base_bytes,
                    self.template_code_per_instruction_bytes,
                ),
                CostedTier::Optimizing => (
                    self.optimizing_compile_base_ns,
                    self.optimizing_compile_per_instruction_ns,
                    self.optimizing_code_base_bytes,
                    self.optimizing_code_per_instruction_bytes,
                ),
            };
        let saved_per_execution =
            self.saved_per_execution(input.tier, input.trigger, input.bytecode_instructions);
        let estimated_compile_ns = compile_base
            .saturating_add(compile_per_instruction.saturating_mul(input.bytecode_instructions))
            .saturating_add(
                self.compile_per_register_ns
                    .saturating_mul(input.register_count),
            )
            .saturating_add(
                self.compile_per_parameter_ns
                    .saturating_mul(input.parameter_count),
            );
        let estimated_code_bytes = code_base
            .saturating_add(code_per_instruction.saturating_mul(input.bytecode_instructions))
            .saturating_add(
                self.code_per_register_bytes
                    .saturating_mul(input.register_count),
            );
        let code_memory_charge_ns =
            estimated_code_bytes.saturating_mul(self.code_memory_charge_per_byte_ns);
        let expected_remaining_executions = input
            .executions
            .saturating_mul(self.continuation_multiplier(input.trigger));
        let estimated_saved_ns = expected_remaining_executions
            .saturating_mul(saved_per_execution)
            .saturating_add(input.exits.saturating_mul(self.exit_penalty_ns));
        let total_cost = estimated_compile_ns
            .saturating_add(input.cumulative_compile_ns)
            .saturating_add(code_memory_charge_ns);
        let reason = if input
            .resident_code_bytes
            .saturating_add(estimated_code_bytes)
            > JIT_CODE_RESOURCE_LIMIT_BYTES
        {
            TierCostReason::CodeMemoryBudget
        } else if estimated_saved_ns > total_cost {
            TierCostReason::Profitable
        } else {
            TierCostReason::InsufficientPayoff
        };
        TierCostDecision {
            reason,
            expected_remaining_executions,
            estimated_saved_ns,
            estimated_compile_ns,
            estimated_code_bytes,
            code_memory_charge_ns,
        }
    }
}

/// Optimizing-tier candidacy for one bytecode function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptimizingDecision {
    /// No feedback or insufficient stable execution evidence.
    Cold,
    /// The first observation establishes the current feedback epoch.
    Warming,
    /// Material feedback changed and restarted the execution observation.
    FeedbackUnstable,
    /// The measured payoff exceeds compilation and code-memory cost.
    Promote,
}

#[derive(Debug, Default)]
struct FunctionTierState {
    last_feedback_epoch: Option<u32>,
    stable_since_execution: u64,
    observed_feedback_change: bool,
}

impl FunctionTierState {
    fn stable_executions(&mut self, executions: u64, epoch: u32) -> u64 {
        match self.last_feedback_epoch {
            None => {
                self.last_feedback_epoch = Some(epoch);
                self.stable_since_execution = executions;
            }
            Some(previous) if previous == epoch => {}
            Some(_) => {
                self.last_feedback_epoch = Some(epoch);
                self.stable_since_execution = executions;
                self.observed_feedback_change = true;
            }
        }
        executions.saturating_sub(self.stable_since_execution)
    }
}

/// Isolate-local feedback-change history and measured compile-cost ledger.
#[derive(Debug, Default)]
pub(crate) struct TierPolicy {
    functions: FxHashMap<u32, FunctionTierState>,
    cumulative_compile_ns: FxHashMap<(u32, CostedTier), u64>,
}

impl TierPolicy {
    /// Establish the feedback/execution baseline owned by the first installed
    /// Template generation. Generated entries can then measure stability from
    /// that real snapshot without making an unrelated first policy sample
    /// retroactively treat all bootstrap executions as stable.
    pub(crate) fn observe_template_generation(
        &mut self,
        function_id: u32,
        executions: u64,
        feedback_epoch: Option<u32>,
    ) {
        let Some(epoch) = feedback_epoch else {
            return;
        };
        let state = self.functions.entry(function_id).or_default();
        if state.last_feedback_epoch.is_none() {
            state.last_feedback_epoch = Some(epoch);
            state.stable_since_execution = executions;
        }
    }

    pub(crate) fn evict_function_range(&mut self, start: u32, end: u32) {
        self.functions
            .retain(|function_id, _| !(*function_id >= start && *function_id < end));
        self.cumulative_compile_ns
            .retain(|(function_id, _), _| !(*function_id >= start && *function_id < end));
    }

    pub(crate) fn record_compile_duration(
        &mut self,
        function_id: u32,
        tier: CostedTier,
        duration_ns: u64,
    ) {
        let total = self
            .cumulative_compile_ns
            .entry((function_id, tier))
            .or_insert(0);
        *total = total.saturating_add(duration_ns);
    }

    pub(crate) fn cumulative_compile_ns(&self, function_id: u32, tier: CostedTier) -> u64 {
        self.cumulative_compile_ns
            .get(&(function_id, tier))
            .copied()
            .unwrap_or(0)
    }

    fn sample_and_decide(
        &mut self,
        function_id: u32,
        executions: u64,
        feedback_epoch: Option<u32>,
        mut input: TierCostInput,
    ) -> OptimizingDecision {
        let Some(epoch) = feedback_epoch else {
            return OptimizingDecision::Cold;
        };
        let state = self.functions.entry(function_id).or_default();
        let stable_executions = state.stable_executions(executions, epoch);
        input.executions = stable_executions;
        input.cumulative_compile_ns = self
            .cumulative_compile_ns
            .get(&(function_id, input.tier))
            .copied()
            .unwrap_or(0);
        if TierCostModel::calibrated().decide(input).should_compile() {
            OptimizingDecision::Promote
        } else if state.observed_feedback_change {
            OptimizingDecision::FeedbackUnstable
        } else if stable_executions == 0 {
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
        function_id: u32,
        bytecode_instructions: u64,
        register_count: u64,
        parameter_count: u64,
        resident_code_bytes: u64,
    ) -> OptimizingDecision {
        let generated_entries = self
            .jit_code_registry
            .generated_entries_for_function(function_id);
        let executions = u64::from(self.jit_call_counts.get(&function_id).copied().unwrap_or(0))
            .saturating_add(generated_entries);
        let trigger = if generated_entries == 0 {
            TierTrigger::FunctionEntry
        } else {
            TierTrigger::DirectCallTarget
        };
        let feedback_epoch = self.code_space.feedback_epoch(function_id);
        self.optimizing_tier_policy.sample_and_decide(
            function_id,
            executions,
            feedback_epoch,
            TierCostInput {
                tier: CostedTier::Optimizing,
                trigger,
                executions: 0,
                exits: 0,
                bytecode_instructions,
                register_count,
                parameter_count,
                resident_code_bytes,
                cumulative_compile_ns: 0,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_model_promotion_is_monotonic_in_expected_execution() {
        let model = TierCostModel::calibrated();
        let mut promoted = false;

        for executions in 0..=100_000 {
            let decision = model.decide(TierCostInput {
                tier: CostedTier::Optimizing,
                trigger: TierTrigger::FunctionEntry,
                executions,
                exits: 0,
                bytecode_instructions: 96,
                register_count: 12,
                parameter_count: 3,
                resident_code_bytes: 0,
                cumulative_compile_ns: 0,
            });
            if decision.should_compile() {
                promoted = true;
            }
            assert!(
                !promoted || decision.should_compile(),
                "more expected executions must not reverse a profitable decision"
            );
        }

        assert!(promoted, "a sufficiently hot bounded function must promote");
    }

    #[test]
    fn cost_model_rejects_code_memory_budget_before_profitability() {
        let model = TierCostModel::calibrated();
        let decision = model.decide(TierCostInput {
            tier: CostedTier::Template,
            trigger: TierTrigger::LoopBackedge {
                span_instructions: 24,
            },
            executions: u64::MAX,
            exits: 0,
            bytecode_instructions: 64,
            register_count: 8,
            parameter_count: 2,
            resident_code_bytes: JIT_CODE_RESOURCE_LIMIT_BYTES,
            cumulative_compile_ns: 0,
        });

        assert_eq!(decision.reason(), TierCostReason::CodeMemoryBudget);
        assert!(!decision.should_compile());
    }

    #[test]
    fn feedback_change_resets_execution_evidence_without_sample_window() {
        let mut policy = TierPolicy::default();
        let base = TierCostInput {
            tier: CostedTier::Optimizing,
            trigger: TierTrigger::FunctionEntry,
            executions: 0,
            exits: 0,
            bytecode_instructions: 24,
            register_count: 1,
            parameter_count: 0,
            resident_code_bytes: 0,
            cumulative_compile_ns: 0,
        };
        assert_eq!(
            policy.sample_and_decide(7, 10_000, Some(1), base),
            OptimizingDecision::Warming
        );
        assert_eq!(
            policy.sample_and_decide(7, 20_000, Some(1), base),
            OptimizingDecision::Promote
        );
        assert_eq!(
            policy.sample_and_decide(7, 20_001, Some(2), base),
            OptimizingDecision::FeedbackUnstable
        );
    }

    #[test]
    fn template_generation_seeds_generated_entry_stability() {
        let mut policy = TierPolicy::default();
        let input = TierCostInput {
            tier: CostedTier::Optimizing,
            trigger: TierTrigger::DirectCallTarget,
            executions: 0,
            exits: 0,
            bytecode_instructions: 17,
            register_count: 14,
            parameter_count: 1,
            resident_code_bytes: 0,
            cumulative_compile_ns: 0,
        };
        let break_even = TierCostModel::calibrated().minimum_profitable_executions(input);
        policy.observe_template_generation(8, 34, Some(1));

        assert_eq!(
            policy.sample_and_decide(8, 34 + break_even, Some(1), input),
            OptimizingDecision::Promote
        );
    }
}
