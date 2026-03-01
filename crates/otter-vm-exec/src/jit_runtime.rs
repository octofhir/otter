use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Mutex, OnceLock};
use std::thread;

use otter_vm_bytecode::Function;
use otter_vm_bytecode::function::HOT_FUNCTION_THRESHOLD;
use otter_vm_jit::runtime_helpers::{
    JIT_CTX_BAILOUT_PC_OFFSET, JIT_CTX_BAILOUT_REASON_OFFSET, RuntimeHelpers,
};
use otter_vm_jit::{BAILOUT_SENTINEL, BailoutReason, DEOPT_THRESHOLD, DeoptMetadata, JitCompiler};

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
    /// Bailouts with unknown reason.
    pub execute_bailouts_unknown: u64,
    /// Bailouts from helper-returned sentinel.
    pub execute_bailouts_helper: u64,
    /// Bailouts from type/speculation guard failure.
    pub execute_bailouts_type_guard: u64,
    /// Number of functions deoptimized after repeated bailouts.
    pub deoptimizations: u64,
    /// Current number of compiled functions cached in runtime state.
    pub compiled_functions: u64,
    /// Last bailout module id seen by runtime telemetry.
    pub last_bailout_module_id: Option<u64>,
    /// Last bailout function index seen by runtime telemetry.
    pub last_bailout_function_index: Option<u32>,
    /// Last bailout bytecode pc seen by runtime telemetry.
    pub last_bailout_pc: Option<u32>,
    /// Last bailout opcode name seen by runtime telemetry.
    pub last_bailout_opcode: Option<&'static str>,
    /// Last bailout reason category seen by runtime telemetry.
    pub last_bailout_reason: BailoutReason,
    /// Number of distinct `(module, function, pc)` bailout sites observed.
    pub bailout_sites_observed: u64,
    /// Hottest observed bailout site.
    pub top_bailout_site_1: Option<JitBailoutSiteStat>,
    /// Second hottest observed bailout site.
    pub top_bailout_site_2: Option<JitBailoutSiteStat>,
    /// Third hottest observed bailout site.
    pub top_bailout_site_3: Option<JitBailoutSiteStat>,
    /// Number of JIT compilations triggered by back-edge counting (hot loops).
    pub back_edge_compilations: u64,
    /// Number of OSR (on-stack replacement) attempts.
    pub osr_attempts: u64,
    /// Number of successful OSR restarts.
    pub osr_successes: u64,
}

#[derive(Debug, Clone, Copy, Default)]
/// Aggregated counter for a specific bytecode bailout site.
pub struct JitBailoutSiteStat {
    /// Module id.
    pub module_id: u64,
    /// Function index inside the module.
    pub function_index: u32,
    /// Bytecode program counter.
    pub pc: u32,
    /// Opcode at this bytecode site.
    pub opcode: &'static str,
    /// Number of bailouts observed for this site.
    pub count: u64,
}

/// Resume mode selected for interpreter re-entry after JIT deopt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeoptResumeMode {
    /// Fall back to restarting function execution from bytecode pc 0.
    #[default]
    RestartFunction,
    /// Metadata indicates a mapped deopt site; interpreter may resume at `bailout_pc`.
    ResumeAtPc,
}

/// Minimal deopt frame snapshot used by the interpreter re-entry path.
///
/// This intentionally contains only plain scalar metadata (no GC-managed
/// values/handles), so capturing/storing it cannot violate rooting invariants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DeoptFrameSnapshot {
    /// Module id of the function that deoptimized.
    pub module_id: u64,
    /// Function index of the function that deoptimized.
    pub function_index: u32,
    /// Deopt reason reported by JIT-side telemetry.
    pub reason: BailoutReason,
    /// Bytecode pc where deopt happened, if available.
    pub bailout_pc: Option<u32>,
    /// Suggested interpreter resume mode.
    pub resume_mode: DeoptResumeMode,
}

// Compile-time GC safety assertions: deopt types must be Send + Sync
// (impossible if they contained GcRef which is !Send + !Sync).
const _: () = {
    fn assert_send_sync<T: Send + Sync>() {}
    fn check() {
        assert_send_sync::<DeoptFrameSnapshot>();
        assert_send_sync::<DeoptResumeMode>();
    }
};

#[derive(Debug, Clone, Copy)]
struct BailoutTelemetry {
    reason: BailoutReason,
    pc: Option<u32>,
}

impl Default for BailoutTelemetry {
    fn default() -> Self {
        Self {
            reason: BailoutReason::Unknown,
            pc: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct BailoutSiteCounter {
    count: u64,
    opcode: &'static str,
}

#[derive(Debug, Clone)]
struct CompiledFunctionEntry {
    code_ptr: usize,
    deopt_metadata: DeoptMetadata,
}

#[derive(Default)]
struct JitRuntimeState {
    compiler: Option<JitCompiler>,
    /// Keyed by `(module_id, function_index)`.
    compiled_entries: HashMap<(u64, u32), CompiledFunctionEntry>,
    compile_errors: u64,
    compile_requests: u64,
    compile_successes: u64,
    execute_attempts: u64,
    execute_hits: u64,
    execute_not_compiled: u64,
    total_bailouts: u64,
    total_deoptimizations: u64,
    bailout_unknown: u64,
    bailout_helper: u64,
    bailout_type_guard: u64,
    bailout_site_counts: HashMap<(u64, u32, u32), BailoutSiteCounter>,
    last_bailout_module_id: Option<u64>,
    last_bailout_function_index: Option<u32>,
    last_bailout_opcode: Option<&'static str>,
    last_bailout: BailoutTelemetry,
    back_edge_compilations: u64,
    osr_attempts: u64,
    osr_successes: u64,
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
        deopt_metadata: DeoptMetadata,
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

fn decode_bailout_telemetry(ctx_ptr: *mut u8) -> BailoutTelemetry {
    if ctx_ptr.is_null() {
        return BailoutTelemetry::default();
    }

    let reason = unsafe {
        let ptr = ctx_ptr
            .add(JIT_CTX_BAILOUT_REASON_OFFSET as usize)
            .cast::<i64>();
        BailoutReason::from_code(*ptr)
    };

    let pc = unsafe {
        let ptr = ctx_ptr
            .add(JIT_CTX_BAILOUT_PC_OFFSET as usize)
            .cast::<i64>();
        let raw = *ptr;
        if raw >= 0 && raw <= u32::MAX as i64 {
            Some(raw as u32)
        } else {
            None
        }
    };

    BailoutTelemetry { reason, pc }
}

fn make_deopt_snapshot(
    module_id: u64,
    function_index: u32,
    telemetry: BailoutTelemetry,
    has_mapped_site: bool,
) -> DeoptFrameSnapshot {
    let resume_mode = if telemetry.pc.is_some() && has_mapped_site {
        DeoptResumeMode::ResumeAtPc
    } else {
        DeoptResumeMode::RestartFunction
    };

    DeoptFrameSnapshot {
        module_id,
        function_index,
        reason: telemetry.reason,
        bailout_pc: telemetry.pc,
        resume_mode,
    }
}

fn opcode_name_at_pc(function: &Function, pc: u32) -> Option<&'static str> {
    function
        .instructions
        .read()
        .get(pc as usize)
        .map(opcode_name_from_instruction)
}

fn opcode_name_from_instruction(instruction: &otter_vm_bytecode::Instruction) -> &'static str {
    match instruction {
        otter_vm_bytecode::Instruction::LoadUndefined { .. } => "LoadUndefined",
        otter_vm_bytecode::Instruction::LoadNull { .. } => "LoadNull",
        otter_vm_bytecode::Instruction::LoadTrue { .. } => "LoadTrue",
        otter_vm_bytecode::Instruction::LoadFalse { .. } => "LoadFalse",
        otter_vm_bytecode::Instruction::LoadInt8 { .. } => "LoadInt8",
        otter_vm_bytecode::Instruction::LoadInt32 { .. } => "LoadInt32",
        otter_vm_bytecode::Instruction::LoadConst { .. } => "LoadConst",
        otter_vm_bytecode::Instruction::GetLocal { .. } => "GetLocal",
        otter_vm_bytecode::Instruction::SetLocal { .. } => "SetLocal",
        otter_vm_bytecode::Instruction::Move { .. } => "Move",
        otter_vm_bytecode::Instruction::Add { .. } => "Add",
        otter_vm_bytecode::Instruction::Sub { .. } => "Sub",
        otter_vm_bytecode::Instruction::Mul { .. } => "Mul",
        otter_vm_bytecode::Instruction::Div { .. } => "Div",
        otter_vm_bytecode::Instruction::Mod { .. } => "Mod",
        otter_vm_bytecode::Instruction::Neg { .. } => "Neg",
        otter_vm_bytecode::Instruction::Inc { .. } => "Inc",
        otter_vm_bytecode::Instruction::Dec { .. } => "Dec",
        otter_vm_bytecode::Instruction::BitAnd { .. } => "BitAnd",
        otter_vm_bytecode::Instruction::BitOr { .. } => "BitOr",
        otter_vm_bytecode::Instruction::BitXor { .. } => "BitXor",
        otter_vm_bytecode::Instruction::BitNot { .. } => "BitNot",
        otter_vm_bytecode::Instruction::Shl { .. } => "Shl",
        otter_vm_bytecode::Instruction::Shr { .. } => "Shr",
        otter_vm_bytecode::Instruction::Ushr { .. } => "Ushr",
        otter_vm_bytecode::Instruction::Eq { .. } => "Eq",
        otter_vm_bytecode::Instruction::StrictEq { .. } => "StrictEq",
        otter_vm_bytecode::Instruction::Ne { .. } => "Ne",
        otter_vm_bytecode::Instruction::StrictNe { .. } => "StrictNe",
        otter_vm_bytecode::Instruction::Lt { .. } => "Lt",
        otter_vm_bytecode::Instruction::Le { .. } => "Le",
        otter_vm_bytecode::Instruction::Gt { .. } => "Gt",
        otter_vm_bytecode::Instruction::Ge { .. } => "Ge",
        otter_vm_bytecode::Instruction::Not { .. } => "Not",
        _ => "Other",
    }
}

fn push_top_bailout_site(top: &mut [Option<JitBailoutSiteStat>; 3], candidate: JitBailoutSiteStat) {
    for idx in 0..top.len() {
        match top[idx] {
            Some(existing) if existing.count >= candidate.count => continue,
            _ => {
                for shift in (idx + 1..top.len()).rev() {
                    top[shift] = top[shift - 1];
                }
                top[idx] = Some(candidate);
                break;
            }
        }
    }
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
            .compile_function_with_constants_and_metadata(&request.function, &request.constants);

        match compile_result {
            Ok((artifact, deopt_metadata)) => {
                let _ = result_tx.send(BackgroundCompileResult::Compiled {
                    module_id,
                    function_index,
                    code_ptr: artifact.code_ptr as usize,
                    deopt_metadata,
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
                deopt_metadata,
            }) => {
                let mut state = lock_state();
                state.compiled_entries.insert(
                    (module_id, function_index),
                    CompiledFunctionEntry {
                        code_ptr,
                        deopt_metadata,
                    },
                );
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

    match compiler
        .compile_function_with_constants_and_metadata(&request.function, &request.constants)
    {
        Ok((artifact, deopt_metadata)) => {
            let code_ptr = artifact.code_ptr as usize;
            request.function.set_jit_entry_ptr(code_ptr);
            state.compiled_entries.insert(
                (request.module_id, request.function_index),
                CompiledFunctionEntry {
                    code_ptr,
                    deopt_metadata,
                },
            );
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
    Bailout(DeoptFrameSnapshot),
    /// No JIT code available for this function.
    NotCompiled,
    /// JIT code bailed out and the function should be recompiled.
    NeedsRecompilation(DeoptFrameSnapshot),
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
                .compiled_entries
                .get(&(module_id, function_index))
                .map(|entry| entry.code_ptr)
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

        let telemetry = decode_bailout_telemetry(ctx_ptr);
        let action = function.record_bailout(jit_deopt_threshold());

        let mut state = lock_state();
        state.total_bailouts = state.total_bailouts.saturating_add(1);
        match telemetry.reason {
            BailoutReason::Unknown => {
                state.bailout_unknown = state.bailout_unknown.saturating_add(1);
            }
            BailoutReason::HelperReturnedSentinel => {
                state.bailout_helper = state.bailout_helper.saturating_add(1);
            }
            BailoutReason::TypeGuardFailure => {
                state.bailout_type_guard = state.bailout_type_guard.saturating_add(1);
            }
        }
        let has_mapped_site = telemetry.pc.is_some_and(|pc| {
            state
                .compiled_entries
                .get(&(module_id, function_index))
                .is_some_and(|entry| entry.deopt_metadata.has_site(pc))
        });
        if let Some(pc) = telemetry.pc {
            let key = (module_id, function_index, pc);
            let opcode = state
                .compiled_entries
                .get(&(module_id, function_index))
                .and_then(|entry| {
                    if entry.deopt_metadata.has_site(pc) {
                        opcode_name_at_pc(function, pc)
                    } else {
                        None
                    }
                })
                .unwrap_or("unknown");
            let counter = state
                .bailout_site_counts
                .entry(key)
                .or_insert(BailoutSiteCounter { count: 0, opcode });
            counter.count = counter.count.saturating_add(1);
        }
        state.last_bailout_module_id = Some(module_id);
        state.last_bailout_function_index = Some(function_index);
        state.last_bailout_opcode = telemetry.pc.and_then(|pc| opcode_name_at_pc(function, pc));
        state.last_bailout = telemetry;
        let snapshot = make_deopt_snapshot(module_id, function_index, telemetry, has_mapped_site);

        let needs_recompile = match action {
            BailoutAction::PermanentDeopt => {
                state.compiled_entries.remove(&(module_id, function_index));
                state.total_deoptimizations = state.total_deoptimizations.saturating_add(1);
                false
            }
            BailoutAction::Recompile => {
                state.compiled_entries.remove(&(module_id, function_index));
                true
            }
            BailoutAction::Continue => false,
        };

        if needs_recompile {
            JitExecResult::NeedsRecompilation(snapshot)
        } else {
            JitExecResult::Bailout(snapshot)
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
    state.compiled_entries.remove(&(module_id, function_index));
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
    let mut top_sites: [Option<JitBailoutSiteStat>; 3] = [None, None, None];
    for (&(module_id, function_index, pc), counter) in &state.bailout_site_counts {
        push_top_bailout_site(
            &mut top_sites,
            JitBailoutSiteStat {
                module_id,
                function_index,
                pc,
                opcode: counter.opcode,
                count: counter.count,
            },
        );
    }

    JitRuntimeStats {
        compile_requests: state.compile_requests,
        compile_successes: state.compile_successes,
        compile_errors: state.compile_errors,
        execute_attempts: state.execute_attempts,
        execute_hits: state.execute_hits,
        execute_not_compiled: state.execute_not_compiled,
        execute_bailouts: state.total_bailouts,
        execute_bailouts_unknown: state.bailout_unknown,
        execute_bailouts_helper: state.bailout_helper,
        execute_bailouts_type_guard: state.bailout_type_guard,
        deoptimizations: state.total_deoptimizations,
        compiled_functions: state.compiled_entries.len() as u64,
        last_bailout_module_id: state.last_bailout_module_id,
        last_bailout_function_index: state.last_bailout_function_index,
        last_bailout_pc: state.last_bailout.pc,
        last_bailout_opcode: state.last_bailout_opcode,
        last_bailout_reason: state.last_bailout.reason,
        bailout_sites_observed: state.bailout_site_counts.len() as u64,
        top_bailout_site_1: top_sites[0],
        top_bailout_site_2: top_sites[1],
        top_bailout_site_3: top_sites[2],
        back_edge_compilations: state.back_edge_compilations,
        osr_attempts: state.osr_attempts,
        osr_successes: state.osr_successes,
    }
}

/// Record that a JIT compilation was triggered by back-edge counting.
pub fn record_back_edge_compilation() {
    if !is_jit_stats_enabled() {
        return;
    }
    let mut state = lock_state();
    state.back_edge_compilations = state.back_edge_compilations.saturating_add(1);
}

/// Record an OSR (on-stack replacement) attempt.
pub fn record_osr_attempt() {
    if !is_jit_stats_enabled() {
        return;
    }
    let mut state = lock_state();
    state.osr_attempts = state.osr_attempts.saturating_add(1);
}

/// Record a successful OSR restart.
pub fn record_osr_success() {
    if !is_jit_stats_enabled() {
        return;
    }
    let mut state = lock_state();
    state.osr_successes = state.osr_successes.saturating_add(1);
}

/// Return deopt metadata for a compiled function, if available.
pub fn deopt_metadata_snapshot(module_id: u64, function_index: u32) -> Option<DeoptMetadata> {
    if is_jit_enabled() {
        drain_background_results();
    }
    let state = lock_state();
    state
        .compiled_entries
        .get(&(module_id, function_index))
        .map(|entry| entry.deopt_metadata.clone())
}

#[cfg(test)]
fn clear_runtime_state_for_tests() {
    {
        let mut state = lock_state();
        state.compiler = None;
        state.compiled_entries.clear();
        state.compile_errors = 0;
        state.compile_requests = 0;
        state.compile_successes = 0;
        state.execute_attempts = 0;
        state.execute_hits = 0;
        state.execute_not_compiled = 0;
        state.total_bailouts = 0;
        state.total_deoptimizations = 0;
        state.bailout_unknown = 0;
        state.bailout_helper = 0;
        state.bailout_type_guard = 0;
        state.bailout_site_counts.clear();
        state.last_bailout_module_id = None;
        state.last_bailout_function_index = None;
        state.last_bailout_opcode = None;
        state.last_bailout = BailoutTelemetry::default();
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
    use otter_vm_jit::runtime_helpers::{JIT_CTX_BAILOUT_PC_OFFSET, JIT_CTX_BAILOUT_REASON_OFFSET};

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
    fn decode_bailout_telemetry_from_null_ctx_is_unknown() {
        let telemetry = decode_bailout_telemetry(std::ptr::null_mut());
        assert_eq!(telemetry.reason, BailoutReason::Unknown);
        assert_eq!(telemetry.pc, None);
    }

    #[test]
    fn decode_bailout_telemetry_reads_reason_and_pc_offsets() {
        let mut raw = [0_u8; 128];
        let ctx = raw.as_mut_ptr();
        unsafe {
            let reason_ptr = ctx
                .add(JIT_CTX_BAILOUT_REASON_OFFSET as usize)
                .cast::<i64>();
            *reason_ptr = BailoutReason::TypeGuardFailure.code();

            let pc_ptr = ctx.add(JIT_CTX_BAILOUT_PC_OFFSET as usize).cast::<i64>();
            *pc_ptr = 42;
        }

        let telemetry = decode_bailout_telemetry(ctx);
        assert_eq!(telemetry.reason, BailoutReason::TypeGuardFailure);
        assert_eq!(telemetry.pc, Some(42));
    }

    #[test]
    fn make_deopt_snapshot_selects_resume_mode_from_metadata_mapping() {
        let telemetry = BailoutTelemetry {
            reason: BailoutReason::TypeGuardFailure,
            pc: Some(17),
        };
        let resume = make_deopt_snapshot(1, 2, telemetry, true);
        assert_eq!(resume.resume_mode, DeoptResumeMode::ResumeAtPc);

        let fallback = make_deopt_snapshot(1, 2, telemetry, false);
        assert_eq!(fallback.resume_mode, DeoptResumeMode::RestartFunction);
    }

    #[test]
    fn push_top_bailout_site_keeps_three_highest_counts() {
        let mut top: [Option<JitBailoutSiteStat>; 3] = [None, None, None];

        push_top_bailout_site(
            &mut top,
            JitBailoutSiteStat {
                module_id: 1,
                function_index: 1,
                pc: 10,
                opcode: "Add",
                count: 4,
            },
        );
        push_top_bailout_site(
            &mut top,
            JitBailoutSiteStat {
                module_id: 2,
                function_index: 2,
                pc: 20,
                opcode: "GetPropConst",
                count: 9,
            },
        );
        push_top_bailout_site(
            &mut top,
            JitBailoutSiteStat {
                module_id: 3,
                function_index: 3,
                pc: 30,
                opcode: "Call",
                count: 6,
            },
        );
        push_top_bailout_site(
            &mut top,
            JitBailoutSiteStat {
                module_id: 4,
                function_index: 4,
                pc: 40,
                opcode: "Mul",
                count: 5,
            },
        );

        assert_eq!(top[0].map(|s| s.count), Some(9));
        assert_eq!(top[1].map(|s| s.count), Some(6));
        assert_eq!(top[2].map(|s| s.count), Some(5));
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
                JitExecResult::NotCompiled | JitExecResult::Bailout(_) => {
                    compile_one_pending_request(&helpers);
                    thread::sleep(Duration::from_millis(5));
                }
                JitExecResult::NeedsRecompilation(_) => {
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
