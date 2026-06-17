# Otter Engine Refactor — Progress

**Plan source:** [`plan.md`](plan.md) — *OtterJS большой breaking-refactor план*
**Naming policy:** all new engine-internal names use Otter/river/den/holt/raft/pelt/lodge/current/bank/stream/slide/dive/burrow vocabulary. Never name a new Otter tier/module/struct after another engine or its tiers.
**Working rule:** every slice is independently buildable, testable, and revertible. One architectural slice per commit. No parallel VM/runtime stack. Security (fs/net/env/subprocess/ffi capability checks) and precise GC rooting are invariant across all slices.

---

## STRATEGIC VERDICT 2026-06-17 — baseline JIT is mature; remaining gap = inlining

Evidence (prop-access, 13.6× node, 510ms): load IC **3,003,243 hits / 13 miss**, ALL
functions compile (`Ok(Compiled)`), OSR fires, 3M direct calls, store/method ICs all
present in main. The baseline tier is feature-complete (ICs load+store, direct calls,
loop-OSR, float arith, element load/store). I kept picking "next slices" (inline store
IC, method IC, callback lean-dispatch) that turned out ALREADY LANDED — memory was stale.

Where the 5-14× gaps actually live (prop-access/sort/array-ops/string): **per-call
overhead (~95ns/call) + NO inlining + no type speculation.** prop-access = 1.5M iters ×
2 tiny method calls (`bump`/`dist2`); each pays ~95ns; node inlines them to ~0. Same
root as sort/array-ops callbacks. The baseline JIT cannot inline or speculate.

**The one real remaining lever = INLINING hot tiny callees** (leaf methods / monomorphic
callbacks): either narrow (baseline leaf-inline: splice a monomorphic tiny compiled
callee into the call site under a shape/code guard, eliminating the per-call bridge) or
broad (an optimizing tier: SSA + type feedback + speculative inline + deopt). This is a
large dedicated structural project, NOT an autonomous-tick slice. Compute-bound benches
(mandelbrot 1.7×, nbody 3.8×) are already competitive — they don't need it; call/property-
bound code does. Incremental baseline tuning is exhausted; do not keep mining it.

## Phase 2 — status update 2026-06-17 (Slice 2c REDUNDANT; call path at baseline floor)

Slice 2c (native-callback lean dispatch) was farmed to a worktree agent — it built a
clean `LeanCallbackStack` and measured sort 2128→1472ms (−31%), array-ops 1239→695ms
(−44%). BUT its worktree was based on `origin/main` (ccf93ac2) which LACKS main's
already-committed lean-invoke machinery (`acquire_lean_callback_stack` +
`run_bytecode_callable_committed`, which already reach `dispatch_jit_sync_entry` and
run the compiled callback). Measured main-current (cb54ca4f): **sort 1456ms, array-ops
723ms** — i.e. main is ALREADY at the agent's "after" numbers. The agent merely caught
its stale base UP to main. Net delta of porting ≈ 0 (sort) / ~4% noise (array-ops).
**Discarded the agent commit f9450214.** Lesson: worktree agents base on origin/main,
not local HEAD — measure main-current before farming a perf slice.

Call path (sort/array callbacks) is now at the BASELINE-JIT lean floor (~95ns/call:
run_compiled_frame entry/exit + per-call Rust setup; node ~5ns inlines the comparator).
Beating ~95ns needs either Slice 2d (machine-code frame-build, kills the Rust stub —
helps compiled→compiled, NOT native→JS callbacks whose caller is Rust) or an
optimizing/inlining tier (Phase 2+ / far). NOT a quick win.

Object model already partially flat (ObjectBody repr(C) + flat value array + SlotMeta;
KelpHeap slice (a) 405e1280 + JIT inline property LOAD 7c0517b0 landed). Remaining
high-leverage bounded structural slices that build on landed work: **JIT inline property
STORE + write barrier**, **method-call IC inline** (Phase 3 d/e) — target prop-access
(13.6× node). These extend existing machine-code IC, not greenfield.

## Phase 2 execution — PupJIT direct calls (LOCKED 2026-06-17, user-directed)

**Verified current state:** compiled→compiled direct calls ALREADY exist
(`jit_prepare_direct_call` lib.rs:1677 + `emit_call`/`emit_direct_call_tail`
baseline.rs:1760; eligibility = ordinary bytecode, compiled, no async/gen/rest/
arguments/eval/derived-ctor, argc≤MAX_INLINE_ARGS). `jitDirectCalls` counts these.
fib/`cmp(a,b)` direct loops hit it (185ms, 3M direct calls).

**THE GAP (measured 2026-06-17, corrected):** the sort comparator DOES tier up +
run compiled on the committed path (`sortonly` JIT=1 = **32961 reductions**, was 159M
interp). So the floor is NOT "doesn't tier" — it's the **per-call Rust bridge itself**:
`run_bytecode_callable_committed` builds a full Rust `Frame` (register-window draw +
arg bind) PER CALL, + `dispatch_jit_sync_entry` re-resolves, + `run_compiled_frame`
entry/exit, + (sort) coerce_to_number. Measured per-call: committed-path compiled
callback ≈ **95ns**, compiled-caller direct call ≈ **61ns**, node ≈ ~5ns. sortonly
1461ms @ ~15.4M calls. So 2c (cut redundant per-call bridge: eligibility + target-
parts + frame built ONCE then reused) ceiling ≈ 95→~40ns ≈ **~1.5-2.5× on sort/
array-ops** — real but moderate. The MULTI-× lever to reach node is **Slice 2d**
(machine-code frame reserve+arg-bind+direct branch, kill the Rust stub: 61→~8ns).
string-ops separately: emitter can't compile its body (jitCompileAttempts=0) → bails
to interp — that's Phase 3/4 coverage, not Phase 2.

**SLICE 2c (first, highest-ROI, tractable Rust — NOT machine code):** native-callback
batched direct dispatch.
- New `Interpreter::run_compiled_callback_batched(context, callee, recv, &mut |emit_args| ...)`
  OR a reusable-frame handle: prepare the callee frame ONCE (eligibility =
  jit_prepare_direct_call's checks + compiled non-osr_only code), then per element:
  overwrite the arg registers + reset pc=0 + `run_compiled_frame`, skipping the
  per-call Frame rebuild. Fall back to `run_bytecode_callable_committed` per-call
  when callee ineligible OR a compiled run Bails (interp the body).
- Wire into sort_compare (array_prototype.rs:1291) first, then map/filter/forEach/
  reduce (the `array_callback_native_dispatch` site).
- GC safety (Stage A): the reused frame lives on the lean HoltStack (traced via
  FrameRoots); args written into frame registers before any safepoint; no GC-bearing
  value in a machine reg across a safepoint. Reclaim register window once at loop end.
- KILL SWITCH: `OTTER_PUP_DIRECT_CALLS=0` forces the current committed path. Default
  must stay correct; flip the batched path ON only after gates pass.
- GATES: cargo test otter-vm/otter-jit; diff.mjs 11/11; test262 built-ins/Array/**
  (sort/map/filter/forEach/reduce/find/every/some) + language/.../call failing-set
  identical JIT-off vs on vs flag-off; OTTER_GC_STRESS=32/64 on a sort+callback
  workload; **the open string-concat GC crash repro must not regress** (see
  [[bug_string_concat_gc_crash]]); measure sort/array-ops min-of-5 vs baseline.
- WHY split this from machine-code frame-build: Slice 2c is pure Rust (low risk,
  big benches), proves the lever; the machine-code emit of frame reserve+arg bind
  (eliminate the prepare-stub too) is Slice 2d, after 2c lands + a fresh context.

**NOTE:** open correctness blocker — heavy string concat in a called fn segfaults
([[bug_string_concat_gc_crash]], deterministic debug repro). A competitive engine
can't segfault on string building; fold its repro into every Phase-2 gate.

## Session state — 2026-06-17 (verified)

**ToPrimitive elision through parens + unary numeric ops (commit 10d79987).**
`expr_is_primitive` now recurses through `ParenthesizedExpression`, so a
parenthesized primitive operand of `+`/`==`/`<`/`<=`/`>`/`>=` (e.g. `(i*2)+(i*3)`)
skips its ToPrimitive; also applied at the unary `-`/`+`/`~` lowering (operand's
own ToNumeric still runs). Paren-heavy interp micro 18→14 coercion ops/iter,
~3920→3580ms (−8.7%). Conformance-neutral (24 expression dirs + Number + BigInt
failing-set byte-identical, stash+rebuild+diff; diff 11/11). Coercion-elision
lever now thoroughly mined. Remaining big levers: regex (67× node, worst bench),
string-ops (14.5×) — larger/riskier, warrant fresh context.

**ToNumeric elision on provably-numeric operands (commit 76ee8805).** Non-additive
numeric/bitwise binary ops (`- * / % ** & | ^ << >> >>>`) emitted
`ToPrimitive(number)+ToNumeric` per operand; `ToNumeric` over a Number/BigInt is
identity with no side effect. New `expr_is_numeric` (compiler/expr/binary.rs,
mirrors `expr_is_primitive`) elides both ops when the operand AST is provably
numeric (numeric/bigint literal, `++`/`--`, nested non-additive arith/bitwise,
unary `-`/`+`/`~`, through parens). Coercion-heavy interp micro 19→7 ToNumeric/
iter, ~5460→4190ms (−23%). Conformance-neutral: test262 coercion dirs +Number
+BigInt failing-set byte-identical to baseline (stash+rebuild+diff); diff 11/11;
574 vm tests. Loop-OSR (Phase 1.5) confirmed already working (top-level loop JIT
off 2291→on 346ms, 6.6×) — earlier "never tiers" note was stale. FOLLOWUP:
`unary.rs:482/610` ToNumeric elidable by the same rule.

**Map/Set O(n²) → O(n) (commit 420fbfcf).** MapBody/SetBody scanned `entries`
linearly per op; bulk build was O(n²) (set-build 10k→40k: 134→1675ms, 3.7×/
doubling). Added `#[pelt(skip)] FxHashMap<u64, SmallVec<[u32;2]>>` index (key
hash → live entry indices) to both bodies. Indexable GC-stable keys (number /
string-by-content / bool / null / undefined) probe the index + verify via
`key_matches`; symbol/object-identity keys keep linear scan (their `Gc` hash
moves under GC). `entries` stays append+tombstone so indices are stable; index
holds no `Gc` (collector never traces it, memcpy-relocates with body). Micro now
~2×/doubling: set 34→145ms, map-string 50→180ms. Gates: cargo test green; diff
11/11; test262 Map/Set/WeakMap/WeakSet identical JIT-off vs on (214/393/140/84,
0 fail); OTTER_GC_STRESS=32/64 mixed obj+int+string churn — zero corruption.

**Shipped this session (13 commits):** WhiskerIC inline ICs 3a–3e (LoadProperty /
StoreProperty + write barrier / direct method calls / upvalue load+store /
StoreUpvalueChecked + array-callback already-rooted), JSON.stringify fast object
path, JSON.parse escape-free key alloc cut. Two larger fixes tried + reverted
(documented): JSON dict-mode fast-shapes (regression), upvalue-spine pool
(neutral).

**otter-jit vs node (min/5 runs, updated 2026-06-17):** mandelbrot 1.7×, nbody
3.8×, typescript 3.2×, typed-array 5.6×, fib 6.5×, json 6.9×, **array-ops 7.8×
(12.4× → 7.8× — lean callback invoke)**, **sort 9.3× (13.5× → 9.3× — lean
comparator invoke)**, prop-access 13.6×, string-ops 14.5×, regex 67× (worst).
Session deltas: prop-access **932.8→534ms (−43%)**, json **2009→1642ms (−18%)**,
array-ops **1287→740ms (−43%)**, sort **2189→1530ms (−30%)**. Correctness floor
held throughout: diff 11/11 identical across interp/jit/jit-osr; test262
failing-set identical JIT-off vs JIT-on on every touched dir.

**Breakthrough — lean callback/comparator invoke (commits bb7d9fd8, c83238da).**
After the resolve_jit_code cache (32ebd28f), controlled isolation showed the
array-callback per-element cost is `run_callable_sync`'s wrapper + per-call stack
draw/return vs the lean Op::Call path. Extracted `run_callable_sync_inner`'s
committed bytecode tail into `run_bytecode_callable_committed(inner, …)` (takes
the re-entry stack instead of drawing one); the Array iteration loop
(map/filter/reduce/forEach/find/every/some) and sort/toSorted now, when the
callback is a plain bytecode closure, draw ONE reservation-stable stack + enter
the reentry guard once for the whole loop and invoke the callback directly per
element — skipping the per-call bound/proxy/native dispatch + draw/return. Each
invocation owns its stack (nested callbacks use a separate one). −35% array-ops,
−25% sort; far above the ~−2-3% estimate (the wrapper + draw/return were the bulk
of the diffuse gap). The methodology lesson: controlled isolation, not `sample`.

**Bounded-win phase is complete.** The cheap, low-risk per-op/per-alloc cuts are
exhausted. Reliable controlled profiling (not `sample`, which misattributed
twice) confirms the remaining tracked-bench gaps are **structural**:
- **array-ops (12×) / sort (13.5×)** — per-element callback is ~60 ns of *diffuse*
  function-call overhead (frame build + JIT dispatch + teardown via
  `run_callable_sync`), measured as forEach-empty 2.53 s vs plain-loop 1.10 s over
  24M iters; the body and element-read are cheap and the spine alloc is tiny (the
  pool was neutral). No single hotspot — only **per-element frame reuse** (build
  the callee frame once, rebind args + reset pc per element) or the optimizing
  tier captures it. Frame reuse is tractable (each `array_callback_native_dispatch`
  invocation owns its frame, so no re-entry aliasing) but the per-iteration frame
  *reset* correctness (registers→undefined except args, pc=0, cold state cleared)
  is subtle — a focused **attended** slice, gated on array-ops AND sort improving.
- **string-ops (14×)** — `charCodeAt` loop; needs KelpHeap string-body layout
  (Slice 4) to read code units by offset / inline the intrinsic.
- **json (6.8×)** — allocation/GC bound (`alloc_old` from birth); needs TideGC
  young-gen for short-lived parse results (Slice 5), not a ShellBuiltins tweak.
- **regex (37×, worst)** — separate engine; RippleRegex (Slice 7).

**Remaining levers are all large attended slices** (frame reuse / KelpHeap string /
TideGC young-gen / RippleRegex / the DiveJIT optimizing tier), each needing
careful iteration + hard gating. The autonomous bounded-cut loop has converged.

## Slice ladder

Order follows the plan's *minimal implementation sequence* (§5). Each slice is gated by the [verification contract](#verification-contract).

| # | Slice | Codename | Status | Entry blocked on |
|---|-------|----------|--------|------------------|
| 0 | Engine Lab — measurement, differential testing, progress scaffold | `OtterLab` | **done** (commit `98f460b3`) | — |
| 1 | Stable VM stack & frame descriptors | `HoltStack` | **done** (1b; 1e folds into Slice 2) | Slice 0 green ✓ |
| 2 | PupJIT direct calls + machine-code frame build | `PupJIT Calls` | **next** | HoltStack stable ✓ |
| 3 | Unified feedback vectors + complete ICs | `WhiskerIC` | **in progress** (3a load + 3b store + 3c method + 3d upvalue + 3e checked-upvalue/callback-trim done) | PupJIT direct calls |
| 4 | Hot heap layouts (Array/TypedArray/String/Closure) | `KelpHeap` | not started | WhiskerIC load/store/method/element |
| 5 | Production GC + precise safepoint stack maps | `TideGC` / `StoneMaps` / `ShellAlloc` | not started | KelpHeap; PupJIT direct calls shipped |
| 6 | Optimized builtin intrinsics | `ShellBuiltins` | not started | KelpHeap |
| 7 | First-class RegExp engine | `RippleRegex` | not started | ShellBuiltins string integration |
| 8 | Mid-tier optimizing compiler | `DiveJIT` | not started | HoltStack + WhiskerIC + StoneMaps + deopt model |
| 9 | Peak optimizer (deopt/inlining/scalar replacement) | `DeepDiveJIT` | not started | DiveJIT |
| 10 | JIT-friendly bytecode metadata + snapshots + code cache | `PebbleBytecode` | not started | WhiskerIC site ids, StoneMaps ids |
| 11 | Async / event loop / module runtime hardening | `TideLoop` | not started | — (parallelizable) |
| 12 | Debugger / profiler / observability | `Scout` | not started | tiers exist to walk |
| 13 | Multi-platform JIT, fuzzing, release hardening | `RaftRelease` | not started | tiers stable |

### Slice 0 — OtterLab task checklist

- [x] Repair workspace build after the `oxc 0.129 → 0.136` bump (see [Build repair](#build-repair-oxc-0136)).
- [x] Differential output-equality runner across Otter tiers (`benchmarks/diff.mjs`).
- [x] Machine-readable timing runner incl. forced-OSR tier (`benchmarks/bench.mjs` + `--only otter-jit-osr`).
- [x] `just` recipes: `bench`, `bench-osr`, `bench-diff`.
- [x] Progress scaffold (this file) with slice ladder, verification commands, baselines, rollback, code anchors, next-slice design note.
- [x] Per-CLI-run engine counter snapshots surfaced behind strict `OTTER_STATS=1` (IC hit/miss/install/disable, runtime budget call/turn/heap counters, JIT runtime/direct/fallback/stub counters, GC allocation/live/cycle/pause counters). Output is one machine-readable JSON line on stderr after `run`, so normal stdout and benchmark result equality are unchanged. Optimizer-only fields (`deopts`, optimized code size, optimizer compile latency) remain future work because no optimizer tier exists yet.

### Slice 1 — HoltStack

The execution stack was the concrete type `SmallVec<[Frame; 8]>`, threaded as an explicit `stack` parameter through **230 sites across 21 files** plus the JIT ABI alias `JitFrameStack` and the GC frame-roots provider. Pure stack discipline (`push`/`pop`/`len`/`is_empty`/`last`/`last_mut`/`get`/`get_mut`/`truncate`/`iter`) + O(1) indexing `stack[i]`. The defect `HoltStack` removes: a contiguous buffer **reallocates and moves every live frame** when it grows — fatal once a compiled callee holds its caller's frame/register address (Slice 2).

| Sub | Scope | Status |
|---|---|---|
| **1a** | Additive `holt_stack` module substrate, not wired. | **superseded by 1b** |
| **1b** | Full swap: `SmallVec<[Frame; 8]>` → `HoltStack` at all 230 sites, the `JitFrameStack` alias, the `trace_active_frame_roots` GC root provider, `resolve_jit_code`/`snapshot_frames` signatures. No fallback flag. | **done** |
| 1d | `HoltParkedSnapshot` for generator/async parking over `HoltStack` (currently parking still uses `Box<Frame>` as before — unchanged and correct). | deferred |
| 1e | `HoltFrameHeader` / `HoltFrameDesc` header↔value-slot split — the descriptor substrate Slice 2 (PupJIT direct calls) consumes. | planned (with Slice 2) |

**Design decision (no flag, no dual-mode).** An `OTTER_HOLT_STACK` runtime flag was rejected: a runtime toggle over a deeply-threaded *type* needs a dual-mode storage enum, which is a compatibility crutch the program forbids. `HoltStack` is the only stack; rollback is `git revert`.

**Storage: `#[repr(transparent)]` over `SmallVec<[Frame; 8]>`.** Explored and rejected two alternatives by measurement:
- *Segmented `Vec<HoltSegment>`* (stable across growth via segments): the per-access `segments[i/CAP].frames[i%CAP]` double-indirection regressed the interpreter hot path badly (fib-jit +110%, array-ops +85%). Wrong tradeoff.
- *Plain `Vec<Frame>`*: one-deref indexing fixed the interp path, but lost `SmallVec`'s inline-8 zero-alloc, so every ephemeral re-entry stack (Array callbacks, per-call JIT reentry) heap-allocated (controlled A/B: fib +31%, array-ops +27%).

`#[repr(transparent)]` over `SmallVec<[Frame; 8]>` keeps inline-8 zero-alloc for ephemeral re-entry stacks **and** is ABI/layout-identical to the bare `SmallVec` the JIT `<*mut JitFrameStack>::cast` reinterprets — so the wrapper is genuinely zero-cost. **Stability comes from reservation, not segmentation:** the three top-level dispatch stacks (`run_inner`, `run_module_init_inner`, `invoke_microtask`) are built with `HoltStack::with_dispatch_capacity()`, reserving `DEFAULT_MAX_STACK_DEPTH` (1024) frames in one heap buffer up front; the VM's stack-overflow guard fires before that is exhausted, so the buffer never reallocates and live-frame addresses are stable for Slice 2. Ephemeral re-entry stacks use `HoltStack::new()` (inline, may move — they hold no pinned addresses yet).

**Verification (2026-06-16, controlled).** `cargo test -p otter-vm -p otter-jit` 594 passed / 0 failed. `cargo clippy -D warnings` clean. `bench-diff` 11/11 identical across interp/jit/jit-osr. test262 interp-vs-JIT **zero failing-set delta**: `language/statements/function` 452/452, `generators` 266/266, `expressions/await` 22/22, `statements/try` 200p/6f (6 pre-existing, identical in both).

**Known perf regression (accepted, Slice-2 territory).** Controlled interleaved A/B (noise floor ±0.5% via A/A) vs the pre-swap SmallVec binary:
- **Interpreter (`OTTER_JIT=0`): neutral** — fib −1.9%; this is what Slice 1 is about.
- **JIT (`OTTER_JIT=1`): regression confined to the compiled-*call* bridge** — fib +31%, array-ops +27%, sort +14%, prop-access +13%. Compute-only JIT (mandelbrot/nbody/typed-array/json/string) is **neutral**, so compiled straight-line code is unaffected; only `jit_runtime_call` / `try_jit_fast_call`'s per-call re-entry path is slower. **Slice 2 (PupJIT direct calls) replaces exactly that bridge** with machine-code frame-build/direct-call, so the cost is erased there rather than papered over now. Tracked as the entry baseline for Slice 2.

---

## Slice 2 — `PupJIT Calls` subplan

> Active. Removes the per-JS-call Rust-bridge floor introduced as accepted regression by Slice 1b.

### Entry baseline (measured 2026-06-16, darwin arm64, 8 runs / 2 warmup, fresh release binary)

| script | otter-jit (ms) | Slice-2 target |
|---|---|---|
| fib.js | **277.2** | ~213 (erase the +31% bridge regression) |
| array-ops.js | 792.0 | recover toward pre-1b |
| prop-access.js | 668.1 | recover toward pre-1b |
| sort.js | 1476.3 | recover toward pre-1b |

`fib.js` is the headline probe: a pure-integer self-recursive call with zero allocation in the
callee body, so its cost is *entirely* the call bridge — the cleanest signal for direct-call work.
Compute-only scripts (mandelbrot/nbody/typed-array) stay the neutral control set.

### Current compiled-call path (the floor being removed)

Per `Op::Call` in compiled code (`baseline.rs::emit_call` → `jit_call_stub` →
`Interpreter::jit_runtime_call` lib.rs:1448):
1. machine→Rust extern-C hop (`blr` into `jit_call_stub`);
2. eligibility checks (`try_jit_fast_call` lib.rs:1536): bytecode target, simple signature, compiled
   body installed (cached resolve);
3. `enter_sync_reentry` depth guard;
4. `run_jit_fast_call_committed` (lib.rs:1599): build upvalue spine, coerce `this`, `draw_registers`,
   **construct a fresh `inner = HoltStack::new()` + push the callee frame**, bind args from the rooted
   caller window, `run_compiled_frame` → `enter_at` **rebuilds a fresh `JitCtx`** (reads regs-ptr /
   self-closure / this) → `transmute` entry → machine code; on return pop + `reclaim_registers`,
   write completion to `dst`.

### Two findings from inspection that shape the decomposition

1. **Latent GC rooting gap on the fast path (correctness, not just perf).** `dispatch_loop`
   (lib.rs:4408) registers a `trace_active_frame_roots` provider for *the stack it is handed* and
   traces every frame on it; `run_compiled_frame` registers **nothing**. The compiled-fast-call
   callee runs on a private `inner` stack that no provider covers, so during the callee's compiled
   body its own register window is **not a GC root**. Harmless for `fib` (no allocation) but a
   use-after-free risk for any allocating compiled callee that triggers a scavenge while a young
   pointer lives only in an `inner`-frame register. The slow path does not have this gap — it runs on
   the shared, already-registered stack via `run_callable_sync_already_rooted`.

2. **Same-stack push needs a reservation-stable host stack.** A compiled caller holds `x19` =
   pointer into its own frame's register array on its host stack. Pushing the callee frame onto that
   same stack is only sound if the stack never reallocates (which would move the caller's frame and
   dangle `x19`). The top dispatch stacks are built with `with_dispatch_capacity()` (1024 reserved,
   overflow guard fires first) and are stable; **ephemeral reentry stacks** (`run_callable_sync_inner`,
   `array_ops`/`async_ops` helpers) are `HoltStack::new()` (inline-8) and may spill/move. Compiled
   code runs on **both** kinds (tier-up/OSR happens inside whichever `dispatch_loop` is active), so a
   same-stack call path must guarantee the host stack is reservation-stable everywhere it can fire.

### Decomposition (one sub-slice = one commit, each independently revertible + measurable)

- **2a — Same-stack compiled callee on the reservation-stable HoltStack (Rust-only, no arm64). Done in current code.**
  Runs the fast compiled→compiled callee **on the caller's stack in place** when that stack is
  reservation-stable (every `HoltStack::new()` reserves `DEFAULT_MAX_STACK_DEPTH`, and pooled re-entry
  stacks preserve that reservation): push the callee frame at the top, `run_compiled_frame` at the new
  index, pop on return; `Threw` truncates back to the caller; `Bailed` resumes the interpreter on the
  appended frame (its `return_register = None` bounds the resume to that frame, never unwinding the
  caller). There is no private-stack fallback in the current code. Effects:
  - **Closes a latent GC rooting gap** (correctness): `run_compiled_frame` installs no root provider;
    the old private `inner` stack was traced by nothing during the callee's compiled body, so an
    allocating compiled callee that triggered a scavenge could free a young pointer living only in an
    `inner`-frame register. The same-stack callee is now covered by the enclosing `dispatch_loop`'s
    provider; the fallback path registers its own.
  - **Establishes the substrate 2b requires**: machine-code frame-build can only append callee frames
    to a stack it knows will not reallocate — it cannot allocate a fresh `inner` stack in emitted code.
  - **Measured (controlled A/B, runs=12, A/A noise floor 0.0 ms): fib-jit 277.0 → 283.3 (+2.3%)**;
    array-ops / prop-access / sort neutral; compute-only set neutral; diff 11/11. The +2.3% is a
    cache-locality cost of threading recursion through one deep reserved buffer instead of fresh inline
    re-entry stacks, with **no perf upside on its own** — the bridge's dominating cost (extern-C hop +
    per-call Rust frame-build + `JitCtx` rebuild) is untouched by 2a and is exactly what 2b removes.
    By the **slice-1b precedent** (a documented substrate regression accepted ahead of its payoff), 2a
    is accepted as substrate for 2b rather than optimized in isolation.

- **2b — Machine-code frame build + direct branch to the callee's compiled entry (arm64). Started.** The
  sub-slice that erases the extern-C hop and recovers fib past baseline. Prereq surfaced during 2a
  inspection: a `Frame` is a Rust struct (register `SmallVec`, `UpvalueSpine`, `this_value`, cold idx,
  async/generator fields) that **cannot be allocated/initialized in emitted machine code as-is** — 2b
  needs the **slice-1e frame-descriptor split** (`HoltFrameHeader` / `HoltValueSlots` / `HoltFrameDesc`)
  so the caller can reserve a frame and fill its value slots from emitted code while the Rust-managed
  header fields are set through a thin reservation helper. Emission plan (after 1e lands): guard callee
  kind + cached resolved code-ptr (monomorphic inline cache on the call site), reserve the callee frame
  on the `HoltStack`, init value slots to `undefined` then publish (two-phase, no allocation while a
  partial frame is GC-visible), bind args/receiver, branch to the callee's compiled entry; on return
  write the result to `dst` and pop. Cold/ineligible callees keep the Rust bridge. **GC Stage A:** every
  live GC-bearing `Value` is spilled to its frame slot before any safepoint; the result lives in a
  register only between the callee's return and the `dst` store (no safepoint in that window). Do
  **not** start arm64 emission until the 1e frame descriptor + the appended-frame ABI are pinned.

  Current work in this patch:
  - Added `HoltCallReservation`: an unpublished call-frame owner. GC cannot see
    the frame until `publish`, and `publish` returns a `HoltFrameDesc`.
  - Added `HoltValueSlots` / `HoltFrameDesc`: raw value-slot pointer + length,
    plus stable `HoltStack` frame index. Safe Rust still indexes through
    `HoltStack`; emitted code gets explicit metadata instead of depending on
    `Frame` layout.
  - Rewired `run_jit_fast_call_committed` to publish callee frames through this
    reservation path instead of naked `stack.push(new_frame)`.
  - Added `JitFunctionCode::entry_addr()` and `JitDirectCallPlan`, so the VM can
    distinguish a compiled callee that is actually direct-branch capable from a
    black-box code object. `BaselineCode` now exposes its main entry address.
  - `JitCtx` is now machine-constructible: direct callees copy plain
    pointers/scalars and share a caller-owned error slot instead of initializing
    `Option<VmError>` in emitted code.
  - Added `jit_prepare_direct_call` / finish / abort VM ABI. Prepare publishes a
    traced callee frame and returns entry/regs/self/this/frame index; finish
    pops/reclaims and stores the result; bail finish resumes the interpreter in
    the callee frame.
  - Replaced compiled `Op::Call` emission on arm64: cold/ineligible callees bail
    to the interpreter; eligible compiled callees build a nested `JitCtx` on the
    native stack and `blr` directly to the callee entry. The old generic
    `jit_call_stub` was removed.
  - Reduced `VmError` layout from 48 → 32 bytes by boxing rare structured error
    payloads (`JsonError`, `Coded`, `TypeMismatchAt`) and removing
    `Deserialize` from the VM-internal error enum.

  Intermediate benchmark after direct-call emission (2026-06-16, darwin arm64,
  release, Otter tiers only, `node benchmarks/bench.mjs --only otter,otter-jit,otter-jit-osr --runs 3`;
  min ms over 3 runs / 2 warmup, process startup included):

  | script | otter | otter-jit | otter-jit-osr |
  |---|---:|---:|---:|
  | array-ops.js | 2561.3 | 1260.3 | 1260.0 |
  | fib.js | 1909.7 | 255.3 | 255.7 |
  | json.js | 1996.8 | 1946.9 | 1990.1 |
  | mandelbrot.js | 1568.8 | 56.0 | 55.5 |
  | nbody.js | 1282.7 | 149.1 | 182.4 |
  | prop-access.js | 3155.8 | 932.8 | 927.3 |
  | regex.js | 2194.0 | 2188.8 | 2200.2 |
  | sort.js | 4253.2 | 2204.6 | 2205.4 |
  | string-ops.js | 546.1 | 547.2 | 552.1 |
  | typed-array.js | 3032.3 | 214.1 | 214.2 |
  | typescript-sample.ts | 2361.8 | 181.7 | 182.2 |

  Direct-call signal: on the same 3-run benchmark shape, `fib.js` improved from
  the previous intermediate `otter-jit=274.8ms` to `255.3ms` after removing the
  generic `jit_call_stub` path for eligible compiled calls. `prop-access`,
  `regex`, and `string-ops` remain dominated by the next slices (WhiskerIC /
  ShellBuiltins / RippleRegex), not the call bridge.

  Correctness gate on the same release binary: `node benchmarks/diff.mjs` → 11/11
  identical across `interp` / `jit` / `jit-osr`.

- **2c — Eligibility widening + tail/argc shapes.** Extend the direct path to the remaining
  fast-binding argc shapes and (if measured to pay) a self-tail-call loopback, keeping cold cases on
  the bridge. Gate: full call/closure/generator/async/super/try parity + bench set.

### Eligibility (conservative, unchanged from the bridge's `try_jit_fast_call` gate)

Ordinary bytecode function/closure; PupJIT code installed; not async/generator/async-generator; no
`arguments`/rest; no direct eval; not a derived constructor; no captured `new.target`/derived-`this`/
inherited eval env; no host/native/capability callee; argc within the fast-binding shape; no active
protected/finally region on the caller frame.

### Files in scope

`crates/otter-vm/src/{lib.rs, call_ops.rs, holt_stack.rs, jit.rs}` (2a — pool + same-stack),
`crates/otter-jit/src/baseline.rs` + `crates/otter-vm/src/jit.rs` ABI (2b — emission). Naming for new
pieces stays in the Otter vocabulary (e.g. `HoltStackPool` / `holt_pool`).

### Primary risks

Dangling caller register pointer on stack growth (mitigated by the reservation-stable invariant +
overflow guard); GC tracing a partially-initialized appended frame (two-phase publish, debug
initialized-slot assertion); bail-path PC/unwind bounded to the appended frame, not the caller;
generator/async callees must continue to miss the fast path. Rollback = `git revert` of the sub-slice
commit (no flag).

---

## Slice 3 — `WhiskerIC` subplan

> Active. Makes property/method/element operations hot-path native, removing the
> per-access Rust stub the baseline currently pays. Entered while Slice 2b is in
> flight because `prop-access.js` is dominated by property runtime stubs, not the
> call bridge.

### 3a — Data-driven `LoadProperty` inline cache (self-patching cell). Done.

**Defect this removes (correctness-of-optimization, measured).** The previous
inline-load path was **baked at compile time** (`bake_inline_property_loads` read
the live IC tables at tier-up and stamped a fixed shape/offset into the emitted
guard). A function that tiers up via **loop OSR off an *earlier* loop** compiles
its *whole* body while the later loop's property ICs are still cold
(`entry_count == 0`), so those sites baked nothing and stayed on the runtime stub
**for the entire program**. `prop-access.js` is exactly this shape: the
top-level function OSR-compiles off the `new Point()` init loop, so the compute
loop's `p.x` / `p.y` / `p.tag` loads (3 per iteration × 1.5M) never inlined —
`OTTER_JIT_TRACE` showed all three sites `count!=1 (entry_count=0)` at bake.

**The fix — runtime-filled WhiskerIC cell, not compile-time bake.** Each
`LoadProperty` op gets one `WhiskerLoadCell { shape: u32, value_byte: u32 }` in a
stable per-function buffer owned by `BaselineCode` (boxed slice; emitted code
bakes each cell's address). Emission: tag + GC-type-tag guard, then
`cbz cached_shape → miss` (empty cell), `cmp receiver_shape, cached → miss`, else
`ldr [obj + cached_value_byte]`. On a miss the shared stub
(`jit_load_prop_stub`, now taking the cell address) runs the normal IC; when the
site resolves to a **warm, monomorphic own-data inline slot** the VM returns a
packed `(shape_offset, value_byte)` fill (`Interpreter::whisker_load_cell_fill`)
which the stub writes into the cell (`value_byte` before `shape`, so the guard
never sees a live shape with a stale offset). The next execution inlines — so a
site that was cold at tier-up **self-patches once warm**, which the static bake
could never do. Cell holds only compressed offsets (no GC pointers → no tracing;
a shape offset is a stable token — shapes are immortal and pinned in old space).
The object pointer is recomputed from the rooted frame slot each load, never held
across a safepoint (no allocation/call on the inline path). The compile-time
`bake_inline_property_loads` / `JitInlineLoad` / `JitInstrView.inline_load` are
deleted; `JitFunctionView` gains a single `object_shape_byte` `#[repr(C)]`
constant for the guard.

**Files:** `crates/otter-jit/src/baseline.rs` (cell struct, `BaselineCode`
backing buffer, `LoadProperty` emission, `jit_load_prop_stub` arg),
`crates/otter-vm/src/{property_dispatch.rs (whisker_load_cell_fill + return
type), jit.rs (DTO), executable.rs (object_shape_byte), lib.rs (drop bake)}`.

**Measured (2026-06-16, darwin arm64, release).** `prop-access.js`
`OTTER_STATS=1`: `jitRuntimePropertyStubs` **8,994,953 → 4,497,961** (−50%);
`propertyLoadHits` (interpreter-side IC, i.e. loads still on the stub)
**4,502,235 → 5,243** (loads now inline after warmup). Bench `otter-jit`
**prop-access 932.8 → 829.1 ms (−11%)**; `fib` 255.3 → 256.0 ms (neutral — no
property loads); all other scripts neutral. Remaining `prop-access` stubs ≈ 3/iter
= 2 method calls + 1 store — the 3b/3c targets.

**Gates.** `cargo test -p otter-vm -p otter-jit` (otter-jit 17, otter-vm 574, +
layout/compile-fail) all pass. `cargo clippy -p otter-vm -p otter-jit -D warnings`
clean; `cargo fmt` clean. `node benchmarks/diff.mjs` **11/11 identical** across
interp/jit/jit-osr. test262 failing-set **identical JIT-off vs JIT-on** on
`language/expressions/property-accessors` (21/21), `built-ins/Object/defineProperty`
(1131/1131), `language/statements/class/elements/private` (186/187, the 1 fail
pre-existing + identical both tiers). `OTTER_GC_STRESS=64/128` diff: 10/11 — the
sole miss is `json.js` panicking in **the interpreter oracle** (`OTTER_JIT=0`,
`crates/otter-gc/src/space.rs:187 "fresh old-space page cannot be full"`), a
**pre-existing GC-stress allocator bug** unrelated to this JIT-only slice
(reproduced with JIT fully disabled).

### 3b — Data-driven `StoreProperty` inline cache + value-gated barrier. Done.

Mirrors 3a for the **existing-own-data** store. Each `StoreProperty` op gets a
`WhiskerIcCell` (the cell struct is now shared load/store) in a second
`BaselineCode` buffer. Emission: tag + GC-type + cell-shape guard, write the
value into the in-object slot, then a **value-tag-gated write barrier** —
`tag >= TAG_PTR_OBJECT (0x7FFC)` calls `jit_write_barrier_stub`
(→ `Interpreter::jit_runtime_write_barrier` → `GcHeap::record_write`, marking the
parent header's card for the old→young edge); primitive stores skip it, so the
common int store stays fully inline. The stub (`jit_store_prop_stub`, now taking
the cell) self-patches the cell from a packed fill
(`Interpreter::whisker_store_cell_fill`) only for a **warm single-entry
`ExistingOwnDataStore`** IC on an inline slot — add-transition stores mutate the
shape and stay on the stub. Sound because the kept shape guard implies the
captured writable-data slot (a shape encodes per-slot flags + key). No
allocation on the inline path (the barrier marks a card, never allocates) → no
safepoint; the object pointer is recomputed from the rooted frame slot each
store.

**Measured (release).** `prop-access.js` `propertyStoreHits` (interpreter-side
store IC, i.e. stores still on the stub) **1,499,999 → 49**;
`jitRuntimePropertyStubs` **4,497,961 → 2,998,011** (cumulative 3a+3b:
**8,994,953 → 2,998,011, −67%**). Bench `otter-jit` **prop-access 829 → 805 ms**
(cumulative entry-baseline **932.8 → 805 ms, −14%**); `fib`/others neutral.

**Barrier validation (the GC-critical part).** `prop-access` stores only ints,
so it never exercises the barrier path. A dedicated differential workload
(`Box.ref = { … }` — an old object's slot repeatedly set to a freshly-allocated
young object, hot enough to tier up the setter) is **bit-identical** across
`interp` / `jit` / `jit + OTTER_GC_STRESS=32` / `=64` (`10396000000`), and
`OTTER_STATS` confirms the inline store fired with pointer values (98 stub hits
out of 2M stores). If the barrier were missing, stress would free the young
children and corrupt the result. `prop-access` itself also stays correct under
`OTTER_GC_STRESS=64/128` with JIT on.

**Gates.** Unit tests (otter-jit 17, otter-vm 574) pass; clippy/fmt clean;
`diff.mjs` 11/11 identical; test262 failing-set identical JIT-off vs JIT-on on
`built-ins/Object/defineProperty` (1131/1131), `language/expressions/assignment`
(804/818, fails pre-existing + identical both tiers),
`class/elements/private-methods` (5/5).

### 3c — Direct method calls (`CallMethodValue` → IC-resolve + direct branch). Done.

`p.bump()` / `p.dist2()` were the dominant remaining `prop-access` cost (~3.0M/run
through `jit_call_method_stub` → full `run_callable_sync` re-entry). Now a method
call IC-resolves the callee (`resolve_method_ic` — the same monomorphic load-IC
the interpreter method path uses, site shared via `property_ic_site`) and
**direct-branches to its compiled entry**, reusing the entire Slice-2b ABI:
`jit_prepare_direct_method_call` publishes a callee frame bound `this = recv`
(via `bytecode_call_target_parts(method, recv, …)`), and the emitted
`emit_method_call` runs the **same dispatch tail** as `Op::Call`
(`emit_direct_call_tail`, factored out of `emit_call`) — build callee `JitCtx`,
`blr` entry, finish/bail/abort.

Key difference from `Op::Call`: an ineligible resolution (status 2) falls back to
the **in-place** full method-call stub (`jit_call_method_stub`), *not* a bail — a
native / polymorphic / accessor method in a hot loop must keep running compiled
rather than deopt the whole frame to the interpreter. Eligibility is the Op::Call
set plus `makes_function` (the method path carries no caller register to re-root a
named-function SELF closure post-allocation, so those use the fallback).
`prepare_jit_direct_call_frame` took `callee_reg: Option<u16>` (`None` on the
method path); GC rooting is identical to the proven Op::Call frame build.

**Measured (release).** `prop-access.js`: `jitDirectCalls` 0 → **2,998,000**
(the 2 method calls/iter now direct-call); `jitRuntimePropertyStubs`
**2,998,011 → 11**; `otter-jit` **805 → 525 ms**. Cumulative across 3a+3b+3c:
**prop-access entry-baseline 932.8 → ~525–540 ms (−42 to −44%)**;
`jitRuntimePropertyStubs` **8,994,953 → 11**. fib and all other scripts neutral.

**Gates.** Unit tests (otter-jit 17, otter-vm 574) pass; clippy/fmt clean;
`diff.mjs` 11/11 identical across interp/jit/jit-osr; `prop-access` and the
method+barrier workload bit-identical under `OTTER_GC_STRESS=32/64/128`; test262
failing-set identical JIT-off vs JIT-on on `language/expressions/call` (83/92),
`expressions/super` (93/94), `statements/try` (200/206),
`object/method-definition` (303/303), `Array/prototype/map` (214/216).

### 3d — Inline upvalue (captured-binding) load/store. Done.

`LoadUpvalue` / `StoreUpvalue` were the last broad always-stub class (every
captured-variable read/write re-entered Rust). Now inlined: a new `JitCtx`
`upvalues_ptr` (the frame's `Box<[UpvalueCell]>` data base, set in `enter_at` and
plumbed through the direct-call path via `JitPreparedDirectCall::upvalues_ptr` →
`ctx.direct_upvalues_ptr` → callee ctx) lets emitted code read the 4-byte cell
handle at `[upvalues_ptr + idx*4]`, decompress (cells are **old-space, immobile**
— GC-trivial), and access the cell's single `Value`. Load misses to the stub on
a TDZ hole or a `0` base; store inlines only the **primitive** case (a non-pointer
value into an old cell needs no write barrier) and misses pointer values to the
barriered stub — same value-tag gate as 3b.

**Measured.** Closure-counter micro (`sum += x; n += 1` captured): upvalue stubs
**~80,000,000 → 2**, −7.5% wall-clock. `array-ops` `jitRuntimePropertyStubs`
**2,599,037 → 199,050** (−92%; the residual is the deferred dense-array
`StoreElement`). Tracked benches otherwise neutral — they are callback-dispatch /
builtin bound, not upvalue bound (this is broad cleanup, not a headline bench
win). diff 11/11 identical.

**Gates.** Unit tests (otter-jit 17, otter-vm 574) pass; clippy/fmt clean; test262
failing-set identical JIT-off vs JIT-on on `arrow-function` (343/343),
`statements/let` (145/145), `const` (138/138, TDZ), `generators` (289/290),
`for-of` (742/752, per-iteration bindings). Primitive-upvalue closure
bit-identical under `OTTER_GC_STRESS=32/64`. (A pointer-upvalue alloc-storm under
`OTTER_GC_STRESS=32` SIGSEGVs in **both interp and JIT** — pre-existing GC-stress
fragility in heavy young-allocation / commonjs load, unrelated to this slice.)

### 3e — Callback-dispatch trim + inline `StoreUpvalueChecked`. Done.

Profiling the `forEach` callback (`sample`) showed the per-element cost is the
`run_callable_sync` frame build (dominant) plus the arrow's captured-`let` store
(`s += x`) hitting `StoreUpvalueChecked` through the `jit_delegate_op` bridge
(with eager `VmError` drops on the success path). Two bounded fixes:
- **Array callback loop → `run_callable_sync_already_rooted`** (`array_prototype.rs`
  `array_callback_native_dispatch`): the loop always runs nested under the
  forEach/map/… native, which already pushed an `ExtraRoots`, so the per-element
  duplicate push/truncate is dead weight (the heap's `same_source` walk skips it
  anyway). Drops one `Vec` push/truncate per element.
- **Inline `StoreUpvalueChecked`** (the TDZ-checked captured-binding store): like
  the 3d `StoreUpvalue` inline but reads the cell first and misses to the
  delegate bridge on a hole (→ `ReferenceError`); inlines only the primitive
  store (no barrier into the old-space cell), pointer values → bridge. Completes
  the upvalue inline coverage (plain + checked).

**Measured.** `array-ops.js` `otter-jit` **1287 → ~1203 ms (−6.5%)** (repeat runs
1.18–1.20 s); `sort` neutral (it uses a different call site — comparator at
`array_prototype.rs:1312`, no captured-`let`); other benches neutral; diff 11/11.

**Gates.** Unit tests green; clippy/fmt clean; test262 failing-set identical
JIT-off vs JIT-on on `Array/prototype/{forEach 190/190, map 214/216, filter
240/242, reduce 520/520}`, `statements/let` (145/145), `assignment` (804/818).
Captured-`let`-store closure bit-identical interp vs JIT (a GC-stress alloc-storm
SIGSEGVs in **both** — pre-existing).

## Slice 6 (early) — `ShellBuiltins`: JSON.stringify fast object path. Done.

`sample` of `JSON.stringify` (json 2009ms, JIT≈interp — native-bound) showed
~47% of stringify in object key enumeration + per-key value resolution:
`ordinary_string_key_entries` (42), `lookup_own` (23), `body_offset_of` (22),
`key_order` (22). The serializer enumerated keys (with their slot offsets) then
**threw the offsets away** and re-resolved each key through `[[Get]]`
(`lookup_own` + `body_offset_of` — key hash + shape walk) per property per object
— redundant across the 5000 identically-shaped records.

Fix: `serialize_json_object_into` now collects `(key, Some(slot))` for an ordinary
object with no replacer list and **no enumerable accessors**
(`Properties::enumerable_string_data_offsets` → `None` bails to `[[Get]]` on any
accessor), and reads each value straight from its flat data slot
(`object::data_value_at`) — **re-validating the holder's live shape-id per key**
so a nested `toJSON` that mutates the holder falls back to the observable
`[[Get]]` (behaviourally identical to spec). Replacer / proxy / typed-array /
module-namespace / String-wrapper keep the `[[Get]]` path.

**Measured.** `json.js` `otter-jit` **2009 → 1911 ms (−4.9%)** (min/5); stringify
micro −~12%. diff 11/11.

**Gates.** Unit tests green; clippy/fmt clean; test262 `built-ins/JSON` 163/165,
**0 failed** (no regression possible); 6 hand edge-cases (getter runs,
toJSON-mutates-holder, replacer array + function, integer-key order,
non-enumerable skip, indent) byte-match node. JIT-independent change (native
serializer), so JIT-off ≡ JIT-on.

## Slice 6 (early) — `ShellBuiltins`: JSON.parse object-key allocation cut. Done.

`sample` of `JSON.parse` showed it is **allocation + GC bound** (alloc ~496,
scavenger/collect_full/marking/sweep ~150 of ~700 leaf samples); `finish_builder`
/ `object::set` is only ~22. Confirmed every object/array/string allocates via
**`alloc_old`** (old-gen from birth — a TideGC/Slice-5 limitation), so a parse's
5000 short-lived records fill old space and trigger full GCs.

Bounded fix landed: `read_object_key` minted a GC `JsString` (via `read_string`)
only to `to_lossy_string` it into a Rust `String` and drop it — a wasted old-gen
allocation per key. The escape-free common case (`"id"`, `"name"`, …) now builds
the `String` straight from the input slice (SWAR escape scan + `from_utf8`),
skipping the `JsString`. Escape/control/error keys defer to the unchanged
`read_string` path (identical decoding + lossy-surrogate behaviour).

**Measured.** `json.js` `otter-jit` **1911 → 1629 ms (−14.8%)** — far above the
raw key-alloc cost, because each skipped `JsString` was an old-gen allocation
driving full GCs (30k keys/parse × 40). Cumulative JSON (stringify + this):
**2009 → 1629 ms (−19%)**. diff 11/11.

**Gates.** Unit tests green; clippy/fmt clean; test262 `built-ins/JSON` 163/165
**0-failed**; 9 hand key edge-cases (`\n`/`\uXXXX`/surrogate-pair/empty/duplicate/
multibyte-UTF8/`__proto__`/lone-surrogate) match node except the lone-surrogate
lossy case, which is the **unchanged escape path** (pre-existing). JIT-independent.

### Larger JSON.parse finding — dict-mode fix TRIED + REVERTED (negative result)
`object::set` (used by `finish_builder`) sets `body.shape = null` → JSON-parsed
objects are **dictionary-mode** (per-object `dictionary_keys` Vec + key-String
clone, no shape sharing) unlike class instances which tier into fast shapes.
Building parsed objects fast-shaped was **fully implemented** (a
`object::push_parsed_data_property` helper building shapes via
`ShapeRuntime::child_with_roots`, threaded `Option<&mut ShapeRuntime>` through the
parser + a `NativeCtx::heap_and_shapes_mut` disjoint-borrow accessor) — **correct
(diff 11/11) but a throughput regression**: json.js **1629 → 1785 ms (+9.6%)**,
parse micro +10%. The per-key shape-transition compute (`child_if_cached` hash
lookups + double `body_offset_of` + `set_with_shape`) exceeds the
dict-allocation savings; fast shapes help *downstream property access* on parsed
objects, which json.js never exercises (it only reads `back.length`). **Reverted.**
Lesson: dict-mode is not a parse-throughput win — only helps access-heavy
consumers of parsed JSON, which no current bench measures.

### Array/sort callback per-element cost — profiled (2026-06-17), fix is an attended slice

`sample` of a hot `forEach(x=>{s+=x})` (after 3d/3e inlined the upvalue ops) shows
the per-element cost is no longer property stubs but **frame teardown /
allocation**: `Interpreter::return_stack` ~106 self-samples, `alloc` ~18,
`call_ops` ~32, `build_upvalues_for_count` ~3. The callback **does** compile (fid
tiers up) and runs through `dispatch_jit_sync_entry`'s fast terminal path
(`inner.pop()` → `reclaim_registers` → `return_stack`). With `regs ≤ 8` the
registers are inline (nothing to reclaim), so the dominant repeated cost is the
**per-call upvalue-spine `Box<[UpvalueCell]>`**: allocated by the
`body.upvalues.clone().into_boxed_slice()` in `bytecode_call_target_parts`
(`call_ops.rs`) and freed when the terminated frame drops. 12M calls × (alloc +
free) of a tiny capturing-closure spine.

**WIN landed (commit 32ebd28f): resolve_jit_code single-entry cache** — the
sync-reentry resolver did a `jit_code` FxHashMap lookup + `Arc` clone per call;
added the `jit_code_cache` fast path the 2b direct path uses. array-ops
1170→1135 (−3%), sort 2189→2094 (−4.3%), forEach micro 3.19→2.95. Found by
**controlled isolation** (same closure: manual `for(i) g(a[i])` [Op::Call/2b] =
1.99s vs `a.forEach(g)` = 2.90s now / 3.19 before, over 24M).

**Remaining gap ≈ 38 ns/call (forEach 2.90 vs floor 1.99) is genuinely diffuse.**
Verified the candidates are individually small: `enter/leave_sync_reentry` is a
counter (~1ns); `draw_stack`/`return_stack` are pooled (cheap after warmup, no
1024-frame realloc); the cost is spread across the per-call frame build + the
`run_callable_sync_inner` wrapper checks (bound/proxy/class/native — a few tag
checks + a `call_native` payload read per call) + the Rust `run_compiled_frame`
JitCtx build (vs 2b's machine-code JitCtx). **No single >15ns piece** — 2b is
faster because it is *emitted/lean*, not because it reuses frames (2b builds a
fresh frame per call too, so frame-reuse is NOT the lever). Capturing the rest
needs a **lean-invoke restructure**: pre-check the callback once as a plain
bytecode closure, then per element invoke it on ONE persistent inner stack via a
lean path that skips the wrapper checks + per-call enter/leave + draw/return —
i.e. a faithful replica of `run_callable_sync_inner`'s bytecode arm, integrated
into the shared spec loop (`array_callback_native_dispatch`, used by map/filter/
reduce/forEach/find/every/some). Partial win (~−2-3% array-ops); the *full* fix
is callback inlining in the optimizing tier (Phase 8). The lean-invoke is an
attended slice (the shared closure-based loop + bytecode-path-replica correctness
need careful iteration); two single-piece attempts on this lever already failed.

---

**Option 1 (length-keyed spine pool) TRIED + REVERTED (2026-06-17): NEUTRAL, no
win.** Fully implemented cleanly: `upvalue_spine_pool: Vec<UpvalueSpine>` + a
static `draw_upvalue_spine(pool, len)` (pops a same-length box / allocs
`vec![null; len]`) + `reclaim_upvalue_spine` (nulls cells, pools) called at all 4
`reclaim_registers` sites + `bytecode_call_target_parts` given a `&mut pool` param
(no `&mut self` needed — disjoint from `&self.gc_heap`; all 8 callers + the
disjoint borrows compiled clean). Tests/diff green. But **array-ops 1170→1187,
sort 2189→2184, fib 266→280 — neutral-to-slightly-worse, zero improvement.** The
pool cycles (build draws, fast-path reclaim fills), so the per-call upvalue-spine
`Box` alloc is **not** the real cost — the earlier `return_stack ~106` attribution
was misread. Reverted per the gate ("must improve array-ops/sort or revert").

**Corrected understanding:** the per-element callback cost is **diffuse frame /
re-entry management** (draw_stack/push/dispatch/pop/return_stack per element +
`this`-coercion + arg-bind), not a single hot allocation. Two profile-guided
single-allocation fixes (dict-mode shapes, spine pool) both failed to move the
benches — the cost is spread across the `run_callable_sync` per-element setup, so
only a structural change captures it:
- **Per-element frame reuse** in the array builtins (build the callee frame ONCE,
  rebind args + reset pc per element) — the genuinely-impactful but high-risk
  lever (frame-reuse correctness on the most-used builtins, mid-loop tier-up,
  throw, recursion). Needs an attended, heavily-gated slice. This is now the ONLY
  remaining array-ops/sort lever; the cheap single-alloc cuts are exhausted.
- `Rc<[UpvalueCell]>` spine (clone→refcount) is also still possible but, given the
  spine pool was neutral, unlikely to pay off either — the spine isn't the cost.

### 3f — next (not started)
- **The real array-ops/sort lever remains untouched**: `run_callable_sync` builds
  a fresh callee frame (incl. a per-call upvalue-spine clone in
  `bytecode_call_target_parts`) for every element — ~136 ns/call overhead
  (forEach 1.79 s vs an inline plain loop 0.16 s for the same 12M iterations). A
  lean per-element invocation that reuses the callee frame/stack across iterations
  is the headline win (hits map/filter/reduce/forEach/sort); larger + higher-risk
  (frame-reuse correctness), so scoped as its own slice.
- Direct-prototype `LoadProperty` inline; dense `StoreElement` inline (deferred —
  length/dirty/sparse/frozen semantics).

## Verification contract

Run as much of this as practical for a slice; record results in the slice's commit / this file.

**Always:**
```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test -p otter-vm -p otter-jit          # touched-tier unit tests
just release                                  # cargo build --release -p otter-cli
just bench-diff                               # output equality across Otter tiers (exit 0 required)
```

**Behavior / perf:**
```bash
just bench           # cross-runtime timings → benchmarks/results/latest.{md,json}
just bench-osr       # include forced-early-OSR tier
```

**test262 (no JIT/interpreter failing-set delta for touched dirs):**
```bash
just test262-filter "<area>"                          # e.g. Array, JSON, language/expressions
# JIT off vs on must yield the same failing set:
OTTER_JIT=0 cargo run -p otter-test262 -- run --filter "<area>" --output /tmp/t262-interp.json
OTTER_JIT=1 cargo run -p otter-test262 -- run --filter "<area>" --output /tmp/t262-jit.json
```

**GC-touching slices:**
```bash
OTTER_GC_STRESS=64  just bench-diff
OTTER_GC_STRESS=128 just bench-diff
bash scripts/test262-safe.sh built-ins/Array          # heap-cap + ulimit guard
```
Record any pre-existing bootstrap stress crash separately — do not hide new failures behind it.

### Slice 0 — latest results (2026-06-16, darwin arm64)

| Gate | Command | Result |
|---|---|---|
| format | `cargo fmt --all` | ✅ clean |
| lint | `cargo clippy --all-targets --all-features -- -D warnings` | ✅ exit 0, no warnings |
| core tests | `cargo test -p otter-vm -p otter-jit` | ✅ 589 passed, 0 failed |
| build-repair crate | `cargo test -p otter-syntax` | ✅ 6 passed |
| full build | `cargo build --all --all-features` | ✅ |
| all tests compile | `cargo test --all --all-features --no-run` | ✅ |
| release binary | `cargo build --release -p otter-cli` | ✅ |
| output equality | `node benchmarks/diff.mjs` | ✅ 11/11 identical (interp/jit/jit-osr), exit 0 |
| timings | `node benchmarks/bench.mjs --only otter,otter-jit,otter-jit-osr,node,deno,bun --runs 8` | ✅ → `results/latest.{md,json}` |

> **`just` is not installed in this environment.** The `just bench*` recipes are correct but the canonical, verified commands are the raw `node benchmarks/{diff,bench}.mjs` invocations above (and `cargo …` directly). Install `just` to use the shorthands.

### Counters follow-up — latest results (2026-06-16)

| Gate | Command | Result |
|---|---|---|
| format | `cargo fmt --all` | ✅ clean |
| compile touched crates | `cargo test -p otter-vm -p otter-runtime --no-run` | ✅ finished; test binaries built |
| stats API test | `cargo test -p otter-runtime --test runtime_budget_stats` | ✅ 5 passed, 0 failed |
| lint | `cargo clippy --all-targets --all-features -- -D warnings` | ✅ exit 0, no warnings |
| VM/JIT tests | `cargo test -p otter-vm -p otter-jit` | ✅ `otter-jit` 17 passed; `otter-vm` 572 passed; compile-fail 3 passed; layout 2 passed; doctests passed |
| release binary | `cargo build --release -p otter-cli` | ✅ finished |
| output equality | `node benchmarks/diff.mjs` | ✅ 11/11 identical across `interp` / `jit` / `jit-osr`; wrote `benchmarks/results/diff-latest.{md,json}` |
| CLI stats smoke | `OTTER_STATS=1 cargo run -p otter-cli -- run benchmarks/scripts/fib.js` | ✅ stdout `1346269`; stderr JSON schema `otter.stats.v1`, `jitRuntimeCalls=4356542`, `jitDirectCalls=4356542`, `jitRustCallFallbacks=0` |
| CLI stats strict flag | `OTTER_STATS=1 target/debug/otter run benchmarks/scripts/fib.js` / `OTTER_STATS=0 target/debug/otter run benchmarks/scripts/fib.js` | ✅ `=1` stdout `1346269`, stderr schema `otter.stats.v1`; `=0` stdout `1346269`, stderr 0 bytes |

**Per-slice extra gates (plan §6):**
- call/frame slices → `function` / `call` / closure / generator / async / super / try dirs.
- property slices → object / reflect / proxy / accessor / delete / array-callback dirs.
- builtin slices → affected builtins + Array safe runner.
- security-touching slices → capability tests for fs/net/env/subprocess/ffi.

---

## Current baselines

### Differential output equality — `benchmarks/results/diff-latest.md`
**11/11 scripts identical across `interp` / `jit` / `jit-osr`** (2026-06-16, darwin arm64). This is the correctness floor every later slice must keep at 11/11.

| script | value (all tiers agree) |
|--------|-------------------------|
| array-ops.js | 5600004 |
| fib.js | 1346269 |
| json.js | 31926960 |
| mandelbrot.js | 959238 |
| nbody.js | -0.169089263 |
| prop-access.js | 512493014 |
| regex.js | 285000 |
| sort.js | 60026220117 |
| string-ops.js | 559800 |
| typed-array.js | -653933.913 |
| typescript-sample.ts | 249.1574 |

### Timing — `benchmarks/results/latest.md`
**Baseline 2026-06-16, darwin arm64**, min wall-clock ms over 8 runs / 2 warmup, includes process startup. `node v24.14.1`, `deno 2.8.3`, `bun 1.3.14`. Regenerate with `just bench-osr --runs 8`.

| script | otter (interp) | otter-jit | otter-jit-osr | node | deno | bun | jit vs node | jit vs bun |
|---|---|---|---|---|---|---|---|---|
| array-ops.js | 1671.9 | 671.4 | 666.8 | 73.3 | 61.2 | 28.4 | 9.2× | 23.6× |
| fib.js | 1464.6 | 232.4 | 233.4 | 31.9 | 28.7 | 15.6 | 7.3× | 14.9× |
| json.js | 1554.3 | 1546.2 | 1541.0 | 193.7 | 194.3 | 137.7 | 8.0× | 11.2× |
| mandelbrot.js | 1184.3 | 55.0 | 54.0 | 24.9 | 21.6 | 13.6 | 2.2× | 4.0× |
| nbody.js | 959.5 | 119.1 | 145.7 | 25.8 | 20.3 | 12.9 | 4.6× | 9.3× |
| prop-access.js | 2424.4 | 614.2 | 613.8 | 31.9 | 27.4 | 23.6 | 19.3× | 26.0× |
| regex.js | 1685.7 | 1677.8 | 1688.8 | 24.6 | 20.6 | 12.5 | 68.1× | 134.3× |
| sort.js | 2917.0 | 1358.2 | 1361.1 | 137.0 | 130.0 | 117.0 | 9.9× | 11.6× |
| string-ops.js | 424.6 | 427.0 | 427.0 | 30.6 | 24.5 | 15.7 | 14.0× | 27.2× |
| typed-array.js | 2154.2 | 156.1 | 156.6 | 29.2 | 24.7 | 15.7 | 5.3× | 10.0× |
| typescript-sample.ts | 1723.2 | 123.7 | 118.0 | 41.9 | 22.5 | 15.2 | 3.0× | 8.2× |

**Reading the baseline (what each later slice should move):**
- **PupJIT already pays off** on numeric/property/typed-array loops vs the interpreter: mandelbrot ~21×, typed-array ~14×, fib/ts-sample ~6–14×, prop-access ~4×.
- **JIT does ~nothing yet** on `json`, `regex`, `string-ops` (jit ≈ interp) — these are dominated by Rust builtin / regex paths. Expected; closed by `ShellBuiltins` (Slice 6) and `RippleRegex` (Slice 7).
- **Forced OSR ≈ baseline JIT** here (`otter-jit-osr` within noise of `otter-jit`) — current workloads tier up via function-entry compilation, so a threshold of 1 rarely changes the hot path. The config still exercises the OSR entry and is the early-warning probe for OSR correctness/regressions.
- **Largest gaps vs leaders** (the headline numbers to shrink): `regex` 68–134×, `prop-access` 19–26×, `string-ops` 14–27×, `array-ops` 9–24×.

---

## Rollback notes

- **Slice 0** is harness + docs + a build-repair only; it changes **no VM/JIT/GC semantics**. Reverting the Slice-0 commit removes `benchmarks/diff.mjs`, the `bench*` justfile recipes, the `otter-jit-osr` bench variant, and this file, with zero runtime effect. The build-repair edits (oxc/miette/sha2 API) must **not** be reverted independently or the workspace stops compiling — see below.
- **General rollback strategy:** every behavior-changing slice ships behind an env kill-switch so it can be disabled without a revert:
  - HoltStack: `OTTER_HOLT_STACK=0` (planned).
  - PupJIT direct calls: `OTTER_PUP_DIRECT_CALLS=0` (planned).
  - WhiskerIC: per-site → megamorphic stub; per-function recompile fallback; `OTTER_WHISKER_IC=0` (planned).
  - Existing global escape hatch today: `OTTER_JIT=0` (interpreter only).

### Build repair (oxc 0.136)
The `oxc 0.129 → 0.136` workspace bump broke compilation; repaired in:
- `crates/otter-syntax/src/lib.rs` — `ParserReturn.errors` → `.diagnostics` (`Diagnostics` derefs to `Vec<OxcDiagnostic>`).
- `crates/otter-syntax/src/diagnostic.rs` — `OxcDiagnostic.labels` is now the `miette::Labels` enum (`.as_slice()`); `LabeledSpan::offset()/len()` now return `u32` (was `usize`), removed the dead `usize_to_u32` helper.
- `crates/otter-cli/src/error_render.rs` — miette `SourceSpan` now `From<Range<u32>>` (`ByteOffset = u32`); pass `range.0..range.1` directly.
- `Cargo.toml` — reverted `sha2 = "0.11"` → `"0.10"` (pinned `0.10.9`). The 0.11 bump pulled `digest 0.11` while `sha1`/`md-5` stayed on `digest 0.10`; `otter-node/src/crypto.rs` drives all three under one `D: Digest` bound, so the split broke the build. Keeping the whole digest family on 0.10 leaves `crypto.rs` untouched. (If a deliberate digest-0.11 migration is wanted, bump `sha1`/`md-5` and port `crypto.rs` to the 0.11 finalize/update API instead.)

---

## Next slice entry criteria — Slice 0 → Slice 1

Slice 0 is **done** (and Slice 1 may begin) only when:
1. This file exists and is accurate. ✅
2. `just bench-diff` reliably proves output equality JIT off/on/forced-OSR and exits non-zero on mismatch. ✅
3. `just bench` / `just bench-osr` reliably collect machine-readable timings. ✅
4. Verification commands and their latest results are recorded here. ✅ (timing table paste pending)
5. No unrelated VM/JIT/GC semantic refactor is mixed into the slice. ✅ (harness + build-repair only)
6. The commit is revertible without affecting runtime behavior. ✅

---

## Code anchors (verified in repository 2026-06-16)

Source map for the substrate the next slices touch. Treat line numbers as drift-prone — re-grep before editing.

### `crates/otter-vm/src/lib.rs`
- **Interpreter dispatch loop** — `dispatch_loop_inner`, bytecode `match op` around **4499–4610**.
- **JIT tier-up entry** — `maybe_dispatch_jit` **1203–1223** (routes pushed frames to compiled code when a hook is installed).
- **`jit_runtime_call`** — fn sig **1446** (Rust bridge compiled code calls per JS call; the floor PupJIT must remove).
- **`run_compiled_frame`** — fn sig **1417** (runs compiled code over a rooted frame window).
- **Loop OSR** — `note_backedge_and_maybe_osr` **1232–1262**; `const JIT_OSR_THRESHOLD = 1000` at **1193**; `OTTER_JIT_OSR_THRESHOLD` read **1047–1051** (lower ⇒ earlier OSR; the diff/bench forced-OSR config sets it to `1`).
- **Stats accessors** — `property_ic_stats()`, `runtime_budget_stats()`, `jit_runtime_stats()`; runtime-facing rollup is `Runtime::execution_stats()`.
- **JIT counters today** — `JitRuntimeStats` records compiled `Op::Call` bridge calls, compiled-to-compiled fast-call hits, Rust fallback calls, function-entry compile attempts, OSR threshold attempts, and JIT property/method/element/global/upvalue runtime stubs.

### `crates/otter-vm/src/runtime_budget.rs`
- **`RuntimeBudgetStats`** struct **74–103** — 14 fields incl. `reductions_executed`, `bytecode_calls`, `native_calls`, `construct_calls`, `current_turn_allocated_bytes`, `max_turn_nanos`, `host_ops_enqueued`. Useful raw material for the deferred CLI counter dump.

### `crates/otter-vm/src/call_ops.rs`
- **Call-frame construction** — `push_bytecode_call_frame` **374–386**; `push_prepared_bytecode_call_frame` **567**; `try_push_bytecode_call_frame_from_window` **610**.
- **Sync callable reentry** — `run_callable_sync` **1541–1558**; `run_callable_sync_already_rooted` **1576–1585** (used by loop-OSR / function-entry tier-up when the frame stack is already rooted).

### `crates/otter-vm/src/frame_state.rs`
- **`Frame`** struct **47–96** (function_id, pc, registers `SmallVec`, return_register, upvalues, this_value, async_state, `cold: ColdFrameIdx`, generator_owner).
- **`trace_frame_slots`** **452–471** — traces every register, upvalue cell, `this`, async result promise, generator owner; cold-record slots traced separately. This is the Stage-A precise root provider HoltStack must preserve.

### `crates/otter-vm/src/cold_frame.rs`
- Exists — cold side-record storage for frames (try/finally, protected paths, etc.).

### `crates/otter-vm/src/object.rs`
- **`ObjectBody`** `#[repr(C)]` **388–417+**; `shape: ShapeHandle` first field; `OBJECT_BODY_SHAPE_OFFSET` (asserted `0`) **490/499**; `inline_values: [Value; INLINE_VALUE_CAP]`, `INLINE_VALUE_CAP = 6` **181–183**; overflow spill to `overflow_values: Vec<Value>`. **The hot-object-layout unlock has already partially landed** — next bottleneck is IC/call/JIT integration around it, not the layout.

### `crates/otter-vm/src/property_ic.rs`
- **`PropertyIcEntry<T>`** enum **135–155**: `Empty` → `Polymorphic { entries, misses }` → `Megamorphic`. `MAX_PIC_ENTRIES = 4` at **49**; megamorphic transition **265–271 / 225–226**.

### `crates/otter-vm/src/jit.rs`
- **JIT ABI / rooting contract** — module doc **18–30** (baseline v1 uses the interpreter frame register array as the precise root set; values cached in machine registers only between safepoints — Stage A).
- **`JitFunctionView`** **44–75** — owned compilation snapshot (function_id, param/register counts, flags, cage_base, ta_layout, instructions).

### `crates/otter-jit/src/baseline.rs`
- **arm64 emitter** — module doc **1–36**; codegen begins **539+**.
- **`jit_call_stub`** **126–159** — extern-C stub; unmarshals ctx + operands, calls `vm.jit_runtime_call()`, writes status/error.
- **Monomorphic `LoadProperty`** inline **1109–1173** — guard receiver tag / GC type tag / shape handle, fixed-offset in-object load; miss → shared stub.

### `crates/otter-gc/`
- **`frame_roots.rs`** — `FrameRoots` trait **20–23** (`trace(&self, &mut dyn FnMut(*mut RawGc))`); `FrameRootProviders` LIFO registry **27–71**.
- **`heap.rs`** — frame-root provider push/pop/trace **285–306**; `OTTER_GC_STRESS` read **~240**.
- **`scavenger.rs`** — `scavenge()` entry **129–178** (roots → external handles → dirty cards → Cheney scan; returns `ScavengeStats`).
- **`barrier.rs`** — write barrier **62–99**; **invariant (28–36): card derives from the parent object header, never from a traced slot address** — slots in malloc-owned storage (boxed frames, spilled `SmallVec`s) would fabricate page headers in foreign memory.

### `crates/otter-vm/src/generator.rs`
- **`GeneratorBody`** parked snapshot **78–125** — `frame: Option<Box<Frame>>`, `cold: Option<Box<ColdFrame>>`; traced via pelt helpers. Must stay correct across the HoltStack/PupJIT changes.

### `crates/otter-vm/src/pelt.rs`
- **`PeltField for Arc<T>`** no-op tracer **150–158** — Arc payloads (JSON bytes, libraries, NativeFn closures) are foreign. **Never hide a GC-bearing field behind `Arc<T>` without an explicit hand-written trace.**

### Binary / CLI
- Binary crate `otter-cli`, binary name `otter`. `Command::Run` dispatch in `crates/otter-cli/src/main.rs` **~485**, forwarding to `run_target`; bare `otter <file>` shorthand **~501** routes the same way.

---

## Counters status

`OTTER_STATS=1` prints one machine-readable JSON line to stderr after `otter run`, with schema `otter.stats.v1`. Other values, including `OTTER_STATS=0`, are treated as disabled. Normal stdout is unchanged, so benchmark output equality stays valid. The payload is an end-of-run snapshot of the current `Runtime`; reused embedding runtimes see cumulative counters unless they reset VM counters between runs.

| Signal (plan §Phase 0) | Source today | Plumbed to CLI? |
|---|---|---|
| IC hit/miss/install/disable (load/store/has) | `Interpreter::property_ic_stats()` → `RuntimeExecutionStats` | yes |
| bytecode / native / construct call counts | `Interpreter::runtime_budget_stats()` → `RuntimeExecutionStats` | yes |
| reductions, per-turn alloc bytes, max turn nanos | `RuntimeBudgetStats` → `RuntimeExecutionStats` | yes |
| GC cycles / pause / alloc / live bytes | `GcHeap::gc_stats()` → `RuntimeExecutionStats` | yes |
| JIT runtime call count | `Interpreter::jit_runtime_stats().runtime_calls` | yes |
| direct compiled→compiled call count | `Interpreter::jit_runtime_stats().direct_calls` | yes |
| JIT Rust callable fallback count | `Interpreter::jit_runtime_stats().rust_call_fallbacks` | yes |
| runtime property/method/element/global/upvalue stub calls | `Interpreter::jit_runtime_stats().runtime_property_stubs` | yes |
| function-entry compile attempts / OSR threshold attempts | `Interpreter::jit_runtime_stats()` | yes |
| deopts / optimized code size / optimizer compile latency | no optimizer tier yet | not applicable |

Latest smoke result (2026-06-16):

```bash
OTTER_STATS=1 cargo run -p otter-cli -- run benchmarks/scripts/fib.js
```

stdout stayed `1346269`; stderr included `schema:"otter.stats.v1"` with `jitRuntimeCalls=4356542`, `jitDirectCalls=4356542`, `jitRustCallFallbacks=0`, `jitCompileAttempts=1`, `jitRuntimePropertyStubs=0`.

---

## Next-slice design note — Slice 2b: `PupJIT` direct branch

> Preview only. **Do not implement yet.** Entry is gated on this counters follow-up being green.

### Objective
Replace the current machine→Rust `jit_call_stub` → `Interpreter::jit_runtime_call` bridge for eligible monomorphic JS callees with emitted arm64 frame reservation + direct branch/call to the callee's compiled entry. Keep all cold/ineligible cases on the existing bridge.

### Frame descriptor substrate
`Frame` is still a Rust-managed hot struct with `SmallVec` registers, upvalue spine, cold index, async state, and generator owner. Emitted code must not construct this struct directly. The next safe substrate is a small Rust reservation helper that creates a fully initialized appended frame on the existing reservation-stable `HoltStack`, returns a descriptor/pointers for the value slots, and publishes only after all `Value` slots are initialized to `undefined`.

Minimum new names should stay in Otter vocabulary: `HoltFrameDesc`, `HoltValueSlots`, `HoltCallReservation`, `PupCallSite`, `PupDirectCall`.

### Stack separation invariant
`HoltStack` must remain disjoint from `Interpreter`. JIT entry already passes erased pointers for `vm` and `stack` separately (`JitReentryPtrs`), and compiled code may need both during reservation and runtime fallback. Moving the stack into `Interpreter` would force aliasing `&mut Interpreter` with an active stack borrow and is not acceptable.

### Stage-A rooting
Keep current full frame-window tracing. A direct-call callee frame is appended to the active `HoltStack`, and the enclosing `dispatch_loop` `FrameRoots` provider traces it via `Frame::trace_frame_slots`. No GC-bearing `Value` may live only in a machine register across a call/allocation safepoint until `StoneMaps` exists.

### Rollback strategy
Add an env kill switch `OTTER_PUP_DIRECT_CALLS=0` that forces every compiled `Op::Call` through the existing `jit_call_stub` bridge. Revert path remains `git revert` of the direct-call commit; no parallel VM stack is introduced.

### Test262 gates
Call/frame slice gates: `language/expressions/call`, `language/statements/function`, `language/expressions/function`, arrow/async/generator function dirs, `language/expressions/super`, `language/statements/try`, `language/expressions/yield`, `language/expressions/await`. Failing sets must be identical with `OTTER_JIT=0`, normal JIT, forced OSR, and direct-calls disabled/enabled.

### Primary risks
Dangling caller `x19` if stack growth could move frames (mitigated by reservation-stable `HoltStack`); tracing partially initialized appended frames (two-phase reservation/publish); wrong receiver/sloppy-this binding; exception/finally unwind across a direct callee; accidentally making async/generator/constructor/eval/rest/arguments calls eligible.
