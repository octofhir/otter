//! Explicit deopt handoff and interpreter-resume helpers for the JIT path.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use otter_vm::RuntimeState;
use otter_vm::deopt::{DeoptHandoff, DeoptId, DeoptReason, DeoptSite};
use otter_vm::interpreter::{ExecutionResult, InterpreterError};
use otter_vm::{FunctionIndex, Interpreter, Module, RegisterValue};

use crate::JitError;
use crate::deopt::BailoutReason;
use crate::pipeline::{
    JitExecResult, execute_function_profiled_with_runtime, execute_function_with_interrupt,
};

/// Errors produced while deopting back into the interpreter.
#[derive(Debug, thiserror::Error)]
pub enum DeoptError {
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

/// Resolve a deopt handoff for one bailout.
#[must_use]
pub fn handoff_for_bailout(
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

/// Resume a function in the interpreter from an explicit deopt handoff.
pub fn resume_function(
    module: &Module,
    function_index: FunctionIndex,
    handoff: DeoptHandoff,
    registers: &[RegisterValue],
) -> Result<ExecutionResult, DeoptError> {
    let _ = module
        .function(function_index)
        .ok_or(DeoptError::InvalidFunctionIndex)?;
    Ok(Interpreter::new().resume(module, function_index, handoff.resume_pc(), registers)?)
}

/// Execute a function in JIT code and explicitly fall back to the interpreter on deopt.
pub fn execute_function_with_fallback(
    module: &Module,
    function_index: FunctionIndex,
    registers: &mut [RegisterValue],
    interrupt_flag: *const u8,
) -> Result<ExecutionResult, DeoptError> {
    let function = module
        .function(function_index)
        .ok_or(DeoptError::InvalidFunctionIndex)?;
    match execute_function_with_interrupt(function, registers, interrupt_flag)? {
        JitExecResult::Ok(raw) => {
            let value = RegisterValue::from_raw_bits(raw).ok_or_else(|| {
                JitError::Internal("jit returned invalid vm register bits".to_string())
            })?;
            Ok(ExecutionResult::new(value))
        }
        JitExecResult::Bailout {
            bytecode_pc,
            reason,
        } => {
            let handoff = handoff_for_bailout(function, bytecode_pc, reason);
            resume_function(module, function_index, handoff, registers)
        }
        JitExecResult::NotCompiled => {
            Ok(Interpreter::new().resume(module, function_index, 0, registers)?)
        }
    }
}

/// Execute a profiled function on shared runtime state and fall back to the interpreter.
pub fn execute_function_profiled_with_fallback(
    module: &Module,
    function_index: FunctionIndex,
    registers: &mut [RegisterValue],
    interrupt_flag: *const u8,
) -> Result<ExecutionResult, DeoptError> {
    let profile = Interpreter::new().profile_property_caches(module, function_index, registers)?;
    let mut runtime = otter_vm::RuntimeState::new();
    match execute_function_profiled_with_runtime(
        module,
        function_index,
        registers,
        &mut runtime,
        &profile,
        interrupt_flag,
    )? {
        JitExecResult::Ok(raw) => {
            let value = RegisterValue::from_raw_bits(raw).ok_or_else(|| {
                JitError::Internal("jit returned invalid vm register bits".to_string())
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

/// Execute the module entry through the JIT on an existing runtime and fall back
/// to the interpreter on bailout or unsupported paths.
///
/// `interrupt_arc`, when present, is forwarded to the bailout interpreter so
/// the watchdog can interrupt long-running scripts that take the
/// interpreter path. Without this, every script that fails to JIT (the
/// vast majority of test262 tests) would silently lose its watchdog flag
/// and infinite loops would never be cancellable.
pub fn execute_module_entry_with_runtime(
    module: &Module,
    runtime: &mut RuntimeState,
    interrupt_flag: *const u8,
    interrupt_arc: Option<Arc<AtomicBool>>,
) -> Result<ExecutionResult, DeoptError> {
    let function_index = module.entry();
    let function = module
        .function(function_index)
        .ok_or(DeoptError::InvalidFunctionIndex)?;
    let register_count = usize::from(function.frame_layout().register_count());
    let mut registers = vec![RegisterValue::undefined(); register_count];

    if let Some(receiver_slot) = function.frame_layout().receiver_slot() {
        let global = runtime.intrinsics().global_object();
        registers[usize::from(receiver_slot)] = RegisterValue::from_object_handle(global.0);
    }

    let make_interpreter = || -> Interpreter {
        let mut interp = Interpreter::new();
        if let Some(ref flag) = interrupt_arc {
            interp = interp.with_interrupt_flag(flag.clone());
        }
        interp
    };

    match execute_function_profiled_with_runtime(
        module,
        function_index,
        &mut registers,
        runtime,
        &[],
        interrupt_flag,
    )? {
        JitExecResult::Ok(raw) => {
            let value = RegisterValue::from_raw_bits(raw).ok_or_else(|| {
                JitError::Internal("jit returned invalid vm register bits".to_string())
            })?;
            Ok(ExecutionResult::new(value))
        }
        JitExecResult::Bailout {
            bytecode_pc,
            reason,
        } => {
            let handoff = handoff_for_bailout(function, bytecode_pc, reason);
            Ok(make_interpreter().resume_with_runtime(
                module,
                function_index,
                handoff.resume_pc(),
                &registers,
                runtime,
            )?)
        }
        JitExecResult::NotCompiled => Ok(make_interpreter().execute_with_runtime(
            module,
            function_index,
            &registers,
            runtime,
        )?),
    }
}
