//! Bench: external/backing-store accounting tokens.
//!
//! Measures cap accounting for off-object bytes such as future typed-array or
//! host backing stores.

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use otter_gc::{GcHeap, init_cage_with_size};

fn bench(c: &mut Criterion) {
    let _ = init_cage_with_size(512 * 1024 * 1024);
    let mut heap = GcHeap::with_max_heap_bytes(64 * 1024 * 1024).expect("heap");

    let mut group = c.benchmark_group("backing_store_accounting");
    group.throughput(Throughput::Bytes(4096));
    group.bench_function("reserve_release_4k", |b| {
        b.iter(|| {
            let token = heap.reserve_external(4096).expect("reserve");
            std::hint::black_box(token.bytes());
            drop(token);
        });
    });
    group.bench_function("resize_4k_to_8k_to_1k", |b| {
        b.iter(|| {
            let mut token = heap.reserve_external(4096).expect("reserve");
            token.resize(8192).expect("grow");
            token.resize(1024).expect("shrink");
            std::hint::black_box(token.bytes());
            drop(token);
        });
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
