# Active parity investigation checkpoint

Goal remains active. No parity claim and no external blocker.
Work stays on main; no stash or compatibility engine. User-owned untracked
`PROMPT_OCTANE_GAP.md` must remain untouched.

## Completed engine slices

1. `bd9285bf2836cdf4f26231d16bef36a8b25b1fa3` — signed G:
   intrusive old-space free lists replace BinaryHeap/Vec hole storage.
   Earley–Boyer instructions -15.45%, nearly unchanged RSS.
2. `cd3aa798d0c7e0c2bc0e4ca9c9ca8a59768899c3` — signed G:
   source-owned indexed layouts, scalar loads/indices/stores, semantic GVN,
   guarded-element inlining, both backend/allocator contracts updated.
   Zlib instructions -31.25%, RSS -12.54%; controlled seeded crypto -17.09%.
   TS RSS +1.30% on the adjacent repeat is an unresolved regression.

Reports: `2026-09-27-engine-research.md`, `2026-09-27-old-free-list.md`,
`2026-09-27-indexed-access.md`. All seven workload counters and V8/JSC ratios
are there. Remaining ratios: 2.51–17.76× V8, 6.21–15.85× embedded JSC.

Latest engine validation: differential 57/57 (GC stress 1/4/16), JIT 283/283,
focused `jit_machine_elements` 7/7; x86 cross-check passes. Cross-target
execution itself was not tested on this ARM64 host. Do not rerun full Test262
or the full runtime suite without a concrete reason.

## Next executable architectural slice

The captured-environments report records primary V8/JSC/SpiderMonkey source
mechanisms, a fresh uninstrumented Earley profile, allocation census and required
invariants. The census accepts this candidate: Earley creates 110,900,416
upvalue cells and 10,172,901 spines, with 606 full GCs. No environment
implementation has been started yet.

Start at `crates/otter-vm/src/upvalue.rs`, `upvalue_spine.rs`,
`frame_state.rs` (own-capture allocation loops), and
`call_ops.rs::jit_initialize_generated_upvalues`.

Replace individual cell bodies with grouped environment slots and one current
typed owner/index binding reference. Keep compiler binding semantics, including
individual loop renewal, shared aliases, eval, mapped arguments and derived-this.
Update all root tracing, native frame/spine layout, both JIT emitters, binding
proofs/barriers, image restore and code liveness; no old-cell adapter.

Audit `GcHeap::alloc_trailing_with_roots`: its pending-root closure currently
calls `T::trace_slots` rather than `trace_pending_slots`. A traced variable-sized
environment must not walk nonexistent stack trailing storage. Also account for
old pinned spines retaining young environments through the remembered set.
Grouping can retain sibling values and larger references grow spines; measure
those costs instead of assuming a memory win.

Before implementation binaries are preserved in the ignored result directory:
- `otter-before-environments` (same engine as indexed-access final measurements)
- `allocation-probe-before-environments`
- `otter-before-element` (preceding GC slice)

The new `otter-allocation-probe script <path>` diagnostic has built in release
and successfully run unchanged Earley and TS scripts. Its per-tag allocation
counts bracket execution and exclude bootstrap. Raw reports:
`environment-earley-before.json`, `environment-ts-before.json`.
These are attribution, not a substitute for CLI instruction/RSS measurements.

## Measurement and operational details

All raw artifacts live under `benchmarks/results/parity-2026-09-27/`.
`element-final/` has latest full production measurements; `element-ts-reference/`
and `element-ts-repeat/` record the repeated memory regression. Seeded crypto
is separate and does not alter the agreed original script.

Event-only zlib: 57 compile completions both revisions, 4.174609 → 3.872612 s,
16,268,012 → 15,257,860 generated code bytes. Full artifact captures hit their
2 GiB retention budget and cannot supply total code size. The corrected
`hotasm.py` preserves stdout/stderr, checks exit status, and disables CLI timeout.
`profile-zlib-element-complete/` has a successful, but artifact-truncated, run;
`plain-earley-element/` is the latest uninstrumented profile.

Use fff for indexed repository searches. Stable Rust remains 1.97.1. Separately
installed nightly-2026-09-27 (1.101.0-nightly) accepts Allocator/Vec::new_in/
Box::new_in without feature gates. No toolchain switch has been made. Allocator
API may support compiler arenas; it does not provide moving-GC pointer semantics.

Mandatory gates after the next coherent engine change:
`cargo run --release -q -p otter-difftest`
`cargo test --release -q -p otter-jit --lib`
Then all seven fixed-work runs through `/usr/bin/time -l`, RSS, relevant JIT
code size/compiler time, signed logical commit with signature status G, and
continue to the next measured gap. Do not stop at this checkpoint.
