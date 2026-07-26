# Next session

Continue `scratchpad/PLAN_CACHEIR_SUBSTRATE.md`. That file is the whole
scope. No detours onto bugs found while measuring — record them under
"Found, not fixed" in `scratchpad/LEDGER.md` and keep going.

## The goal, in one line

Stop paying hand-written machine code for every engine win. Every
improvement is a **declaration** that generates its interpreter path, its
machine code, its diagnostics and its tests. If a change adds a per-case
machine-code arm, it is the wrong change.

## The loop, every slice

```
1. just cost          # attribution; take the top line
2. just vs            # the bar: otter against node and bun
3. write ONE declaration (a CacheIR attacher, or an ABI descriptor + entry)
4. just gate          # fmt, clippy, vm/jit/bytecode tests, difftest
5. record the number in scratchpad/LEDGER.md
```

`just bcdiff <kernel>` puts otter's bytecode beside V8 Ignition's when you
need to rule the bytecode in or out.

**test262 is not in the iteration gate** — it is the closing gate of the
whole plan, compared as failing *sets* against a pre-plan snapshot.

## Hard rules

1. **Fix forward. Do not revert.** A wrong result is a bug to debug, not a
   change to undo. Bisect it, instrument the path, find the cause. Reverting
   working-in-progress throws away the diagnosis that came with it.
2. No coexistence. A slice deletes the hand-written thing it replaces, in
   the same slice. No adapter, no bridge, no legacy mode, no flag.
3. No internal contract versioning. Change the one contract in place.
4. No hand-written machine code for a case a declaration can describe.
   Machine code is written once per *op*, never per *case*.
5. Simple is not simplified. Algorithms stay Maglev-grade; `ir/` (SSA,
   regalloc, repr, safepoints, deopt lowering) is consumed, not weakened.
6. No new IR. Layering is `bytecode -> CacheIR (guards/ICs/native calls) ->
   SSA (optimizing)`.
7. Never ship a regression. Difftest must stay green.
8. No full workspace test runs. Targeted crates only.
9. No slice or phase labels in code comments; comments are timeless. No AI
   trailer in commits.
10. A declaration nothing consumes is dead weight — wire it in the same
    change that declares it.

## Where things stand

Slices 0 and 1 are done. Slice 2 is in progress.

Landed, newest first:

- `ca55cbfb` static natives lower from a declaration, not a code arm.
  `Math.abs` was 30 lines of aarch64 in a `match kind`; the operation now
  lives in `math_abs_leaf` behind `STUB_MATH_ABS_LEAF`, and support follows
  the declaration (`leaf_no_alloc_stub2_by_id(...).is_some()`), so no
  builtin is named in the emitter. Cost: 1.05ms -> 1.25ms per 200k calls.
- `a514913f` exotic `.length` served inline in both tiers via one shared
  `emit_exotic_length_fast`.
- `90a24f59` collection method guard offset fixed; optimizing tier bakes and
  emits allocating collection and array calls.
- `28cd1eca` CacheIR lowered generically, one shared probe emitter,
  megamorphic re-probation. Engine code 57 lines shorter.
- `1efca445` / `da8aa507` / `7ee01239` the measurement loop.

The bar right now (`just vs`, ms per `engineKernel()`):

| kernel | otter | vs node | vs bun |
| --- | ---: | ---: | ---: |
| native-boundary | 52.54 | 13.07x | 16.39x |
| branch-phi | 5.37 | 7.32x | 5.46x |
| method-call-monomorphic | 4.94 | 5.01x | 9.76x |
| dense-array | 4.22 | 4.73x | 5.59x |
| boxed-double-property | 4.30 | 4.42x | 3.28x |
| property-polymorphic | 8.33 | 3.42x | 5.50x |
| numeric-leaf | 1.75 | 33.30x | 54.00x |

## Do this next, in order

**1. Land `Math.floor`, `Math.sqrt`, `Math.max`, `Math.min` as table rows.**

They were drafted in `crates/otter-vm/src/math/mod.rs` (`JIT_LEAF_BUILTINS`)
with descriptors and entries, then reverted — that was the wrong call, redo
them and debug the failure instead. The five-builtin kernel produced
`2808813.2185012717` against node's `3107313.2185012717`, a difference of
298 500 over 200 000 iterations, i.e. 1.4925 per iteration.

Bisect one builtin at a time with a kernel of the shape
`c = c + a(i & 15)` etc. over locals bound from `Math.*`. Prime suspect:
what `argument_registers` actually holds at an `Op::Call` site — whether
index 0 is the first argument or the receiver — versus what
`emit_static_native_call` assumes for `argument_x` and the new
`second_argument_x` it now loads into `x2`. `Math.max`/`Math.min` are the
first two-argument users of that path, so it has never been exercised.

Each row also needs: a `JitStaticNativeCallKind` variant and its `from_u32`
arm, a descriptor appended to `RUNTIME_STUB_DESCRIPTORS` **in dense id
order** (the table is indexed by `id - 1`; inserting in the middle trips
`TransitionTable::resolve`), a `runtime_stub_name` arm, a registry record in
`crates/otter-vm/src/runtime_stubs.rs`, and the Rust entry. `math_unary_leaf`
is already there; `math_binary_leaf` was written and removed with the revert
— write it back.

When all five are green, that is the proof the substrate claim holds: five
builtins, zero machine-code arms.

**2. Callable identity as cache-program ops.**

Measured: `Math.abs(x)` as a method call costs 12.91ms per 200k against
1.05ms for `f(x)` with `f = Math.abs`, and a 0.51ms no-call baseline. 12.3x
is available. Cause: static-native targets are recorded only from ordinary
`Op::Call` feedback. The `CallMethodValue` arm in
`crates/otter-vm/src/interp/dispatch.rs` records a target only when a
*bytecode* callee frame was pushed, and a static native completes
synchronously without pushing one, so `staticNativeCalls` is always 0.

Do **not** fix this by teaching the method arm to record
`OrdinaryCallTarget::StaticNative`. `JitStaticNativeCallKind` is on this
plan's deletion list. The plan-conforming shape is:

```
GuardShapeId(receiver)          // the namespace object's shape
LoadDataSlotResult -> method    // the own slot holding the builtin
GuardNativeFn(desc)             // identity plus the declared signature
CallNativeLeaf(desc)
```

lowered by one emitter in `crates/otter-jit/src/template/arm64/ic_probe.rs`,
beside the way walk and the prototype hop. Then `JitStaticNativeCallKind`
and the separate method-call IC are deleted rather than grown, and the
relocation schema carries the stub id instead of the enum.

**3. The rest of Slice 2's op-set coverage**, per the plan: element access,
prototype/accessor ops, and `CallNativeLeaf(desc)` with descriptors
generated by `otter-macros` from the binding declaration, plus `.d.ts`
generated from the same descriptor.

## Traps already paid for

- The descriptor table is indexed by `id - 1`. Append only.
- There are **two** compile snapshot builders in
  `crates/otter-vm/src/interp/jit_compile.rs`. The optimizing one silently
  bakes less than the template one, under a comment claiming parity. Check
  both when feedback "does not arrive".
- The kernel loop usually runs in **OSR-compiled optimizing code**, so
  `--jit-tier template` does not prove what you think it proves.
- `--jit-events`' `methodFeedbackSites` counter reads 0 even for working
  sites; it reports a different channel. Instrument the bake path directly.
- Leaf calls emitted as a direct `blr` are not counted by
  `jit-*-stub-transitions`. A counter of 0 can mean "inline and fast", not
  "never taken". Confirm with wall time.
- Retired instructions is the metric; wall time is a sanity check. A wall
  reading that moves while retired instructions do not is thermal noise.
- `Op::LoadLength` exists, has an emitter, and is never emitted by the
  compiler. Dead opcodes with live code behind them are a recurring shape
  here — check that the compiler actually emits an op before optimizing it.
- The kernel harness validates a checksum; passing the wrong `--expected`
  yields an empty `metrics` array and a `0.00ms` reading, not an error.
