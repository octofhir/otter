# OtterJS: большой breaking-refactor план до конкурентного production-движка

**Версия:** 2026-06-16  
**Репозиторий:** `octofhir/otter`  
**Назначение документа:** архитектурный план, который переводит OtterJS из состояния “быстрый self-contained Rust JS engine в разработке” в состояние “конкурентный production JavaScript engine”.  
**Правило нейминга:** внутренние tiers, JIT-компоненты и подсистемы получают имена в тематике выдры/реки/холта/камней/раковин. В коде, документации, ветках и задачах не использовать названия чужих движков или их tier-ов как имена компонентов Otter.

---

## 0. Executive summary

Цель не ограничивается ускорением `fib` или `prop-access`. Цель — сделать OtterJS конкурентным во всех ключевых аспектах движка:

- скорость исполнения горячего JS-кода;
- cold start и latency;
- память;
- GC throughput и pause time;
- качество IC и JIT tiering;
- Arrays, TypedArrays, Strings, JSON, RegExp, Promises, Modules;
- conformance;
- observability;
- безопасность host capabilities;
- удобство embedded/server deployment.

Текущий код уже имеет сильный фундамент: register-bytecode interpreter, baseline arm64 JIT, loop OSR, moving generational GC, pointer compression, precise roots, `#[repr(C)]` object fast layout и inline own-data property load. Но архитектурный потолок остаётся: текущий baseline JIT всё ещё ходит в Rust per JS call, не имеет полноценного machine-code frame-build/call ABI, не держит GC-bearing `Value` в машинных регистрах через safepoints, имеет неполные IC fast paths, а optimizer tier ещё отсутствует.

Главная программа refactor-а:

1. **Engine Lab** — измеримость, benchmark matrix, differential testing.
2. **HoltStack** — стабильный VM stack и frame descriptors.
3. **PupJIT** — настоящий baseline JIT с direct JS calls и machine-code frame-build.
4. **WhiskerIC** — полный IC/feedback subsystem: property, method, element, global, call, construct.
5. **KelpHeap** — JIT-readable hot heap layouts: Object, Array, TypedArray, String, Closure, Function.
6. **TideGC + StoneMaps** — production GC, precise stack maps, JIT allocation, write barriers.
7. **ShellBuiltins** — optimized builtin intrinsics.
8. **RippleRegex** — first-class RegExp engine.
9. **DiveJIT** — mid-tier optimizing compiler.
10. **DeepDiveJIT** — peak optimizer with deopt, inlining, scalar replacement and advanced specialization.
11. **PebbleBytecode + snapshots** — JIT-friendly bytecode metadata, startup snapshot, code cache.
12. **TideLoop** — async/event loop/module runtime hardening.
13. **Scout** — debugger, profiler, heap/IC/deopt observability.
14. **RaftRelease** — multi-platform JIT, fuzzing, release hardening.

Минимальная дорога к “Otter уже ощущается конкурентным”: **Engine Lab → HoltStack → PupJIT direct calls → WhiskerIC → KelpHeap arrays/strings/typed arrays → TideGC/StoneMaps → ShellBuiltins**.

Дорога к “Otter может биться с лидерами на горячем коде”: добавить **DiveJIT** и затем **DeepDiveJIT**.

---

## 1. Naming policy: выдровая тематика для всех tier-ов и подсистем

### 1.1. Запрещённые naming patterns

Не использовать в новых именах компонентов, модулей, веток, задач и документов:

- названия чужих JS-движков;
- названия их JIT tier-ов;
- прямые аналоги вроде `spark`, `mag`, `turbo`, `ion`, `dfg`, `ftl`;
- любые `external-engine-style` labels как имена модулей.

Допустимо в отдельном research-документе говорить “industry baseline tier” или “browser-engine pattern”, но внутренние имена Otter должны быть самостоятельными.

### 1.2. Предлагаемый словарь имён Otter

| Имя | Что означает | Rust/module suggestion |
|---|---|---|
| **PebbleBytecode** | bytecode + metadata format | `pebble_bytecode`, `otter-pebble` |
| **HoltStack** | stable VM stack, frame headers, frame descriptors | `holt_stack` |
| **PupJIT** | fast single-pass baseline JIT | `pup_jit`, `pup.rs` |
| **WhiskerIC** | feedback vector + inline caches | `whisker_ic` |
| **RaftTier** | tiering coordinator, code cache, OSR policy | `raft_tier` |
| **KelpHeap** | JIT-readable heap layouts and layout descriptors | `kelp_heap` |
| **TideGC** | production GC workstream | `tide_gc` |
| **StoneMaps** | precise safepoint stack maps | `stone_maps` |
| **ShellAlloc** | JIT allocation fast paths | `shell_alloc` |
| **ShellBuiltins** | optimized builtins and intrinsic lowering | `shell_builtins` |
| **RippleRegex** | RegExp parser/interpreter/JIT subsystem | `ripple_regex` |
| **DiveJIT** | mid-tier optimizing JIT | `dive_jit` |
| **DeepDiveJIT** | peak optimizer | `deep_dive_jit` |
| **TideLoop** | event loop, async, microtasks, module scheduling | `tide_loop` |
| **Scout** | profiler/debugger/observability | `scout` |
| **RaftRelease** | platform, fuzzing, CI, release hardening | `raft_release` |

### 1.3. Naming rules for future PRs

- New JIT tiers must be named `PupJIT`, `DiveJIT`, `DeepDiveJIT`.
- New IC code should live under `WhiskerIC` vocabulary: `WhiskerSite`, `WhiskerVector`, `WhiskerStub`, `WhiskerPatch`.
- Stack maps should be `StoneMap`, `StoneSlot`, `StoneSafepoint`.
- VM stack should use `HoltFrame`, `HoltSegment`, `HoltFrameDesc`.
- Tiering coordinator should use `RaftTier`, `RaftCodeCache`, `RaftOSR`.
- Builtins should use `ShellBuiltin`, `ShellStub`, `ShellIntrinsic`.

---

## 2. Current-state facts verified in repository

These are source anchors to keep the plan tied to code rather than notes.

| Claim | Source anchor |
|---|---|
| Baseline JIT uses interpreter frame register array as precise root set and avoids stack maps in current tier | `crates/otter-vm/src/jit.rs`, lines 21-28 |
| Baseline JIT call stub reconstructs VM/context/stack and delegates to `Interpreter::jit_runtime_call` | `crates/otter-jit/src/baseline.rs`, lines 74-160 |
| Current compiled-to-compiled “fast” call still builds a Rust `Frame` and inner stack before running compiled callee | `crates/otter-vm/src/lib.rs`, lines 1430-1655 |
| `ObjectBody` is already `#[repr(C)]`, has fixed shape field and inline data-value slots | `crates/otter-vm/src/object.rs`, lines 380-529 |
| VM already bakes inline own-data property loads from monomorphic ICs | `crates/otter-vm/src/lib.rs`, lines 1700-1772 |
| Baseline emitter already emits inline own-data `LoadProperty` shape guard and fixed-offset value load | `crates/otter-jit/src/baseline.rs`, lines 1108-1184 |
| Active frame roots are precise `FrameRoots` providers, not conservative native-stack scan | `crates/otter-gc/src/frame_roots.rs`, lines 23-60 |
| `Frame::trace_frame_slots` traces the whole register window and frame GC fields | `crates/otter-vm/src/frame_state.rs`, lines 452-474 |
| Write barrier is parent-header/card based and must not derive cards from malloc-owned slot addresses | `crates/otter-gc/src/barrier.rs`, lines 32-40 and 51-103 |
| `PeltField for Arc<T>` is a no-op; GC-bearing `Arc` payloads need manual trace | `crates/otter-vm/src/pelt.rs`, lines 153-162 |

Immediate correction to earlier design notes: **the hot object layout unlock has already partially landed**. The next bottleneck is not “make `ObjectBody` repr(C)” but “finish the IC/call/JIT integration around the layout”.

---

## 3. Architecture North Star

### 3.1. Execution tiers

Otter should converge to four execution modes:

1. **Interpreter**  
   Correctness oracle, cold code, debug mode, fallback target, feedback collection.

2. **PupJIT**  
   Fast baseline JIT: one pass, low compile latency, direct calls for common JS functions, inline ICs, no aggressive speculation at first.

3. **DiveJIT**  
   Mid-tier optimizer: SSA, type feedback, common inlining, typed arithmetic, property specialization, bounds-check elimination.

4. **DeepDiveJIT**  
   Peak optimizer: aggressive inlining, deopt, scalar replacement, escape analysis, allocation sinking, loop optimization, typed array/vector opportunities.

### 3.2. Stack/frame model

The VM should stop treating Rust `SmallVec<Frame>` as the core machine model. It should use **HoltStack**:

- stable segmented stack;
- explicit `HoltFrameHeader`;
- separate `Value` slots;
- cold side records;
- frame descriptors;
- safepoint descriptors;
- snapshot/restore path for async/generator parking.

The stack must remain **logically separate from `Interpreter`** to preserve disjoint `vm`/`stack` JIT reentry and avoid aliasing UB.

### 3.3. GC-rooting strategy

Two stages:

- **Stage A:** full initialized frame-window tracing. Safer, supports direct calls first.
- **Stage B:** `StoneMaps` precise safepoint maps. Required before keeping GC-bearing `Value` in machine registers across safepoints and before optimizing tiers.

No conservative stack scan. Pointer compression and moving young gen require precise root slots that can be rewritten by GC.

### 3.4. Object/runtime model

Hot heap bodies must be JIT-readable:

- fixed layout;
- pinned offsets;
- stable layout descriptors;
- shape/prototype/version guards;
- write barrier hooks;
- cold fallback for accessors/proxies/dictionary/exotics.

### 3.5. Runtime model

Host capabilities remain Rust-boundary checked. JIT may inline pure JS/runtime operations, but must not bypass permission checks for fs/net/env/subprocess/ffi.

---

## 4. Phase plan

## Phase 0 — Engine Lab

**Codename:** `OtterLab`  
**Objective:** make every perf/correctness claim measurable.

### Work

- Rebuild benchmark matrix:
    - interpreter;
    - PupJIT;
    - forced OSR;
    - external baseline runner;
    - optional browser-shell baselines if available in CI.
- Categorize benchmarks:
    - call/recursion;
    - property/method;
    - arrays;
    - typed arrays;
    - strings;
    - JSON;
    - RegExp;
    - promises/async;
    - modules/startup;
    - compiler-style workloads.
- Add per-run counters:
    - tier used;
    - IC hit/miss;
    - direct-call hit/miss;
    - Rust stub calls;
    - allocations;
    - GC time;
    - deopts;
    - code size;
    - compile latency.
- Differential runner:
    - output equality JIT on/off;
    - interpreter vs JIT result equality;
    - randomized JS snippets;
    - GC stress;
    - JIT stress;
    - later deopt stress.

### Done means

- One command regenerates `benchmarks/results/latest.md`.
- One command proves all `benchmarks/scripts/*.js` output-identical JIT on/off.
- Every later phase reports before/after.

### Expected speedup

None. This phase prevents fake wins.

---

## Phase 1 — HoltStack: stable VM stack and frame descriptors

**Codename:** `HoltStack`  
**Objective:** create the substrate for machine-code calls and optimizer deopt.

### Breaking change

Replace `SmallVec<[Frame; 8]>` as the execution stack with a segmented stable-address `HoltStack`.

New concepts:

- `HoltSegment`;
- `HoltFrameHeader`;
- `HoltValueSlots`;
- `HoltFrameDesc`;
- `HoltColdRef`;
- `HoltParkedSnapshot`.

### Why this shape

Compiled code needs stable frame and register-slot addresses. A growable Rust collection is not a good machine ABI. The stack must also remain separate from the `Interpreter` object to maintain the existing disjoint reentry invariant.

### Files touched

- `crates/otter-vm/src/jit.rs`
- `crates/otter-vm/src/lib.rs`
- `crates/otter-vm/src/call_ops.rs`
- `crates/otter-vm/src/frame_state.rs`
- `crates/otter-vm/src/generator.rs`
- `crates/otter-vm/src/cold_frame.rs`
- `crates/otter-gc/src/frame_roots.rs`

### GC safety

- Every published frame slot is initialized to `Value::undefined()`.
- Frame publish is two-phase: initialize first, publish second.
- No allocation may occur while a partially initialized frame is visible to GC.
- The frame root provider traces all published frames.

### Risks

- tracing garbage if frame publication is wrong;
- async/generator snapshot bugs;
- exception unwind mismatch.

### Rollback

Keep old stack adapter behind `OTTER_HOLT_STACK=0` during rollout.

---

## Phase 2 — PupJIT direct calls and machine-code frame build

**Codename:** `PupJIT Calls`  
**Objective:** remove the per-JS-call Rust bridge floor.

### Breaking change

PupJIT call emission should no longer always emit `jit_call_stub`. For simple JS calls, compiled caller emits:

1. guard callee kind/code pointer;
2. reserve callee frame on HoltStack;
3. initialize frame slots;
4. bind args/receiver;
5. publish frame;
6. branch/call to compiled callee entry;
7. on return, store result into caller destination;
8. pop callee frame.

Cold cases still use Rust bridge.

### Initial direct-call eligibility

- ordinary bytecode function or closure;
- installed PupJIT code;
- no async/generator;
- no rest/arguments;
- no direct eval;
- no derived constructor;
- no active protected/finally path until descriptors are ready;
- no host/native/capability call;
- argc within fast binding shape.

### Expected perf

- `fib`: target ~2-3x of external baseline instead of current high single-digit gap.
- call/arith kernels: near competitive before inlining.
- profile: `jit_runtime_call` must stop being dominant.

### GC safety

Stage A still spills all live GC-bearing values into initialized frame slots before safepoints. Return value may live in a machine register only between callee return and immediate store, with no safepoint in between.

### Risks

- exception/finally unwinding;
- partially initialized callee frame;
- wrong receiver/sloppy-this behavior;
- recursion stack overflow policy drift.

### Rollback

Env flag `OTTER_PUP_DIRECT_CALLS=0` restores old bridge path.

---

## Phase 3 — WhiskerIC: unified feedback vectors and complete ICs

**Codename:** `WhiskerIC`  
**Objective:** make property/method/element operations hot-path native.

### Breaking change

Move from ad-hoc per-op IC bake to unified `WhiskerVector` per function.

Feedback site kinds:

- `WhiskerLoadSite`;
- `WhiskerStoreSite`;
- `WhiskerHasSite`;
- `WhiskerMethodSite`;
- `WhiskerElementSite`;
- `WhiskerGlobalSite`;
- `WhiskerCallSite`;
- `WhiskerConstructSite`;
- `WhiskerBinarySite`.

### PupJIT IC fast paths

- own data load;
- direct-prototype data load;
- polymorphic load chain;
- own data store with write barrier;
- method load + direct call;
- dense array element load/store;
- typed array element load/store;
- global lexical/global object load;
- megamorphic machine-code stubs.

### Expected perf

- `prop-access`: single-digit, then 2-5x.
- `array-ops`: major improvement.
- `sort`: improves through callback calls and method/property ICs.
- object-oriented code: no Rust stub per hot property access.

### GC safety

Inline stores must either call a barrier stub or inline the parent-header/card barrier. Never compute card from a malloc-owned slot address.

### Risks

- stale IC after delete/shape transition;
- accessor/proxy semantics bypass;
- missed barrier;
- megamorphic site too eager or too late.

### Rollback

Per-site disable to megamorphic stub; per-function recompile fallback; global `OTTER_WHISKER_IC=0` for emergency.

---

## Phase 4 — KelpHeap: hot heap layouts

**Codename:** `KelpHeap`  
**Objective:** every hot heap body gets fixed JIT-readable layout and layout descriptors.

### Object

Already has key foundation:

- `#[repr(C)]`;
- shape at fixed offset;
- inline values;
- overflow values;
- dictionary fallback.

Next work:

- direct-prototype versioning;
- inline store layout;
- method IC integration;
- shape invalidation API;
- debug layout verifier.

### Array

Needed:

- packed elements;
- holey elements;
- double elements;
- dictionary elements;
- length tracking;
- prototype pollution guards;
- fast array builtin hooks.

### TypedArray/DataView

Needed:

- stable header descriptors;
- buffer pointer/length/kind offsets;
- detached buffer guard;
- bounds-check elimination hooks;
- endian-aware DataView stubs.

### String

Needed:

- flat one-byte/two-byte strings;
- ropes/cons strings;
- substring/slice views;
- flattening policy;
- fast indexing;
- string builder for concat;
- optimized common methods.

### Closure/Function

Needed:

- JIT-readable closure body;
- cached code pointer;
- feedback vector pointer;
- context/upvalue descriptor;
- direct-call target cell.

### Expected perf

- typed-array: target <=3x.
- string-ops: target <=5x first, then <=3x.
- array-ops: <=3-5x.
- JSON improves through object/array/string improvements.

---

## Phase 5 — TideGC and StoneMaps

**Codenames:** `TideGC`, `StoneMaps`, `ShellAlloc`  
**Objective:** production GC and safe register-resident JIT values.

### Breaking changes

- Add `StoneMapTable` to JIT code.
- Each safepoint has `StoneSafepointId`.
- Stack map entries describe:
    - `Value64` slot;
    - `RawGc32` slot;
    - spilled object pointer slot;
    - frame slots already covered by HoltStack.
- Add JIT allocation fast paths through `ShellAlloc`.
- Add inline write barrier slow/fast split.
- Add incremental marking hooks.
- Add GC scheduling policy.

### Strategy

Do not enable register-resident GC-bearing values across safepoints until StoneMaps passes stress. PupJIT direct calls can ship first with full frame-window tracing.

### Expected perf

- 1.2-1.8x additional on call/numeric loops after direct calls.
- allocation-heavy workloads improve after JIT allocation.
- lower GC overhead and fewer unnecessary spills.

### Risks

- silent use-after-move;
- wrong stack-map slot kind;
- missed reload after moving GC;
- barrier omission.

### Done means

- GC stress with JIT allocation and direct calls is clean.
- Synthetic relocation tests pass.
- Every safepoint can be dumped and audited.

---

## Phase 6 — ShellBuiltins

**Codename:** `ShellBuiltins`  
**Objective:** make standard library operations competitive.

### Breaking change

Introduce builtin lowering and fast intrinsic layer. Each builtin has:

- spec-correct generic path;
- guardable fast path;
- PupJIT stub;
- DiveJIT intrinsic;
- DeepDiveJIT lowering;
- fallback path.

### Priority builtins

1. `Array.prototype.map/filter/reduce/forEach/sort`
2. `Object.keys/values/entries`
3. `String.prototype.slice/includes/startsWith/charCodeAt/indexOf`
4. `JSON.parse/stringify`
5. `TypedArray.prototype.*`
6. `Map/Set` hot operations
7. `Promise` combinators and microtask paths

### Security rule

Host capability builtins are not pure intrinsics. File/network/env/subprocess/ffi access must stay behind explicit permission checks.

### Expected perf

- JSON <=3-5x.
- array builtins <=3x.
- promise/async microbenchmarks <=5x first.
- TypeScript/compiler workloads improve through arrays/strings/maps.

---

## Phase 7 — RippleRegex

**Codename:** `RippleRegex`  
**Objective:** make RegExp first-class, not a separate huge gap.

### Work

- ECMAScript RegExp parser to AST.
- Regex bytecode interpreter.
- Literal regex cache.
- Fast one-pass paths where possible.
- Backtracking VM for full semantics.
- Optional regex JIT for hot expressions.
- ReDoS/time-budget integration.
- Integration with string methods.

### Expected perf

- regex gap <=5-10x first.
- common literal/search patterns near competitive.

### Risks

- catastrophic backtracking;
- unicode/capture semantics;
- replacement semantics;
- sticky/global lastIndex bugs.

---

## Phase 8 — DiveJIT: mid-tier optimizer

**Codename:** `DiveJIT`  
**Objective:** warm-code optimizer without peak-tier compile cost.

### Required prerequisites

- HoltStack stable.
- WhiskerVector feedback.
- StoneMaps working.
- Deopt metadata model.
- IC invalidation/recompile policy.

### IR features

- SSA values;
- basic blocks;
- effect/control ordering;
- type feedback;
- shape feedback;
- bounds-check info;
- call target feedback;
- deopt states.

### Optimizations

- typed integer/double arithmetic;
- common inlining;
- property load/store specialization;
- bounds-check elimination;
- typed-array specialization;
- simple loop-invariant code motion;
- basic escape analysis.

### Expected perf

- numeric/call/property hot functions <=2-3x.
- fewer baseline stores/loads.
- less overhead in long-running warm code.

### Risks

- wrong speculation;
- bad deopt reconstruction;
- compile latency too high.

---

## Phase 9 — DeepDiveJIT: peak optimizer

**Codename:** `DeepDiveJIT`  
**Objective:** best hot-code performance.

### Work

- advanced inlining;
- scalar replacement;
- allocation sinking;
- range analysis;
- loop optimization;
- typed array/vector opportunities;
- object allocation folding;
- deeper escape analysis;
- polymorphic specialization;
- tier-aware code cache eviction.

### Backend strategy

Prototype with a backend abstraction. Keep the option open between:

- using an existing Rust-native codegen backend for selected optimized code;
- building a JS-specific backend if deopt/GC/NaN-box integration becomes too awkward.

Internal names remain Otter names either way: `DiveJIT` and `DeepDiveJIT`.

### Expected perf

- call/arith kernels near parity.
- numeric loops 1-2x.
- monomorphic property kernels near parity.
- typed arrays close to external baselines.

---

## Phase 10 — PebbleBytecode, snapshots and startup

**Codename:** `PebbleBytecode`  
**Objective:** make bytecode and startup JIT-friendly.

### Breaking changes

- Add explicit metadata tables:
    - register liveness;
    - call descriptors;
    - exception tables;
    - feedback site ids;
    - safepoint ids;
    - source spans;
    - deopt environment descriptors.
- Add startup snapshot:
    - global object;
    - builtins;
    - intrinsic shapes;
    - common strings/symbols;
    - prewarmed feedback where safe.
- Add code cache.
- Add module graph cache.

### Expected perf

- faster CLI startup;
- lower repeated startup cost;
- less bootstrap allocation;
- better embedded mode.

---

## Phase 11 — TideLoop: async, event loop, modules, host runtime

**Codename:** `TideLoop`  
**Objective:** production runtime behavior beyond CPU kernels.

### Work

- Promise fast paths.
- Async function resume optimization.
- Microtask queue profiling and optimization.
- Module loader cache.
- Dynamic import correctness and caching.
- Timer/event loop policy.
- Worker/task isolation model if required.
- Capability boundary tests.

### Rule

Core JS VM optimizations must not bypass host capabilities. Host operations stay explicit and auditable.

---

## Phase 12 — Scout: debugger, profiler, observability

**Codename:** `Scout`  
**Objective:** make Otter diagnosable in production.

### Work

- Stack walking across interpreter/PupJIT/DiveJIT/DeepDiveJIT.
- CPU profiler with JS frames.
- Heap profiler.
- Allocation profiler.
- IC profiler.
- Deopt profiler.
- JIT code map.
- Disassembler.
- IR dumps.
- Trace events.
- Perfetto/Chrome trace export.
- Crash diagnostics.

### Done means

A user can answer:

- why a function did not tier up;
- why an IC went megamorphic;
- where allocations happen;
- where GC pauses happen;
- why deopt fires;
- why a benchmark regressed.

---

## Phase 13 — RaftRelease: platform, fuzzing, release hardening

**Codename:** `RaftRelease`  
**Objective:** make the engine shippable across platforms.

### Work

- arm64 macOS first-class.
- x64 Linux/macOS.
- arm64 Linux.
- Windows x64.
- JITless mode.
- W^X executable memory discipline.
- Fuzzers:
    - parser;
    - bytecode verifier;
    - interpreter vs JIT differential;
    - GC stress;
    - RegExp;
    - JSON;
    - async;
    - deopt.
- CI matrix.
- Release profile tuning.

---

## 5. Minimal implementation sequence

If the team wants the shortest path to broad competitiveness, execute in this order:

1. **OtterLab** — measurement and gates.
2. **HoltStack** — stable frame stack.
3. **PupJIT direct calls** — remove JS-call bridge ceiling.
4. **WhiskerIC load/store/method/element** — remove object/property ceiling.
5. **KelpHeap arrays/typed arrays/strings** — fix common JS data structures.
6. **TideGC + StoneMaps** — safe register-resident values and JIT allocation.
7. **ShellBuiltins** — arrays/strings/JSON/promises fast paths.
8. **RippleRegex** — close regex gap.
9. **DiveJIT** — mid-tier optimizer.
10. **DeepDiveJIT** — peak optimizer.
11. **PebbleBytecode snapshots** — startup/code cache.
12. **TideLoop + Scout + RaftRelease** — production runtime and release hardening.

### Milestone A: credible engine

- All tracked benchmarks output-identical JIT on/off.
- No JIT/interpreter test262 failing-set delta.
- Call/property/array/typed-array/string/JSON <=10x.
- GC stress clean at supported strides.
- JITless mode remains correct.

### Milestone B: competitive engine

- Call/arith: 1-3x.
- Property/array/typed-array/string/JSON: <=3-5x.
- RegExp: <=5-10x.
- Startup competitive for small scripts.
- Memory lower than heavy external runtimes on embedded workloads.

### Milestone C: strategically better engine

Otter does not need to beat every mature browser engine on every mega-workload to be better for its target users. It can win on:

- self-contained deployment;
- Rust-native integration;
- secure capability sandbox;
- predictable embedded/server behavior;
- observability;
- lower memory for isolated workloads;
- good enough hot-code performance.

---

## 6. Verification contract for every breaking slice

Every slice must land as a revertible commit. No “big bang”.

Required gates:

1. `cargo fmt --all`
2. `cargo clippy --all-targets --all-features -- -D warnings`
3. `cargo test -p otter-vm -p otter-jit`
4. Benchmark output equality JIT on/off for all `benchmarks/scripts/*.js`.
5. test262 failing-set identical JIT on/off for touched dirs.
6. For GC-touching slices: `OTTER_GC_STRESS` at supported usable strides.
7. For call/frame slices: function/call/closure/generator/async/super/try dirs.
8. For property slices: object/reflect/proxy/accessor/delete/array callback dirs.
9. For builtin slices: affected builtins plus Array safe runner where needed.
10. For security-touching slices: capability tests for fs/net/env/subprocess/ffi.

---

## 7. Risk register

| Risk | Failure mode | Mitigation |
|---|---|---|
| Moving GC + machine registers | use-after-move if `Value` lives only in register across safepoint | full frame spill first; StoneMaps before register-resident GC values |
| Frame publication | GC traces uninitialized slot | two-phase publish; debug initialized-slot bitmap |
| Write barrier omission | young object lost when stored into old object | inline/call parent-header barrier on every heap-pointer store |
| Stack aliasing | UB if stack becomes field of Interpreter | HoltStack remains separate reentry object |
| Async/generator parking | lost locals/cold state on yield/await | snapshot adapter and dedicated test262 dirs before enabling direct path |
| Exception/finally | direct call skips finally or resumes wrong PC | call-site descriptors; initially disable direct calls under protected sites |
| IC staleness | wrong property value after shape/delete/prototype change | shape/version guards, invalidation, megamorphic fallback |
| Proxy/accessor semantics | JIT bypasses observable JS behavior | never cache proxy/accessor as data fast path; guard and fallback |
| Capability bypass | JIT invokes host op without permission | host/native/capability calls remain Rust-boundary checked |
| `Arc<T>` hidden GC slots | Pelt skips GC-bearing Arc payload | forbid or hand-trace GC-bearing Arc fields |
| Optimizer early | deopt/stack maps built on unstable ABI | DiveJIT blocked on HoltStack, WhiskerIC and StoneMaps |
| Compile latency | optimizer slows short scripts | tier thresholds, code cache, fallback, per-function disable |
| Cross-platform JIT | platform-specific crashes | JITless mode, CI matrix, W^X discipline, fuzzing |

---

## 8. Suggested repository organization

This is a possible organization; apply gradually, not in one commit.

```text
crates/otter-vm/src/
  holt_stack/
    mod.rs
    frame.rs
    segment.rs
    roots.rs
    snapshot.rs
  whisker_ic/
    mod.rs
    vector.rs
    property.rs
    element.rs
    call.rs
    global.rs
  pebble/
    descriptors.rs
    liveness.rs
    safepoints.rs
  kelp_heap/
    layout.rs
    object_layout.rs
    array_layout.rs
    typed_array_layout.rs
    string_layout.rs

crates/otter-jit/src/
  pup/
    mod.rs
    arm64.rs
    call.rs
    ic.rs
    stubs.rs
  dive/
    mod.rs
    ir.rs
    lower.rs
    deopt.rs
  deep_dive/
    mod.rs
    opt.rs
    backend.rs
  stone_maps.rs
  raft_tier.rs

crates/otter-runtime/src/
  tide_loop/
    mod.rs
    microtasks.rs
    modules.rs
    timers.rs
    capabilities.rs

crates/otter-regexp/src/
  ripple/
    parser.rs
    bytecode.rs
    vm.rs
    jit.rs

crates/otter-tools/src/
  scout/
    profiler.rs
    heap.rs
    ic.rs
    deopt.rs
```

---

## 9. One-sentence strategy

**Make the VM/JIT/GC contract production-grade first (`HoltStack`, `PupJIT` direct calls, `WhiskerIC`, `StoneMaps`), then optimize data structures and builtins (`KelpHeap`, `ShellBuiltins`, `RippleRegex`), then add real optimizing tiers (`DiveJIT`, `DeepDiveJIT`), while every slice is gated by interpreter parity, GC stress, benchmark output equality and capability-security tests.**
