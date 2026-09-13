# Otter Engine Program

This is the sole repository-level implementation tracker. Otter is pre-user
and pre-stability: internal APIs, ABI, bytecode, metadata, artifacts, fixtures,
and tests may break whenever that produces the intended final architecture.
Completed slice history and measurements live in git and the ignored
`scratchpad/LEDGER.md`, not in this active plan.

## North star

Build an embeddable TypeScript/JavaScript engine that is simultaneously:

- safe for mutually untrusted tenants and deny-by-default hosts;
- predictable in CPU, memory, code, I/O, worker, and queue consumption;
- fast on representative application workloads, not only microbenchmarks;
- standards-compatible, deterministic, and diagnosable;
- pleasant to embed and extend without exposing VM or GC internals;
- portable across the supported macOS, Linux, and Windows targets.

"Best" is a measured outcome. Every performance or resource claim requires a
reproducible baseline, unchanged-result checksum, and before/after evidence.
Correctness, security, and boundedness are never traded for an unmeasured speed
claim.

## Non-negotiable invariants

1. The active dependency direction remains
   `otter-gc -> otter-vm -> otter-runtime -> product crates`.
2. There is one current runtime, bytecode format, compiled pipeline, resource
   policy, and extension boundary. Do not add compatibility readers, replay
   paths, dual writers, or parked shims to the active graph.
3. Every external effect is authorized at the last meaningful boundary before
   it occurs. Redirects, retries, aliases, imports, and delegated work cannot
   inherit an authorization decision for a different target.
4. Moving GC roots are explicit. Native and extension code uses `NativeCtx`
   handle scopes; raw heap mutation and compiled-frame plumbing remain private.
5. Untrusted input is structurally validated before execution. Validation is
   mandatory at the trust boundary, bounded, deterministic, and side-effect
   free.
6. Runtime limits are aggregate, observable, and fail closed. Per-isolate
   limits do not excuse unbounded process-wide threads, code, queues, sources,
   or external allocations.
7. Public runtime and extension DTOs are owned and `Send + Sync` friendly. Do
   not expose `Rc`, `RefCell`, raw VM handles, or GC carriers.
8. JS/TS parsing and transformation remain AST-first through the active
   frontend; never parse source semantics with regular expressions.
9. `lib.rs` files remain crate maps and small glue surfaces. Non-trivial new
   behavior belongs in focused, documented modules.

## Live snapshot

- Snapshot date: 2026-09-13. Validation starts from `a7aa87ab` plus the S0
  repair patch recorded in the benchmark metadata; the signed checkpoint
  contains the validated engine source.
- The final source gate passed 1665 tests: runtime 216, GC/VM units 997,
  bytecode/compiler/JIT 452. Scoped all-target/all-feature clippy passed.
- The corrected differential runner explicitly selects `--jitless` for
  Template and rejects common timeouts or signal termination. Runner clippy
  and 3 unit tests passed; release corpus 27/27 and affected-corpus GC
  strides 1..16 passed. Do not repeat these successful gates.
- Five fresh processes for each of seven Octane suites on Otter, Node/V8 and
  Bun produced 105 valid runs. Report, exact programs, binary hash, source
  identity, commands, medians/MAD and process memory are in
  `benchmarks/results/s0-20260913-global-script/README.md`.
- The measurement uses identical global Script semantics. The incomplete
  sibling file-mode run is marked abandoned and excluded from comparisons.
  Historical multi-file scores are not a valid before/after baseline.
- `ES_CONFORMANCE.md` still records the 2026-08-30 Test262 baseline:
  99.98%, 12 failures, zero crashes/timeouts. No new full Test262 or
  cross-platform acceptance is claimed by this ARM64 JIT slice.
- Production readiness and engine-wide performance leadership remain goals;
  these seven workloads still show a substantial gap to Node/V8 and Bun.

## Program scorecard

| ID | Lane | Priority | Status | Depends on | Active outcome |
|---|---|---:|---|---|---|
| H1 | Security | P0 | complete | none | every HTTP redirect hop and cached final alias is authorized |
| H2 | Isolation | P0 | complete | R1 | bounded, capability-aware Workers with deterministic shutdown |
| B1 | Bytecode | P0 | complete | none | mandatory panic-free verifier before any bytecode executes |
| R1 | Resources | P0 | complete | none | one aggregate runtime budget and typed exhaustion errors |
| R3 | Resources | P1 | active | R1 | measure bounded-resource cost, peaks, rejection, and recovery |
| E1 | Embedding | P1 | started by H1 | none | one reusable capability evaluator for all host surfaces |
| J1 | JIT | P1 | active lane | B1, R1 | one typed target-neutral compiled pipeline |
| J2 | Portability | P1 | queued | J1 | x86-64 parity over the same Machine IR |
| C1 | Conformance | P1 | complete | none | reproducible full Test262 baseline, 99.98%, residual documented |
| O1 | Observability | P1 | started | R1 | resource, scheduling, JIT, and denial telemetry |

Status meanings: `active` is the current implementation slice, `queued` has
a defined boundary but is not being edited, `complete` has passed its stated
acceptance gates, and `blocked` may be used only when the named dependency
truly prevents progress.

## H — Security and trust boundaries

### H1. Redirect capability enforcement

Complete. `otter-runtime` owns one cloneable evaluator; Fetch and one-hop
remote-module providers authorize every redirect before connection, preserve
deny-wins across host/authority and IPv6 spellings, and re-authorize cached
alias final URLs. Cancellation wins over late provider results. The built-in
clients remain pooled with automatic redirects disabled.

Focused denial-before-effect, allowed-follow/final-identity, redirect-loop,
malformed-location, cancellation, and cache-policy paths pass together with
runtime/Web clippy. DNS pinning, proxies, TLS, and network quotas remain
explicit future slices rather than implicit H1 scope.

### H2. Worker isolation

Complete. JavaScript workers are managed isolates: each `new Worker` spawns a
`RuntimeHandle` runner with the full wake-driven inbox, timers, host
completions, and dynamic imports; the raw thread/mpsc/poll-timer path is
deleted and a static source gate keeps it out. Parent↔child messages travel as
typed `RuntimeTask`s through the bounded inboxes after a fixed
validate/measure → admission → fallible clone → enqueue → detach pipeline with
checked arithmetic. Workers and messages charge the shared main ledger and a
finite per-family hard-limit ledger (workers, queued messages, queued message
bytes, single-message bytes) atomically per account; nested workers inherit
the same family, account, and capabilities, and an unlimited main ledger
cannot bypass the family. Each worker pre-reserves a guaranteed terminal
credit so its Error/Closed outcome lands exactly once even through a full
parent inbox; `terminate()` cancels the blocking `Atomics.wait` agent,
interrupts, shuts down, and joins deterministically. A direct `Runtime`
rejects `Worker` synchronously without resource effect. Census-to-baseline,
family-limit, nested-worker, transfer-preservation, oversized-graph,
terminal-race, busy-inbox, idle-wakeup, and GC-stress tests pass.

Worker-scoped capability narrowing is landed (see E1): `new Worker(url,
{ otter: { capabilities } })` requests a subset, and the child runs under
the intersection of the parent set and the request.

### H3. Remaining effect surfaces

Inventory filesystem, sockets, subprocesses, environment, clocks, randomness,
dynamic native installation, in-process snapshot restore, FFI, Node modules,
and Web APIs. Each surface must name its evaluator call, effect point, resource
charge, typed denial, cancellation behavior, and tests. Fix vertical slices;
do not create a second generic host-call registry.

Inventory captured (2026-08-25): every active surface checks its capability
before the effect with a typed denial — `Otter.serve` (net), `fetch` (H1
evaluator), module import (net), `otter:kv`/`otter:sql` (fs allowlists),
`otter:ffi` (ffi path), `process.execve` and `child_process` spawn (run),
`process.env` (env filter), `node:fs` bindings (fs_read/fs_write path
checks), snapshot restore (donor capability set). Deliberate non-gates:
Web Crypto RNG (spec-mandated), Worker construction (H2 resource bounds,
capability narrowing stays E1 scope), `Date.now` (interpreter opcode; a
gated clock would be a new E1 surface). No duplicated or post-effect checks
found. Remaining H3 work is E1 unification, not new gates.

## R — Resource accounting and isolation

### R1. Aggregate runtime budget

Define one runtime-owned budget model with explicit counters and reservations
for heap, external memory, generated code, source/module bytes, workers, queued
tasks/messages, timers, host operations, and CPU work. Reservations are
transactional: reserve before the effect, commit exact usage, and release on
all success, error, cancellation, and panic-safe teardown paths.

CPU enforcement must be cooperative and deterministic through interpreter,
JIT, microtask, native reentry, and event-loop boundaries. Limits report a
typed cause and stable counters; they do not depend on wall-clock races alone.

The runtime-independent `otter-resource` ledger now provides immutable limits,
shared fixed-array accounting, transactional reservations, exact RAII leases,
typed exhaustion/overflow, stable snapshots, and concurrent recovery tests.
Runtime construction is role-aware and atomic before effects: direct runtimes,
dedicated handle threads, and worker threads retain their exact isolate/worker/
native-stack tuples, failed bootstrap and pool construction roll back as one
unit, and the opaque in-process `RuntimeSnapshot` remains bound to its donor
configuration, capability policy, and account. Public commands retain a
`QueuedTasks` lease across the bounded inbox and deferred FIFO.

The hosted scheduler is wake-driven: one bounded Tokio inbox and one ordered
completion pump replace polling/retry loops; shutdown is cancellation-safe;
timer handles are isolate-scoped and JavaScript-safe; process finalization
detaches callback roots and host deadlines; the shared timer driver releases
owners and compacts cancellation tombstones; IPC messages close untaken file
descriptors. The guaranteed-completion backlog is finite per isolate:
`CompletionAdmissionPool` reserves a hard slot plus the complete ledger tuple
before the producer publishes any terminal state, exhaustion is typed
backpressure, and dynamic-import liveness rides the same unique carrier from
before the pending promise exists through to dispatch. Retained module source,
generated code, linked chunks, timers, host operations, and worker message
bodies are charged at their physical owning lifetimes.

CPU metering now lives in that one budget model rather than beside it. The
per-turn execution policy is part of the runtime configuration
(`RuntimeBuilder::runtime_budget`), so it is in force before the isolate's
first turn on direct runtimes, handle threads, and workers alike — a child
isolate inherits its parent's policy through the cloned configuration and
cannot widen it, exactly as capabilities narrow but never escalate. It is
installed after bootstrap: the builtin shims are host-chosen work of a fixed
size, and metering them against a caller's per-turn limit would reject the
isolate instead of the script it was configured for. Enforcement stays
cooperative — interpreter instruction checkpoints, and compiled back edges
that decrement inline fuel and reconcile the whole batch at the same
checkpoint.

Telemetry is one surface. The isolate publishes its counters into a shareable
cell at every root-turn boundary and at every enforcement rejection, and
`budget_report()` returns the configured limits, those counters, and the shared
resource-ledger snapshot together, from a `Runtime`, a `RuntimeHandle`, or the
`Otter` facade. The handle path crosses no command inbox, so a busy or blocked
isolate still reports. Publishing at boundaries rather than per instruction is
the point: a reader observes whole turns instead of a count torn out of the
middle of one, and the hot path keeps charging a plain local counter.

### R3. Resource performance

For each bounded resource record idle cost, steady-state cost, peak usage,
rejection count, and recovery to baseline. Prefer zero-allocation fast checks,
batched accounting, and cache-friendly counters, but only after correctness is
measured.

Measured first. `otter-resource` now carries a criterion bench for the shared
ledger (single-class reserve/release, lease resize, multi-class admission,
rejection, snapshot, and the same resize loop under 1/2/4/8-thread contention
on one account), and the `otter-gc` backing-store bench runs with and without
the runtime ledger installed. One uncontended ledger operation is a mutex
round trip of about 9 ns; the GC external reserve/release pair goes from about
20 ns heap-local to about 33 ns mirrored, and a JavaScript
`new ArrayBuffer(4096)` costs about 500 ns, so the ledger is roughly two
percent of the cheapest path that charges it. Eight threads resizing leases on
one account convoy to about 750 ns per grow/shrink pair. That contention is
not what bounds multi-isolate work: eight workers churning 4 KiB buffers spend
their time in the platform allocator, and a standalone 4 KiB alloc/free loop
scales identically badly on macOS under both the system allocator and
mimalloc. The ledger therefore stays a single mutex; lock-free counters are
not justified by any measured path.

The peak-usage finding was the real defect. Objects owning external bytes are
small old-space slots with drop glue, so old-space page occupancy barely moved
while their payloads pinned native memory; dead `ArrayBuffer` bodies survived
until the heap byte cap. Churning two million 4 KiB buffers reached 2.2 GB of
resident memory on one isolate and 5.6 GB across eight workers. Landed:
outstanding external reservations are part of the one major-GC budget. The
occupancy that fires a growth-triggered major GC is old/large page bytes plus
reserved external bytes, and after every major GC the next budget is
`live_pages × factor + live_reserved × min(factor, 11/10)`, floored and
cage-clamped as before; the reserve paths run the same due check before
admission. The same churn now peaks at 79 MB resident (399 MB across eight
workers) and runs about 30% faster per allocation. The ledger's `ExternalBytes`
peak follows the budget, which the `otter-gc` tests assert directly.

The model was taken from production engines rather than chosen as a constant.
V8 folds external memory into its global allocation limit — consumed
old-generation bytes times the growing factor plus external memory since the
last mark-compact times `min(factor, external_memory_max_growing_factor)` (1.1)
— re-checks it every 128 KB of external growth, and keeps a hard limit at the
low-water mark plus half the maximum old generation:
[`heap-controller.cc`](https://github.com/v8/v8/blob/main/src/heap/heap-controller.cc),
[`heap.cc` `UpdateExternalMemory`](https://github.com/v8/v8/blob/main/src/heap/heap.cc).
JavaScriptCore counts `extraMemorySize()` as heap bytes: `didAllocate` charges
the cycle budget and `updateAllocationLimits` grows the next maximum heap
proportionally from the visited bytes plus extra memory:
[`Heap.cpp`](https://github.com/WebKit/WebKit/blob/main/Source/JavaScriptCore/heap/Heap.cpp).
SpiderMonkey keeps a separate malloc threshold of
`max(lastBytes, JSGC_MALLOC_THRESHOLD_BASE = 38 MB) × growthFactor`:
[`Scheduling.cpp`](https://searchfox.org/mozilla-central/source/js/src/gc/Scheduling.cpp).
Otter follows V8 and JSC — one budget — with V8's capped external ratio, since
external bytes are neither marked nor copied and a large live external set must
not license a proportionally large external garbage set.

The steady-state finding for `SourceModuleBytes` was algorithmic. Resolving a
function id walked the append-only code-chunk chain, and the feedback
directory probed its installed-chunk list linearly, so `new Function` cost
grew with every chunk already linked: 21 µs at two thousand chunks, 350 µs at
fifty thousand, and two hundred thousand did not finish in ten minutes. The
registry is now one vector ordered by `function_base` under a read/write lock,
resolved by binary search and appended under the single-writer link lock;
installed chunks are keyed by executable address. The cost is flat at about
12 µs per `new Function` at every size, and two hundred thousand finish in
2.3 s. The `CodeSpaceConflict` link error, which only the one-shot chain could
raise, is gone.

The timer finding led into the old-space allocator. One `setTimeout` cost
about 18 µs and about 3 KB resident while queued, almost entirely in Node's
`Timeout` constructor; isolating that showed a twelve-field constructor
costing 12.8 µs against 0.4 µs for a three-field one, growing with heap size
(6.6 µs at twenty thousand live objects, 20 µs at four hundred thousand), and
94% of the time inside `OldSpace::alloc`. The free list had eight power-of-two
classes and served a request by scanning its own class linearly for the first
range that fit, so every slot-slab allocation into a swept heap walked every
too-small hole the sweep had left in that class. The classes now follow V8's
`FreeListMany::categories_min`: one class per cell size below 256 bytes, then
one per doubling. A precise class holds ranges of exactly its size and pops in
O(1); a doubling class keeps its ranges largest-first and either pops its
largest or fails in O(1); any class above the request's own guarantees a fit.
The twelve-field constructor now costs 3.4 µs at every heap size and the
sixteen-symbol-and-field variant 4.6 µs instead of 25 µs
([`free-list.h`](https://github.com/v8/v8/blob/main/src/heap/free-list.h)).

The remaining per-store cost was the add-property inline cache. Generated
code could commit an add-transition only for the three in-body inline slots
and only when the receiver's direct prototype had no prototype of its own —
which no constructor instance satisfies, since `Foo.prototype` inherits from
`Object.prototype`. Every other store left generated code for the runtime
stub, which re-resolved `[[Set]]` along the prototype chain with string
compares and a shape-table lookup before its own cache could answer. The
transition record now captures the shape of every prototype on a missing-key
chain (up to eight links) and replays by walking the live chain, and the
generated way carries a second chain shape so a two-link chain — a
constructor prototype above `Object.prototype` — commits inline; the guard
also bound-checks the appended slot against the live slab's capacity, so
stores into the spilled slab commit inline and only the growth points reach
the runtime. A chain link is guarded by its shape, its fast-IC mode, and a
new body flag that marks it opaque (a Proxy or non-object prototype, or a
String wrapper whose keys the shape does not list); sidecars and attribute
overrides on a link do not affect key absence, so `Object.prototype`, which
carries a sidecar, stays guardable. This is SpiderMonkey's design:
`SetPropIRGenerator::tryAttachAddSlotStub` emits `ShapeGuardProtoChain`, one
shape guard per prototype, before `EmitAddAndStoreSlotShared`
([`CacheIR.cpp`](https://searchfox.org/mozilla-central/source/js/src/jit/CacheIR.cpp)).
V8 reaches the same proof through one prototype-chain validity cell per
receiver map in `StoreHandler::StoreTransition`
([`ic.cc`](https://github.com/v8/v8/blob/main/src/ic/ic.cc)), and JSC through
an `ObjectPropertyConditionSet` of absence conditions on each prototype
structure in `AccessCase::Transition`
([`AccessCase.cpp`](https://github.com/WebKit/WebKit/blob/main/Source/JavaScriptCore/bytecode/AccessCase.cpp)).
No tier was added: the VM record, the CacheIR replay, and the one way-cell
guard shared by the template and machine tiers changed in place, and the
runtime property-stub count for a twelve-field constructor is now bounded by
its slab growth points instead of its store count. The twelve-field
constructor costs 1.3 µs per object (12.8 µs at the start of this lane) and
the sixteen-slot symbol variant 2.5 µs (25 µs), about 80 ns per appended
field; `otter-difftest` agrees across interpreter, tiers, and GC stress
(22/22), and `OTTER_GC_STRESS` 1/3/5 smoke on the constructor scripts is
clean.

Found while gating, outside this lane: seventeen tests across the
`otter-runtime` `jit_machine_*`, `jit_control_ops`, `jit_artifacts`, and
`jit_debug_events` suites fail identically with and without this slice and
with each of this lane's commits reversed, so they predate it; `gate.sh` does
not run them. Most assert on artifact shapes or event counts that the machine
tier no longer produces the same way, but `jit_control_ops` reports a tier
completion of `3240:180:3` against the interpreter's `2760:180:3` — a getter
evaluated more than once in generated code — which is a real divergence the
JIT lane must take.

Still recorded, not yet acted on: Node's timer lists run in the interpreter,
so one `setTimeout` still costs about 10 µs and grows with the number queued;
symbol-keyed stores have no inline cache; and V8 sizes new instances from
constructor feedback (in-object slack tracking) where Otter grows the slab by
doubling. Worker messaging round-trips a 4 KiB `ArrayBuffer` in about
12 µs and a 4 KiB string in about 35 µs. Typed-array and `DataView` messages
now clone as views over their cloned buffer with the original kind, offset,
and length, and `new Worker` accepts a `file:` URL as well as a path. Under
eight
concurrent isolates the 4 KiB allocate/free pattern is bounded by the platform
allocator; the structural answer is young-generation finalization of buffer
bodies as in V8's `ArrayBufferSweeper`, which is a GC design slice of its own.

## B — Bytecode and artifact integrity

### B1. Mandatory bytecode verifier

Create a focused verifier owned by the bytecode/VM boundary. It validates all
untrusted or deserialized functions before installation or execution:

- opcode and operand decoding without out-of-bounds reads;
- register, constant, upvalue, exception, jump, and source-map ranges;
- instruction-boundary and control-flow targets;
- register and operand domains needed by interpreter and JIT invariants;
- handler nesting and abrupt-completion structure;
- bounded work and memory for adversarial artifacts.

Verification runs once per admitted artifact and produces an immutable trusted
carrier consumed by the interpreter and compiler. There is no unchecked public
constructor or compatibility verifier. Invalid input returns a typed error and
cannot allocate executable code or mutate runtime state.

Acceptance includes mutation/fuzz corpora, every opcode family, truncated and
oversized artifacts, invalid CFGs/handlers, and agreement tests across
interpreter/JIT consumers.

The bytecode boundary now rejects malformed raw operand storage in debug and
release; validates module/function/register/constant/upvalue/closure, metadata,
handler, suspension, and abrupt-completion domains; and returns typed
decoder/link errors. One immutable proof carrier crosses compile caches,
CodeSpace publication, interpreter admission, and executable conversion
without exposing a mutable unchecked DTO. The bytecode compile cache remains a
distinct verified artifact boundary; it does not carry or restore GC pages.
CFG analysis is bounded, tracks normal versus abrupt `finally` entries, models
generator external `return`, and distinguishes local exceptions from
caller-only frame exits.

The legacy serialized heap-image boundary is removed: raw `HeapImage` and
`IsolateSnapshot` bytes, snapshot-blob restore, and the disk snapshot cache can
no longer turn arbitrary bytes into Rust/GC object bodies. Opaque,
capture-produced in-process `RuntimeSnapshot` restore remains and keeps the
donor resource account. A future durable snapshot requires a typed logical
object format that validates and reconstructs values instead of copying Rust
representations, followed by fresh startup and memory measurements. The
admission boundary is now gated in a RELEASE build, not only in debug: the
verifier, its adversarial corpus, and the code-space / snapshot admission
tests run under `just verifier-gate` and inside `scripts/gate.sh`.

The corpus (`crates/otter-bytecode/tests/adversarial.rs`) drives seed modules
through single- and multi-byte flips, every truncation prefix, insertions,
appended bytes, and tampered `u32` counts, plus structural mutations of the
decoded module. The seeds span every opcode family that carries a verification
domain of its own: control flow and handler nesting, closures and the capture
spine, constants and metadata, the variadic call / construct / spread sites
whose argument count rides in the instruction, the iteration protocol around a
back edge, the suspension points of generator, async, and async-generator
bodies, class construction with private names and `super` accessors, and module
linkage with its `module_resolutions` and `module_inits` tables. Each family
also carries a structural mutation, and each is rejected by the check that
belongs to it — a closure capture-count mismatch, a call argument outside the
register window, a back edge leaving the function, a suspension target outside
the function table, a private name pointing at a number, a module-init function
id past the table end. Two properties are asserted on every artifact: the decoder never
panics, and it never returns a carrier its own verifier would reject. The
corpus is deterministic — a fixed xorshift, no system entropy — so a failure
reproduces byte for byte. Its teeth are demonstrated rather than assumed: a
deliberately weakened constant-index bound and an injected panic on the
metadata path are both caught by the sweeps.

The two mutation layers reach different depths, and the split is measured
rather than assumed. Byte flips reach the semantic checks in bulk — the
closure verifier runs 1843 times across one single-byte sweep — but they
almost never perturb an operand VALUE in place: the wordcode layout is tight,
so a flip inside an operand word desynchronizes the instruction stream and is
rejected by the decoder before the value is read. Byte mutation is therefore a
panic-freedom and decode-consistency instrument, and the structural mutations
are what exercise the value domains each semantic check owns.

Two of the original bullets resolve differently than written. Register
verification is index-domain only, and that is sufficient rather than
incomplete: every frame window is fully initialized to `undefined` before the
first instruction runs (`RegisterStack::allocate`), so a read with no
dominating write yields `undefined` instead of unmapped memory. Def-before-use
is therefore a semantic property of the compiler, not a safety obligation of
the verifier, and adding a dataflow lattice would buy no soundness. Bounded
work is likewise met by construction: verification is one pass per function,
the handler layout is a single pass whose cost does not grow with nesting
depth (asserted at depth 4096), the one analysis that is not linear carries an
explicit transition budget, and the decoder bounds every allocation by the
remaining input length.

Artifact-level agreement is covered by
`crates/otter-jit/tests/artifact_agreement.rs`: one verified artifact is run on
the interpreter and on the JIT with the OSR threshold lowered so the loop body
is compiled, and the two outcomes must match. Building the artifact directly
reaches shapes no compiler emits, which is the point — `otter-difftest`
compares tiers from JavaScript source and can only exercise what the compiler
chooses to produce.

That distinction paid immediately, and the defect it found was the kind B1
exists to prevent: a consumer relying on an invariant the admission boundary
did not enforce. The immediate-right operators (`AddImm`, `LessThanImm`, …)
carry their constant in the instruction, and generated code materializes it
into the destination register before running the ordinary two-register
operator — which is sound only while the destination differs from the left
operand. The interpreter reads both operands before writing and tolerates the
alias. Nothing checked it: the lowering simply commented that "the compiler
guarantees" it, the compiler guaranteed it only on the path where it allocates
its own scratch, and the verifier admitted the aliased form. An admitted
artifact therefore ran correctly on one consumer and, on the other, destroyed
its own induction variable and looped forever.

The invariant is now real at all three levels: the verifier rejects the
aliased form (`ImmediateOperandAliasesDestination`), the compiler allocates a
distinct destination instead of emitting it, and the lowering cites the
verified guarantee rather than an assumption. `finally` reached through tier-up stays covered from source by
the difftest corpus, because emitting a correct finally by hand means
reproducing the compiler's parked-completion protocol.

### B2. Fuzzing and differential checks

Continuously fuzz parser -> compiler -> verifier -> interpreter, serialized
artifacts, GC stress, and JIT deopt reconstruction. Minimized regressions become
normal tests. Compare supported semantics with the interpreter oracle and,
where licensing and determinism permit, established ECMAScript engines.

## J — JIT, portability, and throughput

### J1. One compiled pipeline

Autonomous execution follows `scratchpad/PLAN_JIT_HANDOFF_2026_09_12.md`.
S0 source, release and measurement validation is complete. Shared fixes cover
committed apply lookup, cross-script compilation ownership, generated catch
routing, explicit derived-this CFG, native leaves and boxed literal allocation
with precise moving roots. Constructor observations flush into bounded scalar
maxima before GC instead of retaining instance graphs; the profile module owns
sampling, recording and deferred invalidation.

Evidence: `/tmp/otter-jit-s0-final-gate.json`,
`/tmp/otter-jit-s0-release-gate.json` and the global-Script benchmark report
above. All initial 20 failures and the additional constructor-retention defect
are resolved. No failures are allowlisted, and no gate restart or bisect was
used. Source/release checks are reusable evidence for the unchanged code.

S1's first change is validated: generated runtime stores probe complete IC
programs before canonical `[[Set]]` resolution. Matched transition allocation
failures propagate through the bank instead of becoming misses after GC.
Focused validation passed 59 object tests, 6 property-IC tests, 5 runtime tests,
GC stress on the new invalidation sequence, scoped clippy and three release
differential programs. No S0 gate was repeated.

RayTrace improved from 857 ± 1 to 939 ± 2 (median ± MAD, five fresh processes),
+9.57%, with identical program SHA-256 and all results validated. Memory was
roughly unchanged. Report:
`benchmarks/results/s1-20260913-store-probe/README.md`. This is a single-workload
win; the substantial gap to other runtimes remains.

S1 sustained CPU attribution now supplies usable post-change evidence:
`benchmarks/results/s1-20260913-store-attribution/README.md`. The isolate has
4644 sampled stacks; the store runtime boundary appears in 1348 and the load
boundary in 1037. System allocator descendants account for 457/428 respectively;
these are overlapping inclusive sample weights, not operation counts.
Source inspection confirms a boxed root provider plus entry-Vec allocation
before each property IC probe. All diagnostic suites complete successfully.

Property operands now use the existing `HandleScopeFrame` and `HandleArena`:
no per-entry Box or root-entry Vec, no parallel provider or new unsafe code.
GC traces the live arena buffer; handles retain indices across arena growth.
Store hits reload the moved receiver before cell fill, and rejected transition
capture reloads both operands before canonical fallback. Scope cleanup uses
the shared RAII owner across return, exception and panic.

Validation: 17 focused tests, both property/reentry tests at every GC stress
stride 1..16 with verification, scoped clippy and three release differential
cases. A single release build supplied five valid RayTrace samples: 939 ± 2
became 1322 ± 7 (+40.79%), same program hash, median RSS unchanged. Report:
`benchmarks/results/s1-20260913-property-handles/README.md`. No full gate or
comparison-engine baseline was repeated, and no engine-wide win is claimed.

Bounded `propertyStoreRuntime` counters now attribute generated runtime entries
by function/logical PC, property name, path, error and offered native way.
They share the event cap, update existing rows in place and reset with the
owning batch. A constant capture specialization of the single store body keeps
observation bookkeeping out of disabled execution. Twenty-six focused tests,
scoped clippy, GC stress and release exact-effect capture validate the change.

RayTrace capture records 10,330,955 runtime stores in 117 rows, without drops.
79.01% are six initializer fields: Vector x/y/z and Color red/green/blue.
Installed recipe hits without a native way dominate. Their source prototypes
contain writable data fields, while native lowering rejects direct-prototype
writable-data transitions. This identifies the next guard/lowering candidate;
the counters do not classify every native rejection. Report:
`benchmarks/results/s1-20260913-store-counters-fast/README.md`.

Disabled-capture timing is 1311 ± 1 versus saved 1322 ± 7 (-0.83%, overlapping
sample ranges), five valid processes and identical program hash. No zero-cost
claim or additional throughput win is made. The initial 1130 ± 39 candidate
was rejected; only its correction required another build, not a repeated gate
or rebuilt baseline. Next: complete and test the native writable-prototype
transition proof, then follow the handoff order for forwarded generated calls,
spread packets, Machine coverage and root scanning.

The native-transition prerequisite removes the duplicate JIT-side IC-way
carrier: cells now store VM-owned `JitPropertyIcWay`, derive stride from that
type and check every field offset. The existing 20-byte representation stays
intact. The cell-fill unit and scoped JIT clippy pass; no release rebuild or
benchmark repeat is needed. The handoff records the complete writable-data
prototype proof, descriptor invalidation requirements and scratch-register
contract before enabling the new native path.

Native writable-data transitions landed in signed `472563aa`. The single
VM-owned way now carries an explicit prototype proof (24 bytes), and both tiers
check live shape/descriptor/sidecar state before receiver mutation. Focused GC
stress also exposed and repaired stale Proxy receiver/descriptor operands.
Nine runtime tests pass at GC strides1..16; scoped clippy, the cell-fill unit
and three release differential cases pass. RayTrace is1712 ± 6 versus1311 ± 1
(+30.59%, five valid fresh processes, same source hash, RSS +0.12%). Runtime
store capture fell from10,330,955 observations to2100; Vector/Color fields now
enter the runtime twice each with native ways. Full evidence is in
`benchmarks/results/s1-20260913-native-writable/README.md`.

A single sustained profile of that accepted binary has2757 isolate stacks:
forwarded-call ancestry960, runtime property load534, base constructor prepare487,
runtime property store1. These inclusive rows overlap and include callee work;
they are not pure overhead percentages. Source and events initially classified the shared
Class.create trampoline as polymorphic. The bounded-forwarding capture below
refines that diagnosis to saturated feedback; a bounded candidate chain alone
misses this hotspot. See
`benchmarks/results/s1-20260913-native-profile/README.md`.

Forwarded argument semantics required repair before generated copying: mapped
parameters must read their live bindings; once an arguments object has been
exposed, subsequent intrinsic apply observes its actual length/index properties.
Both interpreter and generated runtime completion now follow that rule. Generic
array-like collection roots its source and earlier arguments across allocating
length coercion and indexed getters. Five focused forwarding tests, including
these mutations and abrupt getters, pass normally and with GC+verification1..16.
Scoped VM clippy and three affected release differential cases also pass, with
one release build. Evidence: `benchmarks/results/s2-20260913-live-forward-arguments/`.
No throughput measurement is attributed to this correctness build.
This is the S2 semantic prerequisite; the generated direct-forward path remains
open and must handle the measured polymorphic target and the full live argument
span, preserving the already-committed apply lookup.

Forwarding now also publishes the resolved bytecode target at the canonical
instruction PC. The shared ordinary-call policy invalidates an installed caller
on every bounded target-population transition, including saturation; repeated
hits remain inert. A regression fails before this change and proves the
monomorphic-to-polymorphic compile transition after a late target switch in both
generated tiers. Six forwarding tests, GC+verify1/4/16, five feedback units and
23 affected Machine direct-call tests pass; scoped VM clippy passes. Evidence:
`benchmarks/results/s2-20260913-forward-feedback/README.md`. Release validation
is deferred to the coherent generated-forwarding implementation; no new release
build or performance claim accompanies this prerequisite.

Shared generated linkage now places fixed control slots and upvalues before
registers/actuals. One focused `arm64/direct_call/layout.rs` owns checked offsets;
control storage is independent of actual arity, while the native register-base
pointer and contiguous argument window preserve the current GC contract. Two
layout tests and 29 affected runtime tests pass; scoped JIT clippy passes.
Forwarding also passes GC+verify stride1. An overbroad whole-target stress run
was stopped; its fast-allocation assertion was corrected to check the rooted GC
sibling under stress while retaining the normal fast-hit requirement. Focused
normal, stress1 and disabled-stress0 checks pass; the interrupted run is not a gate.
This fixed-layout prerequisite left dynamic reservation/copy to the native
forwarding slice below. Source audit confirms forwarding can finish SSA loads
before reservation, avoiding a general spill-addressing rewrite.
Evidence: `benchmarks/results/s2-20260913-call-layout/README.md`.

Native bounded forwarding is now debug-verified. Template selects the ordinary
bounded callee population and uses shared generated linkage with the complete
live mapped/actual window. A fixed control slot owns dynamic reservation size;
all roots are initialized before capture allocation, and normal/throw/deopt/reject
paths release that size. Exposed arguments and custom apply retain committed
canonical completion. Artifact argumentMode includes forward; dynamic sizes are
null. Pending-error and pure-value throw routes remain distinct.

Forwarding8/8, Machine direct calls23/23, layout2/2 and scoped VM/JIT clippy pass;
the final full-window/throw regression passes GC+verify1/4/16. A settled256-call
polymorphic probe demonstrates native entries with <64 rooted runtime transitions
in both generated policies. Evidence:
`benchmarks/results/s2-20260913-native-forwarding/README.md`.

S2 remains active: a single debug RayTrace capture validates but function535/PC4
has zero direct targets at both preparation tiers; the bounded Poly arm's lack of
per-target plans indicates saturated feedback. Machine also rejects the forwarding
opcode. Do not claim a RayTrace win or repeat release measurements yet. Next:
generic native target selection through the existing entry-cell/frame contract,
>8-target coverage, explicit Machine call CFG, then one affected release cycle.
No larger target cap, duplicate registry, full gate or new benchmark baseline.
The authoritative handoff was shortened; superseded history is preserved in
`scratchpad/JIT_LANE_HISTORY_2026_09_13.md`.

The intended replacement path remains:

```text
bytecode + feedback
        |
        v
typed CFG HIR -> Machine IR -> target selection/legalization
        -> regalloc2 -> code + stack maps + deopt maps
```

Quick and optimizing tiers share HIR, Machine IR, target backends, frame
layout, call descriptors, safepoints, deopt reconstruction, dispatch cells,
and artifact schemas. They differ only in optimization budget and tier policy.

The current AArch64 Machine path already owns substantial scalar CFG, OSR,
typed values, direct calls/constructs, exact leaves, GC-aware allocation,
element/property/binding access, string constants, intrinsics, exceptions, and
deopt reconstruction. Keep extending the final path; do not translate new IR
back into deleted/legacy SSA or add an emitter-specific semantic path.

Current state (measured 2026-09-12, Octane in fresh processes against node
v24): NavierStokes 2x, Richards 19x, Splay 27x (2x noise), Box2D 61x,
DeltaBlue 86x, EarleyBoyer 96x, RayTrace 94x, Crypto 248x behind. A body
whose every use of `arguments` is `callee.apply(x, arguments)` now compiles
to `Op::CallForwardArguments` and skips the prologue's arguments object: the
interpreter and generated code forward the activation's actual arguments
when the observably read `apply` is the intrinsic (JSC's ForwardVarargs,
V8's CallWithArrayLike with arguments elision), and any other method
receives a per-activation arguments object materialized on first use
(RayTrace 676 → 984). The
generated construct-prepare transition sampled the collector's cycle counts
through `gc_stats()`, which re-aggregates the whole per-type table; two such
calls per construct were the hottest leaf in RayTrace (17% of the main
thread). The heap now exposes the two maintained counters directly
(RayTrace 559 → 670, EarleyBoyer 833 → 1043, DeltaBlue 1373 → 1543). The
template's plain and explicit-receiver calls without a generated edge now
complete through the callee-carrying value transition that the Machine tier
already used, so a native call inside a stack-owned generated callee no
longer side-exits the whole call chain (RayTrace generated-call deopts 380 →
116 per run, bails 108 → 2; EarleyBoyer 689 → 818, Box2D 2211 → 2305). A
generated caller now publishes every actual argument after the callee's
register window (`NativeFrameFlags::INCOMING_ARGUMENTS`, traced with the
registers), so a body that materializes `arguments` takes direct call and
construct linkage and builds its arguments object from the published window
through one allocating transition instead of rejecting the plan; in RayTrace
every `Class.create` construct site is now generated linkage, yet the score
is unchanged because the constructor body immediately forwards through
`Function.prototype.apply` in the runtime. Explicit-receiver calls
and attempted polymorphic plain calls now lower to one explicit-receiver
direct call (generic completion through a callee-carrying value entry), the
template's unplanned plain calls complete in place, callees entered only
through generated linkage promote through the registry's hot-function word,
every producer of a constructor's receiver shares one preparation contract
and every baked field transition proves its storage, sloppy callees bind an
object receiver in generated code, and only a native-function cell leaves a
nullish comparison. The Machine HIR still admits 80 of 188 opcodes and one
unsupported opcode declines the whole function; over the hot Octane
functions it refused, the remaining blockers are `Throw`/`NewError`,
`MakeClosure`/`StoreUpvalue`, `CollectArguments`, object and array
literals, and catch regions around named-property misses. Recompilation
never reads the recorded bail PCs, so a failed speculation is re-emitted
until the generation is abandoned.

The recorded optimized exit PCs (`optimized_bail_pcs`) are now folded into
the baked per-instruction feedback of every snapshot, standalone or spliced
inline, so a rebuilt generation speculates no narrower than the exit already
refuted — the analogue of JSC's per-site ExitProfile that the DFG consults
before re-speculating; an inlined frame's exit records the innermost PC as
well as the root's. The rebuild still waits for the 100-exit budget (JSC's
`osrExitCountForReoptimization`), so a site such as RayTrace's `rayTrace`
`LooseNotEqual` pays one budget per generation.

Next work, in order: `Op::CallForwardArguments` still enters the callee
through the runtime call boundary — the direct-call linkage must accept the
published incoming window as its argument source so the forward becomes a
generated call and the Machine tier can admit the opcode; the spread
call/construct transition
still requires a materialized frame and side-exits stack-owned callees;
admit `Throw`/`NewError`, closures with captured cells, and object/array
literals into the Machine HIR with committed cold paths where no fast path
is proven yet, measuring the decline histogram on Octane after each family;
then the sequence below.

Active sequence:

1. Complete reentrant call descriptors, exact safepoints, exception landing,
   cancellation, and budget exits without interpreter-window root plumbing.
2. Move nested/finally exceptional edges and remaining object/element families
   to explicit Machine CFG.
3. Add virtual objects, allocation sinking, broader inlining, and complete OSR
   only after precise reconstruction is proven.
4. Put quick compilation on the same pipeline, then delete Template-only ABI,
   duplicated emitters, and obsolete artifacts.
5. Add shared representation propagation, guard elimination, GVN, LICM, loop
   scheduling, and cold outlining with separate optimization budgets.

### J2. Target parity

Implement x86-64 selection, legalization, frames, calls, polls, safepoints, and
deopt exits over the same Machine IR. JavaScript semantics stay above target
selection. Every shared opcode needs cross-target verifier, allocation, and
normalized-artifact tests.

### J3. Performance method

Optimize from profiles and stable workloads. Track wall time, retired
instructions, compile latency, emitted and resident code bytes, deopts,
allocation rate, peak RSS, and VM/Rust transitions. The result checksum must be
identical. Failed or unvalidated observations remain visible but unscoreable.

## E — Embedding and extension API

### E1. Capability-aware host boundary

The evaluator introduced by H1 becomes the single reusable authorization
surface for runtime installers, dynamic natives, modules, Web APIs, Node APIs,
and extensions. It is cloneable, immutable from extension code, cheap on the
allowed fast path, and testable without constructing a VM.

Landed: sound capability narrowing. `Permission::Intersect` composes two
rule sets with deny-wins evaluation at check time (pattern languages have
no computable intersection, so both layers are kept), `CapabilitySet::
narrowed` applies it per class, and `new Worker(url, { otter: {
capabilities } })` requests a worker-scoped subset — `false` denies a
class, `true`/missing inherits, a pattern list allows only what the
parent also allows. Option parsing reads plain data properties (no
getters) and validates before the spawn effect; nested workers narrow
again from the already-narrowed set. Escalation and inheritance paths
are covered by unit and end-to-end worker tests.

### E2. Stable contributor model

Finish the descriptor/spec/builder/bootstrap path for constructors,
prototypes, namespaces, and modules. Static specs and native function pointers
are the backend; macros remain zero-cost syntax sugar. Preserve explicit JS
name, arity, capability requirements, and install order.

Extension acceptance requires owned `Send + Sync` DTOs, handle-scoped native
allocation, compile-fail boundary tests, no per-call metadata parsing, and the
runtime behavior <-> `.d.ts` <-> docs/examples/tests triangle.

### E3. Ergonomics and diagnostics

Expose precise typed errors, source locations, cancellation, resource usage,
and capability denials without leaking VM internals. Measure bootstrap time,
bootstrap allocations, native-call allocations, and host-call transition cost.

## C — Conformance and compatibility

### C1. Reproducible conformance baseline

Complete, with a documented residual. Before changing a language area, consult
`ES_CONFORMANCE.md` and run its focused Test262 subset. After a substantial
semantic slice, capture a fresh full run on a stable checkout and update the
report with commit, configuration, pass/fail, timeout, and deltas. Never hide
timeouts or compare partial runs as full runs, and diff failing *sets* rather
than counts.

Baseline at `242593b2`: 99.98%, 12 fails of 53575, 0 crashes, 0 timeouts, 0
OOM. Reproduce with `otter-test262 run --output test262_results/latest.json`,
then `otter-test262 conformance test262_results/latest.json`. The per-subdir
batch script omits `harness/`, so its totals are not comparable to this
baseline.

The residual twelve are held deliberately, not pending:

- nine `intl402/DateTimeFormat` cases (CLDR interval patterns, the chinese and
  dangi calendars, `Intl.Era-monthcode`) — a data lift against ICU4X, not an
  engine defect;
- `staging/sm/String/internalUsage.js`, which needs day/month padding to differ
  between locales that ICU4X's data does not distinguish;
- `staging/sm/regress/regress-602621.js` and
  `staging/sm/lexical-environment/block-scoped-functions-annex-b-arguments.js`,
  which specify mutually exclusive Annex B behaviour — passing either fails the
  other.

Clusters closed on the way from 293 fails, each verified by a full run with a
zero-regression set diff: direct and indirect `eval` (scope capture, dynamic
and deletable bindings, per-site block-scope tables, `with` object
environments); Temporal (6642/6642); the realm model (parked-in-slot state
swap, typed intrinsic prototypes, cross-realm identity for body-slot exotics);
GC soundness under `OTTER_GC_STRESS` (write barriers, iteration anchors,
handle re-derivation after every allocating step); iterator helpers (647/647);
dynamic `import` with attributes; class fields, private names, and the private
MOP; destructuring and spec-ordered observable steps; `Function.prototype`
metadata and the legacy `fn.arguments` / `RegExp` static surfaces.

Two engine-level defects surfaced through this lane rather than through any
benchmark, and are the reason the set diff is mandatory: generated template
code returned through its own epilogue and silently skipped `finally` once a
function tiered up, and `Op::CallWithThis` had no feedback slot at all, so it
could never resolve a direct callee.


### C2. Web and Node compatibility

Treat compatibility as vertical slices: runtime semantics, declaration files,
docs/examples, adversarial tests, cancellation, permissions, and resource
limits land together. Parked legacy crates are references only.

## O — Observability and diagnostics

### O1. Runtime resource telemetry

Expose bounded, low-overhead counters for heap/live/external/code/source/module
bytes, workers, tasks, queue depth/bytes, host operations, cancellations,
denials, reductions, turn duration, and exhaustion causes. Counters must remain
usable during failure and teardown and must not allocate on hot rejection paths.

Landed: `RuntimeActivityStats` embeds the shared-ledger `ResourceSnapshot`
(current/peak/rejections/limit for every class, including `ExternalBytes`,
`SourceModuleBytes`, and `GeneratedCodeBytes`), so one handle call captures
activity and resource telemetry together. Remaining: reductions/turn-duration
counters and denial causes on the same surface.

### O2. JIT and execution evidence

Keep JIT events and artifacts typed, bounded, deterministic, and address-safe.
Join compile decisions, code objects, deopts, safepoints, resource charges, and
execution profiles without introducing a second execution ABI.

## Cross-cutting acceptance gates

Every substantial slice defines its affected subset of these gates before
implementation:

- **Correctness:** focused unit/integration tests, no new panic or timeout, then
  `bash scripts/gate.sh` when the focused loop is green.
- **GC:** exact rooting and `OTTER_GC_STRESS=1..16` for multi-allocation native
  paths; no conservative scan or stale raw `Value` across allocation.
- **Conformance:** focused Test262 before/after and a full reproducible run for
  substantial language changes.
- **Security:** deny tests for initial, intermediate, and final resource targets;
  denial occurs before the external effect.
- **Resources:** named counter, limit, rejection semantics, cancellation, and
  adversarial recovery-to-baseline test.
- **Performance:** fresh-process release A/B, unchanged checksum, predeclared
  regression threshold, and clean-tree baseline before publication.
- **API/extensibility:** owned boundary DTOs, no VM/GC carriers, and updated
  declarations/docs/tests where JS-visible behavior changes.
- **Architecture:** one final path; remove any replaced shim, registry, schema,
  or ABI in the same slice.
- **Documentation:** update the stable contract in the same patch; keep task
  history out of this file.

## Program metrics

| Area | Required signals |
|---|---|
| CPU/JIT | wall time, retired instructions, compile latency, code bytes, deopts, VM/Rust transitions |
| Memory | idle/peak RSS, live/allocated/page/external bytes, code/source/module bytes |
| Scheduling | reductions, maximum turn duration, microtasks, queue depth, rejected/cancelled operations |
| Isolation | active workers/tasks, thread budget, queued message bytes, aggregate cage pressure |
| Conformance | targeted/full Test262 pass, fail, timeout, and delta |
| Extensibility | bootstrap time/allocations, native-call allocations, boundary compile-fail tests |

## Global stop conditions

Stop and revise the architecture instead of adding a workaround if any occurs:

1. A permission decision is duplicated or can be bypassed through redirects,
   custom providers, aliases, retries, snapshots, or dynamic reattachment.
2. A limit protects one isolate while process-wide threads, queues, code,
   sources, or external memory remain unbounded.
3. Unverified bytecode can reach the interpreter, JIT, deserializer, cache, or
   embedding surface.
4. A moving root requires an interpreter window or conservative stack scan.
5. x86-64 requires a JavaScript-semantic lowering fork.
6. A caller must be recompiled when a callee changes tier or frame size.
7. An extension needs raw VM/GC types or a parallel registration/runtime path.
8. A performance improvement changes results, moves cost outside measurement,
   or lacks a reproducible baseline.

## Working rules

- Work on the current live checkout and re-read every touched dirty file before
  applying a patch. Preserve unrelated concurrent edits.
- Search the git-indexed repository with `fff`; use `rg` only when the service
  is unavailable.
- Keep patches vertical and reviewable. Prefer a clean breaking change over an
  adapter or internal compatibility mode.
- Keep GC roots explicit and allocation-driven. Native value construction has
  one high-level entry through branded `NativeCtx` handle scopes.
- Keep generated-code ABI private to `otter-jit` <-> `otter-vm`.
- Use deterministic collections whenever observable ordering matters.
- Record only current state, next work, dependencies, and accepted gates here.
  Put completed history and raw measurements in git and `scratchpad/LEDGER.md`.
