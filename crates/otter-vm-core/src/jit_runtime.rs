#![cfg(feature = "jit")]

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use otter_vm_bytecode::Function;
use otter_vm_jit::{JitCompiler, BAILOUT_SENTINEL, DEOPT_THRESHOLD};

use crate::jit_queue;

#[derive(Default)]
struct JitRuntimeState {
    compiler: Option<JitCompiler>,
    compiled_code_ptrs: HashMap<(usize, u32), usize>,
    compile_errors: u64,
    total_bailouts: u64,
    total_deoptimizations: u64,
}

static JIT_RUNTIME_STATE: OnceLock<Mutex<JitRuntimeState>> = OnceLock::new();

fn runtime_state() -> &'static Mutex<JitRuntimeState> {
    JIT_RUNTIME_STATE.get_or_init(|| Mutex::new(JitRuntimeState::default()))
}

/// Result of attempting to execute JIT-compiled code.
pub(crate) enum JitExecResult {
    /// JIT code ran successfully, returning this NaN-boxed value.
    Ok(i64),
    /// JIT code bailed out — re-execute in interpreter.
    Bailout,
    /// No JIT code available for this function.
    NotCompiled,
}

/// Try to execute JIT-compiled code for a function.
///
/// Returns `JitExecResult::Ok(value)` on success, `JitExecResult::Bailout`
/// if the JIT code bailed out (type guard failure), or `JitExecResult::NotCompiled`
/// if no compiled code is available.
pub(crate) fn try_execute_jit(
    module_ptr: usize,
    function_index: u32,
    function: &Function,
    ctx: *mut u8,
) -> JitExecResult {
    // Don't attempt JIT execution for deoptimized functions
    if function.is_deoptimized() {
        return JitExecResult::NotCompiled;
    }

    let code_ptr = {
        let state = runtime_state()
            .lock()
            .expect("jit runtime mutex should not be poisoned");
        state
            .compiled_code_ptrs
            .get(&(module_ptr, function_index))
            .copied()
    };

    let Some(ptr) = code_ptr else {
        return JitExecResult::NotCompiled;
    };

    // SAFETY: ptr is a valid function pointer with signature (i64) -> i64
    let func: extern "C" fn(*mut u8) -> i64 = unsafe { std::mem::transmute(ptr) };
    let result = func(ctx);

    if result == BAILOUT_SENTINEL {
        // Record the bailout on the function
        let deoptimized = function.record_bailout(DEOPT_THRESHOLD);

        let mut state = runtime_state()
            .lock()
            .expect("jit runtime mutex should not be poisoned");
        state.total_bailouts = state.total_bailouts.saturating_add(1);

        if deoptimized {
            // Remove compiled code — this function returns to interpreter permanently
            state
                .compiled_code_ptrs
                .remove(&(module_ptr, function_index));
            state.total_deoptimizations = state.total_deoptimizations.saturating_add(1);
        }

        JitExecResult::Bailout
    } else {
        JitExecResult::Ok(result)
    }
}

/// Invalidate JIT code for a specific function (e.g., after deoptimization).
pub(crate) fn invalidate_jit_code(module_ptr: usize, function_index: u32) {
    let mut state = runtime_state()
        .lock()
        .expect("jit runtime mutex should not be poisoned");
    state
        .compiled_code_ptrs
        .remove(&(module_ptr, function_index));
}

/// Compile one pending JIT request, if present.
pub(crate) fn compile_one_pending_request() {
    let Some(request) = jit_queue::pop_next_request() else {
        return;
    };

    // Don't compile functions that have been deoptimized
    if request.function.is_deoptimized() {
        return;
    }

    let mut state = runtime_state()
        .lock()
        .expect("jit runtime mutex should not be poisoned");

    if state.compiler.is_none() {
        match JitCompiler::new() {
            Ok(compiler) => state.compiler = Some(compiler),
            Err(_) => {
                state.compile_errors = state.compile_errors.saturating_add(1);
                return;
            }
        }
    }

    let compiler = state
        .compiler
        .as_mut()
        .expect("jit compiler should be initialized");
    match compiler.compile_function(&request.function) {
        Ok(artifact) => {
            state.compiled_code_ptrs.insert(
                (request.module_ptr, request.function_index),
                artifact.code_ptr as usize,
            );
        }
        Err(_) => {
            state.compile_errors = state.compile_errors.saturating_add(1);
        }
    }
}

/// Get JIT runtime statistics.
#[cfg(test)]
pub(crate) fn stats() -> (u64, u64) {
    let state = runtime_state()
        .lock()
        .expect("jit runtime mutex should not be poisoned");
    (state.total_bailouts, state.total_deoptimizations)
}

#[cfg(test)]
pub(crate) fn clear_for_tests() {
    let mut state = runtime_state()
        .lock()
        .expect("jit runtime mutex should not be poisoned");
    state.compiler = None;
    state.compiled_code_ptrs.clear();
    state.compile_errors = 0;
    state.total_bailouts = 0;
    state.total_deoptimizations = 0;
}

#[cfg(test)]
pub(crate) fn compiled_count() -> usize {
    let state = runtime_state()
        .lock()
        .expect("jit runtime mutex should not be poisoned");
    state.compiled_code_ptrs.len()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use otter_vm_bytecode::{Function, Instruction, Module, Register};

    use super::*;

    #[test]
    fn compile_one_pending_request_consumes_queue_item() {
        crate::jit_queue::clear_for_tests();
        clear_for_tests();

        let mut builder = Module::builder("jit-runtime-test.js");
        builder.add_function(
            Function::builder()
                .name("f")
                .instruction(Instruction::LoadInt32 {
                    dst: Register(0),
                    value: 1,
                })
                .instruction(Instruction::Return { src: Register(0) })
                .build(),
        );
        let module = Arc::new(builder.build());
        let function = module.function(0).expect("function 0 should exist");
        function.mark_hot();

        assert!(crate::jit_queue::enqueue_hot_function(&module, 0, function));
        assert_eq!(crate::jit_queue::pending_count(), 1);

        compile_one_pending_request();

        assert_eq!(crate::jit_queue::pending_count(), 0);
        assert_eq!(compiled_count(), 1);
    }
}
