//! Ordinary-call recording into CodeBlock-owned typed feedback slots.
//!
//! # Contents
//! - [`Interpreter::record_ordinary_call_feedback`] — additive recording keyed
//!   by the canonical instruction index in the supplied CodeBlock.
//!
//! # Invariants
//! - The typed `Op::Call` payload owns the bounded target population; no
//!   interpreter-side `(function_id, pc)` map mirrors it.
//! - Existing baseline decisions continue to read the compact dense cell.
//!
//! # See also
//! - [`crate::feedback`] — compact per-instruction feedback and epochs.
//! - [`crate::feedback::CallSiteDistribution`] — bounded typed payload.

use crate::{CodeBlock, Interpreter, feedback::CallTargetTransition};

impl Interpreter {
    /// Record both compact and bounded ordinary-call feedback for one site.
    ///
    /// Dense and side-table transitions that describe the same first or second
    /// target share one epoch bump. Later distinct targets and saturation add
    /// their own single transition without feeding any current JIT decision.
    pub(crate) fn record_ordinary_call_feedback(
        &mut self,
        code_block: &CodeBlock,
        instruction_pc: u32,
        callee_fid: u32,
    ) -> CallTargetTransition {
        code_block.record_call_feedback(instruction_pc as usize, callee_fid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feedback::{CallSiteDistribution, CallTargetCount, MAX_CALL_TARGETS};

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
        assert_eq!(targets.as_slice()[0], CallTargetCount { fid: 10, hits: 2 });
        assert_eq!(targets.as_slice()[1], CallTargetCount { fid: 11, hits: 1 });

        for fid in 12..(10 + MAX_CALL_TARGETS as u32) {
            assert_eq!(
                code_block.record_call_feedback(0, fid),
                CallTargetTransition::Unchanged
            );
        }
        let Some(CallSiteDistribution::Poly(targets)) = code_block.call_distribution_at(0) else {
            panic!("the bounded target set must remain polymorphic at its cap");
        };
        assert_eq!(targets.len(), MAX_CALL_TARGETS);
        assert_eq!(
            code_block.record_call_feedback(0, 17),
            CallTargetTransition::Unchanged
        );
        let Some(CallSiteDistribution::Poly(targets)) = code_block.call_distribution_at(0) else {
            panic!("a repeated target at the cap must remain polymorphic");
        };
        assert_eq!(targets.last().map(|target| target.hits), Some(2));

        assert_eq!(
            code_block.record_call_feedback(0, 10 + MAX_CALL_TARGETS as u32),
            CallTargetTransition::Unchanged
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

        for fid in 0..MAX_CALL_TARGETS as u32 {
            let transition = interpreter.record_ordinary_call_feedback(&code_block, 0, fid);
            let expected = match fid {
                0 => CallTargetTransition::BecameMonomorphic,
                1 => CallTargetTransition::BecamePolymorphic,
                _ => CallTargetTransition::Unchanged,
            };
            assert_eq!(transition, expected);
            assert_eq!(code_block.feedback_epoch(), fid + 1);
        }

        assert_eq!(
            interpreter.record_ordinary_call_feedback(&code_block, 0, 0),
            CallTargetTransition::Unchanged
        );
        assert_eq!(code_block.feedback_epoch(), MAX_CALL_TARGETS as u32);

        assert_eq!(
            interpreter.record_ordinary_call_feedback(&code_block, 0, MAX_CALL_TARGETS as u32),
            CallTargetTransition::Unchanged
        );
        assert_eq!(code_block.feedback_epoch(), MAX_CALL_TARGETS as u32 + 1);
        assert!(matches!(
            code_block.call_distribution_at(0),
            Some(CallSiteDistribution::Megamorphic)
        ));

        interpreter.record_ordinary_call_feedback(&code_block, 0, u32::MAX);
        assert_eq!(code_block.feedback_epoch(), MAX_CALL_TARGETS as u32 + 1);
    }
}
