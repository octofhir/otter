# Engine improvement backlog

Only open work belongs here. Remove a row when its acceptance checks pass;
implementation history and completed validation belong in Git and raw reports.
A targeted pass does not establish a green workspace or market leadership.

## Correctness and safety first

| Priority | Open work and owner | Acceptance checks |
|---|---|---|
| P0 | **Finish the closing gate.** `scripts/gate.sh`, runtime integration tests. The last complete differential gate failed; subsequent slices passed focused matrices only. | Reclassify failures on current HEAD against a comparable baseline, fix their causes, and complete the unchanged gate. Preserve native admission, exact generations, no replay/fallback, real OOM and moving-GC assertions. |
| P0 | **Diagnose GC-stress watchdog failures.** `scripts/dev/gc-stress.py`, `otter-difftest`, GC/VM. Existing full reports contain timed-out candidate processes. | Preserve exact paths, watchdogs and outcomes; attribute startup, verification and mutator cost separately. Complete the full checked-profile corpus at strides 1–16 with `status: passed`. No filtered corpus or timeout-as-agreement verdict. |
| P0 | **Reproduce and isolate startup-image failures.** `isolate_snapshot_capture`, startup-image restore. An older restored release executable crashed before source; current shipped-image coverage is incomplete. | Compare raw/restored launches from the same current binary/profile, including fresh processes, restored allocation pressure and weak/bootstrap bodies. Classify the old crash separately from current failures. |
| P1 | **Separate Array semantics from materialization limits.** `array_prototype.rs`, `array_prototype/{mutate,join}.rs`; inspect remaining `MAX_ARRAY_LIKE_PROBE_LEN` uses individually. Fill still truncates its span; join retains a length ceiling and eager length-sized scratch vectors. | Concrete public regressions for each driver; complete live ordered property attempts, bounded scratch/root retention, cancellation and explicit allocation/output limits. Preserve UTF-16, species and observable error order. |
| P1 | **Account for dense bulk native work.** `array_fill` and existing bulk helpers. Generic-loop checks alone do not control admitted dense fill. | Reject/Yield/interrupt proofs on the actual admitted path, correct committed-prefix policy, and comparable hot-path measurements. Keep reservation, descriptor/prototype proofs and barriers intact. |
| P1 | **Resolve remaining broad-suite failures.** Runtime GC/JIT, snapshot, compile-fail and audit fixtures. Older baseline reports contain failures outside the repaired global-binding/known-native targets. | Reproduce current failures first; distinguish engine bugs, stale contracts and environment restrictions. Fix owners and fixtures together without weakening safety or native-execution proofs. |
| P1 | **Diagnose flaky accounting observations.** `ipc` incoming-charge lifetime and compile-cache pruning. Both failed previously and also passed repeated standalone runs. | Controlled scheduling/lifetime reproductions, exact resource ownership assertions and repeated isolated/concurrent checks. Passing retries alone do not close the task. |

Start broad-suite reclassification with `gc_atomics_wait_async_result`,
`code_chunk_eviction`, `jit_actual_only_calls`, `jit_canonical_homes_gc`,
`jit_stack_owned_array_construct`, snapshot restore, runtime `compile_fail`,
`dependency_graph`, `step_trace_golden` and `tokio_spawn_audit`. These are leads
from prior failures, not a claim that every named target currently fails.
The retained baseline identities are in
`/private/tmp/otter-queue-baseline-audit/comparison.json`; raw gate failures are
in `/private/tmp/otter-engine-full-difftest{,-failures}.json`.

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
