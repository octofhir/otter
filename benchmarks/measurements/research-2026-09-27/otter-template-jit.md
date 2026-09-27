# Template (baseline) JIT on AArch64: structure, hot paths and transition costs

**Evidence base.** I read the code at HEAD, including the uncommitted work-in-progress. Static sizes come from 94 Template code maps (`benchmarks/results/parity-2026-09-27/null-prototype-artifacts/*template*` and `benchmarks/results/slice11/*/*template*`), covering 10,563 operations. Compile statistics come from `benchmarks/results/parity-2026-09-27/arguments-count-{zlib,earley}-events.json`. Instruction counts are read from the disassembly in those artifacts.

## 0. Key findings
1. **Steady-state hot code is Machine, not Template.** Every attributed JIT sample in Earley and zlib lands in optimizing code objects (`earley-after-closure-tail-profile.txt`, `zlib-warm10-attribution.txt`). Template still matters in three ways:
   - The call linkage, runtime transitions and frame machinery it defines are shared with Machine (`emit_direct_call_with_access`, `crates/otter-jit/src/arm64/direct_call.rs:717`).
   - Some callees never leave Template: Earley `sc_cons`; zlib `u`, `z`, `b` (events JSON).
   - It carries the warm-up period.
2. **Direct calls are expensive.** One generated direct call runs about 128 caller-side instructions plus about 12 in the callee prologue/epilogue. It makes 3 separate frame publications and costs a median 888 B of code per call site.
3. **Template inline caches are frozen at compile time.** A site that had no CacheIR program when it was compiled calls into Rust on every execution until the function is recompiled (`template/arm64/ic_probe.rs:389-392`).
4. **Every runtime transition rebuilds its context.** It re-binds `RuntimeCall`: frame validation, `for_function`, and a clone of `ExecutionContext` (2 atomic reference-count increments and 2 decrements). Most families then re-decode the bytecode at the published PC.
5. **Code density and compile speed.** Output is 117 B per bytecode op on zlib and 147 B/op on Earley. Compile speed is about 1.2 µs/op (zlib: 12,940 ops in 15.3 ms; Earley: 3,389 ops in 4.2 ms).

## 1. Components & key files
- `crates/otter-jit/src/template.rs:137` `compile`: entry point that dispatches to `arm64::compile`.
- `template/plan.rs:114` `TemplateOp`, about 70 variants.
  - `plan.rs:592` `TemplatePlan`; `plan.rs:719` `build`, which lowers bytecode on top of `entry::BaselinePlan`.
  - `plan.rs:750` expands immediate-operand forms (e.g. `AddImm`) into two ops.
  - `plan.rs:1888` `fuse_numeric_chains`.
  - Unsupported opcodes lower to `UnsupportedBail` and mark the code `osr_only` (`plan.rs:1795-1801`).
- `template/arm64.rs:151` `compile_with_reach`: a single dynasm pass. On a relocation failure it retries with far branches (`arm64.rs:127-141`). Emission order: exit epilogues (`1632-1770`), then one OSR trampoline per loop header (`1777-1796`).
- Template emitters:
  - `arm64/values.rs`: tag primitives and the write barrier.
  - `arm64/binding.rs`: upvalues and globals.
  - `arm64/properties.rs` plus `arm64/ic_probe.rs`: CacheIR lowering, shared with Machine.
  - `arm64/arith.rs`, `arm64/calls.rs`, `arm64/transitions.rs`, `arm64/value_packet.rs`.
  - `arm64/scalar.rs`, `protocol.rs`, `exceptions.rs`, `iterators.rs`, etc.: one thin runtime-call emitter per op family.
- `crates/otter-jit/src/arm64/direct_call.rs` (plus `layout.rs`, `completion.rs`, `tiering.rs`, `runtime_forward.rs`): generated call linkage shared by both tiers.
- `template/code.rs:25` `TemplateCode` owns the mmap, operand slices, IC source cells, safepoints and OSR offsets.
- `entry/code.rs:42` `enter_compiled`: the interpreter-to-native boundary.
- `entry.rs:72` `TransitionTable`; `entry.rs:147` `runtime_stub_bindings` lists 63 JIT-owned stubs, out of 99 `STUB_*` descriptors in `otter-vm/src/native_abi/runtime_stubs.rs`.
- VM side:
  - `otter-vm/src/runtime_activation/mod.rs:103` `RuntimeCall::bind`.
  - `otter-vm/src/interp/jit_call.rs:186` `maybe_dispatch_jit`.
  - `otter-vm/src/runtime_stubs.rs`: leaf stubs such as `to_boolean` and `WRITE_BARRIER_MUTATING`.
  - `otter-vm/src/code_space.rs` is the bytecode chunk / function-id registry, not executable memory.
- `crates/otter-jit/src/template/x86_64.rs` (3,611 lines) plus `x86_64/direct_call.rs` (1,801 lines): a full second backend.

## 2. Data layouts
- **`JitCtx`, 136 B** (`entry/abi.rs:37-67,330`). Fields: `thread`, `native_frame`, `error`, `activation_base`, `activation_top_ptr`, `activation_limit`, `global_this_offset`, `native_stack_limit`, `generated_feedback_clean`, `machine_roots_ptr`, `receiver_alloc` (window), `runtime_stats`.
- **`VmThread`, 96 B** (`native_abi/frame.rs:32-73,404`): 12 u64 addresses (`current_frame`, `code_object_id`, `runtime_context`, `registry`, `interrupt`, `gc_heap`, `fuel`, `epoch`, `marking`, protectors, `realm`).
- **`NativeFrame`, 72 B** (`frame.rs:194-236,407-427`):

  | Offset | Field |
  |---|---|
  | 0 | header, 12 B: `function_id` u32, `pc` u32 at +4, `register_count` u16, `kind` u8, `flags` u8 |
  | 16 | `register_base` |
  | 24 | `upvalue_base` |
  | 32 | `this` |
  | 40 | `new_target` |
  | 48 | `self` |
  | 56 | `upvalue_count` |
  | 60 | `eval_env` (4 B Gc) |
  | 64 | `argument_count` |
  | 68 | `arguments_object` |

- **Entry cells.** `FunctionEntryCell` is 16 B; `CodeEntryCell` is 88 B (`code_entry.rs:42,94,281-291`). The code-entry cell holds `entry_addr`, `code_object_id`, `flags`, stack bytes, a pre-built frame header copy, tiering break-even, and entries/deopts/throws counters.
- **Template machine frame: 48 B** (`arm64.rs:111,1921-1933`). It saves fp/lr, x19, x20, x21.
  - Pinned registers: x19 = register window, x20 = `JitCtx`, x21 = `NativeFrame` (`arm64.rs:29-32`).
  - Linkage also uses x25 and saves/restores it on every call.
- **Direct-call linkage frame** (`direct_call/layout.rs:86-121`), 16-byte aligned, at most 4080 B:
  - `NativeFrame` (80 B)
  - control slots `saved_x25`, `entry_addr`, `caller_frame`, `caller_code_object_id`, `target_cell` (40 B)
  - upvalue spine, 4 B per entry
  - registers, 8 B each
  - incoming actual arguments, 8 B each
  - Example: a 5-register callee takes 160 B (asm `sub sp,#0xa0`).
- **Value encoding** (from emitted asm):
  - int32 = `0xfffe<<48 | u32`
  - double = raw bits `+ 0x0002<<48`
  - cell = full 64-bit address whose low 32 bits are the cage offset; `NOT_CELL_MASK` = `0xfffe000000000002`
  - immediates: undefined `0xa`, null `0x2`, true `0x7`, false `0x6`, hole `0x12`
- **Object layout facts baked into code.** Type tag at byte 0 (`0x11`); slab handle at +24; inline values at +64 (`values.rs:334-337`). The shape field is a `Gc<ShapeBody>` cage offset; shapes are pinned in old space (`object/shape_body.rs:27,51`).
- **Closure layout baked into call guards.** Tag `0x23`, function id at +8, flags at +0xc, `upvalue_base` at +0x10 (an absolute derived pointer), count at +0x18, `eval_env` at +0x1c, `bound_this` at +0x20. Upvalue cell value sits at +8; spine entries are 4-byte handles (`binding.rs:263`).
- **Side tables.**
  - `PropertySourceCell = Option<(u32,u32)>`, identity only (`entry/runtime_ops/vm_ops.rs:37`).
  - Every `Add`/`AddImm` site gets its own full-window `SafepointRecord` of `register_count` × 4 B (`entry/lowering.rs:1206-1228`; `safepoints.rs:255`).

## 3. Hot paths

### 3.1 Per-op fixed overhead
- **PC stamp.** Before every op that can exit or call, the code runs `movz w9,#pc` then `str w9,[x21,#4]`: 2 instructions (`arm64.rs:338,1847-1876,1968`).
- **No register allocation across ops.** Every operand is `ldr`/`str` to the x19 window (`values.rs:30-41`).
- **Constant re-materialization.**
  - Each use of the cage base is 2 instructions and each runtime-stub address is 3 (`movz`/`movk`; the artifact shows "encoded-bytes=8/12").
  - The not-cell mask is 2 instructions per cell test (`values.rs:105-118`).
- **Status decode after a runtime call:** `cbz x1` / `cmp #2` / `b.eq throw` / `b fatal`, i.e. 3–4 instructions (`transitions.rs:49-76`).
- **Loop back-edge poll:** 9 instructions, 4 loads, 1 store (`arm64.rs:2056-2105`).

### 3.2 Per-op cost (static median from 94 maps; dynamic instruction count on the fast path)

| Op | Bytes | Fast path |
|---|---|---|
| Move / Return | 8 | 2 |
| BinaryArith int32 Sub/Mul | 188 | ≈17, inline int32 then f64 (`arith.rs:106-253`) |
| AddGeneric | 328 | inline int32/f64; concat alloc call and `STUB_JIT_ADD` also emitted inline (`arith.rs:293-347`) |
| Compare / IntBitwise / Increment | 164 / 192 / 136 | ≈14–18 |
| Branch | 12 (proven boolean) to 280 | 12–16. **Any heap cell calls `to_boolean_leaf`** (`arm64.rs:2011-2031`); object truthiness is a C call. The boolean proof is disabled for the whole function if any try/finally op exists (`arm64.rs:1884-1897`) |
| Upvalue read | 108 | ≈15: `frame.upvalue_base`, then spine[4 B], cage base, cell+8, hole check; miss goes to `STUB_JIT_BINDING_VALUE` (`binding.rs:257-289,441-466`) |
| Upvalue write | about 108+ | adds cell test plus inline generational/marking barrier (`values.rs:470-526`) |
| Global read | 164 | epoch cell, globalThis, shape, slab (`binding.rs:56-100`) |
| LoadProperty, monomorphic own-data | 232 | ≈30–36: decompress header, 3 state-byte guards, shape immediate, slab-base select (`ic_probe.rs:202-318,380-522`). **With no CacheIR program at compile time, the code emits an unconditional `b miss` to `jit_load_property_value`** (seen in f514 asm) |
| StoreProperty | 796 | existing-slot or transition program plus barrier (`properties.rs:152-245`) |
| LoadElement without element feedback | 56 | always `jit_load_element` (`transitions.rs:386-433`) |
| StoreElement, boxed array | 300 | cell values always go to the stub, since the inline path has no barrier (`ic_probe.rs:1276-1290`) |
| MakeClosure, NewObject, NewArray, literals, FreshUpvalue | 52–88 | always a runtime transition; no inline allocation in Template (`transitions.rs:128-333`) |

### 3.3 Anatomy of one transition: `MakeClosure` (the Earley "make_closure_value 127+36" frames)
1. Generated code sets up `ctx`, code-block id, dst, function index, a baked operand-slice pointer and count, then calls through the stub address (`transitions.rs:143-176`).
2. `jit_make_closure_stub` (`entry/runtime_ops.rs:131`) calls `JitCtx::runtime_call`, which calls `RuntimeCall::bind` (`runtime_activation/mod.rs:103-173`). Bind:
   - validates `ActiveFrameRef`;
   - for a materialized frame, compares function id, register length/pointer and upvalue pointer against `stack[frame_index]`;
   - runs `for_function` and clones `ExecutionContext` (`execution_context.rs:88-96,185-210`).
3. `make_closure` (`value_ops.rs:250`) calls `jit_runtime_make_closure` (`jit_runtime_ops.rs:266`), which runs `for_function` again and saves/restores the PC.
4. `make_closure_value` (`function_ops.rs:244-304`):
   - collects cells into a `SmallVec<16>`;
   - performs 3 `function_is_arrow` lookups;
   - allocates the closure in old space;
   - runs `mark_closure_lookup`, which does a prototype-kind lookup (`function_ops.rs:57-69`).
5. `frame.write`, then `advance_pc`.

### 3.4 Calls
**(a) Generated direct linkage** (`direct_call.rs:717-1644`), counted from the f514→f549 asm:

| Stage | Instructions | Notes |
|---|---|---|
| Guard | 36 | activation-limit check (`:779`); function-id immediate; cell/tag/flags/function-id; `bound_this` |
| Setup | ≈57 | see list below |
| Entry | 9 | generated-entries counter plus break-even check (`tiering.rs:19-38`), then `str xzr, [ctx, feedback_clean]`, then `blr` |
| Callee prologue + return epilogue | ≈12 | |
| Status classify | 6 | `completion.rs:62-74` |
| Cleanup | 18 | restore `ctx.native_frame`, `VmThread` pair, pop the activation array, restore x25, `add sp` (`completion.rs:224-245`) |

Setup covers:
- `sub sp` and about 10 stores of `NativeFrame` fields;
- argument copy, 2 instructions per argument;
- entry cell materialization (3), then `ldar` of the generation cell (`:1114`), then stack-limit check;
- copying the 12 B header from the `CodeEntryCell`;
- filling uninitialized registers with undefined using `stp`;
- **three publications** of the same frame: the activation array slot and cursor, `ctx.native_frame`, and `VmThread.{current_frame, code_object_id}` (`:1571-1590`).

Total: about 128 caller-side plus about 12 callee instructions, and about 20 stores. The static code map median is 888 B per call site. A callee with own upvalues additionally makes a Rust call to `STUB_JIT_INITIALIZE_UPVALUES` on **every** call (`:1482-1520`). That lands in `jit_initialize_generated_upvalues` (`otter-vm/src/call_ops.rs:1143-1230`): `for_function`, `exec_function`, `collect_allocation_roots`, `alloc_old_batch_with_roots`. This is the source of the profile's "alloc_upvalue_with_roots 94" and "generated upvalue init 54", and Machine linkage shares it.

**(b) Generic call path.**
1. A value packet on the stack goes to `jit_call_with_this_value_stub` (`vm_ops.rs:162-188`), which copies into a `SmallVec` and binds.
2. `call_with_this_values` re-decodes the instruction at the PC and copies into a `SmallVec` again (`value_ops.rs:112-139`).
3. The VM runs its call machinery. Entering a compiled callee from the VM builds a fresh `JitCtx` + `VmThread` (about 17 VM getters), calls push/pop activation stubs, and runs `finish_compiled_entry` (`entry/code.rs:42-200`).

**(c) Method calls** (`calls.rs:1473-1731`), tried in order: guarded leaf, inline numeric bodies, then for each target a method guard (228 B) plus a *full linkage copy*, then a packet fallback.

### 3.5 Exits and deoptimization
- Exit epilogues pack the frame PC with a reason/action: `TypeMismatch`, `IdentityGuard` and `Unsupported` lead to Recompile; `AllocationMiss`, `RuntimeTransition` and `Interrupt` lead to Resume (`arm64.rs:1644-1690,1946-1966`).
- The VM sets `stack[top].pc` and continues in the interpreter (`jit_call.rs:207-235`). Deoptimizing a materialized frame is therefore just a PC write.
- A bail in a stack-owned callee goes through `STUB_JIT_DEOPT_STACK_CALL`, which materializes the frame without replay (`completion.rs:162-220`).
- Throws go through `STUB_JIT_ROUTE_THROW`. A throw caught in the same frame *exits to the interpreter* at the handler (`arm64.rs:1697-1725`; `calls.rs:14-15`).

### 3.6 Inline versus runtime fraction (static only)
Across the 10,563 ops:
- 67.3% are inline-only (moves, immediates, arithmetic, compares, branches, returns);
- 25.7% are guarded inline with a runtime miss (bindings, properties, elements, calls);
- 6.8% always call the runtime;
- 0.2% are `UnsupportedBail`.

The available data does not support a dynamic breakdown.

### 3.7 Compile and code size
- zlib: 13 Template compiles, 1.51 MB of code, 15.3 ms.
- Earley: 74 compiles, 0.50 MB, 4.2 ms.
- Largest single function: zlib `bk`, 6,395 ops producing 716 KB.
- The TS profile's 839 ms of Template compile time is inflated by artifact rendering (`build_bundle`/`assembly::render` takes most of the 662 `compile_with_reach` samples in `profile-ts/sample.txt`), so it should not be used as a compile-speed figure.

## 4. Invariants relied on
- **PC publication.** The canonical PC is stamped before any op that can exit or call; pure ops inherit the last stamp (`arm64.rs:13-17`). The VM asserts the exit payload PC equals the frame PC (`entry/code.rs:170-178`).
- **Roots live in the frame window.** Tagged values are homed in the published window across every call or allocation. Receiver pointers are recomputed on every access, and the inline slab base is never cached (`properties.rs:10-13`; `values.rs:320-352`). Allocation sites name a full-window safepoint (`lowering.rs:1222`).
- **Stack-owned frames.** Frames flagged `STACK_REGISTERS` are traced through the activation array. The upvalue count stays 0 until batch allocation finishes (`call_ops.rs:1169-1201`).
- **Stable baked identities.** Shapes are pinned in old space; the cage base is constant; the global-lexical epoch guards global-object loads (`binding.rs:56-100`).
- **Exact exits.** Side exits happen before any effect. Code containing `UnsupportedBail`, or a `return` inside a try/finally region (`plan.rs:730-738,1782-1789`), is OSR-only: it is never entered at function entry.
- **Unsupported opcodes** (derived by diffing the `Op` enum against `plan.rs`): `Await`, `Yield`, `YieldDelegate`, `GeneratorStart`, `AsyncIteratorReturn`, `CheckIteratorResult`, `TailCall`, `PromiseNew`, `PromiseCall`, `ImportNamespaceDynamic`, `EvaluateModule`, `StorePropertyStrict`, `StoreElementStrict`.
- **Write barrier.** Inline fast path; the slow path is a leaf call that publishes no frame (`values.rs:442-526`).
- **NaN purification** makes exact bit-pattern truthiness complete (`arm64.rs:19-21`).

## 5. Duplication and collapsible transitions
1. **Three frame publications per call:** `ctx.native_frame`, `VmThread.current_frame` and `code_object_id`, and `activation_base[top]` plus the cursor. The linkage also saves `caller_frame` and `caller_code_object_id`. A single linked list through the saved `caller_frame` slot would cover all of them.
2. **`JitCtx` duplicates `VmThread`** (both carry frame and state pointers). Every entry from the interpreter rebuilds both (`entry/code.rs:60-146`).
3. **Two activation representations**, the interpreter `Frame` and the stack-owned `NativeFrame` window. `RuntimeCall::bind` re-validates one against the other on every transition.
4. **17 runtime-stub signature families** (`artifact/relocation.rs:175-193`), each with its own status decoding. Several carry the same information in different forms: `Variadic` passes register indices, `CommittedValue2`/`ReentrantValue*` pass boxed values, `ReentrantValueSpan` passes packets. Opcode identity is then re-derived from the published PC by the VM.
5. **Feedback counted in four places:** `CodeEntryCell` entries/deopts/throws, `generated_feedback_clean`, `CodeRegistryView.hot_function`, and the interpreter's `note_jit_entry_*`.
6. **Identical safepoint records.** Every `Add` site gets its own copy of the same full-window record.
7. **Code bloat from duplicated sequences:**
   - a full 888 B linkage copy per polymorphic method target;
   - fused numeric chains keep the entire unfused fallback stream (`plan.rs:1879-1887`);
   - cold paths (binding, property, concat) are emitted inline rather than outlined.
8. **A separate x86_64 Template backend** (5.4k lines) instead of shared lowering over a macro-assembler.
9. **Stale documentation.** `template/code.rs:47-52` still describes "self-patching" IC cells, while `vm_ops.rs:15-16` states the cells are identity-only and cannot be patched.

## 6. What would have to change
**(a) A much cheaper hot path**
- **Calls:**
  - one frame-chain publication instead of three;
  - drop the per-call activation-array push/pop and `CodeEntryCell` counters (sample, or count in the callee prologue);
  - bake a `bl` to the current generation with patch-on-retire instead of the function-cell → `ldar` generation → `entry_addr` chain;
  - skip filling registers with undefined when they are proven written before read;
  - allocate own upvalue cells inline (bump allocation in an old-space allocation buffer, or the immutable-capture slot already proposed in the parity checkpoint) instead of a Rust call per invocation.
- **Pinned constants.** Keep the cage base, not-cell mask and number tag in pinned registers.
- **PC stamps.** Replace the per-op stamp with a return-address → PC map, as HotSpot's OopMaps do (cited in `2026-09-27-engine-research.md`).
- **Truthiness.** Decide it inline for object cells.
- **Stores.** Add an inline barrier to `StoreElement`.
- **Self-patching ICs.** Let a miss update a monomorphic/polymorphic inline cache in place, so Template does not need a recompile to stop calling Rust.
- **Transition fast binding.** Cache the resolved `ExecutionContext` pointer in `JitCtx`; skip the materialized-frame comparison; pass opcode operands directly instead of re-decoding by PC.

**(b) A portable ahead-of-time artifact**
Every absolute address is currently a variable-width MOVZ/MOVK immediate: stubs, cage base, entry cells, property source cells, string cells, global-lexical cells, and baked operand-slice pointers. Instruction length therefore depends on the address, so relocation needs either fixed-width sequences or PC-relative or `ctx`-relative table loads. `RelocationCapture` and "OTJNCODE" normalization (`relocation.rs:1-27`) already name these semantically, but only when artifacts are requested. Several values that change per process are not relocated at all:
- dense function ids (`box_function_id` immediates in call guards; the code-block id passed to MakeClosure), which depend on `CodeSpace` link order;
- shape cage offsets used as `cmp` immediates;
- the global-lexical epoch;
- the closure body's absolute `upvalue_base`.

AOT would need symbolic relocations for these, or indirection through per-function constant tables, plus deterministic heap/shape restore (for example a snapshot) or runtime-patched inline caches in place of baked CacheIR.

## 7. Open questions
- In the TS profile, fid 1377 was Template-compiled 15 times (entry counts 43→722, `exitCount` 0; `profile-ts/events.json`), even though Template records no code dependencies (`arm64.rs:1821-1827` passes an empty list). The trigger is unresolved: the feedback-refresh path (`jit_call.rs:1492-1518`) is guarded by an "attempted" set.
- No counters exist for the dynamic number of runtime transitions or for the per-op-kind miss rate.
- The cost of Machine→Template calls into `sc_cons` (Earley) was not measured.
- Whether the `ldar` on the generation cell is required under a single-mutator model.
- The x86_64 backend's parity for these hot paths was not checked.