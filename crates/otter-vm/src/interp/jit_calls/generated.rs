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
//! - An inline caller is checked against its owning generation's exact safepoint
//!   and the physical parent immediately preceding the bailed callee.
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
        // SAFETY: generated linkage retains the caller root record until after
        // this non-reentrant check. The callee has already left its Machine body.
        let Some(roots) = (unsafe {
            (self.jit_machine_roots as *const crate::jit::JitMachineRootRecord).as_ref()
        }) else {
            return false;
        };
        if roots.code_object_id != code_object_id {
            return false;
        }
        let Some(record) = self
            .jit_code_registry
            .safepoint_record(code_object_id, roots.safepoint_id)
        else {
            return false;
        };
        if !record.inline_frames_published || record.id != roots.safepoint_id {
            return false;
        }
        let Some(source) = record.inline_frames.last() else {
            return false;
        };
        if source.function_id != function_id || self.jit_native_activation_top < 2 {
            return false;
        }
        let active = &self.jit_native_activations[..self.jit_native_activation_top];
        if !std::ptr::eq(active[active.len() - 1].frame, callee) {
            return false;
        }
        let Some(base) = active.len().checked_sub(record.inline_frames.len() + 2) else {
            return false;
        };
        // SAFETY: the published activation array owns live canonical frame pointers.
        let Some(root) = (unsafe { active[base].frame.as_ref() }) else {
            return false;
        };
        if root.header.function_id != owner {
            return false;
        }
        for (index, source) in record.inline_frames.iter().enumerate() {
            // SAFETY: all published parents remain live throughout the cold check.
            let Some(parent) = (unsafe { active[base + 1 + index].frame.as_ref() }) else {
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

        // An optimizing generation that exits is a round trip on every entry
        // that reaches it, wherever that entry came from. Generated linkage is
        // an entry like any other, so it charges the same bounded
        // reoptimization budget the interpreter's entry paths do.
        if state.tier == NativeFrameKind::Optimizing {
            self.note_jit_optimized_bail(callee_function_id, callee_resume_pc);
            return Ok(());
        }

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
