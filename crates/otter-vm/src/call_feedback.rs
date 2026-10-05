//! Call execution and target recording into CodeBlock-owned feedback.
//!
//! # Contents
//! - [`Interpreter::record_call_attempt_feedback`] — monotonic pre-effect
//!   execution feedback shared by plain and method calls.
//! - [`Interpreter::record_ordinary_call_feedback`] — typed recording keyed by
//!   the canonical instruction index in the supplied CodeBlock.
//! - [`Interpreter::record_resolved_call_feedback`] — generated-call
//!   target publication before committed callee entry.
//! - [`Interpreter::commit_method_call_feedback_transition`] — epoch
//!   publication for isolate-owned method-target growth.
//!
//! # Invariants
//! - `call_attempted` is recorded before callable lookup, method lookup, getter
//!   invocation, or callee entry. A throwing call is therefore never confused
//!   with a never-taken branch.
//! - A method call records the independent property attempt before lookup,
//!   including receivers and getters whose result cannot enter the call cache.
//! - The typed `Op::Call` / `Op::New` payload owns the bounded target
//!   population; no interpreter-side `(function_id, pc)` map mirrors it.
//! - Bytecode, exact leaf and general native-kind observations share one coherent
//!   target population and one transition epoch. A native kind stores no handle.
//! - The first attempt and first resolved target are independent material
//!   facts. Each advances the feedback epoch once; repeated attempts and hits
//!   do not. No transition discards installed code: compiled bodies keep
//!   their own guards and later compilations read the new population.
//!
//! # See also
//! - [`crate::feedback`] — compact per-instruction feedback and epochs.
//! - [`crate::feedback::CallSiteDistribution`] — bounded typed payload.

use crate::{
    CodeBlock, Interpreter,
    feedback::{CallTargetTransition, OrdinaryCallTarget},
};

impl Interpreter {
    /// Record one plain/method call attempt before any observable operation,
    /// returning whether the site's feedback changed. Installed code is kept:
    /// a body that represented this instruction as a cold exit leaves through
    /// that exit, and the next compilation reads the new state.
    pub(crate) fn record_call_attempt_feedback(
        &mut self,
        code_block: &CodeBlock,
        instruction_pc: u32,
    ) -> bool {
        let call_changed = code_block
            .feedback_recorder_at(instruction_pc as usize)
            .is_some_and(|feedback| feedback.record_call_attempted());
        let property_changed = code_block
            .property_feedback_at(
                instruction_pc as usize,
                crate::property_ic::PropertyIcKind::Load,
            )
            .is_some_and(|slot| slot.record_attempt());
        let changed = call_changed || property_changed;
        changed
    }

    /// Publish one isolate-owned method-target population transition.
    ///
    /// Repeat hits are deliberately ignored. A new shape/target advances the
    /// caller's shared feedback epoch, so the next optimizing compilation
    /// replans the chain. Installed optimized code keeps its own guards until
    /// one fails, as V8 keeps optimized code until a check deoptimizes it;
    /// discarding it on every new target only churned code memory.
    pub(crate) fn commit_method_call_feedback_transition(
        &mut self,
        code_block: &CodeBlock,
        changed: bool,
    ) -> bool {
        if changed {
            code_block.bump_feedback_epoch();
        }
        changed
    }

    /// The function a call of `callee` with `receiver` as `this` runs when
    /// `callee` is `%Function.prototype.call%` and `receiver` a bytecode
    /// function.
    pub(crate) fn function_prototype_call_target(
        &self,
        callee: crate::Value,
        receiver: crate::Value,
    ) -> Option<OrdinaryCallTarget> {
        let is_call = callee.as_native_function().is_some_and(|native| {
            native.is_vm_intrinsic(
                &self.gc_heap,
                crate::native_function::VmIntrinsicFunction::FunctionPrototypeCall,
            )
        });
        if !is_call {
            return None;
        }
        receiver
            .as_function()
            .or_else(|| {
                receiver
                    .as_closure(&self.gc_heap)
                    .map(|closure| closure.function_id())
            })
            .map(OrdinaryCallTarget::FunctionPrototypeCall)
    }

    /// Stable feedback selected without allocation or user code.
    pub(crate) fn native_call_target(&self, callee: crate::Value) -> Option<OrdinaryCallTarget> {
        callee.as_native_function().map(|native| {
            crate::jit_static_native::jit_static_call_target(native, &self.gc_heap)
                .map(|entry| OrdinaryCallTarget::StaticNative(entry.leaf_stub_id))
                .unwrap_or(OrdinaryCallTarget::Native)
        })
    }

    /// Resolve the one current kind/identity population before a call can collect.
    pub(crate) fn resolved_call_target(
        &self,
        callee: crate::Value,
        receiver: crate::Value,
    ) -> Option<OrdinaryCallTarget> {
        self.function_prototype_call_target(callee, receiver)
            .or_else(|| callee.as_function().map(OrdinaryCallTarget::Bytecode))
            .or_else(|| {
                callee
                    .as_closure(&self.gc_heap)
                    .map(|closure| OrdinaryCallTarget::Bytecode(closure.function_id()))
            })
            .or_else(|| self.native_call_target(callee))
    }

    /// Publish an already-resolved callable at a generated call site.
    /// This leaf observation never allocates in the GC heap or invokes user
    /// code. Exact declared bootstrap leaves retain priority over general native kind,
    /// and `%Function.prototype.call%` records the function it runs.
    pub(crate) fn record_resolved_call_feedback(
        &mut self,
        code_block: &CodeBlock,
        instruction_pc: u32,
        callee: crate::Value,
        receiver: crate::Value,
    ) {
        let target = self.resolved_call_target(callee, receiver);
        let Some(target) = target else {
            return;
        };
        let _ = self.record_ordinary_call_feedback(code_block, instruction_pc, target);
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
            &[],
        )
    }

    #[test]
    fn caller_invalidation_follows_bounded_target_growth() {
        let code_block = call_code_block();
        for target in 0..=PROFILED_CALL_TARGET_CAPACITY as u32 {
            assert!(code_block.record_call_feedback(0, target).state_changed());
            assert!(!code_block.record_call_feedback(0, target).state_changed());
        }
        assert!(!code_block.record_call_feedback(0, u32::MAX).state_changed());
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
        let mut interpreter = Interpreter::new().expect("fixture interpreter bootstrap");

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
        let mut interpreter = Interpreter::new().expect("fixture interpreter bootstrap");

        assert!(!code_block.jit_compile_snapshot().instructions[0].call_attempted);
        assert!(interpreter.record_call_attempt_feedback(&code_block, 0));
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

        assert!(!interpreter.record_call_attempt_feedback(&code_block, 0));
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
        let mut interpreter = Interpreter::new().expect("fixture interpreter bootstrap");

        assert!(interpreter.commit_method_call_feedback_transition(&code_block, true));
        assert_eq!(code_block.feedback_epoch(), 1);
        assert!(!interpreter.commit_method_call_feedback_transition(&code_block, false));
        assert_eq!(code_block.feedback_epoch(), 1);
    }
    #[test]
    fn general_native_kind_is_one_target_across_live_static_and_dynamic_bodies() {
        fn static_body(
            _ctx: &mut crate::NativeCtx<'_>,
            _args: &[crate::Value],
        ) -> Result<crate::Value, crate::NativeError> {
            Ok(crate::Value::number_i32(1))
        }
        let mut interpreter = Interpreter::new().expect("fixture interpreter bootstrap");
        let code = call_code_block();
        let first = crate::NativeFunction::new_static(
            &mut interpreter.gc_heap,
            "kindStatic",
            0,
            static_body,
        )
        .expect("static native fixture");
        let first_target = interpreter.native_call_target(crate::Value::native_function(first));
        assert_eq!(first_target, Some(OrdinaryCallTarget::Native));
        // Only scalar feedback remains live while this second native allocates.
        let second = crate::NativeFunction::new(
            &mut interpreter.gc_heap,
            "kindDynamic",
            |_ctx, _args, _captures| Ok(crate::Value::number_i32(2)),
        )
        .expect("dynamic native fixture");
        let second_target = interpreter.native_call_target(crate::Value::native_function(second));
        assert_eq!(second_target, first_target);
        assert_eq!(
            interpreter.record_ordinary_call_feedback(&code, 0, first_target.unwrap()),
            CallTargetTransition::BecameMonomorphic
        );
        assert_eq!(
            interpreter.record_ordinary_call_feedback(&code, 0, second_target.unwrap()),
            CallTargetTransition::Unchanged
        );
        assert_eq!(code.feedback_epoch(), 1);
        assert_eq!(
            code.call_distribution_at(0),
            Some(CallSiteDistribution::Mono(CallTargetCount {
                target: OrdinaryCallTarget::Native,
                hits: 2,
            }))
        );
        assert_eq!(
            interpreter.record_ordinary_call_feedback(&code, 0, OrdinaryCallTarget::Bytecode(19)),
            CallTargetTransition::BecamePolymorphic
        );
        assert_eq!(code.feedback_epoch(), 2);
    }

    #[test]
    fn exact_declared_leaf_has_priority_over_general_native_kind() {
        let mut interpreter = Interpreter::new().expect("fixture interpreter bootstrap");
        let leaf = crate::NativeFunction::new_static(
            &mut interpreter.gc_heap,
            "exactAbs",
            1,
            crate::math::native_abs,
        )
        .expect("declared static native fixture");
        assert_eq!(
            interpreter.native_call_target(crate::Value::native_function(leaf)),
            Some(OrdinaryCallTarget::StaticNative(
                crate::native_abi::STUB_MATH_ABS_LEAF.id
            ))
        );
    }
}
