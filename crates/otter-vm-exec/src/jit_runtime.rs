use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Mutex, OnceLock};
use std::thread;

use otter_vm_bytecode::Function;
use otter_vm_bytecode::function::HOT_FUNCTION_THRESHOLD;
use otter_vm_jit::runtime_helpers::RuntimeHelpers;
use otter_vm_jit::{BAILOUT_SENTINEL, DEOPT_THRESHOLD, JitCompiler};

use crate::jit_queue::{self, JitCompileRequest};

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
    /// Keyed by `(module_id, function_index)`.
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
static JIT_BACKGROUND_ENABLED: OnceLock<bool> = OnceLock::new();
static JIT_HOT_THRESHOLD: OnceLock<u32> = OnceLock::new();
static JIT_DEOPT_THRESHOLD: OnceLock<u32> = OnceLock::new();
static JIT_BACKGROUND_WORKER: OnceLock<Option<BackgroundCompileWorker>> = OnceLock::new();

struct BackgroundCompileWorker {
    request_tx: Sender<JitCompileRequest>,
    result_rx: Mutex<Receiver<BackgroundCompileResult>>,
}

enum BackgroundCompileResult {
    Compiled {
        module_id: u64,
        function_index: u32,
        code_ptr: usize,
    },
    Error {
        module_id: u64,
        function_index: u32,
    },
}

fn runtime_state() -> &'static Mutex<JitRuntimeState> {
    JIT_RUNTIME_STATE.get_or_init(|| Mutex::new(JitRuntimeState::default()))
}

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

fn parse_env_u32(var_name: &str) -> Option<u32> {
    std::env::var(var_name)
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
}

/// Check whether JIT is enabled via environment flags.
pub fn is_jit_enabled() -> bool {
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

/// Check whether eager JIT mode is enabled.
pub fn is_jit_eager_enabled() -> bool {
    *JIT_EAGER_ENABLED.get_or_init(|| {
        std::env::var("OTTER_JIT_EAGER")
            .ok()
            .is_some_and(|v| parse_env_truthy(&v))
    })
}

/// Check whether background JIT compilation is enabled.
///
/// Enabled by default. Set `OTTER_JIT_BACKGROUND=0` to force synchronous
/// compilation on the VM thread.
pub fn is_jit_background_enabled() -> bool {
    *JIT_BACKGROUND_ENABLED.get_or_init(|| {
        std::env::var("OTTER_JIT_BACKGROUND")
            .ok()
            .map(|v| parse_env_truthy(&v))
            .unwrap_or(true)
    })
}

/// Hot-call threshold used to mark functions as JIT candidates.
///
/// Defaults to `HOT_FUNCTION_THRESHOLD` (1000).
/// Override with `OTTER_JIT_HOT_THRESHOLD=<u32>`.
pub fn jit_hot_threshold() -> u32 {
    *JIT_HOT_THRESHOLD.get_or_init(|| {
        parse_env_u32("OTTER_JIT_HOT_THRESHOLD")
            .filter(|threshold| *threshold > 0)
            .unwrap_or(HOT_FUNCTION_THRESHOLD)
    })
}

/// Bailout threshold before triggering JIT recompilation/deopt handling.
///
/// Defaults to `DEOPT_THRESHOLD` from `otter-vm-jit`.
/// Override with `OTTER_JIT_DEOPT_THRESHOLD=<u32>`.
pub fn jit_deopt_threshold() -> u32 {
    *JIT_DEOPT_THRESHOLD.get_or_init(|| {
        parse_env_u32("OTTER_JIT_DEOPT_THRESHOLD")
            .filter(|threshold| *threshold > 0)
            .unwrap_or(DEOPT_THRESHOLD)
    })
}

fn background_worker(helpers: &RuntimeHelpers) -> Option<&'static BackgroundCompileWorker> {
    if !is_jit_background_enabled() {
        return None;
    }

    let worker = JIT_BACKGROUND_WORKER.get_or_init(|| {
        let (request_tx, request_rx) = mpsc::channel::<JitCompileRequest>();
        let (result_tx, result_rx) = mpsc::channel::<BackgroundCompileResult>();
        let worker_helpers = helpers.clone();

        let spawn_result = thread::Builder::new()
            .name("otter-jit-bg".to_string())
            .spawn(move || run_background_worker(request_rx, result_tx, worker_helpers));

        match spawn_result {
            Ok(_) => Some(BackgroundCompileWorker {
                request_tx,
                result_rx: Mutex::new(result_rx),
            }),
            Err(_) => None,
        }
    });

    worker.as_ref()
}

fn run_background_worker(
    request_rx: Receiver<JitCompileRequest>,
    result_tx: Sender<BackgroundCompileResult>,
    helpers: RuntimeHelpers,
) {
    let mut compiler: Option<JitCompiler> = None;

    for request in request_rx {
        let module_id = request.module_id;
        let function_index = request.function_index;

        if request.function.is_deoptimized() {
            let _ = result_tx.send(BackgroundCompileResult::Error {
                module_id,
                function_index,
            });
            continue;
        }

        if compiler.is_none() {
            match JitCompiler::new_with_helpers(helpers.clone()) {
                Ok(instance) => compiler = Some(instance),
                Err(_) => {
                    let _ = result_tx.send(BackgroundCompileResult::Error {
                        module_id,
                        function_index,
                    });
                    continue;
                }
            }
        }

        let compile_result = compiler
            .as_mut()
            .expect("jit compiler should be initialized")
            .compile_function_with_constants(&request.function, &request.constants);

        match compile_result {
            Ok(artifact) => {
                let _ = result_tx.send(BackgroundCompileResult::Compiled {
                    module_id,
                    function_index,
                    code_ptr: artifact.code_ptr as usize,
                });
            }
            Err(_) => {
                let _ = result_tx.send(BackgroundCompileResult::Error {
                    module_id,
                    function_index,
                });
            }
        }
    }
}

fn drain_background_results() {
    let Some(worker) = JIT_BACKGROUND_WORKER.get().and_then(|w| w.as_ref()) else {
        return;
    };

    loop {
        let recv_result = {
            let receiver = worker
                .result_rx
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            receiver.try_recv()
        };

        match recv_result {
            Ok(BackgroundCompileResult::Compiled {
                module_id,
                function_index,
                code_ptr,
            }) => {
                let mut state = lock_state();
                state
                    .compiled_code_ptrs
                    .insert((module_id, function_index), code_ptr);
                if is_jit_stats_enabled() {
                    state.compile_successes = state.compile_successes.saturating_add(1);
                }
                drop(state);
                jit_queue::mark_request_finished(module_id, function_index);
            }
            Ok(BackgroundCompileResult::Error {
                module_id,
                function_index,
            }) => {
                let mut state = lock_state();
                state.compile_errors = state.compile_errors.saturating_add(1);
                drop(state);
                jit_queue::mark_request_finished(module_id, function_index);
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
        }
    }
}

fn compile_request_sync(request: JitCompileRequest, helpers: &RuntimeHelpers) {
    let mut state = runtime_state()
        .lock()
        .expect("jit runtime mutex should not be poisoned");

    if state.compiler.is_none() {
        match JitCompiler::new_with_helpers(helpers.clone()) {
            Ok(compiler) => state.compiler = Some(compiler),
            Err(_) => {
                state.compile_errors = state.compile_errors.saturating_add(1);
                drop(state);
                jit_queue::mark_request_finished(request.module_id, request.function_index);
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

    drop(state);
    jit_queue::mark_request_finished(request.module_id, request.function_index);
}

/// Result of attempting to execute JIT-compiled code.
#[derive(Debug)]
pub enum JitExecResult {
    /// JIT code ran successfully, returning NaN-boxed bits.
    Ok(i64),
    /// JIT code bailed out — caller should re-execute in interpreter.
    Bailout,
    /// No JIT code available for this function.
    NotCompiled,
    /// JIT code bailed out and the function should be recompiled.
    NeedsRecompilation,
}

/// Execute JIT code via a raw context pointer.
///
/// `ctx_ptr` must point to the caller-defined JIT context struct matching
/// helper ABI expected by generated code.
pub fn try_execute_jit_raw(
    module_id: u64,
    function_index: u32,
    function: &Function,
    argc: u32,
    args_ptr: *const i64,
    ctx_ptr: *mut u8,
) -> JitExecResult {
    if !is_jit_enabled() {
        return JitExecResult::NotCompiled;
    }

    drain_background_results();

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

    // SAFETY: ptr is produced by JitCompiler with signature
    // `(*mut u8, *const i64, u32) -> i64`.
    let func: extern "C" fn(*mut u8, *const i64, u32) -> i64 = unsafe { std::mem::transmute(ptr) };
    let result = func(ctx_ptr, args_ptr, argc);

    if result == BAILOUT_SENTINEL {
        use otter_vm_bytecode::function::BailoutAction;

        let action = function.record_bailout(jit_deopt_threshold());

        let mut state = lock_state();
        state.total_bailouts = state.total_bailouts.saturating_add(1);

        let needs_recompile = match action {
            BailoutAction::PermanentDeopt => {
                state
                    .compiled_code_ptrs
                    .remove(&(module_id, function_index));
                state.total_deoptimizations = state.total_deoptimizations.saturating_add(1);
                false
            }
            BailoutAction::Recompile => {
                state
                    .compiled_code_ptrs
                    .remove(&(module_id, function_index));
                true
            }
            BailoutAction::Continue => false,
        };

        if needs_recompile {
            JitExecResult::NeedsRecompilation
        } else {
            JitExecResult::Bailout
        }
    } else {
        if stats_enabled {
            let mut state = lock_state();
            state.execute_hits = state.execute_hits.saturating_add(1);
        }
        JitExecResult::Ok(result)
    }
}

/// Invalidate cached compiled code pointer for a function.
pub fn invalidate_jit_code(module_id: u64, function_index: u32) {
    let mut state = runtime_state()
        .lock()
        .expect("jit runtime mutex should not be poisoned");
    state
        .compiled_code_ptrs
        .remove(&(module_id, function_index));
}

/// Compile one pending JIT request using the provided helper table.
pub fn compile_one_pending_request(helpers: &RuntimeHelpers) {
    if !is_jit_enabled() {
        return;
    }

    drain_background_results();

    let Some(request) = jit_queue::pop_next_request() else {
        return;
    };

    if request.function.is_deoptimized() {
        jit_queue::mark_request_finished(request.module_id, request.function_index);
        return;
    }

    if is_jit_stats_enabled() {
        let mut state = runtime_state()
            .lock()
            .expect("jit runtime mutex should not be poisoned");
        state.compile_requests = state.compile_requests.saturating_add(1);
    }

    if let Some(worker) = background_worker(helpers) {
        match worker.request_tx.send(request) {
            Ok(()) => return,
            Err(err) => {
                compile_request_sync(err.0, helpers);
                return;
            }
        }
    }

    compile_request_sync(request, helpers);
}

/// Snapshot runtime JIT counters.
pub fn stats_snapshot() -> JitRuntimeStats {
    if is_jit_enabled() {
        drain_background_results();
    }
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

#[cfg(test)]
fn clear_runtime_state_for_tests() {
    {
        let mut state = lock_state();
        state.compiler = None;
        state.compiled_code_ptrs.clear();
        state.compile_errors = 0;
        state.compile_requests = 0;
        state.compile_successes = 0;
        state.execute_attempts = 0;
        state.execute_hits = 0;
        state.execute_not_compiled = 0;
        state.total_bailouts = 0;
        state.total_deoptimizations = 0;
    }

    drain_background_results();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use otter_vm_bytecode::{Instruction, Module, Register};

    fn build_test_module() -> Arc<Module> {
        let function = Function::builder()
            .name("jit_runtime_bg_compile")
            .register_count(1)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 11,
            })
            .instruction(Instruction::Return { src: Register(0) })
            .build();

        let mut builder = Module::builder("jit-runtime-bg.js");
        let entry = builder.add_function(function);
        Arc::new(builder.entry_point(entry).build())
    }

    #[test]
    fn enqueue_compile_and_execute_pipeline_compiles_function() {
        let _guard = crate::test_lock();
        crate::clear_for_tests();
        clear_runtime_state_for_tests();

        let helpers = RuntimeHelpers::new();
        let module = build_test_module();
        let function = module
            .function(0)
            .expect("test module should expose function");

        assert!(crate::enqueue_hot_function(&module, 0, function));
        compile_one_pending_request(&helpers);

        let args: [i64; 0] = [];
        let mut saw_compiled = false;
        for _ in 0..100 {
            match try_execute_jit_raw(
                module.module_id,
                0,
                function,
                0,
                args.as_ptr(),
                std::ptr::null_mut(),
            ) {
                JitExecResult::Ok(bits) => {
                    assert_ne!(bits, BAILOUT_SENTINEL);
                    saw_compiled = true;
                    break;
                }
                JitExecResult::NotCompiled | JitExecResult::Bailout => {
                    compile_one_pending_request(&helpers);
                    thread::sleep(Duration::from_millis(5));
                }
                JitExecResult::NeedsRecompilation => {
                    panic!("unexpected recompile request for constant-return function");
                }
            }
        }

        assert!(
            saw_compiled,
            "expected function to become executable after JIT compilation"
        );

        crate::clear_for_tests();
        clear_runtime_state_for_tests();
    }
}
