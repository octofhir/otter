//! Cold policy and diagnostics for compiler-generated direct calls.
//!
//! # Contents
//! - [`Interpreter::note_generated_call_deopt`] — validates one exact generated
//!   code generation, emits its structured cold-deopt event, and feeds the
//!   existing baseline entry-bail eviction policy.
//!
//! # Invariants
//! - This module runs only after generated code reports a callee bailout.
//!   Successful generated calls never transition through the VM.
//! - Callee identity, tier, and consecutive-bail streak come from the exact
//!   retained [`crate::native_abi::CodeEntryCell`] generation.
//! - The published stack-owned frame must agree with that generation before
//!   diagnostics or policy state changes.
//! - Event construction remains lazy and allocation-free while JIT event
//!   capture is disabled.
//!
//! # See also
//! - `deopt` — materializes and resumes the already-started callee.
//! - [`crate::jit_registry`] — owns retained exact-generation entry cells.

use crate::{
    Interpreter, NativeFrame, NativeFrameFlags, NativeFrameKind, VmError,
    jit::JitDirectCallKind,
    jit_debug::{JitDebugEvent, JitDebugTier},
};

impl Interpreter {
    /// Record one exact generated-call deopt and apply baseline bail policy.
    pub(super) fn note_generated_call_deopt(
        &mut self,
        caller_function_id: u32,
        caller_call_pc: u32,
        caller_code_object_id: u64,
        callee_code_object_id: u64,
        call_kind: JitDirectCallKind,
        callee: NativeFrame,
    ) -> Result<(), VmError> {
        if caller_code_object_id == 0
            || callee_code_object_id == 0
            || !callee
                .header
                .flags
                .contains(NativeFrameFlags::STACK_REGISTERS)
            || callee.header.flags.contains(NativeFrameFlags::MATERIALIZED)
        {
            return Err(VmError::InvalidOperand);
        }
        if self
            .jit_code_registry
            .generation_function_id(caller_code_object_id)
            != Some(caller_function_id)
        {
            return Err(VmError::InvalidOperand);
        }

        let Some((registered_function_id, registered_tier, consecutive_deopts)) = self
            .jit_code_registry
            .generated_bail_streak(callee_code_object_id)
        else {
            return Err(VmError::InvalidOperand);
        };
        if registered_function_id != callee.header.function_id
            || registered_tier != callee.header.kind
            || consecutive_deopts == 0
        {
            return Err(VmError::InvalidOperand);
        }

        let tier = match registered_tier {
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
            consecutive_deopts,
        });

        // Baseline generations participate in the existing entry-bail
        // threshold/recompile/pin policy. Optimizing generations still report
        // their exact deopt streak above, but do not masquerade as baseline
        // reoptimization candidates. Successful generated completion clears
        // the cell streak without entering this path.
        if registered_tier == NativeFrameKind::Baseline {
            self.jit_entry_bail_counts
                .insert(callee_function_id, consecutive_deopts.saturating_sub(1));
            self.note_jit_entry_bail(callee_function_id);
        }
        Ok(())
    }
}
