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

- Snapshot date: 2026-08-24.
- Observed commit: `708c7bc5` (clean tree at snapshot time).
- The checkout may change in parallel. Re-snapshot touched files immediately
  before each edit and merge with concurrent work; never reset or overwrite it.
- Checkpoint gates passed on this commit: compile-cache unit/Unix tests
  (incl. root-symlink and hardlinked-lock regressions), worker suite under
  `OTTER_GC_STRESS` 1/13, `resource_accounting`, `otter-resource`,
  release `otter-bytecode` verifier/mutation tests, workspace
  `cargo check --all-targets`, and `clippy --all-targets --all-features
  -D warnings`. These are local observations, not a publishable clean-tree
  baseline.
- The published `ES_CONFORMANCE.md` snapshot is historical. A fresh full run
  is required before claiming a new conformance number.
- No performance result from this dirty snapshot is eligible for publication.

## Program scorecard

| ID | Lane | Priority | Status | Depends on | Active outcome |
|---|---|---:|---|---|---|
| H1 | Security | P0 | complete | none | every HTTP redirect hop and cached final alias is authorized |
| H2 | Isolation | P0 | complete | R1 | bounded, capability-aware Workers with deterministic shutdown |
| B1 | Bytecode | P0 | active | none | mandatory panic-free verifier before any bytecode executes |
| R1 | Resources | P0 | active | none | one aggregate runtime budget and typed exhaustion errors |
| R2 | Resources | P1 | active | R1 | bound code, source, module, queue, worker, and external memory |
| E1 | Embedding | P1 | started by H1 | none | one reusable capability evaluator for all host surfaces |
| J1 | JIT | P1 | active lane | B1, R1 | one typed target-neutral compiled pipeline |
| J2 | Portability | P1 | queued | J1 | x86-64 parity over the same Machine IR |
| C1 | Conformance | P1 | queued | current slices | reproducible green targeted and full Test262 baselines |
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
descriptors. The next slices must bound the guaranteed-completion backlog,
move dynamic-import liveness to an owned admission carrier, and charge retained
message bodies, module sources, timers, host operations, external memory, and
generated code at their physical owning lifetimes. Existing CPU reduction
metering remains authoritative until it is folded into the same public budget
configuration and telemetry surface.

### R2. Close unbounded stores and queues

Landed: retained module source text is accounted end to end. `SharedSource`
carries one exact `SourceModuleBytes` charge per physical allocation through
the module loader, data/`data:` synthesis, the remote provider contract
(`RemoteModuleRequest.account`, streamed chunk admission in the built-in
Tokio provider), the redirect alias cache (aliases share one charge), the
module graph, `LinkedProgram`, and the VM source registry, which admits
VM-synthesized wrappers against the runtime account. Local file reads stream
through the charged builder; script/eval registration admits before
retention; charges die with the isolate and rejections are typed and
observable. Worker count and message queues/bytes are bounded by H2.

Also landed: every installed JIT code object charges its exact retained
bytes — executable mapping, operand/IC/safepoint tables, dependencies, OSR
map, and the full deopt-frame metadata — to `GeneratedCodeBytes` through the
isolate registry, sharing the runtime account with source admission. A
rejected budget declines the install (the function stays on its previous
tier) with a visible rejection; physical retirement releases the charge. Per-function `[[SourceText]]` no
longer duplicates the module source: functions carry validated byte ranges
into one shared module-level snapshot, and `Function.prototype.toString`
slices it through the owning chunk.

Also landed: every linked `CodeSpace` chunk charges its exact retained bytes
(bytecode module, executable view, atom table) to `SourceModuleBytes` at link
time, before publication, against the linking interpreter's account; a
rejected budget is a typed `BytecodeLinkError::RetainedBytes` that leaves the
registry unchanged, and the charge is released when the code space drops with
its isolate. Restored snapshot isolates keep their donor-charged chunks.

Still open, in R1 terms:

- Size-driven chunk eviction. Design constraints from the ownership audit:
  escaped function values are bare `u32` ids with no ownership edge to their
  chunk, so eviction requires a liveness proof, not a refcount. The intended
  shape is a GC-census pipeline at an explicit between-turns safepoint:
  select candidate chunks (unloaded eval/dynamic-import graphs), invalidate
  and retire any JIT code for the id range, prove via full-heap census that
  no live closure/frame/module-registry/timer reference carries an id in the
  range, then splice the chunk under the single-writer link lock, leaving a
  slim tombstone node so lock-free readers never observe a freed link.
  Resolution of an evicted id must stay a typed miss, never a stale hit;
- Remaining host-owned backing stores outside the ArrayBuffer/Blob paths.
  JS-side body buffering (`response.text()`/`arrayBuffer()` chunk collection,
  stream queues) accumulates `Uint8Array` chunks whose ArrayBuffer backing
  stores charge heap external memory and therefore the `ExternalBytes`
  ledger; host-class byte payloads (Blob/File) charge at construction and
  release on collection; the native fetch boundary holds one bounded chunk
  with pull backpressure. Residual audit target: any Rust-side retained
  `Vec<u8>`/`BytesMut` accumulation not represented by a charged token.

Also landed: heap external charges are folded into the runtime ledger.
`GcHeap` mirrors its outstanding external/off-slot reservation bytes into an
aggregate resizable `ExternalBytes` lease on the runtime account; growth is
admitted on the ledger before any heap booking (typed
`OutOfMemory::ExternalBudgetExceeded` on refusal), shrinks follow every
release path, and installation charges already-outstanding bytes atomically.
`ExternalMemory` tokens no longer hold a raw heap pointer — releases flow
through the shared deferred-release channel, so payload-embedded tokens stay
sound across isolate moves and heap teardown (the old raw-pointer release
wrote through stale heap copies after a move).

Use streaming or bounded reads where whole-body collection is not semantically
required. Eviction must be deterministic and must not invalidate live code or
GC roots.

### R3. Resource performance

For each bounded resource record idle cost, steady-state cost, peak usage,
rejection count, and recovery to baseline. Prefer zero-allocation fast checks,
batched accounting, and cache-friendly counters, but only after correctness is
measured.

## B — Bytecode and artifact integrity

### B1. Mandatory bytecode verifier

Create a focused verifier owned by the bytecode/VM boundary. It validates all
untrusted or deserialized functions before installation or execution:

- opcode and operand decoding without out-of-bounds reads;
- register, constant, upvalue, exception, jump, and source-map ranges;
- instruction-boundary and control-flow targets;
- stack/register dataflow needed by interpreter and JIT invariants;
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
remaining B1 closeout is the release verifier/mutation gate set.

### B2. Fuzzing and differential checks

Continuously fuzz parser -> compiler -> verifier -> interpreter, serialized
artifacts, GC stress, and JIT deopt reconstruction. Minimized regressions become
normal tests. Compare supported semantics with the interpreter oracle and,
where licensing and determinism permit, established ECMAScript engines.

## J — JIT, portability, and throughput

### J1. One compiled pipeline

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

Before changing a language area, consult `ES_CONFORMANCE.md` and run its focused
Test262 subset. After a substantial semantic slice, capture a fresh full run on
a stable checkout and update the report with commit, configuration, pass/fail,
timeout, and deltas. Never hide timeouts or compare partial runs as full runs.

Current baseline (see `ES_CONFORMANCE.md` for the full report): 99.79% at
`f8115ad8`, 111 fails, 0 crashes/timeouts. Follow-up slice (125→111):
legacy Intl-constructed chaining on service-instance receivers +
proxy-observable unwrap; per-closure-instance name/length deletion with
real %Function.prototype% fallback; spec-ordered bind reads (proto,
length, name — Proxy traps observe exactly those); dynamic Function
without a self-name binding; proxy trap dispatch snapshots the target
before the handler lookup (revoke-as-side-effect), getPrototypeOf
invariant runs the target's isExtensible trap; array iteration and
join take the observable [[Get]] for anything but plain dense elements. Latest slices (169→125, zero
regressions): the Temporal wave went green (6642/6642 — ZonedDateTime
offset-option string parsing via ParsedZonedDateTime, ToBigInt
constructor coercion, identifier-only constructor time zones, fallible
hoursInDay, PlainTime sub-second compare + per-field RegulateTime,
PYM/PMD from date-bearing instances, %Object.prototype% on the
namespaces, DateTimeFormat ISO-field civil conversion, h24 midnight,
zero-offset GMT, PYM/PMD exact-calendar match); dispatch/object-model
fixes (WeakMap/WeakSet expandos, bound-function [[Prototype]] slot +
proxy-aware bind, proxy get-trap invariant on class-ctor prototype,
temporal prototype-walk fallback, derived-this cell reads for escaped
arrows); and compiler class/eval work (per-evaluation class cells via
FreshUpvalue, eager super-store base, default-derived-ctor live parent,
direct eval super() in derived constructors, static-block/-field
eval-binding tables, HomeObject-gated super legality in eval).
Remaining fail mass, largest first: staging/sm (~95, heterogeneous —
Function metadata, RegExp constructor edges, TypedArray cross-realm,
Proxy/Reflect realm semantics), intl402 DateTimeFormat
chinese/dangi/era/formatRange rendering (11), annexB parser cluster
(7, oxc-level), language/eval-code deletable-binding closures (6),
import-defer (2), legacy Intl-constructed symbol object model (4).

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
