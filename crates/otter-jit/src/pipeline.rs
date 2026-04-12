//! JIT compilation pipeline: bytecode → MIR → CLIF → machine code.
//!
//! This is the main entry point for compiling and executing JIT code.

use std::collections::HashMap;
use std::time::Instant;

use otter_vm as vm;

use crate::code_memory::{CompiledFunction, compile_clif_function, create_host_isa};
use crate::codegen::lower::lower_mir_to_clif;
use crate::context::JitContext;
use crate::mir::builder::build_mir;
use crate::mir::verify::verify;
use crate::telemetry;
use crate::{BAILOUT_SENTINEL, JitError};

/// Result of attempting JIT execution.
#[derive(Debug)]
pub enum JitExecResult {
    /// JIT code ran successfully, return value is NaN-boxed u64.
    Ok(u64),
    /// JIT code bailed out — resume interpreter at this bytecode PC.
    Bailout {
        bytecode_pc: u32,
        reason: crate::BailoutReason,
    },
    /// Function not eligible or compilation failed.
    NotCompiled,
}

// ============================================================
// Helper symbol registration
// ============================================================

thread_local! {
    /// Helper address lookup: name → address. O(1).
    static HELPER_ADDRS: std::cell::RefCell<HashMap<&'static str, usize>> = std::cell::RefCell::new(HashMap::new());
}

/// Register runtime helper symbols for JIT compilation.
pub fn register_helper_symbols(symbols: Vec<(&'static str, *const u8)>) {
    HELPER_ADDRS.with(|m| {
        let mut map = m.borrow_mut();
        map.clear();
        for &(name, ptr) in &symbols {
            map.insert(name, ptr as usize);
        }
    });
}

/// Look up a helper function address by symbol name. O(1).
pub fn lookup_helper_address(name: &str) -> Option<usize> {
    HELPER_ADDRS.with(|m| m.borrow().get(name).copied())
}

/// Clear the helper symbol table (called on runtime teardown).
pub fn clear_helper_symbols() {
    HELPER_ADDRS.with(|m| m.borrow_mut().clear());
}

// ============================================================
// Compilation & Execution API
// ============================================================

/// Compile a VM function into machine code for the Tier 1 subset.
pub fn compile_function(function: &vm::Function) -> Result<CompiledFunction, JitError> {
    compile_function_profiled(function, &[])
}

/// Compile a profiled VM function into machine code for the Tier 1 subset.
pub fn compile_function_profiled(
    function: &vm::Function,
    property_profile: &[Option<vm::PropertyInlineCache>],
) -> Result<CompiledFunction, JitError> {
    let cfg = crate::config::jit_config();
    let start = Instant::now();

    if cfg.dump_bytecode {
        eprintln!("[JIT] === Bytecode for {:?} ({} instructions) ===",
                  function.name(), function.bytecode().len());
        for (pc, instr) in function.bytecode().instructions().iter().enumerate() {
            eprintln!("  {:04}: {:?} a={} b={} c={}",
                      pc, instr.opcode(), instr.a(), instr.b(), instr.c());
        }
    }

    let graph = build_mir(
        function,
        (!property_profile.is_empty()).then_some(property_profile),
    )?;

    if cfg.dump_mir {
        eprintln!("[JIT] === MIR for {:?} ===", function.name());
        eprintln!("{}", graph);
    }

    #[cfg(debug_assertions)]
    {
        if let Err(errors) = verify(&graph) {
            let msgs: Vec<_> = errors.iter().map(|e| e.to_string()).collect();
            return Err(JitError::MirVerification(msgs.join("; ")));
        }
    }

    let isa = create_host_isa()?;
    let clif_func = lower_mir_to_clif(&graph, isa.as_ref())?;
    let compiled = compile_clif_function(clif_func, isa, &[])?;

    let duration_ns = start.elapsed().as_nanos() as u64;
    telemetry::record_compile_time(true, duration_ns);
    telemetry::record_function_compiled(
        function.name().unwrap_or("<anonymous>"),
        1, // tier 1
        duration_ns,
        compiled.code_size,
    );
    Ok(compiled)
}

/// Execute a VM function through the Tier 1 JIT path.
pub fn execute_function(
    function: &vm::Function,
    registers: &mut [vm::RegisterValue],
) -> Result<JitExecResult, JitError> {
    execute_function_with_interrupt(function, registers, std::ptr::null())
}

/// Execute a VM function through the Tier 1 JIT path with an explicit interrupt flag.
pub fn execute_function_with_interrupt(
    function: &vm::Function,
    registers: &mut [vm::RegisterValue],
    interrupt_flag: *const u8,
) -> Result<JitExecResult, JitError> {
    let required_len = usize::from(function.frame_layout().register_count());
    if registers.len() < required_len {
        return Err(JitError::Internal(format!(
            "register slice too small for vm function: need {}, got {}",
            required_len,
            registers.len()
        )));
    }

    let compiled = compile_function(function)?;
    let register_count = u32::try_from(required_len)
        .map_err(|_| JitError::Internal("register count does not fit into u32".to_string()))?;

    let mut ctx = JitContext {
        registers_base: registers.as_mut_ptr().cast::<u64>(),
        local_count: register_count,
        register_count,
        constants: std::ptr::null(),
        this_raw: vm::RegisterValue::undefined().raw_bits(),
        interrupt_flag,
        interpreter: std::ptr::null(),
        vm_ctx: std::ptr::null_mut(),
        function_ptr: std::ptr::null(),
        upvalues_ptr: std::ptr::null(),
        upvalue_count: 0,
        callee_raw: vm::RegisterValue::undefined().raw_bits(),
        home_object_raw: vm::RegisterValue::undefined().raw_bits(),
        proto_epoch: 0,
        bailout_reason: 0,
        bailout_pc: 0,
        secondary_result: 0,
        module_ptr: std::ptr::null(),
        runtime_ptr: std::ptr::null_mut(),
    };

    let result = unsafe { compiled.call(&mut ctx) };
    if result == BAILOUT_SENTINEL {
        Ok(JitExecResult::Bailout {
            bytecode_pc: ctx.bailout_pc,
            reason: crate::BailoutReason::from_raw(ctx.bailout_reason)
                .unwrap_or(crate::BailoutReason::Unsupported),
        })
    } else {
        Ok(JitExecResult::Ok(result))
    }
}

/// Execute a profiled VM function with access to shared runtime state.
pub fn execute_function_profiled_with_runtime(
    module: &vm::Module,
    function_index: vm::FunctionIndex,
    registers: &mut [vm::RegisterValue],
    runtime: &mut vm::RuntimeState,
    property_profile: &[Option<vm::PropertyInlineCache>],
    interrupt_flag: *const u8,
) -> Result<JitExecResult, JitError> {
    let function = module
        .function(function_index)
        .ok_or_else(|| JitError::Internal("vm function index is out of bounds".to_string()))?;
    let required_len = usize::from(function.frame_layout().register_count());
    if registers.len() < required_len {
        return Err(JitError::Internal(format!(
            "register slice too small for vm function: need {}, got {}",
            required_len,
            registers.len()
        )));
    }

    let compiled = compile_function_profiled(function, property_profile)?;
    let register_count = u32::try_from(required_len)
        .map_err(|_| JitError::Internal("register count does not fit into u32".to_string()))?;

    // Resolve `this` from the receiver slot if the function declares one,
    // otherwise default to `undefined`. Global scripts use a receiver slot
    // that points to the global object.
    let this_raw = function
        .frame_layout()
        .receiver_slot()
        .and_then(|slot| registers.get(usize::from(slot)))
        .map_or(vm::RegisterValue::undefined().raw_bits(), |v| v.raw_bits());

    let mut ctx = JitContext {
        registers_base: registers.as_mut_ptr().cast::<u64>(),
        local_count: register_count,
        register_count,
        constants: std::ptr::null(),
        this_raw,
        interrupt_flag,
        interpreter: std::ptr::null(),
        vm_ctx: std::ptr::null_mut(),
        function_ptr: std::ptr::null(),
        upvalues_ptr: std::ptr::null(),
        upvalue_count: 0,
        callee_raw: vm::RegisterValue::undefined().raw_bits(),
        home_object_raw: vm::RegisterValue::undefined().raw_bits(),
        proto_epoch: 0,
        bailout_reason: 0,
        bailout_pc: 0,
        secondary_result: 0,
        module_ptr: module as *const vm::Module as *const (),
        runtime_ptr: runtime as *mut vm::RuntimeState as *mut (),
    };

    let result = unsafe { compiled.call(&mut ctx) };
    if result == BAILOUT_SENTINEL {
        Ok(JitExecResult::Bailout {
            bytecode_pc: ctx.bailout_pc,
            reason: crate::BailoutReason::from_raw(ctx.bailout_reason)
                .unwrap_or(crate::BailoutReason::Unsupported),
        })
    } else {
        Ok(JitExecResult::Ok(result))
    }
}
