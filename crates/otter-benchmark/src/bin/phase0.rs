//! Focused Phase 0 call and baseline-JIT measurements.
//!
//! # Contents
//! - `call` compiles one generated call workload before timing and validates
//!   every VM execution result.
//! - `jit-compile` compiles one named bytecode function repeatedly and records
//!   median emitter latency plus finalized executable bytes.
//! - `module` records cumulative resolve/load/parse/compile/link/execute times
//!   for validated cold-runtime or warm-runtime module graph runs.
//! - `macro-memory` runs an ordered, unmodified multi-file macro workload in a
//!   CLI-equivalent runtime, forces full GC, and records retained heap/GC/RSS.
//!
//! # Invariants
//! - Parsing and bytecode lowering are outside call execution samples.
//! - JIT snapshot cloning is outside emitter samples.
//! - Every successful sample is semantically validated; compile/runtime
//!   failures emit a non-scoreable JSON record and exit non-zero.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[cfg(feature = "rss")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "rss")]
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use otter_benchmark::{
    BENCHMARK_RESULT_SCHEMA_VERSION, BenchmarkResult, CacheState, ExecutionMetrics, GcMode,
    JitMode, MemoryMetrics, RuntimeMode, ValidationStatus,
};
use otter_compiler::compile_script_source;
use otter_jit::{BaselineJitCompiler, compile};
use otter_modules::OtterModulesBuilderExt;
use otter_node::NodeApiBuilderExt;
use otter_runtime::{
    ConsoleLevel, ConsoleSink, Runtime, RuntimeExecutionStats, SourceInput,
    module_graph::ModulePhaseTimings,
};
use otter_syntax::SourceKind;
use otter_vm::{ExecutionContext, Interpreter, JitFunctionCode};
use otter_web::WebApiBuilderExt;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CallKind {
    Bytecode,
    Host,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CallJitMode {
    InterpreterOnly,
    Baseline,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ModuleCacheState {
    /// Build a fresh runtime for every measured graph execution.
    Cold,
    /// Reuse and pre-execute one runtime before measured graph executions.
    Warm,
}

#[derive(Debug, Default)]
struct CapturingConsoleSink {
    lines: Mutex<Vec<String>>,
}

impl CapturingConsoleSink {
    fn matching_line(&self, marker: &str) -> Option<String> {
        self.lines
            .lock()
            .expect("console capture lock")
            .iter()
            .rev()
            .find(|line| line.contains(marker))
            .cloned()
    }
}

impl ConsoleSink for CapturingConsoleSink {
    fn write(&self, _level: ConsoleLevel, fields: &[String]) {
        self.lines
            .lock()
            .expect("console capture lock")
            .push(fields.join(" "));
    }
}

#[cfg(feature = "rss")]
struct RssSampler {
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<u64>,
}

#[cfg(not(feature = "rss"))]
struct RssSampler;

impl From<ModuleCacheState> for CacheState {
    fn from(value: ModuleCacheState) -> Self {
        match value {
            ModuleCacheState::Cold => Self::Cold,
            ModuleCacheState::Warm => Self::Warm,
        }
    }
}

impl From<CallJitMode> for JitMode {
    fn from(value: CallJitMode) -> Self {
        match value {
            CallJitMode::InterpreterOnly => Self::InterpreterOnly,
            CallJitMode::Baseline => Self::Baseline,
        }
    }
}

#[derive(Debug, Parser)]
#[command(about = "Emit machine-readable Phase 0 call/JIT measurements")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Measure a validated, precompiled VM call workload.
    Call {
        #[arg(long, value_enum, default_value = "bytecode")]
        kind: CallKind,
        #[arg(long, default_value_t = 0)]
        arity: usize,
        #[arg(long, value_enum, default_value = "baseline")]
        jit_mode: CallJitMode,
        #[arg(long, default_value_t = 10_000)]
        iterations: u32,
        #[arg(long, default_value_t = 30)]
        samples: u32,
        #[arg(long, default_value_t = 3)]
        warmup: u32,
    },
    /// Measure baseline-JIT emission for one named function.
    JitCompile {
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        function: String,
        #[arg(long)]
        expected: f64,
        #[arg(long, default_value_t = 100)]
        samples: u32,
        #[arg(long, default_value_t = 10)]
        warmup: u32,
    },
    /// Measure managed allocations, heap bytes, and cumulative GC pause time.
    Memory {
        #[arg(long, default_value_t = 100_000)]
        iterations: u32,
        #[arg(long, default_value_t = 5)]
        samples: u32,
    },
    /// Measure validated module graph phases in a cold or warm runtime.
    Module {
        #[arg(long)]
        entry: PathBuf,
        #[arg(long, value_enum)]
        cache_state: ModuleCacheState,
        #[arg(long, default_value_t = 20)]
        samples: u32,
        #[arg(long, default_value_t = 3)]
        warmup: u32,
    },
    /// Measure retained managed heap and RSS for a validated macro workload.
    MacroMemory {
        #[arg(long)]
        name: String,
        #[arg(long, required = true, num_args = 1..)]
        source: Vec<PathBuf>,
        #[arg(long)]
        validation_marker: String,
        #[arg(long, default_value_t = 5)]
        samples: u32,
        /// Opt-in self-RSS sampling interval; requires the `rss` feature.
        #[arg(long, default_value_t = 0)]
        rss_sample_ms: u64,
    },
}

fn command_text(command: &str, args: &[&str]) -> String {
    std::process::Command::new(command)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_else(|| "unknown".into())
}

fn median(mut values: Vec<u64>) -> u64 {
    values.sort_unstable();
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        values[middle - 1].saturating_add(values[middle]) / 2
    } else {
        values[middle]
    }
}

fn elapsed_ns(started: Instant) -> u64 {
    started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
}

fn result(
    benchmark: String,
    runtime_mode: RuntimeMode,
    jit_mode: JitMode,
    execution: ExecutionMetrics,
    success: bool,
    marker: Option<String>,
    failure: Option<String>,
) -> BenchmarkResult {
    let code_memory_bytes = execution.code_bytes;
    BenchmarkResult {
        schema_version: BENCHMARK_RESULT_SCHEMA_VERSION,
        benchmark,
        commit: command_text("git", &["rev-parse", "HEAD"]),
        platform: format!(
            "{} {} {}",
            std::env::consts::OS,
            std::env::consts::ARCH,
            command_text("uname", &["-r"])
        ),
        toolchain: command_text("rustc", &["-Vv"]),
        build_profile: "release".into(),
        runtime_mode,
        jit_mode,
        gc_mode: GcMode::Normal,
        gc_stress_stride: None,
        cache_state: CacheState::NotApplicable,
        execution,
        memory: MemoryMetrics {
            code_memory_bytes,
            ..MemoryMetrics::default()
        },
        exit_code: Some(i32::from(!success)),
        success,
        validation: if success {
            ValidationStatus::Validated
        } else {
            ValidationStatus::Failed
        },
        validation_marker: marker,
        command: std::env::args().collect(),
        failure,
    }
}

fn emit_and_exit(result: BenchmarkResult) -> ! {
    println!("{}", serde_json::to_string_pretty(&result).unwrap());
    std::process::exit(i32::from(!result.is_scoreable()));
}

fn bytecode_call_source(arity: usize, iterations: u32) -> String {
    let params = (0..arity)
        .map(|index| format!("a{index}"))
        .collect::<Vec<_>>()
        .join(",");
    let args = vec!["1"; arity].join(",");
    let returned = if arity == 0 { "1" } else { "a0" };
    format!(
        "(function(){{function phase0CallTarget({params}){{return {returned};}}\
         let sum=0;for(let i=0;i<{iterations};i=i+1){{\
         sum=sum+phase0CallTarget({args});}}return sum;}})();"
    )
}

fn host_call_source(arity: usize, iterations: u32) -> Result<String, String> {
    if arity != 1 {
        return Err(format!(
            "host workload currently defines the extracted Math.abs/1 shape, not arity {arity}"
        ));
    }
    Ok(format!(
        "(function(){{const phase0CallTarget=Math.abs;let sum=0;\
         for(let i=0;i<{iterations};i=i+1){{\
         sum=sum+phase0CallTarget(-1);}}return sum;}})();"
    ))
}

fn memory_source(iterations: u32) -> String {
    format!(
        "(function(){{let total=0;for(let i=0;i<{iterations};i=i+1){{\
         let row=[i,i+1,i+2];let boxed={{value:row[1]}};\
         total=total+boxed.value;}}return total;}})();"
    )
}

fn run_call(
    kind: CallKind,
    arity: usize,
    jit_mode: CallJitMode,
    iterations: u32,
    samples: u32,
    warmup: u32,
) -> BenchmarkResult {
    let benchmark = format!(
        "call-{}-arity-{arity}",
        match kind {
            CallKind::Bytecode => "bytecode",
            CallKind::Host => "host",
        }
    );
    if samples == 0 {
        return result(
            benchmark,
            RuntimeMode::Vm,
            jit_mode.into(),
            ExecutionMetrics::default(),
            false,
            None,
            Some("samples must be greater than zero".into()),
        );
    }
    let source = match kind {
        CallKind::Bytecode => Ok(bytecode_call_source(arity, iterations)),
        CallKind::Host => host_call_source(arity, iterations),
    };
    let source = match source {
        Ok(source) => source,
        Err(error) => {
            return result(
                benchmark,
                RuntimeMode::Vm,
                jit_mode.into(),
                ExecutionMetrics::default(),
                false,
                None,
                Some(error),
            );
        }
    };
    let module = match compile_script_source(&source, SourceKind::JavaScript, "phase0-call.js") {
        Ok(module) => module,
        Err(error) => {
            return result(
                benchmark,
                RuntimeMode::Vm,
                jit_mode.into(),
                ExecutionMetrics::default(),
                false,
                None,
                Some(format!("bytecode compile failed: {error}")),
            );
        }
    };
    let context = ExecutionContext::from_module(module);
    let mut interpreter = Interpreter::new();
    if matches!(jit_mode, CallJitMode::Baseline) {
        interpreter.set_jit_compiler(Some(Arc::new(BaselineJitCompiler::new())));
    }
    let validate = |value: otter_vm::Value| {
        value
            .as_f64()
            .is_some_and(|actual| actual == f64::from(iterations))
    };
    for _ in 0..warmup {
        match interpreter.run(&context) {
            Ok(value) if validate(value) => {}
            Ok(value) => {
                return result(
                    benchmark,
                    RuntimeMode::Vm,
                    jit_mode.into(),
                    ExecutionMetrics::default(),
                    false,
                    None,
                    Some(format!("warmup returned {value:?}, expected {iterations}")),
                );
            }
            Err(error) => {
                return result(
                    benchmark,
                    RuntimeMode::Vm,
                    jit_mode.into(),
                    ExecutionMetrics::default(),
                    false,
                    None,
                    Some(format!("warmup failed: {error}")),
                );
            }
        }
    }
    let mut durations = Vec::with_capacity(samples as usize);
    for _ in 0..samples {
        let started = Instant::now();
        match interpreter.run(&context) {
            Ok(value) if validate(value) => durations.push(elapsed_ns(started)),
            Ok(value) => {
                return result(
                    benchmark,
                    RuntimeMode::Vm,
                    jit_mode.into(),
                    ExecutionMetrics::default(),
                    false,
                    None,
                    Some(format!("sample returned {value:?}, expected {iterations}")),
                );
            }
            Err(error) => {
                return result(
                    benchmark,
                    RuntimeMode::Vm,
                    jit_mode.into(),
                    ExecutionMetrics::default(),
                    false,
                    None,
                    Some(format!("sample failed: {error}")),
                );
            }
        }
    }
    let measured = median(durations);
    result(
        benchmark,
        RuntimeMode::Vm,
        jit_mode.into(),
        ExecutionMetrics {
            wall_time_ns: measured,
            execution_time_ns: Some(measured),
            ..ExecutionMetrics::default()
        },
        true,
        Some(format!("return={iterations}")),
        None,
    )
}

fn run_jit_compile(
    source_path: PathBuf,
    function_name: String,
    expected: f64,
    samples: u32,
    warmup: u32,
) -> BenchmarkResult {
    let benchmark = format!("jit-compile-{function_name}");
    if samples == 0 {
        return result(
            benchmark,
            RuntimeMode::Vm,
            JitMode::ForcedBaseline,
            ExecutionMetrics::default(),
            false,
            None,
            Some("samples must be greater than zero".into()),
        );
    }
    let source = match std::fs::read_to_string(&source_path) {
        Ok(source) => source,
        Err(error) => {
            return result(
                benchmark,
                RuntimeMode::Vm,
                JitMode::ForcedBaseline,
                ExecutionMetrics::default(),
                false,
                None,
                Some(format!("read {}: {error}", source_path.display())),
            );
        }
    };
    let module = match compile_script_source(
        &source,
        SourceKind::JavaScript,
        source_path.to_string_lossy().as_ref(),
    ) {
        Ok(module) => module,
        Err(error) => {
            return result(
                benchmark,
                RuntimeMode::Vm,
                JitMode::ForcedBaseline,
                ExecutionMetrics::default(),
                false,
                None,
                Some(format!("bytecode compile failed: {error}")),
            );
        }
    };
    let function_id = match module
        .functions
        .iter()
        .find(|function| function.name == function_name)
        .map(|function| function.id)
    {
        Some(function_id) => function_id,
        None => {
            return result(
                benchmark,
                RuntimeMode::Vm,
                JitMode::ForcedBaseline,
                ExecutionMetrics::default(),
                false,
                None,
                Some(format!("function {function_name:?} not found")),
            );
        }
    };
    let context = ExecutionContext::from_module(module);
    let view = match context.jit_compile_snapshot(function_id) {
        Some(view) => view,
        None => {
            return result(
                benchmark,
                RuntimeMode::Vm,
                JitMode::ForcedBaseline,
                ExecutionMetrics::default(),
                false,
                None,
                Some(format!(
                    "function id {function_id} has no executable snapshot"
                )),
            );
        }
    };
    let mut validation_interpreter = Interpreter::new();
    validation_interpreter.set_jit_compiler(Some(Arc::new(BaselineJitCompiler::new())));
    match validation_interpreter.run(&context) {
        Ok(value) if value.as_f64() == Some(expected) => {}
        Ok(value) => {
            return result(
                benchmark,
                RuntimeMode::Vm,
                JitMode::ForcedBaseline,
                ExecutionMetrics::default(),
                false,
                None,
                Some(format!(
                    "baseline validation returned {value:?}, expected {expected}"
                )),
            );
        }
        Err(error) => {
            return result(
                benchmark,
                RuntimeMode::Vm,
                JitMode::ForcedBaseline,
                ExecutionMetrics::default(),
                false,
                None,
                Some(format!("baseline validation failed: {error}")),
            );
        }
    }
    for _ in 0..warmup {
        if let Err(error) = compile(&view, 1) {
            return result(
                benchmark,
                RuntimeMode::Vm,
                JitMode::ForcedBaseline,
                ExecutionMetrics::default(),
                false,
                None,
                Some(format!("baseline JIT unsupported during warmup: {error:?}")),
            );
        }
    }
    let mut durations = Vec::with_capacity(samples as usize);
    let mut code_bytes = None;
    for _ in 0..samples {
        let started = Instant::now();
        let code = match compile(&view, 1) {
            Ok(code) => code,
            Err(error) => {
                return result(
                    benchmark,
                    RuntimeMode::Vm,
                    JitMode::ForcedBaseline,
                    ExecutionMetrics::default(),
                    false,
                    None,
                    Some(format!("baseline JIT unsupported: {error:?}")),
                );
            }
        };
        durations.push(elapsed_ns(started));
        let current = code.code_len() as u64;
        if code_bytes
            .replace(current)
            .is_some_and(|previous| previous != current)
        {
            return result(
                benchmark,
                RuntimeMode::Vm,
                JitMode::ForcedBaseline,
                ExecutionMetrics::default(),
                false,
                None,
                Some("baseline code size changed across identical samples".into()),
            );
        }
    }
    let measured = median(durations);
    let code_bytes = code_bytes.unwrap_or(0);
    result(
        benchmark,
        RuntimeMode::Vm,
        JitMode::ForcedBaseline,
        ExecutionMetrics {
            wall_time_ns: measured,
            compile_time_ns: Some(measured),
            code_bytes: Some(code_bytes),
            ..ExecutionMetrics::default()
        },
        true,
        Some(format!(
            "return={expected};compiled={function_name};code_bytes={code_bytes}"
        )),
        None,
    )
}

fn run_memory(iterations: u32, samples: u32) -> BenchmarkResult {
    let benchmark = "memory-allocation-churn-forced-full".to_string();
    if samples == 0 {
        return result(
            benchmark,
            RuntimeMode::Vm,
            JitMode::InterpreterOnly,
            ExecutionMetrics::default(),
            false,
            None,
            Some("samples must be greater than zero".into()),
        );
    }
    let source = memory_source(iterations);
    let module = match compile_script_source(&source, SourceKind::JavaScript, "phase0-memory.js") {
        Ok(module) => module,
        Err(error) => {
            return result(
                benchmark,
                RuntimeMode::Vm,
                JitMode::InterpreterOnly,
                ExecutionMetrics::default(),
                false,
                None,
                Some(format!("bytecode compile failed: {error}")),
            );
        }
    };
    let context = ExecutionContext::from_module(module);
    let expected = f64::from(iterations) * f64::from(iterations.saturating_add(1)) / 2.0;
    let mut wall_durations = Vec::with_capacity(samples as usize);
    let mut durations = Vec::with_capacity(samples as usize);
    let mut allocations = Vec::with_capacity(samples as usize);
    let mut heap_bytes = Vec::with_capacity(samples as usize);
    let mut gc_times = Vec::with_capacity(samples as usize);
    for _ in 0..samples {
        let mut interpreter = Interpreter::new();
        let before = interpreter.gc_heap_mut().gc_stats().clone();
        let wall_started = Instant::now();
        let started = Instant::now();
        let value = match interpreter.run(&context) {
            Ok(value) => value,
            Err(error) => {
                return result(
                    benchmark,
                    RuntimeMode::Vm,
                    JitMode::InterpreterOnly,
                    ExecutionMetrics::default(),
                    false,
                    None,
                    Some(format!("memory workload failed: {error}")),
                );
            }
        };
        durations.push(elapsed_ns(started));
        if value.as_f64() != Some(expected) {
            return result(
                benchmark,
                RuntimeMode::Vm,
                JitMode::InterpreterOnly,
                ExecutionMetrics::default(),
                false,
                None,
                Some(format!(
                    "memory workload returned {value:?}, expected {expected}"
                )),
            );
        }
        if let Err(error) = interpreter.force_gc() {
            return result(
                benchmark,
                RuntimeMode::Vm,
                JitMode::InterpreterOnly,
                ExecutionMetrics::default(),
                false,
                None,
                Some(format!("post-workload full GC failed: {error}")),
            );
        }
        wall_durations.push(elapsed_ns(wall_started));
        let after = interpreter.gc_heap_mut().gc_stats().clone();
        let before_allocations = before.by_type.iter().fold(0u64, |total, row| {
            total.saturating_add(row.alloc_count_total)
        });
        let after_allocations = after.by_type.iter().fold(0u64, |total, row| {
            total.saturating_add(row.alloc_count_total)
        });
        allocations.push(after_allocations.saturating_sub(before_allocations));
        heap_bytes.push(u64::try_from(after.live_bytes).unwrap_or(u64::MAX));
        let before_gc = before
            .minor_pause_ns_total
            .saturating_add(before.full_pause_ns_total);
        let after_gc = after
            .minor_pause_ns_total
            .saturating_add(after.full_pause_ns_total);
        gc_times.push(after_gc.saturating_sub(before_gc));
    }
    let wall_measured = median(wall_durations);
    let measured = median(durations);
    let mut record = result(
        benchmark,
        RuntimeMode::Vm,
        JitMode::InterpreterOnly,
        ExecutionMetrics {
            wall_time_ns: wall_measured,
            execution_time_ns: Some(measured),
            ..ExecutionMetrics::default()
        },
        true,
        Some(format!("return={expected}")),
        None,
    );
    record.gc_mode = GcMode::ForcedFull;
    record.memory = MemoryMetrics {
        allocations: Some(median(allocations)),
        gc_time_ns: Some(median(gc_times)),
        heap_bytes: Some(median(heap_bytes)),
        ..MemoryMetrics::default()
    };
    record
}

fn configured_runtime_jit_mode() -> JitMode {
    if std::env::var("OTTER_JIT").as_deref() == Ok("0") {
        JitMode::InterpreterOnly
    } else if std::env::var("OTTER_EXPERIMENTAL_OPTIMIZER").as_deref() == Ok("1") {
        JitMode::ExperimentalOptimizer
    } else {
        JitMode::Baseline
    }
}

fn build_runtime() -> Result<(Runtime, u64), String> {
    let started = Instant::now();
    Runtime::builder()
        .build()
        .map(|runtime| (runtime, elapsed_ns(started)))
        .map_err(|error| format!("runtime build failed: {error}"))
}

fn validate_module_run(
    runtime: &mut Runtime,
    entry: &PathBuf,
) -> Result<(u64, ModulePhaseTimings), String> {
    let (execution, timings) = runtime
        .run_module_profiled(entry)
        .map_err(|error| format!("module run failed: {error}"))?;
    if execution.exit_code() != 0 {
        return Err(format!(
            "module requested exit code {}",
            execution.exit_code()
        ));
    }
    let wall_time_ns = execution.duration.as_nanos().min(u128::from(u64::MAX)) as u64;
    Ok((wall_time_ns, timings))
}

fn run_module(
    entry: PathBuf,
    cache_state: ModuleCacheState,
    samples: u32,
    warmup: u32,
) -> BenchmarkResult {
    let benchmark = format!(
        "module-phases-{}",
        entry
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("entry")
    );
    let jit_mode = configured_runtime_jit_mode();
    if samples == 0 {
        let mut record = result(
            benchmark,
            RuntimeMode::Package,
            jit_mode,
            ExecutionMetrics::default(),
            false,
            None,
            Some("samples must be greater than zero".into()),
        );
        record.cache_state = cache_state.into();
        return record;
    }

    let mut wall_times = Vec::with_capacity(samples as usize);
    let mut runtime_build_times = Vec::with_capacity(samples as usize);
    let mut resolve_times = Vec::with_capacity(samples as usize);
    let mut load_times = Vec::with_capacity(samples as usize);
    let mut parse_times = Vec::with_capacity(samples as usize);
    let mut compile_times = Vec::with_capacity(samples as usize);
    let mut link_times = Vec::with_capacity(samples as usize);
    let mut execute_times = Vec::with_capacity(samples as usize);
    let mut push_sample = |wall_time: u64, timings: ModulePhaseTimings| {
        wall_times.push(wall_time);
        resolve_times.push(timings.resolve_time_ns);
        load_times.push(timings.load_time_ns);
        parse_times.push(timings.parse_time_ns);
        compile_times.push(timings.compile_time_ns);
        link_times.push(timings.link_time_ns);
        execute_times.push(timings.execute_time_ns);
    };

    match cache_state {
        ModuleCacheState::Cold => {
            for _ in 0..samples {
                let (mut runtime, build_time) = match build_runtime() {
                    Ok(built) => built,
                    Err(error) => return module_failure(benchmark, jit_mode, cache_state, error),
                };
                runtime_build_times.push(build_time);
                let (wall_time, timings) = match validate_module_run(&mut runtime, &entry) {
                    Ok(sample) => sample,
                    Err(error) => return module_failure(benchmark, jit_mode, cache_state, error),
                };
                push_sample(wall_time, timings);
            }
        }
        ModuleCacheState::Warm => {
            let (mut runtime, build_time) = match build_runtime() {
                Ok(built) => built,
                Err(error) => return module_failure(benchmark, jit_mode, cache_state, error),
            };
            runtime_build_times.push(build_time);
            for _ in 0..warmup {
                if let Err(error) = validate_module_run(&mut runtime, &entry) {
                    return module_failure(benchmark, jit_mode, cache_state, error);
                }
            }
            for _ in 0..samples {
                let (wall_time, timings) = match validate_module_run(&mut runtime, &entry) {
                    Ok(sample) => sample,
                    Err(error) => return module_failure(benchmark, jit_mode, cache_state, error),
                };
                push_sample(wall_time, timings);
            }
        }
    }

    let mut record = result(
        benchmark,
        RuntimeMode::Package,
        jit_mode,
        ExecutionMetrics {
            wall_time_ns: median(wall_times),
            execution_time_ns: Some(median(execute_times)),
            parse_time_ns: Some(median(parse_times)),
            compile_time_ns: Some(median(compile_times)),
            resolve_time_ns: Some(median(resolve_times)),
            load_time_ns: Some(median(load_times)),
            link_time_ns: Some(median(link_times)),
            runtime_build_time_ns: Some(median(runtime_build_times)),
            code_bytes: None,
        },
        true,
        Some("module assertions passed".into()),
        None,
    );
    record.cache_state = cache_state.into();
    record
}

fn module_failure(
    benchmark: String,
    jit_mode: JitMode,
    cache_state: ModuleCacheState,
    error: String,
) -> BenchmarkResult {
    let mut record = result(
        benchmark,
        RuntimeMode::Package,
        jit_mode,
        ExecutionMetrics::default(),
        false,
        None,
        Some(error),
    );
    record.cache_state = cache_state.into();
    record
}

#[cfg(feature = "rss")]
fn start_rss_sampler(interval_ms: u64) -> Result<Option<RssSampler>, String> {
    if interval_ms == 0 {
        return Ok(None);
    }
    let pid = sysinfo::get_current_pid().map_err(|error| format!("current pid: {error}"))?;
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let interval = Duration::from_millis(interval_ms);
    let thread = std::thread::spawn(move || {
        let mut system = sysinfo::System::new();
        let mut peak = 0u64;
        while !thread_stop.load(Ordering::Relaxed) {
            system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
            if let Some(process) = system.process(pid) {
                peak = peak.max(process.memory());
            }
            std::thread::sleep(interval);
        }
        system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
        if let Some(process) = system.process(pid) {
            peak = peak.max(process.memory());
        }
        peak
    });
    Ok(Some(RssSampler { stop, thread }))
}

#[cfg(not(feature = "rss"))]
fn start_rss_sampler(interval_ms: u64) -> Result<Option<RssSampler>, String> {
    if interval_ms == 0 {
        Ok(None)
    } else {
        Err("--rss-sample-ms requires building otter-phase0 with feature `rss`".into())
    }
}

#[cfg(feature = "rss")]
fn finish_rss_sampler(sampler: Option<RssSampler>) -> Option<u64> {
    sampler.map(|sampler| {
        sampler.stop.store(true, Ordering::Relaxed);
        sampler.thread.join().unwrap_or(0)
    })
}

#[cfg(not(feature = "rss"))]
fn finish_rss_sampler(_sampler: Option<RssSampler>) -> Option<u64> {
    None
}

fn gc_pause_ns(stats: RuntimeExecutionStats) -> u64 {
    stats
        .gc_minor_pause_ns_total
        .saturating_add(stats.gc_full_pause_ns_total)
}

struct MacroMemorySample {
    wall_time_ns: u64,
    execution_time_ns: u64,
    runtime_build_time_ns: u64,
    gc_time_ns: u64,
    heap_bytes: u64,
    code_memory_bytes: u64,
    peak_rss_bytes: Option<u64>,
    validation_line: String,
}

fn run_macro_memory_sample(
    sources: &[PathBuf],
    validation_marker: &str,
    rss_sample_ms: u64,
) -> Result<MacroMemorySample, String> {
    let sampler = start_rss_sampler(rss_sample_ms)?;
    let outcome = (|| {
        let sink = Arc::new(CapturingConsoleSink::default());
        let build_started = Instant::now();
        let mut runtime = Runtime::builder()
            .with_node_apis()
            .with_otter_modules()
            .with_web_apis()
            .console_sink(sink.clone())
            .build()
            .map_err(|error| format!("runtime build failed: {error}"))?;
        let runtime_build_time_ns = elapsed_ns(build_started);
        let before = runtime.execution_stats();
        let wall_started = Instant::now();
        let mut execution_time_ns = 0u64;
        for source in sources {
            let input = SourceInput::from_path(source)
                .map_err(|error| format!("{}: {error}", source.display()))?;
            let specifier = source.to_string_lossy();
            let execution = runtime
                .run_script(input, &specifier)
                .map_err(|error| format!("{}: {error}", source.display()))?;
            if execution.exit_code() != 0 {
                return Err(format!(
                    "{} requested exit code {}",
                    source.display(),
                    execution.exit_code()
                ));
            }
            execution_time_ns = execution_time_ns
                .saturating_add(execution.duration.as_nanos().min(u128::from(u64::MAX)) as u64);
        }
        let validation_line = sink
            .matching_line(validation_marker)
            .ok_or_else(|| format!("validation marker {validation_marker:?} was not emitted"))?;
        runtime
            .force_gc()
            .map_err(|error| format!("post-workload full GC failed: {error}"))?;
        let wall_time_ns = elapsed_ns(wall_started);
        let after = runtime.execution_stats();
        let code_residency = runtime.jit_code_residency();
        Ok(MacroMemorySample {
            wall_time_ns,
            execution_time_ns,
            runtime_build_time_ns,
            gc_time_ns: gc_pause_ns(after).saturating_sub(gc_pause_ns(before)),
            heap_bytes: u64::try_from(after.gc_live_bytes).unwrap_or(u64::MAX),
            code_memory_bytes: code_residency.code_bytes,
            peak_rss_bytes: None,
            validation_line,
        })
    })();
    let peak_rss_bytes = finish_rss_sampler(sampler);
    outcome.map(|mut sample| {
        sample.peak_rss_bytes = peak_rss_bytes;
        sample
    })
}

fn run_macro_memory(
    name: String,
    sources: Vec<PathBuf>,
    validation_marker: String,
    samples: u32,
    rss_sample_ms: u64,
) -> BenchmarkResult {
    let jit_mode = configured_runtime_jit_mode();
    if samples == 0 {
        return macro_memory_failure(name, jit_mode, "samples must be greater than zero".into());
    }
    let mut wall_times = Vec::with_capacity(samples as usize);
    let mut execution_times = Vec::with_capacity(samples as usize);
    let mut runtime_build_times = Vec::with_capacity(samples as usize);
    let mut gc_times = Vec::with_capacity(samples as usize);
    let mut heap_bytes = Vec::with_capacity(samples as usize);
    let mut code_memory_bytes = Vec::with_capacity(samples as usize);
    let mut peak_rss_bytes = Vec::with_capacity(samples as usize);
    let mut observed_marker = None;
    for _ in 0..samples {
        let sample = match run_macro_memory_sample(&sources, &validation_marker, rss_sample_ms) {
            Ok(sample) => sample,
            Err(error) => return macro_memory_failure(name, jit_mode, error),
        };
        wall_times.push(sample.wall_time_ns);
        execution_times.push(sample.execution_time_ns);
        runtime_build_times.push(sample.runtime_build_time_ns);
        gc_times.push(sample.gc_time_ns);
        heap_bytes.push(sample.heap_bytes);
        code_memory_bytes.push(sample.code_memory_bytes);
        if let Some(rss) = sample.peak_rss_bytes {
            peak_rss_bytes.push(rss);
        }
        observed_marker = Some(sample.validation_line);
    }
    let code_memory_bytes = median(code_memory_bytes);
    let mut record = result(
        name,
        RuntimeMode::Package,
        jit_mode,
        ExecutionMetrics {
            wall_time_ns: median(wall_times),
            execution_time_ns: Some(median(execution_times)),
            runtime_build_time_ns: Some(median(runtime_build_times)),
            code_bytes: Some(code_memory_bytes),
            ..ExecutionMetrics::default()
        },
        true,
        observed_marker,
        None,
    );
    record.cache_state = CacheState::Cold;
    record.gc_mode = GcMode::ForcedFull;
    record.memory = MemoryMetrics {
        gc_time_ns: Some(median(gc_times)),
        peak_rss_bytes: (!peak_rss_bytes.is_empty()).then(|| median(peak_rss_bytes)),
        heap_bytes: Some(median(heap_bytes)),
        code_memory_bytes: Some(code_memory_bytes),
        ..MemoryMetrics::default()
    };
    record
}

fn macro_memory_failure(name: String, jit_mode: JitMode, error: String) -> BenchmarkResult {
    let mut record = result(
        name,
        RuntimeMode::Package,
        jit_mode,
        ExecutionMetrics::default(),
        false,
        None,
        Some(error),
    );
    record.cache_state = CacheState::Cold;
    record.gc_mode = GcMode::ForcedFull;
    record
}

fn main() {
    let args = Args::parse();
    let result = match args.command {
        Command::Call {
            kind,
            arity,
            jit_mode,
            iterations,
            samples,
            warmup,
        } => run_call(kind, arity, jit_mode, iterations, samples, warmup),
        Command::JitCompile {
            source,
            function,
            expected,
            samples,
            warmup,
        } => run_jit_compile(source, function, expected, samples, warmup),
        Command::Memory {
            iterations,
            samples,
        } => run_memory(iterations, samples),
        Command::Module {
            entry,
            cache_state,
            samples,
            warmup,
        } => run_module(entry, cache_state, samples, warmup),
        Command::MacroMemory {
            name,
            source,
            validation_marker,
            samples,
            rss_sample_ms,
        } => run_macro_memory(name, source, validation_marker, samples, rss_sample_ms),
    };
    emit_and_exit(result);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_call_source_preserves_requested_arity() {
        let source = bytecode_call_source(256, 1);
        assert!(source.contains("a255"));
        assert_eq!(source.matches("=sum+phase0CallTarget").count(), 1);
    }

    #[test]
    fn median_handles_even_and_odd_samples() {
        assert_eq!(median(vec![9, 1, 4]), 4);
        assert_eq!(median(vec![8, 2, 4, 6]), 5);
    }

    #[test]
    fn console_capture_returns_the_latest_validation_line() {
        let sink = CapturingConsoleSink::default();
        sink.write(ConsoleLevel::Log, &["Score (version 7): 10".into()]);
        sink.write(ConsoleLevel::Log, &["Score (version 7): 20".into()]);
        assert_eq!(
            sink.matching_line("Score (version 7):").as_deref(),
            Some("Score (version 7): 20")
        );
    }
}
