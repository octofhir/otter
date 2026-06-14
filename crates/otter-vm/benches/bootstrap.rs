//! Criterion ratchets for active-stack global bootstrap.
//!
//! Measures the centralized Task 96 registry with and without optional Task 98
//! telemetry. The telemetry case is intentionally separate so production
//! runtime startup does not pay for phase timing or duplicate-name validation.

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use otter_vm::bootstrap::{
    BootstrapFeatures, BootstrapTelemetry, build_global_this_with_features,
    build_global_this_with_telemetry,
};
use otter_vm::symbol::WellKnownSymbols;

fn build_default_global_this() {
    let mut heap = otter_gc::GcHeap::new().expect("heap");
    let well_known = WellKnownSymbols::new(&mut heap).expect("well-known symbols");
    let global = build_global_this_with_features(&mut heap, BootstrapFeatures::all(), &well_known)
        .expect("default globalThis");
    std::hint::black_box(global);
}

fn build_core_without_console() {
    let mut heap = otter_gc::GcHeap::new().expect("heap");
    let well_known = WellKnownSymbols::new(&mut heap).expect("well-known symbols");
    let global = build_global_this_with_features(
        &mut heap,
        BootstrapFeatures::all().without(BootstrapFeatures::CONSOLE),
        &well_known,
    )
    .expect("core globalThis");
    std::hint::black_box(global);
}

fn build_default_global_this_with_telemetry() {
    let mut heap = otter_gc::GcHeap::new().expect("heap");
    let well_known = WellKnownSymbols::new(&mut heap).expect("well-known symbols");
    let mut telemetry = BootstrapTelemetry::default();
    let global = build_global_this_with_telemetry(
        &mut heap,
        BootstrapFeatures::all(),
        &mut telemetry,
        &well_known,
    )
    .expect("instrumented globalThis");
    std::hint::black_box(global);
    std::hint::black_box(telemetry.gc_allocations());
    std::hint::black_box(telemetry.gc_allocated_bytes());
}

fn bench_bootstrap(c: &mut Criterion) {
    let mut group = c.benchmark_group("bootstrap_global_this");
    group.sample_size(30);
    group.warm_up_time(std::time::Duration::from_secs(1));
    group.measurement_time(std::time::Duration::from_secs(2));

    group.bench_function("default_features", |b| {
        b.iter_batched(
            || (),
            |()| build_default_global_this(),
            BatchSize::SmallInput,
        );
    });
    group.bench_function("core_without_console", |b| {
        b.iter_batched(
            || (),
            |()| build_core_without_console(),
            BatchSize::SmallInput,
        );
    });
    group.bench_function("default_features_with_telemetry", |b| {
        b.iter_batched(
            || (),
            |()| build_default_global_this_with_telemetry(),
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

criterion_group!(benches, bench_bootstrap);
criterion_main!(benches);
