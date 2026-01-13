//! Benchmarks for JSC runtime
//!
//! Run with: cargo bench -p otter-runtime

use criterion::{Criterion, criterion_group, criterion_main};

fn jsc_benchmarks(_c: &mut Criterion) {
    // TODO: Add JSC benchmarks once runtime is working
}

criterion_group!(benches, jsc_benchmarks);
criterion_main!(benches);
