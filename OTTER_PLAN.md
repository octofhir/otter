# Otter Implementation Plan

This is the single repository-level implementation tracker. It records active
engine work, cross-cutting invariants, and required verification. Shipped
behavior belongs in `AGENTS.md`, crate/module documentation, and the docs site;
completed implementation detail belongs in commits rather than permanent task
diaries.

## Plan Rules

- Update this file when a repository-level slice starts, lands, changes scope,
  or is deliberately dropped.
- Keep completed work as a short ledger. Do not append commit-by-commit history.
- Keep subsystem-specific usage instructions in `AGENTS.md` and the docs site.
- Change internal contracts in place. Do not add compatibility readers,
  versioned internal schemas, parallel runtimes, or parked transition layers.
- Every active item must have an explicit correctness gate; performance items
  also require reproducible before/after measurements.

## Status At A Glance

Active repository-level work:

1. x86_64 parity for the template JIT.
2. Measurement-driven baseline-v2 and runtime-throughput work.
3. Optimizing-tier prerequisites and the optimizing tier itself.
4. JIT-aware profiling, tracing, and bounded failure diagnostics.
5. Production-ready `Otter.serve` async dispatch, streaming, and transport.

The public WebAssembly surface is documented separately in the docs-site
[WebAssembly roadmap](docs/site/src/content/docs/web/webassembly-roadmap.md).
Its remaining runtime-integration candidates are zero-copy `Memory.buffer`,
Wasm ES modules, and WASI.

## Permanent Architecture Invariants

- The active stack remains `otter-gc -> otter-vm -> otter-runtime -> product
  crates`; legacy crates never re-enter the active build graph.
- The interpreter is the semantic oracle for every JIT tier.
- The VM/JIT share one typed execution representation and authoritative dense
  feedback. JIT code must not maintain live mirrors of VM state.
- JIT runtime entries are typed, classified as leaf/allocating/reentrant, and
  resolved from the isolate-owned immutable entry table.
- Allocating or reentrant transitions publish exact frame PC and safepoint
  state. JavaScript exceptions never unwind native Rust frames.
- Invalid code accepts no new entries; live anchors keep mappings resident
  until the final active frame leaves.
- Moving GC values are built and retained through handle scopes or persistent
  roots. No raw GC handle crosses an allocation or `.await`.
- Diagnostics are explicit owned configuration and default-off. Disabled
  diagnostics must not allocate payloads, format output, open files, or lock.
- Standard diagnostic formats are primary when available: Chrome Trace Event,
  Chrome/V8 CPU profiles and heap snapshots, folded stacks, machine code, and
  annotated assembly.
- Permissions remain deny-by-default and are enforced at the Rust host
  boundary, including for servers, WASI, loaders, and async resources.

## 1. Engine And JIT

### Completed Ledger

- [x] Measurement, GC-stress, differential-testing, benchmark, and profiling
  gates established.
- [x] Legacy monolithic baseline split into focused compiler, ABI, runtime-op,
  architecture, and executable-code modules.
- [x] Typed lowering plan and authoritative dense feedback landed.
- [x] Exact-PC exit contracts, typed isolate-owned runtime-entry table,
  canonical native frames, classified stub inventory, code lifetime states,
  and compiled-to-compiled calls landed.
- [x] Template AArch64 compiler reached synchronous opcode coverage, replaced
  the legacy emitter, and became the production baseline tier.
- [x] Baseline-v1 arithmetic, comparisons, calls, IC probes, collection fast
  paths, runtime transitions, exception edges, and OSR landed.
- [x] Template coverage reached all 163 synchronously completable active
  opcodes. The nine suspend/tail/module opcodes remain deliberate exact side
  exits until the optimizing tier owns suspend/resume and deopt.
- [x] Optimizing monomorphic plain/method callee splicing landed with synthetic
  `this`, multi-frame exact-PC deopt, loop-invariant method-guard caching,
  numeric coercion residency, and bounded batched polling. On the controlled
  M1 matrix it improved the method-call kernel by 1.56x and the five-kernel
  geomean by 1.17x.
- [x] Optimizing loop-versioned own-data Number loads, fused numeric
  compare/branch emission, activation-local backedge polling, and deopt literal
  rematerialization landed as one measured instruction-count slice. The
  version becomes active only after one complete all-fast-hit iteration; an
  accessor, proxy, non-number, or IC miss keeps the canonical `[[Get]]` path.
  On the controlled M1 five-kernel matrix it improved the property kernel by
  1.41x, the branch kernel by 1.11x, and geomean by 1.09x, while the largest
  unrelated regression stayed below 2%.

### 1.1 x86_64 Template Backend

- [ ] Implement x86_64 emission from the existing `TemplatePlan` and entry ABI.
- [ ] Reach AArch64 parity for exact side exits, runtime calls, safepoints, IC
  probes, code lifetime, and differential coverage.
- [ ] Add architecture-specific optimization only after parity gates pass.

### 1.2 Baseline V2

Each lever lands independently with workload measurements plus a
differential/GC-stress regression case.

- [ ] Add profitable self-recursive and bounded polymorphic-method inlining for
  measured Richards/DeltaBlue gaps.
- [ ] Keep floating-point values resident where measurements justify it.
- [x] Add fused compare/branch emission to the optimizing tier; keep template
  fusion measurement-triggered.
- [ ] Improve PIC probing and protector/version cells.
- [ ] Add architecture-specific peepholes that preserve semantics.
- [ ] Remove experiments that do not win representative workloads.

### 1.3 Runtime Throughput

- [ ] Add a persistent content-addressed bytecode/module cache.
- [ ] Add an immutable or copy-on-write bootstrap image.
- [ ] Make TypeScript stripping/lowering one-pass with correct type-only import
  erasure.
- [ ] Add package/loader metadata caches.
- [ ] Close package-script and representative workload compatibility gaps.
- [ ] Measure and tune object, property, and array storage paths.

### 1.4 Optimizing-Tier Prerequisites

- [ ] Stabilize typed feedback epochs and bounded target/type distributions.
- [ ] Complete dependency/protector invalidation contracts.
- [x] Complete the deopt frame-state schema, verifier, and interpreter-frame
  reification stub.
- [x] Enforce callee-identity guards for spliced calls.
- [x] Stabilize backend-independent typed SSA, CFG, liveness, register
  allocation, and safepoint models.
- [ ] Tune tier policy from real hot workloads.
- [x] Remove the measured duplicate instruction/operand decoding path: the VM
  now translates compiler wordcode once into one 32-byte execution record and
  hot dispatch reads it without repeated function-table lookup. Typed execution
  entry for return, primitive coercion, and the widest method-call handler also
  bypasses the generic opcode schema after verification. Across the five
  controlled kernels these slices improved interpreter medians by 1.87-2.34x
  (2.13x geomean) and cut the property-kernel Node `--jitless` gap from 27.2x
  to 11.8x with unchanged peak RSS. The follow-up typed variadic-argument
  window now carries verified execution records through ordinary calls,
  explicit-`this` calls, proper tail calls, and constructors without rebuilding
  schema operands. Relative to that checkpoint it improved the same five
  interpreter kernels by another 1.09% geomean, bytecode calls with four
  arguments by 1.73%, computed explicit-`this` calls by about 1%, and the
  constructor corpus by about 2%; zero-argument calls and tail recursion stayed
  neutral within measurement resolution. The next interpreter-only call-entry
  batch removes repeated callable-family probes, reuses the already resolved
  `CodeBlock`, carries the decoded closure through frame preparation, and reads
  closure id/state in one heap access. A clean 25-sample A/B against `123d81df`
  improves bytecode calls by 2.70% at arity 0, 1.41% at arity 4, and 0.72% at
  arity 8; the monomorphic method kernel stays neutral. The arity-4 process
  retires 44.10-44.15 billion instructions after the change versus 45.01
  billion before it, a reproducible 1.92-2.02% reduction without JIT state,
  atomics, or a new cache. The following jitless batch removes four measured
  sources of useless work: cooperative cancellation is polled at loop entry,
  back-edges, and tail calls instead of every instruction; the turn-local
  reduction count is derived from the total instead of maintained beside it;
  narrow fixed-shape opcodes read verified operand words straight out of the
  32-byte execution record while variadic and wide opcodes resolve the overflow
  table; argument windows bind from one operand-word slice and skip both
  side-record vectors for callees without `arguments` or a rest parameter; and
  Number arithmetic folds without a handle scope or a `NumericKind` round trip.
  Measured as retired instructions per process on M1, that batch cuts bytecode
  calls by 17.2% at arity 0, 19.0% at arity 4, and 20.1% at arity 8, and the
  five controlled kernels by 14.3-20.1%.
  The next jitless batch removes per-instruction, per-call, and per-property
  work rather than adding caches: every execution record carries its own
  runtime-budget charge so metering no longer re-derives one from the opcode;
  back-edge OSR accounting is gated on the loop-invariant JIT-installed flag;
  callees that capture nothing and bodies without direct `eval` keep the
  cell-building and environment-installing paths out of line; an ordinary call
  binds its arguments and pushes its frame without a prepared-frame record,
  with only generator entry keeping a separate tail; frame pop resolves the
  whole construct/derived/async completion vocabulary from one cold-pool probe;
  relational comparison folds Number operands in IEEE-754 directly; `Type(v)`
  primitive classification costs one body type-tag read instead of three; an
  own dense array element answers a computed integer index without spelling it
  as a heap string through `ToPropertyKey`; and a shape-matched data-slot IC
  hit no longer re-reads the shape body to re-prove the slot is not an
  accessor. Measured as retired instructions per process on M1, the batch cuts
  bytecode calls by 10.7% at arity 0, 11.2% at arity 4, and 11.4% at arity 8,
  the monomorphic method kernel by 9.7%, branch-phi by 9.9%, the boxed-double
  property kernel by 10.9%, numeric-leaf by 8.5%, and the dense-array kernel by
  42.8%. One measurement rule came out of it: the arms of `dispatch_loop_inner`
  share one register allocation, so a change confined to a cold arm can still
  move every kernel by around 1%; per-opcode fast paths belong in the helper
  the arm already calls.
  The following batch moved the coercion the arithmetic operators need out of
  the instruction stream and into the operators themselves. `Op::Add`, the
  relational comparisons, and the non-additive numeric / bitwise / shift
  opcodes now run their own `ToPrimitive` / `ToNumeric` ladders in the order
  §13.15.3, §7.2.13, and §7.2.14 prescribe, so the compiler emits the operator
  alone. That removed four dispatched instructions per non-additive arithmetic
  operation and two per addition or comparison, all of which reduced to
  identity as soon as the operand was already a Number. On the reference
  `numeric-leaf` body the compiler now emits 25 bytecodes where it emitted 49,
  against 23 for V8's Ignition on the same source. Measured as retired
  instructions per process on M1 the interpreter improved by 11.8-35.4% across
  the eight controlled workloads, the template tier by 15.5-27.2%, and the
  optimizing tier by 7.2-30.7% on four of five kernels with `branch-phi`
  neutral. The full Test262 corpus is byte-identical to the pre-change run:
  53289 tests, the same 463 failures, 11 timeouts, and zero crashes.

  Two invariants came out of it. Removing the coercion opcodes removed the
  producers an optimizing-tier whitelist recognized, so a checked
  tagged-to-numeric conversion is now admitted for any value with a machine
  home — a parameter or any bytecode operation result — rather than for an
  enumerated set of producer opcodes. And recycling the operand temporaries
  must not hand the result the register an operand still occupies: the opcode
  reads before it writes, so an aliased destination is semantically fine, but
  the optimizing tier side-exits on it and pins the function to the template
  tier.
  A separate pass cleared ten of the eleven Test262 timeouts, each a distinct
  defect rather than general slowness. `Op::IteratorClose` ran the iterator's
  `[[return]]` while the loop body's own `try` handlers were still pushed, so a
  throwing `return()` was caught by the body and the abrupt completion was
  lost, leaving `for (x of it) { try { return; } catch {} }` spinning forever;
  the close now disarms the catch arms inside the iterator's region on the
  throwing path while leaving their `finally` blocks to run. `Date`'s local-time
  getters re-resolved the host time zone on every call at roughly a hundred
  microseconds each, which is now resolved once per isolate. `GetSubstitution`
  grew its replacement buffer until the final string allocation failed, and now
  stops as soon as the expansion passes what the heap could hold. `WeakMap` and
  `WeakSet` scanned their entries linearly, making a hundred-thousand-entry
  chain quadratic; both now carry an identity index that the ephemeron walk
  invalidates, since a moving collection relocates the addresses its hashes are
  derived from. The one remaining timeout, `dst-offset-caching-3-of-8`, is no
  longer a time-zone problem: its cost is the surrounding interpreter loop and
  the `new Date` per probe, so it belongs to the general throughput work.
  Toward a template interpreter, the frame register window is now verified once
  when a `CodeBlock` is built rather than bound-checked on every access. The
  opcode schema already declares which operands address the window — both the
  `Register`-encoded operands and the `Imm32` local indices of `LoadLocal` /
  `StoreLocal` — so `register_access_at` drives the verifier and the set cannot
  drift as opcodes change. The hot dispatch arms (`LoadLocal`, `StoreLocal`, the
  binary arithmetic and comparison operands, the destination commit, and the
  `JumpIf*` conditions) then read and write the window unchecked. Retired
  instructions on M1 fell 2.8-4.9% across the eight controlled workloads. This
  is the "verify once, access unchecked" invariant a template interpreter needs:
  generated handlers cannot afford a bounds check per operand.
- [ ] Replace the remaining measured dispatch/boxed-register/runtime-entry
  bottleneck with generated bytecode handlers and shared IC fast paths; retain
  the interpreter as semantic oracle rather than weakening its contracts.

Deopt remains keyed by dense `DeoptExitId`, not bytecode PC. Abstract frame
states lower to an outermost-first frame chain; every inlined frame is
reified, caller frames resume after their call, and the ordinary return path
fills the caller result register. Exit-metadata compaction and lazy exit
compilation remain measurement-triggered, not prerequisites.

### 1.5 Optimizing Tier

- [x] Start with local numeric specialization and verified deopt.
- [x] Add monomorphic plain and method inlining after call-target stability and
  reconstructed-frame tests are proven.
- [ ] Generalize the shipped property-loop versioning into SSA expression
  LICM, then add loop constant propagation, dead scaffolding elimination, and
  measured unrolling to close the remaining native instruction-count gap.
- [ ] Choose Cranelift/custom emission from measurements rather than treating a
  backend as an architectural premise.
- [ ] Own suspend/resume semantics for `TailCall`, generator/await/promise, and
  async-module opcodes before removing their template-tier side exits.

### Engine Verification

Every phase-changing engine commit runs the relevant subset of:

```bash
cargo test -p otter-vm --locked
cargo test -p otter-jit --locked
cargo test -p otter-runtime --locked
cargo clippy -p otter-vm -p otter-jit --all-targets --locked -- -D warnings
cargo build --release -p otter-cli -p otter-difftest -p otter-benchmark \
  --features otter-benchmark/phase0 --locked
target/release/otter-difftest --otter target/release/otter \
  --gc-strides 1,2,4,8,16
```

Also preserve affected `ES_CONFORMANCE.md` results, startup/code-size
baselines, deterministic benchmark success markers, and capability/runtime/GC
invariants.

## 2. Debugging, Tracing, And Profiling

### Shipped Surface

- [x] Text/JSON bytecode disassembly and current text step trace.
- [x] Opt-in `.cpuprofile` and folded-stack sampling.
- [x] Embedder IC, shape, frame, heap-summary, and Chrome heap snapshots.
- [x] One production CLI policy plus `--jitless` selecting the existing
  interpreter oracle. Explicit tier controls remain confined to benchmark and
  Test262 harnesses.
- [x] Bounded owned JIT events and current-format JIT artifact bundles with
  machine code, normalized code, annotated ARM64, relocations, code maps,
  deopt metadata, and safepoints.
- [x] Direct-call/method/global-load/static-native lowering and deopt events.
- [x] Benchmark idle-memory and complete JIT runtime-stat deltas.

### Active Diagnostics Work

- [ ] Cover JIT recompilation and abrupt compiler failure in artifact tests.
- [ ] Add golden artifact tests for branches, calls, safepoints, OSR entries,
  and deopt exits.
- [ ] Record exact safepoint native-return offsets and join them to assembly.
- [ ] Add a live runtime code-range map for profiler symbolization.
- [ ] Account for native JIT body time instead of the last interpreter frame.
- [ ] Symbolize samples with function, tier, code offset, and source/bytecode
  location while preserving Chrome/V8 CPU-profile compatibility.
- [ ] Add tier-entry, OSR, deopt, compile, and invalidation Chrome/Perfetto
  events.
- [ ] Quantify disabled/enabled profiling overhead.
- [ ] Add a bounded timeout ring buffer with deterministic snapshots.
- [ ] Add Chrome/Perfetto async/op spans with stable parent/span linkage.
- [ ] Retain Test262 diagnostics only for failures and timeouts.
- [ ] Preview-cap large values and explicitly bound every diagnostic buffer.
- [ ] Validate every machine-readable output against its target tool or its
  complete current schema.

Diagnostics slices run:

```bash
cargo test -p otter-vm
cargo test -p otter-jit
cargo test -p otter-runtime --test jit_call_lifecycle
cargo check -p otter-web
cargo test -p otter-runtime
```

## 3. `Otter.serve`

### Boundaries And Current State

- `otter-modules` owns options, permissions, HTTP transport, Fetch conversion,
  and the `Server` host object.
- `otter-web` owns standard Request/Response/Headers/Web Streams behavior.
- `otter-runtime` owns keep-alive, isolate inboxes, and typed runtime tasks.
- `otter-vm` exposes only generic roots/context helpers and contains no HTTP
  policy.
- [x] `Otter.serve` and `import { serve } from "otter"` are wired.
- [x] Requests enter the isolate through typed runtime tasks and persistent
  callback roots.
- [x] Server lifecycle exposes stop/close/ref/unref and runtime liveness.
- [x] Request bytes reach the existing one-chunk `ReadableStream` layer.

The current transport remains a bootstrap `TcpListener` with
`Connection: close`; it is not the production backend.

### Active Server Work

- [ ] Allow `fetch(request)` to return `Response | Promise<Response>` and await
  settlement on the isolate event loop.
- [ ] Replace full request buffering with a host-backed
  `ReadableStream<Uint8Array>` plus abort/timeout cleanup.
- [ ] Stream `Response.body` with backpressure while preserving buffered
  string/byte fast paths.
- [ ] Replace the bootstrap listener with an async HTTP/1.1 transport while
  keeping VM interaction on the isolate thread.
- [ ] Close Hono blockers: thenable adoption, member/private-field update
  expressions, package-entry `.ts` detection, and URL/prototype gaps reached by
  the workload.
- [ ] Update the source-of-truth Otter `.d.ts` files and contributor docs.
- [ ] Add Hono smoke, throughput, latency, and cold-start benchmarks against
  equivalent applications.

Server slices run:

```bash
cargo test -p otter-modules
cargo test -p otter-runtime runtime_keep_alive_liveness_is_idempotent
cargo test -p otter-runtime runtime_task_runs_on_isolate_loop
```

Smoke coverage includes both serve surfaces, a `Response` round trip, POST body
stream identity, and stop/ref/unref liveness.
