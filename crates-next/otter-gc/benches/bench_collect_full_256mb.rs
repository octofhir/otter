//! Bench: STW full GC at 256 MiB live.
//!
//! Target: ≤ 50 ms wall.

use criterion::{Criterion, criterion_group, criterion_main};
use otter_gc::trace::{SlotVisitor, Traceable};
use otter_gc::{GcHeap, HandleScope, init_cage_with_size};

struct Block {
    _payload: [u8; 1024], // 1 KiB
}

impl Traceable for Block {
    const TYPE_TAG: u8 = 0xF3;
    unsafe fn trace_slots(_: *mut Self, _: &mut SlotVisitor<'_>) {}
}

fn bench(c: &mut Criterion) {
    // Cage size: 512 MiB so 256 MiB live + scratch fits.
    let _ = init_cage_with_size(512 * 1024 * 1024);
    let mut group = c.benchmark_group("collect_full_256mb");
    // Reduce sample count — single-iter wall time is multi-ms.
    group.sample_size(10);
    group.bench_function("256MiB_live", |b| {
        b.iter_custom(|iters| {
            let mut total = std::time::Duration::ZERO;
            for _ in 0..iters {
                let mut heap = GcHeap::new().expect("heap");
                heap.register_traceable::<Block>();
                let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
                // 256 MiB / ~1.04 KiB ≈ 256_000 blocks.
                let mut roots = Vec::with_capacity(256_000);
                for _ in 0..256_000u32 {
                    let g = match heap.alloc(Block {
                        _payload: [0; 1024],
                    }) {
                        Ok(g) => g,
                        Err(_) => break,
                    };
                    roots.push(scope.local(g));
                }
                // Promote everything to old-gen so the full GC
                // does meaningful work.
                heap.collect_minor(otter_gc::EmptyRoots);
                heap.collect_minor(otter_gc::EmptyRoots);
                let start = std::time::Instant::now();
                heap.collect_full(&mut |_| {});
                total += start.elapsed();
            }
            total
        });
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
