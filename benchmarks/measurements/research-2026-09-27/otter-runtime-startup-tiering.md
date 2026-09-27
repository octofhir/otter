# Runtime/host and tiering policy: startup, tier-up triggers and host overhead in fixed-work runs

The CLI never uses the snapshot restore; it runs a full bootstrap every time. All JIT compilation and GC run synchronously on the one isolate thread. The tiering policy is a per-function cost model with no fixed thresholds. Compile durations are measured by wall clock and feed later decisions.

## 1. Components and key files

| Component | Location | Role |
|---|---|---|
| CLI entry | `crates/otter-cli/src/main.rs:925` `#[tokio::main(flavor="multi_thread")]` | Default tokio worker pool (one per core). |
| Tier selection | `main.rs:930-936` | Default is `JitSelection::ProductionTiered`. `--jitless` gives Template only, `--interpreter` gives interpreter only. |
| File run | `main.rs:1175` `run_file_with_cwd` | Builds the runtime at `:1212`, runs at `:1214`, calls `std::mem::forget(otter)` at `:1257` so there is no teardown. |
| Builder | `main.rs:2346` `cli_otter_builder` | Adds node, otter and web APIs. |
| Project lookup | `main.rs:2393-2426` | `find_project_root_for_entry` stats upward. If no `package.json` is found it falls back to cwd and calls `otter_pm::resolve_installed_project`. |
| Measurement | `scripts/dev/fixed-work.py:73-77` | Runs `/usr/bin/time -l otter run <file>` with cwd = repo root. It counts every thread's retired instructions, including startup and compilation. |
| Isolate thread | `crates/otter-runtime/src/handle.rs:1556` | Named "otter-isolate", 16 MiB stack (`admission.rs:35`). Main thread blocks on `interrupt_rx.recv()` (`handle.rs:1578`) until bootstrap ends. |
| Event loop | `event_loop.rs:172-179` `TokioEventLoop::from_handle` | Spawns `TimerDriverOwner` and eagerly builds a `reqwest` client (`:516-523`). |
| Runtime build | `crates/otter-runtime/src/lib.rs:2995-3275` `build_from_config` | See §3.1. |
| Bytecode cache | `compile_cache.rs:186-200` (`~/…/Caches/otter/compiled`), used at `lib.rs:5646-5690` | Bootstrap sources only. User scripts are "deliberately left alone" (`lib.rs:5680-5683`). |
| Snapshot | `runtime_snapshot.rs`, `crates/otter-vm/src/snapshot.rs` | In-process only; "no byte representation" (`snapshot.rs:26-27`). The only restore caller is `crates/otter-test262/src/isolation.rs:95`. |
| Tier cost model | `crates/otter-vm/src/tier_policy.rs` | `TierCostModel`, `TierPolicy`, `optimizing_tier_decision_for` (`:447`). |
| Tier-up and OSR driver | `crates/otter-vm/src/interp/jit_call.rs` | Hooks at `:187`, `:265`, `:373`, `:428`, `:1098`, `:1159`, `:1219`, `:1311`, `:1379`. |
| Compile | `interp/jit_compile.rs:466` (Machine), `:700-800` (Template) | Synchronous `hook.compile_*` at `:527`/`:748`. |
| Code registry | `crates/otter-vm/src/jit_registry.rs` | Generations, entry cells, hot mailbox (`:331`). |
| Entry cell ABI | `native_abi/code_entry.rs` | `FunctionEntryCell`, `CodeEntryCell`. |
| Back-edge poll | `runtime_activation/control.rs:40-65`, `interp/stats.rs:204-230` | Batch size `JIT_BACKEDGE_POLL_BATCH = 4096` (`interp/init.rs:1395`). |
| Generated tiering code | `otter-jit/src/arm64/direct_call/tiering.rs:19-38`, `template/arm64.rs:2056-2100`, `entry/code.rs:60-200` | Direct-call counter, back-edge poll, interpreter-to-JIT entry. |

**Concurrency.** No `thread::spawn` exists in `otter-jit`, `otter-gc` or the non-test parts of `otter-vm`; `code_space.rs:1056` is a test. Both compilers run inline on the mutator (`jit_compile.rs:527`, `:748`). Moving compilation to background threads would not lower the metric, because `time -l` counts all threads.

### Benchmark scripts

| Workload | Path | What it stresses |
|---|---|---|
| ts | `benchmarks/results/slice11/ts-fixed.js` (2.53 MB) | TypeScript 1.0 compiling `compiler_input` 8× (`runTypescript`, `:445`). Very large code volume, so compile cost and tier-policy volume dominate. `__extends` class hierarchies, polymorphic properties, string scanning. |
| zlib | `slice11/zlib-fixed.js` (209 KB) | Emscripten zlib pulled in by `zlibEval` (indirect eval) of one big string (`:633`). 128 MiB `ArrayBuffer` (`TOTAL_MEMORY‖134217728`) with typed-array heap views, `\|0` int32 arithmetic, giant relooper functions. 12 × `runZlib`. |
| crypto | `slice11/crypto-fixed.js` (61 KB) | jsbn RSA with `am3` 28-bit digits, 60 × encrypt+decrypt. Dense small-int arrays, multiply-accumulate loops, prototype method calls. |
| fib | `benchmarks/results/calls/fib.js` | 5 × `fib(30)`, about 8.3M calls. Pure call/return. The self-reference is a wrapper-scope binding under the CJS wrapper; I expect it to be a captured cell load but did not dump bytecode. |
| mega_method | `calls/mega_method.js` | One `objs[i&7].f(i)` site, 8 classes, 20M calls. Exceeds `PROFILED_PROPERTY_PIC_CAPACITY=4` (`tier_policy.rs:42`) and exactly fills `PROFILED_CALL_TARGET_CAPACITY=8` (`:49`). |
| ast_ctor | `calls/ast_ctor.js` | 2M `new C(i)` through `_super.call(this, …)` chains over 24 subclasses. Megamorphic add-property stores and `.call` forwarding. |
| earley-boyer | `benchmarks/results/octane_fixed/earley-boyer-x1.js` | Scheme2js output, Earley 2500 + Boyer 200 iterations (`:396-399`, `:5081`). Cons-cell allocation, closures per call, `arguments`, recursion, GC. |

## 2. Data layouts

- **`FunctionEntryCell`** (16 B, asserted `code_entry.rs:281`): `generation_cell: AtomicU64` @0, `function_id: u32` @8, `param_count: u16`, `register_count: u16`.
- **`CodeEntryCell`** (88 B, asserted `:286-299`): `entry_addr` @0, `code_object_id` @8, `flags` @16, `generated_stack_frame_bytes` @20, `active_count` @24, pad @28, `VmFrameHeader` (12 B) @32, `generated_tiering_break_even` @48, `generated_tiering_enabled: Cell<u32>` @56, `generated_entries` @64, `deopts` @72, `throws` @80. The counters are plain `Cell` fields under the single-mutator rule (`:127-131`).
- **Frame and context structs:**
  - `NativeFrame`: 72 B (`frame.rs:407`).
  - `VmThread`: 96 B, twelve u64 pointers to isolate-invariant cells (`frame.rs:34-67`, `:404`).
  - `JitCtx`: 136 B (`otter-jit/src/entry/abi.rs:330`).
  - `CodeRegistryView`: 24 B (`safepoints.rs:256`), including the single-slot `hot_function` mailbox.
- **Interpreter tier state.** There is no dense record. Separate `FxHashMap`/`FxHashSet` fields are keyed by function id or `(fid, pc)` (`crates/otter-vm/src/lib.rs:1189-1240` and following): `jit_call_counts`, `jit_entry_bail_counts`, `jit_feedback_refresh_attempted`, `jit_pending_direct_targets`, `jit_relinked_direct_targets`, `jit_osr_disabled`, `jit_osr_counts`, `jit_code`, `jit_optimized_code`, `jit_entry_osr_only`, `jit_template_osr_fids`, `jit_optimized_declined_epoch` and `jit_optimized_exit_profiles`. On top of these sit `TierPolicy` with three maps (`tier_policy.rs:345-349`) and the registry maps `codes`, `entry_cells`, `function_entry_cells`, `generated_feedback_seen` and `epochs` (`jit_registry.rs:100-116`).

## 3. Hot paths

### 3.1 Startup before the first user bytecode

The path is always a full bootstrap:

1. The process execs; tokio's multi-thread runtime starts; clap parses arguments.
2. Filesystem walk for `package.json`, then `resolve_installed_project` (`main.rs:2399-2426`).
3. `RuntimeHandle::spawn` creates the event loop, including the reqwest client and timer driver, then spawns the isolate thread.
4. `Interpreter::with_string_heap_cap` (`interp/init.rs:83`): GC heap with a 2 GiB cage (`otter-gc/src/compressed.rs:52`), well-known symbols, error classes, `build_global_this`.
5. `set_tenure_all(true)` (`lib.rs:3016`). Global-class and extension native installs, then `process::install_global` (`:3143`).
6. The JIT compiler is installed at `:3175`, before any bootstrap JavaScript runs. Bootstrap JS therefore pays feedback recording and tier-up counting.
7. Realm installers (`:3227`), including the node-globals `install_script`.
8. Web extension JS (`:3242`): 120,575 bytes across `otter-web/src/web_{bootstrap,streams,fetch,urlpattern,console}.js`, loaded as bytecode from the disk cache when present.
9. Class glue (`:3254`), then `set_tenure_all(false)` (`:3264`).

Then the user file is loaded:

1. `run_file_with_context` (`lib.rs:6213`). A `.js` file with no `package.json` gets a **full OXC parse just to detect module syntax** (`:6252`).
2. `run_commonjs_file` (`:6519`) compiles and links an empty `<commonjs-root>` chunk (`:6535`).
3. `cjs_instantiate_file` leads to `create_commonjs_wrapper` (`eval_ops.rs:705-707`). It copies the whole body into a `format!` string and runs `compile_eval_source`, which is a **second parse plus compile**. None of this is cached.
4. Top-level declarations become wrapper locals and captured bindings. Node wraps CommonJS files the same way.

A memory note from 2026-09-27, which I did not re-measure, puts empty-script startup at **830M instructions for Otter versus 255M for Node**. On that basis startup is about 22% of fib's 3.85G total (`parity-2026-09-27/closure-tail-final/results.json`) and about 8% of mega_method's 10.15G.

### 3.2 Interpreted call with the JIT installed

Call hook at `dispatch.rs:338-354`: `record_call_attempt_feedback`, `record_ordinary_call_feedback`, then `maybe_dispatch_jit` (`jit_call.rs:187`).

1. `run_optimized_frame` (`:1098`) first calls `promote_hot_generated_callee`, which takes the mailbox.
2. `resolve_optimized_code_for_fid` (`:1159`):
   - dynamic call to `optimizing_tier_enabled`;
   - cache check plus `is_current_for_entry` (`jit_registry.rs:317`): `codes.get`, one `epochs.get` per dependency, `entry_cells.get`;
   - `jit_optimized_code.get`.
3. If nothing is installed, `optimizing_tier_decision_for` (`tier_policy.rs:447`) runs:
   - `generated_entries_for_function` (hash lookup) and `jit_call_counts.get`;
   - `code_space.feedback_epoch` (`code_space.rs:607`), which costs two `RwLock` reads, a binary search, and an `Arc` clone and drop (`:548-581`);
   - `TierPolicy.functions.entry` and `cumulative_compile_ns.get`.
   - When the first result is Promote, the decision runs a second time with residency included.
4. `resolve_jit_code_for_fid` (`:1219`): promote again, `note_jit_function_entry` (`:1476`), `feedback_refresh_due`, cache check plus `is_current_for_entry`, `jit_entry_osr_only`, `jit_code.get`.
   - If no code exists: Template cost decision; then `jit_code_residency()` (`jit_compile.rs:67-92`), which walks all installed code into a fresh `FxHashSet`; then compiles synchronously.

My estimate is 15–20 hash probes and about 6 atomic read-modify-writes per interpreted call to a function without Machine code. I did not measure this.

### 3.3 Interpreter to compiled entry

`run_compiled_frame` (`jit_call.rs:1699`) calls `for_function` and `VmRuntimeActivation::new`, then `run_entry` (`entry/code.rs:60-200`):

1. About 15 VM getter calls.
2. Builds `NativeFrame` (72 B) and `VmThread` (96 B, rebuilt on every entry at `:121-137`) and `JitCtx` (136 B) on the Rust stack.
3. Push-activation stub (`:155`), `with_native_eval_env_owner` closure, pop stub.
4. `finish_compiled_entry` (`:173`), which reaches `finish_compiled_entry_transaction` (`jit_call.rs:1311`). At the outermost level this calls `retire_unreferenced` and, if feedback is dirty, `reconcile_generated_feedback` (`:1379-1470`). The reconcile allocates an `FxHashMap` and a `Vec` and may compile.

### 3.4 Generated direct call, tiering part only

`tiering.rs:19-38`:
- Common path is 6 instructions: `ldr/add/str generated_entries`, then `ldr break_even; cmp; b.lo`.
- Past break-even it loads `generated_tiering_enabled` as well. Optimized generations sit past break-even permanently, so they pay 8 instructions per call.
- Hot requests go into one mailbox slot. It is drained only at interpreter-mediated entries (`jit_call.rs:1104`, `:1224`) and back-edge polls (`stats.rs:212`).

### 3.5 Back-edges

- **Interpreted** (`dispatch.rs:2291-2350` calls `note_backedge_and_maybe_osr`, `jit_call.rs:265`): two `jit_osr_disabled` lookups, a `jit_osr_counts` increment, `exec_function` plus `loop_latch`, then one or two `jit_tier_cost_decision` calls (`:56`).
  - With production tiering, only the **Optimizing** loop decision can trigger; Template is used only when optimizing is disabled (`:314`). `maybe_osr` (`:428`) tries Machine OSR first and falls back to Template (`:476-493`).
- **Template** (`template/arm64.rs:2067-2078`): 9 instructions per back-edge (thread, interrupt cell, `ldrb`, `cbnz`, fuel cell, `ldr/subs/str`, `b.gt`).
  - Every 4096 back-edges, counted by one global counter (`lib.rs:964`), it calls `STUB_JIT_BACKEDGE_POLL`, which runs `backedge_poll` (`control.rs:40-65`).
  - `baseline_backedges_reach_osr` (`jit_call.rs:373`) credits the whole 4096 window to whichever loop header polled, and always evaluates `jit_code_residency()` (`:414`).
  - A profitable result returns `Relink`: exit to the interpreter, and the next interpreted back-edge enters Machine OSR.

### 3.6 Thresholds implied by `calibrated()` (`tier_policy.rs:154-176`)

Cost terms:
- Template: compile `4000 + 270N + 40R + 80P` ns; code `(560 + 56N + 16R)` B, charged at ×4 ns/B.
- Machine: compile `15000 + 1750N + 40R + 80P` ns; code `(320 + 38N + 16R)` B, charged at ×4 ns/B.
- Savings per entry: Template `12N − 96`, Machine `20N − 128` (`:189-207`).
- Direct-call-target trigger saves a flat 1450 ns; loop triggers save `103·span` (Template) or `108·span` (Machine).

Computed break-even executions:

| N / R / P | Template entry | Machine entry | Machine direct-call | Template loop, span 10 | Machine loop, span 10 |
|---|---|---|---|---|---|
| 6 / 4 / 1 | never | never | 20 | 10 | 27 |
| 14 / 8 / 1 | 196 | 289 | 31 | 14 | 41 |
| 100 / 24 / 3 | 53 | 112 | 145 | 57 | 194 |
| 10000 / 400 / 3 | 42 | 96 | 13158 | 4843 | 17665 |

- Functions with N ≤ 8 never tier on the entry trigger. This covers mega_method's `f` bodies.
- Large functions tier on entry after roughly 42 (Template) or 96 (Machine) entries whatever their size, because savings are counted per static instruction.
- Machine executions count only "stable" entries; any feedback-epoch change restarts the count (`tier_policy.rs:326-340`).
- `cumulative_compile_ns` is **measured wall time** (`jit_compile.rs:526-541`, `:747-762`) and enters every later decision (`jit_call.rs:76`). That makes recompile tiering timing-dependent.

## 4. Invariants relied on

- The registry is boxed once, so its view address is stable. Registration happens only between turns (`jit_registry.rs:17-24`).
- Dependency epochs are monotonic, and entry requires the exact epoch (`:26-30`).
- Executable retirement happens only at the outermost native-activation boundary (`jit_call.rs:1311-1325`). Invalidation unlinks `entry_addr` first; cells are never reused (`code_entry.rs:13-26`).
- The generated counters rely on the single-mutator isolate (`code_entry.rs:127-131`).
- A poll from an optimizing frame runs under `always_allocate_scope`, because its registers are unrooted at the poll (`control.rs:47-52`).
- OSR applies only to ordinary frames with no suspension owner (`jit_call.rs:441-443`).
- Bootstrap allocation is tenured (`lib.rs:3016`, `:3264`). Snapshot capture needs an empty nursery (`snapshot.rs:34-36`).
- Hard executable-code cap of 64 MiB (`tier_policy.rs:36`).
- Compiled code may bake only non-moving references (`jit_compile.rs:15-18`).

## 5. Duplication that could be collapsed

1. **Per-function tier state** lives in 20+ hash structures (§2), even though function ids are dense per code space. It could become one dense record next to the existing `FunctionEntryCell`.
2. **Two validity checks for the same generation.** Interpreter entries re-check with `is_current_for_entry` (hash lookups plus a dependency walk). Generated callers only look at `generation_cell`/`entry_addr`.
3. **Two call-hotness counters.** `jit_call_counts` and `CodeEntryCell.generated_entries` are merged in a cold pass (`jit_call.rs:1379-1460`).
4. **Two back-edge counters.** Interpreted loops count per `(fid, pc)`; compiled loops share one global fuel counter that is credited to whichever header polls.
5. **Resident code bytes computed twice.** The residency walk runs per decision while the registry already charges `GeneratedCodeBytes` to its ledger (`jit_registry.rs:265-271`). The decision itself runs twice (without, then with residency) at `:299-341` and `:1184-1202`.
6. **`VmThread` rebuilt per entry** from isolate-invariant pointers (`entry/code.rs:85-137`).
7. **Entry file parsed twice plus copied** (`lib.rs:6252`; `eval_ops.rs:705-707`).
8. **Two startup mechanisms.** The disk bytecode cache is used; the in-process heap snapshot is unused by the CLI.
9. **Identical install loops** for `global_classes` and `extension.classes` (`lib.rs:3040-3130`).

## 6. What would have to change

**(a) A much cheaper hot path**
- Replace items 1–5 with one dense per-function record holding the counter, a precomputed break-even (already done for generated calls at `jit_registry.rs:219`) and the current generation pointer. Interpreter entry would then be "increment, compare, load cell, enter".
- Take `feedback_epoch` locks and residency walks off the per-call path.
- Keep one persistent `VmThread` per isolate.
- Use the modelled compile cost instead of wall-clock durations, so tiering is deterministic.
- Count per-loop in compiled code.
- Reuse the module-detection AST for the CommonJS wrapper.
- Make startup a restore from a serialized heap image. The CLI would need a byte-level snapshot; today there is none (`snapshot.rs:26-27`).
- Background compilation would cut wall time but not instructions retired.

**(b) A portable AOT artifact**
- Every process-local constant baked into code is already listed in `RelocationTarget` (`otter-jit/src/artifact/relocation.rs:108-160`): RuntimeStub, GcCageBase, PropertyLookup/StoreTransition cache tables, DeoptRuntimeData, GlobalLexicalCell, StringConstantCell, PropertySourceCell, TemplateOperandSlice, GuardedHeapReference, DirectCallEntryCell (for example `site.target.plan.entry_cell` at `arm64/direct_call.rs:1105`).
- Each would have to load through `VmThread`/`JitCtx` or a per-code constant table patched at load time.
- Baked function ids (`direct_call.rs:240-251`) are rebased at link (`code_space.rs:40-44`); shape ids come from a process counter (`snapshot.rs:103-106`); atom ids are per-isolate. All three need load-time remapping.
- `code-normalized.bin` and `relocations.json` are produced but nothing loads them. The magic bytes appear only in producers and tests.

## 7. Open questions

1. What drives zlib's peak RSS of 795 MB? The 128 MiB buffer comes from `vec![0u8; len]` (`binary/array_buffer.rs:401`), which should not touch memory. This is unresolved.
2. How many instructions does startup take on the current HEAD, including the eager reqwest/rustls client? The 830M figure is from a memory note, not re-measured.
3. How much do wall-clock compile durations change instruction counts from run to run? This fits the ±25% same-binary swing in the memory notes, but that link is my speculation.
4. Is the CommonJS dispatch context the chunk that owns the user's functions? `for_function` and `feedback_epoch` locking costs depend on it.
5. How much fixed work goes to synchronous compilation per workload? The event JSONs are in `benchmarks/results/parity-2026-09-27/*-events.json`; I did not total them.
6. Does the single-slot mailbox delay promotion in call-heavy loop-free code?