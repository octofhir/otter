//! JIT compilation pipeline: bytecode → MIR → CLIF → machine code.
//!
//! This is the main entry point for compiling and executing JIT code.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Instant;

use otter_vm as next_vm;
use otter_vm_bytecode::Function;

use crate::code_cache;
use crate::code_memory::{CompiledFunction, compile_clif_function, create_host_isa};
use crate::codegen::lower::lower_mir_to_clif;
use crate::config::JIT_CONFIG;
use crate::context::JitContext;
use crate::mir::builder::build_mir;
use crate::mir::next_builder::build_next_mir;
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
    /// Helper symbols as Vec for JITBuilder registration.
    static HELPER_SYMBOLS: RefCell<Vec<(&'static str, *const u8)>> = const { RefCell::new(Vec::new()) };
    /// Helper address lookup: name → address. O(1).
    static HELPER_ADDRS: RefCell<HashMap<&'static str, usize>> = RefCell::new(HashMap::new());
}

/// Register runtime helper symbols for JIT compilation.
/// Called once by `otter-vm-core` during initialization.
pub fn register_helper_symbols(symbols: Vec<(&'static str, *const u8)>) {
    HELPER_ADDRS.with(|m| {
        let mut map = m.borrow_mut();
        map.clear();
        for &(name, ptr) in &symbols {
            map.insert(name, ptr as usize);
        }
    });
    HELPER_SYMBOLS.with(|s| *s.borrow_mut() = symbols);
}

fn get_helper_symbols() -> Vec<(&'static str, *const u8)> {
    HELPER_SYMBOLS.with(|s| s.borrow().clone())
}

/// Look up a helper function address by symbol name. O(1).
pub fn lookup_helper_address(name: &str) -> Option<usize> {
    HELPER_ADDRS.with(|m| m.borrow().get(name).copied())
}

// ============================================================
// Compilation & Execution API
// ============================================================

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
            if JIT_CONFIG.dump_mir || JIT_CONFIG.dump_asm {
                eprintln!(
                    "[otter-jit] compile failed for {:?}: {}",
                    function.name.as_deref().unwrap_or("<anon>"),
                    _e
                );
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
        next_module: std::ptr::null(),
        next_runtime: std::ptr::null_mut(),
    };

    let jit_fn: unsafe extern "C" fn(*mut JitContext) -> u64 =
        unsafe { std::mem::transmute(entry) };
    let result = unsafe { jit_fn(&mut ctx) };

    telemetry::record_jit_entry();

    if result == BAILOUT_SENTINEL {
        JitExecResult::Bailout {
            bytecode_pc: ctx.bailout_pc,
            reason: crate::BailoutReason::from_raw(ctx.bailout_reason)
                .unwrap_or(crate::BailoutReason::Unsupported),
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

    let helpers = get_helper_symbols();
    let helper_refs: Vec<(&str, *const u8)> = helpers.iter().map(|&(n, p)| (n, p)).collect();
    let compiled = compile_clif_function(clif_func, isa, &helper_refs)?;

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

/// Compile a new-VM function into machine code for the Tier 1 subset.
pub fn compile_next_function(function: &next_vm::Function) -> Result<CompiledFunction, JitError> {
    compile_next_function_profiled(function, &[])
}

/// Compile a profiled new-VM function into machine code for the Tier 1 subset.
pub fn compile_next_function_profiled(
    function: &next_vm::Function,
    property_profile: &[Option<next_vm::PropertyInlineCache>],
) -> Result<CompiledFunction, JitError> {
    let start = Instant::now();
    let graph = build_next_mir(
        function,
        (!property_profile.is_empty()).then_some(property_profile),
    )?;

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
    Ok(compiled)
}

/// Execute a new-VM function through the Tier 1 JIT path.
pub fn execute_next_function(
    function: &next_vm::Function,
    registers: &mut [next_vm::RegisterValue],
) -> Result<JitExecResult, JitError> {
    execute_next_function_with_interrupt(function, registers, std::ptr::null())
}

/// Execute a new-VM function through the Tier 1 JIT path with an explicit interrupt flag.
pub fn execute_next_function_with_interrupt(
    function: &next_vm::Function,
    registers: &mut [next_vm::RegisterValue],
    interrupt_flag: *const u8,
) -> Result<JitExecResult, JitError> {
    let required_len = usize::from(function.frame_layout().register_count());
    if registers.len() < required_len {
        return Err(JitError::Internal(format!(
            "register slice too small for new-vm function: need {}, got {}",
            required_len,
            registers.len()
        )));
    }

    let compiled = compile_next_function(function)?;
    let register_count = u32::try_from(required_len)
        .map_err(|_| JitError::Internal("register count does not fit into u32".to_string()))?;

    let mut ctx = JitContext {
        registers_base: registers.as_mut_ptr().cast::<u64>(),
        local_count: register_count,
        register_count,
        constants: std::ptr::null(),
        this_raw: next_vm::RegisterValue::undefined().raw_bits(),
        interrupt_flag,
        interpreter: std::ptr::null(),
        vm_ctx: std::ptr::null_mut(),
        function_ptr: std::ptr::null(),
        upvalues_ptr: std::ptr::null(),
        upvalue_count: 0,
        callee_raw: next_vm::RegisterValue::undefined().raw_bits(),
        home_object_raw: next_vm::RegisterValue::undefined().raw_bits(),
        proto_epoch: 0,
        bailout_reason: 0,
        bailout_pc: 0,
        secondary_result: 0,
        next_module: std::ptr::null(),
        next_runtime: std::ptr::null_mut(),
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

/// Execute a profiled new-VM function with access to the shared new-VM runtime state.
pub fn execute_next_function_profiled_with_runtime(
    module: &next_vm::Module,
    function_index: next_vm::FunctionIndex,
    registers: &mut [next_vm::RegisterValue],
    runtime: &mut next_vm::RuntimeState,
    property_profile: &[Option<next_vm::PropertyInlineCache>],
    interrupt_flag: *const u8,
) -> Result<JitExecResult, JitError> {
    let function = module
        .function(function_index)
        .ok_or_else(|| JitError::Internal("new-vm function index is out of bounds".to_string()))?;
    let required_len = usize::from(function.frame_layout().register_count());
    if registers.len() < required_len {
        return Err(JitError::Internal(format!(
            "register slice too small for new-vm function: need {}, got {}",
            required_len,
            registers.len()
        )));
    }

    let compiled = compile_next_function_profiled(function, property_profile)?;
    let register_count = u32::try_from(required_len)
        .map_err(|_| JitError::Internal("register count does not fit into u32".to_string()))?;

    let mut ctx = JitContext {
        registers_base: registers.as_mut_ptr().cast::<u64>(),
        local_count: register_count,
        register_count,
        constants: std::ptr::null(),
        this_raw: next_vm::RegisterValue::undefined().raw_bits(),
        interrupt_flag,
        interpreter: std::ptr::null(),
        vm_ctx: std::ptr::null_mut(),
        function_ptr: std::ptr::null(),
        upvalues_ptr: std::ptr::null(),
        upvalue_count: 0,
        callee_raw: next_vm::RegisterValue::undefined().raw_bits(),
        home_object_raw: next_vm::RegisterValue::undefined().raw_bits(),
        proto_epoch: 0,
        bailout_reason: 0,
        bailout_pc: 0,
        secondary_result: 0,
        next_module: module as *const next_vm::Module as *const (),
        next_runtime: runtime as *mut next_vm::RuntimeState as *mut (),
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
