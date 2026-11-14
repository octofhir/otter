# Engine improvement backlog

Only open work belongs here. Remove a row when its acceptance checks pass;
implementation history and completed validation belong in Git and raw reports.
A targeted pass does not establish a green workspace or market leadership.

## Correctness and safety first

| Priority | Open work and owner | Acceptance checks |
|---|---|---|
| P0 | **Finish the closing gate.** `scripts/gate.sh`, runtime integration tests. The last complete differential gate failed; subsequent slices passed focused matrices only. | Reclassify failures on current HEAD against a comparable baseline, fix their causes, and complete the unchanged gate. Preserve native admission, exact generations, no replay/fallback, real OOM and moving-GC assertions. |
| P0 | **Diagnose GC-stress watchdog failures.** `scripts/dev/gc-stress.py`, `otter-difftest`, GC/VM. Existing full reports contain timed-out candidate processes. | Preserve exact paths, watchdogs and outcomes; attribute startup, verification and mutator cost separately. Complete the full checked-profile corpus at strides 1–16 with `status: passed`. No filtered corpus or timeout-as-agreement verdict. |
| P1 | **Separate remaining Array semantics from materialization limits.** `array_prototype.rs`, `array_prototype/join.rs`; inspect remaining `MAX_ARRAY_LIKE_PROBE_LEN` uses individually. Join retains a length ceiling and eager length-sized scratch vectors. | Concrete public regressions for each driver; complete live ordered property attempts, bounded scratch/root retention, cancellation and explicit allocation/output limits. Preserve UTF-16, species and observable error order. |
| P1 | **Audit remaining typed-array native work and live bounds.** Start with `binary/typed_array_prototype.rs::impl_set` and search loops. | Reproduce each issue through the public runtime before changing its owner. Verify Reject/Yield/interrupt and committed prefixes; preserve raw NaN bits, BigInt roots and shared-storage semantics. Reuse the common view validator where the algorithm requires it; do not merge different preparation contracts. |
| P1 | **Resolve remaining broad-suite failures.** Runtime GC/JIT, snapshot, compile-fail and audit fixtures. Older baseline reports contain failures outside the repaired global-binding/known-native targets. | Reproduce current failures first; distinguish engine bugs, stale contracts and environment restrictions. Fix owners and fixtures together without weakening safety or native-execution proofs. |
| P1 | **Diagnose flaky accounting observations.** `ipc` incoming-charge lifetime and compile-cache pruning. Both failed previously and also passed repeated standalone runs. | Controlled scheduling/lifetime reproductions, exact resource ownership assertions and repeated isolated/concurrent checks. Passing retries alone do not close the task. |

Start broad-suite reclassification with `gc_atomics_wait_async_result`,
`code_chunk_eviction`, `jit_actual_only_calls`, `jit_canonical_homes_gc`,
`jit_stack_owned_array_construct`, runtime `compile_fail`,
`dependency_graph`, `step_trace_golden` and `tokio_spawn_audit`. These are leads
from prior failures, not a claim that every named target currently fails.
The retained baseline identities are in
`/private/tmp/otter-queue-baseline-audit/comparison.json`; raw gate failures are
in `/private/tmp/otter-engine-full-difftest{,-failures}.json`.

The current TypedArray `copyWithin` subset retains 10-second timeouts in
`coerced-values-end-detached-prototype.js`, `coerced-values-end-detached.js`
and `coerced-values-start-detached.js`. Exact-path repeated baseline and
candidate captures are under
`benchmarks/results/typed-array-copy-within-2026-11-14/`. Attribute constructor,
preparation and operation costs separately without relaxing the watchdog.
An isolated checked-profile probe of the same 80 constructor/factory combinations,
without calling `copyWithin`, spent about 92 ms in constructors and 2.7 s in
factory preparation. Investigate array-like indexed stores and the JavaScript
byte-copy loops used to prepare resizable buffers. This is a lead, not a complete
attribution of the debug-runner timeouts. Probe sources, hashes and individual
observations are under `benchmarks/results/typed-array-reverse-2026-11-14/`.

## Establish current compatibility evidence

- Regenerate a complete canonical Test262 report with per-path outcomes,
  actual runner hash, corpus commit, target, semantic configuration and skips.
  `ES_CONFORMANCE.md` is historical; its 99.98% cannot support a current claim.
  Compare affected subsets before/after each semantic change by exact path and
  outcome. Keep crashes, timeouts, unsupported features and skips visible.
- Run relevant release/checked native and GC proofs on ARM64 and x86_64, then
  Linux and Windows. Rosetta execution does not establish Linux/Windows support.
- Verify deny-by-default capabilities, resource exhaustion and runtime reuse
  through embedding/CLI boundaries. Keep parser-only, runtime-only extension,
  no-default-feature and normal shipped-CLI builds independently checked.

## Earn performance and memory claims

| Axis | Required evidence and next work |
|---|---|
| Execution | Use `benchmarks/README.md` and the existing engine harness. Record current Otter, Node/V8 and Bun versions on the same idle hardware; check equal semantics/results. Separate interpreter, Template and production policies, startup, warmup and steady state. Attribute a measured bottleneck before changing lowering or adding fast paths. |
| Memory and GC | Measure peak RSS, retained live/external bytes, allocation rate and pause distributions on the same workloads and limits. Include long-lived runtime reuse, closures/eval, snapshots and native reentry; verify root retention stays bounded. Separate nested minor work from full-service accounting. |
| Startup and loading | Measure raw and startup-image processes, parse/compile/module loading and fresh/reused runtime cases separately. A cache hit or parser microbenchmark is not a whole-engine win. |
| Build and footprint | Repeat at least three alternating comparable clean-build pairs, measure linking separately, and restore between incremental edit triggers. Measure retained shared-routine metadata and diagnostics footprint. Remove dependency edges only when their functionality has another current owner or no caller. No build speedup is established by the old busy-host samples. |
| Regression policy | Keep raw observations, binary/source identities, commands, limits and variance. Set workload-specific budgets from a reproducible baseline; retain per-workload regressions rather than hiding them in one aggregate score. |
