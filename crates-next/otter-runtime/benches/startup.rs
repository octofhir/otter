//! Criterion ratchets for public runtime startup and first execution.
//!
//! These benches measure active-stack Layer B (`RuntimeBuilder`) and Layer A
//! (`Otter::builder`) construction plus the first script execution paths that
//! exercise bootstrap-installed static native builtins.

use std::time::{Duration, Instant};

use criterion::{Bencher, Criterion, criterion_group, criterion_main};
use otter_runtime::{CapabilitySet, ExecutionResult, Otter, Runtime, SourceInput};

fn iter_startup_bench<T>(bencher: &mut Bencher<'_>, mut f: impl FnMut() -> T) {
    bencher.iter_custom(|iters| {
        let mut measured = Duration::ZERO;
        for _ in 0..iters {
            let start = Instant::now();
            let value = std::hint::black_box(f());
            measured += start.elapsed();
            drop(value);
        }
        measured
    });
}

fn production_runtime() -> Runtime {
    Runtime::builder()
        .capabilities(CapabilitySet::sandbox())
        .timeout(Duration::from_secs(5))
        .build()
        .expect("production runtime")
}

fn run_first_javascript(source: &'static str) -> (Runtime, ExecutionResult) {
    let mut runtime = Runtime::builder().build().expect("runtime");
    let result = runtime
        .run_script(SourceInput::from_javascript(source), "<bench.js>")
        .expect("first JavaScript run");
    std::hint::black_box(&result);
    (runtime, result)
}

fn run_first_typescript(source: &'static str) -> (Runtime, ExecutionResult) {
    let mut runtime = Runtime::builder().build().expect("runtime");
    let result = runtime
        .run_script(SourceInput::from_typescript(source), "<bench.ts>")
        .expect("first TypeScript run");
    std::hint::black_box(&result);
    (runtime, result)
}

fn bench_runtime_startup(c: &mut Criterion) {
    let mut build = c.benchmark_group("runtime_builder_build");
    build.sample_size(30);
    build.warm_up_time(Duration::from_secs(1));
    build.measurement_time(Duration::from_secs(2));

    build.bench_function("default", |b| {
        iter_startup_bench(b, || Runtime::builder().build().expect("runtime"));
    });
    build.bench_function("production_sandbox", |b| {
        iter_startup_bench(b, production_runtime);
    });
    build.bench_function("otter_builder_default", |b| {
        iter_startup_bench(b, || Otter::builder().build().expect("otter"));
    });
    build.finish();

    let mut first_run = c.benchmark_group("runtime_first_run");
    first_run.sample_size(30);
    first_run.warm_up_time(Duration::from_secs(1));
    first_run.measurement_time(Duration::from_secs(2));

    first_run.bench_function("javascript_undefined", |b| {
        iter_startup_bench(b, || run_first_javascript("undefined;"));
    });
    first_run.bench_function("typescript_undefined", |b| {
        iter_startup_bench(b, || run_first_typescript("undefined as undefined;"));
    });
    first_run.bench_function("static_native_extracted_math_abs", |b| {
        iter_startup_bench(b, || run_first_javascript("const abs = Math.abs; abs(-1);"));
    });
    first_run.finish();
}

criterion_group!(benches, bench_runtime_startup);
criterion_main!(benches);
