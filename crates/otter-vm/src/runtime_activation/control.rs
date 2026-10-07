//! Typed control, exception, and cold-deopt operations.
//!
//! # Contents
//! - [`BackedgePollOutcome`] — what a compiled back-edge does after its poll.
//! - [`RuntimeCall::backedge_poll`] — the cooperative loop checkpoint.
//!
//! # Invariants
//! - Identity branching stays in the VM boundary rather than leaking
//!   interpreter stack indices to the JIT.
//! - Baseline frames may leave for optimizing OSR. Every native frame leaves
//!   at the next bounded poll if its exact generation was invalidated.
//! - Baseline windows and optimizing canonical homes are rooted at the poll;
//!   optimizing code publishes its exact safepoint before entering this call.
//!   Invalidated active generations retain those root and deopt records.
//!
//! # See also
//! - [`crate::native_abi::Frame`] owns the canonical register window.

use crate::VmError;
use crate::native_abi::NativeFrameKind;
use crate::work_budget::WorkBudgetCheckpoint;

use super::RuntimeCall;

/// What a compiled back-edge does after its cooperative poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackedgePollOutcome {
    /// Continue the loop in native code.
    Continue,
    /// The work budget rotated its slice; the loop still continues.
    Yield,
    /// Resume at the loop header after invalidation or to enter optimizing OSR.
    ResumeInterpreter,
}

impl RuntimeCall<'_> {
    /// Poll interrupts and the work budget at one compiled back-edge.
    pub fn backedge_poll(&mut self) -> Result<BackedgePollOutcome, VmError> {
        // SAFETY: the branded call owns exclusive mutator access for this
        // short operation and retains only a raw descriptor afterwards.
        let context = &self.context;
        let vm = unsafe { &mut *self.vm.as_ptr() };
        // SAFETY: the polling frame stays published for this call.
        let (header, code_object_id) = {
            let frame = unsafe { self.frame.as_ref() };
            (frame.header, frame.code_object_id)
        };
        let batch = vm.jit_backedge_fuel_window;
        if header.kind == NativeFrameKind::Baseline
            && let Ok(owner) = context.for_function(header.function_id)
            && let Some(function) = owner.exec_function(header.function_id)
        {
            // Template loops charge source work here, once per fuel batch:
            // the batch's back edges each completed one pass of this loop,
            // the way an interrupt budget charges a back edge's distance.
            let span = function
                .loop_latch(header.pc)
                .map_or(1, |latch| u64::from(latch.saturating_sub(header.pc)) + 1);
            function.source_work().charge(batch.saturating_mul(span));
        }
        let checkpoint = vm.jit_backedge_poll()?;
        if code_object_id != 0
            && !vm
                .jit_code_registry
                .is_current_generation(u64::from(code_object_id))
        {
            return Ok(BackedgePollOutcome::ResumeInterpreter);
        }
        if header.kind == NativeFrameKind::Baseline
            && vm.baseline_backedges_reach_osr(context, header.function_id, header.pc, batch)
        {
            return Ok(BackedgePollOutcome::ResumeInterpreter);
        }
        Ok(if checkpoint == WorkBudgetCheckpoint::Yield {
            BackedgePollOutcome::Yield
        } else {
            BackedgePollOutcome::Continue
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{marker::PhantomData, ptr::NonNull, sync::Arc};

    use super::*;
    use crate::native_abi::{CodeObjectMetadata, Frame, VmFrameHeader};
    use crate::{ActivationStack, ExecutionContext, Interpreter, jit::JitFunctionCode};

    #[derive(Debug)]
    struct PollCode(NativeFrameKind);

    impl JitFunctionCode for PollCode {
        fn metadata(&self) -> CodeObjectMetadata {
            CodeObjectMetadata {
                id: 41,
                code_block_id: 0,
                entry_offset: 0,
                code_size: 4,
                safepoint_count: 0,
                frame_map_count: 0,
                spill_map_count: 0,
                dependency_count: 0,
            }
        }
        fn native_frame_kind(&self) -> NativeFrameKind {
            self.0
        }
        fn code_len(&self) -> usize {
            4
        }
        fn entry_addr(&self) -> Option<usize> {
            Some(0x1000)
        }
    }

    #[test]
    fn active_invalidated_generation_leaves_at_poll_in_both_native_tiers() {
        for tier in [NativeFrameKind::Baseline, NativeFrameKind::Optimizing] {
            let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
            let context = ExecutionContext::from_module(
                crate::test_support::minimal_bytecode_module("invalidated-poll.js"),
                crate::source_registry::SourceRegistry::default(),
            )
            .unwrap();
            let mut stack = ActivationStack::new();
            assert!(
                vm.jit_code_registry
                    .register(41, Arc::new(PollCode(tier)))
                    .is_ok()
            );
            let mut header = VmFrameHeader::interpreter(0, 0);
            header.kind = tier;
            let mut frame = Frame::new(
                header,
                0,
                crate::Value::undefined(),
                crate::Value::undefined(),
            );
            frame.code_object_id = 41;
            let poll = |vm: &mut Interpreter, stack: &mut ActivationStack, frame: &mut Frame| {
                let mut runtime = RuntimeCall {
                    vm: NonNull::from(vm),
                    stack: NonNull::from(stack),
                    context: crate::code_space::ResolvedCtx::Ambient(&context),
                    frame: NonNull::from(frame),
                    _exclusive: PhantomData,
                };
                runtime.backedge_poll().unwrap()
            };
            assert_eq!(
                poll(&mut vm, &mut stack, &mut frame),
                BackedgePollOutcome::Continue
            );
            assert_eq!(vm.jit_code_registry.invalidate_function(0), [0]);
            assert_eq!(
                poll(&mut vm, &mut stack, &mut frame),
                BackedgePollOutcome::ResumeInterpreter
            );
            assert!(
                !vm.interrupt.is_set(),
                "invalidation does not arm a permanent interrupt"
            );
            assert_eq!(
                frame.header.pc, 0,
                "resume remains the committed loop header"
            );
        }
    }
}
