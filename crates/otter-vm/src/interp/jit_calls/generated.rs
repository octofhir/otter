//! Cold policy and diagnostics for side exits of entered generations.
//!
//! # Contents
//! - [`Interpreter::note_entered_generation_deopt`] — validates one exact
//!   generated code generation, emits its structured deopt event, and feeds
//!   the shared cost policy.
//!
//! # Invariants
//! - This module runs only after a generation entered through the call
//!   trampoline reports a side exit. Successful calls never transition
//!   through the VM.
//! - Callee identity and tier come from the exact retained
//!   [`crate::native_abi::CodeEntryCell`] generation, and the published frame
//!   must agree with it before diagnostics or policy state changes.
//! - The exit is charged to the generation that took it; the caller that
//!   entered it is not consulted.
//! - Exit cost is applied once per still-linked generation; later deopts from
//!   already-active frames cannot invalidate an already-unlinked cell.
//! - Event construction remains lazy and allocation-free while JIT event
//!   capture is disabled.
//!
//! # See also
//! - `deopt` — materializes and resumes the already-started callee.
//! - [`crate::jit_registry`] — owns retained exact-generation entry cells.

use crate::{
    ExecutionContext, Interpreter, VmError,
    jit_debug::{JitDebugEvent, JitDebugTier},
    native_abi::{Frame, NativeFrameKind},
};

impl Interpreter {
    /// Record one exact entered-generation deopt and apply its bail policy.
    pub(crate) fn note_entered_generation_deopt(
        &mut self,
        context: &ExecutionContext,
        callee: &Frame,
        exit: crate::native_abi::SideExit,
    ) -> Result<(), VmError> {
        let callee_code_object_id = u64::from(callee.code_object_id);
        if callee_code_object_id == 0 {
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
            NativeFrameKind::Interpreter | NativeFrameKind::Host => {
                return Err(VmError::InvalidOperand);
            }
        };
        let callee_function_id = callee.header.function_id;
        let callee_resume_pc = callee.header.pc;
        self.record_jit_debug_event(|| JitDebugEvent::EnteredGenerationDeopt {
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
            // An entry-guard exit leaves the actual parameters in the callee's
            // published window.
            let param_count = context
                .for_function(callee_function_id)
                .ok()
                .and_then(|owner| {
                    owner
                        .exec_function(callee_function_id)
                        .map(|f| f.param_count)
                })
                .map_or(0, usize::from)
                .min(usize::from(callee.header.register_count));
            let parameters = (0..param_count)
                .map(|index| {
                    // SAFETY: the stack-owned callee window is live and
                    // initialized for `register_count` tagged slots until the
                    // deoptimizer consumes it.
                    crate::Value::from_bits(unsafe {
                        std::ptr::read((callee.register_base() as *const u64).add(index))
                    })
                })
                .collect::<smallvec::SmallVec<[crate::Value; 8]>>();
            self.widen_exited_parameters(callee_function_id, exit, &parameters);
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
