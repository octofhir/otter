# Otter Refactor Plan — 2026-05 (Phase 2+)

Baseline audit: `docs/architecture-audit-2026-05.md`. This document
is the executable roadmap; it tracks what is shipped and what is
still open.

## Status — 2026-05-24

Phases 0, 1, 2.1–2.5, 3.1–3.3, and 4.1–4.3 shipped. Active tracked
work is:

| Phase | Task                                       | Status                |
| ----- | ------------------------------------------ | --------------------- |
| 3.3   | Snapshot checkpoint decision (docs only)   | DONE 2026-05-24       |
| 4.1   | Replace macro API with `otter_*`           | DONE 2026-05-24       |
| 4.2   | Port first intrinsics through new macros   | DONE 2026-05-24       |
| 4.3   | Module install macro + Web API ports       | DONE 2026-05-24       |
| 5.1   | Step trace                                 | Open                  |
| 5.2   | IC / shape / frame snapshots               | Open                  |
| 5.3   | GC snapshot bridge                         | Open, blocked on 5.1  |
| 6.1   | Object internal-method vtable evaluation   | Open                  |
| 6.2   | Tighten Promise capability / job records   | Open                  |
| 6.3   | Derive Trace / Finalize for new GC bodies  | Open (4.1 unblocked)  |

Open carry-over tech debt from Phase 2.3 / 2.4 (not yet promoted to
their own task numbers):

- **Temporal `[[Construct]]` argument shapes.** Direct
  `new Temporal.Instant(epochNs)` and the partial-record forms still
  throw `TypeError` — wiring is in
  `crates/otter-vm/src/temporal/intrinsic.rs::temporal_class_direct_construct`.
  Test262 `built-ins/Temporal` at 208/4603 — most of the remaining
  4393 fails depend on `[[Construct]]`, prototype chains, and the
  rest of the proposal-temporal property surface.
- **Object 15 residual failures.** `built-ins/Object` 3391/3414;
  remaining 15 are pre-existing gaps in unrelated surfaces — async-
  function class wrapping (`seal/seal-asyncfunction.js` family),
  `Object.prototype.toString` for Proxy-of-function, `nan-equivalence`
  redefine corners, `Object.getOwnPropertyNames(15.2.3.4-4-2)` for
  prototype overrides. Each is a one-off spec gap, not an architectural
  miss.
- **Proxy residual.** `built-ins/Proxy` 219/311; the ~95 remaining
  largely block on compiler-side `Function.prototype.apply` support
  (FEATURE_NOT_IN_SLICE on test bodies that use
  `target.apply(thisArg, args)` syntax). Filed against Phase 4 /
  macro work indirectly via Reflect surface coverage.
- **Compiler operand cap audit.** Dense `NewArray` capped at 240
  elements (commit 52c82d31). The same `u8::MAX` operand limit
  affects other variadic opcodes (`MakeClosure`, template raw / cooked
  list, call-arg windows). Audit + chunked-fallback work tracked here.
- **`is_object_like` audit follow-up.** Fixed 5 spec-Object widening
  sites (commit b1d67278). `value/mod.rs` still has internal
  accessor-extractor sites that intentionally use the narrow form —
  re-grep on every new builtin to keep the spec-leaning check
  honest.

## Principles

- Correctness > DX > performance > new features.
- No backward-compatible duplicate APIs during this refactor. Otter
  is pre-1.0.
- No long-lived feature flags. Large refactors land as hard cut-over
  PRs on short-lived branches, tests + clippy gated.
- Every task has an acceptance signal: Test262 delta, unit coverage,
  benchmark guard, or static invariant.
- Every new non-trivial Rust module gets repository-standard
  `//!` sections: purpose, contents, invariants, see also.

## Completed Phases (historical record)

Detail for each shipped task — including pre-merge analysis,
implementation notes, test262 deltas — lives in the commit messages
referenced below. This section is the index.

### Phase 0 — Foundational

- **0.1 Test262 baseline.** Captured at start of Phase 1; every
  subsequent task cites before/after deltas in commit messages.
- **0.2 Unsafe boundary.** `#![forbid(unsafe_code)]` on
  `otter-vm` / `otter-runtime` / `otter-compiler` / `otter-bytecode`.
  GC + FFI keep local `[lints.rust]` overrides.
- **0.3 Roadmap truth.** ROADMAP P1 corrected; re-marked DONE
  after Task 2.4 PIC slots landed.
- **0.4 ADR.** `Value(u64)`, byte-offset PC, fixed IC slot ABI,
  `BuiltinIntrinsic` registry backend, `otter_*` macro preferred
  DX all approved.

### Phase 1 — Value & Allocation

- **1.1 `Value(u64)` cut-over** — DONE 2026-05-23.
  `pub struct Value(u64)`; LuaJIT/JSC-style NaN-box, sub-tags
  `0x7FFC..0x7FFF`, `GcHeader::tag()` discrimination for
  TAG_PTR_OTHER. Hard cut-over; legacy enum deleted.
- **1.2 GC payload migration** — DONE 2026-05-23.
  Closure/upvalue, BigInt, ArrayBuffer, DataView, TypedArray,
  string Stage 1 bodies moved into GC. Symbol / Temporal payloads
  left on the foundation form pending follow-up.
- **1.3 Hot / Cold Frame split** — DONE 2026-05-23.
  Hot `Frame` 488 B → 128 B (two cache lines) with `const _: () =
  assert!(...)` lock. Cold protocol state moved into per-`Interpreter`
  `ColdFramePool`, lazily acquired; async-await / generator-yield
  detach the cold record so pool slots rotate while frames sleep.
- **1.4 GC extra-roots callback** — DONE 2026-05-23.
  `ExtraRootsCallback` trampoline on `GcHeap` walks
  `RuntimeState`-owned roots from cap-trigger `collect_full` without
  re-entering `Interpreter::force_gc`. Unblocked
  `runtime_array_cap_is_catchable_as_range_error`.

### Phase 2 — Bytecode, Dispatch, IC, JIT-Ready ABI

- **2.1 Bytecode wire format + byte-offset PC** — DONE 2026-05-23.
  Dense `OP_BYTE_TABLE`, `op_byte_assignments_are_sequential` test
  enforces density on every opcode add.
- **2.2 Collapse dispatch to one loop** — DONE 2026-05-23.
  Single `match op` body per instruction; trailing `_ => {}` dropped
  (match is exhaustive).
- **2.3 Remove shortcut call opcodes** — DONE 2026-05-23,
  follow-up DONE 2026-05-24.
  Ten by-name shortcuts deleted: `JsonCall`, `MathCall`, `SymbolCall`,
  `DateCall`, `ReflectCall`, `ProxyCall`, `IteratorCall`,
  `TypedArrayCall`, `GlobalCall`, `TemporalCall`. Follow-up: also
  removed `ObjectCall` and added three spec-primitive opcodes
  (`ForInKeys`, `CopyDataProperties`, `DefineOwnProperty`) for the
  remaining compiler-internal callers. User-facing `<NS>.<method>(args)`
  flows through `LoadGlobalOrThrow + CallMethodValue` exclusively;
  shadows of `globalThis.<NS>` and `<NS>.<method>` are observable
  per spec. `Temporal` placeholder replaced with a real
  `NamespaceBuilder`-driven installer that exposes Instant / Duration
  / PlainDate / PlainTime / PlainDateTime as `NativeFunction`
  constructors plus the `Now` namespace; Proxy `is_object_like` →
  `is_object_type` widening recovers `Proxy/apply/*` family.
- **2.4 Polymorphic IC slots** — DONE 2026-05-24.
  `PropertyIcEntry<T>` rewritten as `Empty / Polymorphic { entries:
  SmallVec<[T; 4]>, misses } / Megamorphic`. Linear probe; install
  appends until full; full PIC + miss budget → Megamorphic (sticky).
  `SmallVec` cap = `MAX_PIC_ENTRIES`, no heap spill. Counter field
  names preserved for introspection compat.
- **2.5 Native call ABI freeze** — DONE 2026-05-23.
  Authoritative spec at [`docs/native-call-abi.md`](native-call-abi.md);
  compile-fail tests under `crates/otter-vm/tests/compile_fail/`
  cover the forbidden patterns. ABI v1; variant additions require
  version bump + coordinated migration.

### Phase 3 — Bootstrap & IntrinsicRegistry

- **3.1 Split bootstrap bodies** — DONE 2026-05-23.
  `bootstrap.rs` 3859 LoC → 638 LoC; per-intrinsic installer bodies
  under `crates/otter-vm/src/intrinsics/`. Shared helpers in
  `intrinsics/shared.rs`, re-exported under `crate::bootstrap::`.
- **3.2 `RealmIntrinsics`** — DONE 2026-05-23.
  Typed slots for `%Object%`, `%Object.prototype%`,
  `%Function.prototype%`, `%Array%`, `%Array.prototype%`. Populated
  end of `build_global_this_impl`; runtime lookups hit slots first.

## Open Tasks

### Task 3.3 — Decide Snapshot Checkpoint — DONE 2026-05-24

- Goal: avoid premature snapshot work while preserving future path.
- Touches: docs only.
- Change: write snapshot prerequisites and explicit defer decision.
- Acceptance: owner sign-off — defer until after Phases 4 and 5
  (bytecode v2 + bootstrap split shipped; macros next).
- Risk: Low.
- Effort: S.
- Depends on: nothing remaining.
- **Status:** Shipped. Decision lives at
  [`docs/snapshot-checkpoint-decision.md`](snapshot-checkpoint-decision.md):
  defer until P1 (`otter_*` macros load-bearing) + P2 (GC body
  schema frozen, Symbol/Temporal payloads migrated, `SafeTraceable`
  derive shipped) + P3 (realm intrinsic slot table closed or growth
  policy documented) + P4 (bytecode wire format ratchet test) + P5
  (cold-start regression budget defined). Scope excludes the
  already-shipped `HeapSnapshot` (Rust-side retained-size walker)
  and `devtools_snapshot` (Chrome `.heapsnapshot` exporter); both
  stay read-only diagnostic surfaces. Future RFC-4 picks up the
  design once acceptance triggers all green.

### Task 4.1 — Replace Current Macro API With `otter_*` — DONE 2026-05-24

Shipped surface: `holt!` (namespace), `couch!` (class), `lodge!`
(hosted module), `raft!` (method table), `#[dive]` (single binding).
Legacy `js_namespace` / `js_class` / `js_fn` / `js_constructor` proc
macros deleted (commit 480a46c0). Detail in
[`docs/otter-macros-refactor-tracker.md`](otter-macros-refactor-tracker.md).

Tests + clippy clean across `otter-macros`, `otter-vm`, `otter-runtime`,
`otter-web`, `otter-modules`. Doctest matrix on each macro.

### Task 4.2 — Port First Intrinsics — DONE 2026-05-24

Every class intrinsic moved onto `couch!`: WeakRef,
FinalizationRegistry, BigInt, Map/Set/WeakMap/WeakSet, ArrayBuffer /
SharedArrayBuffer, DataView, RegExp, Boolean, Number, Symbol, String,
Array, Object, Function, Date, Proxy, Promise, Iterator, the five
Temporal classes, and the 11 TypedArrays + abstract `%TypedArray%`.
Namespaces (Math, JSON, Reflect, Atomics, Console) on `holt!`. Error
classes intentionally excluded — they live in a per-interpreter
`ErrorClassRegistry`, not `BOOTSTRAP_ENTRIES`.

### Task 4.3 — Module Install Macro — DONE 2026-05-24

`lodge!` ships at `crates/otter-macros/src/lodge.rs` — generates
hosted-module installers (capability-aware closures or static fn
exports). Consumers: `otter:kv`, `otter:sql`, `otter:ffi` (commit
d569c060).

Web APIs (URL / Headers / Blob / Request / Response) folded into
`couch!` rather than a separate `web!` macro — they ARE just global
classes, only the install backend differs. `GlobalClass` reshaped to
wrap either a `RuntimeClassSpec` (legacy) or a
`BuiltinIntrinsic::install` fn pointer (couch!-generated). `feature
= WEB` flag added (commit 745f1ccf).

### Task 5.1 — Step Trace

- Goal: Boa parity for VM execution trace.
- Touches: VM inspect module, CLI, dispatch loop, disassembler.
- Change: `otter --trace run file.ts` prints frame entry and
  per-instruction lines with PC / op / operands / register summary.
- Acceptance: golden trace tests for simple script, call stack,
  throw path, async resume.
- Risk: Low.
- Effort: M.
- Depends on: 2.1 ✓, 2.2 ✓ (unblocked).

### Task 5.2 — IC / Shape / Frame Snapshots

- Goal: make performance bugs diagnosable.
- Touches: `property_ic.rs`, shape runtime / cache, frame state,
  inspector CLI / TUI.
- Change: commands for IC state (per-site PIC slot dump including
  Megamorphic markers), shape transition tree, frame / register
  windows.
- Acceptance: shape-transition breakpoint test; IC dump shows PIC
  entry list and Megamorphic state.
- Risk: Medium.
- Effort: M.
- Depends on: 2.4 ✓ (unblocked).

### Task 5.3 — GC Snapshot Bridge

- Goal: expose heap state through the same inspector surface.
- Touches: `otter-gc` snapshot API callers, runtime / CLI.
- Change: inspector command writes Chrome-compatible heap snapshot
  and type-count summary.
- Acceptance: existing heap snapshot tests plus inspector command
  test.
- Risk: Low.
- Effort: S/M.
- Depends on: 5.1.

### Task 6.1 — Evaluate Object Internal-Method Vtable

- Goal: remove scattered object-kind dispatch where it hurts.
- Touches: object body, proxy / array / typed-array / arguments
  object internal ops.
- Change: adopt Boa/JSC-style static internal-method table if
  measurement shows a win after Value unification.
- Acceptance: no correctness regression; property / proxy benchmarks
  improve, or task is rejected with data.
- Risk: Medium.
- Effort: M.
- Depends on: 1.1 ✓ (unblocked).

### Task 6.2 — Tighten Promise Capability / Job Records

- Goal: make Promise implementation read like ECMA-262 records.
- Touches: `promise.rs`, `promise_dispatch.rs`, `microtask.rs`,
  runtime promise registry.
- Change: align `PromiseCapability`, `PromiseReaction`, queued jobs
  with ECMA-262 records; preserve isolate-local queue and Tokio
  token boundary.
- Acceptance: Test262 `built-ins/Promise`, await, async generators
  no regression; FIFO job-order tests added.
- Risk: Medium/High.
- Effort: M.
- Depends on: 1.1 ✓ (unblocked).

### Task 6.3 — Derive Trace / Finalize For New GC Bodies

- Goal: reduce manual tracing omissions.
- Touches: `otter-macros`, GC body types introduced in Phase 1.
- Change: derive macro mirrors Boa's `Trace`/`Finalize` safety
  pattern but emits Otter `SafeTraceable` / slot-visitor code.
- Acceptance: compile-fail tests for untraceable fields; migrated
  bodies use the derive unless they need custom weak semantics.
- Risk: Medium.
- Effort: M.
- Depends on: 4.1.

## Migration Order (remaining)

Phase 4 fully shipped. Remaining work in strict order:

1. **5.1** step trace — unblocks 5.3.
2. **5.2** PIC introspection — independent of 5.1.
3. **5.3** GC snapshot — after 5.1.
4. **6.1** vtable measurement — independent; can run alongside any
   of the above once a baseline microbench exists.
5. **6.2** Promise records — independent; gated only on test262
   Promise stability.
6. **6.3** trace derive — 4.1 unblocked, ready to start.

Parallelizable: 5.x / 6.x do not contend for the same files. Promise
work (6.2) can land on an independent branch.

Blocked: JIT remains blocked on a PIC-aware tiering RFC (RFC-5).
Object vtable decision (6.1) is unblocked; needs measurement before
commit.

## Estimated Remaining Effort

| Phase                     | Effort        | Risk       |
| ------------------------- | ------------- | ---------- |
| Phase 5 (5.1 + 5.2 + 5.3) | M, 2-4 weeks  | Low/Medium |
| Phase 6 (6.1 + 6.2 + 6.3) | M, 2-4 weeks  | Medium     |

Serial total: ~1-2 months for one engineer. Phases 5 + 6 parallelize
cleanly across two engineers.

## Open RFCs (remaining)

- **RFC-4: Snapshot pipeline.** Deferred; defer doc with
  prerequisites lives at
  [`docs/snapshot-checkpoint-decision.md`](snapshot-checkpoint-decision.md).
  Resume when P1–P5 in that doc all green plus a concrete embedder
  cold-start budget surfaces.
- **RFC-5: JIT backend.** Cranelift recommended for first baseline
  JIT after Phase 4 ships and the macro-generated descriptor surface
  is stable. No backend commit yet.
- **RFC-7: Object vtable.** Measurable in 6.1; commit only after
  benchmark data.

RFC-1 (Value tag layout), RFC-2 (shortcut opcodes), RFC-3 (PIC
topology), and RFC-6 (macro vs trait split) are resolved — see the
matching Phase 1 / Phase 2 / Phase 4 tasks for the shipped form.

## Top Risks (remaining)

1. **Macro rewrite (4.1)** churns every intrinsic call site. Land
   on a short-lived branch with the full Test262 suite as the gate;
   no partial macro / legacy coexistence in `main`.
2. **Promise record tightening (6.2)** is the highest correctness
   risk left — ordering bugs in microtask flush are easy to
   introduce. Mandatory: `built-ins/Promise`, `language/expressions/await`,
   `language/statements/async-generator` slices stay flat.
3. **Inspector trace (5.1)** can leak interpreter-internal state
   into golden tests; keep the format documented and tied to a
   schema test so future opcode renames break the trace at compile
   time rather than at the test diff.
