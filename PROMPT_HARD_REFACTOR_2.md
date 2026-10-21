# Otter — hard refactor, round 2: build a competitive production engine

You are a VM / compiler engineer. The optimizing tier now exists and is in
production on AArch64. This round deletes the remaining wrong structures in it
and around it. You do not tune instructions inside a wrong design. You replace
the design, delete the old one, and measure.

Internal breaking changes are explicitly authorized: APIs, ABI, object layouts,
bytecode, diagnostics and artifact formats may change in place. Update every
producer, consumer, test and document together. Preserve JavaScript semantics,
GC safety and capability enforcement. Do not preserve a bad boundary for the
sake of compatibility.

## Objective and evidence

Bun is the primary competitive baseline; Node is a second reference. The goal
is leadership across a declared workload and platform matrix, including cold
start, time to first useful result, sustained execution, peak and retained
memory, GC pauses, compilation cost and conformance. Fixed-work instruction
counts diagnose overhead; they are not the user-visible success metric.

- Report each workload and metric separately, including losses and unsupported
  cases. An aggregate score cannot establish leadership in every case.
- Establish same-machine, same-input, validated baselines before making a
  speed claim. Record engine versions, binary/script hashes, source revision,
  dirty-tree provenance, platform, architecture and execution mode.
- Separate end-to-end cold runs, warm steady-state runs and compile-only runs.
  State wrapper differences explicitly; do not silently compare Script and
  Module semantics or different algorithms.
- Run at least five measured fresh-process repetitions, rotate engine order,
  preserve raw samples, and report median and spread for elapsed time,
  instruction count and peak RSS. Increase repetitions when the observed
  difference overlaps run-to-run noise. Check outputs before scoring.
- Measure retained memory and GC pause distributions with their own harnesses;
  maximum RSS does not measure retained heap, and repeated process starts do
  not measure warm throughput. Keep profiling separate from timing.
- Use real programs, polymorphic/uncacheable cases, short-lived tasks and
  allocation pressure alongside microbenchmarks. Never specialize for a
  benchmark's names, bytecode, constants or expected results.
- Reproduce every diagnosis on the current tree. Saved handoff reports are
  historical observations unless their binary/source provenance matches.
- Prioritize the largest measured cost with a causal explanation. Reference
  engines supply evidence and ideas; their architecture is not a requirement
  to copy. Compare competing designs when the cost is uncertain.

The targets below are intermediate checkpoints, not the definition of victory.
Final competitive claims require per-case Bun comparisons on the declared
matrix with no hidden correctness, coverage or memory tradeoff.

Read this file whole, then read in `~/work/engines/v8`:
- `src/maglev/maglev-regalloc.cc`, `maglev-regalloc-data.h`,
  `maglev-code-generator.cc` (safepoint table emission, spill slot regions)
- `src/codegen/safepoint-table.*`, `src/codegen/maglev-safepoint-table.*`
- `src/maglev/maglev-graph-builder.cc` (feedback use, `EnsureType`, allocation
  folding), `maglev-ir.h` (node set, `InlinedAllocation`, `CallKnownJSFunction`)
- `src/execution/tiering-manager.cc`, `src/deoptimizer/deoptimizer.cc`
- `src/objects/map-updater.cc`, `src/objects/js-objects.cc` (slack tracking)
- `src/ic/stub-cache.*`, `src/ic/accessor-assembler.cc` (`TryProbeStubCache`)
- `src/codegen/x64/` and `src/maglev/x64/` (second backend)

Compare approaches from other engines as well:
- JavaScriptCore: structure-owned layout, inline properties and butterfly,
  DFG/FTL allocation, OSR exit recovery and compilation cost. Read the pinned
  source or official WebKit documentation; verify what the local checkout
  actually contains. Its GC assumptions must not be imported into Otter's
  moving young generation without a complete rooting proof.
- SpiderMonkey in `~/work/engines/gecko-dev`: `js/src/jit/CacheIR.h`,
  `WarpCacheIRTranspiler.cpp`, `BacktrackingAllocator.cpp`, `Safepoints.cpp`,
  and `js/src/vm/{Shape,NativeObject}.h`. Compare shared IC semantics,
  specialization from observed stubs, fixed/dynamic slots and precise roots.
- Reference implementations inform an independently written design. Record
  source revisions and the invariant being studied; do not copy code, inherit
  unrelated complexity, or treat another engine's threshold as an Otter policy.

## Current working slice (2026-10-04, Moscow)

The uncommitted cut-over contains CFG-aware canonical homes, deterministic
retraining, true uncacheable feedback, exact native-generation ownership and
invalidation of actual splices. R/F now replaces whole-body/loop-span admission
estimates with actual attempted source opcodes, one canonical retained scalar,
explicit Template prefix boundaries on both backends, and disabled accounting
in Graph's shared emitters. Post-compiler resource admission charges actual
retained mappings and metadata before publication; unchanged headroom suppresses
repeated hooks until capacity or source feedback changes.

The incoming active-frame retention fixture demonstrated an 8,519,608-byte
live-heap delta after the payload's last use, disappearing only after native
return. Dead tagged-home expiration now clears physical homes at actual
collecting/reentrant calls and recipe-specific cold exits. The strict runtime
proof passed: the payload contributes the same 8,519,608 bytes while live,
zero bytes after its last use inside the still-active Graph BODY frame, and
zero after return. This measures managed live heap, not process RSS or a
competitive pause-time result.

Targeted checks passed: actual-work runtime tests 9/9 on AArch64 and 7/7 on
x86_64; actual native saturation/ABI/flags tests on both backends; canonical
home expiration including real large-offset stores; 31 Float64 homes across
moving GC and own Graph OSR; constructor and speculation/invalidation tests
under stress stride 1 plus heap verification. Method replacement legitimately
retires the immutable caller guard after its active call finishes; its regression
proves the exact native generation and zero deopt rather than demanding that
the old guard remain installed.

Test262 now has one complete canonical row list with typed skip causes. Workers
and reports reject wrong/duplicate/missing identities, malformed diagnostics,
inconsistent rollups and failed traversal; no absent row implies Pass. Reports
record the actual executable hash, target, assertions and effective policy.
Full publication requires exact full-corpus coverage and the same publishing
runner. Historical aggregate-only public data is archived unchanged and remains
unpublished until a genuine complete capture; do not manufacture historical rows.

These changes are not accepted until the coherent full tests/clippy, checked GC,
difftest, conformance and measured performance gates pass. In particular, the
100-repeat all-corpus checked GC acceptance and current Bun comparisons have
not run. Saved targeted successes are not a finished gate.

The next parallel source wave is written and awaiting its combined gate:
Graph empty-object/empty-array LAB allocation with canonical source-realm cold
allocation, one typed VM field location/layout contract across both backends,
x86_64 Template native bounded method linkage and corrected constructor plan
selection, and exact-generation ownership of stale exits and completion costs.
Their new regressions exercise real native execution, moving GC, additional
realms, live storage selection and reentrant generation replacement. This is
initial N/O/C work, not completion: nonempty allocations, shape-owned capacity
with persistent inline fields, cheaper call entries and x86_64 Graph lowering
remain subsequent implementation work. Do not report the new wave as passing
until its frozen-source checks have actually completed.

The table and diagnoses below describe the incoming tree. Reproduce a remaining
cost before changing it; do not reintroduce an old mechanism because a handoff
paragraph still describes it. The RSA fixture now resets its existing RNG before
pool initialization and prints a deterministic result. The zlib fixture now requires decoder stream-end, exact length and all 720
full binary comparisons with a deterministic input checksum. Node, Bun and the
preserved Otter agree on its result; its added validation cost is part of the
new declared workload.
Preserve the original fixtures and hashes and rerun every engine on the corrected
input; old timings do not become equivalent-work measurements retrospectively.

## Historical incoming-tree observations (fixed-work instructions)

These saved figures are diagnoses, not current equivalent-work performance
evidence. Crypto and zlib inputs/validation changed; rerun all engines on their
current hashes before using ratios or claiming a milestone.

| workload | Otter | Node | gap |
|---|---|---|---|
| ts | 95.7G | 21.0G | 4.6x |
| zlib | 99.93G | 23.1G | 4.3x |
| crypto | 8.94G | 2.93G | 3.0x |
| earley-boyer | 80.0G | 18.7G | 4.3x |
| ast_ctor | 4.48G | 3.30G | 1.36x (Bun 2.06G) |
| mega_method | 3.01G | 4.03G | 0.75x (Bun 1.65G) |
| fib | 1.93G | 0.95G | 2.0x (Bun 0.57G) |
| startup (`console.log`) | 0.85G | 0.26G | 3.3x |

The optimizing tier is `crates/otter-jit/src/graph/`: a graph built from bytecode
and feedback; `Generic` nodes run the baseline (Template) operation on the frame
window. Calls are inlined, with multi-frame deopt. There is polymorphic dispatch and
loop-invariant code motion. It is AArch64 only: x86_64 runs Template code.

## Diagnosis (measured this session, not guessed)

**1. Register allocation and GC roots have the wrong model.**
- The allocator is one linear walk. Spill slots are assigned lazily, at a later
  use, but written at the value's definition. Safepoint roots are computed
  during the walk, so a collecting point allocated before the slot is assigned
  does not root it. Code later reloads the pre-collection address from that slot.
  This is a use-after-move, found through a 7–10% GC-stress flake that took
  hours to localize.
- The patch (roots recomputed from the final spill map) is sound, but costs
  zlib +4.2G in the saved run. This motivates replacing duplicate root and
  storage mechanisms; it does not by itself prove that scanning more slots is
  cheaper. Reproduce the overhead and measure both scan and generated-code cost.
- Slow paths copy every live register into per-node "snapshot slots" and back.
  That is a second home for every value and a second root list.

**2. Feedback and deopt cascade.**
- A function whose property sites meet receivers the inline cache cannot
  describe (a setter installed on a prototype, mapped `arguments`) recompiles once
  per site: `fill` was compiled 8 times in a row, one InsufficientFeedback exit
  per site.
- The "this site never ran" test is "its IC is empty". An uncacheable site
  stays empty forever.

**3. Tiering is nondeterministic.**
- Measured compile duration feeds the profitability decisions. The same input
  tiers differently from run to run, so GC and deopt bugs show up as flakes,
  not failures.

**4. `Generic` is the default lowering.**
- Every `Generic` node publishes the window, is a call, spills everything,
  and reloads through `LoadWindow` afterwards.
- Allocation (`NewObject`, `NewObjectLiteral`, `NewArray`, closures), string
  concatenation, most comparisons and most runtime operations are still
  `Generic`.
- The graph has no allocation node.

**5. Object model (audit in memory `object_model_maps_plan_2026_10_03`).**
- In-object capacity belongs to the cell, not the shape. The first overflow
  copies every slot to a slab, so every load branches inline-vs-slab: about
  22 instructions per own-slot load in the graph tier against V8's 4–6.
- Five lookup-state bits live in the object header instead of shape identity:
  EXTENSIBLE, SLOT_ATTRS_OVERRIDDEN, CHAIN_LINK_OPAQUE, DICTIONARY_COMPATIBLE,
  USED_AS_PROTOTYPE.
- Delete always goes to dictionary mode. There is no slack tracking.
- The shape does not imply the body type.

**6. No second backend.** The deployment target (Linux x86_64) has no optimizing
tier.

**7. Calls.** fib is at 2x Node. Entry and trampoline overhead is in every call.
Forwarding (`f.apply(this, arguments)`) is native now in both tiers, but the generic
call path still classifies the callee in a trampoline.

## Target architecture (replace; do not patch)

### R. Register allocation, stack frames and precise safepoints

- **Frame layout.** Two spill regions with fixed boundaries: tagged slots, then
  untagged slots.
  - Both function and OSR entry initialize the tagged region to valid tagged
    immediates before a safepoint can observe it.
  - A tagged slot only ever holds a tagged value.
  - Use actual CFG liveness for all representations when saving registers:
    linear intervals alone include values whose definition did not execute on
    the current path. Never store those register contents into tagged homes.
- **Safepoints.** Every safepoint scans the whole tagged region and nothing
  else.
  - Delete per-node root lists, `RootLiveness`, `finish_roots`, snapshot slots
    and `saved_repr`.
  - A slow path that may collect spills its live tagged registers into their
    canonical tagged homes, never into snapshot copies.
  - Deopt and throw recipes read only canonical homes.
  - Bound and measure frame growth and dead-object retention. Whole-region
    scanning needs an explicit invariant for slot reuse and root lifetime;
    correctness and total execution cost decide the design.
- **Allocator.** A real allocator over live ranges (Maglev's straight-forward
  allocator with explicit spill decisions, or linear scan over intervals with
  splitting).
  - A value's spill home is decided before any code that depends on it is
    emitted.
  - Moves at block edges are resolved once, as parallel moves.
  - Temporaries are never allocated over inputs.
- **Acceptance.**
  - GC stress strides 1–16 run with a debug-assertions build (see Gates) on the
    whole difftest corpus, 100 runs per case, zero failures.
  - Recover the root patch's generated-code overhead without increasing total
    elapsed time, GC cost or retained memory outside measured noise.

### F. Feedback, deopt and tiering — deterministic, budget-driven

- One per-site state machine for property and call sites:
  `uninitialized → mono → poly(4) → megamorphic`.
  - It has an explicit "executed, uncacheable" state, which the optimizing tier
    lowers to the generic probe and never deopts on.
- A deopt sends the whole function back to the interpreter.
  - The interrupt budget resets (`SetInterruptBudget(kReset)`) and the next
    compile waits for it.
  - Sites learn from the full interpreted run, so there is one recompile, not
    one per site.
  - Define a deterministic retraining interval and a completion boundary;
    prevent immediate OSR or generated-call reentry from bypassing it.
- The tiering budget counts executed bytecode only.
  - No wall-clock input anywhere in tier decisions: the same input tiers the
    same way every run.
  - Delete the compile-duration cost model.
  - Count actual attempted source opcodes in the interpreter and Template
    tier. An entry, backedge, skipped branch, native helper's internal work or
    fused preflight is not a substitute for executed bytecode. Replayed
    interpreter operations are separate real attempts.
  - Expanded or fused Template operations retain exact source attribution.
    Flush completed work before reentrant helpers and abrupt exits; accepted
    inlines charge their actual callee's work without double charging callers.
  - Keep one address-stable scalar work owner per function, retained by code
    that references it. Highest-tier Graph execution does not need promotion
    accounting, including Graph's shared Template emitters.
  - Use static compiler-work estimates and a separate code-memory resource
    cap. Calibrate their integer units offline and measure admission changes;
    deterministic thresholds alone do not establish a performance win.
- A function that deopts N times for the same reason at the same site is never
  speculated on again at that site (V8's `deopt_count` per feedback slot).

### N. Native lowering for everything hot — `Generic` becomes rare

- **Allocation nodes.**
  - `AllocateObject { shape, capacity }`, `AllocateArray`, closure allocation
    and `NewObjectLiteral` / `NewArray` with initializing stores.
  - Allocation folding: one bump for consecutive allocations in a block, as in
    Maglev's `InlinedAllocation` / `AllocationBlock`.
  - The GC slow path is a single safepointed call.
- **String concatenation.** A string `+` node with a cons-string or flat-copy
  fast path.
- **Compares.** Every compare and branch over Int32/Float64/tagged/string
  feedback is native; the generic compare is a fallback node.
- **Runtime calls.** The remaining runtime operations become typed `CallRuntime`
  nodes with declared clobbers. They never publish the window.
- **Acceptance.**
  - Fewer than 5% of executed graph nodes on ts, earley-boyer and zlib are
    `Generic`. Measure with a per-kind execution profile.
  - Ordinary compiled operations do not materialize the interpreter window.
    GC, host reentry, exceptions and stack inspection still have one explicit
    frame/root contract. Do not erase an essential boundary to satisfy a counter.

### O. Object model as maps (plan: memory `object_model_maps_plan_2026_10_03`)

- **P0.**
  - One `FieldLocation` through every `slot*8` producer.
  - A debug layout verifier.
  - Roots keyed by `(prototype, capacity, kind)`.
- **B1–B5.** Opaque kinds, attribute changes, delete-last, prevent-extensions
  and prototype-use all become transitions or kinds. Delete every header flag
  bit.
- **C.** Capacity lives in the shape. Slot `i < capacity` is in-object,
  otherwise slab word `i - capacity`, with no migration. Delete `emit_slab_base`
  and `LoadSlotBase`.
- **D.** Slack tracking (7 constructions, shrink before any code bakes the
  lineage). Delete the constructor profile.
- **E.** Inline allocation for `new C()` and literals, through N.
- **Acceptance.**
  - An own-slot load in the graph tier is `ldr shape; cmp; b.ne; ldr`.
  - ts ≤ 3x Node; ast_ctor ≤ 1.2x Node.

### X. x86_64 backend for the graph tier

- `crates/otter-jit/src/graph/x64.rs`. The graph, allocation, deopt and
  safepoint model are target-neutral; only instruction selection and encoding
  are per target.
- The register file and scratch convention are declared per target. No AArch64
  assumptions may leak into graph passes.
- **Acceptance.**
  - Difftest, GC stress and test262 pass under
    `--target x86_64-apple-darwin` (Rosetta).
  - The fixed-work workloads run on x86_64. Use Docker/qemu for AVX2 kernels; see
    memory `x86_kernels_run_under_docker_qemu`.

### C. Calls

- **Known JS calls.** A call to a known function enters its current generation
  through the function-entry cell with one indirect branch. The callee's
  prologue builds the frame; the caller pushes only actuals.
- **Pinned registers.** The context and frame registers are pinned. There are
  no pc or call-site stamps on the hot path: stack walks read return addresses
  through a code-object PC table.
- **Trampoline.** The trampoline exists only for an unknown callee kind.
  Bound, native and class callees get their own known-kind entries.
- **Acceptance.** fib ≤ 1.3x Node.

## Order

1. **Finish the cut-over commit** (State at handoff below).
2. **R**: every later step depends on a sound, cheap frame model.
3. **F**: makes every later measurement deterministic.
4. **N** and **O** interleaved: N's allocation nodes need O's shape capacity.
5. **C**.
6. **X**: it can start in parallel once R lands, because R defines the
   target-neutral frame.

Every step replaces a mechanism and deletes the old one in the same commit.

## Milestones (fixed-work instructions vs Node)

1. **After R + F.**
   - GC stress is flake-free under the checked build.
   - zlib ≤ 3.5x; no workload regresses.
2. **After N + O.**
   - ts ≤ 3x; zlib ≤ 2x; crypto ≤ 2x; ast_ctor ≤ 1.2x; earley-boyer ≤ 2.5x.
3. **After C.**
   - fib ≤ 1.3x; ts ≤ 2x.
4. **After X.**
   - Every gate passes on x86_64.
   - Compare native x86_64 Linux results against same-host native competitors.
     Rosetta and qemu are correctness smoke tests and are never native
     performance evidence. Do not require equal cross-architecture ratios.

Report every milestone with before/after tables, profiles (region costs by node
kind) and wall clock.

## State at handoff

- **HEAD and commit.**
  - HEAD is `5f1c195c`. The cut-over is uncommitted WIP in `main`:
    `machine/` deleted, graph production on AArch64, x86_64 Template-only.
  - Commit message draft: `benchmarks/results/cutover/commit-msg.txt`. It still
    needs bullets for native forwarding, the GC root fix and the final numbers.
- **WIP contents.**
  - Native `apply(this, arguments)` forwarding in both tiers (`CallForward`
    node, shared `emit_push_forwarded`).
  - Property-site exit profiles carry the IC population, so a site re-speculates
    only after it learned.
  - A declined OSR compile no longer poisons entry promotion.
  - The GC root fix (`finish_roots` in `graph/regalloc.rs`); it is the patch R
    replaces.
  - Temporaries are no longer allocated over `Any` inputs.
- **Gates.**
  - The saved `benchmarks/results/cutover/gates.log` ends at difftest start;
    its existence does not establish a passing final gate. Revalidate before
    committing, and inspect every command's exit status and result artifact.
  - The previous tree passed test262 `language/` 100% and GC stress 48/48 on all
    corpora.
  - Difftest was 109/1. The one failure was the root flake, now fixed: 0/150.
- **Tests.**
  - Runtime test binaries touched: `jit_forward_arguments` (13/13) and the 8
    binaries in `benchmarks/results/cutover/test-*.log`.
  - Re-run them one at a time on the final tree before committing.
- **Results and scripts.** In `benchmarks/results/cutover/`: gate scripts
  `gates.sh` and `checks.sh`, the saved patch `gated.patch`, and the flake repro
  under `nst/`.

## Rules (non-negotiable)

- Work only on `main`. No branches, worktrees or stash; never checkout/restore
  over WIP; this prompt may be improved as explicitly authorized, but never
  commit `PROMPT_*.md`, `benchmarks/measurements/`
  research files or `scripts/dev/t262_tostring_probe.js`.
- Commits are GPG-signed. **No Co-Authored-By or any AI trailer.** Run the gates,
  read the results, then `git commit` in a separate call.
- Do not copy competitor code or add competitor references to production
  implementation comments or commits. Benchmark commands, provenance and
  comparison reports may name measured engines; honest attribution is required.
- No flags, env switches, adapters, versions, compatibility shims or bridges.
  No `thread_local` / `static mut`, and no bench-specific special cases.
  Temporary debug prints are allowed while investigating and are removed before
  any build that is gated.
- No debt notes or phase labels in comments or commits. New or changed modules
  keep accurate `//!` docs (`# Contents`, `# Invariants`, `# See also`).
- **Banned:**
  - shaving instructions inside an op whose contract is wrong;
  - adding conditions to a pass to route around a structural problem;
  - per-workload tuning;
  - dismissing a reproducible improvement solely because its percentage is
    small; report effect size and noise, and prefer changes with material impact.
- Never run `cargo test --workspace`. Run at most one or two runtime `--test`
  binaries at a time. Run one test262 runner at a time, with
  `--timeout 30000 --jobs 1`.
- No cargo during fixed-work measurement. Never edit crate sources while a
  background cargo build or test of the same tree runs.
- Gate commands need an explicit watchdog supported by the execution tool.
  Inspect disk space before large builds. Clean only identified disposable
  build artifacts while no build uses them; preserve binaries and raw evidence.
- Results and scripts live in the repo (`benchmarks/results/…`,
  `scripts/dev/…`), never `/tmp`.
- **Bugs.** When a gate fails, find the root cause in place: bisect by
  mechanism, instrument, fix. Never weaken a test to make it pass. A test that
  checks a mechanism deleted by design is rewritten against the mechanism that
  replaced it, in the same commit.

## Measure and gate

- **Fixed work:**
  `python3 scripts/dev/fixed-work.py benchmarks/results/<dir> --engines otter [node bun]`.
  Use repeated validated samples and inspect elapsed time and RSS alongside
  instructions. Re-run noisy or surprising results before believing them.
- **Checked build for GC gates.** Add a `[profile.checked]` that inherits
  `release` with `debug-assertions = true`. GC stress and difftest run against
  the checked binary to exercise invariant assertions. A checked Rust build
  does not automatically validate every generated machine-code load; GC
  verification, semantic comparison and canonical-home proofs remain required.
  Bisect GC flakes with
  `OTTER_GC_STRESS=<n>`, `OTTER_GC_VERIFY=1` and `RUST_BACKTRACE=1` on it.
- **Gates before every commit:**
  - freeze source edits before cargo starts; all gate scripts fail closed on
    command failure, missing/truncated results, mismatch or watchdog expiry;
  - `otter-difftest --timeout-ms 60000`, all cases;
  - GC stress strides 1–16 on every corpus case (`scripts/dev/gc-stress.py`)
    with the checked binary;
  - test262 `language/` (100%) and `built-ins/` (only the 2 known RegExp
    budget failures);
  - `cargo test -p otter-vm --lib`;
  - `cargo test -p otter-jit --lib`, also with `--target x86_64-apple-darwin`;
  - `cargo clippy --all-targets --all-features -- -D warnings`, plus otter-jit
    clippy on the x86_64 target;
  - the runtime test binaries the change touches, one at a time;
  - fixed-work on all 7 workloads; investigate changes outside noise, disclose
    tradeoffs, and compare the primary target metrics against Bun;
- Full gates run once per coherent completed change. Start iteration with the
  relevant regression, then broaden when that fix is ready. Do not weaken
  tests, normalize semantic mismatches away, or label an incomplete gate green.
- **Profiles:**
  - `sample <pid> <s> -file f.txt` with `scripts/dev/selfprof.py` and
    `bucketprof.py`;
  - graph region costs with `scripts/dev/graph/region_costs.py`, aggregated by
    node kind.
