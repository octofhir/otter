# Debug, Trace, and Profiling Plan

This file is the implementation tracker for engine diagnostics. Contributor
workflows that are already usable belong in `AGENTS.md` and the documentation
site; this plan records the gap between that shipped surface and the tooling
needed to optimize the VM and JIT safely.

## Invariants

- Diagnostics are default-off. A normal runtime must not allocate diagnostic
  payloads, format text, open files, or take locks.
- Configuration is explicit owned data passed from the CLI or embedder through
  `otter-runtime` and `otter-vm` to `otter-jit`. Engine crates must not discover
  diagnostics through environment variables, TLS, or process-global registries.
- JIT compilers return owned artifacts. They do not perform filesystem I/O.
  The outermost host owns filtering, directory creation, serialization, and
  write failures.
- Artifact DTOs contain no live GC handles, typed executable pointers, or
  borrows into a mutator turn. Exact machine-code bytes can contain baked
  address immediates; the bundle marks those bytes runtime-local and describes
  the sites with symbolic relocation records. Artifact ownership remains safe
  across GC, abrupt completion, and nested JIT re-entry.
- Standard formats are the primary interchange when one exists:
  Chrome Trace Event, Chrome/V8 CPU profiles and heap snapshots, standard
  folded stacks, raw machine-code bytes, and ordinary annotated assembly text.
- Every current-format JSON artifact has shape tests. Absolute code addresses are
  runtime-local metadata and must not appear in annotated assembly or be
  required for portable comparisons.
- Debugging must not create a second compiler traversal, GC traversal, or
  raw-root snapshot path. Artifact data is captured from already-owned compiler
  plans, metadata, and emitted code.

## Shipped Surface

- [x] Bytecode disassembly through `--dump-bytecode` (text) and
  `--dump-bytecode=json` (compile and exit).
- [x] Current text step trace through `--trace` or `--trace=<path>`.
  It records interpreter-dispatched bytecode only.
- [x] Opt-in VM stack sampling through `--cpu-prof`, producing Chrome/V8
  `.cpuprofile` and standard `.folded` outputs. Sampling is driven by bytecode
  dispatch ticks and is currently blind while native JIT code is running.
- [x] Embedder snapshots for inline caches, shape transitions, frames, heap
  summaries, and Chrome `.heapsnapshot` output.
- [x] Explicit runtime tier selection (`ProductionTiered`, `Template`, or
  `InterpreterOnly`); template-only runs do not execute optimizer policy.
- [x] Owned current-format JIT events through
  `--jit-events[=<path>]`: compile preparation/results, inline candidates,
  side exits, and inline-frame materialization. Capture is bounded, survives
  abrupt completion, and includes JIT work performed by event-loop callbacks.
- [x] Owned current-format JIT compile bundles through
  `--jit-artifacts[=<directory>]`: exact runtime-local code, bytecode,
  template plan or the selected optimizing backend input (Otter unit or CLIF),
  typed symbolic relocations, portable normalized code, annotated ARM64
  assembly, native offset maps, deopt metadata, and safepoints. The CLI writes
  a new root atomically under a cooperative single-writer contract and never
  intentionally merges with an existing target.
- [x] Template plain-call and method-inline artifacts expose caller/callee
  identity, exact guard/setup/body/hit/deopt-teardown native ranges,
  per-callee-operation ranges, and compact live virtual-register/receiver
  scratch assignment. Coalesced operations remain visible as zero-width
  source regions.
- [x] Compiler-generated plain calls and bounded polymorphic method chains
  expose typed method/call guards, frame setup, native entry, return, cleanup,
  and entry-reject regions plus one symbolic stable-entry-cell relocation per
  exact target. Exact artifacts expose caller/callee identity, target
  generation/tier, call kind, receiver binding, method
  receiver/prototype/slot facts, call PC, frame/linkage/total stack bytes, and
  register count; portable normalized code excludes only the
  generation-local code-object id.
- [x] Compiler-generated callee deopts emit an exact-generation structured
  event with baked call kind, exact caller/callee code-object ids, caller
  call-site PC, callee resume PC, tier, and consecutive deopt streak; baseline
  streaks feed the existing cold eviction policy.
- [x] Compiler-generated call plan events distinguish plain/method edges and
  expose an exact available generation/tier/`this` mode or a typed planning
  rejection (`missing callee`, ineligible, self-recursive, own-upvalue
  allocation, unavailable method guard, or no entry generation). Bounded
  method chains carry `targetIndex` / `targetCount`, while compile preparation
  separates direct-method site and target totals.
  Successful compiles then report the actual backend lowering (`generated`,
  `inlined`, layout rejection, or elimination) separately for every target,
  while compile preparation separates generated-link counts from leaf-inline
  candidates.
- [x] Ordinary-call feedback identifies bytecode callees and static-native
  operations in one typed target population. Monomorphic original `Math.abs`
  sites expose separate static-native plan and actual-lowering events, typed
  guard/body code-map regions, and an address-free bootstrap-function
  relocation. Disabled artifact capture builds neither code-map nor relocation
  DTOs.
- [x] Capture Cranelift numeric leaves through the existing optimizing bundle:
  backend-marked CLIF, the exact installed code object, one structural code-map
  region, and empty relocation/safepoint/deopt inventories.
- [x] Benchmark-only fresh-process idle-memory capture: runtime bootstrap and
  empty-turn drain, phased process RSS, forced-full-GC counters, live/space/
  off-heap accounting, controlled idle RSS, and release binary size. The
  normal runtime path enables no sampler; its public snapshots copy existing
  counters only.
- [x] Kernel benchmark records expose every `JitRuntimeStats` field as an
  informational delta spanning warmup plus measurement. Snapshots execute
  outside timed samples and explicit zeroes keep tier inactivity visible.

The text step trace is not a Chrome/Perfetto trace. Async/op tracing, timeout
ring-buffer dumps, and Test262 failure traces are not shipped yet. Structured
JIT events and artifact manifests are Otter diagnostic schemas, not
replacements for the planned Chrome/Perfetto timeline.

## Known Debt

- Step tracing and CPU sampling happen at interpreter dispatch. JIT-heavy
  execution therefore has a diagnostic blind spot.
- The direct synchronous runtime used by `--cpu-prof` does not enforce its
  informational timeout yet.
- A single file trace target is truncated when a command creates multiple
  sequential runtimes (notably multiple `otter test` files).
- A host-side command timeout may fire before the isolate can return its partial
  JIT event report. In that case the CLI preserves the timeout error and does
  not write a misleading empty artifact; bounded timeout snapshots remain a
  later slice.
- Exact native bytes and native-offset maps are available alongside typed
  symbolic relocations, a portable semantic code stream, and offset-based
  annotated ARM64 assembly. The normalized stream is intentionally not
  executable; assembly address sites remain symbolic and redact resolved
  process-local values.
- Bytecode PC, tier plan/IR, native code offsets, OSR entries, and deopt exits
  can be correlated. Safepoint-to-native-return offsets remain open and stay
  explicit `null` in `safepoints.json`.
- Successful JIT compile events join to artifact manifests by `codeObjectId`.
  Generated-call deopts now carry that exact id; general native-entry and
  side-exit execution events remain open.

## Ordered Implementation Slices

### 1. Explicit diagnostics configuration

- [x] Add an owned, default-empty diagnostics request shared by runtime, VM,
  and JIT compile requests.
- [x] Replace CLI timeout/trace globals with normal execution configuration.
- [x] Add an explicit CLI tier selector for reproducible interpreter,
  template-only, and production-tiered runs; keep legacy environment parsing
  only at the CLI compatibility boundary.
- [x] Replace `OTTER_JIT_TRACE` reads with structured requested events or
  artifacts.
- [x] Prove the disabled path produces no artifact payloads and performs no
  filesystem work.

### 2. Current JIT artifact bundle

- [x] Return an optional owned bundle with each successful template or
  optimizing compile.
- [x] Let the CLI/embedder persist bundles into one directory per compile.
- [x] Publish the current `manifest.json` containing target, architecture,
  tier, function identity, module, entry kind, bytecode/code sizes, and the
  files present.
- [x] Emit exact runtime-local `code.bin`, deterministic `code-map.json`, and
  symbolic `relocations.json`. Portable byte comparisons use a normalized view
  with relocation immediates redacted, not the exact executable bytes.
- [x] Emit tier input from the already-owned compiler representation:
  template plan for the template tier and optimized unit/IR for the optimizing
  tier.
- [x] Expose deopt-safe template plain-call and method inlining as typed
  code-map regions, including identity guards, compact scratch slots,
  deterministic entry values, and exact post-teardown deopt boundaries.
- [ ] Cover multiple functions, recompilation, OSR entry, abrupt compile
  failure, nested JIT re-entry, and full-GC relocation.

Multiple functions, template and optimizing OSR, abrupt execution, nested JIT
re-entry, full-GC ownership, typed relocations, and portable code comparison
are covered. Recompilation and abrupt compiler failure remain follow-ups in
this roadmap item.

Bundle shape:

```text
jit-<ordinal>-<tier>-f<function-id>/
  manifest.json
  bytecode.txt
  template-plan.txt | optimized-ir.txt
  code.bin
  code-normalized.bin
  asm.txt
  code-map.json
  relocations.json
  deopt.json
  safepoints.json
```

Files that do not apply to a tier are omitted and listed as absent by the
manifest rather than emitted as placeholders.

### 3. Annotated ARM64 assembly

- [x] Disassemble emitted AArch64 bytes without executing them.
- [x] Annotate native offsets with function, entry kind, bytecode PC,
  template operation or optimized IR instruction, runtime transition,
  OSR entry, and deopt/side-exit id when known. Safepoints are included as an
  explicit summary with unavailable native offsets rather than attached to a
  fabricated instruction.
- [x] Annotate template call/method-inline caller/callee identity, compact
  scratch layout, callee operation index, and distinct hit/deopt-teardown
  regions without inferring runtime execution.
- [x] Use code offsets in annotations and render baked address sites as
  symbolic relocations. Never print absolute executable addresses in ASM;
  exact `code.bin` remains explicitly runtime-local.
- [ ] Add golden tests for branches, calls, safepoints, OSR entries, and deopt
  exits.
- [ ] Record exact safepoint native return offsets and join each safepoint to
  the corresponding assembly range.
- [ ] Add a separate runtime code-range map only for live profiler symbolization.

Deterministic unit coverage is green for direct branch/call labels, relocation
redaction, decoder `.word` fallback, OSR annotations, deopt summaries/region
joins, Cranelift numeric-leaf source-or-backend-glue coverage at every
four-byte machine offset, and the explicit safepoint-unavailable summary. The
combined golden item remains open until emission records a real safepoint
return offset and a test can cover that exact native range without inference.

### 4. JIT-aware profiling and traces

- [ ] Account for time spent in native JIT bodies instead of attributing it to
  the last interpreter frame.
- [ ] Symbolize samples with function, tier, code offset, and source/bytecode
  location while preserving the Chrome/V8 CPU profile schema.
- [ ] Add tier-entry, OSR, deopt, compile, and invalidation events to a
  Chrome/Perfetto trace.
- [ ] Validate that profiling remains opt-in and quantify disabled/enabled
  overhead before declaring it negligible.

### 5. Hang, async, and conformance capture

- [ ] Add a bounded timeout ring buffer with deterministic snapshots.
- [ ] Add Chrome/Perfetto async/op spans with stable parent/span linkage.
- [ ] Let Test262 retain diagnostics only for failures and timeouts.
- [ ] Keep large values preview-capped and make all buffers explicitly bounded.

### 6. Documentation and validation

- [x] Add a docs-site JIT debugging guide once the artifact CLI is real.
- [x] Document bundle shapes, annotated assembly, source correlation, and
  before/after optimization workflow.
- [ ] Add current-shape tests that load every machine-readable output in its
  target tool or validate the complete emitted structure.

## Required Verification

Every engine diagnostics slice runs:

```sh
cargo test -p otter-vm
cargo test -p otter-jit
cargo test -p otter-runtime --test jit_call_lifecycle
cargo check -p otter-web
cargo test -p otter-runtime
```

Run focused CLI/schema/golden tests for the surface changed by the slice.
Diagnostics that interact with moving values also require full-GC, abrupt-exit,
and nested-JIT invariants. No performance claim is accepted without a
reproducible release baseline captured after the correctness matrix is green.
