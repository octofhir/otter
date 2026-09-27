# Fixed-work parity checkpoint

Main, macOS ARM64. Goal remains active; parity has not been reached.
Do not switch the repository toolchain, rerun full Test262 or run the full
runtime suite without a concrete reason. Stable Rust remains 1.97.1.

## Retained architectural changes

- `bd9285bf`: linked old-space free lists. Earley instructions −15.45%.
- `cd3aa798`: source-owned indexed layouts and scalar Machine indexed access.
  Zlib instructions −31.25%; seeded crypto −17.09%.
- `2f3cf313`: whole-script allocation census using existing per-tag counters.
- `866b7be5`: trace pending allocation tails before publishing their payload.
- `d9e81603`: arguments replacement: compiler alias/escape proof, explicit activation
  length/index operations, both Template/Machine backends, one canonical cold
  materialization, traced native cache and saved iterator intrinsic. Earley
  instructions −27.59%; allocated bytes −38.15%; full GC 606 → 257. Final RSS
  −0.46% on Earley is not a robust corpus-wide memory win. TS/zlib peak RSS
  varies in both directions and remains unresolved.

- `76aa818d`: inline closure captures; one old-space closure owns its trailing
  four-byte cell references; the separate GC capture-array type is removed.
  The allocator takes a mutable slice and construction uses SmallVec storage.
  Earley instructions −3.91%..−4.05% in three pairs; allocated bytes −1.49%.
  RSS is unresolved (Earley −0.66%..+0.07%; zlib −5.43%..+1.75%).

Every retained implementation was validated with the two required gates.
Latest: difftest 59/59, JIT 283/283; closure/arguments stress 1..16 in interpreter,
Template and production (96/96, Node agreement, slot verification). Closure
unit tests 9/9, shared-capture snapshot and cycle reclamation pass. Previous
arguments checks include bytecode 96/96,
analysis 4/4, native root rewrite, intrinsic roots, generated arguments,
Machine/cold identity, iterator snapshot and snapshot round-trip tests pass.
x86 cross-compilation passes; x86 execution was not tested on this ARM64 host.

Reports:
- `2026-09-27-engine-research.md`
- `2026-09-27-old-free-list.md`
- `2026-09-27-indexed-access.md`
- `2026-09-27-arguments-reads.md` — all seven final counters and V8/JSC ratios
- `2026-09-27-young-bindings.md` — rejected nursery allocation policy
- `2026-09-27-closure-captures.md` — seven counters, repetitions, GC and code size
- `2026-09-27-null-prototype.md` — separate user-requested investigation

## Rejected experiment

The grouped owner/index captured-environment representation passed correctness
checks but regressed RSS: Earley +14.83%, TS +1.32%, zlib +5.59%. It was completely
removed. See `2026-09-27-captured-environments.md`. The independent pending-tail
GC fix and alias regression corpus remain. No old/new runtime flag exists.

A second experiment moved local binding cells to the nursery while preserving
pinned globals. It regressed Earley instructions by 6.273% in a fresh pair and
was removed before full validation. See `2026-09-27-young-bindings.md`; do not
repeat a policy-only nursery change. Its root-audit findings remain relevant
when directly captured values become moving references.

## Next measured mechanism

The successful post-arguments Earley profile has 1739 isolate self samples:
upvalue allocation 119, spine allocation 82, generated upvalue initialization 60,
runtime closure construction 98, closure allocation 20. Together these disjoint
self samples are 21.8%; additional shared GC cost is not fully attributable.
The closure-tail change removes the separate spine allocations. The fresh profile
in `earley-after-closure-tail-profile/` records 1790 isolate self samples: local
cell allocation 94, generated initialization 54, closure construction 127 + 36,
closure allocation 27. The allocation census
still counts 110,900,416 binding cells. Bytecode capture-graph analysis finds
8 of 10 own `deriv_trees` bindings initialized once from simple parameters, with
no descendant writes. Investigate a four-byte tagged capture slot that carries
immutable values directly, while mutable bindings retain shared cells. Prove
initialization dominance and exclude dynamic scope/mapped arguments; a store
count alone is insufficient. `next-capture-design.md` contains the implementation
audit checklist. No tagged capture representation is implemented yet.
Do not repeat the rejected eight-byte owner/index design without addressing its
reference-size and retained-memory costs. All GC roots, native frame layouts,
spines, compiler capture consumers and snapshots must change together if their
representation changes.

The user's null-prototype lead is also measured. V8 forces dictionary mode for
null-prototype literals but has dictionary-index IC handlers, so “every read
hashes” is inaccurate. Otter already produces shape-proven direct property loads
for both null literals and Object.create(null). Creation is the gap: the null
literal falls out of Machine on DefineDataProperty. In the exploratory retained
100k-object workload it retires 51.7% more whole-process instructions than an
ordinary literal. These tests are separate from the agreed seven workloads.

## Raw artifacts and reproducibility

Everything below lives in ignored `benchmarks/results/parity-2026-09-27/`:
- `closure-tail-reference/`, `closure-tail-final/`, `closure-tail-repeat/`:
  complete seven-workload counters and alternating Earley/zlib repetitions.
- `closure-tail-{earley,ts}-allocation.json`,
  `closure-tail-{earley,zlib}-events.json`: allocation and compiler evidence.
- `closure-tail-stress/results.json`, `closure-tail-{difftest-gate.json,jit-gate.txt}`:
  final validation of inline captures. Other `closure-tail-*-test*.txt` are focused checks.
- `otter-before-closure-tail`: exact arguments baseline executable, including
  the pending-tail GC fix; final executable hash is in the closure report.
- `arguments-reference/`, `arguments-count-final/`: seven before/final CLI runs,
  `/usr/bin/time -l`, stdout, commands and executable/source hashes.
- `arguments-final/`: first candidate with a redundant frame-cache zero store;
  `arguments-ts-{reference2,repeat2}/`: its TS repeat.
- `arguments-{earley,ts}-allocation.json`: allocation census.
- `arguments-count-{earley,zlib}-events.json`,
  `arguments-reference-earley-events.json`: compiler time and generated size.
- `earley-after-arguments-profile/`: successful native/JIT attribution;
  `earley-arguments-profile/`: pre-change attributed profile.
- `arguments-count-{jit-gate.txt,difftest-gate.json,stress.json,x86-check.txt}`:
  final gates and stress evidence. Other `arguments-*-tests.txt` hold focused checks.
- `null-prototype-final-counters/`, `null-prototype-artifacts/`,
  `null-prototype-events.json`:
  independent construction/read investigation.
- `otter-before-environments`: preserved indexed-access baseline executable.
- Rejected experiment: `rejected-captured-environments.patch`,
  `otter-rejected-environments`, `allocation-probe-rejected-environments`,
  `environment-{reference,final}/`, `environment-{earley,ts}-{before,after}.json`.

The older `arguments-reference` baseline binary predates the independent pending-tail GC fix. The candidate
includes it; the reports identify both revisions rather than pretending that the
preserved executable was built from the immediately preceding commit.

Mandatory gates after the next coherent engine change:
`cargo run --release -q -p otter-difftest`
`cargo test --release -q -p otter-jit --lib`
Then measure all seven fixed-work workloads, RSS and affected JIT code size/time,
make a signed logical commit in main, and continue from the next measured gap.
Leave the user's untracked `PROMPT_OCTANE_GAP.md` untouched.
