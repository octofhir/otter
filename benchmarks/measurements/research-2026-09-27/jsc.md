## Citation check (WebKit `501a722a66dd9d333f95e69d8cb503d4d51516bd`, fetched raw from GitHub)

| # | Citation | Verdict | Note |
|---|---|---|---|
| 1 | Card 1: `llint/LowLevelInterpreter64.asm` `getClosureVar` / `op_resolve_scope`; `dfg/DFGSpeculativeJIT.cpp` `compileNewFunctionCommon`, `compileCreateActivation`; `dfg/DFGGraph.cpp` `Graph::tryGetConstantClosureVar` | Verified, one correction | `loadq JSLexicalEnvironment_variables[t0, t1, 8]` is at L2921. `m_localScopeDepth` and `m_constantScope` are used by `op_resolve_scope`. The NewFunction sequence matches exactly (alloc, `storePtr(scope)`, `storeLinkableConstant(executable)`, `mutatorFence`). While the singleton is valid, the code calls `operationNewFunction` / `operationCreateActivationDirect`. The "Don't need a memory barriers…" quote is verbatim (L8617). `tryGetConstantClosureVar` returns empty when `isUnlinked()`, requires `IsWatched` and calls `addLazily`. **Correction:** after `m_next`, CreateActivation stores the **symbol table**, not an executable. |
| 2 | Card 3: `jit/AssemblyHelpers.cpp` `emitAllocateWithNonNullAllocator`; `heap/FreeListInlines.h` | Verified, two corrections | The ARM64 fast path (`loadPairPtr` start/end, `branchPtr AboveOrEqual`, add, store) matches. So does the pop path (`loadPairPtr` nextInterval/secret, sentinel bit test, `xor64` with the scrambled bits). The quote "When going to the slow path, we must leave resultGPR with zero in it" is verbatim. "we don't create empty intervals" is in FreeListInlines. **Corrections:** (a) the embedded constant is `TrustedImmPtr(allocator.allocator().localAllocator())`; (b) the cell size is a compile-time constant only when `hasConstantCellSize()`. Otherwise the emitter loads `LocalAllocator::offsetOfCellSize`, and when the allocator is not `isConstant()` it arrives in a register. |
| 3 | Card 4: `heap/ConservativeRoots.cpp` `genericAddPointer` / `genericAddSpan`; `jit/AssemblyHelpers.h` `barrierBranch` | Verified, detail corrected | `MarkedBlock::blockFor`, `TinyBloomFilter::ruleOut`, block-set `contains` and `isLiveCell` are all present. `barrierBranch` is `load8 cellStateOffset` + `branch32` against `addressOfBarrierThreshold`. **Correction:** precise (large) allocations are checked first, by binary search. Interior pointers into any cell are aligned down with `cellAlign`. The `IndexingHeader` check covers only butterfly pointers that land just past the end of a cell or block. |
| 4 | Card 5: `runtime/JSCJSValue.h` encoding; `runtime/Structure.h`, `StructureID.h`, `JSCell.h`; LLInt `op_get_prototype_of` | Verified, two corrections | The encoding comment matches exactly: 2^49 offset, `FFFE:0000:IIII:IIII`, 0x06/0x07/0x0a/0x02. The Structure fields and `s_maxTransitionLength = 128` are present. `btqz` on `m_prototype` leads to `knownPolyProtoOffset`. The CSR comment is verbatim ("used used" typo in the source). **Corrections:** (a) JIT code decodes a StructureID with one `or64` against `structureIDBase()`; the C++ `decode()` masks the nuke bit and adds. (b) There are three limits: 128, 512 for `PutById` adds and 4096 for remove/attribute change. Crossing one produces a *cacheable* dictionary, which the IC code flattens (`bytecode/Repatch.cpp` L618) before caching. |
| 5 | Card 6: `bytecode/AdaptiveInferredPropertyValueWatchpointBase.cpp` `fire()`; `bytecode/PropertyCondition.h` | Verified, one correction | The `fire()` snippet is verbatim. The class `RELEASE_ASSERT`s the `Equivalence` kind. The three `generateConditionsFor…` functions exist. **Correction:** `PropertyCondition::Kind` also contains `Replacement`, which the report leaves out. |
| 6 | Card 7 and the rename claims: `bytecode/PropertyInlineCache.h`, `InlineCacheCompiler.h`, `runtime/OptionsList.h`, `runtime/MegamorphicCache.h` | Verified | `StructureStubInfo.h` and `heap/MarkedAllocator.h` both return 404, which fits the renames. `PropertyInlineCacheType { Handler, Repatching }`, `m_inlineAccessBaseStructureID`, `countdown`, `repatchCount` and `resetStubAsJumpInAccess` are present. The data-only sharing quote is verbatim (L423-425). The source comments document caller-frame execution, reads before calls, `GCAwareJITStubRoutine`, `BinarySwitch` and "Why only in FTL". `useHandlerICInFTL` is false and `repatchBufferingCountdown` is 6. The load cache is 2048 + 512. |
| 7 | Speculation post (webkit.org/blog/10308, 2020-07-29), cited in cards 4, 6 and 9 | Verified | 500 / 1000 / 100000 points; 15 per call, 1 per loop execution. The post calls these static baselines that are adjusted dynamically. All quoted sentences are verbatim. |
| 8 | B3 post (webkit.org/blog/5852, 2016-02-15) and `b3/air/AirGenerate.cpp` | Verified | "LLVM takes 4.7 times longer to compile than B3" and "three quarters or more" are verbatim. The post describes IRC. optLevel 0 uses `GenerateAndAllocateRegisters`; higher levels use `allocateRegistersByGreedy` + `allocateStackByGraphColoring`. `defaultB3OptLevel` is 2. |
| 9 | Riptide post (webkit.org/blog/7122, 2017-01-20) | Verified | Both quotes are verbatim. |
| 10 | `dfg/DFGPlan.cpp` ordering; `DFGObjectAllocationSinkingPhase.cpp`; `ftl/FTLOSRExit.h`; `dfg/DFGOSRExit.cpp`; `runtime/CachedTypes.cpp` | Verified | VarargsForwarding sits inside `if (!isFTL())`. ArgumentsElimination and ObjectAllocationSinking run after SSAConversion. Sinking checks `singleton().isStillValid()` (L1110, L1183). FTLOSRExit.h says: "0x49 MaterializeNewObject: LEB128 index into OSRExitDescriptor::m_materializations". `OSRExitStream` is a chunked, delta and flag byte encoding. `m_bootSessionUUID` exists. The other cited files and symbols (about 25) all resolve. |
| — | Otter-side figures: the 12.3x / 9.6x / 9.4x / 11.8x gaps, 110.9M cells, 10.2M closures, the ~10 s compile, d9e81603 | Not verifiable here | These are Otter measurements, not JSC sources, and were not re-measured in this pass. |

---

# JavaScriptCore: mechanisms mapped to Otter's gaps

**Pin:** WebKit `501a722a66dd9d333f95e69d8cb503d4d51516bd` (main, 2026-09-27). `J/` = `https://github.com/WebKit/WebKit/blob/501a722a66dd9d333f95e69d8cb503d4d51516bd/Source/JavaScriptCore/`. I grepped every symbol below at this SHA and grep-checked every blog quote against the page text.

**Verification pass:** Cards 1, 3, 4, 5, 6 and 7, the three blog posts, and the phase ordering were re-fetched at this SHA and checked; corrections are applied in place. Otter-side figures (gap ratios, 110.9M cells, 10.2M closures, the ~10 s compile) are Otter measurements and were not re-verified.

Renames at this SHA:
- The StructureStubInfo role is now `PropertyInlineCache`; `StructureStubInfo.h` is gone.
- The MarkedAllocator role is `BlockDirectory` + `LocalAllocator`; `MarkedAllocator.h` is gone.
- Air's optimizing register allocator is `allocateRegistersByGreedy`. The 2016 B3 post describes IRC.
- `Repatch.cpp` lives in `bytecode/`, not `jit/`.

Cards are ordered by relevance to earley 12.3x, ast_ctor 9.6x, ts 9.4x and zlib 11.8x (Otter measurements, not re-verified here).

## 1. Closures: one environment per scope; closure = {scope, executable}
1. **Source:**
   - `J/runtime/JSLexicalEnvironment.h`: `variables()`, `offsetOfVariable(ScopeOffset)`, `allocationSize(SymbolTable*)`.
   - `runtime/JSCallee.h`: `m_scope`. `runtime/JSFunction.h`: `m_executableOrRareData`.
   - `runtime/SymbolTable.h`: thin/fat `SymbolTableEntry` with `InlineWatchpointSet`, `VarKind::Scope/Stack`, `singleton()`.
   - `llint/LowLevelInterpreter64.asm`: `op_resolve_scope` (`m_localScopeDepth`, `m_constantScope`) and `op_get_from_scope` (`getClosureVar`: `loadq JSLexicalEnvironment_variables[t0, t1, 8]`).
   - `dfg/DFGSpeculativeJIT.cpp`: `compileNewFunctionCommon`, `compileCreateActivation`.
   - `dfg/DFGGraph.cpp`: `Graph::tryGetConstantClosureVar`.
2. **Work eliminated:** per-binding cells. All captured variables of one scope instance share one `JSLexicalEnvironment`: cell header, butterfly, `m_next`, symbol table and N inline slots. Creating a closure costs O(1) regardless of how many variables it captures: a fixed-size `JSFunction` plus two stores (scope, executable).
3. **Hot path:**
   - Read: get the scope, then one indexed load. Getting the scope means walking `m_next` `m_localScopeDepth` times, or loading `m_constantScope`.
   - DFG `NewFunction`: inline allocation (card 3), header, null butterfly, `storePtr(scope)`, `storeLinkableConstant(executable)`, then `mutatorFence`. The fence is nothing on x86 and a conditional store fence on ARM64.
   - `CreateActivation`: the same inline allocation, header and null butterfly, then `storePtr(scope)` into `m_next`, `storeLinkableConstant(symbolTable)`, one store per slot (undefined or TDZ), then `mutatorFence`. *(Corrected: it stores the symbol table, not an executable.)*
   - Cold: while `singleton().isStillValid()`, creation calls `operationNewFunction` / `operationCreateActivationDirect`, so the runtime sees the second instance.
4. **Invariants:**
   - `ScopeOffset` is fixed when bytecode is generated.
   - The scope chain is static except for var injection (sloppy direct `eval`, `with`). `JSGlobalObject::m_varInjectionWatchpointSet` guards it (`varInjectionCheck`).
   - Singleton inference: while only one instance exists, DFG treats the scope as a constant.
   - `tryGetConstantClosureVar` folds a variable into a constant when its entry watchpoint is `IsWatched` (written once, not yet rewritten). It registers that watchpoint with `addLazily`.
5. **GC/exception/deopt:** slots are `WriteBarrier<Unknown>`. The source says: "Don't need a memory barriers since we just fast-created the activation, so the activation must be young." A second instance or a second write fires the watchpoint, and the dependent code is jettisoned.
6. **Cost:** a surviving closure keeps its whole environment alive; flat closures avoid this leak. Each nesting level costs one load.
7. **AOT:** offsets are static and portable. Singleton and constant folding embed heap pointers, so `tryGetConstantClosureVar` returns nothing when `m_plan.isUnlinked()`.
8. **JS limits:** sloppy `eval`/`with`; mapped `arguments` (ScopedArguments); per-iteration `let`, which needs a fresh environment each iteration when captured; the debugger.
9. **Otter experiment:** in the compiler, allocate one environment object per scope instance for captured mutable bindings, and have each closure hold one ref to it. This replaces one `UpvalueCell` per binding (`crates/otter-vm/src/upvalue.rs`). On earley-boyer, measure allocations (110.9M cells today; Otter figure, not re-verified), retired instructions and GC share. Refute if allocations fall by less than 3x (for example because per-iteration `let` dominates) or instructions do not move.

## 2. Sinking of activations and closures (FTL)
1. **Source:**
   - `J/dfg/DFGObjectAllocationSinkingPhase.cpp`: `ObjectAllocationSinkingPhase::performSinking`, `promoteLocalHeap`, `placeMaterializations`, `createMaterialization`, `populateMaterialization`, `convertToPhantomNewFunction`, `convertToPhantomCreateActivation`, `MaterializeCreateActivation`.
   - `dfg/DFGPromotedHeapLocation.h`: `ActivationScopePLoc`, `ActivationSymbolTablePLoc`, `ClosureVarPLoc`, `FunctionActivationPLoc`, `FunctionExecutablePLoc`.
   - `ftl/FTLOSRExit.h`: `OSRExitDescriptor::m_materializations`, `OSRExitValues`.
   - `dfg/DFGPlan.cpp`: the phase runs after SSA conversion, in FTL only.
2. **Work eliminated:** allocation, initialization and write barriers for environments and closures that do not escape. `GetClosureVar`/`PutClosureVar` on a sunk environment become SSA values. Once `arr.forEach(x => …)` is inlined, neither the closure nor its environment exists.
3. **Hot path:** zero instructions. Objects are materialized only on paths where they escape, or at OSR exit.
4. **Invariants:**
   - It is a must-points-to plus escape analysis ("similar to an abstract interpreter focused on local allocations").
   - It supports cycles between allocations: `PhantomNewFunction` ↔ `PhantomCreateActivation` through `FunctionActivationPLoc`/`ClosureVarPLoc`.
   - It skips allocations whose `singleton().isStillValid()`, so card 1's folding still applies.
5. **Deopt:** an exit rebuilds sunk objects in dependency order. FTL exit values reference materializations by LEB128 index ("0x49 MaterializeNewObject: LEB128 index into OSRExitDescriptor::m_materializations").
6. **Cost:** a fixpoint over the whole function, which is why it runs only in FTL.
7. **AOT:** it is a static transformation. AOT code can use it if the artifact keeps deopt descriptors. Without them, it applies only where escape is known without speculation.
8. **JS limits:** the closure escapes if it is passed to a call that is not inlined, stored to the heap, or exposed through `eval` or `arguments` aliasing. This is the same class of analysis as Graal's partial escape analysis (PEA); JS adds scope objects.
9. **Otter experiment:** count `Op::MakeClosure` and `alloc_upvalue` per executing tier (interpreter, Template, Machine) on earley-boyer and on a `forEach` microbenchmark.
   - If most of the 10.2M closures and 110.9M cells (Otter figures, not re-verified) come from frames outside Machine, sinking cannot close the gap; cards 1 and 3 matter more.
   - Otherwise, extend partial escape analysis to graphs of closures and cells.

## 3. Inline allocation: bump'n'pop in JIT code
1. **Source:**
   - `J/heap/FreeListInlines.h`: `FreeList::allocateWithCellSize`.
   - `heap/LocalAllocator.h`; `heap/BlockDirectory.h`.
   - `jit/AssemblyHelpers.cpp`: `AssemblyHelpers::emitAllocateWithNonNullAllocator`.
   - `jit/AssemblyHelpers.h`: `emitAllocateJSObjectWithKnownSize`, `mutatorFence`.
   - Riptide post (webkit.org/blog/7122, 2017-01-20): "Bump’n’pop’s bump-allocator always bumps by the block’s cell size to make it look like the objects had been allocated from the free list."
2. **Work eliminated:** a runtime call per allocation, and sweeping of empty blocks.
3. **Hot path (ARM64, from the emitter):**
   - Fast: `ldp start,end,[alloc+freeList]`; `cmp`/`b.hs pop`; `add next,start,#cellSize` (an immediate when the cell size is constant, otherwise loaded from `LocalAllocator::offsetOfCellSize`); `str next`; then the 8-byte header and butterfly stores.
   - Pop path: `ldp nextInterval,secret`, test the sentinel bit, `xor` the scrambled bits to get the next interval, then bump again.
   - Cold: the C++ slow path (sweep, new block, GC).
4. **Invariants:**
   - "When going to the slow path, we must leave resultGPR with zero in it."
   - Intervals are never empty ("we don't create empty intervals").
   - Free-list links are XOR-scrambled with `m_secret`.
   - For known-size sites (`emitAllocateJSObjectWithKnownSize`) the size class is fixed at compile time. The emitter also accepts a non-constant allocator in a register with a runtime-loaded cell size. *(Corrected.)*
5. **GC:** new cells are young (sticky mark bits, card 4), so their initializing stores skip barriers. The slow-path call needs no stack map, because roots are found conservatively.
6. **Cost:** one `LocalAllocator` per size class.
7. **AOT:** each site embeds the allocator address (`TrustedImmPtr(allocator.allocator().localAllocator())` when `allocator.isConstant()`; *corrected spelling*). AOT code must instead load it through a VM or thread register. The size-class choice is portable.
8. **JS limits:** fixed-size cells only on this path. Variable-size cells use `emitAllocateVariableSized`.
9. **Otter experiment:** emit inline nursery bump allocation in Template and Machine code for closures, cells, object literals and known-size `new`. This replaces the Rust runtime calls that allocate in old space. Measure ast_ctor and earley instructions, scavenges, old-space growth and zlib RSS. Confirm if ast_ctor instructions drop by at least 30%.

## 4. Riptide: non-moving generational GC, conservative stack scan, no stack maps
1. **Source:**
   - `J/heap/ConservativeRoots.cpp`: `ConservativeRoots::genericAddPointer` (binary search over precise allocations, `MarkedBlock::blockFor`, `TinyBloomFilter`, block-set `contains`, `isLiveCell`) and `genericAddSpan`.
   - `heap/MachineStackMarker.cpp`: `MachineThreads::gatherFromCurrentThread`, `tryCopyOtherThreadStack`.
   - `jit/AssemblyHelpers.h`: `barrierBranch` (load8 at `cellStateOffset`, compare with `addressOfBarrierThreshold`).
   - `dfg/DFGStoreBarrierInsertionPhase.cpp`: barriers elided by GC epoch.
   - Riptide post: "we don’t clear any mark bits at the start of an eden collection" ("sticky mark bit generational garbage collection").
   - Speculation post (webkit.org/blog/10308): "The collector scans the stack conservatively. This means that compilers don’t have to worry about how to report pointers to the collector."
2. **Work eliminated:**
   - GC stack and register maps in all tiers.
   - Rooting and handle scopes in runtime code.
   - Re-deriving pointers after an allocation.
   - Copying survivors.
3. **Hot path:** nothing at calls or allocations. The write barrier is a byte load, a compare and a branch on the object, whatever value is stored. It is elided for objects allocated in the current epoch. Cold: each GC scans every word on the stack. For each word it runs a binary search over precise (large) allocations, then masks to a block, checks the Bloom filter, looks up the set and checks liveness.
4. **Invariants:**
   - Every small cell sits in an aligned, segregated `MarkedBlock`, so any word can be tested. Large cells are precise allocations, found by binary search.
   - Objects referenced conservatively are pinned implicitly. That is sound only because nothing moves.
   - Pointers into the middle of a cell are aligned down with `cellAlign`. A separate `IndexingHeader`-sized check catches butterfly pointers that land just past the end of a cell or of the previous block. *(Corrected.)*
5. **GC/deopt:** optimized code keeps pointers in any register across calls. B3 stackmaps serve only OSR exit.
6. **Cost:** fragmentation (no compaction), false retention, and 64-bit pointers.
7. **AOT:** the most AOT-friendly option. Native code needs no GC metadata, and runtime code needs no rooting API.
8. **JS limits:** independent of the language. It conflicts with Otter's moving GC: a moving nursery would have to pin every page a conservative word points into. That is mostly-copying collection, which JSC does not do.
9. **Otter experiment:** attribute ast_ctor and earley samples to root bookkeeping: handle scopes, frame-root registration, safepoint maps, and re-derivation after allocation. If that exceeds 10% of instructions, prototype a conservative native-stack scan with nursery page pinning. Refute if more than 10% of pages get pinned or savings are under 5%.

## 5. Value, cell and Structure layout: the prototype is part of the Structure
1. **Source:**
   - `J/runtime/JSCJSValue.h`, encoding comment: pointer tag 0x0000; doubles offset by 2^49; int32 `FFFE:0000:IIII:IIII`; False 0x06, True 0x07, Undefined 0x0a, Null 0x02; 0 = empty/hole.
   - `runtime/JSCell.h`: an 8-byte header of u32 `m_structureID`, `m_indexingTypeAndMisc`, `m_type`, `m_flags`, `m_cellState`.
   - `runtime/StructureID.h`: `decode()` = bits with `nukedStructureIDBit` cleared + `structureIDBase()`. JIT code (`emitNonNullDecodeZeroExtendedStructureID`) emits one `or64` with the base.
   - `runtime/Structure.h`: `m_prototype`, `m_realm`, `m_transitionTable`, `m_transitionWatchpointSet`, `hasPolyProto()`, `s_maxTransitionLength = 128` (also `s_maxTransitionLengthForNonEvalPutById = 512`, `s_maxTransitionLengthForRemove = 4096`), `addPropertyTransition`.
   - `runtime/StructureTransitionTable.h`: a single slot or a `WeakGCMap`.
   - `runtime/Butterfly.h`: negative `offsetOfPropertyStorage()`, `offsetOfPublicLength`, `offsetOfVectorLength`.
2. **Work eliminated:**
   - A separate prototype guard: a structure check proves which prototype the object has.
   - Pointer decompression: cell pointers are raw. Only the 32-bit StructureID needs decoding, which is one OR in JIT code (one add in C++). *(Corrected.)*
   - Extra storage pointers: one butterfly holds `length`, out-of-line properties and elements.
3. **Hot path:**
   - A self load is `ldr w,[obj]; cmp w,#id; b.ne; ldr v,[obj+off]`. Out of line, it is `ldr b,[obj+8]; ldr v,[b-off]`.
   - The int check is one compare against a tag held in a callee-save register ("The last three csr registers are used used to store the PC base and two special tag values"; the doubled "used" is in the source).
   - `op_get_prototype_of` is `loadq Structure::m_prototype`. Zero means poly proto: the prototype sits in the object at `knownPolyProtoOffset`.
4. **Invariants:**
   - The transition for a given (uid, attributes) always leads to the same structure.
   - Past the transition limits (128 in general, 512 for `PutById` adds, 4096 for remove and attribute change), the object gets a *cacheable dictionary* structure. ICs flatten a dictionary (`flattenDictionaryStructure`, `bytecode/Repatch.cpp`) before caching it; uncacheable dictionaries are not cached. *(Corrected.)*
   - Poly-proto structures keep the prototype in the object.
   - A nuked StructureID marks a concurrent transition in progress.
5. **GC:** structures live in a reserved structure heap, and IDs are offsets into it. Transition maps are weak.
6. **Memory:** values and pointers take 8 bytes, where V8 and Otter use compressed 4-byte references. There is one structure per (shape, prototype, realm).
7. **AOT:** StructureIDs are runtime values, so AOT code must load them from a per-module table. Because the prototype is inside, a structure belongs to one realm (`m_realm`).
8. **JS limits:** `setPrototypeOf` is a transition. Class factories that produce many prototypes use poly proto. Proxies and exotic objects are flagged in `m_flags` (`OverridesGetPrototype`).
9. **Otter experiment:** add the prototype to Otter shapes, ordinary and dictionary.
   - In Machine output for mega_method and ts, count dynamic prototype loads and compares per access before and after, plus retired instructions.
   - Separately, count cage-base adds per hot loop in crypto and zlib.

## 6. Watchpoints, ObjectPropertyCondition, adaptive watchpoints
1. **Source:**
   - `J/bytecode/Watchpoint.h`: `WatchpointSet`, `InlineWatchpointSet`; states `ClearWatchpoint`/`IsWatched`/`IsInvalidated`.
   - `bytecode/PropertyCondition.h`: `Presence, Replacement, Absence, AbsenceOfSetEffect, AbsenceOfIndexedProperties, Equivalence, HasStaticProperty, HasPrototype`. *(Corrected: `Replacement` added.)*
   - `bytecode/ObjectPropertyCondition.h`: `isStillValid`, `structureEnsuresValidity…`.
   - `bytecode/ObjectPropertyConditionSet.h`: `generateConditionsForPrototypePropertyHit`, `generateConditionsForPropertyMiss`, `generateConditionsForInstanceOf`.
   - `bytecode/AdaptiveInferredPropertyValueWatchpointBase.cpp`: `fire()`.
   - `bytecode/LLIntPrototypeLoadAdaptiveStructureWatchpoint.h`; the DFG node `InvalidationPoint`.
   - Speculation post: "structure transition watchpoint", "property replacement watchpoint", "Failing watchpoints cause immediate jettison."
2. **Work eliminated:** structure checks on every object along the prototype chain for proto hits, misses, `in` and `instanceof`. Under `Equivalence`, the loaded method is a compile-time constant, so it is called directly or inlined without checking the callee.
3. **Hot path:** only the receiver's structure check; hops along the chain cost nothing.
   - Cold: when a watchpoint fires, the IC is reset or the code is jettisoned.
   - An adaptive watchpoint instead runs `if (m_key.isWatchable(PropertyCondition::EnsureWatchability)) { install(vm); return; }`. `AdaptiveInferredPropertyValueWatchpointBase` accepts only `Equivalence` keys (`RELEASE_ASSERT`). So when the holder only gained an unrelated property, the watchpoint pair (structure transition + property replacement) moves to the holder's new structure.
4. **Invariants:** a condition is usable only if it can be watched: the holder structure's transition set must still be valid, and for Equivalence the replacement set too. Code is installed only if every set is still valid when compilation finishes.
5. **Deopt:** jettison, plus `InvalidationPoint`s so frames already running leave at the next safe point.
6. **Cost:** one watchpoint per (condition × dependent code). Jettison storms follow if prototypes mutate after warm-up.
7. **AOT:** needs a live heap. AOT code must turn conditions into explicit guards, or into checks linked at load time against a startup snapshot.
8. **JS limits:** the mechanism exists because prototypes are mutable. Proxy and dictionary holders cannot be watched; they fall back to guards or the megamorphic path. A getter is Presence of a GetterSetter plus a call.
9. **Otter experiment:** once card 5 is in place, attach a transition watch set to prototype-holder shapes and express method loads as Presence/Absence conditions. Measure instructions on mega_method and ts. Count jettisons across Octane and test262 as a stability check.

## 7. Inline caches: data-only handler chains, code patching only in FTL
1. **Source:**
   - `J/bytecode/PropertyInlineCache.h`: `PropertyInlineCacheType { Handler, Repatching }`, `HandlerPropertyInlineCache`, `RepatchingPropertyInlineCache`, `m_inlineAccessBaseStructureID`, `countdown`, `repatchCount`, `resetStubAsJumpInAccess`.
   - `bytecode/InlineCacheCompiler.h`: `PolymorphicAccess`, `InlineCacheCompiler::compileHandler`, `generateWithGuard`, `emitDataICPrologue`.
   - `bytecode/InlineCacheHandler.h`; `bytecode/SharedJITStubSet.h`.
   - `runtime/MegamorphicCache.h`: the load cache has 2048 primary and 512 secondary entries.
   - `runtime/OptionsList.h`: `useHandlerICInFTL` false, `repatchBufferingCountdown` 6.
2. **Work eliminated:** patching machine code, flushing the icache and toggling W^X in LLInt, Baseline and DFG, and generating a stub per site. The source says: "multiple IC sites with the same access pattern share the same machine code. This sharing works because everything is data only".
3. **Hot path:**
   - The site checks the structure inline against a field of the IC (data, not an immediate), loads the offset from the IC, then loads the property.
   - On a miss: `load m_handler; call [handler+jumpTarget]`. The handler compares against `[handler+structureID]`, does the access and returns. If it also misses, it follows `m_next`.
   - In FTL, a patched stub dispatches over structureIDs, linearly or with a `BinarySwitch`.
4. **Invariants:**
   - Handler stubs run in the caller's frame and set up no frame unless they call a getter.
   - A handler reads its own struct only before making any call.
   - Stub code is freed only after GC proves it is not on any stack (`GCAwareJITStubRoutine`).
5. **GC/deopt:** watchpoints trigger resets. Structures are held weakly and pruned.
6. **Cost:** one extra indirect call on polymorphic sites. FTL still patches, for peak speed.
7. **AOT:** handler ICs are the AOT-compatible design. The code is position-independent, the state is data, and stubs can be precompiled as shared thunks.
8. **JS limits:** getters and setters, custom accessors, proxies (`ProxyableAccessCase`) and module namespaces. Megamorphic sites use the hashed `MegamorphicCache`.
9. **Otter experiment:** measure Otter's `property_ic.rs`/`cache_ir.rs` path on mega_method, in instructions per access at 1, 4, 16 and 64 shapes. Compare it with a prototype that uses handler chains plus a global (shape, atom)→offset megamorphic table. Confirm if sites with 16 or more shapes get at least 2x cheaper.

## 8. Arguments elimination and varargs forwarding
1. **Source:**
   - `J/dfg/DFGArgumentsEliminationPhase.cpp`: `identifyCandidates` over `CreateDirectArguments`, `CreateClonedArguments`, `CreateRest`, `Spread` and `NewArrayWithSpread`; `eliminateCandidatesThatEscape`; `eliminateCandidatesThatInterfere`; `transform`, which produces `Phantom*`, `GetMyArgumentByVal[OutOfBounds]`, `ForwardVarargs`, `CallForwardVarargs`, `ConstructForwardVarargs` and `TailCallForwardVarargs`.
   - `dfg/DFGVarargsForwardingPhase.cpp`: works within one block (`handleBlock`).
   - `dfg/DFGPlan.cpp`: VarargsForwarding runs only when `!isFTL()`; ArgumentsElimination runs in the FTL SSA pipeline.
2. **Work eliminated:** allocating and copying `arguments`, rest and spread arrays for `f.apply(this, arguments)`, `(...a) => g(...a)` and `arguments[i]`.
3. **Hot path:** `arguments[i]` becomes a bounds check and a stack load. Forwarding copies stack slots straight into the callee's frame.
4. **Invariants:**
   - Nothing may overwrite the argument stack slots between creation and use (`clobberize` over the `Stack` heaps).
   - Rest and spread are eliminated only under `isWatchingHavingABadTimeWatchpoint`, meaning no indexed accessors on the Array or Object prototypes.
   - Array iteration must also be unobservable (`ArrayUse`).
5. **Deopt:** phantom nodes rebuild the object at exit.
6. **Cost:** a liveness pass plus an interference check; cheap.
7. **AOT:** static, except that the HavingABadTime assumption needs a global guard.
8. **JS limits:** sloppy mapped `arguments` that alias parameters, `arguments` that escape, and `fn.arguments`.
9. **Otter experiment:** after d9e81603 ("elide local arguments objects"), count the `arguments`, rest and spread arrays still allocated in ts and earley. If hot forwarding paths still allocate, add forwarding call ops.

## 9. One frame layout across tiers; lazy, compressed OSR exit; validated OSR entry
1. **Source:**
   - `J/llint/LowLevelInterpreter.asm`: offlineasm; "For consistency with the baseline JIT, t0 is always r0"; `checkSwitchToJIT`.
   - `llint/LowLevelInterpreter64.asm`: `checkSwitchToJITForLoop` → `_llint_loop_osr`.
   - `LLIntOffsetsExtractor`.
   - `jit/JIT.h` and `jit/BaselineJITCode.h`: `JITConstantPool`, `BaselineUnlinkedPropertyInlineCache`, `m_isShareable`.
   - `dfg/DFGOSRExit.cpp`: `OSRExitStream`, `operationCompileOSRExit`, `OSRExit::compileExit`.
   - `dfg/DFGOSREntry.cpp`: `prepareOSREntry`, which validates `OSREntryExpectedValues`; `prepareCatchOSREntry`.
   - Speculation post: "We use an operation called MovHint as our delta encoding"; "DFG nodes do not have data flow edges for the stackmap. Doing literally that would be too costly in terms of memory usage".
   - Tier-up thresholds, per the same post: 500 points (LLInt→Baseline), 1000 (Baseline→DFG) and 100000 (DFG→FTL). A call adds 15 points; a loop iteration adds 1. The post gives these as static baselines that JSC adjusts dynamically.
2. **Work eliminated:**
   - Frame conversion between LLInt and Baseline, since they share a layout ("The only difference between frame layouts between functions in the profiling tier is the frame size").
   - A Baseline compile per CodeBlock, since unlinked Baseline code is shared through a constant pool.
   - Exit thunks built up front, since each is compiled on its first exit.
   - A stackmap on every node.
3. **Hot path:** none. Checking whether to tier up is an add-and-branch on a counter.
4. **Invariants:**
   - `exitOK` marks the nodes where the exit state is consistent.
   - MovHint counts as a store effect.
   - OSR entry refuses (returns nullptr) when frame values contradict the predicted abstract values.
5. **Deopt:** an exit rebuilds Baseline or LLInt frames, including inlined frames and materialized objects.
6. **Cost:** exit metadata is `OSRExitStream`, a chunked byte stream of flag words and deltas against the previous exit.
7. **AOT:** unlinked Baseline code embeds no heap constants, and it is as close as JSC gets to AOT. Exits need the bytecode and a profiling tier inside the executable.
8. **JS limits:** any bytecode can be an exit target, so the interpreter must be able to resume in the middle of a block.
9. **Otter experiment:** measure deopt-metadata bytes per Machine function and the cost of the first exit on earley. Try generating exit thunks lazily and delta-encoding exit state. Confirm if code bytes or compile time drop by at least 20%.

## 10. B3/Air: why FTL left LLVM
1. **Source:**
   - `J/b3/B3Procedure.h`, `B3CheckValue.h`, `B3PatchpointValue.h`, `B3StackmapValue.h`, `B3LowerToAir.cpp`.
   - `b3/air/AirGenerate.cpp`: optLevel 0 uses `GenerateAndAllocateRegisters`; higher levels use `allocateRegistersByGreedy` + `allocateStackByGraphColoring`. `defaultB3OptLevel` is 2.
   - B3 post (webkit.org/blog/5852, 2016-02-15): "the overwhelming majority – often three quarters or more – of the FTL compile time is LLVM"; "LLVM takes 4.7 times longer to compile than B3".
   - Speculation post: "the DFG compiler needs about 4× the time of the baseline compiler, and the FTL needs about 6× the time of the DFG."
2. **Work eliminated:** memory traffic inside the compiler.
   - Values do not track their users. `replaceWithIdentity` plus a lazy `performSubstitution` stand in for replace-all-uses.
   - An Air `Inst` is passed by value, with inline room for three arguments.
   - A block is "a single contiguous slab of memory".
   - `Check` is a branch and an exit in one opcode, so no extra blocks are created.
3. **Hot path (compile time):** patchpoints need no pre-sized code, and registers for exit values are picked late.
4. **Invariants:** B3 is SSA. Each stackmap value is constrained by a `ValueRep`.
5. **Deopt:** `Check` and `Patchpoint` carry the exit state. Generator callbacks emit the exit code last.
6. **Cost:** optLevel 0 skips the greedy register allocator so compiles stay fast.
7. **AOT:** the backend assumes nothing about the running process beyond constants, so it could serve both JIT and AOT. JSC does not use it for AOT.
8. **JS limits:** none; it is language-neutral, and Wasm uses it too.
9. **Otter experiment:** profile the Machine compile of the function that takes about 10 s (Otter figure, not re-verified).
   - Measure bytes allocated per HIR value, the cost of keeping use-lists up to date, and blocks created per guard.
   - Try guards that are one instruction with an exit edge, and instructions allocated in slabs.
   - Confirm if compile time drops at least 3x with the same generated code.

## 11. Bytecode cache and unlinked code (AOT-relevant)
1. **Source:**
   - `J/runtime/CachedTypes.cpp`: `Encoder`, `Decoder`, `CachedPtr`, `CachedFunctionExecutable`, `CachedFunctionCodeBlock`, `GenericCacheEntry` (with `computeJSCBytecodeCacheVersion()` and a boot-session UUID, `m_bootSessionUUID`), `encodeCodeBlock`, `decodeCodeBlockImpl`, `isCachedBytecodeStillValid`.
   - `jit/JITCompilationMode.h`: `UnlinkedDFG`.
   - `dfg/DFGGraph.cpp`: `isUnlinked()` turns off folding of heap constants.
2. **Work eliminated:** parsing and bytecode generation on a warm start. Function bodies are decoded lazily from offsets into the buffer (`m_cachedCodeBlockForCallOffset`).
3. **Hot path:** none; a body is decoded on its first call.
4. **Invariants:** only unlinked, heap-independent bytecode is cached. Offsets are self-relative, with no absolute pointers. A version or boot-UUID mismatch invalidates the cache.
5. **GC:** decoded objects are ordinary cells.
6. **Cost:** not measured.
7. **AOT:** this is the split Otter needs. The portable layer is bytecode, unlinked Baseline and unlinked DFG. The linked layer is heap constants, StructureIDs and watchpoints. JSC ships no machine code in the cache.
8. **JS limits:** entries are keyed by source text. `eval` and `new Function` results are not cached across runs.
9. **Otter experiment:** compile a Machine function in an unlinked mode, where every heap constant and shape ID goes through a relocation table. Measure the throughput loss against linked code on ts and crypto. If the loss is under 10%, one unlinked backend plus a link step can be the shared JIT/AOT foundation.

## What JSC does not spend that Otter spends
- **A GC object per captured binding.** JSC uses one environment per scope instance, and often none after sinking.
- **A Rust runtime call and an old-space allocation per closure or cell.** JSC bump-allocates young cells inline and skips barriers on their initializing stores.
- **Pointer decompression on every cell load.** Only the StructureID is compressed, and decoding it is one OR in JIT code.
- **Stack maps, handle scopes, root registration and re-deriving pointers after allocation.** The heap does not move and the stack is scanned conservatively.
- **A prototype guard at every hop.** The prototype is part of the Structure. Chain conditions are watched, and adaptive watchpoints (Equivalence) move to the new structure when a condition still holds.
- **Code patching in the lower tiers.** Handler ICs are data, with shared thunks.
- **Baseline code per instance.** Unlinked Baseline code is shared.
- **Deopt thunks built up front and a stackmap on every node.** Exits are compiled lazily from delta-compressed state.
- **`arguments` and rest arrays on forwarding paths in optimized code.**
- **An LLVM-class compile cost.** LLVM took 4.7x longer to compile than B3.

JSC pays for this with 8-byte pointers, no compaction, false retention and the risk of jettisons.

Fetched sources are cached under /private/tmp/claude-501/-Users-alexanderstreltsov-work-octofhir-otter/a2e9abdb-0d8f-437a-8cf1-f2cad155594e/scratchpad/jsc/ (blog text in /private/tmp/claude-501/-Users-alexanderstreltsov-work-octofhir-otter/a2e9abdb-0d8f-437a-8cf1-f2cad155594e/scratchpad/blog*.txt).