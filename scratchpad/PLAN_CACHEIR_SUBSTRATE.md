# PLAN: CacheIR as the single guard substrate

Named plan file. This file is the whole scope. No detours onto bugs found
while measuring — record them at the bottom under "Found, not fixed".

## Goal

Stop paying hand-written machine code for every engine win. Make every
future improvement a **declaration** that generates its interpreter path,
its machine code, its diagnostics, and its tests.

Target working loop, ~1 slice each, identical shape every time:

```
1. just cost      -> cost attribution report; take the top line
2. write ONE declaration (a CacheIR attacher or an ABI descriptor)
3. codegen emits: interpreter path, machine code, dumps, difftest hook
4. just gate      -> difftest + test262 failing-SET diff + kernel ledger
5. record the number. no movement -> the declaration is deleted, not kept
```

## Diagnosis (measured, not assumed)

The substrate already exists and already has the right design:

- `crates/otter-vm/src/cache_ir.rs` (657 LOC) — linear guard/load IR
  (`CacheOp`, `CacheStub`), operand file, data tables, three terminals.
  Its own module doc states the intent: *"This is the contract a later
  optimizing JIT lowers, so the interpreter and the JIT never describe
  the same cache twice."*

That intent is **not delivered**:

- `CacheOp` / `CacheStub` / `CacheStubSnapshot` are `pub(crate)` in
  `otter-vm`. `otter-jit` contains **zero** references to any of them.
- Template tier describes the same cache a second time by hand:
  `crates/otter-jit/src/template/arm64/properties.rs:43` `emit_load_property`,
  `:227` `emit_store_property`.
- Optimizing tier describes it a third time by hand:
  `crates/otter-jit/src/optimizing/arm64.rs` emits its own inline IC with
  `RelocationTarget::PropertyIcCell` at `:3081`, `:3184`, `:3277`, `:3396`.
- `CACHE_STUB_ABI_VERSION` (`cache_ir.rs:47`) versions an internal
  contract — forbidden by repo rules, and exists only because the three
  descriptions can drift.

Second symptom of the same cause — taxonomies grown by accretion instead
of abstraction:

- `crates/otter-vm/src/jit.rs:559` `JitStaticNativeCallKind` — **one**
  variant, `MathAbs`.
- `crates/otter-jit/src/artifact/relocation.rs:92` `GuardedBuiltinKind` =
  `Leaf | Alloc | Primitive | Array`, where `Array` means literally
  "dense-array push/pop".
- `crates/otter-vm/src/native_function.rs:124`
  `NativeFastFn = fn(&mut NativeCtx, &[Value]) -> Result<Value, NativeError>`
  — macros in `otter-macros` know every argument's Rust type at expansion
  and throw all of it away, so no type or effect reaches the JIT.

Cost of the current shape: one new fast path = hand work in the
interpreter **plus** template arm64 **plus** optimizing arm64 **plus** a
new feedback enum variant **plus** a new relocation kind. O(cases x tiers),
arm64 only.

Surface being replaced:

```
crates/otter-jit                      45 384 LOC
  template/ (25 hand arm64 files)     11 530
  optimizing/arm64.rs                  8 801   (single file)
  ir/ (ssa,regalloc,repr,safepoint..) 11 876   KEEP - Maglev-grade skeleton
crates/otter-vm/property_dispatch.rs   5 308
crates/otter-vm/method_ops/mod.rs      3 138
```

Dependency direction is clean and needs no new crate:
`otter-jit -> otter-vm -> otter-gc`.

## Hard rules for this plan

1. **No coexistence.** A slice that introduces the shared description
   deletes the hand-written descriptions it replaces, in the same slice.
   No parallel path, no adapter, no legacy mode, no fallback flag.
2. **No internal contract versioning.** `CACHE_STUB_ABI_VERSION` is
   deleted, not bumped. One contract, all producers and consumers changed
   together.
3. **No hand-written machine code for a case that a declaration can
   describe.** Machine code is written once per *op*, never per *case*.
4. **Simple != simplified.** Algorithms stay Maglev-grade: exact-PC deopt
   with frame state, SSA regalloc, representation selection, inlining.
   `ir/` is kept and consumed, not weakened.
5. **No new IR.** CacheIR is not a fourth IR. Layering is
   `bytecode -> CacheIR (guards/ICs/native calls) -> SSA (optimizing)`.
   Each layer has exactly one owner.
6. **Never ship a regression.** Gate compares failing *sets*, not
   percentages.
7. **No full workspace test runs.** Targeted crates only.
8. **No slice/phase labels in code.** Comments are timeless: they describe
   invariants, not the plan that produced them. This file owns the
   sequencing; the code never mentions it. Commits carry no AI trailer.

## Slice 0 — the loop itself

Nothing is replaced here; these are tools, not contracts.

- `just cost` — one reproducible report attributing retired instructions
  across: opcode / IC-miss / native boundary / GC / deopt. Build on the
  existing `--jit-events` and `benchmarks/profile.mjs --all`; retired
  instructions via `/usr/bin/time -l`.
- `just gate` — one command: `otter-difftest`, test262 failing-SET diff,
  kernel ledger (5 kernels, retired instructions, delta).
- `scratchpad/LEDGER.md` — per slice: hand-written LOC added, measured %
  won, and the ratio. **The ratio must fall slice over slice. If it does
  not, the substrate is not working: revert, do not extend.**

Exit: both commands run green from a clean checkout, ledger has its
first row (today's numbers as the baseline).

## Slice 1 — CacheIR gets its three consumers

Make the existing IR the only description of a property cache.

- Promote `CacheOp`, `CacheStub`, `CacheStubSnapshot` to `pub` in
  `otter-vm`. Delete `CACHE_STUB_ABI_VERSION` and every read of it.
- `otter-jit` gains **one** lowering: `CacheOp -> machine code`, plus
  **one** lowering `CacheOp -> SSA nodes` so the optimizing tier sees
  guards as ordinary hoistable/CSE-able nodes.
- A CacheIR verifier: operand defined before use, terminal is last and
  matches the access, data indices in range. Runs on every attach in
  debug and in difftest.

Deleted in this slice:

- `crates/otter-jit/src/template/arm64/properties.rs` (382 LOC) entirely.
- The bespoke property-IC emission in
  `crates/otter-jit/src/optimizing/arm64.rs` at `:3081/:3184/:3277/:3396`
  and its `PropertyIcAccess` special-casing.

Exit: `LoadProperty` / `StoreProperty` / `HasProperty` in all three tiers
are generated from one `CacheStub`. Kernel ledger shows no regression;
property-access kernels are the first place to expect a win, because
`GuardShapeId` becomes hoistable in the optimizing tier for free.

## Slice 2 — op-set coverage kills the parallel mechanisms

Extend `CacheOp` until every guarded fast path in the engine is a CacheIR
program. Each addition deletes its hand-written counterpart in the same
slice.

- Element access: `GuardDenseElements`, `LoadElementDense`,
  `StoreElementDense`, typed-array variants. Deletes the dense-array
  special cases and the `GuardedBuiltinKind::Array` category.
- Prototype chain and accessors: `GuardProtoChain`, `CallGetter`,
  `CallSetter`.
- Callable identity: `GuardNativeFn(desc)`, `GuardFunctionId(fid)`.
  Deletes `JitStaticNativeCallKind` (`jit.rs:559`) and the rest of
  `GuardedBuiltinKind` (`relocation.rs:92`). The method-call IC in
  `crates/otter-vm/src/method_ops/mod.rs` stops being a separate
  mechanism and becomes attachers.
- Native calls: `CallNativeLeaf(desc)` / `CallNativeAlloc(desc)` where
  `desc` is a `NativeAbiDescriptor` generated by `otter-macros` from the
  binding declaration — argument types, return type, effects, exception
  behavior, safepoint requirement. Reuse the existing families in
  `crates/otter-vm/src/native_abi/runtime_stubs.rs:186`; do not invent a
  second ABI.

Native ABI constraints (these are correctness, not style):

- The typed entry and the boxed entry are generated from **one**
  `#[inline]` typed core. Two hand-maintained bodies are forbidden.
- Declared effects are enforced by the type the entry receives, not by a
  comment: leaf gets `&Heap`, mutating leaf gets `&mut GcHeap`,
  allocating gets `NativeCtx` with a scope. Same discipline as the
  existing stub table.
- A typed entry is only reachable when the call site's guards have proven
  the argument types. Coercion order stays observable-identical —
  `ToPrimitive`/`ToNumeric` side effects must not disappear.
- `.d.ts` under `crates/otter-pm/src/types/otter/` is generated from the
  same descriptor. Hand-synced type files stop existing.

Exit: `NativeFastFn`'s untyped `&[Value]` shape is no longer the only
builtin ABI; `just cost`'s native-boundary axis moves measurably on the
serve/fetch path (previously measured native-bridge-bound with the JIT
providing no help).

## Slice 3 — the hand-written asm surface collapses

With guards, elements, calls and natives all expressed as CacheIR, the
per-domain arm64 files stop holding case logic and hold only op lowering.

- Collapse `crates/otter-jit/src/template/arm64/*` into the single op
  lowering table plus the genuinely per-opcode arithmetic paths.
- Split `crates/otter-jit/src/optimizing/arm64.rs` (8 801 LOC) — over the
  1 000-line module limit — along the same op boundaries.

Exit: template arm64 LOC down by a large multiple; adding a new fast path
touches no `.rs` file under `template/arm64/`.

## Slice 4 — copy-and-patch, x86_64 for free

Only now, when the asm surface is small and declarative.

- Extract stencils at build time from ordinary Rust compiled by the
  ordinary backend; the template tier becomes generated machine code with
  no hand emission.
- The optimizing tier keeps its real emitter, SSA and regalloc — that is
  not a bridge, it is two tiers with different jobs.
- x86_64 falls out. The engine is arm64-only today, which is a limit on
  who can use it at all, not a perf number.

Exit: `just gate` green on both architectures.

## Slice 5 — one opcode definition, generated scaffolding

One source of truth per opcode generating: enum, decoder, register
verifier (already schema-derived — extend, do not duplicate), dispatch
skeleton, JIT capability table, dumper, difftest hook.

Exit: adding an opcode is one edit to the definition file.

## Non-goals

- Rewriting `ir/`. SSA, regalloc, repr, safepoints, deopt lowering stay.
- Chasing benchmark numbers. Kernels are a thermometer; the engine is
  refactored, never fitted to a benchmark.
- Any capability, security or spec-conformance change. Failing sets only
  shrink.

## Gate, every slice

```
just gate     # fmt, clippy, otter-vm + otter-jit tests, difftest, kernel ledger
just cost     # ledger row
```

**test262 is not part of the iteration gate.** It costs too much wall clock
to run between slices. Correctness while iterating is carried by
`otter-difftest` (interpreter vs every tier, same program, same result) plus
the crate unit tests — that is the signal that actually catches tier
divergence, which is the risk this plan creates.

test262 runs **once, as the closing gate of the whole plan**, comparing
failing *sets* against the pre-plan snapshot. Nothing lands at the end that
grew the failing set. Take that snapshot before Slice 1 touches any tier
code, so the comparison exists later.

## Found, not fixed

(Anything discovered while measuring goes here and stays here until this
plan is finished.)
