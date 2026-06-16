# Otter Engine Refactor — Progress

**Plan source:** [`benchmarks/plan.md`](benchmarks/plan.md) — *OtterJS большой breaking-refactor план*
**Naming policy:** all new engine-internal names use Otter/river/den/holt/raft/pelt/lodge/current/bank/stream/slide/dive/burrow vocabulary. Never name a new Otter tier/module/struct after another engine or its tiers.
**Working rule:** every slice is independently buildable, testable, and revertible. One architectural slice per commit. No parallel VM/runtime stack. Security (fs/net/env/subprocess/ffi capability checks) and precise GC rooting are invariant across all slices.

---

## Slice ladder

Order follows the plan's *minimal implementation sequence* (§5). Each slice is gated by the [verification contract](#verification-contract).

| # | Slice | Codename | Status | Entry blocked on |
|---|-------|----------|--------|------------------|
| 0 | Engine Lab — measurement, differential testing, progress scaffold | `OtterLab` | **done** (commit `98f460b3`) | — |
| 1 | Stable VM stack & frame descriptors | `HoltStack` | **done** (1b; 1e folds into Slice 2) | Slice 0 green ✓ |
| 2 | PupJIT direct calls + machine-code frame build | `PupJIT Calls` | **next** | HoltStack stable ✓ |
| 3 | Unified feedback vectors + complete ICs | `WhiskerIC` | not started | PupJIT direct calls |
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
- [ ] *(deferred to a follow-up Slice-0 commit, not required for done)* per-run engine counters surfaced to the CLI (IC hit/miss, direct-call hit/miss, Rust-stub calls, alloc/GC time, deopts, code size, compile latency). Accessors already exist in-VM (`Interpreter::property_ic_stats`, `Interpreter::runtime_budget_stats`, GC `ScavengeStats`) but are not yet plumbed to a `run`-time dump. Tracked in [Counters](#counters-status).

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
- **Stats accessors** — `property_ic_stats()` **1166–1167**; `runtime_budget_stats()` **1846–1847** (+ `reset_runtime_budget_stats` 1851).
- **JIT counters today** — per-frame back-edge counter + `osr_disabled_headers` set + `jit_code` cache; **no compile-count / direct-call meters yet** (Slice 0 counters work will add them).

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

In-VM accessors that already exist and what they cover; the Slice-0 follow-up will surface a subset to the CLI behind an env flag (e.g. `OTTER_STATS=1`) printing one machine-readable line to stderr after `run`, with **no semantic change**:

| Signal (plan §Phase 0) | Source today | Plumbed to CLI? |
|---|---|---|
| IC hit/miss/install/disable (load/store/has) | `Interpreter::property_ic_stats()` → `PropertyIcStats` | no |
| bytecode / native / construct call counts | `Interpreter::runtime_budget_stats()` → `RuntimeBudgetStats` | no |
| reductions, per-turn alloc bytes, max turn nanos | `RuntimeBudgetStats` | no |
| GC collections / time / promoted bytes | scavenger `ScavengeStats` | no |
| tier used / direct-call hit/miss / Rust-stub calls | **not yet counted** | no |
| deopts / code size / compile latency | **not yet counted** (no deopt/optimizer tier yet) | no |

---

## Next-slice design note — Slice 1: `HoltStack`

> Preview only. **Do not implement yet.** Entry is gated on Slice 0 green.

### Objective
Replace `SmallVec<[Frame; 8]>` as the execution stack with a segmented, **stable-address** `HoltStack`, creating the substrate compiled code needs for machine-code calls (Slice 2) and, later, optimizer deopt (Slices 8–9).

### Stable VM stack / frame descriptor substrate
New concepts (Otter naming): `HoltSegment`, `HoltFrameHeader`, `HoltValueSlots`, `HoltFrameDesc`, `HoltColdRef`, `HoltParkedSnapshot`. A growable Rust collection is not a machine ABI: compiled callees need pinned frame and register-slot addresses that don't move when the stack grows. Segmented allocation gives stable addresses within a segment; growth adds a segment instead of reallocating live frames.

### Why the stack must stay disjoint from `Interpreter`
The current design keeps the frame stack reachable as a separate object so JIT reentry can hold `&mut vm` and `&mut stack` simultaneously without aliasing UB (`run_compiled_frame` already threads `vm` and `stack` as disjoint borrows — lib.rs:1417). If `HoltStack` became a field of `Interpreter`, every compiled-reentry path that needs both would alias `&mut Interpreter`, which is unsound. HoltStack therefore stays a standalone reentry object passed alongside the VM, preserving the existing disjoint-borrow invariant (plan risk register: "Stack aliasing").

### How current `SmallVec<Frame>` root tracing is preserved initially (Stage A)
`Frame::trace_frame_slots` (frame_state.rs:452–471) and the `FrameRoots`/`FrameRootProviders` registry (gc/frame_roots.rs:20–71, heap.rs:285–306) are the precise root path today. Slice 1 keeps **Stage A: full initialized frame-window tracing** — HoltStack publishes a `FrameRoots` provider that traces every published frame's value slots exactly as `trace_frame_slots` does now. No `StoneMaps` precise safepoint maps yet (that is Slice 5, required before any GC-bearing `Value` may live only in a machine register across a safepoint). Frame publication is **two-phase**: initialize every slot to `Value::undefined()`, *then* publish; no allocation may occur while a partially-initialized frame is visible to GC (plan §Phase 1 GC safety + risk register "Frame publication").

### Rollback flag / strategy
Land behind `OTTER_HOLT_STACK` with the old `SmallVec<Frame>` stack retained as an adapter; `OTTER_HOLT_STACK=0` restores the legacy path during rollout. Remove the adapter only after the new path is stress-clean and test262-parity-clean.

### test262 dirs that must be gated (call/frame slice, plan §6.7)
`language/expressions/call`, `language/statements/function`, `language/expressions/function`, arrow/async/generator function dirs, `language/expressions/super`, `language/statements/try`, `language/expressions/yield`, `language/expressions/await`, plus generator/async-function builtins. The failing set must be identical interp vs JIT and identical `OTTER_HOLT_STACK=0` vs `=1` before the adapter is removed.

### Files in scope
`crates/otter-vm/src/{jit.rs, lib.rs, call_ops.rs, frame_state.rs, generator.rs, cold_frame.rs}`, `crates/otter-gc/src/frame_roots.rs`. Suggested eventual layout (apply gradually, plan §8): `crates/otter-vm/src/holt_stack/{mod,frame,segment,roots,snapshot}.rs`.

### Primary risks (plan §7)
Tracing garbage on wrong frame publication; async/generator snapshot loss on yield/await; exception/finally unwind PC mismatch. Mitigations: two-phase publish + debug initialized-slot bitmap; dedicated generator/async test262 dirs gated before enabling; keep protected/finally paths on the cold-frame path until `HoltFrameDesc` descriptors are ready.
