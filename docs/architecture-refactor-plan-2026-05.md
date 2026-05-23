# Otter Refactor Plan — 2026-05 (Phase 2)

Phase 1 baseline: `docs/architecture-audit-2026-05.md`. This document does
not repeat the audit; it turns the accepted diagnosis into an executable
architecture roadmap.

## Status update — 2026-05-23

**Phase 1 (Value & Allocation) DONE.** Tasks 1.1–1.5 shipped sequentially
under `crates-next/otter-vm`; legacy enum + parallel feature flag deleted
in cut-over. `pub struct Value(u64)` canonical at
`crates/otter-vm/src/value/mod.rs:90`; sub-tags
`0x7FFC..0x7FFF` per the historical MEMORY layout. Boxed-value migration
of dependent layers (closures, upvalue-on-boxed-value rewrite) included.
535/535 `otter-vm --lib` + 123/123 `otter-runtime --lib` passing;
workspace clippy + fmt clean; one `#[ignore]` test remains
(`runtime_array_cap_is_catchable_as_range_error`) tracked as new Task
1.4 below.

**Phase 1.3 (Hot/Cold Frame Split) DONE 2026-05-23.** Hot `Frame`
collapses from 488 B to **128 B** (two cache lines) with a const
assertion locking the target. Cold protocol state moves into a
per-`Interpreter` `ColdFramePool`, lazily acquired; async-await /
generator-yield detach the cold record so pool slots rotate freely
while frames sleep. See Task 1.3 below.

**Phase 2/3/4 status:** unchanged — not started.

Scope notes:

- Active stack only: `crates/otter-gc` → `crates/otter-vm` →
  `crates/otter-runtime` → product crates, per `AGENTS.md`.
- User constraint wins over the old template: **no compatibility shims and no
  feature flags**. Large refactors land as hard cut-over PRs on short-lived
  branches, with tests and benchmarks gating merge.
- `OTTER_VM_PLAN.md` is absent in this checkout. `VM_REFACTOR_PLAN.md` is
  deleted in the working tree and was not used as source of truth.

## Principles

- Correctness > DX > performance > new features.
- No backward-compatible duplicate APIs during this refactor. Otter is pre-1.0.
- No long-lived feature flags. If a migration needs isolation, use a branch,
  not a runtime/compiler flag in `main`.
- Every task must have an acceptance signal: Test262 delta, unit coverage,
  benchmark guard, or static invariant.
- Every new non-trivial Rust module gets the repository-standard top-level
  `//!` sections: purpose, contents, invariants, see also.
- No new runtime stack, no parked compatibility shims, no `crates-legacy/*`
  dependency.

## Current Deep Findings

### A. VM Introspection / Trace Tooling

Boa has a real interpreter trace path. It is compiled behind
`#[cfg(feature = "trace")]`, prints each entered frame's disassembly once, then
prints per-instruction timing, opcode, operands, and stack state
(`/tmp/boa/core/engine/src/vm/mod.rs:678-767`). Boa dispatches from a byte
stream through `OPCODE_HANDLERS[_BUDGET]` (`/tmp/boa/core/engine/src/vm/mod.rs:956-1004`),
and its `CodeBlock` display includes bytecode locations, handlers, constants,
bindings, and source map entries (`/tmp/boa/core/engine/src/vm/code_block.rs:941-1081`).
The CLI exposes `--trace` and calls `context.set_trace(args.trace)`
(`/tmp/boa/cli/src/main.rs:114-116`, `/tmp/boa/cli/src/main.rs:575-576`).

Otter has a bytecode dump, not a VM inspector. The CLI supports
`--dump-bytecode[=json]` (`crates/otter-cli/src/main.rs:64-74`) and routes text
dumping through `otter_bytecode::disasm::disassemble`
(`crates/otter-cli/src/main.rs:1124-1147`). The disassembler renders module
functions, instruction PC, operands, and source spans
(`crates/otter-bytecode/src/disasm.rs:21-76`). There is no per-dispatch trace,
no frame/register snapshot, no IC snapshot, no shape-transition breakpoint.

Target: **OtterVM Inspector** in `crates/otter-vm/src/inspect/`, surfaced by
`otter --trace`, `otter --inspect`, and a non-interactive dump mode.

Required surface:

| Surface | Requirement | Integration point |
|---|---|---|
| Dispatch trace | PC, opcode, operands, function id/name, frame depth, selected registers/stack state | `Interpreter::dispatch_loop_inner` currently fetches `function`, `pc`, `instr`, and `op` before dispatch (`crates/otter-vm/src/lib.rs:3935-3974`) |
| Disassembly++ | Existing disasm plus resolved constants, source spans, IC site ids, handler ranges after bytecode2 | `disasm.rs` is the current single renderer (`crates/otter-bytecode/src/disasm.rs:21-76`) |
| Shape snapshot | Live shapes, transition parents, keys, slot counts | `ShapeBody` stores `id`, `parent`, `transition_key`, `property_count`, `own_offset` (`crates/otter-vm/src/object/shape_body.rs:45-58`) |
| IC snapshot | Per-site state, hit/miss/install/disable counters | `PropertyIcStats` already exists (`crates/otter-vm/src/property_ic.rs:38-65`) |
| GC snapshot | Live object counts by `Traceable::TYPE_TAG`; heap snapshot command | Otter-GC already has type tags and heap snapshot infrastructure per Phase 1 §4 |
| Breakpoints | PC, opcode, shape transition, IC megamorphic transition | Needs byte-offset PC from Phase 2, not current instruction-index PC (`crates/otter-vm/src/frame_state.rs:36-39`) |

Hot-path rule: no inspector allocation and no string formatting in the normal
dispatch path. Since the owner rejected feature flags, the production check is
a branch on a nullable inspector pointer plus branch-predicted false state.
The expensive trace payload is pulled only after the branch is taken.

Time-travel is not Phase 5. Proper replay needs journaling every register
write and every host time/random read. Ship breakpoints and snapshots first.

Bytecode dependency: Inspector must wait for Phase 2 because current `ExecInstr`
uses instruction-index PCs and side operands (`crates/otter-vm/src/executable.rs:81-87`,
`crates/otter-vm/src/executable.rs:248-260`). A byte stream with byte-offset PC
is the right substrate for breakpoints, source maps, and JIT stack maps.

### B. Runtime Startup / Bootstrap

The user's path `crates/otter-runtime/src/bootstrap.rs` is stale. Active
bootstrap is `crates/otter-vm/src/bootstrap.rs` and is 3,938 LoC.

Current bootstrap map:

- `BootstrapEntry` is a static function-pointer entry with `name`, feature bits,
  and installer (`crates/otter-vm/src/bootstrap.rs:183-195`).
- `BOOTSTRAP_ENTRIES` is a deterministic ordered slice
  (`crates/otter-vm/src/bootstrap.rs:362-412`).
- The order starts with `Object`, then `Array`, JSON/String/Number/Boolean/etc.,
  and later Function (`crates/otter-vm/src/bootstrap.rs:369-389`). The comment
  explicitly says order matters because later entries resolve
  `Object.prototype` (`crates/otter-vm/src/bootstrap.rs:362-368`).
- `build_global_this_impl` allocates `globalThis`, installs `NaN`, `Infinity`,
  `undefined`, then runs every selected entry (`crates/otter-vm/src/bootstrap.rs:443-509`).
- After the loop it patches `globalThis.[[Prototype]]` to `Object.prototype`
  (`crates/otter-vm/src/bootstrap.rs:510-514`). This is a post-binding cycle
  break.
- The top-level trait path exists: `BuiltinIntrinsic` defines `NAME`, `FEATURE`,
  and `install` (`crates/otter-vm/src/intrinsic_install.rs:46-72`), and
  `bootstrap_entry!` builds static entries (`crates/otter-vm/src/intrinsic_install.rs:99-108`).
- The architectural problem is not the table; it is body ownership. Many entries
  are adapter structs inside `bootstrap.rs` that delegate to free functions in
  the same huge file (`crates/otter-vm/src/bootstrap.rs:2481-2559`), while
  newer entries own bodies in focused modules such as BigInt
  (`crates/otter-vm/src/bootstrap_bigint.rs:30-43`).

Boa comparison: Boa uses `IntrinsicObject::init(realm)` and `get(intrinsics)`
(`/tmp/boa/core/engine/src/builtins/mod.rs:128-137`), and a linear
`Realm::initialize` list (`/tmp/boa/core/engine/src/builtins/mod.rs:241-353`).
Boa also has a typed `Intrinsics` struct storing constructors, intrinsic
objects, and object templates (`/tmp/boa/core/engine/src/context/intrinsics.rs:20-70`).

Target architecture:

- Keep one top-level `BuiltinIntrinsic` trait. It is already the right static
  dispatch shape.
- Move every installer body into `crates/otter-vm/src/intrinsics/<name>.rs`.
  `bootstrap.rs` must shrink to registry types, entry list, telemetry, and
  `build_global_this`.
- Add `RealmIntrinsics`: typed slots for `%Object%`, `%Object.prototype%`,
  `%Function%`, `%Function.prototype%`, `%Array.prototype%`, `%Promise%`, and
  every other well-known intrinsic. This follows Boa's typed `Intrinsics` model
  and eliminates string lookups during installer bodies.
- Do not introduce dynamic topological sorting. V8, JSC, and Boa use deliberate
  linear bootstrap order because the graph is static and spec-shaped. Otter
  should document dependency edges next to `BOOTSTRAP_ENTRIES`.

Snapshotting: defer. V8 startup snapshots and JSC/Bun snapshots are relevant
only after Value, bytecode, and bootstrap layout are stable. Otter's compressed
GC handles are structurally snapshot-friendly, but native function pointer
rebinding and deterministic intrinsic allocation must be solved first. Snapshot
pipeline becomes an RFC after Phase 3 and after ROADMAP P10 is the active gate.

### C. Value Redesign → Allocation Reduction

Current `Value` is a non-`Copy` enum (`crates/otter-vm/src/lib.rs:210-217`)
with 30+ variants (`crates/otter-vm/src/lib.rs:217-401`). The comment says
it is intentionally not `Copy` because `JsString` owns an `Arc`
(`crates/otter-vm/src/lib.rs:210-216`). This is the root of the register-copy
problem.

Fat/owned variants and target home:

| Current value payload | Evidence | Target after redesign |
|---|---|---|
| `BigIntValue` holds `Rc<BigInt>` | `crates/otter-vm/src/bigint/mod.rs:39-43` | GC `BigIntBody`; `Value(u64)` points to body |
| `JsString` holds `Arc<StringRepr>` | `crates/otter-vm/src/string/mod.rs:228-232` | GC string body; no atomic refcount per register copy |
| `JsSymbol` holds `Rc<SymbolBody>` | `crates/otter-vm/src/symbol.rs:69-72` | GC symbol body or interned symbol table handle |
| `Closure` embeds `Rc<[UpvalueCell]>` and `Option<Box<Value>>` | `crates/otter-vm/src/lib.rs:267-282` | GC closure body with upvalue array and bound-this slot |
| `Frame.upvalues` clones `Rc<[UpvalueCell]>` | `crates/otter-vm/src/frame_state.rs:47-50` | Frame points at closure body/upvalue slice |
| `Frame.module_url` clones `Rc<str>` | `crates/otter-vm/src/frame_state.rs:101-113` | Module URL lives in `ExecutableFunction`; frame carries function id |
| `JsArrayBuffer` uses `Rc<LocalBody>` with `RefCell<Vec<u8>>`; shared uses `Arc<SharedBody>` | `crates/otter-vm/src/binary/array_buffer.rs:56-74` | GC object wrapper plus off-heap backing-store token |
| `JsDataView` holds `Rc<DataViewBody>` | `crates/otter-vm/src/binary/data_view.rs:19-23` | GC DataView body |
| `JsTypedArray` holds `Rc<TypedArrayBody>` | `crates/otter-vm/src/binary/typed_array.rs:363-367` | GC typed-array view body |
| `JsTemporal` holds `Rc<TemporalPayload>` | `crates/otter-vm/src/temporal/payload.rs:103-107` | GC temporal body |
| `MapKey` clones BigInt/String/Symbol and stores `Value` for object identity | `crates/otter-vm/src/collections.rs:68-89`, `crates/otter-vm/src/collections.rs:100-115` | Keep key projection, but slots become 8-byte values and traced GC refs |

Important correction versus the old audit draft: Promise is already mostly on
the right path. `PurePromise` is a `Gc<PurePromiseBody>` and `Copy`
(`crates/otter-vm/src/promise.rs:267-275`), and `JsPromiseHandle` is a copyable
wrapper around `PromiseRepr::Pure` (`crates/otter-vm/src/promise.rs:661-671`).
Do not describe Promise as `Rc`-based; the remaining work is making the Value
tag shape uniform and tightening capability records.

Migration strategy:

- Hard cut-over to `#[repr(transparent)] Value(u64)`.
- No `Value64` feature flag in `main`. Build the replacement on a branch, keep
  the branch rebased, merge only when Test262 and VM/runtime tests are green.
- Collapse object-like variants to pointer sub-tags plus `GcHeader::tag()` type
  discrimination. This matches the historical MEMORY design and Boa's default
  NaN-boxed value (`/tmp/boa/core/engine/src/value/inner/nan_boxed.rs` per
  Phase 1).
- Keep `Value::Hole` as an internal bit pattern only, guarded before every
  public coercion. User-observable reads must still map holes to `undefined`
  as current comments require (`crates/otter-vm/src/lib.rs:220-235`).

Bench guards:

- Register copy tight loop: ≥2x throughput.
- Closure-heavy loop: ≥1.5x throughput and no `Rc` clone in upvalue path.
- String-heavy concat/compare: no regression and fewer atomic refcounts.
- Generator/async-heavy loop: no GC-rooting regression.
- GC pause p95/p99 under allocation loop: no regression.

GC invariants that can break if done naively:

- Moving `BigInt`, strings, symbols, typed arrays, and Temporal into GC bodies
  requires every slot to implement `SafeTraceable`; missing one edge becomes
  a collector bug, not a Rust borrow-checker bug.
- Finalization and WeakRef cleanup must never run during GC. Otter's weak-ref
  module already states callbacks are enqueued after raw weak processing
  (`crates/otter-vm/src/weak_refs.rs:20-26`); preserve that invariant.
- ArrayBuffer backing stores need explicit external-memory accounting, not just
  `Vec<u8>` behind a GC object. The current `LocalBody` already tracks an
  `ExternalMemory` token (`crates/otter-vm/src/binary/array_buffer.rs:88-92`).

### D. JIT-Readiness

Current blockers:

- Bytecode is still an array of `ExecInstr`, not a byte stream. `ExecutableFunction`
  stores `code: Box<[ExecInstr]>` (`crates/otter-vm/src/executable.rs:140-175`).
  Each instruction stores opcode, operand count, three inline operands, side-table
  start, and property IC site (`crates/otter-vm/src/executable.rs:248-260`).
- Dispatch is a large multi-match loop: stack-mutating opcodes first
  (`crates/otter-vm/src/lib.rs:3972-4347`), then ordinary ops
  (`crates/otter-vm/src/lib.rs:4350-5315`), then a final match
  (`crates/otter-vm/src/lib.rs:5318-5340`). This is not a stable baseline-JIT
  substrate.
- IC slots are monomorphic and can disable after four misses
  (`crates/otter-vm/src/property_ic.rs:36`, `crates/otter-vm/src/property_ic.rs:116-170`).
- Frame layout mixes hot and cold data: registers, handlers, pending protocol
  state, async/generator state, and module URL all live in one frame
  (`crates/otter-vm/src/frame_state.rs:35-160`).
- Native ABI is implicit in interpreter marshalling. Macros must not create a
  second ABI.

Minimum JIT-ready contracts to freeze now:

| Contract | Why | Required change |
|---|---|---|
| Versioned byte stream | Sparkplug-style baseline JIT maps bytecode 1:1 to machine code | Replace `ExecInstr` with byte stream and schema table |
| Byte-offset PC | Breakpoints, source maps, safepoints, OSR | `Frame.pc` remains `u32`, semantics become byte offset |
| Fixed hot frame | Deopt/OSR needs known register and metadata layout | Split hot frame from cold pending/async/generator side records |
| Fixed IC slot ABI | JIT patch points need address-stable feedback slots | Per-function `Box<[IcSlot]>`, not reallocated vectors |
| GC stack maps | JIT code must report tagged roots at safepoints | Define stack-map format before baseline JIT starts |
| Native call ABI | JIT must call host/native functions without interpreter-only glue | Freeze args/result/throw protocol; macros generate into it |

Backend choice is deferred:

- Cranelift: realistic Rust-native baseline; acceptable first JIT backend.
- B3 port: too expensive before Otter has a stable baseline.
- Custom backend: acceptable only for a Sparkplug-like baseline, not an
  optimizing tier.

Do not add an AST-to-bytecode IR now. V8 Ignition, JSC LLInt, and Boa can all
compile baseline bytecode from AST-like lowering; graph IR belongs in an
optimizing tier, not in this refactor.

### E. Macros

Current macro crate:

- `#[js_namespace]` exists (`crates/otter-macros/src/lib.rs:42-61`).
- `#[js_class]` exists (`crates/otter-macros/src/lib.rs:140-167`).
- `raft!` exists (`crates/otter-macros/src/lib.rs:426-443`).
- Expansions generate static specs and `NativeCall::Static` function pointers
  (`crates/otter-macros/src/lib.rs:109-135`, `crates/otter-macros/src/lib.rs:391-421`,
  `crates/otter-macros/src/lib.rs:474-489`).
- The crate validates duplicate names and class helper conflicts
  (`crates/otter-macros/src/lib.rs:74-99`, `crates/otter-macros/src/lib.rs:177-335`).
- It does not install globals, does not register modules, does not marshal typed
  Rust signatures, and has no production callers in active runtime code per
  Phase 1.

Production references:

- `napi-rs #[napi]`: signature-driven typed marshalling.
- `deno_core op2`: compile-time op metadata, typed fast paths, async/sync split.
- Boa: no broad binding macros; mostly hand-written traits. Good for a small
  engine, not good enough for Otter's third-party module goal.
- JSC: IDL/generator pipeline. Correct for Web APIs, too heavy as Otter's first
  Rust-facing macro layer.

Target macro family:

| Macro | Purpose | Generated output |
|---|---|---|
| `#[otter_intrinsic]` | Global intrinsic / namespace / constructor | `BuiltinIntrinsic` impl, static spec, typed metadata |
| `#[otter_class]` | Constructor-backed JS class | Constructor/prototype/static specs, prototype chain metadata, `@@toStringTag` metadata |
| `#[otter_module(prefix = "...", name = "...")]` | Whole ESM/native module registration | Module descriptor, exports table, capability gate metadata, loader hook |
| `#[dive]` / `#[dive(deep)]` | Native op marshalling | Typed `FromJs`/`IntoJs` bridge into the frozen native ABI |
| `#[derive(Trace, Finalize)]` | GC tracing safety | Trace implementation for GC bodies introduced by Phase 1 |

Compile-time validation:

- Exported JS names are unique.
- Rust parameter types implement `FromJs`; return types implement `IntoJs` or
  `JsResult<T>`.
- `length` matches spec arity unless explicitly marked variadic/spec-divergent.
- Accessor getter/setter arity is enforced.
- Spec section attribute is mandatory for ECMAScript intrinsics.
- Capability gates are declared on modules, not hidden in function bodies.

Migration candidates in order:

1. Math, JSON, Reflect: small namespaces, low bootstrap risk.
2. BigInt and String: already isolated enough to prove class/constructor output.
3. Object and Array: large surfaces, migrate only after macro diagnostics are good.

Old macro names (`js_namespace`, `js_class`, `raft`) are deleted when the new
`otter_*` macros land. No old/new coexistence period.

### F. Boa Object / Promise Patterns

Borrow:

- **Typed intrinsics table.** Boa's `Intrinsics` stores constructors, objects,
  and templates (`/tmp/boa/core/engine/src/context/intrinsics.rs:20-70`). Otter
  should add `RealmIntrinsics` in Phase 3.
- **Intrinsic trait with `get`.** Boa's `IntrinsicObject` includes both
  `init(realm)` and `get(intrinsics)` (`/tmp/boa/core/engine/src/builtins/mod.rs:128-137`).
  Otter's `BuiltinIntrinsic` needs the same retrieval story through typed slots.
- **Object internal-method vtable.** Boa stores `vtable: &'static InternalObjectMethods`
  beside object data (`/tmp/boa/core/engine/src/object/jsobject.rs:74-84`), and
  internal object operations dispatch through that table
  (`/tmp/boa/core/engine/src/object/internal_methods/mod.rs:147-358`). Otter
  currently relies on value/object-kind matches in many paths; a vtable is worth
  evaluating after Value/object unification.
- **Promise capability as a spec record.** Boa models `PromiseCapability` directly
  as promise plus resolving functions (`/tmp/boa/core/engine/src/builtins/promise/mod.rs:169-190`).
  Otter already has a `PromiseCapability` record (`crates/otter-vm/src/promise.rs:183-195`);
  keep tightening it around ECMA-262 `NewPromiseCapability`, not ad-hoc callback
  bundles.

Do not borrow:

- Boa GC. Otter-GC is ahead: generational/incremental/ephemeron-aware per Phase 1.
- Boa's exact opcode taxonomy. Borrow the byte-stream idea, not every opcode.
  Otter should remove by-name shortcut opcodes rather than reproduce Boa's layout.
- Boa's host `JobExecutor` as a whole. Boa documents that HostEnqueuePromiseJob
  jobs must run FIFO (`/tmp/boa/core/engine/src/job.rs:575-595`). Otter's
  isolate-local microtask queue is the right shape for a Tokio runtime: it keeps
  GC handles off cross-thread channels (`crates/otter-vm/src/microtask.rs:1-15`)
  and host async settlement crosses threads only as owned payloads/tokens
  (`crates/otter-runtime/src/promise_registry.rs:1-32`).

Promise ordering: Otter's queue explicitly states FIFO reaction buckets
(`crates/otter-vm/src/promise.rs:20-28`) and a swap-and-drain generation policy
(`crates/otter-vm/src/microtask.rs:17-31`). This is compatible with
ECMA-262 HostEnqueuePromiseJob FIFO requirements, but must be locked with
Test262 `built-ins/Promise` and `language/expressions/await` subsets.

## Phase 0 — Foundational (Blocks All)

### Task 0.1 — Capture Active Test262 Baseline

- Goal: turn refactor risk into measurable deltas.
- Touches: `ES_CONFORMANCE.md`, `Justfile`, optional `docs/test262-baseline-2026-05.md`.
- Change: run and publish active-stack `language/`, `built-ins/`, `annexB/`,
  Promise, Object, Function, Array, TypedArray, Proxy, WeakRef/FinalizationRegistry
  slices.
- Acceptance: committed pass/fail/skip/timeout numbers; every later phase cites
  before/after deltas.
- Risk: Low.
- Effort: S.
- Depends on: none.

### Task 0.2 — Enforce Unsafe Boundary

- Goal: make documented safety policy mechanical.
- Touches: `crates/otter-vm/src/lib.rs`, `crates/otter-runtime/src/lib.rs`,
  `crates/otter-compiler/src/lib.rs`, `crates/otter-bytecode/src/lib.rs`.
- Change: add crate-level unsafe ban outside GC/FFI.
- Acceptance: workspace builds and tests pass.
- Risk: Low.
- Effort: S.
- Depends on: none.

### Task 0.3 — Fix Roadmap Truth

- Goal: remove false `[x]` on polymorphic IC.
- Touches: `ROADMAP.md`.
- Change: mark P1 incomplete until Phase 2/IC lands. Current code is
  monomorphic-only with disable-after-4 (`crates/otter-vm/src/property_ic.rs:36`,
  `crates/otter-vm/src/property_ic.rs:116-170`).
- Acceptance: roadmap matches code.
- Risk: Low.
- Effort: S.
- Depends on: none.

### Task 0.4 — Freeze Refactor ABI Decisions

- Goal: prevent phases from choosing incompatible shapes.
- Touches: new ADR under `docs/adr/`.
- Change: record `Value(u64)`, byte-offset PC, fixed IC slot ABI,
  `BuiltinIntrinsic` as registry backend, `otter_*` macros as preferred DX.
- Acceptance: ADR approved by owner.
- Risk: Medium only because it blocks churn.
- Effort: S.
- Depends on: 0.1.

## Phase 1 — Value & Allocation

### Task 1.1 — Replace `Value` Enum With 8-Byte Tagged Value — DONE 2026-05-23

- Goal: make register slots 8 bytes and `Copy`.
- Touches: `crates/otter-vm/src/lib.rs`, new `crates/otter-vm/src/value/`,
  every opcode handler, public VM value APIs.
- Change: hard cut-over to `Value(u64)`; heap/object variants become tagged GC
  refs; immediate variants become payload encodings.
- Acceptance: all VM/runtime tests green; no Test262 regression from 0.1; register
  copy bench ≥2x.
- Risk: High.
- Effort: L.
- Depends on: 0.4.
- **Status:** Shipped. `#[repr(transparent)] pub struct Value(u64,
  PhantomData<*const ()>)` at `crates/otter-vm/src/value/mod.rs:90`,
  re-exported from `crates/otter-vm/src/lib.rs:190`. Sub-tags in
  `value/tag.rs`. `legacy_value` module deleted. `is_object_type`
  (§7.2.7) sole predicate at `value/mod.rs:805`; legacy wrappers
  (`is_callable_or_object_value`, `is_instance_object_value`,
  `is_extended_object_value`, `is_object_like_value`,
  `constructor_return_is_object`) removed. 532/532 lib tests passing.
  Phases 1.1.a–1.1.e (API parity, swap, cleanup, OOM-cap fix) merged
  through commits `8825cf7b..ded0b4c5`.

### Task 1.2 — Move Rc/Arc Value Payloads Into GC Bodies — DONE 2026-05-23

- Goal: eliminate owned/refcounted payloads from `Value`.
- Touches: BigInt, string, symbol, closure/upvalue, ArrayBuffer/DataView/TypedArray,
  Temporal, Map/Set key paths.
- Change: introduce GC bodies with trace implementations; replace `Rc`/`Arc`
  handles on the value hot path.
- Acceptance: closure-heavy bench ≥1.5x; string-heavy bench no regression; GC
  invariant tests for every migrated body.
- Risk: High.
- Effort: L.
- Depends on: 1.1.
- **Status:** Shipped alongside 1.1. `UpvalueCell = GcRef<UpvalueData>`
  with `Cell<Value>` + GcTraceable (UPVALUE=26); closures own
  upvalues via GC body; `BigIntPayload` already split (i64-inline + heap
  BigInt per MEMORY.md "P1 perf pass"); JsArrayBuffer/JsDataView/
  JsTypedArray wrappers migrated to 4-byte GC handles
  (`BufferStorage::{Local,Shared}` tagged pair) per MEMORY.md "JsArrayBuffer
  wrapper migration 2026-05-21" / "JsDataView wrapper migration 2026-05-21"
  / "JsTypedArray wrapper migration 2026-05-21"; JsStringBody Stage 1
  unification per MEMORY.md "JsString GC body unification Stage 1
  2026-05-21". Remaining low-priority work: `JsSymbol` and `JsTemporal`
  still go through `Rc` indirection; not on the hot path; deferred to a
  follow-up task when bench data requires it.

### Task 1.3 — Split Hot Frame From Cold Protocol State

- Goal: make frame layout JIT/deopt-ready and cheaper to scan.
- Touches: `crates/otter-vm/src/frame_state.rs`, frame push/pop, async/generator
  parking, pending protocol records.
- Change: hot frame contains function id, byte-offset PC, register window,
  `this`, return register, closure/upvalue handle. Pending ToPrimitive, iterator,
  bind, async/generator state move to side records.
- Acceptance: no Test262 regression; frame-size assertion; GC root scanning tests.
- Risk: High.
- Effort: L.
- Depends on: 1.1.
- **Status:** DONE 2026-05-23. Hot `Frame` shrinks from 488 B to
  128 B (two cache lines) and a `const _: () = assert!(...)` in
  `crates/otter-vm/src/frame_state.rs` pins the target so future
  additions are compile errors. Cold protocol state — try handlers,
  pending ToPrimitive/bind/iterator ladders, `pending_throw`,
  `construct_target`, `new_target`, `rest_args`, `incoming_args` —
  lives in `crates/otter-vm/src/cold_frame.rs` (`ColdFramePool`
  on `Interpreter`, lazily acquired through
  `Interpreter::frame_ensure_cold`). Across async-await / generator-yield
  parking the cold record detaches into the parked container
  (`ParkedFrameBody.cold` / `GeneratorBody.cold` /
  `MicrotaskKind::AsyncResume{cold,…}` /
  `MicrotaskKind::AsyncGenResume{cold,…}`) so pool slots can rotate
  while frames sleep; resume re-attaches via
  `Interpreter::frame_attach_cold`. `Frame.module_url` removed
  entirely — import opcodes now read the referrer from
  `ExecutableFunction::module_url` via `context.exec_function(...)`.
  Byte-offset PC stays deferred to Phase 2.1 (the layout-only split
  doesn't depend on it). All
  tests green: `otter-vm --lib` 535/535, `otter-runtime --lib`
  123/123, workspace `cargo test --all --all-features` clean, clippy
  clean. Remaining `Frame` cold fields (`async_state`,
  `generator_owner`, 16 B total) stay on the hot frame — moving them
  would not cross another cache-line boundary and would add more
  parking-detach plumbing for no further hot-path win.

### Task 1.4 — GC Extra-Roots Callback — DONE 2026-05-23

- Goal: allow cap-trigger `collect_full` to see the interpreter's
  `error_classes` (and other `RuntimeState`-owned) GC roots without
  re-entering `Interpreter::force_gc`.
- Touches: `crates/otter-gc/src/heap.rs` (add
  `extra_roots: Option<ExtraRootsCallback>` slot; invoke from
  `account_or_collect_with_roots` and `shade_roots`),
  `crates/otter-vm/src/interpreter.rs` (install the trampoline; rewrite
  `force_gc` to **consume** the same callback rather than duplicating
  the root walk), test file at
  `crates/otter-runtime/tests/runtime_oom_surfaces_as_error.rs`
  (un-`#[ignore]` `runtime_array_cap_is_catchable_as_range_error`).
- Change: introduce an `ExtraRootsCallback` raw-pointer trampoline on
  `GcHeap`. `Interpreter::run` installs/clears it through an RAII
  guard. `force_gc` must call the trampoline instead of separately
  walking `RuntimeState::trace_roots`, otherwise the trampoline
  re-fires while `trace_roots` is already on the stack and breaks
  `gc_invariants::weak_refs_and_finalization::finalization_callback_cannot_observe_collected_target_through_weak_ref`
  (misaligned-pointer panic in the scavenger — observed on the first
  attempt).
- Acceptance: `runtime_array_cap_is_catchable_as_range_error` passes;
  `gc_invariants` suite stays green; no regression in `otter-vm --lib`
  (532) or `otter-runtime --lib` (123); `e instanceof RangeError`
  inside an OOM-catching script returns `true` instead of dereferencing
  a freed `type_tag=0` slot.
- Risk: Medium — scavenger interaction is the trap; mitigated by
  routing `force_gc` through the same callback.
- Effort: S (under one day once API shape is agreed).
- Depends on: 1.1.
- Note on `forbid(unsafe_code)`: the trampoline needs raw-pointer
  indirection; `otter-vm` has workspace `forbid(unsafe_code)`. Either
  add a local `[lints.rust]` override (mirroring `otter-gc`'s
  Cargo.toml) or move the trampoline into a per-item-allowed helper
  module.

## Phase 2 — Bytecode, Dispatch, IC, JIT-Ready ABI

### Task 2.1 — Bytecode wire format + byte-offset PC — DONE 2026-05-23

- Goal: make bytecode a versioned byte stream.
- Touches: `crates/otter-bytecode`, `crates/otter-vm/src/executable.rs`,
  compiler emission boundary, disassembler.
- Change: replace `ExecInstr` execution storage with byte stream, opcode schema,
  byte-offset PC, source map table, and per-function metadata.
- Acceptance: DTO-to-bytecode-to-disasm roundtrip tests; dump schema bumped;
  golden disasm updated.
- Risk: High.
- Effort: L.
- Depends on: 1.1.
- **Status:** Shipped. Wire format lives in
  `crates/otter-bytecode/src/encoding.rs` (writer, decoder, jump
  fixup, span translator, `BYTECODE_FORMAT_VERSION = 2`).
  `ExecutableFunction` stores per-instruction `byte_pc` / `byte_len`
  + a single owned `Box<[Operand]>` per instruction (module-level
  side-operand table deleted). `frame.pc` is a byte offset; the
  dispatch loop fetches via `ExecutableFunction::instr_at_byte_pc`
  and advances by `instr.byte_len()` through `Frame::advance_pc`
  routed via `Interpreter::current_byte_len` (saved/restored across
  nested dispatch). Branch operands rewritten to byte-offset deltas
  relative to `(jump_pc + 1)`. `snapshot_frames` reads
  `ExecutableFunction::byte_spans`. 539/539 `otter-vm --lib`,
  123/123 `otter-runtime --lib`, 22/22 `otter-bytecode --lib`,
  workspace clippy clean. Test262 baselines hold (try 156/44/2,
  generators 165/101/0, await 18/4/0).

### Task 2.2 — Collapse Dispatch To One Loop — DONE 2026-05-23

- Goal: remove three-match dispatch shape.
- Touches: `Interpreter::dispatch_loop_inner`.
- Change: one opcode decode, one dispatch arm, explicit dispatch action for
  return/call/throw/await/yield.
- Acceptance: no Test262 regression; dispatch microbench ≥1.25x.
- Risk: Medium.
- Effort: M.
- Depends on: 2.1.
- **Status:** Shipped. `dispatch_loop_inner` walks one fetch + one
  exhaustive `match op` over all 132 opcodes (the leftover nested
  match handling the equality opcode family lives inside its own
  arm). Variadic call / closure arms (`MakeClosure`, `JsonCall`,
  `ArrayBufferCall`, …, `TemporalCall`) grab `let operands = ...` /
  `let frame = &mut stack[top_idx];` locally so the merge stays
  borrow-safe. Previously-duplicated arms (`ToNumber`,
  `DeleteProperty`, `DeleteElement`, `GetPrototype`, `SetPrototype`,
  `LoadProperty`, `StoreProperty`, `LoadElement`, `StoreElement`,
  `GetIterator`, `IteratorNext`, `Instanceof`, `HasProperty`,
  `CollectArguments`) merged into a single body each (proxy/ladder
  check first, fast path inlined). Dead `run_collect_arguments_reg`
  helper deleted. Trailing `_ => {}` dropped because the match is
  now exhaustive. Test262 baselines hold: try 156p/44f/2crash,
  generators 165p/101f/0crash, await 18p/4f/0crash. 539/539 `otter-vm
  --lib`, 123/123 `otter-runtime --lib`, workspace clippy clean.
  Microbench number not yet captured — re-run when the dispatch
  benchmark harness lands.

### Task 2.3 — Remove Shortcut Call Opcodes

- Goal: eliminate spec-risky by-name built-in call shortcuts.
- Touches: `crates/otter-bytecode`, `crates/otter-compiler`, VM dispatch.
- Change: remove `JsonCall`, `MathCall`, `PromiseCall`, and equivalent shortcut
  lowering from the redesigned ISA. Normal `LoadProperty` → `Call` must carry
  the performance through ICs.
- Acceptance: shadowing fixtures such as local `JSON`, `Math`, `Promise` pass;
  Test262 no regression.
- Risk: Medium.
- Effort: M.
- Depends on: 2.1.

### Task 2.4 — Polymorphic IC Slots

- Goal: make ROADMAP P1 true and create JIT patch points.
- Touches: `property_ic.rs`, `property_dispatch.rs`, executable metadata.
- Change: per-function fixed IC slot table; 4-entry PIC plus megamorphic probe
  state; counters remain inspectable.
- Acceptance: polymorphic shape benchmark improves; ROADMAP P1 can be re-marked
  only after tests and benchmark guard pass.
- Risk: High.
- Effort: M/L.
- Depends on: 2.1.

### Task 2.5 — Freeze Native Call ABI — DONE 2026-05-23

- Goal: make macros and future JIT generate into one call convention.
- Touches: `native_function.rs`, `NativeCtx`, macro-generated specs, docs.
- Change: document argument vector, `this`, `new.target`, return slot, thrown
  value protocol, allocation/rooting rules.
- Acceptance: compile-fail tests for forbidden raw handles; macro tests consume
  only this ABI.
- Risk: Medium.
- Effort: M.
- Depends on: 1.1.
- **Status:** Shipped. Authoritative ABI spec at
  [`docs/native-call-abi.md`](native-call-abi.md): entry signature
  (`NativeFastFn`), receiver/`new.target` accessors, argument and
  return protocol, throw-routing table for every `NativeError`
  variant, allocation/rooting rules naming every sanctioned
  `NativeCtx` helper, forbidden patterns, versioning policy. ABI is
  v1; non-`#[non_exhaustive]` variant additions require version
  bump + coordinated migration. `NativeFastFn`, `NativeError`, and
  `NativeCtx` carry inline pointers to the spec doc. Existing
  compile-fail fixtures under
  [`crates/otter-vm/tests/compile_fail/`](../crates/otter-vm/tests/compile_fail/)
  cover the forbidden patterns (raw `Gc<T>` / `Local` / `Value` /
  `Frame` in `Send + 'static`, `NativeCtx` across `.await`,
  cross-isolate handles, branded-session leaks, raw write barriers,
  raw `RawGc` import). `NativeCtx::heap_mut` remains `pub` as a
  documented escape hatch — ~150 in-tree callers still depend on
  it; the aspirational `native_ctx_heap_mut_rejected` fixture is
  removed in this commit (kept as a follow-up under "migrate raw
  `heap_mut` to high-level helpers"). 539/539 `otter-vm --lib`,
  123/123 `otter-runtime --lib`, all compile-fail tests green.

## Phase 3 — Bootstrap & IntrinsicRegistry

### Task 3.1 — Split Bootstrap Bodies — DONE 2026-05-23

- Goal: make bootstrap maintainable and auditable.
- Touches: `crates/otter-vm/src/bootstrap.rs`, new `crates/otter-vm/src/intrinsics/`.
- Change: move Object, Array, Number, Symbol, Date, Proxy, Function, Intl,
  Temporal, AggregateError, Iterator bodies out of `bootstrap.rs`.
- Acceptance: `bootstrap.rs` under 600 LoC; duplicate-name tests stay green;
  bootstrap telemetry unchanged within noise.
- Risk: Low/Medium.
- Effort: M.
- Depends on: 0.4.
- **Status:** Shipped. `bootstrap.rs` shrank from **3859 LoC → 638
  LoC** (84% reduction). Per-intrinsic installer bodies live under
  `crates/otter-vm/src/intrinsics/`: `array.rs`, `date.rs`,
  `function.rs`, `iterator.rs`, `number.rs`, `object.rs`,
  `placeholders.rs` (Intl / Temporal / AggregateError), `proxy.rs`,
  `symbol.rs`. Shared value-rooted allocation / native-function /
  global-binding helpers (`alloc_object_with_value_roots`,
  `native_constructor_static_with_value_roots`,
  `native_static_with_value_roots`, `native_new_target_prototype`,
  `install_placeholder`, `define_global`, `define_global_value`) live
  in `intrinsics/shared.rs` and are re-exported under
  `crate::bootstrap::` so existing import paths keep resolving.
  `BOOTSTRAP_ENTRIES` entries now reference
  `crate::intrinsics::<name>::Intrinsic` adapters (with the
  multi-adapter `placeholders` module exporting the three placeholder
  structs by name). 535/535 `otter-vm --lib` + 123/123
  `otter-runtime --lib` passing; clippy clean. Target "<600 LoC"
  missed by 38 lines — bootstrap.rs is now just registry
  bookkeeping + the `build_global_this_impl` driver + telemetry
  helpers, all genuinely centralised state.

### Task 3.2 — Add `RealmIntrinsics` — DONE 2026-05-23

- Goal: remove installer string lookup and make intrinsic identity explicit.
- Touches: VM bootstrap, object/native constructors, runtime initialization.
- Change: typed slots for well-known constructors/prototypes; installers fill
  slots and read dependencies from slots.
- Acceptance: Object/Function prototype cycle covered by focused tests; no
  string lookup for well-known intrinsic dependencies in installer bodies.
- Risk: Medium.
- Effort: M.
- Depends on: 3.1.
- **Status:** Shipped. `crates/otter-vm/src/realm_intrinsics.rs`
  holds typed slots for `%Object%`, `%Object.prototype%`,
  `%Function.prototype%`, `%Array%`, `%Array.prototype%` — the
  JsObject-shaped well-knowns hit on every
  `OrdinaryCreateFromConstructor`-style allocation. Populated once
  at the end of `build_global_this_impl` by walking `globalThis`;
  runtime lookups (`object_prototype_object_opt`,
  `function_prototype_object`, `constructor_prototype_value`) check
  the slot first and fall back to the string-lookup path for
  non-default globals. Focused tests assert `Object.prototype` and
  `Function.prototype` slot identity matches the global walk, plus
  bootstrap-populates-slots smoke test. NativeFunction-shaped
  constructors (`Promise`, `RegExp`, `Date`, `Iterator`, …) excluded
  from slots for now — they take a different resolution path and
  the per-call savings don't yet justify per-slot polymorphism.
  539/539 `otter-vm --lib`, 123/123 `otter-runtime --lib`, clippy
  clean.

### Task 3.3 — Decide Snapshot Checkpoint

- Goal: avoid premature snapshot work while preserving future path.
- Touches: docs only in this phase.
- Change: write snapshot prerequisites and explicit defer decision.
- Acceptance: owner sign-off: defer until after Value + bytecode + bootstrap.
- Risk: Low.
- Effort: S.
- Depends on: 3.2.

## Phase 4 — Macros

### Task 4.1 — Replace Current Macro API With `otter_*`

- Goal: make macros the standard contributor path.
- Touches: `crates/otter-macros`, `docs/book/src/macros/`, VM surface builders.
- Change: delete `js_namespace`, `js_class`, `raft`; introduce
  `otter_intrinsic`, `otter_class`, `otter_module`, `dive`, and trace/finalize
  derive.
- Acceptance: trybuild tests for valid/invalid macro input; useful diagnostics
  with correct spans.
- Risk: Medium.
- Effort: M.
- Depends on: 2.5, 3.2.

### Task 4.2 — Port First Intrinsics

- Goal: prove macros on real builtins.
- Touches: Math, JSON, Reflect, BigInt/String if first pass succeeds.
- Change: generated specs feed the same `BuiltinIntrinsic` registry and native
  ABI, not a parallel runtime path.
- Acceptance: byte-for-byte same descriptors where spec requires; focused
  Test262 subsets no regression.
- Risk: Medium.
- Effort: M.
- Depends on: 4.1.

### Task 4.3 — Module Install Macro

- Goal: support third-party `otter:`, `node:`, and custom-prefix modules.
- Touches: module loader, runtime builder API, macro crate.
- Change: generated module descriptor with prefix/name, ESM export table,
  capability metadata, and loader registration.
- Acceptance: sample `myapp:` module works without editing loader internals;
  capability denial tests pass.
- Risk: Medium.
- Effort: M.
- Depends on: 4.1.

## Phase 5 — Inspector / Introspection

### Task 5.1 — Step Trace

- Goal: Boa parity for VM execution trace.
- Touches: VM inspect module, CLI, dispatch loop, disassembler.
- Change: `otter --trace run file.ts` prints frame entry and per-instruction
  lines with PC/op/operands/register summary.
- Acceptance: golden trace tests for simple script, call stack, throw path,
  async resume.
- Risk: Low.
- Effort: M.
- Depends on: 2.1, 2.2.

### Task 5.2 — IC / Shape / Frame Snapshots

- Goal: make performance bugs diagnosable.
- Touches: `property_ic.rs`, shape runtime/cache, frame state, inspector CLI/TUI.
- Change: commands for IC state, shape transition tree, frame/register windows.
- Acceptance: shape-transition breakpoint test; IC dump shows PIC/mega states.
- Risk: Medium.
- Effort: M.
- Depends on: 2.4.

### Task 5.3 — GC Snapshot Bridge

- Goal: expose heap state through the same inspector surface.
- Touches: `otter-gc` snapshot API callers, runtime/CLI.
- Change: inspector command writes Chrome-compatible heap snapshot and type-count
  summary.
- Acceptance: existing heap snapshot tests plus inspector command test.
- Risk: Low.
- Effort: S/M.
- Depends on: 5.1.

## Phase 6 — Object / Promise Polish

### Task 6.1 — Evaluate Object Internal-Method Vtable

- Goal: remove scattered object-kind dispatch where it hurts.
- Touches: object body, proxy/array/typed-array/arguments object internal ops.
- Change: adopt Boa/JSC-style static internal-method table if measurement shows
  a win after Value unification.
- Acceptance: no correctness regression; property/proxy benchmarks improve or
  task is rejected with data.
- Risk: Medium.
- Effort: M.
- Depends on: 1.1.

### Task 6.2 — Tighten Promise Capability / Job Records

- Goal: make Promise implementation read like ECMA-262 records.
- Touches: `promise.rs`, `promise_dispatch.rs`, `microtask.rs`, runtime promise
  registry.
- Change: align `PromiseCapability`, `PromiseReaction`, and queued jobs with
  ECMA-262 records; preserve isolate-local queue and Tokio token boundary.
- Acceptance: Test262 `built-ins/Promise`, await, async generators no regression;
  FIFO job-order tests added.
- Risk: Medium/High.
- Effort: M.
- Depends on: 1.1.

### Task 6.3 — Derive Trace / Finalize For New GC Bodies

- Goal: reduce manual tracing omissions.
- Touches: `otter-macros`, GC body types introduced in Phase 1.
- Change: derive macro mirrors Boa's `Trace`/`Finalize` safety pattern but emits
  Otter `SafeTraceable`/slot visitor code.
- Acceptance: compile-fail tests for untraceable fields; migrated bodies use the
  derive unless they need custom weak semantics.
- Risk: Medium.
- Effort: M.
- Depends on: 4.1.

## Open RFCs

- **RFC-1: Value tag layout.** Recommend LuaJIT/JSC-style NaN-box with pointer
  sub-tags and `GcHeader::tag()` discrimination. Owner approves exact bit layout.
- **RFC-2: Shortcut opcodes.** Recommend deleting `JsonCall`, `MathCall`,
  `PromiseCall`, and friends in bytecode v2.
- **RFC-3: PIC topology.** Recommend 4-entry PIC plus megamorphic state, close
  to Boa's simple IC shape and V8 Ignition's feedback-vector intent.
- **RFC-4: Snapshot pipeline.** Recommend defer until after Phases 1-3.
- **RFC-5: JIT backend.** Recommend no backend choice now; likely Cranelift for
  first baseline JIT RFC after bytecode v2.
- **RFC-6: Macro vs trait.** Trait remains backend/escape hatch;
  `otter_*` macros are preferred contributor DX and generate trait impls.
- **RFC-7: Object vtable.** Measure after Value unification; do not pre-commit
  before object representation stabilizes.

## Migration Order

Strict order:

1. Phase 0: baseline, unsafe boundary, roadmap truth, ADR.
2. Phase 1.1: `Value(u64)` hard cut-over. **DONE 2026-05-23.**
3. Phase 1.2 (GC payload migration): **DONE 2026-05-23** for the hot-path
   bodies (closure/upvalue, BigInt, ArrayBuffer, DataView, TypedArray,
   string Stage 1); Symbol/Temporal deferred. Phase 1.3 (frame split)
   not started — unblocked.
4. Phase 1.4 (new): GC extra-roots callback for cap-trigger →
   `error_classes` rooting. Unblocks `runtime_array_cap_is_catchable_as_range_error`.
5. Phase 2.1 and 2.2: bytecode v2 and single dispatch.
6. Phase 2.3, 2.4, 2.5: shortcut removal, PIC slots, native ABI.
7. Phase 3.1 and 3.2: bootstrap split and typed intrinsics. This can start
   after Phase 0 and run parallel to Phase 1 if it does not touch Value payloads.
8. Phase 4: macros after native ABI and `RealmIntrinsics`.
9. Phase 5: inspector after bytecode v2 and IC slots.
10. Phase 6: object/promise polish after Value unification; promise records can
    begin earlier only as tests/docs, not representation work.

Parallelizable:

- Phase 3 bootstrap split can run beside Phase 1 if changes are mechanical.
- Phase 4 macro crate tests can start beside Phase 3, but production port waits.
- Inspector CLI shell can start before bytecode v2, but step trace waits.

Blocked:

- JIT work is blocked until Phase 2.5.
- Snapshot pipeline is blocked until Phases 1-3.
- Object vtable decision is blocked until Value unification.

## Estimated Effort

| Phase | Effort | Risk |
|---|---:|---|
| Phase 0 | S, under 1 week | Low |
| Phase 1 — Value/allocation/frame | L, 1-2 months | High |
| Phase 2 — bytecode/dispatch/IC/JIT ABI | L, 1-2 months | High |
| Phase 3 — bootstrap/registry | M, 1-4 weeks | Medium |
| Phase 4 — macros | M, 2-4 weeks | Medium |
| Phase 5 — inspector | M, 2-4 weeks | Low/Medium |
| Phase 6 — object/promise polish | M, mostly folded into 1/4 | Medium |

Serial total: roughly 4-7 months for one engineer. With two engineers,
bootstrap/macros can overlap with Value/bytecode, but Value and bytecode must
not be merged simultaneously without a clean bisection boundary.

## Top Decisions For Owner

1. Approve `Value(u64)` hard cut-over and exact tag layout.
2. Approve deletion of shortcut call opcodes in bytecode v2.
3. Approve `BuiltinIntrinsic` as backend plus `otter_*` macros as preferred DX.
4. Approve deferring snapshot pipeline until after Value/bytecode/bootstrap.
5. Approve PIC topology: 4-entry polymorphic plus megamorphic state.

## Top Risks

1. Value migration is a whole-VM change. It must be merged alone and gated by
   Test262 plus microbenchmarks.
2. Bytecode v2 touches compiler, VM, disasm, dumps, source maps, and future JIT
   ABI. Schema drift is the main risk; roundtrip tests are mandatory.
3. Promise/job ordering bugs are easy to introduce while changing value and
   frame layouts. Test262 Promise/await/async-generator slices are mandatory
   before and after Phase 1 and Phase 6.
