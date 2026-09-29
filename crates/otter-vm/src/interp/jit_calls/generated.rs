//! Cold policy and diagnostics for compiler-generated direct calls.
//!
//! # Contents
//! - [`Interpreter::note_generated_call_deopt`] — validates one exact generated
//!   code generation, emits its structured cold-deopt event, and feeds the
//!   shared cost policy.
//!
//! # Invariants
//! - This module runs only after generated code reports a callee bailout.
//!   Successful generated calls never transition through the VM.
//! - Callee identity and tier come from the exact
//!   retained [`crate::native_abi::CodeEntryCell`] generation.
//! - The published stack-owned frame must agree with that generation before
//!   diagnostics or policy state changes.
//! - An inline caller is checked against its owning generation's exact safepoint
//!   and the physical parent immediately preceding the bailed callee.
//! - Exit cost is applied once per still-linked generation; later deopts from
//!   already-active callers cannot invalidate an already-unlinked cell.
//! - Event construction remains lazy and allocation-free while JIT event
//!   capture is disabled.
//!
//! # See also
//! - `deopt` — materializes and resumes the already-started callee.
//! - [`crate::jit_registry`] — owns retained exact-generation entry cells.

use crate::{
    ExecutionContext, Interpreter, VmError,
    jit::JitDirectCallKind,
    jit_debug::{JitDebugEvent, JitDebugTier},
    native_abi::{NativeFrame, NativeFrameFlags, NativeFrameKind},
};

impl Interpreter {
    /// A source caller may be an inline descendant of the active generation.
    /// In that case both its exact safepoint and published parent must agree.
    fn generated_caller_matches(
        &self,
        context: &ExecutionContext,
        function_id: u32,
        call_pc: u32,
        code_object_id: u64,
        callee: &NativeFrame,
    ) -> bool {
        let Some(owner) = self
            .jit_code_registry
            .generation_function_id(code_object_id)
        else {
            return false;
        };
        if owner == function_id {
            return true;
        }
        // The physical caller is the first frame above the callee that names
        // a call site; published inline parents between them name none.
        // SAFETY: generated linkage keeps the whole chain published until
        // after this non-reentrant check.
        let Some(call_site) = self
            .jit_native_frames()
            .skip(1)
            .map(|frame| unsafe { &*frame })
            .find(|frame| frame.call_site != crate::native_abi::NO_SAFEPOINT)
            .filter(|frame| u64::from(frame.code_object_id) == code_object_id)
            .map(|frame| frame.call_site)
        else {
            return false;
        };
        let Some(record) = self
            .jit_code_registry
            .safepoint_record(code_object_id, call_site)
        else {
            return false;
        };
        if !record.inline_frames_published || record.id != call_site {
            return false;
        }
        let Some(source) = record.inline_frames.last() else {
            return false;
        };
        if source.function_id != function_id {
            return false;
        }
        // The callee is innermost; its callers are the published inline
        // parents, callee-most first, then the physical root frame.
        let active: Vec<_> = self
            .jit_native_frames()
            .take(record.inline_frames.len() + 2)
            .collect();
        if active.len() != record.inline_frames.len() + 2 || !std::ptr::eq(active[0], callee) {
            return false;
        }
        // SAFETY: published frames stay live throughout the cold check.
        let Some(root) = (unsafe { active[active.len() - 1].as_ref() }) else {
            return false;
        };
        if root.header.function_id != owner {
            return false;
        }
        for (index, source) in record.inline_frames.iter().enumerate() {
            // SAFETY: all published parents remain live throughout the cold check.
            let Some(parent) = (unsafe { active[active.len() - 2 - index].as_ref() }) else {
                return false;
            };
            let Some(function) = context.exec_function(source.function_id) else {
                return false;
            };
            if parent.header.function_id != source.function_id
                || function.instruction_byte_pc(parent.header.pc as usize) != Some(source.byte_pc)
                || parent.header.register_count != function.register_count
                || usize::from(parent.header.register_count) != source.slots.len()
                || !parent
                    .header
                    .flags
                    .contains(NativeFrameFlags::STACK_REGISTERS)
                || (index + 1 == record.inline_frames.len() && parent.header.pc != call_pc)
            {
                return false;
            }
        }
        true
    }

    /// Record one exact generated-call deopt and apply baseline bail policy.
    pub(super) fn note_generated_call_deopt(
        &mut self,
        context: &ExecutionContext,
        caller_function_id: u32,
        caller_call_pc: u32,
        caller_code_object_id: u64,
        callee_code_object_id: u64,
        call_kind: JitDirectCallKind,
        callee: &NativeFrame,
        exit: crate::native_abi::SideExit,
    ) -> Result<(), VmError> {
        if caller_code_object_id == 0
            || callee_code_object_id == 0
            || !callee
                .header
                .flags
                .contains(NativeFrameFlags::STACK_REGISTERS)
        {
            return Err(VmError::InvalidOperand);
        }
        if !self.generated_caller_matches(
            context,
            caller_function_id,
            caller_call_pc,
            caller_code_object_id,
            callee,
        ) {
            return Err(VmError::InvalidOperand);
        }

        let Some(state) = self
            .jit_code_registry
            .generated_deopt_state(callee_code_object_id)
        else {
            return Err(VmError::InvalidOperand);
        };
        if state.function_id != callee.header.function_id || state.tier != callee.header.kind {
            return Err(VmError::InvalidOperand);
        }

        let tier = match state.tier {
            NativeFrameKind::Baseline => JitDebugTier::Template,
            NativeFrameKind::Optimizing => JitDebugTier::Optimizing,
            NativeFrameKind::Interpreter => return Err(VmError::InvalidOperand),
        };
        let callee_function_id = callee.header.function_id;
        let callee_resume_pc = callee.header.pc;
        self.record_jit_debug_event(|| JitDebugEvent::GeneratedCallDeopt {
            call_kind,
            caller_function_id,
            caller_call_pc,
            caller_code_object_id,
            callee_function_id,
            callee_code_object_id,
            callee_tier: tier,
            callee_resume_pc,
            exit_reason: exit.reason(),
            exit_action: exit.action(),
        });

        // An optimizing generation that exits is a round trip on every entry
        // that reaches it, wherever that entry came from. Generated linkage is
        // an entry like any other, so it charges the same bounded
        // reoptimization budget the interpreter's entry paths do.
        if state.tier == NativeFrameKind::Optimizing {
            // The shared optimizing-exit owner also applies the one-way
            // arithmetic widening, so a later generation cannot repeat an
            // overflow or negative-zero speculation.
            self.note_jit_optimized_bail(context, callee_function_id, exit);
            return Ok(());
        }

        self.note_receiver_allocation_exit(context, callee_function_id, callee_resume_pc, exit);
        // Only a linked generation may charge the shared cost policy:
        // invalidation unlinks it, while already-active callers can still
        // report later deopts during unwind.
        if state.tier == NativeFrameKind::Baseline && state.linked {
            self.note_jit_entry_bail(context, callee_function_id);
        }
        Ok(())
    }
}
