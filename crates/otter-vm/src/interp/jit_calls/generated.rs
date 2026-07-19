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
//! - Callee identity, tier, and generation-health counters come from the exact
//!   retained [`crate::native_abi::CodeEntryCell`] generation.
//! - The published stack-owned frame must agree with that generation before
//!   diagnostics or policy state changes.
//! - Aggregate deopt pressure is applied once per still-linked generation;
//!   later deopts from already-active callers cannot consume more recompile
//!   budget after invalidation unlinks the cell.
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

        let Some(state) = self
            .jit_code_registry
            .generated_deopt_state(callee_code_object_id)
        else {
            return Err(VmError::InvalidOperand);
        };
        if state.function_id != callee.header.function_id
            || state.tier != callee.header.kind
            || state.consecutive_deopts == 0
        {
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
            consecutive_deopts: state.consecutive_deopts,
        });

        // Baseline generations participate in the bounded recompile/pin
        // policy. A purely consecutive threshold misses workloads where a
        // generated body succeeds just often enough to reset its streak while
        // still deopting on a large fraction of entries. Aggregate pressure
        // catches that case after a meaningful sample. Only a linked
        // generation may consume policy budget: invalidation unlinks it, while
        // already-active callers can still report later deopts during unwind.
        if state.tier == NativeFrameKind::Baseline && state.linked {
            if generated_call_generation_is_unhealthy(state.entries, state.deopts) {
                self.reopt_or_pin_jit_function(callee_function_id);
            } else {
                self.jit_entry_bail_counts.insert(
                    callee_function_id,
                    state.consecutive_deopts.saturating_sub(1),
                );
                self.note_jit_entry_bail(callee_function_id);
            }
        }
        Ok(())
    }
}

#[inline]
fn generated_call_generation_is_unhealthy(entries: u64, deopts: u64) -> bool {
    entries >= Interpreter::JIT_GENERATED_DEOPT_MIN_ENTRIES
        && deopts.saturating_mul(Interpreter::JIT_GENERATED_DEOPT_RATE_DENOMINATOR) >= entries
}

#[cfg(test)]
mod tests {
    use super::generated_call_generation_is_unhealthy;

    #[test]
    fn aggregate_deopt_pressure_requires_sample_size_and_high_rate() {
        assert!(!generated_call_generation_is_unhealthy(63, 63));
        assert!(!generated_call_generation_is_unhealthy(64, 15));
        assert!(generated_call_generation_is_unhealthy(64, 16));
        assert!(generated_call_generation_is_unhealthy(10_000, 2_500));
        assert!(!generated_call_generation_is_unhealthy(1_000_000, 64));
    }
}
