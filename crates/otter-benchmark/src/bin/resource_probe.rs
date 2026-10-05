//! One persistent runtime's natural collection service and retention phases.
//!
//! # Contents
//! - One loaded script, persistent callable, and repeated finite-checksum calls.
//! - Explicit setup, warm, measured, retention validation and release phases.
//! - One post-capture Chrome Trace Event artifact and an owned JSON result.
//!
//! # Invariants
//! - Init is compiled/linked once before capture. Warm and measured calls use
//!   the same strong persistent root and exact JavaScript function identity.
//! - NativeScope roots callable/results through collection; only scalar IDs,
//!   finite checksums and owned diagnostics leave a native callback.
//! - Forced retention collections and source compilation stay outside capture.
//! - Raw failed observations are written before semantic/retention validation.
//! - Coverage reconciles every outer and nested cycle to the exact runtime
//!   counters. Missing collection classes supply no available tail summary.
//! - This probe makes no Bun pause or managed-external-footprint victory claim.

use std::{error::Error, fs::OpenOptions, io, path::PathBuf};

use clap::{Parser, ValueEnum};
use otter_benchmark::gc_resources::{memory_snapshot, trace_document};
use otter_runtime::{
    JitSelection, Runtime, RuntimeExecutionContext, RuntimeNativeCtx, RuntimePersistentRootId,
    SourceInput,
};
use otter_vm::{NativeError, Value};
use serde_json::json;
use sha2::{Digest, Sha256};

type ProbeResult<T> = Result<T, Box<dyn Error>>;

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Tier {
    Interpreter,
    Template,
    Production,
}

#[derive(Parser)]
struct Args {
    /// Classic init script defining the named function on globalThis.
    source: PathBuf,
    /// Exact function property to root once; called with no arguments/undefined this.
    #[arg(long, default_value = "engineKernel")]
    function: String,
    /// Exact finite numeric checksum required after every warm/measured call.
    #[arg(long, value_parser = finite_checksum, allow_hyphen_values = true)]
    expected: f64,
    /// Fresh Chrome Trace Event file; an existing artifact is refused.
    #[arg(long)]
    trace: PathBuf,
    /// Optional setup script loaded once in the same realm before init.
    #[arg(long)]
    setup: Option<PathBuf>,
    #[arg(long, default_value_t = 0)]
    warmup: u32,
    #[arg(long, default_value_t = 16_384)]
    records: usize,
    #[arg(long, value_enum, default_value_t = Tier::Production)]
    tier: Tier,
    /// Optional retained checksum script, run after retained full GC.
    #[arg(long, requires = "expected_retained")]
    validate_retained: Option<PathBuf>,
    /// Exact completion string required from --validate-retained.
    #[arg(long, requires = "validate_retained")]
    expected_retained: Option<String>,
    /// Optional owner-release script, after retained validation.
    #[arg(long, requires = "expected_released")]
    release: Option<PathBuf>,
    /// Exact completion string required from --release.
    #[arg(long, requires = "release")]
    expected_released: Option<String>,
}

struct Phase {
    path: PathBuf,
    input: SourceInput,
    sha256: String,
}

impl Phase {
    fn load(path: PathBuf) -> ProbeResult<Self> {
        let bytes = std::fs::read(&path)?;
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        let input = SourceInput::from_path(&path)?;
        if format!("{:x}", Sha256::digest(input.text.as_bytes())) != sha256 {
            return Err(invalid("source identity changed during loading").into());
        }
        Ok(Self {
            path,
            input,
            sha256,
        })
    }

    fn run(&self, runtime: &mut Runtime) -> ProbeResult<String> {
        let result = runtime.run_script(self.input.clone(), &self.path.to_string_lossy())?;
        if result.exit_code() != 0 {
            return Err(invalid("phase requested an unsuccessful process exit").into());
        }
        Ok(result.completion_string().to_owned())
    }

    fn identity(&self) -> serde_json::Value {
        json!({ "path": self.path, "sha256": self.sha256 })
    }
}

/// Host state keeps one traced root key, never a moving Value or closure handle.
struct RootedCallable {
    root: RuntimePersistentRootId,
    function_id: u32,
}

fn plain_function_id(ctx: &RuntimeNativeCtx<'_>, value: Value) -> Option<u32> {
    value.as_function().or_else(|| {
        value
            .as_closure(ctx.heap())
            .map(|closure| closure.function_id())
    })
}

fn native_invalid(reason: &str) -> NativeError {
    NativeError::TypeError {
        name: "resource probe",
        reason: reason.to_string(),
    }
}

impl RootedCallable {
    fn load(
        runtime: &mut Runtime,
        context: &RuntimeExecutionContext,
        name: &str,
    ) -> ProbeResult<Self> {
        let mut owned = None;
        let entry = runtime.run_native_event(context, |ctx| {
            let value = ctx.scope(|mut scope| {
                let global = scope.global_this();
                let target = scope.get(global, name)?;
                if !scope.is_callable(target) {
                    return Err(native_invalid("named global property is not callable"));
                }
                Ok(scope.finish(target))
            })?;
            // No collecting operation occurs between scope finish, this scalar
            // read and insertion into the interpreter-owned strong root table.
            let function_id = plain_function_id(ctx, value).ok_or_else(|| {
                native_invalid("kernel must be a plain JavaScript function or closure")
            })?;
            owned = Some(Self {
                root: ctx.persistent_root_insert(value),
                function_id,
            });
            Ok(Value::undefined())
        });
        let exit = runtime.take_pending_exit_code();
        if entry.is_err() || exit.is_some() {
            if let Some(callable) = owned {
                // An event drain can fail after insertion. Close that exact
                // root even when the original abrupt completion is primary.
                let _ = callable.remove(runtime, context);
            }
            entry?;
            return Err(invalid("kernel admission requested a process exit").into());
        }
        owned.ok_or_else(|| invalid("kernel admission did not produce its strong root").into())
    }

    fn invoke(&self, runtime: &mut Runtime, context: &RuntimeExecutionContext) -> ProbeResult<f64> {
        let mut checksum = None;
        runtime.run_native_event(context, |ctx| {
            let value = ctx
                .persistent_root_get(self.root)
                .ok_or_else(|| native_invalid("kernel persistent root is missing"))?;
            if plain_function_id(ctx, value) != Some(self.function_id) {
                return Err(native_invalid(
                    "kernel root changed JavaScript function identity",
                ));
            }
            let number = ctx.scope(|mut scope| {
                let target = scope.value(value);
                let receiver = scope.undefined();
                let result = scope.call(target, receiver, &[])?;
                // Only this owned primitive leaves the scope; no result Value
                // survives the event's later collecting microtask drain.
                scope
                    .finish(result)
                    .as_f64()
                    .filter(|number| number.is_finite())
                    .ok_or_else(|| {
                        native_invalid("kernel did not return a finite numeric checksum")
                    })
            })?;
            checksum = Some(number);
            Ok(Value::undefined())
        })?;
        if runtime.take_pending_exit_code().is_some() {
            return Err(invalid("kernel invocation requested a process exit").into());
        }
        checksum.ok_or_else(|| invalid("kernel invocation ended without a numeric checksum").into())
    }

    fn remove(&self, runtime: &mut Runtime, context: &RuntimeExecutionContext) -> ProbeResult<()> {
        let mut removed = false;
        runtime.run_native_event(context, |ctx| {
            removed = ctx.persistent_root_remove(self.root).is_some();
            Ok(Value::undefined())
        })?;
        if runtime.take_pending_exit_code().is_some() || !removed {
            return Err(invalid("kernel root cleanup did not complete").into());
        }
        Ok(())
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn finite_checksum(text: &str) -> Result<f64, String> {
    text.parse::<f64>()
        .ok()
        .filter(|number| number.is_finite())
        .ok_or_else(|| "expected checksum must be a finite number".to_string())
}

fn checksum_matches(actual: f64, expected: f64) -> bool {
    actual.to_bits() == expected.to_bits()
}

fn own_generations(runtime: &Runtime, function_id: u32) -> Vec<serde_json::Value> {
    runtime
        .jit_code_generation_snapshot()
        .into_iter()
        .filter(|generation| generation.function_id == function_id)
        .map(|generation| {
            json!({
                "codeObjectId": generation.code_object_id,
                "functionId": generation.function_id,
                "tier": format!("{:?}", generation.tier),
                "lifecycle": format!("{:?}", generation.lifecycle),
                "linked": generation.linked,
                "activeCount": generation.active_count,
            })
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn measure(
    args: &Args,
    runtime: &mut Runtime,
    context: &RuntimeExecutionContext,
    callable: &RootedCallable,
    init: &Phase,
    setup: Option<&Phase>,
    validation: Option<&Phase>,
    release: Option<&Phase>,
) -> ProbeResult<serde_json::Value> {
    for _ in 0..args.warmup {
        if !checksum_matches(callable.invoke(runtime, context)?, args.expected) {
            return Err(invalid("warm invocation checksum mismatch").into());
        }
    }
    runtime.force_gc()?;
    let baseline = memory_snapshot(runtime).map_err(io::Error::other)?;
    let generations_before = own_generations(runtime, callable.function_id);
    let trace_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&args.trace)?;
    let counters_before = runtime.execution_stats();
    runtime.start_gc_pause_capture(args.records)?;
    let execution = callable.invoke(runtime, context);
    let capture = runtime.take_gc_pause_capture()?;
    let counters_after = runtime.execution_stats();
    let mut trace = trace_document(
        &capture,
        std::process::id(),
        &counters_before,
        &counters_after,
    );
    let natural_gaps: Vec<_> = capture
        .records
        .iter()
        .filter(|record| {
            matches!(
                record.trigger,
                otter_runtime::GcPauseTrigger::Explicit | otter_runtime::GcPauseTrigger::Stress
            )
        })
        .map(|record| format!("record {} has a forced/stress trigger", record.sequence))
        .collect();
    let complete = trace["metadata"]["coverageComplete"] == true && natural_gaps.is_empty();
    trace["metadata"]["naturalPauseCoverageComplete"] = json!(complete);
    trace["metadata"]["naturalPauseCoverageGaps"] = json!(natural_gaps);
    // Raw events/counters are persisted before a checksum or retained graph
    // validation can fail. All serialization happens after capture stopped.
    serde_json::to_writer_pretty(trace_file, &trace)?;
    let generations_after = own_generations(runtime, callable.function_id);
    let mut failures = Vec::new();
    let checksum = match execution {
        Ok(checksum) => {
            if !checksum_matches(checksum, args.expected) {
                failures
                    .push(json!({ "kind": "checksum", "message": "measured checksum mismatch" }));
            }
            Some(checksum)
        }
        Err(error) => {
            failures.push(json!({ "kind": "execution", "message": error.to_string() }));
            None
        }
    };
    if !complete {
        failures.push(json!({ "kind": "coverage", "message": "natural capture does not cover the exact collector service" }));
    }
    // Retention snapshots must not keep the native capture/trace tree alive.
    // Its allocator pages can still affect RSS, which remains a separate
    // absolute process component rather than GC-owned live memory.
    let tail_coverage_complete = trace["metadata"]["tailCoverageComplete"] == true && complete;
    let tail_coverage_gaps = trace["metadata"]["tailCoverageGaps"].clone();
    drop(trace);
    drop(capture);
    let mut retained = None;
    let mut released = None;
    let mut retained_completion = None;
    let mut released_completion = None;
    if failures.is_empty() {
        let retention_result = (|| -> ProbeResult<()> {
            runtime.force_gc()?;
            retained = Some(memory_snapshot(runtime).map_err(io::Error::other)?);
            if let Some(validation) = validation {
                let completion = validation.run(runtime)?;
                let matches = Some(completion.as_str()) == args.expected_retained.as_deref();
                retained_completion = Some(completion);
                if !matches {
                    return Err(invalid("retained validation completion mismatch").into());
                }
            }
            if let Some(release) = release {
                let completion = release.run(runtime)?;
                let matches = Some(completion.as_str()) == args.expected_released.as_deref();
                released_completion = Some(completion);
                if !matches {
                    return Err(invalid("release completion mismatch").into());
                }
                runtime.force_gc()?;
                released = Some(memory_snapshot(runtime).map_err(io::Error::other)?);
            }
            Ok(())
        })();
        if let Err(error) = retention_result {
            failures.push(json!({ "kind": "retention", "message": error.to_string() }));
        }
    }
    Ok(json!({
        "status": if failures.is_empty() { "passed" } else { "failed" },
        "failures": failures,
        "architecture": std::env::consts::ARCH,
        "debugAssertions": cfg!(debug_assertions),
        "tier": format!("{:?}", args.tier),
        "warmup": args.warmup,
        "sources": {
            "init": init.identity(), "setup": setup.map(Phase::identity),
            "retentionValidation": validation.map(Phase::identity), "release": release.map(Phase::identity),
        },
        "kernel": {
            "name": args.function, "functionId": callable.function_id,
            "persistentRootIndex": callable.root.index(), "persistentRootGeneration": callable.root.generation(),
            "argumentCount": 0, "this": "undefined",
            "nativeGenerationsBefore": generations_before, "nativeGenerationsAfter": generations_after,
            "identityInterpretation": "one strong callable root; exact function ID checked before every call; native generation presence alone does not prove execution",
        },
        "checksum": checksum, "expectedChecksum": args.expected,
        "trace": args.trace,
        "naturalPauseCoverageComplete": complete,
        "naturalPauseTailCoverageComplete": tail_coverage_complete,
        "naturalPauseTailCoverageGaps": tail_coverage_gaps,
        "countersBefore": counters_before, "countersAfter": counters_after,
        "baselineAfterFullGc": baseline, "retainedAfterFullGc": retained, "releasedAfterFullGc": released,
        "retainedValidationCompletion": retained_completion, "expectedRetainedCompletion": args.expected_retained,
        "releaseCompletion": released_completion, "expectedReleaseCompletion": args.expected_released,
        "retentionInterpretation": "absolute component snapshots; paired controls and matching retained recipes required externally; validation executes after retained full-GC snapshot",
        "strictBunPauseComparison": null,
    }))
}

fn main() -> ProbeResult<()> {
    let args = Args::parse();
    if std::env::var_os("OTTER_GC_STRESS").is_some_and(|value| value != "0") {
        return Err(invalid("natural GC capture requires stress to be absent or zero").into());
    }
    let init = Phase::load(args.source.clone())?;
    let setup = args.setup.clone().map(Phase::load).transpose()?;
    let validation = args
        .validate_retained
        .clone()
        .map(Phase::load)
        .transpose()?;
    let release = args.release.clone().map(Phase::load).transpose()?;
    let selection = match args.tier {
        Tier::Interpreter => JitSelection::InterpreterOnly,
        Tier::Template => JitSelection::Template,
        Tier::Production => JitSelection::ProductionTiered,
    };
    let mut runtime = Runtime::builder().jit_selection(selection).build()?;
    if let Some(setup) = &setup {
        setup.run(&mut runtime)?;
    }
    let (initialized, context) =
        runtime.run_script_with_context(init.input.clone(), &init.path.to_string_lossy())?;
    if initialized.exit_code() != 0 {
        return Err(invalid("init requested an unsuccessful process exit").into());
    }
    let callable = RootedCallable::load(&mut runtime, &context, &args.function)?;
    let measured = measure(
        &args,
        &mut runtime,
        &context,
        &callable,
        &init,
        setup.as_ref(),
        validation.as_ref(),
        release.as_ref(),
    );
    // The one callable root stays live through every snapshot/phase and is
    // closed on both measured success and failure before returning to the host.
    let cleanup = callable.remove(&mut runtime, &context);
    let mut report = measured?;
    report["kernel"]["persistentRootRemoved"] = json!(cleanup.is_ok());
    if let Err(error) = cleanup {
        report["status"] = json!("failed");
        if let Some(failures) = report["failures"].as_array_mut() {
            failures.push(json!({ "kind": "cleanup", "message": error.to_string() }));
        }
    }
    println!("{report}");
    if report["status"] == "passed" {
        Ok(())
    } else {
        Err(io::Error::other("resource probe failed; raw trace retained").into())
    }
}

#[cfg(test)]
#[path = "resource_probe_tests.rs"]
mod tests;
