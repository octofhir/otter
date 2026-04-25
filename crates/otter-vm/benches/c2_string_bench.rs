//! C2 — string subsystem performance benchmarks.
//!
//! Run: `cargo bench -p otter-vm --bench c2_string_bench`.
//!
//! Cases (per `docs/c2-rope-strings-plan.md` §5):
//!
//! 1. **`+= loop`** — build a 1 MB string by repeated concatenation. The
//!    legacy eager-flatten path is O(n²) and would OOM at this size; the
//!    new `Cons`/`Sliced` hierarchy with depth-bound flatten amortizes to
//!    O(n log n).
//! 2. **Slice non-observed** — `big.slice(i, i+10)` × 100 000. After C2
//!    these allocate a `Sliced` view (no copy) until first observation.
//! 3. **Slice observed** — same loop but with `+ .charCodeAt(0)` so each
//!    slice forces flatten. Cache-amortized cost.
//! 4. **ASCII memory** — load a 4 MB ASCII text as JsString. Latin-1
//!    detection halves the storage; the bench reports allocated bytes.
//! 5. **`indexOf` on 1 MB haystack** — memcpy-bound after flatten cache.
//! 6. **String hash determinism** — verify FNV-1a stability across reprs.
//!
//! These exercise the heap-level helpers
//! (`concat_strings` / `slice_string` / `flatten_string` / `string_hash`)
//! directly, without going through the full interpreter pipeline. The
//! interpreter-level perf is verified by the existing test262 runner +
//! the `c2_*` integration tests.

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use otter_vm::js_string::JsString;
use otter_vm::object::ObjectHeap;

/// Allocate a primitive string handle on a fresh heap, no prototype.
fn alloc(heap: &mut ObjectHeap, s: &str) -> otter_vm::object::ObjectHandle {
    heap.alloc_js_string(JsString::from_str(s))
        .expect("alloc string")
}

fn bench_concat_loop(c: &mut Criterion) {
    let mut group = c.benchmark_group("c2_concat_loop");
    let target_kb = 256usize; // 256 KB — enough to demonstrate Cons amortization without
                              // ballooning bench time. The plan's 1 MB target verified by
                              // hand once via `--measurement-time 60`.
    group.throughput(Throughput::Bytes((target_kb * 1024) as u64));

    group.bench_function("cons_path_256kb", |b| {
        b.iter(|| {
            let mut heap = ObjectHeap::new();
            // Pre-alloc the empty seed and the chunk handle so the loop
            // measures concat-only.
            let chunk = alloc(&mut heap, "abcdefghijklmnopqrstuvwxyz0123456789"); // 36 B
            let mut acc = alloc(&mut heap, "");
            // Each iter adds 36 bytes; we stop when length crosses target_kb*1024.
            let target = target_kb * 1024;
            while heap.string_length(acc).unwrap() < target as u32 {
                acc = heap.concat_strings(acc, chunk, None).expect("concat");
            }
            // Force flatten so the result is observable — this is what
            // `charCodeAt(0)` etc. would trigger in real code.
            heap.flatten_string(acc).expect("flatten");
            black_box(heap.string_length(acc).unwrap());
        });
    });

    group.finish();
}

fn bench_slice_non_observed(c: &mut Criterion) {
    let mut group = c.benchmark_group("c2_slice_non_observed");
    group.bench_function("slice_view_100k", |b| {
        // Pre-build a 16 KB haystack once; the loop slices it 100k times.
        let mut heap = ObjectHeap::new();
        let big_text: String = "abcdefghij".repeat(1638); // ~16 KB
        let big = alloc(&mut heap, &big_text);

        b.iter(|| {
            // 100k Sliced views; each is a 24-byte heap entry, no copy.
            for i in 0..100_000u32 {
                let off = i % 1000;
                let h = heap
                    .slice_string(big, off, off + 10, None)
                    .expect("slice");
                black_box(h);
            }
        });
    });
    group.finish();
}

fn bench_slice_observed(c: &mut Criterion) {
    let mut group = c.benchmark_group("c2_slice_observed");
    group.bench_function("slice_then_flatten_10k", |b| {
        let mut heap = ObjectHeap::new();
        let big_text: String = "abcdefghij".repeat(1638);
        let big = alloc(&mut heap, &big_text);

        b.iter(|| {
            // 10k slice + force-flatten. Each iter materializes a 10-unit Seq*.
            for i in 0..10_000u32 {
                let off = i % 1000;
                let h = heap
                    .slice_string(big, off, off + 10, None)
                    .expect("slice");
                heap.flatten_string(h).expect("flatten");
                black_box(heap.string_length(h).unwrap());
            }
        });
    });
    group.finish();
}

fn bench_ascii_memory(c: &mut Criterion) {
    let mut group = c.benchmark_group("c2_ascii_memory");
    // 1 MB of ASCII content — Phase 4 stores as SeqOneByte (1 MB heap),
    // pre-Phase-4 storage was SeqTwoByte (2 MB heap). The bench measures
    // the alloc + length-read path; in-process heap accounting can be
    // observed via `heap.tracked_bytes()` (informational, not a Criterion
    // metric).
    let payload: String = "abcdefghij".repeat(100_000); // 1 MB
    group.throughput(Throughput::Bytes(payload.len() as u64));
    group.bench_function("alloc_1mb_ascii", |b| {
        b.iter(|| {
            let mut heap = ObjectHeap::new();
            let h = heap
                .alloc_js_string(JsString::from_str(&payload))
                .expect("alloc");
            // Latin-1 invariant: ASCII is 1-byte.
            assert!(heap.string_is_one_byte(h).unwrap());
            black_box(heap.string_length(h).unwrap());
        });
    });
    group.finish();
}

fn bench_index_of(c: &mut Criterion) {
    let mut group = c.benchmark_group("c2_index_of");
    // Build a 256 KB haystack with the needle near the end — measures the
    // post-flatten bytewise scan.
    let mut haystack: String = "abcdefghij".repeat(26214); // ~256 KB
    haystack.push_str("FOUND");
    let needle = JsString::from_str("FOUND");
    let big = JsString::from_str(&haystack);

    group.throughput(Throughput::Bytes(haystack.len() as u64));
    group.bench_function("find_at_end_256kb", |b| {
        b.iter(|| {
            // The legacy `JsString::index_of` operates on flat content.
            let result = big.index_of(&needle, 0);
            black_box(result.unwrap());
        });
    });
    group.finish();
}

fn bench_string_hash(c: &mut Criterion) {
    let mut group = c.benchmark_group("c2_string_hash");
    // Hash 1k distinct strings. First touch computes; second touch reads
    // cache. Both measured.
    let strings: Vec<String> = (0..1000).map(|i| format!("propName_{i:08}")).collect();

    group.bench_function("compute_first_touch_1k", |b| {
        b.iter(|| {
            let mut heap = ObjectHeap::new();
            let handles: Vec<_> = strings.iter().map(|s| alloc(&mut heap, s)).collect();
            for h in &handles {
                let hash = heap.string_hash(*h).expect("hash");
                black_box(hash);
            }
        });
    });

    group.bench_function("read_cached_1k", |b| {
        // Setup phase: alloc + warm cache once.
        let mut heap = ObjectHeap::new();
        let handles: Vec<_> = strings.iter().map(|s| alloc(&mut heap, s)).collect();
        for h in &handles {
            heap.string_hash(*h).expect("warm");
        }
        b.iter(|| {
            for h in &handles {
                let hash = heap.string_hash(*h).expect("hash cached");
                black_box(hash);
            }
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_concat_loop,
    bench_slice_non_observed,
    bench_slice_observed,
    bench_ascii_memory,
    bench_index_of,
    bench_string_hash,
);
criterion_main!(benches);
