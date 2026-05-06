//! Bench: compressed-pointer offset and decompression hot path.
//!
//! Target: keep `Gc<T>` offset reads and header-pointer decompression in the
//! low single-digit nanosecond range.

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use otter_gc::trace::{SlotVisitor, Traceable};
use otter_gc::{GcHeap, init_cage_with_size};

struct Cell {
    _payload: u64,
}

impl Traceable for Cell {
    const TYPE_TAG: u8 = 0xF4;
    unsafe fn trace_slots(_: *mut Self, _: &mut SlotVisitor<'_>) {}
}

fn bench(c: &mut Criterion) {
    let _ = init_cage_with_size(512 * 1024 * 1024);
    let mut heap = GcHeap::new().expect("heap");
    heap.register_traceable::<Cell>();
    let cell = heap.alloc(Cell { _payload: 1 }).expect("alloc");

    let mut group = c.benchmark_group("pointer_compress_decompress");
    group.throughput(Throughput::Elements(1));
    group.bench_function("offset", |b| {
        b.iter(|| std::hint::black_box(cell.offset()));
    });
    group.bench_function("header_ptr", |b| {
        b.iter(|| std::hint::black_box(cell.as_header_ptr()));
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
