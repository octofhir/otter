//! JIT compilation pipeline: bytecode → MIR → CLIF → machine code.
//!
//! This is the main entry point for compiling and executing JIT code.

use std::time::Instant;

use otter_vm_bytecode::Function;

use crate::code_cache;
use crate::code_memory::{compile_clif_function, create_host_isa, CompiledFunction};
use crate::codegen::lower::lower_mir_to_clif;
use crate::codegen::value_repr;
use crate::config::JIT_CONFIG;
use crate::context::JitContext;
use crate::mir::builder::build_mir;
use crate::mir::verify::verify;
use crate::telemetry;
use crate::{BAILOUT_SENTINEL, JitError};

/// Result of attempting JIT execution.
pub enum JitExecResult {
    /// JIT code ran successfully, return value is NaN-boxed u64.
    Ok(u64),
    /// JIT code bailed out — resume interpreter at this bytecode PC.
    Bailout { bytecode_pc: u32 },
    /// Function not eligible or compilation failed.
    NotCompiled,
}

/// Check if a function should be JIT-compiled.
pub fn should_jit(function: &Function) -> bool {
    if !JIT_CONFIG.enabled {
        return false;
    }
    if function.flags.is_generator || function.flags.is_async {
        return false;
    }
    if function.is_deoptimized() {
        return false;
    }
    function.is_hot_function()
}

/// Compile a function if not already cached. Returns true on success.
pub fn ensure_compiled(function: &Function) -> bool {
    let func_ptr = function as *const Function;
    if code_cache::contains(func_ptr) {
        return true;
    }
    match compile_function(function) {
        Ok(compiled) => {
            code_cache::insert(func_ptr, compiled);
            true
        }
        Err(_e) => {
            #[cfg(debug_assertions)]
            if JIT_CONFIG.dump_mir {
                eprintln!("[otter-jit] compile failed: {}", _e);
            }
            false
        }
    }
}

/// Try to execute a function via JIT. Compiles on first call if hot.
///
/// # Safety
/// The raw pointers in the parameters must be valid for the call duration.
#[allow(clippy::too_many_arguments)]
pub unsafe fn try_execute(
    function: &Function,
    registers_base: *mut u64,
    local_count: u32,
    register_count: u32,
    this_raw: u64,
    constants: *const (),
    interpreter: *const (),
    vm_ctx: *mut (),
    upvalues_ptr: *const (),
    upvalue_count: u32,
    callee_raw: u64,
    home_object_raw: u64,
    proto_epoch: u64,
    interrupt_flag: *const u8,
) -> JitExecResult {
    if !should_jit(function) {
        return JitExecResult::NotCompiled;
    }

    if !ensure_compiled(function) {
        return JitExecResult::NotCompiled;
    }

    let func_ptr = function as *const Function;
    let entry = match code_cache::get(func_ptr) {
        Some(entry) => entry,
        None => return JitExecResult::NotCompiled,
    };

    // Set up JitContext
    let mut ctx = JitContext {
        registers_base,
        local_count,
        register_count,
        constants,
        this_raw,
        interrupt_flag,
        interpreter,
        vm_ctx,
        function_ptr: function,
        upvalues_ptr,
        upvalue_count,
        callee_raw,
        home_object_raw,
        proto_epoch,
        bailout_reason: 0,
        bailout_pc: 0,
        secondary_result: 0,
    };

    // Call the compiled code
    let jit_fn: unsafe extern "C" fn(*mut JitContext) -> u64 =
        unsafe { std::mem::transmute(entry) };
    let result = unsafe { jit_fn(&mut ctx) };

    telemetry::record_jit_entry();

    if result == BAILOUT_SENTINEL {
        JitExecResult::Bailout {
            bytecode_pc: ctx.bailout_pc,
        }
    } else {
        JitExecResult::Ok(result)
    }
}

/// Compile a bytecode function to native code (internal, not cached).
fn compile_function(function: &Function) -> Result<CompiledFunction, JitError> {
    let start = Instant::now();

    let graph = build_mir(function);

    #[cfg(debug_assertions)]
    {
        if let Err(errors) = verify(&graph) {
            let msgs: Vec<_> = errors.iter().map(|e| e.to_string()).collect();
            return Err(JitError::MirVerification(msgs.join("; ")));
        }
    }

    if JIT_CONFIG.dump_mir {
        eprintln!("=== MIR for {} ===\n{}", graph.function_name, graph);
    }

    let isa = create_host_isa()?;
    let clif_func = lower_mir_to_clif(&graph, isa.as_ref())?;
    let compiled = compile_clif_function(clif_func, isa)?;

    let duration_ns = start.elapsed().as_nanos() as u64;
    telemetry::record_compile_time(true, duration_ns);

    if JIT_CONFIG.dump_asm {
        eprintln!(
            "=== Compiled {} ({} bytes, {}us) ===",
            graph.function_name,
            compiled.code_size,
            duration_ns / 1000,
        );
    }

    Ok(compiled)
}
