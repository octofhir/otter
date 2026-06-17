# Otter engine performance audit

Deep profile-driven audit of the structural gaps between OtterJS and
node/bun/deno on `benchmarks/scripts/*`. Goal: find the **architectural**
holes that cause the 5–15× (and 67× regex) gaps, not micro-noise.

- Host: `darwin arm64`, Apple Silicon. node `v24.16.0`, bun, deno current.
- Binary: `target/release/otter` built with `cargo build --release -p otter-cli`.
  Release profile now carries `debug = 1` + `force-frame-pointers = true`
  (added to `Cargo.toml`) so samply/atos symbolicate cleanly.
- Profiler: `samply record --save-only` (raw `.json.gz` Firefox profiles
  committed under `benchmarks/profiles/`). Symbolication via a dSYM
  (`dsymutil target/release/otter`) + `atos`; see **Methodology** for why
  samply alone mis-symbolicates and how the parser fixes it.
- Counters: `OTTER_STATS=1` end-of-run snapshot (IC hit/miss, jit
  direct/runtime/fallback calls, GC alloc bytes / cycles / pause).

> **Reproduce:** `node benchmarks/profiles/collect.mjs` (timings + counters),
> `bash benchmarks/profiles/prof.sh <script.js…>` (samply + per-bench
> self-time), `node benchmarks/profiles/symbolicate.mjs <prof.json.gz>`.

---

## 0. Two findings that reframe everything

**(a) `OTTER_JIT` is dead — the JIT is always on.** Gating is now
`OTTER_JIT=0` *disables*; default is ON (`otter-runtime/src/lib.rs:1679`).
The bench harness' `otter-jit` vs implicit-off split is meaningless; deleting
the var leaves the JIT enabled. The audit's "JIT off" column was re-collected
with `OTTER_JIT=0` to get a true interpreter baseline. (Memory note "JIT off
by default" is stale.)

**(b) The JIT is a Sparkplug-style baseline template tier — there is no
optimizing tier.** `crates/otter-jit` (dynasm arm64 macro-assembler):
*"no IR, no register allocation, and no deopt … one linear pass, one emit
routine per op … Guard failure = bail, not deopt."* It deletes the
interpreter dispatch envelope for hot functions, and that alone buys 7–38×
over the interpreter — but every value still round-trips through the tagged
8-byte `Value`, every property/method/element access calls a runtime bridge
stub, and there is no cross-op type specialization, inlining, LICM, or
bounds-check elimination. **This is the single biggest structural gap vs
node (TurboFan) / bun (JSC FTL).**

---

## 1. Headline numbers

Wall-clock, min ms over 6 runs (incl. process startup; `bench.mjs`):

| script | otter | node | node× | bun | bun× | dominant cost (otter) |
|---|---|---|---|---|---|---|
| regex.js | 2307.0 | 34.3 | **67.3×** | 14.6 | 158× | backtracking regex interp |
| string-ops.js | 572.3 | 39.3 | **14.5×** | 18.5 | 31× | interp dispatch + VmError drop |
| sort.js | 1578.0 | 177.6 | **8.9×** | 124.3 | 12.7× | comparator call floor (interp) |
| prop-access.js | 348.8 | 40.6 | **8.6×** | 29.7 | 11.7× | property hashing + method-in-interp |
| array-ops.js | 774.6 | 98.2 | **7.9×** | 41.0 | 18.9× | callback call floor (not inlined) |
| json.js | 1654.4 | 252.8 | **6.5×** | 135.5 | 12.2× | malloc/GC + UTF conversions |
| fib.js | 275.0 | 43.8 | **6.3×** | 19.3 | 14.3× | per-call frame setup |
| typed-array.js | 204.5 | 42.4 | **4.8×** | 19.6 | 10.4× | element access via runtime stub |
| nbody.js | 133.4 | 32.6 | **4.1×** | 15.0 | 8.9× | property access via bridge stub |
| typescript-sample.ts | 186.0 | 61.6 | **3.0×** | 16.3 | 11.4× | matmul (typed-array pattern) |
| mandelbrot.js | 54.2 | 35.5 | **1.5×** | 17.1 | 3.2× | pure compiled float loop (ideal) |

**Startup correction.** Measured cold startup: otter ≈10 ms, bun ≈10 ms,
**node ≈30 ms**. node's heavy startup compresses the ratio on sub-100 ms
benches: mandelbrot's 1.5× wall is really ~7× on compute (otter script-only
40 ms vs node compute ~5 ms), and nbody/fib gaps are likewise wider than the
wall ratio. The bun column (matched ~10 ms startup) is the fairer mirror and
shows the true spread: **3–31× on everyday workloads, 158× on regex.**

### JIT vs interpreter + counters (`collect.mjs`, durationMs, min of 4)

| script | interp ms | jit ms | jit speedup | GC MB | GC cyc | propHit (stub) | jitDirect |
|---|---|---|---|---|---|---|---|
| mandelbrot | 1517 | 40 | **37.5×** | 0.9 | 0 | 0 | 0 |
| typed-array | 2911 | 189 | 15.4× | 0.9 | 0 | 131070 | 0 |
| typescript | 2405 | 171 | 14.1× | 0.9 | 0 | 0 | 0 |
| nbody | 1291 | 116 | 11.1× | 1.2 | 0 | **1407476** | 19000 |
| prop-access | 3172 | 339 | 9.4× | 42.1 | 2 | 5243 | 0 |
| fib | 1923 | 262 | 7.3× | 0.9 | 0 | 0 | **4356542** |
| array-ops | 2059 | 762 | 2.7× | 0.9¹ | 0 | 0 | 0 |
| sort | 3682 | 1530 | **2.4×** | 0.9 | 0 | 0 | 1 |
| json | 1637 | 1639 | **1.0×** | **386.9** | 10 | 78 | 4000 |
| string-ops | 546 | 551 | **1.0×** | 210.7 | 3 | 0 | 1004 |
| regex | 2291 | 2278 | **1.0×** | 17.4 | 0 | 0 | 0 |

¹ `gcAllocBytesTotal` counts GC-cell bytes only; dense-array element storage
is a side `Vec` (malloc), so array-ops/json under-report here — the profile's
malloc% is the real allocation signal.

**Reading the speedup column is the whole story:**
- **37×/15×/11×** (mandelbrot, typed-array, nbody): tight numeric loops the
  baseline JIT compiles well. The JIT is doing its job.
- **2.4×/2.7×** (sort, array-ops): a JS **callback** is invoked per element
  through the native→JS call path. The callback body may be compiled, but the
  per-call frame setup is a hard floor the JIT can't cross — see §3.
- **1.0×** (json, regex, string-ops): the work is in **native Rust**
  (JSON serializer, regex backtracker, string ops) with a trivial JS driver
  loop. The JIT has nothing to compile; these measure native throughput +
  allocation, not codegen.

---

## 2. Per-bench self-time (samply, busiest "otter-isolate" thread)

Full tables in `benchmarks/profiles/*.selftime.txt`; raw profiles in
`*.json.gz` (open at profiler.firefox.com or `samply load`). `[dylib] …`
rows are leaf time in a system library (classified by samply's lib table,
not guessed). `[dylib] (anon/jit)` = the JIT-emitted machine code itself.

### regex.js — 67× (the outlier)
```
 66.4%  otter_regex::exec::backtrack::Matcher::run
 11.3%  <otter_regex::api::Matches as Iterator>::next
  8.6%  <SmallVec as Clone>::clone        ← capture-slot snapshot per step
  3.6%  RuntimeState::trace_roots_inner
  2.4%  [dylib] libsystem_malloc.dylib «malloc/free»
```
Root cause: **backtracking interpreter** (`otter-regex/src/exec/backtrack.rs`)
with a step budget, snapshotting the full capture-slot array onto its stack on
every quantifier step (the 8.6% SmallVec clone). node/bun compile each regex to
native code (Irregexp / JSC). This is a different *class* of engine, not a
slow function.

### string-ops.js — 14.5×
```
 45.7%  Interpreter::dispatch_loop_inner    ← charCodeAt scan runs in interpreter
 14.3%  drop_in_place<VmError>              ← see §4
  3.1%  string::gc_body::alloc_flat_string_body_with_roots
  2.6%  [dylib] libsystem_malloc.dylib
  ...   method_ops / invoke / get_method_value_for_call (per-char charCodeAt call)
```
The `for … charCodeAt(i)` loop never tiers up (loop body is a method call), so
it runs in the interpreter. 60% of time is interpreter dispatch + VmError drop.
210 MB allocated for string building.

### sort.js — 8.9×
```
 18.2%  dispatch_loop_inner                 ← comparator (a-b) runs in interpreter
 10.6%  [dylib] libsystem_malloc.dylib
  8.7%  array_prototype::sort_merge         ← native merge sort
  8.4%  run_bytecode_callable_committed  ┐
  5.2%  bind_bytecode_call_arguments     │  per-comparator-call frame machinery
  2.6%  reclaim_registers                │  (~20% total)
  2.0%  draw_registers                   ┘
  7.4%  drop_in_place<VmError>
  1.8%  coerce::to_number_or_throw          ← a-b coercion
```
~11.4M comparator calls (20k·log₂20k·40). Each pays a full bytecode-call frame
build/teardown ≈ 130 ns. node inlines the comparator into the sort.

### prop-access.js — 8.6×
```
 30.5%  [dylib] (anon/jit)                  ← compiled outer loop
 17.1%  dispatch_loop_inner                 ← bump()/dist2() methods run in interpreter
  5.3%  drop_in_place<VmError>
  3.9%  core::hash::BuildHasher::hash_one ┐
  2.3%  sip::Hasher::write                │  property access HASHES the string key
  2.3%  string::gc_body::eq_str           │  ("x","y","tag") instead of a cached slot
  3.2%  object::lookup_own                ┘
  2.8%  object::body_offset_of
```
Two costs: (1) the `dist2`/`bump` methods are not inlined into the JIT'd loop
and re-enter the interpreter; (2) property lookup still hashes the key name and
`eq_str`-compares — the shape→slot inline cache is not eliminating the hash on
this path.

### array-ops.js — 7.9×
```
 14.9%  [dylib] libsystem_m.dylib «fmod»    ← `%` on overflowed doubles (int32→float bail)
 11.8%  run_bytecode_callable_committed  ┐
  6.6%  array_callback_native_dispatch   │  map/filter/reduce/forEach callback
  5.5%  bytecode_call_target_parts       │  invoked per element through the
  5.1%  bind_bytecode_call_arguments     │  generic call path — NOT inlined
  8.8%  [dylib] libsystem_malloc
  5.2%  [dylib] (anon/jit)
  3.x%  reclaim_registers / draw_registers / enter_at / resolve_jit_code
```
`jitDirect=0`: the callbacks are not direct-called/inlined; each of 200k·12
elements pays the call floor. Plus int32 results overflow to double and `x%3`/
`% 1_000_000` go through `fmod` (libm 14.9%).

### json.js — 6.5×
```
 36.6%  [dylib] libsystem_malloc.dylib «malloc/free»   ┐
  6.4%  [dylib] libsystem_platform.dylib «memcpy»       │  ~43% allocation + copy
  4.8%  GcHeap::start_incremental_mark_phase            ┐
  3.3%  scavenger::process_slot                         │  ~12% GC
  3.0%  GcHeap::sweep_phase_with_pause_start            ┘
  5.4%  <String as FromIterator<char>>::from_iter       ┐
  2.6%  JsString::from_str                              │  UTF-16↔UTF-8↔Latin1
  2.6%  alloc_latin1_string_body_with_roots             │  conversions per value
  2.3%  json::serialize::quote_units_into               │
  1.7%  str::from_utf8                                  ┘
  1.4%  string::gc_body::to_utf16_vec
```
387 MB allocated, 10 full GC cycles, 26 ms max pause. The serializer allocates
a fresh `String`/body per value and converts encodings repeatedly. node
serializes into a single growable buffer.

### nbody.js — 4.1×
```
 33.6%  [dylib] (anon/jit)                  ← compiled advance()/energy()
  9.4%  jit_runtime_load_property           ← b.x/b.vx/… via runtime BRIDGE STUB
  4.8%  drop_in_place<VmError>
  3.9%  object::load_own_data_slot_atom
  3.9%  jit_runtime_call_method
  2.7%  string::gc_body::eq_str             ← string-key compare per field load
  2.5%  jit_runtime_write_barrier
```
1.4M property-load stub hits. The compiled float math is fast, but every
`body.x` field read leaves compiled code for a runtime stub that does a
shape/key lookup (`eq_str`). node speculates the object shape and loads the
field as an inlined, fixed-offset, unboxed double.

### typed-array.js — 4.8×
```
 33.1%  [dylib] (anon/jit)
 20.3%  dispatch_loop_inner
 14.1%  property_dispatch::jit_runtime_delegate_op   ← a[i] element access delegated to runtime
 12.5%  drop_in_place<VmError>
  2.4%  baseline::jit_delegate_op_stub
```
`Float64Array` element load/store delegates to a runtime op rather than an
inline bounds-checked, unboxed load/store. node compiles typed-array access to
a couple of machine instructions.

### fib.js — 6.3× (the pure-call case)
```
 22.7%  [dylib] (anon/jit)                  ← compiled fib body
 18.6%  Interpreter::prepare_jit_direct_call_frame  ┐
 14.6%  Interpreter::jit_prepare_direct_call        │  ~56% in per-call
  8.5%  Interpreter::jit_finish_direct_call_returned│  frame setup, even on the
  4.6%  Interpreter::draw_registers                 │  compiled→compiled DIRECT
  3.0%  jit_prepare_direct_call_stub                │  call fast path
  2.5%  Frame::build_upvalues_for_count             │
  2.3%  bytecode_call_target_parts                  ┘
  6.9%  drop_in_place<VmError>
```
Even the optimized compiled→compiled direct-call path builds a frame, draws
registers, and builds upvalues per call. With 4.36M calls this dominates.

### mandelbrot.js — 1.5× (the ideal)
```
 74.9%  [dylib] (anon/jit)                  ← compiled float loop, ~all the time
  4.8%  malloc · 2.4% dispatch · 1.7% VmError drop · parse/startup remainder
```
When everything compiles and there are **no calls, no property access, no
allocation**, otter is competitive. The residual gap vs node here is pure
**baseline-JIT code quality**: no LICM, no SIMD, naive register use, per-op
box/unbox. This is the irreducible "no optimizing tier" tax, isolated.

---

## 3. Confirmed root causes (hypothesis → evidence)

| Hypothesis | Verdict | Evidence |
|---|---|---|
| NaN-box tax (box/unbox per op) | **Partly** — not double-*boxing* (doubles stored raw inline, ints are `TAG_INT32` immediates), but the baseline JIT box/unboxes through `Value` every op (`emit_box_double`/`emit_num_to_double`) and has no unboxed-double loop registers. Shows as the residual in mandelbrot's 75% anon/jit being slower than TurboFan output. | `value/mod.rs:289`, `jit/baseline.rs` |
| Moving-GC use-after-move reload | **Not a hot cost** here. No bench shows reload/root-stack churn in the top self-time. The moving collector's cost surfaces as alloc/GC throughput (json), not per-op reloads. | profiles |
| Per-call bridge floor (~95–130 ns) | **Confirmed, major.** sort 2.4× / array-ops 2.7× JIT speedup capped by it; fib 56% self-time in call setup; sort ~20% in bind/draw/reclaim registers. | fib/sort/array-ops self-time |
| No optimizing tier | **Confirmed, the headline.** Property/method/element access via runtime stubs from compiled code (nbody 9.4% load-prop stub, typed-array 14% delegate-op); no inlining (methods/callbacks re-enter interpreter); no LICM/bounds-elim. | nbody, typed-array, prop-access, array-ops |
| Property = hashed key lookup | **Confirmed.** prop-access ~8% in hash_one+sip+eq_str; nbody eq_str per field. Shape→slot IC is not removing the key hash/compare on the stub path. | prop-access.on, nbody.on |
| Interp dispatch is a heavy match-loop | **Confirmed but secondary** — 59% in prop-access.off, but the JIT bypasses it. Only matters for *uncompiled* code: loop-bodies-with-calls (string-ops 46%, sort 18%) that never tier up. | prop-access.off, string-ops.on |
| String rope vs flat | **Not the problem** — strings already have Rope/Flat/Latin1/Sliced (`string/gc_body.rs`). string-ops is bound by interpreter dispatch + per-char `charCodeAt` calls + allocation, not string representation. | string/gc_body.rs, string-ops.on |
| Regex backtracking | **Confirmed.** 66% in `backtrack::Matcher::run` + 8.6% capture-slot clone per step. Backtracker vs compiled engine. | regex.on |
| **VmError fat-enum drop (new finding)** | **Confirmed, cross-cutting.** `drop_in_place<VmError>` is 5–15% of self-time in *every* interp-heavy bench (string-ops 14.3%, typed-array 12.5%, prop-access.off 15.3%, sort 7.4%, fib 6.9%) with **zero thrown JS errors**. `VmError` is a large multi-`String` enum; `Result<Value, VmError>` is fat, so the move/drop glue runs constantly on the hot path. | all profiles, `run_control.rs:97` |

---

## 4. Ranked architectural holes (by gain ÷ effort)

| # | Hole | Benches affected | Est. gain | Refactor | Effort/risk |
|---|---|---|---|---|---|
| 1 | **`VmError` fat-enum on the hot `Result`** | ALL (5–15% each) | 1.05–1.18× broad | Box every payload → `Result<Value, Box<VmError>>` (8–16 B). Drop = null-check + free. | **LOW** / low risk. Mechanical. |
| 2 | **No optimizing tier** (unboxed type-specialized SSA + inline + LICM + bounds/guard elim + deopt) | prop-access, nbody, typed-array, array-ops, fib, sort, ts | **2–5×** on object/array/call benches | New tier above baseline; profile-guided, deopt on guard fail. | **HIGH** / high. Large project. |
| 3 | **Per-call frame floor** (frame build, bind args, draw/reclaim registers, build upvalues per call) | fib, sort, array-ops, any call-heavy | **2–4×** on call-bound | Register-window calling convention; inline monomorphic callees; **inline builtin callbacks** (map/filter/sort comparator) into the compiled loop. | **MED-HIGH** / med. Partly subsumed by #2. |
| 4 | **Allocation + GC throughput + encoding conversions** | json (43% malloc), string-ops, array-ops | **1.5–3×** on alloc-bound | Serialize into one buffer (no per-value `String`); bump-allocate young gen inline; cut UTF-16↔8↔Latin1 round-trips; keep dense-array storage off the malloc path. | **MED** / med. |
| 5 | **Property access hashes the key** instead of shape-cached slot | prop-access, nbody, all object code | folds into #2; ~1.1–1.3× standalone | Make the JIT load/store-property stub a monomorphic shape-guard + fixed slot offset with **no key hash/`eq_str`**; cache offset in the IC site. | **MED** / med. |
| 6 | **Regex backtracker vs compiled** | regex (67×) | **5–20×** on regex | Compile pattern to a NFA/bytecode program executed without per-step capture snapshot, or a native-code regex tier; lazy capture materialization. | **MED-HIGH** / med. Isolated crate. |
| 7 | Interp dispatch match-loop overhead | cold code, uncompiled loop bodies | small (JIT bypasses) | Tier up loop-bodies-with-calls too; or threaded dispatch for cold path. Low priority. | LOW value |

---

## 5. Top-3 whole-class levers (with refactor plan)

### Lever A — Shrink `VmError`; stop paying error-drop on the success path
*Why multiplicative-ish and nearly free:* `drop_in_place<VmError>` is **5–15%
of self-time in every interpreter-heavy bench, with no exceptions thrown.**
The cause is structural: `VmError` (`run_control.rs:97`) has ~10 variants
carrying `String`/`Box`, so it is large; `Result<Value, VmError>` is a fat
return value moved through every `?` on the hot path, and LLVM emits drop glue
that is sampled constantly.

Plan:
1. Make every error variant carry its payload behind one `Box`:
   `enum VmError { Throw(Box<ThrownError>), StackOverflow{limit}, NotCallable, Exit{..}, … }`
   so `size_of::<VmError>()` ≈ 8–16 B and `Result<Value, VmError>` is small.
2. Audit hot opcode handlers for `Err(...)` used on *expected* paths
   (property/method miss → prototype walk, coercion fallbacks). Return a
   non-`Result` "not-found" enum for those so no `VmError` is ever built.
3. Add a `const _: () = assert!(size_of::<VmError>() <= 16)` guard.

Expected: a broad **5–15% across the board**, immediately, at low risk. Best
gain/effort on the board — do this first; it also de-noises every later
profile.

### Lever B — Optimizing JIT tier (the kratny lever)
*Why multiplicative:* the baseline tier removes interpreter dispatch but keeps
every value boxed in the 8-byte tag, every property/method/element access as a
runtime bridge stub, and inlines nothing. nbody spends 9.4% in a load-property
stub + 2.7% in `eq_str` per field; typed-array 14% in a delegate-op stub;
array-ops/sort pay the call floor per element; mandelbrot's compiled loop is
still ~7× node on compute because there is no LICM/regalloc/SIMD. These are not
slow functions — they are missing compiler passes.

Plan (incremental, profile-guided, on top of the existing baseline tier):
1. **Type feedback** from the baseline tier + IC sites (already collected:
   `MethodCallFeedback`, property IC). Record observed `Value` kinds and object
   shapes per site.
2. A second tier that builds **SSA**, speculates types from feedback, and
   keeps numbers **unboxed** (f64/i32 in FP/GP registers across the loop) —
   eliminates per-op box/unbox.
3. **Speculative inlining** of monomorphic methods (`dist2`, `bump`) and
   builtin callbacks; **inline-cache property access** as shape-guard + fixed
   slot, no key hash; **bounds-check elimination** for typed-array/dense-array
   indexing; **LICM** for invariant field loads / `length`.
4. **Deopt** on guard failure back to the baseline tier (replaces the current
   "bail to interpreter").

Expected: **2–5×** on prop-access, nbody, typed-array, array-ops, ts-matmul;
closes most of the everyday gap to bun. Large, multi-month project — scope it
as its own plan (extends `JIT_DESIGN.md`).

### Lever C — Kill the per-call floor (calling convention + builtin inlining)
*Why multiplicative on call-bound code:* fib is **56% per-call frame setup**;
sort and array-ops JIT speedups are capped at 2.4×/2.7× purely because each
comparator/callback invocation rebuilds a frame, binds args, and draws/reclaims
a register window. node inlines these.

Plan:
1. **Register-window calling convention** for compiled→compiled calls: pass
   args in the callee's register window in place, skip `bind_bytecode_call_arguments`
   / `draw_registers` / `reclaim_registers` for the monomorphic fast path.
2. **Inline builtin callbacks**: when `Array.map/filter/forEach/reduce` and
   `Array.sort` see a monomorphic JS callback, splice its compiled body into
   the native iteration so there is no per-element re-entry
   (`run_bytecode_callable_committed` disappears from the profile).
3. Fold `build_upvalues_for_count` out of the hot path for callees with no
   captured upvalues (the common case).

Expected: **2–4×** on fib/sort/array-ops. Partly subsumed by Lever B's
inlining, but the calling-convention work is independently valuable and
cheaper than a full optimizing tier — a good mid-term win.

**Secondary scoped projects:** allocation/GC + JSON-buffer serialization
(Lever 4 — fixes json/string), and the regex engine (Lever 6 — fixes the 67×
outlier in isolation). Both are real but narrower than A–C.

---

## 6. Methodology notes (so the numbers are trustworthy)

- **samply mis-symbolicates by default on this binary.** macOS keeps DWARF in
  `.o` files, not the linked executable, so samply emits raw `0x…` for otter
  frames; worse, dylib/JIT addresses mmap'd *above* the 16.6 MB `__TEXT` get
  attributed by `atos` to the *last* binary symbols (clap/aho-corasick),
  fabricating bogus "10% in CLI parsing" rows. Fixes applied:
  1. `dsymutil target/release/otter` → dSYM for `atos`.
  2. `symbolicate.mjs` classifies each leaf by **samply's own lib table**
     (`funcTable.resource → resourceTable.lib → libs[]`): only `otter`-lib
     frames are `atos`'d; dylib frames are bucketed by name
     (`libsystem_malloc` = malloc/free, `libsystem_platform` = memcpy,
     `libsystem_m` = libm). This is what surfaced json's real 43% allocation.
- **Thread filter:** otter runs JS on the `otter-isolate` thread; the parser
  uses only the busiest thread so parked tokio/main threads don't drown the
  histogram (they otherwise showed as two phantom 33% entries).
- `[dylib] (anon/jit)` = JIT-emitted machine code (no symbols by nature);
  high values there mean the JIT *is* engaged and running compiled code.
- Short benches (mandelbrot 40 ms, nbody 116 ms) carry visible startup
  (oxc parse, malloc) in their profiles — discount the parse/startup rows.

_Generated 2026-06-17. Profiles + parser committed under `benchmarks/profiles/`._
