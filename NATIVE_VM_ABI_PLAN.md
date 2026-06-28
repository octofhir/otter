# Native VM ABI Plan

**Status:** active architectural refactor. Tracks only OUTSTANDING work.
Completed slices live in git history (`git log -- NATIVE_VM_ABI_PLAN.md`).

Goal: move Otter from "Rust VM with JIT fast paths" toward a JSC/V8-style tiered
native engine — one shared execution ABI across interpreter, baseline JIT,
optimizing JIT, runtime stubs, and GC. No backward compatibility with the
current internal ABI; breaking changes allowed when they improve peak
throughput and preserve ECMAScript conformance.

## Where the gap is now (measured)

The collection method ABI is done: the narrow Rust bridge is fully bypassed,
hot Map/Set calls run as direct machine calls to leaf/alloc `AllocValueStub`
entries (`jitRuntimeCollectionMethodIcStubs` = 0 on `map-set.js`). Removing it
did **not** move wall-clock — proof the remaining gap is NOT boundary crossings.

Two dominant levers remain:

1. **Native-heavy benches (map-set, string-ops, json, tree):** the hot loop is
   still partly an interpreter dispatch loop plus collection-primitive / string
   work. Lever = Phase 5 interpreter quickening + Phase 3/4 lowering more of the
   loop into compiled code. The first string win (short-concat inline flatten)
   landed; map-set is 6.9x node, string subsystem is the floor.
2. **Pure-compute benches (fib 11x, nbody 17x, mandelbrot):** codegen quality,
   not the ABI. Lever = a real optimizing/Tier2 tier (see `CRANELIFT_TIER2.md`).

## Target ABI (reference)

### JS frame layout

Every JS frame, interpreted or compiled, describable by one metadata shape:

```text
FrameHeader   prev-fp | function id | code block id | resume pc/instr-index
              | flags | register count | arg count | feedback vector | scratch
RegisterWindow  tagged Value slots visible to deopt/debugger/profiler/GC
NativeSpillArea tier-owned machine spills, described by safepoint maps when live
```

Rules: interpreter and JIT materialize the same header; deopt reconstructs the
register window directly (no replay-from-entry); one frame walk for
debugger/profiler/stack-trace; JIT may keep unboxed values in registers between
safepoints; every safepoint maps tagged values in registers/spills/frame slots.

### Runtime stub ABI

Machine-callable functions, not generic `NativeCtx` calls on hot paths.

```text
StubResult  status: ok|throw|deopt|oom|interrupt ; value: Value ; side payload
```

Stub classes: `LeafNoAlloc` (no safepoint, no GC, no reentry), `AllocStub`
(safepoint id required, may GC), `ReentrantStub` (safepoint + deopt
continuation). Inputs: isolate ptr, frame ptr, safepoint id (alloc/reentrant),
raw `Value` args in ABI regs/slots, feedback id where relevant.

### Safepoint maps

Cover: frame-header location, tagged frame slots, tagged registers, tagged spill
slots, deopt frame-state id, call continuation PC, stub class + alloc/reentry
permissions. One metadata source serves moving-GC root discovery/update, deopt
materialization, stack walking, and GC-stress validation.

## Outstanding work

### Phase 6: lightweight self-recursive / direct-call frame ABI (compute gap)

Measured: pure-compute call benches are codegen-bound, not coverage-bound.
`fib` (2.96x Bun) and `call-dispatch` (3.24x Bun) compile fully into the
optimizing tier (`fib`: frameless self-recursion, `red=3619`, `jitDirect=0`)
yet stay ~3x. The dynasm self-recursive-call sequence
(`emit::emit_self_recursive_call`) rebuilds a full frame per call — and `fib`
makes 2.7M of them:

- bump a fresh register window on the JIT reg-stack (+ overflow guard);
- a fill loop writing `undefined` across all `register_count` window slots;
- copy the entire 8-field `JitCtx` struct onto the stack for the callee;
- copy args from the caller frame into the new window;
- `emit_frame_materialize` (box int32->tagged + store live values to the
  `[x19]` frame) before the call and `emit_frame_reload` (load + unbox) after,
  because every allocatable GP/FP register is caller-saved and the recursion
  clobbers them.

JSC/Bun recursion is args-in-registers + `bl` with callee-saved survivors. The
gap is this per-call frame rebuild, which is the shared frame-header ABI this
document targets.

Staged, each independently verifiable (diff.mjs 21/21, test262, GC-stress 128):

- [x] **6a — callee-saved register pool.** Extend the optimizing allocator GP
      pool `x9..x15` (7, all caller-saved) with `x21..x28` and the FP pool
      `d0..d5` with `d8..d15`; map the new abstract `Reg` indices in
      `emit::phys` / `phys_fp`; save/restore the *used* callee-saved registers in
      both prologues (`emit_prologue` entry + OSR) and every `emit_epilogue`
      exit; keep the backedge-poll save area caller-saved-only (the Rust poll
      stub preserves callee-saved). Independently correct with materialize/reload
      still present; reduces spilling on register-pressured compiled functions.
- [x] **6b — call-clobber-aware allocation.** In `regalloc`, mark an interval
      that spans any `Call` / `CallMethod` position as call-crossing and restrict
      its candidate registers to the callee-saved sub-pool (spill on overflow).
      Caller-saved registers are then free to reuse across calls.
- [~] **6c — drop per-call materialize/reload.** Done for the self-recursive
      call's post-call `emit_frame_reload` — with 6b every cross-call value is in
      a callee-saved register or spill slot, both of which the recursion
      preserves, so only the result is reloaded. Remaining: drop the pre-call
      full-frame `emit_frame_materialize` too (it still runs every call so the
      self-call can read callee/args from `[x19]`); materialize only callee+args
      on the hot path and the full deopt frame on the bail exit; extend to the
      generic direct-call path.
- [ ] **6d — lighten the self-call ctx setup.** Avoid the per-call 8-field
      `JitCtx` copy + window fill: define a self-call entry that takes the new
      register-window base in a register and shares the parent ctx's invariant
      fields, resetting only `bail_pc`. The window fill (undefined-init) is only
      needed for a deopt read — emit it lazily on the bail path, not per call.

Exit: `fib` / `call-dispatch` recursion is args-in-registers + `bl` with no
per-call frame rebuild; both within ~1.5x Bun. Direct (non-self) calls share the
same callee-saved survivor convention.

### Phase 0: transition inventory and counters (partial)

- [ ] Document every live transition: interpreter→native, JIT→VM, JIT→runtime,
      runtime→JS callback, alloc slow path, write-barrier slow path, deopt, OSR.
- [ ] Count hot transition frequency on `map-set.js`, `string-ops.js`,
      `tree-traversal.js`, `prop-access.js`, `array-ops.js`.
- [ ] Counters for root-scope pushes, native calls by builtin, allocating
      builtin calls, safepoint-like calls, deopts, OSR entries, IC misses.
- [ ] Per-op interpreter cost and per-stub-call cost on Apple Silicon.

Exit: a transition table with frequency, inclusive time, and owner; each top
benchmark has a measured dominant boundary or proof it is compiler coverage.

### Phase 3 leftover: optimizing-tier string-concat node

Baseline `string_concat_alloc` stub and the primitive short-concat inline
flatten are done. Open:

- [ ] Lower `"k" + int` style concat into a specialized optimizing-tier string
      node with allocation fast path and deopt fallback.
- [ ] OSR into loops containing it; invalidate on dependency changes.

Exit: hot string-building loops compile the concat node; no interpreter delegate
for the primitive string case.

### Phase 4: object header unification

- [ ] Design a shared object header for ordinary AND exotic objects (class id /
      type flags, shape ptr, prototype ptr, slot storage ptr, exotic payload
      ptr).
- [ ] Migrate Map/Set behind compatibility accessors; move
      prototype/expando/shape-visible state out of bespoke collection bodies.
- [ ] Make object ICs and method ICs work for Map/Set with no bespoke receiver
      special cases; preserve tracing, write barriers, insertion order, iterators.

Exit: `Value::as_object_like` returns a guardable shared header for ordinary
objects, arrays, maps, sets; Map/Set method lookup uses the same IC/dependency
model as ordinary objects.

### Phase 5: interpreter quickening and bytecode metadata

- [ ] Feedback-vector-indexed quickening for stable monomorphic sites.
- [ ] Hot frame PC as instruction index, preserving byte-PC debug/deopt mapping.
- [ ] Inline fixed hot operands / compact hot operand view.
- [ ] Reduction checks on weighted blocks/backedges with interrupt polls at
      calls, loops, allocations, explicit safepoints.

Exit: interpreter cost approaches the direct-dispatch floor; interpreter stays a
correct deopt/debug target but is no longer the hot-loop performance strategy.

## Verification contract

Every slice states which surfaces it touches (frame layout, deopt, GC
safepoints, runtime stubs, object layout, bytecode metadata) and runs the
checks that scale with them:

- `cargo test -p otter-vm`, `cargo test -p otter-jit`;
- targeted Test262 subset for touched builtins/opcodes;
- JIT on/off parity for touched benchmarks (`node benchmarks/diff.mjs`);
- `OTTER_GC_STRESS=128 OTTER_JIT=1` for any allocating compiled path;
- before/after profile on the triggering benchmark, including transition counters.

## Non-goals

- No benchmark-specific semantic shortcuts.
- No conservative native stack scanning as the default GC strategy.
- No bail-to-function-entry deopt shortcuts.
- No per-feature runtime kill switches.
- No public runtime boundary exposing raw `Rc`/`RefCell`/raw heap handles or
  untracked GC pointers.
