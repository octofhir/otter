//! Explicit deopt handoff and interpreter-resume helpers for the new VM path.

use otter_vm::deopt::{DeoptHandoff, DeoptId, DeoptReason, DeoptSite};
use otter_vm::interpreter::{ExecutionResult, InterpreterError};
use otter_vm::{FunctionIndex, Interpreter, Module, RegisterValue};

use crate::JitError;
use crate::deopt::BailoutReason;
use crate::pipeline::{
    JitExecResult, execute_next_function_profiled_with_runtime,
    execute_next_function_with_interrupt,
};

/// Errors produced while deopting back into the new interpreter.
#[derive(Debug, thiserror::Error)]
pub enum NextDeoptError {
    #[error("{0}")]
    Jit(#[from] JitError),
    #[error("{0}")]
    Interpreter(#[from] InterpreterError),
    #[error("module entry function index is out of bounds")]
    InvalidFunctionIndex,
}

fn map_bailout_reason(reason: BailoutReason) -> DeoptReason {
    match reason {
        BailoutReason::TypeGuardFailed
        | BailoutReason::ShapeGuardFailed
        | BailoutReason::ProtoEpochMismatch
        | BailoutReason::BoundsCheckFailed
        | BailoutReason::ArrayNotDense
        | BailoutReason::CallTargetMismatch
        | BailoutReason::Overflow => DeoptReason::GuardFailure,
        BailoutReason::Interrupted | BailoutReason::TierUp => DeoptReason::Materialization,
        BailoutReason::Unsupported | BailoutReason::Exception | BailoutReason::Breakpoint => {
            DeoptReason::UnsupportedPath
        }
    }
}

/// Resolve a deopt handoff for one new-VM bailout.
#[must_use]
pub fn handoff_for_next_bailout(
    function: &otter_vm::Function,
    bytecode_pc: u32,
    reason: BailoutReason,
) -> DeoptHandoff {
    let site = function
        .deopt()
        .site_for_pc(bytecode_pc)
        .unwrap_or_else(|| DeoptSite::new(DeoptId(bytecode_pc), bytecode_pc));
    DeoptHandoff::new(site, bytecode_pc, map_bailout_reason(reason))
}

/// Resume a new-VM function in the interpreter from an explicit deopt handoff.
pub fn resume_next_function(
    module: &Module,
    function_index: FunctionIndex,
    handoff: DeoptHandoff,
    registers: &[RegisterValue],
) -> Result<ExecutionResult, NextDeoptError> {
    let _ = module
        .function(function_index)
        .ok_or(NextDeoptError::InvalidFunctionIndex)?;
    Ok(Interpreter::new().resume(module, function_index, handoff.resume_pc(), registers)?)
}

/// Execute a new-VM function in JIT code and explicitly fall back to the interpreter on deopt.
pub fn execute_next_function_with_fallback(
    module: &Module,
    function_index: FunctionIndex,
    registers: &mut [RegisterValue],
    interrupt_flag: *const u8,
) -> Result<ExecutionResult, NextDeoptError> {
    let function = module
        .function(function_index)
        .ok_or(NextDeoptError::InvalidFunctionIndex)?;
    match execute_next_function_with_interrupt(function, registers, interrupt_flag)? {
        JitExecResult::Ok(raw) => {
            let value = RegisterValue::from_raw_bits(raw).ok_or_else(|| {
                JitError::Internal("jit returned invalid new-vm register bits".to_string())
            })?;
            Ok(ExecutionResult::new(value))
        }
        JitExecResult::Bailout {
            bytecode_pc,
            reason,
        } => {
            let handoff = handoff_for_next_bailout(function, bytecode_pc, reason);
            resume_next_function(module, function_index, handoff, registers)
        }
        JitExecResult::NotCompiled => {
            Ok(Interpreter::new().resume(module, function_index, 0, registers)?)
        }
    }
}

/// Execute a profiled new-VM function on shared runtime state and fall back to the interpreter.
pub fn execute_next_function_profiled_with_fallback(
    module: &Module,
    function_index: FunctionIndex,
    registers: &mut [RegisterValue],
    interrupt_flag: *const u8,
) -> Result<ExecutionResult, NextDeoptError> {
    let profile = Interpreter::new().profile_property_caches(module, function_index, registers)?;
    let mut runtime = otter_vm::RuntimeState::new();
    match execute_next_function_profiled_with_runtime(
        module,
        function_index,
        registers,
        &mut runtime,
        &profile,
        interrupt_flag,
    )? {
        JitExecResult::Ok(raw) => {
            let value = RegisterValue::from_raw_bits(raw).ok_or_else(|| {
                JitError::Internal("jit returned invalid new-vm register bits".to_string())
            })?;
            Ok(ExecutionResult::new(value))
        }
        JitExecResult::Bailout { bytecode_pc, .. } => Ok(Interpreter::new().resume_with_runtime(
            module,
            function_index,
            bytecode_pc,
            registers,
            &mut runtime,
        )?),
        JitExecResult::NotCompiled => Ok(Interpreter::new().execute_with_runtime(
            module,
            function_index,
            registers,
            &mut runtime,
        )?),
    }
}
