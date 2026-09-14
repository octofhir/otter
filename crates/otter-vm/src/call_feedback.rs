//! Call execution and target recording into CodeBlock-owned feedback.
//!
//! # Contents
//! - [`Interpreter::record_call_attempt_feedback`] — monotonic pre-effect
//!   execution feedback shared by plain and method calls.
//! - [`Interpreter::record_ordinary_call_feedback`] — typed recording keyed by
//!   the canonical instruction index in the supplied CodeBlock.
//! - [`Interpreter::record_resolved_bytecode_call_feedback`] — generated-call
//!   target publication and caller invalidation before committed callee entry.
//! - [`Interpreter::commit_method_call_feedback_transition`] — publication and
//!   invalidation for isolate-owned method-target growth.
//!
//! # Invariants
//! - `call_attempted` is recorded before callable lookup, method lookup, getter
//!   invocation, or callee entry. A throwing call is therefore never confused
//!   with a never-taken branch.
//! - The typed `Op::Call` / `Op::New` payload owns the bounded target
//!   population; no interpreter-side `(function_id, pc)` map mirrors it.
//! - Bytecode and supported static-native identities share one coherent target
//!   population and one transition epoch.
//! - The first attempt and first resolved target are independent material
//!   facts. Each advances the feedback epoch once; repeated attempts and hits
//!   do not.
//!
//! # See also
//! - [`crate::feedback`] — compact per-instruction feedback and epochs.
//! - [`crate::feedback::CallSiteDistribution`] — bounded typed payload.

use crate::{
    CodeBlock, Interpreter,
    feedback::{CallTargetTransition, OrdinaryCallTarget},
};

impl Interpreter {
    /// Record one plain/method call attempt before any observable operation.
    ///
    /// A first attempt invalidates an installed caller whose immutable body may
    /// have represented this instruction as an unconditional cold exit.
    pub(crate) fn record_call_attempt_feedback(
        &mut self,
        code_block: &CodeBlock,
        instruction_pc: u32,
        caller_function_id: u32,
    ) -> bool {
        let changed = code_block
            .feedback_recorder_at(instruction_pc as usize)
            .is_some_and(|feedback| feedback.record_call_attempted());
        if changed {
            self.evict_compiled_for_reopt(caller_function_id);
        }
        changed
    }

    /// Publish one isolate-owned method-target population transition.
    ///
    /// Repeat hits are deliberately ignored. A new shape/target changes the
    /// immutable optimizing chain, so it advances the caller's shared feedback
    /// epoch and invalidates that caller for immediate replanning.
    pub(crate) fn commit_method_call_feedback_transition(
        &mut self,
        code_block: &CodeBlock,
        caller_function_id: u32,
        changed: bool,
    ) -> bool {
        if changed {
            code_block.bump_feedback_epoch();
            self.evict_compiled_for_reopt(caller_function_id);
        }
        changed
    }

    /// Publish an already-resolved bytecode callable at a generated call site.
    /// This leaf observation never allocates in the GC heap or invokes user
    /// code. Non-bytecode callables retain their canonical dispatch behavior.
    pub(crate) fn record_resolved_bytecode_call_feedback(
        &mut self,
        code_block: &CodeBlock,
        instruction_pc: u32,
        caller_function_id: u32,
        callee: crate::Value,
    ) {
        let Some(target_function_id) = callee.as_function().or_else(|| {
            callee
                .as_closure(&self.gc_heap)
                .map(|closure| closure.function_id())
        }) else {
            return;
        };
        let transition = self.record_ordinary_call_feedback(
            code_block,
            instruction_pc,
            OrdinaryCallTarget::Bytecode(target_function_id),
        );
        if transition.evict_for_reopt() {
            self.evict_compiled_for_reopt(caller_function_id);
        }
    }

    /// Record both compact and bounded ordinary-call feedback for one site.
    ///
    /// Dense and side-table transitions that describe the same first or second
    /// resolved target share one epoch bump. This population is independent of
    /// the pre-effect attempt bit: first attempt and first target may therefore
    /// advance the epoch separately. Later distinct targets and saturation each
    /// add one target-population transition.
    pub(crate) fn record_ordinary_call_feedback(
        &mut self,
        code_block: &CodeBlock,
        instruction_pc: u32,
        target: OrdinaryCallTarget,
    ) -> CallTargetTransition {
        code_block.record_call_target_feedback(instruction_pc as usize, target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feedback::{CallSiteDistribution, CallTargetCount};
    use crate::tier_policy::PROFILED_CALL_TARGET_CAPACITY;

    fn call_code_block() -> std::sync::Arc<CodeBlock> {
        CodeBlock::jit_test_stub(
            42,
            0,
            0,
            &[crate::jit::JitTestInstruction::new(
                otter_bytecode::Op::Call,
                0,
                0,
                vec![
                    otter_bytecode::Operand::Register(0),
                    otter_bytecode::Operand::Register(0),
                    otter_bytecode::Operand::ConstIndex(0),
                ],
            )],
        )
    }

    #[test]
    fn caller_invalidation_follows_bounded_target_growth() {
        let code_block = call_code_block();
        for target in 0..=PROFILED_CALL_TARGET_CAPACITY as u32 {
            assert!(code_block.record_call_feedback(0, target).evict_for_reopt());
            assert!(!code_block.record_call_feedback(0, target).evict_for_reopt());
        }
        assert!(
            !code_block
                .record_call_feedback(0, u32::MAX)
                .evict_for_reopt()
        );
    }

    #[test]
    fn distribution_counts_targets_to_bound_then_saturates() {
        let code_block = call_code_block();

        assert_eq!(
            code_block.record_call_feedback(0, 10),
            CallTargetTransition::BecameMonomorphic
        );
        assert_eq!(
            code_block.record_call_feedback(0, 10),
            CallTargetTransition::Unchanged
        );
        assert_eq!(
            code_block.record_call_feedback(0, 11),
            CallTargetTransition::BecamePolymorphic
        );
        let Some(CallSiteDistribution::Poly(targets)) = code_block.call_distribution_at(0) else {
            panic!("second distinct target must make the site polymorphic");
        };
        assert_eq!(
            targets.as_slice()[0],
            CallTargetCount {
                target: OrdinaryCallTarget::Bytecode(10),
                hits: 2,
            }
        );
        assert_eq!(
            targets.as_slice()[1],
            CallTargetCount {
                target: OrdinaryCallTarget::Bytecode(11),
                hits: 1,
            }
        );

        for fid in 12..(10 + PROFILED_CALL_TARGET_CAPACITY as u32) {
            assert_eq!(
                code_block.record_call_feedback(0, fid),
                CallTargetTransition::BecamePolymorphic
            );
        }
        let Some(CallSiteDistribution::Poly(targets)) = code_block.call_distribution_at(0) else {
            panic!("the bounded target set must remain polymorphic at its cap");
        };
        assert_eq!(targets.len(), PROFILED_CALL_TARGET_CAPACITY);
        assert_eq!(
            code_block.record_call_feedback(0, 17),
            CallTargetTransition::Unchanged
        );
        let Some(CallSiteDistribution::Poly(targets)) = code_block.call_distribution_at(0) else {
            panic!("a repeated target at the cap must remain polymorphic");
        };
        assert_eq!(targets.last().map(|target| target.hits), Some(2));

        assert_eq!(
            code_block.record_call_feedback(0, 10 + PROFILED_CALL_TARGET_CAPACITY as u32),
            CallTargetTransition::BecamePolymorphic
        );
        assert_eq!(
            code_block.call_distribution_at(0),
            Some(CallSiteDistribution::Megamorphic)
        );
        assert_eq!(
            code_block.record_call_feedback(0, u32::MAX),
            CallTargetTransition::Unchanged
        );
    }

    #[test]
    fn ordinary_call_epoch_tracks_each_distinct_target_and_saturation_once() {
        let code_block = call_code_block();
        let mut interpreter = Interpreter::new();

        for fid in 0..PROFILED_CALL_TARGET_CAPACITY as u32 {
            let transition = interpreter.record_ordinary_call_feedback(
                &code_block,
                0,
                OrdinaryCallTarget::Bytecode(fid),
            );
            let expected = match fid {
                0 => CallTargetTransition::BecameMonomorphic,
                _ => CallTargetTransition::BecamePolymorphic,
            };
            assert_eq!(transition, expected);
            assert_eq!(code_block.feedback_epoch(), fid + 1);
        }

        assert_eq!(
            interpreter.record_ordinary_call_feedback(
                &code_block,
                0,
                OrdinaryCallTarget::Bytecode(0),
            ),
            CallTargetTransition::Unchanged
        );
        assert_eq!(
            code_block.feedback_epoch(),
            PROFILED_CALL_TARGET_CAPACITY as u32
        );

        assert_eq!(
            interpreter.record_ordinary_call_feedback(
                &code_block,
                0,
                OrdinaryCallTarget::Bytecode(PROFILED_CALL_TARGET_CAPACITY as u32),
            ),
            CallTargetTransition::BecamePolymorphic
        );
        assert_eq!(
            code_block.feedback_epoch(),
            PROFILED_CALL_TARGET_CAPACITY as u32 + 1
        );
        assert!(matches!(
            code_block.call_distribution_at(0),
            Some(CallSiteDistribution::Megamorphic)
        ));

        interpreter.record_ordinary_call_feedback(
            &code_block,
            0,
            OrdinaryCallTarget::Bytecode(u32::MAX),
        );
        assert_eq!(
            code_block.feedback_epoch(),
            PROFILED_CALL_TARGET_CAPACITY as u32 + 1
        );
    }

    #[test]
    fn first_attempt_is_frozen_and_repeat_attempt_is_inert() {
        let code_block = call_code_block();
        let mut interpreter = Interpreter::new();

        assert!(!code_block.jit_compile_snapshot().instructions[0].call_attempted);
        assert!(interpreter.record_call_attempt_feedback(&code_block, 0, 42));
        assert_eq!(code_block.feedback_epoch(), 1);
        assert!(code_block.jit_compile_snapshot().instructions[0].call_attempted);

        assert_eq!(
            interpreter.record_ordinary_call_feedback(
                &code_block,
                0,
                OrdinaryCallTarget::Bytecode(7),
            ),
            CallTargetTransition::BecameMonomorphic
        );
        assert_eq!(code_block.feedback_epoch(), 2);

        assert!(!interpreter.record_call_attempt_feedback(&code_block, 0, 42));
        assert_eq!(
            interpreter.record_ordinary_call_feedback(
                &code_block,
                0,
                OrdinaryCallTarget::Bytecode(7),
            ),
            CallTargetTransition::Unchanged
        );
        assert_eq!(code_block.feedback_epoch(), 2);
    }

    #[test]
    fn method_population_transition_advances_epoch_only_when_material() {
        let code_block = call_code_block();
        let mut interpreter = Interpreter::new();

        assert!(interpreter.commit_method_call_feedback_transition(&code_block, 42, true));
        assert_eq!(code_block.feedback_epoch(), 1);
        assert!(!interpreter.commit_method_call_feedback_transition(&code_block, 42, false));
        assert_eq!(code_block.feedback_epoch(), 1);
    }
}
