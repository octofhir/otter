# Standby promotion pages survive scavenges

Investigation and closing validation: September 25, 2026.

## Starting point

A per-region profile of `derived-constructor` on ProductionTiered put about 14%
of its time under minor collection, and about 7% in `Page::drop` → `__bzero` +
`madvise`. Before every scavenge the promotion preflight reserves empty
old-space "standby" pages sized for the worst-case promotion; after the
scavenge all unused ones went back to the cage, which zeroes 256 KiB and
advises the OS for each, and the next reservation faulted fresh pages back in.

## Change

- `reserve_promotion_pages` counts pages still held from earlier reservations
  and takes only the shortfall from the cage.
- The scavenger no longer releases unused standby pages; a full collection's
  `reap_dead_pages` returns them. V8's memory allocator similarly pools
  semispace/old pages instead of unmapping them per GC.

## Final paired result

Same series and binaries as the polymorphic-load report. The attribution is by
kernel: `derived-constructor` and `string-concat` allocate on every iteration
and use neither polymorphic loads nor late-bound calls.

| Kernel / tier | Pair 1 ms | Pair 2 | Pair 3 | Wall | Instructions | Cycles |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| derived-constructor / Production | 9.270 → 8.012 | 9.212 → 8.042 | 9.268 → 8.163 | **−12.74%** | −1.33% | −7.10% |
| derived-constructor / Template | | | | −1.40% | −0.27% | −0.57% |
| derived-constructor / Interpreter | | | | −3.52% | −0.05% | −3.99% |
| string-concat / Production | 14.680 → 13.300 | 14.588 → 13.297 | 14.544 → 13.397 | **−8.71%** | −1.11% | −5.26% |
| string-concat / Template | | | | −8.10% | −1.07% | −4.81% |

The saving is mostly kernel time (page zeroing, `madvise` and refaults), which
the retired-instruction counter of the process only partly sees. The Template
`derived-constructor` body spends most of its time in reentrant stubs, so its
share is small.

At about **10 hand-written production Rust lines added**, the ProductionTiered
`derived-constructor` gain is **1.27 percentage points per added line**.

## Validation

`otter-gc` unit tests (58, including a new retention/shortfall/reap test),
the full gate, and the runtime GC-builder/rooting set at every stress level on
both architectures.

[Environment](2026-09-25-standby-promotion-pages-environment.json),
[run counters](2026-09-25-standby-promotion-pages-runs.csv),
[wall samples](2026-09-25-standby-promotion-pages-samples.csv).
