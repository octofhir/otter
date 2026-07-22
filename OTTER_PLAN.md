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
  to 11.8x with unchanged peak RSS.
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
