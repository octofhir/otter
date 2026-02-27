#![cfg(feature = "jit")]

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use otter_vm_bytecode::Function;
use otter_vm_jit::{BAILOUT_SENTINEL, DEOPT_THRESHOLD, JitCompiler};

use crate::jit_queue;
use crate::value::Value;

#[derive(Debug, Clone, Copy, Default)]
/// Snapshot of runtime JIT counters for diagnostics.
pub struct JitRuntimeStats {
    /// Number of dequeued compilation requests.
    pub compile_requests: u64,
    /// Number of successful compilations.
    pub compile_successes: u64,
    /// Number of compilation failures.
    pub compile_errors: u64,
    /// Number of JIT execution attempts.
    pub execute_attempts: u64,
    /// Number of successful JIT executions.
    pub execute_hits: u64,
    /// Number of attempts that had no compiled machine code.
    pub execute_not_compiled: u64,
    /// Number of JIT bailouts to interpreter.
    pub execute_bailouts: u64,
    /// Number of functions deoptimized after repeated bailouts.
    pub deoptimizations: u64,
    /// Current number of compiled functions cached in runtime state.
    pub compiled_functions: u64,
}

#[derive(Default)]
struct JitRuntimeState {
    compiler: Option<JitCompiler>,
    /// Keyed by `(module_id, function_index)`.  `module_id` is the stable
    /// unique ID assigned at `Module` construction — unlike `Arc::as_ptr`,
    /// it cannot be reused after the `Arc` is dropped.
    compiled_code_ptrs: HashMap<(u64, u32), usize>,
    compile_errors: u64,
    compile_requests: u64,
    compile_successes: u64,
    execute_attempts: u64,
    execute_hits: u64,
    execute_not_compiled: u64,
    total_bailouts: u64,
    total_deoptimizations: u64,
}

static JIT_RUNTIME_STATE: OnceLock<Mutex<JitRuntimeState>> = OnceLock::new();
static JIT_ENABLED: OnceLock<bool> = OnceLock::new();
static JIT_STATS_ENABLED: OnceLock<bool> = OnceLock::new();
static JIT_EAGER_ENABLED: OnceLock<bool> = OnceLock::new();

fn runtime_state() -> &'static Mutex<JitRuntimeState> {
    JIT_RUNTIME_STATE.get_or_init(|| Mutex::new(JitRuntimeState::default()))
}

/// Lock the runtime state, recovering from poisoned mutex (test resilience).
fn lock_state() -> std::sync::MutexGuard<'static, JitRuntimeState> {
    runtime_state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn parse_env_truthy(value: &str) -> bool {
    !matches!(value.trim(), "" | "0")
        && !value.trim().eq_ignore_ascii_case("false")
        && !value.trim().eq_ignore_ascii_case("off")
        && !value.trim().eq_ignore_ascii_case("no")
}

pub(crate) fn is_jit_enabled() -> bool {
    *JIT_ENABLED.get_or_init(|| {
        !std::env::var("OTTER_DISABLE_JIT")
            .ok()
            .is_some_and(|v| parse_env_truthy(&v))
    })
}

fn is_jit_stats_enabled() -> bool {
    *JIT_STATS_ENABLED.get_or_init(|| {
        std::env::var("OTTER_JIT_STATS")
            .ok()
            .is_some_and(|v| parse_env_truthy(&v))
    })
}

pub(crate) fn is_jit_eager_enabled() -> bool {
    *JIT_EAGER_ENABLED.get_or_init(|| {
        std::env::var("OTTER_JIT_EAGER")
            .ok()
            .is_some_and(|v| parse_env_truthy(&v))
    })
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
    module_id: u64,
    function_index: u32,
    function: &Function,
    args: &[Value],
) -> JitExecResult {
    if !is_jit_enabled() {
        return JitExecResult::NotCompiled;
    }

    let stats_enabled = is_jit_stats_enabled();

    if function.is_deoptimized() {
        if stats_enabled {
            let mut state = lock_state();
            state.execute_attempts = state.execute_attempts.saturating_add(1);
            state.execute_not_compiled = state.execute_not_compiled.saturating_add(1);
        }
        return JitExecResult::NotCompiled;
    }

    if stats_enabled {
        let mut state = lock_state();
        state.execute_attempts = state.execute_attempts.saturating_add(1);
    }

    let mut ptr = function.jit_entry_ptr();
    if ptr == 0 {
        ptr = {
            let state = lock_state();
            state
                .compiled_code_ptrs
                .get(&(module_id, function_index))
                .copied()
                .unwrap_or(0)
        };
        if ptr != 0 {
            function.set_jit_entry_ptr(ptr);
        } else if stats_enabled {
            let mut state = lock_state();
            state.execute_not_compiled = state.execute_not_compiled.saturating_add(1);
        }
    }

    if ptr == 0 {
        return JitExecResult::NotCompiled;
    }

    // SAFETY: ptr was produced by JitCompiler with signature
    // `(*const i64, u32) -> i64`.
    let func: extern "C" fn(*const i64, u32) -> i64 = unsafe { std::mem::transmute(ptr) };
    let argc = args.len() as u32;
    let result = if args.len() <= 8 {
        let mut inline = [0_i64; 8];
        for (idx, arg) in args.iter().enumerate() {
            inline[idx] = arg.to_jit_bits();
        }
        func(inline.as_ptr(), argc)
    } else {
        let mut arg_bits = Vec::with_capacity(args.len());
        for arg in args {
            arg_bits.push(arg.to_jit_bits());
        }
        func(arg_bits.as_ptr(), argc)
    };

    if result == BAILOUT_SENTINEL {
        let deoptimized = function.record_bailout(DEOPT_THRESHOLD);

        let mut state = lock_state();
        state.total_bailouts = state.total_bailouts.saturating_add(1);

        if deoptimized {
            function.clear_jit_entry_ptr();
            state
                .compiled_code_ptrs
                .remove(&(module_id, function_index));
            state.total_deoptimizations = state.total_deoptimizations.saturating_add(1);
        }

        JitExecResult::Bailout
    } else {
        if stats_enabled {
            let mut state = lock_state();
            state.execute_hits = state.execute_hits.saturating_add(1);
        }
        JitExecResult::Ok(result)
    }
}

/// Invalidate JIT code for a specific function.
#[allow(dead_code)]
pub(crate) fn invalidate_jit_code(module_id: u64, function_index: u32) {
    let mut state = runtime_state()
        .lock()
        .expect("jit runtime mutex should not be poisoned");
    state
        .compiled_code_ptrs
        .remove(&(module_id, function_index));
}

/// Compile one pending JIT request, if present.
pub(crate) fn compile_one_pending_request() {
    if !is_jit_enabled() {
        return;
    }

    let Some(request) = jit_queue::pop_next_request() else {
        return;
    };

    if request.function.is_deoptimized() {
        return;
    }

    let mut state = runtime_state()
        .lock()
        .expect("jit runtime mutex should not be poisoned");
    if is_jit_stats_enabled() {
        state.compile_requests = state.compile_requests.saturating_add(1);
    }

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
    match compiler.compile_function_with_constants(&request.function, &request.constants) {
        Ok(artifact) => {
            let code_ptr = artifact.code_ptr as usize;
            request.function.set_jit_entry_ptr(code_ptr);
            state
                .compiled_code_ptrs
                .insert((request.module_id, request.function_index), code_ptr);
            if is_jit_stats_enabled() {
                state.compile_successes = state.compile_successes.saturating_add(1);
            }
        }
        Err(_) => {
            state.compile_errors = state.compile_errors.saturating_add(1);
        }
    }
}

pub(crate) fn stats_snapshot() -> JitRuntimeStats {
    let state = lock_state();
    JitRuntimeStats {
        compile_requests: state.compile_requests,
        compile_successes: state.compile_successes,
        compile_errors: state.compile_errors,
        execute_attempts: state.execute_attempts,
        execute_hits: state.execute_hits,
        execute_not_compiled: state.execute_not_compiled,
        execute_bailouts: state.total_bailouts,
        deoptimizations: state.total_deoptimizations,
        compiled_functions: state.compiled_code_ptrs.len() as u64,
    }
}

/// Directly compile and register a function (bypasses queue).
/// Used for testing and eager compilation.
#[cfg(test)]
fn compile_and_register(module_id: u64, function_index: u32, function: &Function) -> bool {
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
            function.set_jit_entry_ptr(artifact.code_ptr as usize);
            state
                .compiled_code_ptrs
                .insert((module_id, function_index), artifact.code_ptr as usize);
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, OnceLock};

    use otter_vm_bytecode::{Function, Instruction, LocalIndex, Module, Register};

    use super::*;
    use crate::value::Value;

    fn boxed_i32(n: i32) -> i64 {
        0x7FF8_0001_0000_0000_u64 as i64 | ((n as u32) as i64)
    }

    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .expect("jit_runtime test lock should not be poisoned")
    }

    #[test]
    fn compile_one_pending_request_consumes_queue() {
        let _guard = test_lock();
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
        let module_id = module.module_id;
        let state = runtime_state().lock().unwrap();
        assert!(state.compiled_code_ptrs.contains_key(&(module_id, 0)));
    }

    #[test]
    fn try_execute_not_compiled() {
        let _guard = test_lock();
        let f = Function::builder()
            .name("unknown")
            .register_count(1)
            .instruction(Instruction::Return { src: Register(0) })
            .build();

        // Use a unique key that can't collide
        match try_execute_jit(0xCAFE_0001_u64, 99, &f, &[]) {
            JitExecResult::NotCompiled => {}
            _ => panic!("expected NotCompiled"),
        }
    }

    #[test]
    fn try_execute_success() {
        let _guard = test_lock();
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

        let key = 0xBEEF_0001_u64;
        assert!(compile_and_register(key, 0, &f));

        match try_execute_jit(key, 0, &f, &[]) {
            JitExecResult::Ok(val) => assert_eq!(val, boxed_i32(30)),
            _ => panic!("expected boxed int32(30)"),
        }
    }

    #[test]
    fn try_execute_success_with_arguments() {
        let _guard = test_lock();
        let f = Function::builder()
            .name("add_one")
            .param_count(1)
            .local_count(1)
            .register_count(3)
            .instruction(Instruction::GetLocal {
                dst: Register(0),
                idx: LocalIndex(0),
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 1,
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

        let key = 0xBEEF_0010_u64;
        assert!(compile_and_register(key, 0, &f));

        let args = [Value::int32(41)];
        match try_execute_jit(key, 0, &f, &args) {
            JitExecResult::Ok(val) => assert_eq!(val, boxed_i32(42)),
            _ => panic!("expected boxed int32(42)"),
        }
    }

    #[test]
    fn bailout_on_div_by_zero() {
        let _guard = test_lock();
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

        let key = 0xBEEF_0002_u64;
        assert!(compile_and_register(key, 0, &f));

        match try_execute_jit(key, 0, &f, &[]) {
            JitExecResult::Bailout => {}
            _ => panic!("expected Bailout on div-by-zero"),
        }

        assert_eq!(f.get_bailout_count(), 1);
        assert!(!f.is_deoptimized());
    }

    #[test]
    fn deoptimize_after_threshold_bailouts() {
        let _guard = test_lock();
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

        let key = 0xBEEF_0003_u64;
        assert!(compile_and_register(key, 0, &f));

        // Bail out until deoptimized
        for _ in 0..DEOPT_THRESHOLD {
            match try_execute_jit(key, 0, &f, &[]) {
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
        match try_execute_jit(key, 0, &f, &[]) {
            JitExecResult::NotCompiled => {}
            _ => panic!("expected NotCompiled after deoptimization"),
        }
    }

    #[test]
    fn deoptimized_function_not_recompiled() {
        let _guard = test_lock();
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
        let module_id = module.module_id;
        let state = runtime_state().lock().unwrap();
        assert!(
            !state.compiled_code_ptrs.contains_key(&(module_id, 0)),
            "deoptimized function should not be compiled"
        );
    }
}
