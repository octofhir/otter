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

- Snapshot date: 2026-08-30.
- Observed commit: `242593b2` (clean tree at snapshot time).
- The checkout may change in parallel. Re-snapshot touched files immediately
  before each edit and merge with concurrent work; never reset or overwrite it.
- Checkpoint gates passed on this commit: compile-cache unit/Unix tests
  (incl. root-symlink and hardlinked-lock regressions), worker suite under
  `OTTER_GC_STRESS` 1/13, `resource_accounting`, `otter-resource`,
  release `otter-bytecode` verifier/mutation tests, workspace
  `cargo check --all-targets`, and `clippy --all-targets --all-features
  -D warnings`. These are local observations, not a publishable clean-tree
  baseline.
- `ES_CONFORMANCE.md` and `docs/site/public/conformance/data.json` are current
  as of this commit: 99.98%, 12 fails of 53575, no crashes or timeouts.
- No performance result from this dirty snapshot is eligible for publication.

## Program scorecard

| ID | Lane | Priority | Status | Depends on | Active outcome |
|---|---|---:|---|---|---|
| H1 | Security | P0 | complete | none | every HTTP redirect hop and cached final alias is authorized |
| H2 | Isolation | P0 | complete | R1 | bounded, capability-aware Workers with deterministic shutdown |
| B1 | Bytecode | P0 | complete | none | mandatory panic-free verifier before any bytecode executes |
| R1 | Resources | P0 | complete | none | one aggregate runtime budget and typed exhaustion errors |
| R2 | Resources | P1 | complete | R1 | bound code, source, module, queue, worker, and external memory |
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
bodies are charged at their physical owning lifetimes (see R2).

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

Also landed: size-driven code-chunk eviction. Eval and on-demand module chunks
publish immutable function/IC ranges and a separately releasable payload. A
tombstone keeps every numeric range permanently spent; resolution distinguishes
live, evicted, and never-linked ids. `ExecutionContext` owns one payload `Arc`,
and all table access is chunk-local: foreign users first resolve the owning
context instead of borrowing through an ambient chunk. Retained contexts in
realms, module records, timers, queued work, and dynamic imports therefore
block physical reclamation independently of the bare-id census.

At a between-turn high-water check, the VM completes a full collection and
walks every live GC payload and isolate-owned value store that can carry an
immediate function id. Candidate bits and ranges are allocated before the
walk; callbacks only inspect values and flip bits. Dead candidates are ordered
largest first. Before a payload is tombstoned, all installed JIT generations
are invalidated and physically retired; the conservative whole-isolate retire
also covers cross-chunk inlining. Feedback and function-keyed side tables are
then purged, and dropping the payload releases its exact `SourceModuleBytes`
lease. Public telemetry reports census passes, live skips, reclaimed chunks
and bytes, current retained bytes, and the observed peak. Live evictable bytes
are maintained incrementally in `CodeSpace`, so linking repeated eval chunks
and reading telemetry remain O(1) instead of rescanning the append-only chain.

The design was checked against production engines rather than inferred from
Otter alone. [V8's function architecture](https://chromium.googlesource.com/v8/v8/+/fdedda15a100dc8b7277cd9c96c7f7372ecb5cf3/docs/runtime/function-architecture.md)
separates closure/context state from shared bytecode and dispatch indirection;
its [mark-compact code flusher](https://chromium.googlesource.com/v8/v8/+/refs/heads/main/src/heap/mark-compact.cc)
tests bytecode liveness and removes baseline code before discarding compiled
metadata. JavaScriptCore's
[DFG plan](https://github.com/WebKit/WebKit/blob/main/Source/JavaScriptCore/dfg/DFGPlan.cpp)
exposes compiling code blocks and their dependencies to GC, while its
[heap](https://github.com/WebKit/WebKit/blob/main/Source/JavaScriptCore/heap/Heap.h)
has explicit executing/compiling-code enumeration and code deletion. SpiderMonkey
requires an [exclusive trace session](https://searchfox.org/firefox-main/source/js/src/gc/GC.h)
for heap iteration, passes a no-GC token to callbacks, and exposes explicit JIT
release. These agree on the properties Otter adopts here: root-derived
liveness, non-allocating census callbacks, and executable retirement before
the shared bytecode payload disappears.

Registry-lock cost was measured separately from tiering. A mixed-tier CLI
comparison was rejected because the same-chunk case compiled and inlined while
the foreign callee remained interpreted. The final release measurement used
`JitSelection::InterpreterOnly` and identical two-million-call loops: local
dispatch took 231.011 ms and a `new Function` sibling chunk took 231.149 ms
(about 0.06% difference). The call path uses a four-entry dispatch-local,
epoch-validated owner cache; it retains no context across the between-turn
census.

The residual host-owned backing-store audit now covers `Otter.serve`. Incoming
HTTP bodies are consumed frame by frame instead of through whole-body
`collect()`, and every retained Rust byte is admitted before buffer growth
against both the runtime `ExternalBytes` account and an always-finite
per-server ledger. One buffered request or response is capped at 16 MiB and
one server at 64 MiB without adding JavaScript options, permission classes, or
configuration ceremony. Known oversized requests fail before their body is
polled; incremental overflow is 413, aggregate pressure is 503, and rejected
or partially read bodies recover both ledgers. Response leases move with the
Hyper body and remain live until network delivery drops it.

This is deliberately an internal safety boundary, not the final Web API.
Hyper models a body as asynchronously yielded frames, the Fetch standard
models request and response bodies as streams, and Node's HTTP API explicitly
avoids buffering entire messages. Otter therefore keeps its public
Fetch-shaped call simple while using bounded buffering only until server-side
Web Streams can carry backpressure end to end:
[Hyper `Body`](https://docs.rs/http-body/latest/http_body/trait.Body.html),
[Fetch bodies](https://fetch.spec.whatwg.org/#concept-body), and
[Node HTTP](https://nodejs.org/api/http.html).

The active `node:zlib` binding now follows the same boundary. Brotli and
Zstandard advance their native state directly into the caller's output chunk;
the handle no longer accumulates a whole compression or decompression step in
an unbounded `Vec`. The remaining zlib dictionary and parameter-change carry
buffers are admitted before allocation against both `ExternalBytes` and an
always-finite module ledger: 16 MiB per dictionary, 64 KiB per carry buffer,
and 64 MiB across one installed binding. Brotli and Zstandard additionally
reject frames requiring decoder windows over 16 MiB. These are internal safety
defaults; Node's existing `chunkSize`, dictionary, and stream API remain
unchanged and no permission/configuration surface was added.

Dynamic deflate parameter changes pre-admit their complete carry capacity
before `deflateParams` can close a block. A `Z_BUF_ERROR` rolls that admission
back and remains an error because zlib specifies that the parameters were not
changed; successful output stays charged until the next caller buffer drains
it ([zlib 1.3.1 manual](https://www.zlib.net/manual.html#Advanced)).

This matches Node's documented model: compression is a `Transform`, with one
internal output slab whose `chunkSize` defaults to 16 KiB, while
`maxOutputLength` limits whole-result convenience methods rather than native
stream retention. The Rust codec APIs also expose the required bounded step:
Brotli accepts caller-owned input/output slices and reports
`NeedsMoreInput`/`NeedsMoreOutput`, and `zstd-safe` streaming calls consume an
`InBuffer` into an `OutBuffer`:
[Node zlib](https://nodejs.org/api/zlib.html),
[Brotli stream API](https://docs.rs/brotli/latest/brotli/fn.BrotliDecompressStream.html),
and [zstd-safe `DStream`](https://docs.rs/zstd-safe/latest/zstd_safe/type.DStream.html).

Native TCP and datagram delivery now account the payloads they retain across a
call or isolate-task boundary. Each installed binding has a finite ledger
capped at 4,096 messages and 64 MiB, paired with the runtime `QueuedMessages` and
`QueuedMessageBytes` account. TCP write bytes are admitted before their owned
copy enters the writer channel and stay charged until the kernel accepts or
refuses them; disconnect and send failure drop the same RAII lease. TCP and UDP
receive payloads carry the lease through the bounded isolate inbox. TCP keeps
the charge until VM backing-store adoption succeeds, while UDP keeps it through
the existing latin1 handoff. Pressure is an observable `ENOBUFS` read/write
failure rather than a silent drop. Direct `tryWrite` and `try_send_to` successes
retain no host copy and therefore take no queue charge.

This follows the ownership boundary exposed by the production stack. Node
reports when socket bytes entered user-memory buffering and emits `drain` when
that buffer is free; libuv requires write buffers to stay valid until their
callback and exposes stream/UDP queued byte counts. Tokio documents that its
unbounded MPSC can buffer arbitrarily until process memory is exhausted:
[Node `net.Socket.write`](https://nodejs.org/api/net.html#socketwritedata-encoding-callback),
[libuv streams](https://docs.libuv.org/en/v1.x/stream.html),
[libuv UDP](https://docs.libuv.org/en/v1.x/udp.html), and
[Tokio MPSC](https://docs.rs/tokio/latest/tokio/sync/mpsc/).

Child-process IPC and process capture are now bounded without extending the
JavaScript permission or option surface. Every isolate owns one finite IPC
family ledger shared by all of its channels: at most 4,096 retained messages,
64 MiB total payload, and 16 MiB for one message. The same payload is admitted
against the runtime `QueuedMessages` / `QueuedMessageBytes` ledger before its
owned copy. The writer uses a bounded Tokio channel, holds both leases until
the socket accepts or refuses the bytes, and distinguishes closed-channel
failure from observable `ENOBUFS` pressure.

The reader no longer grows a pending `Vec` from a peer-controlled 32-bit
length. It incrementally collects the fixed four-byte header, rejects an
oversized frame, admits its complete size, reserves exact capacity, and only
then copies payload bytes. Its lease crosses the bounded isolate inbox and is
released after JSON/string handoff or cancellation. Descriptor accumulation
is capped with the frame and surplus descriptors are closed. This matches the
public contract Node documents — `subprocess.send()` returns false once the
unsent backlog is unsafe — while enforcing the harder memory boundary below
that compatibility surface. Tokio's own documentation recommends bounded
MPSC when backpressure is required and warns that the unbounded form can abort
the process on OOM:
[Node child-process IPC](https://nodejs.org/api/child_process.html#subprocesssendmessage-sendhandle-options-callback),
[Node IPC implementation](https://github.com/nodejs/node/blob/main/lib/internal/child_process.js),
and [Tokio MPSC](https://docs.rs/tokio/latest/tokio/sync/mpsc/).

Process output has two distinct ownership paths. Async `spawn` pipes are
ordinary bounded native transports; `exec` / `execFile` collect VM-owned
Buffers under Node's existing 1 MiB default `maxBuffer`. Synchronous capture
is native scratch retained for the duration of one call, so it now has an
internal 64 MiB ceiling per stdout/stderr stream in addition to the caller's
smaller `maxBuffer`, truncates exactly at the boundary, kills the overflowing
child, and reports the existing `ENOBUFS` outcome. Node likewise specifies
`maxBuffer` in bytes and terminates the child when it is exceeded:
[Node `maxBuffer`](https://nodejs.org/api/child_process.html#maxbuffer-and-unicode).

The snapshot/artifact audit separates bounded diagnostics from retained
stores. JIT event capture already stops at 16,384 events; artifact capture is
already capped at 1,024 bundles and 64 MiB. The opaque in-process runtime
snapshot is an explicit caller-owned whole-heap image, bounded by the
isolate's heap/cage and semantically unable to omit live image bytes.

The Chrome DevTools heap exporter was the hidden multiplier: it retained an
object list, lookup map, node array, edge array, string table, and complete
`serde_json::Value` before the VM serialized any bytes. It now walks the
stable heap in multiple non-allocating passes and streams the flat node and
edge arrays directly to the caller. Its only size-dependent scratch is a
sorted compressed-offset index with a hard 64 MiB ceiling; admission failure
aborts before that index is allocated. The lightweight heap summary now folds
the collector's fixed 256-tag census instead of constructing the retained-size
graph. No option, permission, feature flag, or capability was added.

This matches V8's caller-owned chunked snapshot `OutputStream` and Node's
Readable heap-snapshot API; Node also warns that materializing a snapshot can
otherwise require roughly twice the heap:
[V8 `OutputStream`](https://v8.github.io/api/head/classv8_1_1OutputStream.html)
and [Node heap snapshots](https://nodejs.org/api/v8.html).

CPU profiling is now bounded across the complete installed-profiler lifetime,
including every batch transferred through `drain()`: at most 65,536 retained
samples and 64 MiB of logical sample storage, with a 1 MiB / 255-frame ceiling
for one sample. The shared stack walker exposes borrowed frame metadata to the
profiler, so oversized function/module strings are rejected before a fallible
owned copy is allocated; error stacks still collect the same complete owned
DTOs through that walker. Count, memory, and allocation admission failures are
reported as `dropped_samples`, and frames beyond the stack ceiling as
`truncated_frames`. CLI JSON, Chrome artifacts, and the normal status line
carry both counters. No new profiler option or capability was added.

This follows V8's maximum sample count, discarded-sample reporting, and
255-frame sampled-stack bound:
[V8 CPU profiler](https://v8.github.io/api/head/classv8_1_1CpuProfiler.html),
[profiling options](https://v8.github.io/api/head/classv8_1_1CpuProfilingOptions.html),
and [discarded samples](https://v8.github.io/api/head/classv8_1_1DiscardedSamplesDelegate.html).

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
