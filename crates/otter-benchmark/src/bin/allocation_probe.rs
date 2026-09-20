//! Measure retained allocations during observable constructor preparation.
//!
//! # Contents
//! - One process runs one fixture, tier, and allocation count, then emits JSON.
//! - Existing runtime counters report moving/full GC, generated allocation,
//!   remembered-set work, compilation, reentry, and deoptimization.
//! - `emit-kernel` writes the same workload as an `engineKernel` JavaScript
//!   function for production measurement through `otter-engine-benchmark`.
//!
//! # Invariants
//! - `OTTER_GC_STRESS` is configured by the invoking process, never mutated here.
//! - The default 200,000-object workload is retained for production measurement.
//! - All observable fixture assertions remain enabled in every tier and size.
//! - GC and JIT times are measured by existing telemetry. The remaining script
//!   time includes frontend compilation, runtime bookkeeping, and JavaScript;
//!   it is not claimed to be an isolated JavaScript execution timer.
//!
//! # See also
//! - `benchmarks/fixtures/engine/reentrant_allocations.rs` defines the shared workloads.
//!
//! Example: `OTTER_GC_STRESS=16 cargo run --release -p otter-benchmark
//! --features engine --bin otter-allocation-probe -- construct production 200000`.

use std::{error::Error, io, time::Instant};

use otter_runtime::{JitDebugEvent, JitDebugRequest, JitSelection, Runtime, SourceInput};
use serde_json::json;

#[path = "../../../../benchmarks/fixtures/engine/reentrant_allocations.rs"]
mod reentrant_allocations;
use reentrant_allocations::AllocationFixture;

fn invalid_argument(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn fixture_named(name: &str) -> Result<AllocationFixture, io::Error> {
    match name {
        "construct" => Ok(AllocationFixture::Fixed),
        "spread" => Ok(AllocationFixture::Spread),
        _ => Err(invalid_argument("fixture must be construct or spread")),
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "construct".to_owned());
    if command == "emit-kernel" {
        let fixture = fixture_named(args.next().as_deref().unwrap_or("construct"))?;
        let allocations: usize = args
            .next()
            .map(|value| value.parse())
            .transpose()?
            .unwrap_or(200_000);
        if args.next().is_some() {
            return Err(invalid_argument(
                "usage: otter-allocation-probe emit-kernel [construct|spread] [allocations]",
            )
            .into());
        }
        print!("{}", fixture.kernel(allocations));
        return Ok(());
    }
    let fixture = fixture_named(&command)?;
    let tier = args.next().unwrap_or_else(|| "production".to_owned());
    let selection = match tier.as_str() {
        "interpreter" => JitSelection::InterpreterOnly,
        "template" => JitSelection::Template,
        "production" => JitSelection::ProductionTiered,
        _ => {
            return Err(
                invalid_argument("tier must be interpreter, template, or production").into(),
            );
        }
    };
    let allocations: usize = args
        .next()
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(200_000);
    if args.next().is_some() {
        return Err(invalid_argument("usage: otter-allocation-probe [construct|spread] [interpreter|template|production] [allocations]").into());
    }

    let source = fixture.source(allocations);
    let build_start = Instant::now();
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_debug(JitDebugRequest::events())
        .build()?;
    let build_ns = build_start.elapsed().as_nanos();
    let before = runtime.execution_stats();
    let started = Instant::now();
    let result = runtime.run_script(
        SourceInput::from_javascript(&source),
        "reentrant-allocation-benchmark.js",
    )?;
    let elapsed_ns = started.elapsed().as_nanos();
    let after = runtime.execution_stats();
    assert_eq!(
        result.completion_string(),
        AllocationFixture::expected_completion(allocations),
        "{tier} {} with {allocations} retained objects",
        fixture.name(),
    );

    let report = result.jit_debug_report();
    let jit_ns: u64 = report
        .into_iter()
        .flat_map(|report| report.events())
        .filter_map(|event| match event {
            JitDebugEvent::CompileFinished {
                compile_duration_ns,
                ..
            } => Some(*compile_duration_ns),
            _ => None,
        })
        .sum();
    let jit_time_complete = report.is_none_or(|report| !report.truncated());
    let minor_ns = after.gc_minor_pause_ns_total - before.gc_minor_pause_ns_total;
    let full_ns = after.gc_full_pause_ns_total - before.gc_full_pause_ns_total;
    // Full-GC pause telemetry includes its initial young scavenge. Avoid
    // claiming a precise remainder while both cumulative timers can overlap.
    let remainder_ns = (full_ns == 0 && jit_time_complete)
        .then(|| elapsed_ns.saturating_sub(u128::from(minor_ns) + u128::from(jit_ns)));
    println!(
        "{}",
        json!({
            "fixture": fixture.name(),
            "tier": tier,
            "allocations": allocations,
            "stress": std::env::var("OTTER_GC_STRESS").ok(),
            "architecture": std::env::consts::ARCH,
            "debugAssertions": cfg!(debug_assertions),
            "runtimeBuildNs": build_ns,
            "scriptElapsedNs": elapsed_ns,
            "minorGcPauseNs": minor_ns,
            "fullGcPauseNs": full_ns,
            "jitCompilationNs": jit_ns,
            "jitTimingComplete": jit_time_complete,
            "javascriptAndFrontendAndRuntimeNs": remainder_ns,
            "completion": result.completion_string(),
            "before": before,
            "after": after,
        })
    );
    Ok(())
}
