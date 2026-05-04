# GC Phase 1 baseline (2026-05-04, host: Apple M1, macOS 25.2)

Numbers captured by Criterion on `cargo bench -p otter-gc`,
release profile, single iteration measurements averaged across
the run length shown. Re-baselined for task 74 — the per-tag
counter writes added to the alloc fast path raise
`bench_alloc_young_bump` by ≈ 1 ns/op (still well under the
10 ns NF1 budget); `bench_scavenge_4mb` and
`bench_collect_full_256mb` reabsorb the per-GC reconciliation
walk into the existing sweep, so the per-cycle costs stay
within noise of the previous baseline.

| Bench | Target | Median measurement |
| --- | --- | --- |
| `bench_alloc_young_bump` (1 × `Cell` alloc) | ≤ 10 ns/op | **3.61 ns/op** |
| `bench_alloc_with_barrier` (2 × alloc + 1 store via `write_barrier`) | ≤ 30 ns/op | **7.17 ns/op** |
| `bench_scavenge_4mb` (4 MiB young-gen ~50 % survival, including heap setup) | ≤ 5 ms wall | **816 µs** |
| `bench_collect_full_256mb` (STW full GC at 256 MiB live, `collect_full` only) | ≤ 50 ms wall | **107 µs** |

All four metrics still clear their NF1 / NF2 budgets by an
order of magnitude on this host. The 256 MiB full-GC bench
measures only the `collect_full` call (heap setup is excluded
via `iter_custom`); the fused sweep + per-tag accounting now
covers both reclamation and stats reconciliation in one walk
over the live set.

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
- The `crate::stats::GcStats` counter layout changes in a way
  that adds or removes alloc-fast-path stores;
- Marking moves to incremental (task 86) — at that point the
  scavenge / collect-full numbers are no longer load-bearing
  for this baseline and the file should fork into a Phase-2
  baseline.

The bench scenarios themselves should not change between
baselines; only the numbers do.
