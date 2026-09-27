## Citation verification (6 most load-bearing)

I checked every citation against the pinned checkouts: jdk-25-ga 6c48f4ed, graal-25.0.0 082f26e3 and graaljs graal-25.0.0 930257e6. Two files missing from the sparse checkout (`JSContextOptions.java`, `JSArguments.java`) were fetched from raw.githubusercontent at the same tags. JEP 450 and JEP 519 were fetched from openjdk.org.

| # | Citation | Verdict | Note |
|---|---|---|---|
| 1 | J `BlockEnvironment.addFrameSlotFromSymbol` / `isScopeCaptured`, `BlockScopeNode` (`FrameBlockScopeNode`, `VirtualBlockScopeNode`), `IterationScopeNode.FrameIterationScopeNode`, `ScopeFrameNode.PARENT_SCOPE_SLOT_INDEX`, `JSFunctionObject.enclosingFrame` (card 1) | **Verified, with a clarification** | `isScopeCaptured` = `hasClosures() \|\| hasNestedEval() \|\| isClassHeadScope()`, or scope optimization is off. The `js.scope-optimization` option (internal) defaults to true. Only `symbol.isClosedOver()` symbols go into the block frame; the rest are hoisted into the function frame. `FrameBlockScopeNode.appendScopeFrame` runs `createVirtualFrame(..).materialize()`. `FrameIterationScopeNode` builds a fresh frame and copies the parent slot and the other slots. `PARENT_SCOPE_SLOT_INDEX = 0`. Missing from the report: when a closure has no block-scope slot, `JSFunctionExpressionNode.ClosureFunctionExpressionNode` falls back to `frame.materialize()` of the whole function frame. |
| 2 | G `truffle/.../api/impl/FrameWithoutBoxing.java` fields; the card 1 costs "1 allocation per captured scope", "up to 4 allocations" and "read = `ldr enclosingFrame`, hops, `ldr slot`" | **Corrected** | The fields `Object[] indexedLocals`, `long[] indexedPrimitiveLocals` and `byte[] indexedTags` exist, plus `Object[] auxiliarySlots`. A reified scope always has slot 0 (the parent link), so the constructor allocates all three arrays. That makes **at least 4** allocations (frame + 3 arrays), not "up to 4" and not 1. Add a 2-element arguments array (`JSArguments.createZeroArg`) for an outermost block scope, and an auxiliary `Object[]` if the descriptor has auxiliary slots. A read is more than `ldr slot`: `getObject` → `verifyIndexedGet` loads the tags array, compares the tag byte (and deopts on a mismatch), then loads the locals array and the element. A parent hop is `getObject(PARENT_SCOPE_SLOT_INDEX)` and costs the same. |
| 3 | H `BarrierSetAssembler::tlab_allocate` (aarch64), `InitializeNode::capture_store`/`complete_stores`, `expand_allocate_common`/`expand_initialize_membar`, `ReduceFieldZeroing`/`ReduceBulkZeroing`/`ReduceInitialCardMarks`, `CardTableBarrierSet::on_slowpath_allocation_exit`, TLAB `_desired_size`/`_refill_waste_limit` (card 2) | **Verified, with a correction** | The asm sequence matches `barrierSetAssembler_aarch64.cpp:251-262` exactly. It is used by the **template interpreter** (`templateTable_aarch64.cpp:3631`) and **C1** (`c1_MacroAssembler_aarch64.cpp:170`), not by C2. C2 emits the same bump as IR in `BarrierSetC2::obj_allocate` (`gc/shared/c2/barrierSetC2.cpp:754`), called from `expand_allocate_common`, with optional allocation prefetch. Store capture, zeroing reduction and card-mark elision are C2-only. All three flags are product flags, default true. `on_slowpath_allocation_exit` card-marks, or defers the mark, only when the new object is neither young nor a typeArray. |
| 4 | J `JSShape.isPrototypeInShape` / `HIDDEN_PROTO`, `getProtoChildTree`, `getPropertyAssumption`, `getPrototypeAssumption`; `PropertyCacheNode.PrototypeShapeCheckNode` quote; G `truffle/docs/AOTOverview.md` rules (card 4) | **Verified, with a correction** | `createObjectShape` adds `HIDDEN_PROTO` as a constant property, and `isPrototypeInShape` = the HIDDEN_PROTO location is constant. The `PrototypeShapeCheckNode` and `PrototypeChainShapeCheckNode` javadoc quotes are exact. The three AOTOverview rules are quoted correctly (lines 51-53). **Missing:** prototype-in-shape is single-context only. When `context.isMultiContext()`, `createPrototypeShape` uses `makeEmptyShapeWithPrototypeInObject`, so the prototype becomes an ordinary field and the context-independent code that engine caching needs does not get this. `getPropertyAssumption(..., prototype=true)` may return the shape's leaf assumption (`JSConfig.LeafShapeAssumption`). |
| 5 | H `deoptimization.hpp`/`.cpp` (`DeoptAction`, `uncommon_trap[_inner]`, `fetch_unroll_info[_helper]`, `unpack_frames`, `realloc_objects`, `reassign_fields`); `globals.hpp` trap limits; G SVM `Deoptimizer.java` (card 5) | **Verified** | `PerBytecodeTrapLimit`=4 ("traps (of one kind) at a particular BCI"), `PerMethodTrapLimit`=100 ("of one kind in a method (includes inlines)") and `PerMethodRecompilationCutoff`=400 are all correct. The report omits `PerBytecodeRecompilationCutoff`=200. The four `DeoptAction` values are exact. The comment "MUST NOT stop at a safepoint once the vframeArray is created" is at `deoptimization.cpp:483`. The Deoptimizer javadoc "The target method is always an AOT-compiled method" and its eager/lazy `DeoptimizedFrame` modes are exact. |
| 6 | H `markWord.hpp` compact layout, `UseCompactObjectHeaders`, `CompressedOops::Mode`, `decode_raw`; JEP 450/519; G `ObjectHeaderSize.md` (card 7) | **Verified, with corrections** | The layout string "klass:22 hash:31 unused_gap:4 age:4 self-fwd:1 lock:2" is exact, `klass_bits = 22`, and `UseCompactObjectHeaders` is a product flag, default false. Corrections: (a) `Mode` has 4 values; `DisjointBaseNarrowOop` = 2 was omitted. (b) Forwarding: `FullGCForwarding` keeps the upper klass bits and encodes the forwardee relative to the heap base in the low 42 bits (`klass_shift`). That is 40 address bits, an 8 TB heap cap, and compact headers are switched off above it. JDK 25 does not call this "sliding forwarding". (c) JEP 450 is the JDK 24 experimental JEP. Its numbers "96 and 128 bits down to 64" and "10%–20%" are exact, as are JEP 519's "22% less heap space and 8% less CPU time" and the fact that the flag is still off by default. ObjectHeaderSize.md does say a 4-byte header is the default and that `Object` takes 8 bytes vs 16, and adds that size depends on the GC and on compressed references. |

Extra spot checks, all verified: `Tier3InvocationThreshold`=200, `Tier4InvocationThreshold`=5000, `Tier3BackEdgeThreshold`=60000 and `Tier4BackEdgeThreshold`=40000 (`compiler_globals.hpp`). The comment "compiled frames do not use callee-saved registers" is at `frame_aarch64.inline.hpp:467`, on the `!_cb->is_nmethod()` branch. The other citations in cards 3, 6, 8, 9 and 10 were not re-checked in this pass.

---

## Corrected report

# HotSpot + GraalVM mechanisms for the Otter redesign

Every name below was checked with grep against these revisions:
- **H** = `openjdk/jdk@jdk-25-ga` (6c48f4ed707b), under `src/hotspot/`
- **G** = `oracle/graal@jdk-25.0.0` (082f26e36e8b)
- **J** = `oracle/graaljs@graal-25.0.0` (930257e6db1e), under `graal-js/src/`

URL form: `https://github.com/openjdk/jdk/blob/jdk-25-ga/src/hotspot/<path>`, and the same pattern for the other two repos. Cards are ordered by how much they bear on the measured gaps.

Verification status: the sources for cards 1, 2, 4, 5 and 7 were re-read line by line in a second pass, and that pass corrected them. From cards 6 and 10, only the items marked (spot-checked) were re-read. The rest of cards 3, 6, 8, 9 and 10 carry only the first-pass grep check.

---

## 1. Captured bindings grouped per scope instance (GraalJS). Gap: earley

1. **Source:**
   - J `com.oracle.truffle.js.parser/.../env/BlockEnvironment.java`:
     - `isScopeCaptured` = `hasClosures() || hasNestedEval() || isClassHeadScope()`. Such a scope gets its own block frame descriptor, as does any scope when `js.scope-optimization` (internal, default true) is off.
     - `addFrameSlotFromSymbol` puts `isClosedOver()` symbols in the block frame descriptor. All others are hoisted into the function frame.
   - `.../GraalJSTranslator.java`: `enterBlockEnvironment`, `newPerIterationEnvironment`.
   - `.../nodes/function/BlockScopeNode.java`: `FrameBlockScopeNode.appendScopeFrame` runs `createVirtualFrame(..).materialize()` on entry. `VirtualBlockScopeNode` merges the block into the function frame.
   - `IterationScopeNode.java`: `FrameIterationScopeNode.execute` makes a fresh frame per iteration and copies the parent slot and the other slots.
   - `nodes/access/ScopeFrameNode.java`: `PARENT_SCOPE_SLOT_INDEX` = 0.
   - `runtime/builtins/JSFunctionObject.java`: field `MaterializedFrame enclosingFrame`.
   - `nodes/function/JSFunctionExpressionNode.java`: `ClosureFunctionExpressionNode` captures the block-scope frame. When there is no block-scope slot, it falls back to `frame.materialize()` of the whole function frame.
2. **Work removed:**
   - One scope frame per *captured scope instance* instead of one cell per binding. In Truffle's layout that frame is several objects; see item 6.
   - One reference per closure instead of k.
   - Bindings nothing captures are never heap-allocated. They stay in the VirtualFrame and become SSA values after partial evaluation (PE).
3. **Hot path:**
   - A read is `ldr enclosingFrame`, then parent hops (the depth is static), then the slot read. Each hop and the final read go through a typed read on a materialized `FrameWithoutBoxing`. For `getObject`, that means: load the tags array, load the tag byte and compare it (deopt on mismatch), load the locals array, load the element.
   - A closure costs 1 allocation plus 1 store (not re-verified).
   - A captured scope costs at least 4 allocations on entry, before PEA: the frame, `Object[]`, `long[]` and `byte[]`. An outermost block scope adds a 2-element arguments array (`JSArguments.createZeroArg`).
4. **Invariants:**
   - Loop `let` gets a fresh copy per iteration (CreatePerIterationEnvironment).
   - Scopes with a nested eval (`hasNestedEval`) and class-head scopes are always reified.
   - TDZ is preserved.
5. **GC/deopt:** the scope is an ordinary object. In principle PEA can remove the scope and the closure together when neither escapes; I have not measured this.
6. **Cost:** the shared scope keeps sibling bindings alive.
   - **Caveat:** a Truffle `FrameWithoutBoxing` (G `truffle/.../api/impl/FrameWithoutBoxing.java`) holds separate `Object[] indexedLocals`, `long[] indexedPrimitiveLocals` and `byte[] indexedTags`, plus `Object[] auxiliarySlots`. Slot 0 of a reified scope is always the parent link, so a materialized scope takes **at least** 4 allocations. An auxiliary array adds one more, and an outermost scope adds its arguments array. Each typed slot read costs about 4 loads plus a tag compare.
   - Copy the grouping, not this layout: use one flat object with N slots and a direct parent field.
7. **AOT:** the choice is made statically at parse time and needs no profile.
8. **JS vs Java:**
   - Java lambdas copy effectively-final values, so Java does not have this problem. JS bindings are shared and mutable.
   - Direct eval, `with`, the debugger and sloppy mapped `arguments` force reified scopes.
   - Generators are handled (`ResumableNode`).
9. **Experiment:**
   - Instrument `alloc_upvalue` / `MakeClosure` (`crates/otter-vm/src/upvalue.rs`). Histogram captured bindings per scope instance on earley-boyer.
   - If the mean is well above 1, emit one `ScopeCell{N}` per captured scope. Measure allocations (110.9M), GC share (~30%) and instructions.
   - If the mean is about 1, the idea is refuted for earley; go to cards 2 and 3.

## 2. TLAB inline allocation + elision of initializing stores. Gaps: ast_ctor, earley

1. **Source:**
   - H `cpu/aarch64/gc/shared/barrierSetAssembler_aarch64.cpp`: `BarrierSetAssembler::tlab_allocate`. This is the bump used by the template interpreter (`templateTable_aarch64.cpp` `_new`) and by C1 (`c1_MacroAssembler_aarch64.cpp`).
   - `gc/shared/c2/barrierSetC2.cpp`: `BarrierSetC2::obj_allocate`. This is C2's version of the same bump, emitted as IR.
   - `share/opto/macro.cpp`: `PhaseMacroExpand::expand_allocate_common` (calls `obj_allocate`, plus `prefetch_allocation`), `expand_initialize_membar`.
   - `share/opto/memnode.cpp`: `InitializeNode::capture_store`, `complete_stores`.
   - `opto/c2_globals.hpp`: `ReduceFieldZeroing`, `ReduceBulkZeroing`, `ReduceInitialCardMarks` (all product flags, default true).
   - `gc/shared/cardTableBarrierSet.cpp`: `CardTableBarrierSet::on_slowpath_allocation_exit`.
   - `gc/shared/threadLocalAllocBuffer.hpp`: `_desired_size`, `_refill_waste_limit`.
2. **Work removed:**
   - The runtime call per allocation.
   - Zeroing of fields that are overwritten at once (C2 only).
   - Card marks on initializing stores (C2 only).
3. **Hot path (verified, interpreter/C1 form):** `ldr obj,[thread,tlab_top]; lea end,[obj,#size]; ldr t,[thread,tlab_end]; cmp; b.hi slow; str end,[thread,tlab_top]`, then the header and field stores.
   - C2 emits the same load/add/compare/store as IR, optionally with allocation prefetch.
   - The cold path refills the TLAB in the runtime, and a GC can happen there.
4. **Invariants:**
   - The object is fully initialized before the next safepoint. `InitializeNode` captures the stores and a membar publishes them.
   - Dropping the barriers relies on fast-path objects being young.
   - `on_slowpath_allocation_exit` card-marks the whole object when the slow path returns one that is neither young nor a typeArray. With `_defer_initial_card_mark` it defers the mark and flushes it at the next slow-path exit.
5. **GC:** only the slow-path call is a safepoint, and it has an OopMap.
6. **Cost:** TLAB waste is bounded by the refill limit. Compile cost is trivial.
7. **AOT:** the sequence is relative to the thread register and position-independent. The only external reference is the call to the slow-path stub.
8. **JS:**
   - Closures, cells, object literals and `new C` have constant size, so they fit this directly.
   - Otter allocates closures and cells **in old space through Rust calls**. It pays a call per allocation, plus remembered-set or barrier work on every later store of a young value into them.
9. **Experiment:**
   - Inline the nursery bump for `MakeClosure`, cells and literals in Template and Machine code, and drop the barriers on initializing stores.
   - Measure instructions on earley and ast_ctor, plus the scavenge survival rate.
   - If survival is above ~80%, switch to pretenuring driven by per-site feedback instead of a blanket old-space rule.

## 3. Escape analysis: C2 scalar replacement vs Graal PEA. Gaps: earley, ast_ctor, ts

1. **Source:**
   - H `share/opto/escape.hpp/.cpp`: `ConnectionGraph::do_analysis`, `compute_escape`, states `NoEscape/ArgEscape/GlobalEscape`, `reduce_phi` (`ReduceAllocationMerges`).
   - `opto/macro.cpp`: `eliminate_allocate_node`, `can_eliminate_allocation`, `scalar_replacement`, `create_scalarized_object_description`.
   - `opto/callnode.hpp`: `SafePointScalarObjectNode`, `SafePointScalarMergeNode`.
   - `runtime/deoptimization.cpp`: `realloc_objects`, `reassign_fields`.
   - G `compiler/src/jdk.graal.compiler/.../virtual/phases/ea/PartialEscapePhase.java`, and `PartialEscapeClosure.java` (`processVirtualizable`, `ensureMaterialized`, `MergeProcessor.mergeObjectStates`, `addVirtualMappings`).
   - G `nodes/virtual/VirtualObjectState.java`, `CommitAllocationNode.java`; `nodes/spi/Virtualizable.java` `virtualize(VirtualizerTool)`.
2. **Work removed:** the allocation, its field stores and loads, and the GC pressure.
   - C2 is flow-insensitive: one escaping path loses the whole object, except through phi reduction.
   - PEA materializes the object only on the branch where it escapes.
3. **Hot path:** a virtual object costs 0 instructions. `CommitAllocationNode` (a bump allocation plus stores) appears only on the escaping path.
4. **Invariants:**
   - Every deopt point describes the fields of the virtual objects.
   - Identity comparisons fold, and locks on virtual objects are elided.
   - C2 caps: arrays of up to 64 elements, and 512 fields.
5. **Deopt:** objects are reallocated at deopt time. That can fail (`realloc_failures`), so a deopt may need a GC.
6. **Cost:**
   - C2 builds a connection graph iteratively.
   - PEA keeps per-block states and runs `EscapeAnalysisIterations` passes.
   - Deopt metadata grows with the number of virtual objects.
7. **AOT:** portable, provided the deopt target can rebuild the objects (card 5).
8. **JS vs Java:**
   - In Java, objects escape only through non-inlined calls and heap stores.
   - In JS, getters/setters, `valueOf`/`toString` coercions, Proxy traps, sloppy `arguments` and `eval` are hidden escapes. EA therefore depends on shape guards (card 4) and deopt (card 5).
   - A closure passed to an inlined `map` or `forEach` becomes virtualizable, together with its scope and cells. In GraalJS the scope is a `FrameWithoutBoxing` plus its arrays (card 1), so PEA has to virtualize all of them.
9. **Experiment:**
   - For earley, ts and ast_ctor, classify each allocation site in Machine code as: removed, materialized-cold, materialized-hot, or not attempted.
   - Check that merges materialize lazily, and that deopt rebuilds virtual closures and cells.
   - Many "not attempted" cells caught by inlined closures would confirm this card's value.

## 4. Speculating on mutable structure: dependencies, Assumptions, prototype in shape. Gaps: mega_method, ts

1. **Source:**
   - H `share/code/dependencies.hpp`: `DepType` (`leaf_type`, `unique_concrete_method_2/_4`, `unique_implementor`, `call_site_target_value`, …).
   - `dependencies.cpp`: `find_unique_concrete_method`.
   - `opto/doCall.cpp`: `find_monomorphic_target`, then `assert_unique_concrete_method`.
   - `code/codeCache.cpp`: `CodeCache::mark_for_deoptimization(DeoptimizationScope*, KlassDepChange&)`.
   - `dependencyContext.cpp`: `mark_dependent_nmethods`.
   - `nmethod.cpp`: `nmethod::make_not_entrant` (`NativeJump::patch_verified_entry`, or an entry barrier).
   - G `truffle/src/com.oracle.truffle.runtime/.../OptimizedAssumption.java`: `invalidate`, `registerDependency`.
   - J `runtime/objects/JSShape.java`:
     - `isPrototypeInShape`: the prototype is stored as a constant-location `HIDDEN_PROTO` property (`createObjectShape`).
     - **Single-context only.** When `context.isMultiContext()`, `createPrototypeShape` uses `makeEmptyShapeWithPrototypeInObject`, and the prototype becomes an ordinary field.
     - Also `getProtoChildTree`, `getPropertyAssumption` (for prototypes it may be the shape's leaf assumption, under `JSConfig.LeafShapeAssumption`) and `getPrototypeAssumption`.
   - J `nodes/access/PropertyCacheNode.java`: `PrototypeShapeCheckNode` ("Check the shape of the object by identity and the shape of its immediate prototype by assumption"), `PrototypeChainShapeCheckNode`.
2. **Work removed:** the prototype-chain walk and the per-level checks on inherited loads. Virtual calls become direct and can be inlined.
3. **Hot path:**
   - An inherited load is `ldr shape; cmp #S; b.ne miss`, with the holder object embedded as a constant. This is the single-context case.
   - Assumptions and CHA cost 0 instructions.
4. **Invariants:**
   - Any mutation that could break a speculation must reach its dependents before that code can see it.
   - Changing a prototype is a shape transition when the prototype is in the shape.
   - Adding a key to a prototype invalidates the property assumption.
5. **Deopt:** invalidation makes the code not entrant, and frames already on the stack are deoptimized lazily. Dependency lists are weak.
6. **Cost:** one list per assumption, created on demand, plus a shape tree per prototype.
7. **AOT:** speculation on object identity does not carry across processes. Truffle's context-independent code rules (G `truffle/docs/AOTOverview.md`):
   - Disable "all speculation on runtime value identity".
   - Use a two-level inline cache: function identity first, then the CallTarget.
   - Keep root Shapes per language, not per context.
   - GraalJS follows these rules itself: in multi-context mode it moves the prototype out of the shape.
   - So an AOT artifact needs symbolic dependencies (for example "`%Array.prototype%.push` unmodified"), checked at load.
8. **Java vs JS:**
   - Only class loading breaks Java CHA, and methods are immutable.
   - Every JS prototype is mutable, so dependencies attach to shapes and keys.
   - Accessors cache the getter as a constant. Proxy and dictionary-mode objects take the generic path.
9. **Experiment:**
   - Otter's shapes lack the prototype, so each inherited load re-checks the chain.
   - Put the prototype into the shape (a transition tree per prototype) and add validity cells per (shape, key).
   - Keep the design usable for AOT: identity-embedded prototypes only in JIT code, symbolic dependencies in AOT code.
   - Count guard instructions per inherited load. Measure mega_method (2.5x) and ts (9.4x).

## 5. Deopt protocol and speculation throttling. Gaps: ts, earley

1. **Source:**
   - H `runtime/deoptimization.hpp`: `DeoptReason` (`Reason_null_check`, `Reason_class_check`, `Reason_unstable_if`, …), `DeoptAction` (`Action_none`, `Action_maybe_recompile`, `Action_reinterpret`, `Action_make_not_entrant`), `make_trap_request`, `UnrollBlock`.
   - `deoptimization.cpp`: `uncommon_trap`, `uncommon_trap_inner`, `fetch_unroll_info` → `fetch_unroll_info_helper` (walks the `compiledVFrame`s, `realloc_objects`, `reassign_fields`, `relock_objects`, `create_vframeArray`), `unpack_frames`.
   - `runtime/vframeArray.cpp`: `vframeArrayElement::fill_in`, `vframeArray::unpack_to_stack`.
   - `opto/runtime.hpp`: `uncommon_trap_blob`.
   - `runtime/globals.hpp`:
     - `PerBytecodeTrapLimit`=4 ("traps (of one kind) at a particular BCI").
     - `PerMethodTrapLimit`=100 ("of one kind in a method (includes inlines)").
     - `PerBytecodeRecompilationCutoff`=200.
     - `PerMethodRecompilationCutoff`=400.
   - G `nodes/StateSplit.java` `stateAfter`: frame states attach only to nodes with side effects.
   - G `substratevm/.../core/deopt/Deoptimizer.java`: "The target method is always an AOT-compiled method" (eager and lazy modes, `DeoptimizedFrame`).
2. **Work removed:**
   - No code for rare cases in the compiled output, so code is smaller and register allocation is better.
   - Trap counts per bytecode and reason stop deopt/recompile cycles. Recompilation cutoffs per bytecode and per method finally give up on the method.
3. **Hot path:** `cmp; b.cond stub`. The stub sits out of line: `mov w,#req; bl uncommon_trap_blob`. The cold path builds one vframeArray entry per inlined frame and unpacks them into interpreter frames.
4. **Invariants:**
   - Each trap has the exact interpreter state, including virtual objects and monitors.
   - No safepoint may happen once the vframeArray exists (comment in `fetch_unroll_info_helper`).
   - Graal re-executes from the last `stateAfter`, which must be free of side effects. So guards carry no state of their own.
5. **Exceptions:** `Unpack_exception` handles an exception thrown into a frame that is being deoptimized.
6. **Cost:** debug info per trap point. Feedback lives in MethodData.
7. **AOT:** Native Image deopts into AOT-compiled code. For Otter AOT, the target would be an interpreter plus bytecode embedded in the binary, or deopt-target variants of the AOT code.
8. **JS:** there are more reasons (shape, int32 overflow, holes, number vs string, TDZ). The mechanism carries over 1:1.
9. **Experiment:**
   - Otter has had deopt storms and bail loops (the `x==null` cell storm, InsufficientFeedback exits).
   - Add trap counters per (pc, reason) with limits, so the Machine tier recompiles without the failed speculation. Add a per-function recompilation cutoff.
   - Measure deopts and instructions on earley and ts through `--jit-events=`.

## 6. Frames, calling convention, OopMaps, safepoints. Gaps: fib, crypto

1. **Source:**
   - H `cpu/aarch64/sharedRuntime_aarch64.cpp`: `SharedRuntime::java_calling_convention` (`j_rarg0..7`, `j_farg0..7`), `gen_i2c_adapter`, `gen_c2i_adapter`.
   - `runtime/sharedRuntime.hpp`: `AdapterHandlerLibrary`.
   - `cpu/aarch64/macroAssembler_aarch64.cpp`: `ic_check` (the unverified entry point, UEP, compares the receiver's narrow klass with `CompiledICData::speculated_klass`), `safepoint_poll`, `read_polling_page`.
   - `aarch64.ad`: `instruct safePoint` (`ldrw zr,[poll]`).
   - `runtime/safepointMechanism.hpp/.cpp`: `_poll_bit`, good/bad polling pages.
   - `runtime/handshake.hpp`: `Handshake::execute`.
   - `compiler/oopMap.hpp`: `OopMapValue` kinds (`oop_value`, `narrowoop_value`, `callee_saved_value`, `derived_oop_value`), `ImmutableOopMapSet`.
   - `cpu/aarch64/frame_aarch64.inline.hpp`: `frame::sender_for_compiled_frame` (sender_sp = unextended_sp + `cb->frame_size()`). The comment "compiled frames do not use callee-saved registers" is on the `!_cb->is_nmethod()` branch of the register-map update (spot-checked).
   - `PreserveFramePointer`=false on aarch64.
2. **Work removed:**
   - No handle or root registration in compiled code. Raw references live in registers and stack slots, and the GC finds them through the OopMap for the pc.
   - No frame-pointer chain.
   - Arguments are passed in registers.
3. **Hot path:**
   - A monomorphic call is `bl UEP; ldrw klass; ldrw spec; cmp; b.eq VEP`. A static or CHA-bound call goes straight to the verified entry point (VEP).
   - A loop poll is `ldrw zr,[poll]`, or `ldr; tbnz`.
   - OopMaps cost 0 instructions.
4. **Invariants:**
   - GC happens only at calls, polls and allocation slow paths, and each such pc has a map.
   - Derived pointers are recorded as base+offset and fixed up after objects move.
   - No Java-frame reference sits in a callee-saved register across a call, so stack walking needs no register reconstruction.
5. **GC/deopt/exceptions:** the same pc-keyed metadata serves all three. Handshakes stop a single thread through the same poll word.
6. **Cost:** maps are compressed per compiled method. Adapters are shared per signature fingerprint.
7. **AOT:** all tables are pc-relative, so they are portable. Native Image uses the same model.
8. **JS:**
   - There is no static signature, so calls need an argc register, an arity adaptor, `this`, `new.target` and the closure.
   - NaN-boxed slots need a "tagged Value" map kind alongside "raw compressed ref" (the analog of `narrowoop_value`).
9. **Experiment:**
   - In fib (4.0x), count Machine-tier instructions per call.
   - Add an exact-arity register entry with stack maps instead of spilling to root slabs.
   - Verify under `OTTER_GC_STRESS`.

## 7. Compressed refs + compact headers (Lilliput). Gap: zlib RSS

1. **Source:**
   - H `oops/compressedOops.hpp`: `CompressedOops::Mode` (`UnscaledNarrowOop`, `ZeroBasedNarrowOop`, `DisjointBaseNarrowOop`, `HeapBasedNarrowOop`).
   - `compressedOops.inline.hpp`: `decode_raw` = `base + (v << shift)`.
   - `oops/markWord.hpp`: "64 bits (with compact headers): klass:22 hash:31 unused_gap:4 age:4 self-fwd:1 lock:2".
   - `gc/shared/fullGCForwarding.hpp`: Full GC forwarding with compact headers.
   - `runtime/globals.hpp`: `UseCompactObjectHeaders` is a product flag, default false in JDK 25.
   - JEP 450 (JDK 24, experimental): headers shrink "from between 96 and 128 bits down to 64 bits", and live data is "typically reduced by 10%–20%".
   - JEP 519 (JDK 25, product, still opt-in): "22% less heap space and 8% less CPU time" on SPECjbb2015.
   - G `docs/reference-manual/native-image/ObjectHeaderSize.md`: the header is 4 bytes by default in Oracle GraalVM Native Image, so an `Object` takes 8 bytes instead of 16. The doc adds that the size depends on the GC and on compressed references.
2. **Work removed:** bytes, and with them cache misses and GC copy and mark work.
3. **Hot path:** decoding is one add, plus a shift. The class comes from one load plus a shift out of the mark word.
4. **Invariants:**
   - The class id fits in 22 bits.
   - Forwarding keeps the class bits intact. The self-forward bit covers promotion failure. Full GC (`FullGCForwarding`) encodes the forwardee relative to the heap base in the low 42 mark-word bits: 40 address bits, an 8 TB heap cap, and compact headers are disabled above it.
   - The identity hash lives in the header.
5. **GC:** object size is derived from the class, so there is no size word.
6. **Cost:** a class-id table.
7. **AOT:** ids must be stable inside the artifact; Native Image assigns them at build time.
8. **JS:**
   - A shape changes over an object's lifetime, so a shape id in the header must be mutable.
   - Otter already uses 32-bit cage offsets.
   - Its 8-byte header (type, flags, 2 reserved bytes, a u32 **size**) makes an `UpvalueCell` 16 bytes, half of it header.
9. **Experiment:**
   - Take a heap census of zlib (795MB RSS vs 80MB): headers, shape words, payload, free-list fragmentation, external buffers and retained garbage.
   - Move the shape id into the header in place of the size only if headers plus shapes exceed ~15%.
   - The RSS gap is probably elsewhere; the census decides.

## 8. Truffle partial evaluation: one definition of the semantics. Goal: a single compiler foundation

1. **Source:**
   - G `compiler/.../truffle/PartialEvaluator.java`: `doGraphPE`, `createGraphDecoder` → `CachingPEGraphDecoder`, `PELoopExplosionPlugin`, decoding plugins for the first and last tiers.
   - `TruffleCompilerImpl.java`.
   - `truffle/.../api/CompilerDirectives.java`: `transferToInterpreterAndInvalidate`, `@CompilationFinal`, `@TruffleBoundary`, `isPartialEvaluationConstant`.
   - `api/nodes/ExplodeLoop.java`, `BytecodeOSRNode.java`.
   - `api/frame/VirtualFrame.java` ("must not be stored in a field… materialize()").
   - `truffle/docs/OnStackReplacement.md`: a bytecode dispatch loop over a `@CompilationFinal byte[]` with `MERGE_EXPLODE`.
2. **Work removed:** a second implementation of the semantics. JIT code is the interpreter specialized to constant code plus its specialization state (the first Futamura projection). Tiers cannot diverge.
3. **Hot path:** dispatch and frame arrays disappear (frames are virtualized), and only the specialization guards remain. A miss calls `transferToInterpreterAndInvalidate`, which deopts, rewrites the node and recompiles.
4. **Invariants:**
   - Interpreter fields read during PE must be `@CompilationFinal` or guarded by an assumption.
   - A VirtualFrame must not escape.
   - Code that PE cannot handle goes behind `@TruffleBoundary`.
5. **Deopt:** always back into the same interpreter, so there is no state-mapping layer.
6. **Cost:** compile time and warmup are high, because PE inlines the interpreter. That is why the encoded-graph cache exists.
7. **AOT:** in a native image, the interpreter's IR ships with the binary so PE can run at run time (`RuntimeCompilationFeature` "encodes Graal graphs for runtime compilation").
8. **JS:**
   - GraalJS shows the approach covers JS. The costs are warmup and memory.
   - Rust has no PE host.
   - The idea that carries over: define each opcode's semantics once, and generate the interpreter handler, the HIR lowering and the AOT lowering from it.
9. **Experiment:**
   - Write single-source definitions for the 15 hottest opcodes and generate both the interpreter code and the HIR from them.
   - Count difftest divergences and lines of code. Otter's history includes a Template `Return` that skipped `finally`, and the `dst==lhs` miscompile.
   - Compare throughput with the hand-written handlers.

## 9. Native Image: closed world, image heap, engine AOT vs guest AOT. Goal: AOT

1. **Source:**
   - G `substratevm/src/com.oracle.graal.pointsto/.../PointsToAnalysis.java`.
   - `svm.hosted/.../image/NativeImageHeap.java`: objects are split into partitions by immutable, references and relocatable. `isRelocatableConstant` means a `RelocatedPointer`. Error text: "Object with relocatable pointers must be explicitly immutable".
   - `svm.core/.../image/ImageHeapLayouter.java`: `assignObjectToPartition`.
   - `svm.core/.../os/ImageHeapProvider.java`: a heap base is used when `SpawnIsolates` is on. The layout is protected, read-only, relocatable, writable.
   - `svm.core.posix/.../linux/LinuxImageHeapProvider.java`: re-maps the executable's heap section for each isolate (copy-on-write, lazily loaded, pages shared).
   - Docs: `NativeImageBasics.md` ("closed-world assumption"; static fields set at build time go into the image heap), `ReachabilityMetadata.md`, `ClassInitialization.md`.
   - `svm.truffle/TruffleFeature.java`, `svm.truffle/api/SubstrateTruffleRuntime.java`.
   - `truffle/docs/AOT.md` (`RootNode.prepareForAOT()`), `AuxiliaryEngineCachingEnterprise.md` (persists "ASTs and optimized machine code"; Oracle GraalVM only).
2. **Work removed at startup:** class loading, verification and the engine's own warmup. Builtins are mapped from the image, not built.
3. **Hot path:** mmap, then patching of the relocatable partition only. I infer from the layout that heap references are relative to the heap base; that they need no patching is not separately verified.
4. **Invariants:**
   - Nothing may be reachable that the analysis did not see, so reflection needs config.
   - State initialized at build time must be valid on every run: no threads, file descriptors, clocks or random seeds.
5. **Deopt:** Truffle code compiled at run time deopts into the AOT-compiled interpreter.
6. **Cost:** a slow whole-program build. The image carries the compiler and the encoded graphs.
7. **AOT, the key distinction:** GraalJS in a native image is AOT **of the engine** only. JS is still parsed, interpreted, profiled and PE-compiled at run time. Guest-program AOT exists only as:
   - Contexts preinitialized at build time.
   - `prepareForAOT` roots compiled with no profile. They need types in the `FrameDescriptor` and must not deopt on first run.
   - Engine caches persisted from a profiling run. These allow only context-independent code (card 4), which in GraalJS also means no prototype-in-shape.
8. **Java vs JS:**
   - Java's closed world holds because classes are static and reflection is declared.
   - In JS, `eval`, `new Function`, dynamic `import()` and prototype mutation make a closed world unsound.
   - Otter's AOT must therefore be open-world: guarded native code, the interpreter plus bytecode as the deopt and eval fallback, and a JS heap snapshot with the same build-time-init hazards (`Date.now`, `Math.random`, environment variables).
9. **Experiment:**
   - Make Otter's snapshot relocation-free (cage-relative) and map it copy-on-write from the executable. Measure startup, and RSS shared across N processes.
   - Then AOT-compile ts using types from a profiling run and compare against the JIT, counting deopts in the AOT code.

## 10. Template interpreter, tiering, OSR. Gaps: fib, crypto warmup

1. **Source:**
   - H `cpu/aarch64/interp_masm_aarch64.cpp`: `InterpreterMacroAssembler::dispatch_next` (`ldrb rscratch1,[rbcp,#step]!`), and `dispatch_base` (one dispatch table per `TosState` in `rdispatch`; the poll is emitted only when `generate_poll` is set).
   - `utilities/globalDefinitions.hpp`: `TosState`.
   - `templateTable_aarch64.cpp`: `TemplateTable::patch_bytecode` (quickening, e.g. to `_fast_agetfield`), `TemplateTable::branch`. `TemplateTable::_new` also inlines the TLAB bump (card 2).
   - `compiler/compilationPolicy.hpp`: levels 0–4, where level 3 is C1 with full profiling.
   - `compilationPolicy.cpp`: `call_event`, `loop_event`, `method_back_branch_event`.
   - `compiler_globals.hpp` (spot-checked): `Tier3InvocationThreshold`=200, `Tier4InvocationThreshold`=5000, `Tier3BackEdgeThreshold`=60000, `Tier4BackEdgeThreshold`=40000.
   - `runtime/sharedRuntime.cpp`: `OSR_migration_begin/_end` copies interpreter locals and monitors into a buffer. No safepoint or GC is allowed during migration.
2. **Work removed:**
   - Push/pop, by caching the top of stack in a register.
   - Constant-pool resolution checks, by quickening.
   - Most profiling moves out of the interpreter into C1 tier-3 code.
3. **Hot path:** `ldrb; addw; ldr [rdispatch,..]; br`, about 4 instructions plus the template body. Polls happen only on back-branches and returns.
4. **Invariants:** the top-of-stack state is static at each bytecode boundary. The frame layout is fixed and known to both deopt unpacking and OSR migration.
5. **GC:** the GC scans interpreter frames by their fixed layout. OSR migration runs outside safepoints.
6. **Cost:** templates are generated at startup.
7. **AOT:** Native Image has no Java interpreter. Otter would ship its interpreter precompiled.
8. **Java vs JS:** HotSpot templates specialize on **static** types (`itos`, `ftos`). JS needs quickening to typed variants driven by runtime feedback instead, as Truffle does.
9. **Experiment:**
   - Quicken property and call ops in place after the first inline-cache fill.
   - Measure the share of time in dispatch and cache probes with `sample` on crypto and fib.
   - Count instructions executed before each hot function tiers up.

## What HotSpot/GraalVM do not spend that Otter spends

- **A heap cell per captured binding.** GraalJS allocates one scope frame per captured scope instance (at least 4 objects in Truffle's frame layout before PEA; a flat N-slot object is the version to copy). Uncaptured bindings become SSA values.
- **Rust calls and old-space placement for fresh closures and cells**, plus barriers on initializing stores. HotSpot inlines the TLAB bump in all tiers, and C2 also elides zeroing and initial card marks.
- **Prototype-chain checks on inherited loads.** With the prototype in the shape plus assumptions, one compare remains. GraalJS does this only in single-context mode and drops it for context-independent code.
- **Root-slab or handle bookkeeping in compiled code.** OopMaps, no references in callee-saved registers, and fixed frame sizes replace it.
- **Repeated deopts at one site.** Per-(site, reason) trap limits switch the speculation off, and recompilation cutoffs abandon the method.
- **A hand-kept copy of the semantics per tier.** In Truffle the interpreter is the specification.
- **A size word per object.** Size comes from the class, and the class id sits inside a 64-bit header (opt-in in JDK 25; 4-byte header by default in Oracle GraalVM Native Image).
- **Building builtins at startup.** The image heap is mapped copy-on-write and shared between processes.

The sparse source checkouts I used for verification are in `/private/tmp/claude-501/-Users-alexanderstreltsov-work-octofhir-otter/a2e9abdb-0d8f-437a-8cf1-f2cad155594e/scratchpad/src/{jdk,graal,graaljs}`. I did not modify the Otter repository.