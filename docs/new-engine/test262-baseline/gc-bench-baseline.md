# GC Phase 1 baseline (2026-05-03, host: Apple M1, macOS 25.2)

Numbers captured by Criterion on `cargo bench -p otter-gc`,
release profile, single iteration measurements averaged across
the run length shown.

| Bench | Target | Median measurement |
|---|---|---|
| `bench_alloc_young_bump` (1 × `Cell` alloc) | ≤ 10 ns/op | **2.63 ns/op** |
| `bench_alloc_with_barrier` (2 × alloc + 1 store via `write_barrier`) | ≤ 30 ns/op | **6.56 ns/op** |
| `bench_scavenge_4mb` (4 MiB young-gen ~50 % survival, including heap setup) | ≤ 5 ms wall | **740 µs** |
| `bench_collect_full_256mb` (STW full GC at 256 MiB live, `collect_full` only) | ≤ 50 ms wall | **119 µs** |

All four metrics clear their NF1 / NF2 budgets by an order of
magnitude on this host. The 256 MiB full-GC bench measures only
the `collect_full` call (heap setup is excluded via
`iter_custom`); the inner `mark` + `sweep` pass empties a
~250 K-object live set in roughly 0.5 ns per object on the
Apple M1.

## How to reproduce

```bash
cargo bench -p otter-gc --bench bench_alloc_young_bump
cargo bench -p otter-gc --bench bench_alloc_with_barrier
cargo bench -p otter-gc --bench bench_scavenge_4mb
cargo bench -p otter-gc --bench bench_collect_full_256mb
```

The cage size is bumped to 512 MiB inside each bench's
`main` via `init_cage_with_size`. On a 16 GiB host the resident
working set stays well below 1 GiB across the whole bench
suite.

## Re-baselining policy

Re-record this file when:

- A non-trivial allocator path lands (any change to
  `space::NewSpace::alloc` or the bump fast path);
- Card-table layout changes (any change to the `barrier::write_barrier`
  hot path);
- Marking moves to incremental (task 86) — at that point the
  scavenge / collect-full numbers are no longer load-bearing
  for this baseline and the file should fork into a Phase-2
  baseline.

The bench scenarios themselves should not change between
baselines; only the numbers do.
