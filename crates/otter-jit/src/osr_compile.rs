//! OSR compilation support — compiling functions with OSR entry points.
//!
//! When a hot loop is detected, we compile the function with a special
//! OSR entry block at the loop header. The interpreter transfers its
//! register window and jumps into the JIT code mid-loop.
//!
//! ## OSR Entry Block
//!
//! ```text
//! osr_entry_block:
//!   // Load all live locals from interpreter register window
//!   v0 = LoadLocal(0)
//!   v1 = LoadLocal(1)
//!   ...
//!   // Optional: type guards based on feedback
//!   v0_i32 = GuardInt32(v0, deopt_to_interp)
//!   v1_i32 = GuardInt32(v1, deopt_to_interp)
//!   ...
//!   // Jump to the loop header block
//!   Jump(loop_header_block, [v0_i32, v1_i32, ...])
//! ```
//!
//! Spec: Phase 3.3 of JIT_INCREMENTAL_PLAN.md

use crate::osr::{OsrEntryPoint, OsrState, OsrValueType};
use crate::pipeline::JitExecResult;
use crate::{JitError, code_memory::CompiledFunction};
use otter_vm::RegisterValue;

/// Options for OSR-aware compilation.
#[derive(Debug, Clone)]
pub struct OsrCompileOptions {
    /// Bytecode PC of the loop header to enter at.
    pub entry_pc: u32,
    /// Expected types for live locals (from feedback).
    pub expected_types: Vec<OsrValueType>,
    /// Number of live locals at the OSR entry point.
    pub live_local_count: u16,
}

/// Result of attempting OSR entry into compiled code.
#[derive(Debug)]
pub enum OsrEntryResult {
    /// Successfully entered JIT code at the loop header.
    Entered(JitExecResult),
    /// OSR entry not possible (guards failed, code not ready, etc.).
    NotEntered,
    /// Compilation or execution error.
    Error(JitError),
}

/// Attempt OSR entry: transfer interpreter state to JIT code at a loop header.
///
/// This is called from the interpreter when a back-edge triggers tier-up:
/// 1. Compile the function with OSR entry at `entry_pc` (if not cached)
/// 2. Transfer register window
/// 3. Execute JIT code from the loop header
/// 4. Return the result (or signal that OSR couldn't proceed)
///
/// The caller (interpreter) provides:
/// - `function`: the VM function
/// - `registers`: current interpreter register window (mutable for JIT to write back)
/// - `entry_pc`: the loop header PC
/// - `osr_state`: function's OSR tracking state
pub fn attempt_osr_entry(
    function: &otter_vm::Function,
    registers: &mut [RegisterValue],
    entry_pc: u32,
    _osr_state: &mut OsrState,
) -> OsrEntryResult {
    // Step 1: Compile with standard pipeline (OSR-aware compilation is future work).
    // For now, we compile the whole function and execute from the beginning.
    // True OSR entry (jumping mid-function) requires MIR builder changes.
    let compiled = match crate::pipeline::compile_function(function) {
        Ok(c) => c,
        Err(e) => return OsrEntryResult::Error(e),
    };

    // Step 2: Execute with standard entry.
    // TODO: True OSR would set up JitContext with entry_pc and jump to the
    // loop header block. For now, this is a full-function re-execution.
    let _ = entry_pc; // Will be used when OSR entry blocks are implemented.
    match execute_compiled(compiled, function, registers) {
        Ok(result) => OsrEntryResult::Entered(result),
        Err(e) => OsrEntryResult::Error(e),
    }
}

/// Execute a compiled function with the given register window.
fn execute_compiled(
    compiled: CompiledFunction,
    function: &otter_vm::Function,
    registers: &mut [RegisterValue],
) -> Result<JitExecResult, JitError> {
    let required_len = usize::from(function.frame_layout().register_count());
    if registers.len() < required_len {
        return Err(JitError::Internal(format!(
            "register slice too small: need {required_len}, got {}",
            registers.len()
        )));
    }

    let register_count = u32::try_from(required_len)
        .map_err(|_| JitError::Internal("register count overflow".to_string()))?;

    let mut ctx = crate::context::JitContext {
        registers_base: registers.as_mut_ptr().cast::<u64>(),
        local_count: register_count,
        register_count,
        constants: std::ptr::null(),
        this_raw: RegisterValue::undefined().raw_bits(),
        interrupt_flag: std::ptr::null(),
        interpreter: std::ptr::null(),
        vm_ctx: std::ptr::null_mut(),
        function_ptr: function as *const otter_vm::Function as *const (),
        upvalues_ptr: std::ptr::null(),
        upvalue_count: 0,
        callee_raw: RegisterValue::undefined().raw_bits(),
        home_object_raw: RegisterValue::undefined().raw_bits(),
        proto_epoch: 0,
        bailout_reason: 0,
        bailout_pc: 0,
        secondary_result: 0,
        module_ptr: std::ptr::null(),
        runtime_ptr: std::ptr::null_mut(),
        heap_slots_base: std::ptr::null(),
    };

    crate::telemetry::record_jit_entry();
    crate::telemetry::record_function_jit_entry(function.name().unwrap_or("<anonymous>"), 1);
    let result = unsafe { compiled.call(&mut ctx) };
    if result == crate::BAILOUT_SENTINEL {
        Ok(JitExecResult::Bailout {
            bytecode_pc: ctx.bailout_pc,
            reason: crate::BailoutReason::from_raw(ctx.bailout_reason)
                .unwrap_or(crate::BailoutReason::Unsupported),
        })
    } else {
        Ok(JitExecResult::Ok(result))
    }
}

/// Build OSR entry point metadata from feedback.
///
/// Examines the feedback vector to determine expected types at a loop header.
#[must_use]
pub fn build_osr_entry_point(
    header_pc: u32,
    live_local_count: u16,
    feedback: &otter_vm::feedback::FeedbackVector,
) -> OsrEntryPoint {
    use otter_vm::feedback::{ArithmeticFeedback, FeedbackSlotData, FeedbackSlotId};

    let mut expected_types = Vec::with_capacity(usize::from(live_local_count));

    // Use feedback to predict types. For each slot with arithmetic feedback,
    // if it's consistently Int32, expect Int32 at the OSR entry.
    for i in 0..live_local_count {
        let slot_id = FeedbackSlotId(i);
        let ty = match feedback.get(slot_id) {
            Some(FeedbackSlotData::Arithmetic(ArithmeticFeedback::Int32)) => OsrValueType::Int32,
            Some(FeedbackSlotData::Arithmetic(ArithmeticFeedback::Number)) => OsrValueType::Float64,
            _ => OsrValueType::Tagged,
        };
        expected_types.push(ty);
    }

    OsrEntryPoint {
        header_pc,
        expected_types,
        code_offset: 0, // Set during compilation.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::osr::OsrValueType;

    #[test]
    fn test_build_osr_entry_point_default() {
        let feedback = otter_vm::feedback::FeedbackVector::empty();
        let entry = build_osr_entry_point(10, 3, &feedback);
        assert_eq!(entry.header_pc, 10);
        assert_eq!(entry.expected_types.len(), 3);
        // All tagged since no feedback.
        assert!(
            entry
                .expected_types
                .iter()
                .all(|t| *t == OsrValueType::Tagged)
        );
    }

    #[test]
    fn test_build_osr_entry_point_with_feedback() {
        use otter_vm::feedback::*;

        let layout = FeedbackTableLayout::new(vec![
            FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Arithmetic),
            FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Arithmetic),
            FeedbackSlotLayout::new(FeedbackSlotId(2), FeedbackKind::Branch),
        ]);
        let mut fv = FeedbackVector::from_layout(&layout);
        fv.record_arithmetic(FeedbackSlotId(0), ArithmeticFeedback::Int32);
        fv.record_arithmetic(FeedbackSlotId(1), ArithmeticFeedback::Number);

        let entry = build_osr_entry_point(5, 3, &fv);
        assert_eq!(entry.expected_types[0], OsrValueType::Int32);
        assert_eq!(entry.expected_types[1], OsrValueType::Float64);
        assert_eq!(entry.expected_types[2], OsrValueType::Tagged);
    }
}
