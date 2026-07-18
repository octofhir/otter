//! Focused engine, runtime, and JIT measurements.
//!
//! # Contents
//! - `call` measures a precompiled direct-call workload under an explicit tier.
//! - `kernel` measures a named, precompiled JavaScript fixture under an explicit tier.
//! - `jit-compile` measures the production template compiler directly.
//! - `memory` records interpreter allocation, retained-heap, and GC samples.
//! - `module` records module-graph phases with explicit runtime reuse.
//!
//! # Invariants
//! - Tier selection and OSR thresholds come only from command-line arguments.
//! - Parsing and bytecode lowering are outside call and kernel execution samples.
//! - Kernel fixture setup runs once before warmup; samples run a precompiled call stub.
//! - One interpreter owns all warmup and measured kernel executions.
//! - JIT snapshot construction is outside template-emitter samples.
//! - Raw observations are retained alongside their median aggregates.
//! - Every successful record is semantically validated.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Instant;

use clap::{Parser, Subcommand, ValueEnum};
use otter_benchmark::{
    BenchmarkConfiguration, BenchmarkFailure, BenchmarkIdentity, BenchmarkOutcome,
    BenchmarkProvenance, BenchmarkResult, CacheState, ExecutionSurface, FailureKind, GcPolicy,
    JitPolicy, Metric, MetricDirection, MetricRole, MetricUnit, OutcomeStatus,
    RuntimeReuse as SchemaRuntimeReuse, SamplingPlan, Statistic, current_build_profile,
};
use otter_compiler::compile_script_source;
use otter_jit::{OtterJitCompiler, TransitionTable, compile};
use otter_runtime::{JitSelection, Runtime, module_graph::ModulePhaseTimings};
use otter_syntax::SourceKind;
use otter_vm::{
    ExecutionContext, Interpreter, JitCompileError, JitCompileRequest, JitCompileStatus,
    JitCompilerHook, JitExecOutcome, JitFunctionCode, JitRuntimeStubBinding, VmRuntimeActivation,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum CallKind {
    Bytecode,
    Host,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum EngineJitTier {
    /// Bytecode interpreter only.
    Interpreter,
    /// Production template compiler only.
    Template,
    /// Production optimizer with template fallback.
    ProductionTiered,
}

impl EngineJitTier {
    fn compiler(self) -> Option<Arc<dyn JitCompilerHook>> {
        match self {
            Self::Interpreter => None,
            Self::Template => Some(Arc::new(OtterJitCompiler::template_only())),
            Self::ProductionTiered => Some(Arc::new(OtterJitCompiler::production_tiered())),
        }
    }

    const fn runtime_selection(self) -> JitSelection {
        match self {
            Self::Interpreter => JitSelection::InterpreterOnly,
            Self::Template => JitSelection::Template,
            Self::ProductionTiered => JitSelection::ProductionTiered,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum RuntimeReuse {
    /// Build a fresh runtime for every measured sample.
    FreshPerSample,
    /// Reuse one runtime across warmups and measured samples.
    ReusedAcrossSamples,
}

#[derive(Debug, Parser)]
#[command(about = "Emit machine-readable Otter engine benchmark records")]
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
        #[arg(long, value_enum)]
        jit_tier: EngineJitTier,
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
        jit_osr_threshold: Option<u32>,
        #[arg(long, default_value_t = 10_000)]
        iterations: u32,
        #[arg(long, default_value_t = 30)]
        samples: u32,
        #[arg(long, default_value_t = 3)]
        warmup: u32,
    },
    /// Measure a validated, named JavaScript kernel from precompiled bytecode.
    Kernel {
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        function: String,
        #[arg(long, allow_hyphen_values = true)]
        expected: f64,
        #[arg(long, value_enum)]
        jit_tier: EngineJitTier,
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
        jit_osr_threshold: Option<u32>,
        #[arg(long, default_value_t = 20)]
        samples: u32,
        #[arg(long, default_value_t = 3)]
        warmup: u32,
    },
    /// Measure native-code emission for one named function.
    JitCompile {
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        function: String,
        #[arg(long, allow_hyphen_values = true)]
        expected: f64,
        #[arg(long, default_value_t = 100)]
        samples: u32,
        #[arg(long, default_value_t = 10)]
        warmup: u32,
    },
    /// Measure managed allocations, retained heap, and full-GC pause time.
    Memory {
        #[arg(long, default_value_t = 100_000)]
        iterations: u32,
        #[arg(long, default_value_t = 5)]
        samples: u32,
    },
    /// Measure validated module-graph phases under an explicit runtime policy.
    Module {
        #[arg(long)]
        entry: PathBuf,
        #[arg(long, value_enum)]
        runtime_reuse: RuntimeReuse,
        #[arg(long, value_enum)]
        jit_tier: EngineJitTier,
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
        jit_osr_threshold: Option<u32>,
        #[arg(long, default_value_t = 20)]
        samples: u32,
        #[arg(long, default_value_t = 3)]
        warmup: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunSurface {
    Vm,
    Module,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunGcPolicy {
    Normal,
    ForcedFull,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunFailureKind {
    Configuration,
    Input,
    Compile,
    Runtime,
    Validation,
    Io,
}

#[derive(Debug)]
struct RunFailure {
    kind: RunFailureKind,
    message: String,
}

#[derive(Debug, Default)]
struct Measurements {
    wall_time_ns: Vec<u64>,
    execution_time_ns: Vec<u64>,
    parse_time_ns: Vec<u64>,
    compile_time_ns: Vec<u64>,
    resolve_time_ns: Vec<u64>,
    load_time_ns: Vec<u64>,
    link_time_ns: Vec<u64>,
    runtime_build_time_ns: Vec<u64>,
    code_bytes: Vec<u64>,
    allocations: Vec<u64>,
    gc_time_ns: Vec<u64>,
    heap_bytes: Vec<u64>,
}

#[derive(Debug)]
struct RunRecord {
    name: String,
    parameters: BTreeMap<String, String>,
    surface: RunSurface,
    jit_tier: EngineJitTier,
    jit_osr_threshold: Option<u32>,
    gc_policy: RunGcPolicy,
    runtime_reuse: Option<RuntimeReuse>,
    warmup: u32,
    samples: u32,
    iterations_per_sample: Option<u64>,
    primary_metric: &'static str,
    measurements: Measurements,
    validation_marker: Option<String>,
    failure: Option<RunFailure>,
}

impl RunRecord {
    fn failure(
        name: String,
        surface: RunSurface,
        jit_tier: EngineJitTier,
        jit_osr_threshold: Option<u32>,
        gc_policy: RunGcPolicy,
        runtime_reuse: Option<RuntimeReuse>,
        warmup: u32,
        samples: u32,
        parameters: BTreeMap<String, String>,
        iterations_per_sample: Option<u64>,
        primary_metric: &'static str,
        failure_kind: RunFailureKind,
        failure: impl Into<String>,
    ) -> Self {
        Self {
            name,
            parameters,
            surface,
            jit_tier,
            jit_osr_threshold,
            gc_policy,
            runtime_reuse,
            warmup,
            samples,
            iterations_per_sample,
            primary_metric,
            measurements: Measurements::default(),
            validation_marker: None,
            failure: Some(RunFailure {
                kind: failure_kind,
                message: failure.into(),
            }),
        }
    }
}

fn elapsed_ns(started: Instant) -> u64 {
    started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
}

const fn jit_policy(tier: EngineJitTier) -> JitPolicy {
    match tier {
        EngineJitTier::Interpreter => JitPolicy::Interpreter,
        EngineJitTier::Template => JitPolicy::Template,
        EngineJitTier::ProductionTiered => JitPolicy::ProductionTiered,
    }
}

const fn runtime_reuse(reuse: Option<RuntimeReuse>) -> SchemaRuntimeReuse {
    match reuse {
        Some(RuntimeReuse::FreshPerSample) => SchemaRuntimeReuse::FreshPerSample,
        Some(RuntimeReuse::ReusedAcrossSamples) => SchemaRuntimeReuse::ReusedAcrossSamples,
        None => SchemaRuntimeReuse::NotApplicable,
    }
}

const fn gc_policy(policy: RunGcPolicy) -> GcPolicy {
    match policy {
        RunGcPolicy::Normal => GcPolicy::Normal,
        RunGcPolicy::ForcedFull => GcPolicy::ForcedFull,
    }
}

const fn failure_kind(kind: RunFailureKind) -> FailureKind {
    match kind {
        RunFailureKind::Configuration => FailureKind::Configuration,
        RunFailureKind::Input => FailureKind::Input,
        RunFailureKind::Compile => FailureKind::Compile,
        RunFailureKind::Runtime => FailureKind::Runtime,
        RunFailureKind::Validation => FailureKind::Validation,
        RunFailureKind::Io => FailureKind::Input,
    }
}

fn metric(
    name: &str,
    unit: MetricUnit,
    direction: MetricDirection,
    role: MetricRole,
    samples: &[u64],
) -> Option<Metric> {
    if samples.is_empty() {
        return None;
    }
    Some(
        Metric::from_u64_samples(
            name,
            unit,
            direction,
            role,
            samples.to_vec(),
            Statistic::Median,
        )
        .expect("non-empty integer samples always admit a median"),
    )
}

fn metric_role(name: &str, primary_metric: &str) -> MetricRole {
    if name == primary_metric {
        MetricRole::Primary
    } else if matches!(name, "runtime-build-time" | "wall-time") {
        MetricRole::Diagnostic
    } else {
        MetricRole::Secondary
    }
}

fn benchmark_result(record: RunRecord) -> BenchmarkResult {
    let mut metrics = Vec::new();
    let mut push_metric =
        |name: &str, unit: MetricUnit, direction: MetricDirection, samples: &[u64]| {
            if let Some(metric) = metric(
                name,
                unit,
                direction,
                metric_role(name, record.primary_metric),
                samples,
            ) {
                metrics.push(metric);
            }
        };
    push_metric(
        "wall-time",
        MetricUnit::Nanoseconds,
        MetricDirection::LowerIsBetter,
        &record.measurements.wall_time_ns,
    );
    push_metric(
        "execution-time",
        MetricUnit::Nanoseconds,
        MetricDirection::LowerIsBetter,
        &record.measurements.execution_time_ns,
    );
    push_metric(
        "parse-time",
        MetricUnit::Nanoseconds,
        MetricDirection::LowerIsBetter,
        &record.measurements.parse_time_ns,
    );
    push_metric(
        "compile-time",
        MetricUnit::Nanoseconds,
        MetricDirection::LowerIsBetter,
        &record.measurements.compile_time_ns,
    );
    push_metric(
        "resolve-time",
        MetricUnit::Nanoseconds,
        MetricDirection::LowerIsBetter,
        &record.measurements.resolve_time_ns,
    );
    push_metric(
        "load-time",
        MetricUnit::Nanoseconds,
        MetricDirection::LowerIsBetter,
        &record.measurements.load_time_ns,
    );
    push_metric(
        "link-time",
        MetricUnit::Nanoseconds,
        MetricDirection::LowerIsBetter,
        &record.measurements.link_time_ns,
    );
    push_metric(
        "runtime-build-time",
        MetricUnit::Nanoseconds,
        MetricDirection::LowerIsBetter,
        &record.measurements.runtime_build_time_ns,
    );
    push_metric(
        "code-size",
        MetricUnit::Bytes,
        MetricDirection::LowerIsBetter,
        &record.measurements.code_bytes,
    );
    push_metric(
        "allocations",
        MetricUnit::Count,
        MetricDirection::LowerIsBetter,
        &record.measurements.allocations,
    );
    push_metric(
        "gc-time",
        MetricUnit::Nanoseconds,
        MetricDirection::LowerIsBetter,
        &record.measurements.gc_time_ns,
    );
    push_metric(
        "retained-heap",
        MetricUnit::Bytes,
        MetricDirection::LowerIsBetter,
        &record.measurements.heap_bytes,
    );

    let status = if record.failure.is_some() {
        OutcomeStatus::Failed
    } else if record.validation_marker.is_some() {
        OutcomeStatus::Validated
    } else {
        OutcomeStatus::Unvalidated
    };
    let failure = record.failure.map(|failure| BenchmarkFailure {
        kind: failure_kind(failure.kind),
        message: failure.message,
    });
    BenchmarkResult {
        benchmark: BenchmarkIdentity {
            suite: "engine".into(),
            name: record.name,
            parameters: record.parameters,
        },
        provenance: BenchmarkProvenance::capture(current_build_profile()),
        configuration: BenchmarkConfiguration {
            surface: match record.surface {
                RunSurface::Vm => ExecutionSurface::Vm,
                RunSurface::Module => ExecutionSurface::Runtime,
            },
            jit_policy: jit_policy(record.jit_tier),
            jit_osr_threshold: record.jit_osr_threshold,
            gc_policy: gc_policy(record.gc_policy),
            gc_stress_stride: None,
            runtime_reuse: runtime_reuse(record.runtime_reuse),
            cache_state: CacheState::NotApplicable,
        },
        sampling: SamplingPlan {
            warmup_count: record.warmup,
            sample_count: record.samples,
            iterations_per_sample: record.iterations_per_sample,
            timeout_ms: None,
        },
        metrics,
        outcome: BenchmarkOutcome {
            status,
            validation_marker: record.validation_marker,
            process_exit_code: None,
            failure,
        },
        command: std::env::args().collect(),
    }
}

fn emit_and_exit(record: RunRecord) -> ! {
    let result = benchmark_result(record);
    println!("{}", serde_json::to_string_pretty(&result).unwrap());
    std::process::exit(i32::from(
        !result.is_scoreable() || result.contract_error().is_some(),
    ));
}

fn configure_interpreter(
    interpreter: &mut Interpreter,
    tier: EngineJitTier,
    jit_osr_threshold: Option<u32>,
) {
    if let Some(threshold) = jit_osr_threshold {
        interpreter.set_jit_osr_threshold(threshold);
    }
    interpreter.set_jit_compiler(tier.compiler());
}

fn bytecode_call_source(arity: usize, iterations: u32) -> String {
    let params = (0..arity)
        .map(|index| format!("a{index}"))
        .collect::<Vec<_>>()
        .join(",");
    let args = vec!["1"; arity].join(",");
    let returned = if arity == 0 { "1" } else { "a0" };
    format!(
        "(function(){{function engineCallTarget({params}){{return {returned};}}\
         let sum=0;for(let i=0;i<{iterations};i=i+1){{\
         sum=sum+engineCallTarget({args});}}return sum;}})();"
    )
}

fn host_call_source(arity: usize, iterations: u32) -> Result<String, String> {
    if arity != 1 {
        return Err(format!(
            "host workload defines the extracted Math.abs/1 shape, not arity {arity}"
        ));
    }
    Ok(format!(
        "(function(){{const engineCallTarget=Math.abs;let sum=0;\
         for(let i=0;i<{iterations};i=i+1){{\
         sum=sum+engineCallTarget(-1);}}return sum;}})();"
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
    jit_tier: EngineJitTier,
    jit_osr_threshold: Option<u32>,
    iterations: u32,
    samples: u32,
    warmup: u32,
) -> RunRecord {
    let name = format!(
        "call-{}-arity-{arity}",
        match kind {
            CallKind::Bytecode => "bytecode",
            CallKind::Host => "host",
        }
    );
    let parameters = BTreeMap::from([
        (
            "kind".into(),
            match kind {
                CallKind::Bytecode => "bytecode".into(),
                CallKind::Host => "host".into(),
            },
        ),
        ("arity".into(), arity.to_string()),
        ("iterations".into(), iterations.to_string()),
    ]);
    let fail = |kind: RunFailureKind, failure: String| {
        RunRecord::failure(
            name.clone(),
            RunSurface::Vm,
            jit_tier,
            jit_osr_threshold,
            RunGcPolicy::Normal,
            None,
            warmup,
            samples,
            parameters.clone(),
            Some(u64::from(iterations)),
            "wall-time",
            kind,
            failure,
        )
    };
    if samples == 0 {
        return fail(
            RunFailureKind::Configuration,
            "samples must be greater than zero".into(),
        );
    }
    if jit_tier == EngineJitTier::Interpreter && jit_osr_threshold.is_some() {
        return fail(
            RunFailureKind::Configuration,
            "--jit-osr-threshold requires a JIT tier".into(),
        );
    }
    let source = match kind {
        CallKind::Bytecode => Ok(bytecode_call_source(arity, iterations)),
        CallKind::Host => host_call_source(arity, iterations),
    };
    let source = match source {
        Ok(source) => source,
        Err(error) => return fail(RunFailureKind::Configuration, error),
    };
    let module = match compile_script_source(&source, SourceKind::JavaScript, "engine-call.js") {
        Ok(module) => module,
        Err(error) => {
            return fail(
                RunFailureKind::Compile,
                format!("bytecode compile failed: {error}"),
            );
        }
    };
    let context = ExecutionContext::from_module(module);
    let mut interpreter = Interpreter::new();
    configure_interpreter(&mut interpreter, jit_tier, jit_osr_threshold);
    let validate = |value: otter_vm::Value| {
        value
            .as_f64()
            .is_some_and(|actual| actual == f64::from(iterations))
    };
    for _ in 0..warmup {
        match interpreter.run(&context) {
            Ok(value) if validate(value) => {}
            Ok(value) => {
                return fail(
                    RunFailureKind::Validation,
                    format!("warmup returned {value:?}, expected {iterations}"),
                );
            }
            Err(error) => {
                return fail(RunFailureKind::Runtime, format!("warmup failed: {error}"));
            }
        }
    }
    let mut measurements = Measurements::default();
    for _ in 0..samples {
        let started = Instant::now();
        match interpreter.run(&context) {
            Ok(value) if validate(value) => {
                let elapsed = elapsed_ns(started);
                measurements.wall_time_ns.push(elapsed);
                measurements.execution_time_ns.push(elapsed);
            }
            Ok(value) => {
                return fail(
                    RunFailureKind::Validation,
                    format!("sample returned {value:?}, expected {iterations}"),
                );
            }
            Err(error) => {
                return fail(RunFailureKind::Runtime, format!("sample failed: {error}"));
            }
        }
    }
    RunRecord {
        name,
        parameters,
        surface: RunSurface::Vm,
        jit_tier,
        jit_osr_threshold,
        gc_policy: RunGcPolicy::Normal,
        runtime_reuse: None,
        warmup,
        samples,
        iterations_per_sample: Some(u64::from(iterations)),
        primary_metric: "wall-time",
        measurements,
        validation_marker: Some(format!("return={iterations}")),
        failure: None,
    }
}

fn validate_kernel_checksum(actual: f64, expected: f64) -> Result<(), String> {
    if !expected.is_finite() {
        return Err(format!("expected checksum {expected:?} is not finite"));
    }
    if !actual.is_finite() {
        return Err(format!("kernel checksum {actual:?} is not finite"));
    }
    if actual.to_bits() != expected.to_bits() {
        return Err(format!(
            "kernel checksum {actual:?} (0x{:016x}) did not match {expected:?} (0x{:016x})",
            actual.to_bits(),
            expected.to_bits()
        ));
    }
    Ok(())
}

const KERNEL_INVOCATION_FUNCTION: &str = "__otter_benchmark_kernel_invocation__";

fn compile_kernel_context(
    source: &str,
    source_path: &Path,
    function_name: &str,
) -> Result<(ExecutionContext, u32), RunFailure> {
    if function_name == KERNEL_INVOCATION_FUNCTION {
        return Err(RunFailure {
            kind: RunFailureKind::Input,
            message: format!(
                "function name {function_name:?} is reserved by the benchmark harness"
            ),
        });
    }
    let source =
        format!("{source}\nfunction {KERNEL_INVOCATION_FUNCTION}(){{return {function_name}();}}\n");
    let module = compile_script_source(
        &source,
        SourceKind::JavaScript,
        source_path.to_string_lossy().as_ref(),
    )
    .map_err(|error| RunFailure {
        kind: RunFailureKind::Compile,
        message: format!("bytecode compile failed: {error}"),
    })?;

    let matching_functions = module
        .functions
        .iter()
        .filter(|function| function.name == function_name)
        .count();
    match matching_functions {
        1 => {}
        0 => {
            return Err(RunFailure {
                kind: RunFailureKind::Input,
                message: format!("function {function_name:?} not found"),
            });
        }
        count => {
            return Err(RunFailure {
                kind: RunFailureKind::Input,
                message: format!("function {function_name:?} is ambiguous ({count} definitions)"),
            });
        }
    }

    let invocation_functions = module
        .functions
        .iter()
        .filter(|function| function.name == KERNEL_INVOCATION_FUNCTION)
        .count();
    if invocation_functions != 1 {
        return Err(RunFailure {
            kind: RunFailureKind::Input,
            message: format!(
                "fixture collides with reserved harness function {KERNEL_INVOCATION_FUNCTION:?}"
            ),
        });
    }
    let invocation_id = module
        .functions
        .iter()
        .find(|function| function.name == KERNEL_INVOCATION_FUNCTION)
        .expect("exactly one invocation function was counted")
        .id;
    Ok((ExecutionContext::from_module(module), invocation_id))
}

fn run_kernel_invocation(
    interpreter: &mut Interpreter,
    context: &ExecutionContext,
    invocation_id: u32,
) -> Result<otter_vm::Value, otter_vm::VmError> {
    let invocation = otter_vm::Value::function_id(invocation_id);
    interpreter.run_callable_sync(
        context,
        &invocation,
        otter_vm::Value::undefined(),
        Default::default(),
    )
}

fn run_kernel(
    source_path: PathBuf,
    function_name: String,
    expected: f64,
    jit_tier: EngineJitTier,
    jit_osr_threshold: Option<u32>,
    samples: u32,
    warmup: u32,
) -> RunRecord {
    let fixture_name = source_path
        .file_stem()
        .map(|name| name.to_string_lossy())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "fixture".into());
    let name = format!("kernel-{fixture_name}");
    let parameters = BTreeMap::from([
        ("source".into(), source_path.display().to_string()),
        ("function".into(), function_name.clone()),
        ("expected".into(), expected.to_string()),
    ]);
    let fail = |kind: RunFailureKind, failure: String| {
        RunRecord::failure(
            name.clone(),
            RunSurface::Vm,
            jit_tier,
            jit_osr_threshold,
            RunGcPolicy::Normal,
            None,
            warmup,
            samples,
            parameters.clone(),
            Some(1),
            "wall-time",
            kind,
            failure,
        )
    };
    if samples == 0 {
        return fail(
            RunFailureKind::Configuration,
            "samples must be greater than zero".into(),
        );
    }
    if jit_tier == EngineJitTier::Interpreter && jit_osr_threshold.is_some() {
        return fail(
            RunFailureKind::Configuration,
            "--jit-osr-threshold requires a JIT tier".into(),
        );
    }
    if !expected.is_finite() {
        return fail(
            RunFailureKind::Configuration,
            format!("expected checksum {expected:?} is not finite"),
        );
    }
    let source = match std::fs::read_to_string(&source_path) {
        Ok(source) => source,
        Err(error) => {
            return fail(
                RunFailureKind::Io,
                format!("read {}: {error}", source_path.display()),
            );
        }
    };
    let (context, invocation_id) =
        match compile_kernel_context(&source, &source_path, &function_name) {
            Ok(prepared) => prepared,
            Err(error) => return fail(error.kind, error.message),
        };
    let mut interpreter = Interpreter::new();
    configure_interpreter(&mut interpreter, jit_tier, jit_osr_threshold);
    if let Err(error) = interpreter.run(&context) {
        return fail(
            RunFailureKind::Runtime,
            format!("fixture setup failed: {error}"),
        );
    }
    for index in 0..warmup {
        let value = match run_kernel_invocation(&mut interpreter, &context, invocation_id) {
            Ok(value) => value,
            Err(error) => {
                return fail(
                    RunFailureKind::Runtime,
                    format!("warmup {} failed: {error}", index + 1),
                );
            }
        };
        let Some(actual) = value.as_f64() else {
            return fail(
                RunFailureKind::Validation,
                format!(
                    "warmup {} returned non-Number {value:?}, expected {expected:?}",
                    index + 1
                ),
            );
        };
        if let Err(error) = validate_kernel_checksum(actual, expected) {
            return fail(
                RunFailureKind::Validation,
                format!("warmup {}: {error}", index + 1),
            );
        }
    }

    let mut measurements = Measurements::default();
    for index in 0..samples {
        let started = Instant::now();
        let execution = run_kernel_invocation(&mut interpreter, &context, invocation_id);
        let elapsed = elapsed_ns(started);
        let value = match execution {
            Ok(value) => value,
            Err(error) => {
                return fail(
                    RunFailureKind::Runtime,
                    format!("sample {} failed: {error}", index + 1),
                );
            }
        };
        let Some(actual) = value.as_f64() else {
            return fail(
                RunFailureKind::Validation,
                format!(
                    "sample {} returned non-Number {value:?}, expected {expected:?}",
                    index + 1
                ),
            );
        };
        if let Err(error) = validate_kernel_checksum(actual, expected) {
            return fail(
                RunFailureKind::Validation,
                format!("sample {}: {error}", index + 1),
            );
        }
        measurements.wall_time_ns.push(elapsed);
        measurements.execution_time_ns.push(elapsed);
    }

    RunRecord {
        name,
        parameters,
        surface: RunSurface::Vm,
        jit_tier,
        jit_osr_threshold,
        gc_policy: RunGcPolicy::Normal,
        runtime_reuse: None,
        warmup,
        samples,
        iterations_per_sample: Some(1),
        primary_metric: "wall-time",
        measurements,
        validation_marker: Some(format!(
            "return={expected};bits=0x{:016x};function={function_name}",
            expected.to_bits()
        )),
        failure: None,
    }
}

/// Compile the snapshot once through the production template compiler.
fn compile_once(
    view: &otter_vm::JitCompileSnapshot,
    transitions: &TransitionTable,
) -> Result<Box<dyn otter_vm::JitFunctionCode>, String> {
    compile(view, 1, transitions)
        .map(|code| Box::new(code) as Box<dyn otter_vm::JitFunctionCode>)
        .map_err(|error| format!("{error:?}"))
}

#[derive(Debug)]
struct ObservedJitCode {
    code: Arc<dyn JitFunctionCode>,
    returned_entries: Arc<AtomicU64>,
}

impl JitFunctionCode for ObservedJitCode {
    fn metadata(&self) -> otter_vm::native_abi::CodeObjectMetadata {
        self.code.metadata()
    }

    fn native_frame_kind(&self) -> otter_vm::native_abi::NativeFrameKind {
        self.code.native_frame_kind()
    }

    fn dependencies(&self) -> &[otter_vm::native_abi::CodeDependency] {
        self.code.dependencies()
    }

    fn code_len(&self) -> usize {
        self.code.code_len()
    }

    fn osr_only(&self) -> bool {
        self.code.osr_only()
    }

    fn entry_addr(&self) -> Option<usize> {
        self.code.entry_addr()
    }

    fn safepoint_count(&self) -> u32 {
        self.code.safepoint_count()
    }

    fn safepoint_record(
        &self,
        safepoint_id: otter_vm::native_abi::SafepointId,
    ) -> Option<&otter_vm::native_abi::SafepointRecord> {
        self.code.safepoint_record(safepoint_id)
    }

    fn run_entry(&self, activation: VmRuntimeActivation) -> JitExecOutcome {
        let outcome = self.code.run_entry(activation);
        if matches!(&outcome, JitExecOutcome::Returned(_)) {
            self.returned_entries.fetch_add(1, Ordering::Relaxed);
        }
        outcome
    }

    fn run_optimized_entry(&self, activation: VmRuntimeActivation) -> Option<JitExecOutcome> {
        let outcome = self.code.run_optimized_entry(activation);
        if matches!(&outcome, Some(JitExecOutcome::Returned(_))) {
            self.returned_entries.fetch_add(1, Ordering::Relaxed);
        }
        outcome
    }

    fn run_optimized_osr_entry(
        &self,
        activation: VmRuntimeActivation,
        logical_pc: u32,
    ) -> Option<JitExecOutcome> {
        let outcome = self.code.run_optimized_osr_entry(activation, logical_pc);
        if matches!(&outcome, Some(JitExecOutcome::Returned(_))) {
            self.returned_entries.fetch_add(1, Ordering::Relaxed);
        }
        outcome
    }

    fn osr_entry(
        &self,
        activation: VmRuntimeActivation,
        logical_pc: u32,
    ) -> Option<JitExecOutcome> {
        let outcome = self.code.osr_entry(activation, logical_pc);
        if matches!(&outcome, Some(JitExecOutcome::Returned(_))) {
            self.returned_entries.fetch_add(1, Ordering::Relaxed);
        }
        outcome
    }
}

struct ExactArtifactCompiler {
    function_id: u32,
    code: Arc<dyn JitFunctionCode>,
    runtime_stub_bindings: Vec<JitRuntimeStubBinding>,
}

impl JitCompilerHook for ExactArtifactCompiler {
    fn runtime_stub_bindings(&self) -> Vec<JitRuntimeStubBinding> {
        self.runtime_stub_bindings.clone()
    }

    fn compile_function(
        &self,
        request: JitCompileRequest,
    ) -> Result<JitCompileStatus, JitCompileError> {
        if request.snapshot.code_block.id != self.function_id {
            return Ok(JitCompileStatus::Unsupported {
                reason: "validation hook exposes only the measured function".into(),
            });
        }
        if request.code_object_id != self.code.metadata().id {
            return Err(JitCompileError::new(format!(
                "measured artifact id {} cannot install as {}",
                self.code.metadata().id,
                request.code_object_id
            )));
        }
        Ok(JitCompileStatus::Compiled {
            code: Arc::clone(&self.code),
            artifact: None,
        })
    }
}

fn run_jit_compile(
    source_path: PathBuf,
    function_name: String,
    expected: f64,
    samples: u32,
    warmup: u32,
) -> RunRecord {
    let name = format!("jit-compile-{function_name}");
    let parameters = BTreeMap::from([
        ("source".into(), source_path.display().to_string()),
        ("function".into(), function_name.clone()),
        ("expected".into(), expected.to_string()),
    ]);
    let fail = |kind: RunFailureKind, failure: String| {
        RunRecord::failure(
            name.clone(),
            RunSurface::Vm,
            EngineJitTier::Template,
            None,
            RunGcPolicy::Normal,
            None,
            warmup,
            samples,
            parameters.clone(),
            Some(1),
            "compile-time",
            kind,
            failure,
        )
    };
    if samples == 0 {
        return fail(
            RunFailureKind::Configuration,
            "samples must be greater than zero".into(),
        );
    }
    let source = match std::fs::read_to_string(&source_path) {
        Ok(source) => source,
        Err(error) => {
            return fail(
                RunFailureKind::Io,
                format!("read {}: {error}", source_path.display()),
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
            return fail(
                RunFailureKind::Compile,
                format!("bytecode compile failed: {error}"),
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
            return fail(
                RunFailureKind::Input,
                format!("function {function_name:?} not found"),
            );
        }
    };
    let context = ExecutionContext::from_module(module);
    let view = match context.jit_compile_snapshot(function_id) {
        Some(view) => view,
        None => {
            return fail(
                RunFailureKind::Compile,
                format!("function id {function_id} has no executable snapshot"),
            );
        }
    };
    let transitions = TransitionTable::resolve();
    for _ in 0..warmup {
        if let Err(error) = compile_once(&view, &transitions) {
            return fail(
                RunFailureKind::Compile,
                format!("compiler declined during warmup: {error}"),
            );
        }
    }
    let mut measurements = Measurements::default();
    let mut expected_code_bytes = None;
    let mut validation_code = None;
    for _ in 0..samples {
        let started = Instant::now();
        let code = match compile_once(&view, &transitions) {
            Ok(code) => code,
            Err(error) => {
                return fail(
                    RunFailureKind::Compile,
                    format!("compiler declined: {error}"),
                );
            }
        };
        let elapsed = elapsed_ns(started);
        measurements.wall_time_ns.push(elapsed);
        measurements.compile_time_ns.push(elapsed);
        let code_bytes = code.code_len() as u64;
        if expected_code_bytes.is_some_and(|previous| previous != code_bytes) {
            return fail(
                RunFailureKind::Validation,
                "template code size changed across identical samples".into(),
            );
        }
        expected_code_bytes = Some(code_bytes);
        measurements.code_bytes.push(code_bytes);
        validation_code = Some(Arc::<dyn JitFunctionCode>::from(code));
    }
    let Some(validation_code) = validation_code else {
        return fail(
            RunFailureKind::Validation,
            "no measured artifact was retained for validation".into(),
        );
    };
    if validation_code.osr_only() {
        return fail(
            RunFailureKind::Validation,
            "measured artifact is OSR-only and cannot validate function entry".into(),
        );
    }
    let returned_entries = Arc::new(AtomicU64::new(0));
    let observed_code: Arc<dyn JitFunctionCode> = Arc::new(ObservedJitCode {
        code: validation_code,
        returned_entries: Arc::clone(&returned_entries),
    });
    let template_compiler = OtterJitCompiler::template_only();
    let exact_compiler = ExactArtifactCompiler {
        function_id,
        code: observed_code,
        runtime_stub_bindings: template_compiler.runtime_stub_bindings(),
    };
    let mut validation_interpreter = Interpreter::new();
    validation_interpreter.set_jit_compiler(Some(Arc::new(exact_compiler)));
    match validation_interpreter.run(&context) {
        Ok(value) if value.as_f64() == Some(expected) => {}
        Ok(value) => {
            return fail(
                RunFailureKind::Validation,
                format!("measured artifact returned {value:?}, expected {expected}"),
            );
        }
        Err(error) => {
            return fail(
                RunFailureKind::Runtime,
                format!("measured artifact validation failed: {error}"),
            );
        }
    }
    if returned_entries.load(Ordering::Relaxed) == 0 {
        return fail(
            RunFailureKind::Validation,
            "validation completed without the measured artifact returning".into(),
        );
    }
    let code_bytes = expected_code_bytes.unwrap_or(0);
    RunRecord {
        name,
        parameters,
        surface: RunSurface::Vm,
        jit_tier: EngineJitTier::Template,
        jit_osr_threshold: None,
        gc_policy: RunGcPolicy::Normal,
        runtime_reuse: None,
        warmup,
        samples,
        iterations_per_sample: Some(1),
        primary_metric: "compile-time",
        measurements,
        validation_marker: Some(format!(
            "return={expected};compiled={function_name};code_bytes={code_bytes}"
        )),
        failure: None,
    }
}

fn run_memory(iterations: u32, samples: u32) -> RunRecord {
    let name = "memory-allocation-churn-forced-full".to_owned();
    let parameters = BTreeMap::from([("iterations".into(), iterations.to_string())]);
    let fail = |kind: RunFailureKind, failure: String| {
        RunRecord::failure(
            name.clone(),
            RunSurface::Vm,
            EngineJitTier::Interpreter,
            None,
            RunGcPolicy::ForcedFull,
            None,
            0,
            samples,
            parameters.clone(),
            Some(u64::from(iterations)),
            "wall-time",
            kind,
            failure,
        )
    };
    if samples == 0 {
        return fail(
            RunFailureKind::Configuration,
            "samples must be greater than zero".into(),
        );
    }
    let source = memory_source(iterations);
    let module = match compile_script_source(&source, SourceKind::JavaScript, "engine-memory.js") {
        Ok(module) => module,
        Err(error) => {
            return fail(
                RunFailureKind::Compile,
                format!("bytecode compile failed: {error}"),
            );
        }
    };
    let context = ExecutionContext::from_module(module);
    let expected = f64::from(iterations) * f64::from(iterations.saturating_add(1)) / 2.0;
    let mut measurements = Measurements::default();
    for _ in 0..samples {
        let mut interpreter = Interpreter::new();
        let before = interpreter.gc_heap_mut().gc_stats().clone();
        let wall_started = Instant::now();
        let execution_started = Instant::now();
        let value = match interpreter.run(&context) {
            Ok(value) => value,
            Err(error) => {
                return fail(
                    RunFailureKind::Runtime,
                    format!("memory workload failed: {error}"),
                );
            }
        };
        measurements
            .execution_time_ns
            .push(elapsed_ns(execution_started));
        if value.as_f64() != Some(expected) {
            return fail(
                RunFailureKind::Validation,
                format!("memory workload returned {value:?}, expected {expected}"),
            );
        }
        if let Err(error) = interpreter.force_gc() {
            return fail(
                RunFailureKind::Runtime,
                format!("post-workload full GC failed: {error}"),
            );
        }
        measurements.wall_time_ns.push(elapsed_ns(wall_started));
        let after = interpreter.gc_heap_mut().gc_stats().clone();
        let before_allocations = before.by_type.iter().fold(0u64, |total, row| {
            total.saturating_add(row.alloc_count_total)
        });
        let after_allocations = after.by_type.iter().fold(0u64, |total, row| {
            total.saturating_add(row.alloc_count_total)
        });
        measurements
            .allocations
            .push(after_allocations.saturating_sub(before_allocations));
        measurements
            .heap_bytes
            .push(u64::try_from(after.live_bytes).unwrap_or(u64::MAX));
        let before_gc = before
            .minor_pause_ns_total
            .saturating_add(before.full_pause_ns_total);
        let after_gc = after
            .minor_pause_ns_total
            .saturating_add(after.full_pause_ns_total);
        measurements
            .gc_time_ns
            .push(after_gc.saturating_sub(before_gc));
    }
    RunRecord {
        name,
        parameters,
        surface: RunSurface::Vm,
        jit_tier: EngineJitTier::Interpreter,
        jit_osr_threshold: None,
        gc_policy: RunGcPolicy::ForcedFull,
        runtime_reuse: None,
        warmup: 0,
        samples,
        iterations_per_sample: Some(u64::from(iterations)),
        primary_metric: "wall-time",
        measurements,
        validation_marker: Some(format!("return={expected}")),
        failure: None,
    }
}

fn build_runtime(
    jit_tier: EngineJitTier,
    jit_osr_threshold: Option<u32>,
) -> Result<(Runtime, u64), String> {
    let started = Instant::now();
    let mut builder = Runtime::builder().jit_selection(jit_tier.runtime_selection());
    if let Some(threshold) = jit_osr_threshold {
        builder = builder.jit_osr_threshold(threshold);
    }
    builder
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

fn push_module_sample(
    measurements: &mut Measurements,
    wall_time_ns: u64,
    timings: ModulePhaseTimings,
) {
    measurements.wall_time_ns.push(wall_time_ns);
    measurements.resolve_time_ns.push(timings.resolve_time_ns);
    measurements.load_time_ns.push(timings.load_time_ns);
    measurements.parse_time_ns.push(timings.parse_time_ns);
    measurements.compile_time_ns.push(timings.compile_time_ns);
    measurements.link_time_ns.push(timings.link_time_ns);
    measurements.execution_time_ns.push(timings.execute_time_ns);
}

fn run_module(
    entry: PathBuf,
    runtime_reuse: RuntimeReuse,
    jit_tier: EngineJitTier,
    jit_osr_threshold: Option<u32>,
    samples: u32,
    warmup: u32,
) -> RunRecord {
    let name = format!(
        "module-phases-{}",
        entry
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("entry")
    );
    let parameters = BTreeMap::from([("entry".into(), entry.display().to_string())]);
    let fail = |kind: RunFailureKind, failure: String| {
        RunRecord::failure(
            name.clone(),
            RunSurface::Module,
            jit_tier,
            jit_osr_threshold,
            RunGcPolicy::Normal,
            Some(runtime_reuse),
            warmup,
            samples,
            parameters.clone(),
            Some(1),
            "wall-time",
            kind,
            failure,
        )
    };
    if samples == 0 {
        return fail(
            RunFailureKind::Configuration,
            "samples must be greater than zero".into(),
        );
    }
    if jit_tier == EngineJitTier::Interpreter && jit_osr_threshold.is_some() {
        return fail(
            RunFailureKind::Configuration,
            "--jit-osr-threshold requires a JIT tier".into(),
        );
    }
    if runtime_reuse == RuntimeReuse::FreshPerSample && warmup != 0 {
        return fail(
            RunFailureKind::Configuration,
            "--warmup must be zero when --runtime-reuse=fresh-per-sample".into(),
        );
    }
    let mut measurements = Measurements::default();
    match runtime_reuse {
        RuntimeReuse::FreshPerSample => {
            for _ in 0..samples {
                let (mut runtime, build_time) = match build_runtime(jit_tier, jit_osr_threshold) {
                    Ok(built) => built,
                    Err(error) => return fail(RunFailureKind::Runtime, error),
                };
                measurements.runtime_build_time_ns.push(build_time);
                let (wall_time, timings) = match validate_module_run(&mut runtime, &entry) {
                    Ok(sample) => sample,
                    Err(error) => return fail(RunFailureKind::Runtime, error),
                };
                push_module_sample(&mut measurements, wall_time, timings);
            }
        }
        RuntimeReuse::ReusedAcrossSamples => {
            let (mut runtime, _build_time) = match build_runtime(jit_tier, jit_osr_threshold) {
                Ok(built) => built,
                Err(error) => return fail(RunFailureKind::Runtime, error),
            };
            for _ in 0..warmup {
                if let Err(error) = validate_module_run(&mut runtime, &entry) {
                    return fail(RunFailureKind::Runtime, error);
                }
            }
            for _ in 0..samples {
                let (wall_time, timings) = match validate_module_run(&mut runtime, &entry) {
                    Ok(sample) => sample,
                    Err(error) => return fail(RunFailureKind::Runtime, error),
                };
                push_module_sample(&mut measurements, wall_time, timings);
            }
        }
    }
    RunRecord {
        name,
        parameters,
        surface: RunSurface::Module,
        jit_tier,
        jit_osr_threshold,
        gc_policy: RunGcPolicy::Normal,
        runtime_reuse: Some(runtime_reuse),
        warmup,
        samples,
        iterations_per_sample: Some(1),
        primary_metric: "wall-time",
        measurements,
        validation_marker: Some("module assertions passed".into()),
        failure: None,
    }
}

fn main() {
    let args = Args::parse();
    let record = match args.command {
        Command::Call {
            kind,
            arity,
            jit_tier,
            jit_osr_threshold,
            iterations,
            samples,
            warmup,
        } => run_call(
            kind,
            arity,
            jit_tier,
            jit_osr_threshold,
            iterations,
            samples,
            warmup,
        ),
        Command::Kernel {
            source,
            function,
            expected,
            jit_tier,
            jit_osr_threshold,
            samples,
            warmup,
        } => run_kernel(
            source,
            function,
            expected,
            jit_tier,
            jit_osr_threshold,
            samples,
            warmup,
        ),
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
            runtime_reuse,
            jit_tier,
            jit_osr_threshold,
            samples,
            warmup,
        } => run_module(
            entry,
            runtime_reuse,
            jit_tier,
            jit_osr_threshold,
            samples,
            warmup,
        ),
    };
    emit_and_exit(record);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_call_source_preserves_requested_arity() {
        let source = bytecode_call_source(256, 1);
        assert!(source.contains("a255"));
        assert_eq!(source.matches("=sum+engineCallTarget").count(), 1);
    }

    #[test]
    fn result_preserves_raw_samples_and_their_median() {
        let record = RunRecord {
            name: "raw-samples".into(),
            parameters: BTreeMap::new(),
            surface: RunSurface::Vm,
            jit_tier: EngineJitTier::Interpreter,
            jit_osr_threshold: None,
            gc_policy: RunGcPolicy::Normal,
            runtime_reuse: None,
            warmup: 0,
            samples: 3,
            iterations_per_sample: Some(1),
            primary_metric: "wall-time",
            measurements: Measurements {
                wall_time_ns: vec![9, 1, 4],
                ..Measurements::default()
            },
            validation_marker: Some("ok".into()),
            failure: None,
        };
        let result = benchmark_result(record);
        assert!(result.contract_error().is_none());
        assert_eq!(
            result.metrics[0]
                .samples
                .iter()
                .map(|value| value.as_f64())
                .collect::<Vec<_>>(),
            vec![9.0, 1.0, 4.0]
        );
        assert_eq!(result.metrics[0].aggregate.value.as_f64(), 4.0);
        assert_eq!(result.metrics[0].aggregate.statistic, Statistic::Median);
        assert!(result.sampling.timeout_ms.is_none());
        assert!(result.outcome.process_exit_code.is_none());
        let json = serde_json::to_value(result).unwrap();
        assert!(json.get("schemaVersion").is_none());
        assert!(json.get("version").is_none());
    }

    #[test]
    fn template_tier_uses_template_only_compiler_policy() {
        let compiler = EngineJitTier::Template
            .compiler()
            .expect("template compiler");
        assert!(!compiler.optimizing_tier_enabled());
        assert_eq!(
            EngineJitTier::Template.runtime_selection(),
            JitSelection::Template
        );
        assert!(
            EngineJitTier::ProductionTiered
                .compiler()
                .expect("production compiler")
                .optimizing_tier_enabled()
        );
        assert!(EngineJitTier::Interpreter.compiler().is_none());
    }

    #[test]
    fn clap_accepts_explicit_tier_and_runtime_reuse() {
        let args = Args::try_parse_from([
            "otter-engine-benchmark",
            "module",
            "--entry",
            "entry.mjs",
            "--runtime-reuse",
            "reused-across-samples",
            "--jit-tier",
            "template",
            "--jit-osr-threshold",
            "7",
        ])
        .expect("new engine arguments");
        match args.command {
            Command::Module {
                runtime_reuse,
                jit_tier,
                jit_osr_threshold,
                ..
            } => {
                assert_eq!(runtime_reuse, RuntimeReuse::ReusedAcrossSamples);
                assert_eq!(jit_tier, EngineJitTier::Template);
                assert_eq!(jit_osr_threshold, Some(7));
            }
            _ => panic!("expected module command"),
        }
    }

    #[test]
    fn clap_accepts_named_kernel_fixture() {
        let args = Args::try_parse_from([
            "otter-engine-benchmark",
            "kernel",
            "--source",
            "kernel.js",
            "--function",
            "engineKernel",
            "--expected",
            "-42",
            "--jit-tier",
            "production-tiered",
            "--jit-osr-threshold",
            "7",
        ])
        .expect("kernel arguments");
        match args.command {
            Command::Kernel {
                source,
                function,
                expected,
                jit_tier,
                jit_osr_threshold,
                ..
            } => {
                assert_eq!(source, PathBuf::from("kernel.js"));
                assert_eq!(function, "engineKernel");
                assert_eq!(expected, -42.0);
                assert_eq!(jit_tier, EngineJitTier::ProductionTiered);
                assert_eq!(jit_osr_threshold, Some(7));
            }
            _ => panic!("expected kernel command"),
        }
    }

    #[test]
    fn clap_accepts_negative_jit_compile_checksum() {
        let args = Args::try_parse_from([
            "otter-engine-benchmark",
            "jit-compile",
            "--source",
            "kernel.js",
            "--function",
            "engineKernel",
            "--expected",
            "-42",
        ])
        .expect("negative compile checksum");
        match args.command {
            Command::JitCompile { expected, .. } => assert_eq!(expected, -42.0),
            _ => panic!("expected JIT compile command"),
        }
    }

    #[test]
    fn kernel_checksum_is_finite_and_bit_exact() {
        assert!(validate_kernel_checksum(42.0, 42.0).is_ok());
        assert!(validate_kernel_checksum(-0.0, 0.0).is_err());
        assert!(validate_kernel_checksum(f64::NAN, 42.0).is_err());
        assert!(validate_kernel_checksum(42.0, f64::INFINITY).is_err());
    }

    #[test]
    fn kernel_reuses_one_interpreter_and_rejects_wrong_checksum() {
        let source_path = std::env::temp_dir().join(format!(
            "otter-engine-kernel-test-{}.js",
            std::process::id()
        ));
        std::fs::write(
            &source_path,
            "function engineKernel(){let sum=0;\
             for(let i=0;i<4;i=i+1){sum=sum+i;}return sum;}",
        )
        .expect("write kernel fixture");

        let passed = run_kernel(
            source_path.clone(),
            "engineKernel".into(),
            6.0,
            EngineJitTier::Interpreter,
            None,
            2,
            1,
        );
        assert!(passed.failure.is_none(), "{:?}", passed.failure);
        assert_eq!(
            passed.name,
            format!("kernel-otter-engine-kernel-test-{}", std::process::id())
        );
        assert_eq!(passed.measurements.wall_time_ns.len(), 2);
        assert_eq!(passed.measurements.execution_time_ns.len(), 2);
        assert_eq!(passed.iterations_per_sample, Some(1));

        let failed = benchmark_result(run_kernel(
            source_path.clone(),
            "engineKernel".into(),
            7.0,
            EngineJitTier::Interpreter,
            None,
            2,
            1,
        ));
        assert_eq!(failed.outcome.status, OutcomeStatus::Failed);
        assert_eq!(
            failed.outcome.failure.as_ref().map(|failure| failure.kind),
            Some(FailureKind::Validation)
        );
        assert!(!failed.is_scoreable());

        std::fs::remove_file(source_path).expect("remove kernel fixture");
    }

    #[test]
    fn kernel_invocation_survives_full_gc_after_setup() {
        let source = "function engineKernel(){return 42;}";
        let (context, invocation_id) =
            compile_kernel_context(source, Path::new("kernel.js"), "engineKernel")
                .expect("compile kernel");
        let mut interpreter = Interpreter::new();
        interpreter.run(&context).expect("evaluate setup");
        interpreter.force_gc().expect("force full GC");
        let value = run_kernel_invocation(&mut interpreter, &context, invocation_id)
            .expect("invoke after full GC");
        assert_eq!(value.as_f64(), Some(42.0));
    }

    #[test]
    fn clap_rejects_legacy_tier_and_macro_surfaces() {
        assert!(Args::try_parse_from(["otter-engine-benchmark", "call"]).is_err());
        assert!(
            Args::try_parse_from(["otter-engine-benchmark", "call", "--jit-mode", "baseline",])
                .is_err()
        );
        assert!(
            Args::try_parse_from(["otter-engine-benchmark", "call", "--jit-tier", "baseline",])
                .is_err()
        );
        assert!(Args::try_parse_from(["otter-engine-benchmark", "macro-memory"]).is_err());
        assert!(
            Args::try_parse_from([
                "otter-engine-benchmark",
                "module",
                "--entry",
                "entry.mjs",
                "--cache-state",
                "cold",
            ])
            .is_err()
        );
    }

    #[test]
    fn build_profile_reflects_the_compiled_binary() {
        assert_eq!(
            current_build_profile(),
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
        );
    }
}
