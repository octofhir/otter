# Interpreter dispatch, frames and call ABI: subsystem report (read-only)

The working tree has uncommitted work: `frame_state.rs`, `call_ops.rs`, `upvalue.rs`, `heap.rs` and the new `otter-gc/src/heap/old_batch.rs` (the "frame cell batches" candidate). Its measurement note says it is not yet validated. Where it changes a path, I describe both the committed (HEAD) behaviour and the WIP behaviour.

## 1. Components and key files

| Location | Item | Role |
|---|---|---|
| `otter-vm/src/interp/dispatch.rs:49-2762` | `dispatch_loop_inner` | One `match` per opcode. It caches the per-frame `(function_id, depth)` (145-208). |
| `interp/exec.rs:1073-1245` | `dispatch_loop*`, `dispatch_loop_rooted` | Drives the loop and turns a `VmError` into a throw plus an unwind. |
| `interp/frames.rs:117-685` | window alloc, `pop_frame_above` (486), unwind helpers, `jit_push_native_frame` (179) | Frame lifecycle. |
| `frame_state.rs:110-153` | `Frame` | Interpreter frame record, 72 B. |
| `frame_state.rs:158-166` | `ParkedFrameState` | Suspended copy of a frame (generator / await). |
| `native_abi/frame.rs:30-222` | `VmThread`, `VmFrameHeader`, `NativeFrame` | Frame and thread records that generated code reads and writes. |
| `active_frame.rs` | `ActiveFrameRef/Mut` | One access API over either `Frame` or `NativeFrame`. |
| `activation_stack.rs:35` | `ActivationStack { frames: Vec<Frame> }` | Stack of interpreter frames. |
| `register_stack.rs:33-306` | `RegisterWindow`, `RegisterStack` | Segmented register arena (4K-slot segments, 512K-slot cap). |
| `cold_frame.rs:114, 410-485` | `ColdFrame`, `ColdFramePool` | Per-frame side record acquired only when needed. |
| `call_ops.rs` | `do_call_inner` 2241, `try_push_bytecode_call_frame_from_window` 2047, `invoke` 2384, `push_bytecode_call_frame` 1724, construct 2631/2664, `jit_initialize_generated_upvalues` 1143 | Call and construct semantics. |
| `function_ops.rs:163-301` | `run_make_closure_operands` → `make_closure_value` 244 | Closure creation. |
| `closure.rs:97-178, 779` | `ClosureCallHeader`, `JsClosureBody`, `alloc_closure_with_roots` | Closure object layout and allocation. |
| `upvalue.rs:52-114` | `UpvalueCellBody` | One heap cell per captured binding. |
| `native_function.rs:223-400, 1351` | `NativeFunctionBody`, `NativeCallTarget::invoke` | Host (Rust) functions. |
| `runtime_activation/mod.rs:103-170` | `RuntimeCall::bind` | Entry point for runtime calls made from generated code. |
| `otter-jit/src/arm64/direct_call.rs:717-1644` (+ `completion.rs`, `layout.rs`, `tiering.rs`) | `emit_direct_call_with_access` | Generated JS→JS call sequence, shared by Template and Machine (`machine/numeric/arm64.rs:3464`). |
| `otter-jit/src/entry/code.rs:43` | `enter_compiled` | Interpreter → compiled entry. |
| `generator.rs:79-232`, `iterator_ops.rs:3206` | `GeneratorBody`, `resume_generator` | Generator suspension and resume. |

## 2. Data layouts

- **Basic units.** `Value` is 8 B (`value/mod.rs:90`). GC handles `Gc<T>` are 4-byte offsets into the heap cage (`compressed.rs:190`). `GcHeader` is 8 B: tag, flags, reserved, size (`header.rs:124`).
- **`VmFrameHeader`, 12 B:** `function_id`@0, `pc`@4, `register_count` u16@8, `kind`@10, `flags`@11 (`native_abi/frame.rs:157-170, 406-420`).
- **`Frame`, 72 B (asserted, `frame_state.rs:73`):**
  - header@0, `eval_env`@12, `registers: RegisterWindow`@16 (asserted, 168-170). The window is 16 B: base pointer, len u32, stack_base u32 (`register_stack.rs:33-38, 93-97`).
  - Derived from `repr(C)` order but not asserted: `upvalues: Box<[UpvalueCell]>`@32 (16 B), `self_value`@48, `this_value`@56, `return_register: Option<u16>`@64, `cold: Option<ColdFrameIdx>`@68.
- **`NativeFrame`, 72 B (asserted, `frame.rs:407-445`):**
  - header@0, `register_base`@16, `upvalue_base`@24, `this`@32, `new_target`@40, `self`@48, `upvalue_count`@56, `eval_env`@60, `argument_count`@64, `arguments_object`@68.
  - The flags byte encodes `STACK_REGISTERS`, `DERIVED_CONSTRUCTOR` and `INCOMING_ARGUMENTS` (`frame.rs:105-129`).
- **`VmThread`, 96 B:** 12 u64 addresses (`frame.rs:30-68`).
- **Stack frame of a generated call (`direct_call/layout.rs:81-120`), limit 4,080 B (`direct_call.rs:112`):**
  - `NativeFrame` padded to 80 B (`entry/abi.rs:307`).
  - 40 B of control slots: saved x25, entry, caller frame, caller code id, target cell.
  - 4 B per own and inherited captured cell.
  - Registers, 8-byte aligned.
  - Actual arguments, only when the callee needs `arguments`.
- **`ClosureCallHeader`, 24 B:** `function_id`, `flags`, `upvalue_base` (an **absolute process address**), `upvalue_count`, `eval_env` (`closure.rs:97-110`, asserted 297-305).
- **`JsClosureBody`, `repr(C)` (`closure.rs:178-213`):**
  - header 24 + `bound_this` 8 + `bound_new_target` 8 + construct header 20 (`closure_construct.rs`) + `Option<UpvalueCell>` 8 + 3 bools + `Option<Value>` 16 + `named_lookup`.
  - By my arithmetic that is about 96 B; it is not asserted. Add the 8 B GC header and 4 B × captures (tail allocated at 834-853).
  - The allocation census before the inline-tail change averaged 112 B per closure (`2026-09-27-closure-captures.md`).
- **`UpvalueCellBody`:** 8 B payload + 8 B header = 16 B. This matches the census: 1,774,406,656 B / 110,900,416 cells.
- **`ColdFrame` (`cold_frame.rs:114-`):** 17 fields, including five `SmallVec`s (`handlers[4]`, `rest_args[4]`, `incoming_args[4]`, `parked_finally[2]`, closers[2]) and a `PendingBindFunction` with a `SmallVec[4]`. There is no size assertion; I estimate several hundred bytes (not measured).
- **`CodeBlockInstruction`:** 32 B (`executable.rs:1347-1368`).

## 3. Hot paths

### 3.1 Per-instruction interpreter overhead (`dispatch.rs:118-252`)

Every instruction pays for:
- a floor check and `stack.len()`;
- an unchecked read of the top frame's `function_id` and `pc`;
- a three-way cache compare;
- `instr_at_index` with a bounds check, then the opcode;
- `feedback_recorder_at(idx)` on every instruction when a JIT is installed (213-217);
- `record_work(instr.reductions())` (222) and one `has_hooks` branch.

A call or return changes `depth`, so the next instruction misses the cache (160-207): `covers_function` + `exec_function` + a max-depth sample that calls `jit_generated_call_depth()`. That is two misses per call/return pair.

### 3.2 Interpreter JS→JS call (`Op::Call` on a closure)

1. The dispatch arm (273-357), with a JIT installed: `record_call_attempt_feedback`, plus a static-native probe.
2. `do_call_inner` (2241): decode, read the callee, then a checked `advance_pc`.
3. `bytecode_call_target_parts` (1390-1443):
   - `read_payload` pulls the flags, bound this, new.target, derived-this and eval env out of the closure;
   - then **`upvalues_snapshot().into_boxed_slice()` (1421)**: a Rust `malloc` plus a copy of every inherited cell handle, on every closure call.
4. Owner resolution (`FunctionOwnerCache::resolve`, 430), `exec_function`, a call counter, and a depth check against `stack.len()` + generated depth (2086-2091).
5. `prepare_bytecode_call_frame_from_window` (1887):
   - `this` binding: for a sloppy callee it resolves the global, or boxes a primitive receiver (allocation) (`interp/protos.rs:832-850`).
   - `build_upvalues_for_exec_with_roots`. If the callee has own captured bindings, `frame_state.rs:528-557` does `Vec::with_capacity(own+parent)` (a second malloc), then:
     - HEAD: one `alloc_upvalue_with_roots` old-space allocation per cell;
     - WIP: one `alloc_old_batch_with_roots` call;
     - then it copies the parent cells in and drops the snapshot `Box`.
   - Rollback checkpoint, `RegisterStack::allocate` (segment check, then fill the window with `undefined`, `register_stack.rs:229-259`), build the 72 B `Frame`.
   - Argument binding (`argument_window.rs:179-207`): a direct register copy. A `ColdFrame` is acquired whenever the callee needs `arguments` or a rest parameter, when new.target or derived-this is present, or when the call is async (1935-1948).
6. Write `frame.self_value`, then `stack.push(frame)` (a 72 B move, possibly a Vec realloc).
7. With a JIT installed: `record_ordinary_call_feedback`, then `maybe_dispatch_jit` (3.4).

**Return** (`frames.rs:661-570`):
- scans the cold record's handlers for a `finally`;
- `pop_frame_above` moves the frame out, releases the cold record (`release` rewrites the whole `ColdFrame::default()`, `cold_frame.rs:426-430`), frees the window, writes the result into the caller's register;
- drops the `Box` spine (a `free`).

Net cost of an interpreted closure call: 1–2 malloc/free pairs, plus N old-space cells when the callee has own captures. A bare function (`Value::function`) avoids the `Box`.

### 3.3 Interpreter → compiled entry (`jit_call.rs:187-253, 1098-1301`; `entry/code.rs:43-200`)

The `Frame` is always fully built first; compiled entry is attempted afterwards.
- `run_optimized_frame`: `promote_hot_generated_callee`, optimized-code cache compare or FxHashMap lookup, an `Arc` clone.
- Otherwise `resolve_jit_code_for_fid`: promotion again, an entry counter, a cache compare, an `osr_only` set probe, a map lookup, an `Arc` clone.
- `enter_compiled`:
  - reads about 15 isolate addresses;
  - builds a `NativeFrame` on the Rust stack, pointing at the `Frame`'s register window and `Box` spine;
  - builds a 96 B `VmThread` and a `JitCtx`;
  - calls `jit_push_native_activation_stub`, runs the code inside `with_native_eval_env_owner`, then pops;
  - runs `finish_compiled_entry`.

### 3.4 Generated direct call (Template and Machine share `emit_direct_call_with_access`)

My count of the ARM64 emitter text, for a plain closure call without own captures, is **about 130–150 fixed instructions, plus about 2 per argument and 1 per local register**, not counting the callee body:

1. **Activation-limit check** (779-783).
2. **Callable guard** (835-874):
   - compare against the boxed function-id immediate (up to 4 `movz/movk`), else require a heap cell, closure tag 0x23, and no runtime-setup flags;
   - check the function id;
   - load `upvalue_base`, `upvalue_count` and `eval_env` from the closure;
   - check the inherited-capture count (1009-1014).
3. **Frame setup:** `sub sp`, stores of self/upvalue_base/eval_env/this/new_target/count (1030-1085).
   - **Inherited captures are zero-copy**: `upvalue_base` points directly into the closure's inline tail (1050).
4. **Copy fixed arguments** (1096).
5. **Load the entry generation** from the function's entry cell (`ldar`); an empty cell goes to the cold resolver stub (1101-1134).
6. **Stack-limit check**, header copy from the code entry (1135-1171), initialize locals with `undefined` unless the entry is flagged "parameter prefix only" (1197-1214).
7. **Own captured bindings (`own_upvalue_count != 0`)**: a Rust stub call (`STUB_JIT_INITIALIZE_UPVALUES` → `jit_initialize_upvalues_stub`, `reentry.rs:1414`) → `jit_initialize_generated_upvalues` (`call_ops.rs:1143-1232`). That function:
   - re-resolves the owner context and the `CodeBlock`;
   - allocates `own` cells (HEAD: one per call; WIP: batch);
   - re-reads SELF through `as_closure` / `call_state`;
   - **copies the inherited handles into the stack suffix** (1218-1221).

   This is the "generated upvalue init" 54 plus `alloc_upvalue_with_roots` 94 samples in the Earley profile.
8. **Publish the frame** (1569-1587): save the caller frame and code id, push onto the activation array, update `ctx.native_frame` and `VmThread.current_frame/code_id`.
9. **Tiering counter** (`tiering.rs:19-38`), clear `generated_feedback_clean`, `blr`.
10. **Callee Template prologue**: `stp`/`str` of fp, lr, x19-x21, plus loads of the frame and register base (`template/arm64.rs:1921-1932`).
11. **Completion** (`completion.rs:66-245`): three-way status branch, then about 14 instructions to restore publication, release sp, restore roots, store the result.

### 3.5 Captured-binding access

- **Interpreter `LoadUpvalue`** (`dispatch.rs:1673-1679` → `frame_ops.rs:56-69`): builds an `ActiveFrameMut` wrapper, bounds-checked index into the `Box`, `read_payload` (cage base from a global atomic, `compressed.rs:134-136`), TDZ hole check, checked register write, checked `advance_pc`.
- **Template** (`template/arm64/binding.rs:257-282`): load the frame's `upvalue_base`, `cbz`, load the 4 B handle, `cbz`, materialize the **cage base as an immediate**, add, load the value, compare against the hole constant, store to the register. About 13–16 instructions, with no caching of the base across accesses.
- **Stores** (`upvalue.rs:108-114`, `binding.rs:291-330`) go through the generational/marking write barrier.

### 3.6 Closure creation cost

**Interpreter** (`dispatch.rs:2686` → `function_ops.rs:163-301`):
1. Decode the parent indices into a `SmallVec<[u32;16]>`, then gather the cells into a `SmallVec<[UpvalueCell;16]>` (266-269).
2. `function_id_constant`; `context.function_is_arrow` is called **three times**, each doing an `exec_function` lookup (272-285).
3. `alloc_closure` → `alloc_variable_with_roots_initialized` → `alloc_old_with_roots_inner` (`heap.rs:1625-1728`), which:
   - probes the trace table;
   - runs `account_or_collect` if a heap cap is set;
   - allocates from `old_space.alloc` (free list);
   - writes header and payload, zeroes the tail, copies the captures in, recomputes `upvalue_base`;
   - **`T::trace_slots` over the new object with `write_barrier` on every edge**. Each barrier loads the child's header (`barrier.rs:59-99`). The trace also clears `last_instance` (`closure.rs:412-420`);
   - updates the per-type stats.
4. `mark_closure_lookup` (57-69) does `for_function` + a function lookup and **a second `with_payload` write** to set `named_lookup`.

**Template:** `jit_make_closure_stub` (`entry/runtime_ops.rs:131`) → `ctx.runtime_call()`. `RuntimeCall::bind` validates the frame and **clones the `ExecutionContext`, which is two `Arc` increments plus decrements on drop** (`runtime_activation/mod.rs:162`). Then `jit_runtime_make_closure` repeats `for_function` before reaching the same `make_closure_value`. The parent indices arrive as a raw pointer into memory owned by the compile plan.

**Machine:** a committed scalar call (`machine/numeric/semantics.rs:123`) → `scalar_values` → `make_closure_value` (`committed_values.rs:607-645`). That path calls `published_opcode`, two `published_const_index`, and **one `published_imm32` per parent**; every one of these re-resolves `exec_function` + `instr_at_index` (428-482). Then `for_function` again, then the same VM function.

No tier emits inline code for closure creation.

### 3.7 Construct

- **Interpreter** (`call_ops.rs:2664-2842`): observable prototype lookup; the prototype is parked on an anchor; allocate the receiver; set its prototype; re-read the callee after each allocation; reserve slots; build the frame (1560-1658). **A `ColdFrame` is always acquired** for `construct_target` / `new_target` (1642-1655). A derived constructor allocates a derived-this cell (1634).
- **Generated code:** a nursery receiver probe (`receiver_allocation.rs`), otherwise the try-prepare or prepare stubs (`direct_call.rs:1216-1424`); new.target stays in `NativeFrame.new_target_bits`.

### 3.8 Native call

`try_invoke_native_call_from_window` (`call_ops.rs:2139-2204`):
- copies the arguments into a `SmallVec<[Value;8]>`;
- `call_target` (heap read), `record_runtime_native_call`, which is a work-budget checkpoint;
- `invoke_native_call_with_roots` (277-309): `NativeCallInfo`, `NativeCallRoots`, `register_extra_roots` (push/pop), an optional realm switch, `RuntimeTurn` + `NativeCtx`, a function-pointer or `Arc<dyn Fn>` call (`native_function.rs:1351-1376`), then error mapping.

### 3.9 Bound functions

- **Interpreter:** `invoke` loops over binding layers; each layer clones the bound arguments `SmallVec` and concatenates (2403-2419).
- **Generated linkage** accepts only a function-id immediate or a closure tag (`direct_call.rs:841-852`). A bound function fails the guard and goes to `caller_bail`.
- **Creating** a bound function goes through `PendingBindFunction` in the cold record, and the body owns a Rust `String` (`bound_function.rs:69-70`).

### 3.10 Exceptions

- Interpreter `Throw` (`dispatch.rs:727-741`) and every converted `VmError` (`exec.rs:1159-1196`) run **`snapshot_frames`: a Vec plus two `String`s per frame, even when the throw is caught** (`stack_snapshot.rs:62-77`).
- The unwinder then calls **`render_thrown`** (`async_ops.rs:559`), which formats the value to a string.
- Unwinding pops frames, running iterator closers, the async rejection path, and catch/finally handlers kept in the cold record (`async_ops.rs:547-640`).
- The dispatch loop is re-entered afterwards, so the frame cache is lost.
- Generated code never unwinds native frames: it returns a (payload, `Throw`) pair (`native_abi/dispatch.rs:13-17`). Callers branch to `throw_value`; the outermost frame returns `JitExecOutcome::Throw` → `unwind_compiled_throw_above`, which takes the snapshot again (`jit_call.rs:86-100`).

### 3.11 Generators

- **Creation** (`call_ops.rs:1833-1880`): park the frame, allocate the generator, then run the prologue through a nested `dispatch_loop_above_rooted`.
- **`yield`** (`dispatch.rs:852-878`):
  - `frame_detach_cold` → `Box::new(ColdFrame)` (generators always carry the owner in the cold record);
  - `park_active_frame` copies the registers into a `SmallVec<[Value;8]>` and frees the window;
  - the parked state is stored as a `Box<ParkedFrameState>` (`generator.rs:83`).
- **Resume** (`iterator_ops.rs:3242-3250`): take the frame, allocate a window and copy back, `Box::new(frame)`, re-attach the cold record, then a nested dispatch.

### 3.12 Non-direct calls from compiled code

`CallMethodValue` / `CallWithThis` stubs → `RuntimeCall::bind` (context clone) → re-decode the instruction (`value_ops.rs:159-200`) → `run_callable_sync` → a nested `dispatch_loop_above_rooted` that builds a full `Frame` (`call_ops.rs:3941-4140`) → possibly `enter_compiled` again. A plain `Call` guard miss deoptimizes at the opcode (`template/arm64/calls.rs:12-13`).

## 4. Invariants the subsystem relies on

- **No `&Frame` survives a push** (the `Vec` may move); register windows are stable because segments never grow while active (`activation_stack.rs:13-15`, `register_stack.rs:12-18`).
- **GC roots:**
  - `with_runtime_turn` registers the frame-root and extra-root providers (`activation_stack.rs:287-308`);
  - the register arena traces only its published prefix (`register_stack.rs:297-306`);
  - native activations are traced from `jit_native_activations` plus the chain of Machine root records (`frames.rs:281-318`).
- **Frames under construction:** allocations while a frame is not yet on a traced stack must pass `*_with_roots` visitors (`frame_state.rs:464-480`). The WIP publishes `upvalue_count=0` until the batch completes (`call_ops.rs:1169-1200`).
- **Old space never moves** (`header.rs:52`, `barrier.rs:79`). Several things depend on it:
  - the absolute `ClosureCallHeader.upvalue_base`;
  - generated frames aliasing a closure's inline tail (`direct_call.rs:871, 1050`);
  - global lexical proofs.

  Because of this, closures and cells are allocated in old space, so reclaiming short-lived ones requires a major GC.
- **Tagged values are homed in the frame at safepoints;** pointers derived from movable objects are recomputed after any allocating or reentrant call (`frame.rs:21-22`).
- **Deopt** of a generated stack call or inline frames materializes a `Frame` exactly once, after the exit (`interp/jit_calls/deopt.rs:1-45`). There is no replay of already-committed effects (`completion.rs:9`).
- **Exceptions** are carried as status values, never by native unwinding.

## 5. Duplication and representations that could be collapsed

1. **Three frame records and two conversions.** `Frame` (Rust, in a `Vec`) and `NativeFrame` (C layout) both hold header, registers, upvalues, self, this and eval_env. `ParkedFrameState` is a third copy. Compiled entry converts `Frame` → `NativeFrame` (`entry/code.rs:60-117`); deopt converts back. new.target, derived-constructor state and incoming arguments live in `ColdFrame` for `Frame` but in `NativeFrame` fields and flags for native frames.
2. **Upvalue spines exist in six forms:** `Frame.upvalues: Box`, the closure inline tail, the generated stack suffix, `UpvalueSource` views, the parked `Box`, and `EvalEnvBody.cells`. The interpreter copies the immutable tail into a `Box` on every closure call (`call_ops.rs:1421`); generated code aliases it (zero copy) except when the callee has own cells (`call_ops.rs:1218`).
3. **Frame builders:** `push_bytecode_call_frame`, `prepare_bytecode_call_frame_from_window`, `build_construct_bytecode_frame[_from_window]`, the lean-callback `invoke_prepared_lean` / `invoke_cold_lean`, `run_bytecode_callable_committed_rooted`, and `jit_initialize_generated_upvalues`. The generator-creation block is duplicated verbatim (`call_ops.rs:1833-1880` vs `2001-2044`).
4. **`MakeClosure` has three decoders** (interpreter `run_make_closure_operands`, Template stub arguments, Machine `published_imm32` loop) feeding one allocator, with repeated context resolution.
5. **Two callable encodings:** the `Value::function(fid)` immediate and a closure with an empty spine. `make_function_value` allocates closures anyway (`function_ops.rs:149-160`); every guard checks both forms.
6. **`JsClosure.cached_function_id`** (`closure.rs:494`) duplicates `call_header.function_id`, and `Value::as_closure` reads the heap anyway (`value/mod.rs:612-615`).
7. **`upvalue_base` is derivable:** it always equals `body + sizeof(JsClosureBody)` (`closure.rs:386-410`), yet it is stored as an absolute u64.
8. **Two tier-dispatch mechanisms:** the interpreter resolves code through `jit_code`, `jit_optimized_code`, their single-entry caches and the `osr_only` set (`jit_call.rs:1159-1301`); generated callers use `FunctionEntryCell` generations.
9. **Possibly dead stub:** `STUB_JIT_INLINE_CLOSURE_UPVALUES` is registered (`entry.rs:327`), but `rg` finds no reference in `template/`, `machine/` or `arm64/`.

## 6. What would have to change

**(a) A much cheaper hot path**

- **Split captured bindings into own and inherited windows** (`own_base` plus an `inherited_base` aliasing the closure tail) in both `Frame` and `NativeFrame`, with compiler index spaces or a runtime split at `own_count`. This removes:
  - the per-call `Box` snapshot and the `Vec` rebuild (`frame_state.rs:538-556`);
  - the stack copy loop in the generated initializer.
- **Eliminate cells for proven-immutable captures** by storing values in the closure tail, the next hypothesis in `closure-captures.md`.
- **Allocate own cells and closures inline in generated code** from an old-space linear allocation window, analogous to `jit_receiver_allocation_window` (`frames.rs:329`):
  - cells initialized to `undefined`/hole create no edges, so no barrier is needed;
  - black allocation keyed off `marking_flag_cell`;
  - the closure's function kind (`named_lookup`) and arrow-ness are known at compile time, which removes `mark_closure_lookup`, the triple `function_is_arrow`, `RuntimeCall::bind` with its `Arc` clones, and the operand re-decoding.
- **Compute the tail address** as `closure + 8 + BODY_SIZE` instead of loading `upvalue_base`.
- **Let the interpreter run directly on `NativeFrame` plus a register window**, dropping `Frame`. At minimum, check the entry cell before materializing a `Frame`.
- **Move construct and new.target state into the hot frame**, so ordinary constructs never touch `ColdFrame`.
- **Make throw diagnostics lazy:** no `snapshot_frames` / `render_thrown` when a throw is caught.
- **Store generator registers in a GC-traced tail of the generator body** instead of `Box` + `SmallVec` + boxed cold record.

**(b) A portable AOT artifact (no process-local addresses)**

- Already relocatable semantically (`relocation.rs:108-160`): cage base, runtime stubs, entry cells, property/global/string cells, lookup tables, deopt data, operand slices (raw Rust pointers, `transitions.rs:160-168`), and guarded heap references. What is missing is either a loader that patches them, or position-independent loads through `ctx`/`VmThread` instead of immediates.
- **Not relocated:** global function ids baked as immediates, and the boxed function-id values built from them (`direct_call.rs:804-808, 836-840, 866`). These are `function_base + local`, assigned by chunk link order (`execution_context.rs:176-180`). They need module-relative ids plus relocation.
- **Heap objects** carry absolute or process-bound data:
  - `ClosureCallHeader.upvalue_base` (removable, see (a));
  - `NativeFunctionBody` static function pointers (documented as "same-process", `native_function.rs:229-231`), which would need `ExternalRefTable` indices;
  - Rust `String` / `SmallVec` inside `BoundFunctionBody`.
- **Layout offsets and type tags** are fixed per VM build (asserted), so an artifact is valid only for the identical binary.

## 7. Open questions

- Does the CLI set `max_heap_bytes != 0`? If so, every old-space allocation runs `account_or_collect_with_roots` (`heap.rs:1663`).
- How does direct old-space allocation of closures and cells schedule major GC and incremental marking? The profile shows mark-start 86 and sweep 96 samples. This belongs to the GC subsystem and I did not read it.
- Machine callee prologue size, and whether Machine inlining (`inline_frames.rs`) covers closures that have own captures. Not read.
- Actual size of `ColdFrame`, and how often constructs, arguments objects and try blocks acquire it on the benchmarks.
- Whether global function ids are deterministic across runs of the same source.
- Whether `STUB_JIT_INLINE_CLOSURE_UPVALUES` is reached from any emitter (for example x86_64).
- How often plain-`Call` guard misses deoptimize on ts/zlib; no counters were read.
- Correctness and performance of the WIP batch allocator are still pending per `2026-09-27-frame-cell-batches.md`.