//! Explicit deopt handoff and interpreter-resume helpers for the JIT path.
//!
//! Public surface: [`execute_module_entry_with_runtime`] is the single
//! entry point `otter-runtime` uses to drive a module through the JIT
//! (when the template baseline accepts the entry function) with an
//! interpreter fallback for bailout, non-eligible functions, and
//! unsupported opcodes. [`handoff_for_bailout`] exposes the deopt
//! descriptor a resume path needs.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use otter_vm::RuntimeState;
use otter_vm::deopt::{DeoptHandoff, DeoptId, DeoptReason, DeoptSite};
use otter_vm::interpreter::{ExecutionResult, InterpreterError};
use otter_vm::{Interpreter, Module, RegisterValue};

use crate::JitError;
use crate::deopt::BailoutReason;
use crate::pipeline::JitExecResult;

/// Errors produced while deopting back into the interpreter.
#[derive(Debug, thiserror::Error)]
pub enum DeoptError {
    /// The JIT raised an internal error.
    #[error("{0}")]
    Jit(#[from] JitError),
    /// The interpreter raised an error while resuming or running the fallback.
    #[error("{0}")]
    Interpreter(#[from] InterpreterError),
    /// The function index stored on a module was out of bounds.
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

/// Build the deopt descriptor an interpreter resume needs after a JIT
/// bailout. The descriptor carries the resume PC plus a
/// [`DeoptSite`] shared-layout reason so the interpreter can re-enter
/// at a spec-compliant sync point.
#[must_use]
pub fn handoff_for_bailout(
    function: &otter_vm::Function,
    bytecode_pc: u32,
    reason: BailoutReason,
) -> DeoptHandoff {
    let deopt_reason = map_bailout_reason(reason);
    let deopt_id = function
        .deopt()
        .sites()
        .iter()
        .find(|site| site.pc() == bytecode_pc)
        .map_or(DeoptId(0), |site| site.id());
    let site = DeoptSite::new(deopt_id, bytecode_pc);
    DeoptHandoff::new(site, bytecode_pc, deopt_reason)
}

/// Execute the module entry through the JIT on an existing runtime, and
/// transparently fall back to the interpreter on bailout, unsupported
/// opcodes, or a non-eligible function.
///
/// `interrupt_arc`, when present, is forwarded to the bailout
/// interpreter so the watchdog can interrupt long-running scripts that
/// take the interpreter path. Without it, scripts that fail to JIT lose
/// their watchdog flag and infinite loops would never be cancellable.
pub fn execute_module_entry_with_runtime(
    module: &Module,
    runtime: &mut RuntimeState,
    interrupt_flag: *const u8,
    interrupt_arc: Option<Arc<AtomicBool>>,
) -> Result<ExecutionResult, DeoptError> {
    if !crate::config::jit_config().enabled {
        let mut interpreter = Interpreter::new();
        if let Some(flag) = interrupt_arc {
            interpreter = interpreter.with_interrupt_flag(flag);
        }
        return Ok(interpreter.execute_module(module, runtime)?);
    }

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

    // Try the template baseline. Anything outside its int32-arithmetic
    // subset returns `Err(JitError::UnsupportedInstruction)` and we fall
    // through to the interpreter.
    let compiled_result = if let Some(feedback) = runtime.feedback_vector(function_index) {
        crate::pipeline::compile_function_with_feedback(function, feedback)
    } else {
        crate::pipeline::compile_function(function)
    };

    let jit_result: Result<JitExecResult, JitError> = match compiled_result {
        Ok(compiled) => {
            let register_count = u32::try_from(register_count)
                .map_err(|_| JitError::Internal("register count overflow".to_string()))?;
            let this_raw = function
                .frame_layout()
                .receiver_slot()
                .and_then(|slot| registers.get(usize::from(slot)))
                .map_or(RegisterValue::undefined().raw_bits(), |v| v.raw_bits());

            let mut ctx = crate::context::JitContext {
                registers_base: registers.as_mut_ptr().cast::<u64>(),
                local_count: register_count,
                register_count,
                constants: std::ptr::null(),
                this_raw,
                interrupt_flag,
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
                module_ptr: module as *const Module as *const (),
                runtime_ptr: runtime as *mut RuntimeState as *mut (),
                heap_slots_base: runtime.heap().slots_ptr(),
                accumulator_raw: RegisterValue::undefined().raw_bits(),
            };

            crate::telemetry::record_jit_entry();
            crate::telemetry::record_function_jit_entry(
                function.name().unwrap_or("<anonymous>"),
                1,
            );
            // SAFETY: `compiled.entry` is a JIT stencil produced by our
            // own pipeline with the documented `extern "C" fn(*mut
            // JitContext) -> u64` ABI. `ctx` above is fully initialised;
            // `registers`/`runtime`/`module` outlive this call. Bailout
            // state and return value are written into `ctx`.
            let raw = unsafe { compiled.call(&mut ctx) };
            if raw == crate::BAILOUT_SENTINEL {
                Ok(JitExecResult::Bailout {
                    bytecode_pc: ctx.bailout_pc,
                    reason: crate::BailoutReason::from_raw(ctx.bailout_reason)
                        .unwrap_or(crate::BailoutReason::Unsupported),
                })
            } else {
                Ok(JitExecResult::Ok(raw))
            }
        }
        Err(_) => Ok(JitExecResult::NotCompiled),
    };

    match jit_result? {
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
            crate::telemetry::record_interpreter_entry();
            crate::telemetry::record_deopt(
                function.name().unwrap_or("<anonymous>"),
                module as *const Module as usize as u64,
                bytecode_pc,
                reason,
            );
            crate::telemetry::record_function_deopt(function.name().unwrap_or("<anonymous>"), 1);
            let handoff = handoff_for_bailout(function, bytecode_pc, reason);
            Ok(make_interpreter().resume_with_runtime(
                module,
                function_index,
                handoff.resume_pc(),
                &registers,
                runtime,
            )?)
        }
        JitExecResult::NotCompiled => {
            crate::telemetry::record_interpreter_entry();
            Ok(make_interpreter().execute_with_runtime(
                module,
                function_index,
                &registers,
                runtime,
            )?)
        }
    }
}
