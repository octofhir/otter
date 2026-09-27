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

## Rejected captured-environment experiment

The full owner/index implementation passed differential 58/58, JIT 283/283,
GC stress 1..16, focused rooting/snapshot/reclamation tests, x86 compilation and
VM/runtime compile-fail boundaries. It failed the resource objective:
Earley instructions -2.27%, RSS +14.83%; TS +0.94% / +1.32%; zlib +1.59% / +5.59%.
Every workload completed. Exact counters and allocation attribution are in
`2026-09-27-captured-environments.md`.

All experimental VM/JIT code, fixtures and layout documentation were restored
together. No old/new flag exists. The independent pending-tail GC tracing fix
and its regression test remain, together with the new alias differential corpus.
Both mandatory gates pass again on the retained state: differential 58/58,
JIT 283/283; pending-tail GC tests 4/4. The next compiler analysis is in
`arguments_elision.rs` and is not yet wired into emission.

Preserved artifacts under the ignored parity result directory:
- `rejected-captured-environments.patch`
- `otter-rejected-environments`, `allocation-probe-rejected-environments`
- `environment-reference/`, `environment-final/`: all seven adjacent counters
- `environment-{earley,ts}-{before,after}.json`: allocation attribution
- `environment-zlib-events.json`: successful untruncated diagnostic capture
- `otter-before-environments`: restored production engine baseline
- `allocation-probe-before-environments`, `otter-before-element`

## Next executable architectural slice

Investigate arguments object materialization. Earley allocates 14.6 million
exotic/symbol sidecars totaling 5,022,525,056 bytes; `collect_arguments_value`,
sidecar allocation and symbol installation are visible in its native profile.
Determine the hot creating functions from a successful JIT-attributed profile.
The source includes read-only local aliases (`sc_list`) and direct indexed reads
(`sc_consStar`), but do not infer execution frequency from source alone.

Primary sources being studied: JSC `DFGArgumentsEliminationPhase.cpp`,
V8 `js-create-lowering.cc`, SpiderMonkey `ScalarReplacement.cpp`. Otter already
avoids materialization for `apply`-only arguments use via AST proof and
`CallForwardArguments`; inspect that mechanism before introducing a new one.
Potential replacement: explicit activation argument operations for proven local
non-escaping uses, with precise alias/mapped/prototype/eval semantics and one
canonical materialization when observable. Alternatively dedicated arguments
storage may remove sidecars when escape is frequent. Select using measured
sites, not a generic promise of escape analysis.

Start at `crates/otter-compiler/src/functions.rs`, `hoist.rs`, `calls.rs`,
`crates/otter-vm/src/jit_spread_call_ops.rs`, `arguments_object.rs`, and
`object.rs::arguments_direct_snapshot`. Preserve the existing generated forward
call path; do not reintroduce Rust-side argument forwarding.

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
