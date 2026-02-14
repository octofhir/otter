#![cfg(feature = "jit")]

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use otter_vm_bytecode::Function;
use otter_vm_jit::{BAILOUT_SENTINEL, DEOPT_THRESHOLD, JitCompiler};

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

/// Lock the runtime state, recovering from poisoned mutex (test resilience).
fn lock_state() -> std::sync::MutexGuard<'static, JitRuntimeState> {
    runtime_state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Result of attempting to execute JIT-compiled code.
pub(crate) enum JitExecResult {
    /// JIT code ran successfully, returning this value.
    Ok(i64),
    /// JIT code bailed out — caller should re-execute in interpreter.
    Bailout,
    /// No JIT code available for this function.
    NotCompiled,
}

/// Try to execute JIT-compiled code for a function.
///
/// Returns `Ok(value)` on success, `Bailout` if a runtime condition caused
/// the JIT code to bail out, or `NotCompiled` if no code exists.
pub(crate) fn try_execute_jit(
    module_ptr: usize,
    function_index: u32,
    function: &Function,
) -> JitExecResult {
    if function.is_deoptimized() {
        return JitExecResult::NotCompiled;
    }

    let code_ptr = {
        let state = lock_state();
        state
            .compiled_code_ptrs
            .get(&(module_ptr, function_index))
            .copied()
    };

    let Some(ptr) = code_ptr else {
        return JitExecResult::NotCompiled;
    };

    // SAFETY: ptr was produced by JitCompiler with signature () -> i64
    let func: extern "C" fn() -> i64 = unsafe { std::mem::transmute(ptr) };
    let result = func();

    if result == BAILOUT_SENTINEL {
        let deoptimized = function.record_bailout(DEOPT_THRESHOLD);

        let mut state = lock_state();
        state.total_bailouts = state.total_bailouts.saturating_add(1);

        if deoptimized {
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

/// Invalidate JIT code for a specific function.
#[allow(dead_code)]
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

/// Directly compile and register a function (bypasses queue).
/// Used for testing and eager compilation.
#[cfg(test)]
fn compile_and_register(module_ptr: usize, function_index: u32, function: &Function) -> bool {
    let mut state = runtime_state()
        .lock()
        .expect("jit runtime mutex should not be poisoned");

    if state.compiler.is_none() {
        match JitCompiler::new() {
            Ok(compiler) => state.compiler = Some(compiler),
            Err(_) => return false,
        }
    }

    let compiler = state.compiler.as_mut().unwrap();
    match compiler.compile_function(function) {
        Ok(artifact) => {
            state
                .compiled_code_ptrs
                .insert((module_ptr, function_index), artifact.code_ptr as usize);
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use otter_vm_bytecode::{Function, Instruction, Module, Register};

    use super::*;

    #[test]
    fn compile_one_pending_request_consumes_queue() {
        // This test only verifies the queue→compile pipeline
        let module = {
            let mut b = Module::builder("jit-queue-test.js");
            b.add_function(
                Function::builder()
                    .name("f")
                    .register_count(1)
                    .instruction(Instruction::LoadInt32 {
                        dst: Register(0),
                        value: 1,
                    })
                    .instruction(Instruction::Return { src: Register(0) })
                    .build(),
            );
            Arc::new(b.build())
        };
        let function = module.function(0).expect("function 0");
        function.mark_hot();

        // Clear queue and add our item
        crate::jit_queue::clear_for_tests();
        assert!(crate::jit_queue::enqueue_hot_function(&module, 0, function));
        assert_eq!(crate::jit_queue::pending_count(), 1);

        compile_one_pending_request();

        assert_eq!(crate::jit_queue::pending_count(), 0);
        // Verify compilation happened by checking the code is accessible
        let module_ptr = Arc::as_ptr(&module) as usize;
        let state = runtime_state().lock().unwrap();
        assert!(state.compiled_code_ptrs.contains_key(&(module_ptr, 0)));
    }

    #[test]
    fn try_execute_not_compiled() {
        let f = Function::builder()
            .name("unknown")
            .register_count(1)
            .instruction(Instruction::Return { src: Register(0) })
            .build();

        // Use a unique key that can't collide
        match try_execute_jit(0xCAFE_0001, 99, &f) {
            JitExecResult::NotCompiled => {}
            _ => panic!("expected NotCompiled"),
        }
    }

    #[test]
    fn try_execute_success() {
        let f = Function::builder()
            .name("add")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 10,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 20,
            })
            .instruction(Instruction::Add {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
                feedback_index: 0,
            })
            .instruction(Instruction::Return { src: Register(2) })
            .feedback_vector_size(1)
            .build();

        let key = 0xBEEF_0001_usize;
        assert!(compile_and_register(key, 0, &f));

        match try_execute_jit(key, 0, &f) {
            JitExecResult::Ok(val) => assert_eq!(val, 30),
            _ => panic!("expected Ok(30)"),
        }
    }

    #[test]
    fn bailout_on_div_by_zero() {
        let f = Function::builder()
            .name("div0")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 42,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 0,
            })
            .instruction(Instruction::Div {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
                feedback_index: 0,
            })
            .instruction(Instruction::Return { src: Register(2) })
            .feedback_vector_size(1)
            .build();

        let key = 0xBEEF_0002_usize;
        assert!(compile_and_register(key, 0, &f));

        match try_execute_jit(key, 0, &f) {
            JitExecResult::Bailout => {}
            _ => panic!("expected Bailout on div-by-zero"),
        }

        assert_eq!(f.get_bailout_count(), 1);
        assert!(!f.is_deoptimized());
    }

    #[test]
    fn deoptimize_after_threshold_bailouts() {
        let f = Function::builder()
            .name("deopt_me")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 1,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 0,
            })
            .instruction(Instruction::Div {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
                feedback_index: 0,
            })
            .instruction(Instruction::Return { src: Register(2) })
            .feedback_vector_size(1)
            .build();

        let key = 0xBEEF_0003_usize;
        assert!(compile_and_register(key, 0, &f));

        // Bail out until deoptimized
        for _ in 0..DEOPT_THRESHOLD {
            match try_execute_jit(key, 0, &f) {
                JitExecResult::Bailout => {}
                JitExecResult::NotCompiled => break,
                _ => panic!("expected Bailout or NotCompiled"),
            }
        }

        assert!(f.is_deoptimized());

        // Code should be removed from compiled map
        let state = runtime_state().lock().unwrap();
        assert!(!state.compiled_code_ptrs.contains_key(&(key, 0)));
        drop(state);

        // Future calls return NotCompiled
        match try_execute_jit(key, 0, &f) {
            JitExecResult::NotCompiled => {}
            _ => panic!("expected NotCompiled after deoptimization"),
        }
    }

    #[test]
    fn deoptimized_function_not_recompiled() {
        let f = Function::builder()
            .name("deoptd")
            .register_count(1)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 1,
            })
            .instruction(Instruction::Return { src: Register(0) })
            .build();

        // Manually deoptimize
        for _ in 0..DEOPT_THRESHOLD {
            f.record_bailout(DEOPT_THRESHOLD);
        }
        assert!(f.is_deoptimized());

        // Direct compile_and_register doesn't check deopt (that's the queue's job)
        // But the queue path should skip it:
        let module = {
            let mut b = Module::builder("jit-no-recompile.js");
            b.add_function(f);
            Arc::new(b.build())
        };
        let function = module.function(0).expect("function 0");
        // Deoptimize via the module's copy
        for _ in 0..DEOPT_THRESHOLD {
            function.record_bailout(DEOPT_THRESHOLD);
        }
        assert!(function.is_deoptimized());

        crate::jit_queue::clear_for_tests();
        assert!(crate::jit_queue::enqueue_hot_function(&module, 0, function));
        compile_one_pending_request();

        // Should NOT have compiled
        let module_ptr = Arc::as_ptr(&module) as usize;
        let state = runtime_state().lock().unwrap();
        assert!(
            !state.compiled_code_ptrs.contains_key(&(module_ptr, 0)),
            "deoptimized function should not be compiled"
        );
    }
}
