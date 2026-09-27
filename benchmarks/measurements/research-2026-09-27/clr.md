## Citation verification

The dotnet/runtime tag `v10.0.0` resolves to commit `60629d14374c56f1cb51819049ad1fa529307f8d`, so the pin in the report is correct. I downloaded every file below at the pinned revisions and grepped it.

| # | Citation | Verdict | Note |
|---|---|---|---|
| 1 | Roslyn `ClosureConversion.cs` class doc, plus `MergeEnvironments`, `InlineThisOnlyEnvironments` and `CanTakeRefParameters` in `ClosureConversion.Analysis.cs` | Verified, with two corrections | The class doc says there is one `SynthesizedClosureEnvironment` "for each scope with captured variables" and "a single field for each captured variable". The `MergeEnvironments` quote is exact (Analysis.cs:430). Correction 1: `MergeEnvironments` runs only when `OptimizationLevel == Release`. Correction 2: a struct environment also requires the function to be neither async nor an iterator, and the method must not be in a variant interface. |
| 2 | `runtime/arm64/AllocFast.S` `RhpNewFast`; `jithelpers.h` maps `CORINFO_HELP_NEWSFAST` to `RhpNew`; `CorInfoImpl.RyuJit.cs` `GetHelperFtnUncached` | Corrected | `jithelpers.h`:102 declares `NEWSFAST` as a `DYNAMICJITHELPER` whose default is `RhpNew`. The VM swaps in `RhpNewFast` inside `InitJITAllocationHelpers` (`vm/jitinterfacegen.cpp`) when thread allocation contexts are used. The fast path is 11 instructions: the report left out `mov x0, x12`. "Objects always go to gen0" is true only on this path. `CEEInfo::getNewHelperStatic` sends objects of at least `LARGE_OBJECT_SIZE`, objects with a finalizer, COM objects, and runs under GC stress or allocation tracking to the slow helper `CORINFO_HELP_NEWFAST`. The NativeAOT line, `ExternFunctionSymbol("RhpNewFast")`, is correct. |
| 3 | `jit/flowgraph.cpp` `fgSetBlockOrder` → `fgHasCycleWithoutGCSafePoint()` → `SetInterruptible(true)`; `compiler.h` `GCPOLL_CALL`/`GCPOLL_INLINE`/`fgCreateGCPoll`; `clr-abi.md` | Verified | The code matches: a cycle that avoids every `BBF_GC_SAFE_POINT` block makes the method fully interruptible. `clr-abi.md` confirms "effectively all methods with EH must be fully interruptible (or at a minimum all try bodies)" and the requirement for an interruptible point right after the catch funclet prolog. |
| 4 | `getExactClasses` / `getStaticFieldContent` in `jitinterface.cpp`, `CorInfoImpl.ReadyToRun.cs` and `CorInfoImpl.RyuJit.cs` | Verified, with detail added | VM: returns -1, with the comment "currently implemented only on NativeAOT". R2R: returns -1 with `// Not implemented for R2R yet`, and reads static content only for RVA fields of types inside the version bubble. NativeAOT: returns 0 if `CanNeverHaveInstanceOfSubclassOf`, 1 if the type is effectively sealed, and otherwise the `GetImplementingClasses` list; static content comes from RVA data or preinitialized data. The VM also checks `IsClassInitedOrPreinited() && IsFdInitOnly`. Separately, the card 9 claim about `getStaticObjRefContent` was **wrong**: when `ignoreMovableObjects` is set, a movable object makes the call return false (nothing is folded). It does not come back as a handle. |
| 5 | `vm/prestub.cpp` `ExternalMethodFixupWorker` (`*(TADDR *)pIndirection = pCode`); `vm/readytoruninfo.cpp` `GetEntryPoint` → `FixupDelayList` → `SetReadyToRunRejectedPrecompiledCode()`; `LoadDynamicInfoEntry` checks | Verified, with detail added | All the symbols exist, and the rejection path matches (readytoruninfo.cpp:1257-1259). On the direct-call path, the cell store is in the helper `PatchNonVirtualExternalMethod`, which the worker calls. The worker itself does the store only on the virtual-stub-dispatch path. `Check_TypeLayout`/`Check_FieldOffset` return FALSE, so the method goes to the JIT. `Verify_*` failures call `EEPOLICY_HANDLE_FATAL_ERROR_WITH_MESSAGE`, which is a hard failure. |
| 6 | `inc/clrconfigvalues.h` `TC_CallCountThreshold` 30, `TC_CallCountingDelayMs` 100, plus the quotes from the tiering, OSR and PGO docs | Verified, with detail added | 30 and 100 are the release values; `_DEBUG` builds use 2 and 1. These quotes are exact: "the precompiled code is the tier0 version", "only 10-20% of methods make it to Tier1", "~13MB" on a "10GB" service, "incorporates the (live portion of the) original method frame", and deopt in §6. |

Other checks: all verified.
- **Pure helpers:** `HelperCallProperties::init` (utils.cpp:1626-1632) sets `isPure`, `noThrow` and `nonNullReturn`.
- **Helper expansion:** `fgExpandStaticInit` and `fgExpandThreadLocalAccess` both contain "Always expand for NativeAOT".
- **Frozen objects:** the `frozenobjectheap.h` header comment matches the report.
- **Mode flags and predicates:** `CORJIT_FLAG_AOT` = 11 ("ReadyToRun or NativeAOT") exists, as do `IsAot`, `IsNativeAot`, `IsReadyToRun` and `IAT_RELPVALUE`.
- **Otter paths:** `alloc_upvalue` (upvalue.rs:81), `emit_backedge_poll` (template/arm64.rs:2056, 9 instructions including `str x10,[x9]`), `subs w29` (machine/numeric/arm64.rs:249), and the `RelocationTarget` variants (relocation.rs:108) all exist.
- **Not re-checked in this pass:** the citations in cards 8 and 10, and the NativeAOT/R2R files named in cards 6 and 9 (`ExceptionHandling.cs`, `TypePreinit.cs`, `StartupCodeHelpers.cs`, `ARM64ReadyToRunGenericHelperNode.cs`, `Crossgen2RootCommand.cs`). They stay as the original report states them.

---

# .NET CLR: one compiler for JIT, ReadyToRun and NativeAOT (mechanism cards for Otter)

**Pinning.** dotnet/runtime tag `v10.0.0` (commit `60629d14374c56f1cb51819049ad1fa529307f8d`, tag resolution confirmed). Paths are relative to `https://github.com/dotnet/runtime/blob/v10.0.0/`. Roslyn is pinned at `dotnet/roslyn@90083ecf59c688110a69cbf3871e5edb2693aadc`. Every file and symbol below was fetched and grepped at those revisions unless it is marked "not verified". A second pass re-checked the citations for cards 1–5 and 7 plus the key symbols of cards 6 and 9 against source. Otter paths were checked in the local tree.

**Where process binding happens.** RyuJIT (`src/coreclr/jit`) never decides whether it is running in a live process or in a build tool. Every question that depends on the process goes through the JIT-EE interface (`src/coreclr/inc/corinfo.h`, `corjit.h`): a type's or helper's address, a field offset, a static's current value, or which subclasses exist. Three hosts implement that interface:

- **The VM:** `CEEInfo`/`CEEJitInfo` in `src/coreclr/vm/jitinterface.cpp`.
- **crossgen2 (ReadyToRun):** `src/coreclr/tools/Common/JitInterface/CorInfoImpl.cs` plus `.../ILCompiler.ReadyToRun/JitInterface/CorInfoImpl.ReadyToRun.cs`.
- **ILCompiler (NativeAOT):** the same `CorInfoImpl.cs` plus `.../ILCompiler.RyuJit/JitInterface/CorInfoImpl.RyuJit.cs`. The shared file switches with `#if READYTORUN`.

The binding lives in the answers the host gives:

- The VM returns live addresses and patches relocations immediately.
- crossgen2 returns indirection cells, which fixups fill lazily at run time.
- NativeAOT returns link-time symbols and whole-program facts.

---

## 1. One environment per scope, not one cell per binding (C# compiler)

This is Roslyn, not the CLR. It is here because it is the reason the CLR never sees per-variable cells.

1. **Source:** `src/Compilers/CSharp/Portable/Lowering/ClosureConversion/ClosureConversion.cs`.
   - Its class doc says there is one `SynthesizedClosureEnvironment` "for each scope with captured variables", with "a single field for each captured variable".
   - `ClosureConversion.Analysis.cs` has `MergeEnvironments`, `InlineThisOnlyEnvironments` and `CanTakeRefParameters` (struct environments).
   - `MergeEnvironments` runs only in Release builds (`OptimizationLevel.Release`), because merging changes variable scopes seen while debugging.
2. **Work eliminated:**
   - A scope with N captured bindings costs 1 allocation at scope entry, not N.
   - Creating a closure is one delegate (target = environment, plus a method pointer), however many variables it captures.
   - `MergeEnvironments` folds a child environment into its parent "only if exactly the same closures directly or indirectly reference both" (Release only).
   - A local function gets a struct environment passed by ref, with no heap allocation, if all of these hold:
     - it is never converted to a delegate;
     - it is not async or an iterator;
     - it is not inside a variant interface.
3. **Hot/cold path:** A captured read is `ldr env,[closure+target]; ldr v,[env+off]`, plus one load for each extra hop up the environment chain. There is no cold path.
4. **Invariants:** Merging must not extend any variable's lifetime. Each variable lives in exactly one environment. A loop-body scope allocates a fresh environment on every iteration.
5. **GC/exception/deopt:** The environment is an ordinary object. For deopt, a frame materializes one environment pointer instead of N cell references.
6. **Memory and compile cost:** One header plus N fields, versus N×(header+value) cells plus a 4-byte ref per capture in every closure. The analysis is linear in the scope tree.
7. **AOT:** Pure compile-time lowering. Environment layouts become fixed shapes.
8. **JS applicability and limits:**
   - This maps directly to spec Environment Records; V8 Contexts use the same design.
   - Direct `eval`, `with` and sloppy `arguments` aliasing need a dynamic environment, so keep today's path for them.
   - `let`/`const` fields need TDZ hole checks.
   - Per-iteration `let` semantics need the environment copied on each iteration.
9. **Otter experiment:** On earley-boyer, instrument `alloc_upvalue` (`crates/otter-vm/src/upvalue.rs`) and `MakeClosure` to histogram captured bindings per scope and closures per scope.
   - If the mean is at most about 1.2 bindings per scope, this card is refuted.
   - Otherwise, lower non-eval functions to scope environments in `otter-compiler` and re-measure against the baseline: 110.9M cell allocations, 10.2M closures, about 30% of samples in GC, and retired instructions.

## 2. Allocation as a thread-local bump helper, picked per type

1. **Source:**
   - `src/coreclr/runtime/arm64/AllocFast.S`: `RhpNewFast`, with rare path `RhpNewObject` → `RhpGcAlloc` behind `PUSH_COOP_PINVOKE_FRAME`.
   - `src/coreclr/inc/jithelpers.h` declares `CORINFO_HELP_NEWSFAST` as a `DYNAMICJITHELPER` whose default is `RhpNew`. At startup the VM rebinds it to `RhpNewFast` in `InitJITAllocationHelpers` (`src/coreclr/vm/jitinterfacegen.cpp`) when thread allocation contexts are in use.
   - The per-type choice is `CEEInfo::getNewHelperStatic` (`jitinterface.cpp`).
   - For R2R: `READYTORUN_FIXUP_NewObject`, and `readytorun-overview.md` ("We will defer the choice of the helper to use to allocate the object to runtime").
   - For NativeAOT: `GetHelperFtnUncached` in `CorInfoImpl.RyuJit.cs` returns `ExternFunctionSymbol("RhpNewFast")`.
2. **Work eliminated:**
   - No full runtime-call transition.
   - No size computation, because the base size is read from the MethodTable.
   - No zeroing on the fast path.
   - Objects on this path always go to gen0. `getNewHelperStatic` gives the fast helper only to types that are all of the following:
     - smaller than `LARGE_OBJECT_SIZE`;
     - without a finalizer;
     - not COM objects;
     - allocated while neither GC stress nor allocation tracking is on.
   - Every other type gets the slow `CORINFO_HELP_NEWFAST`.
3. **Hot/cold path:** 11 arm64 instructions, plus the TLS access and the call:
   - Get the alloc context.
   - Load `m_uBaseSize`, `alloc_ptr` and `combined_limit`.
   - `sub`, `cmp`, `b.hi`, `add`.
   - Store the MethodTable, store `alloc_ptr`, `mov x0,x12`, `ret`.

   The cold path pushes a transition frame and may run a GC.
4. **Invariants:** The fast path writes only the MethodTable word, so the GC must hand out allocation contexts that are already zeroed. The object header is a single MethodTable pointer.
5. **GC/exception/deopt:** A GC can happen only on the rare path, inside a described transition frame.
6. **Memory and compile cost:** No extra memory. Short-lived objects die in gen0.
7. **AOT:** Same helper in all modes. R2R binds it through a per-type cell; NativeAOT links a direct symbol.
8. **JS applicability and limits:** Fits closures, cells, AST nodes and `new C()` objects. A JS object also needs its shape, slots and elements initialized, plus in-object slack tracking.
9. **Otter experiment:** Otter allocates closures and cells in old space through Rust runtime calls.
   - First run a survival census on earley and ast_ctor. If more than about 50% of these objects survive a scavenge, pretenuring is justified and this card is refuted.
   - Otherwise, emit a nursery bump stub (or an inline bump) for closure and cell allocation in the Machine tier. Measure retired instructions and minor-GC promotion.
   - Re-measure zlib RSS (795 MB) too, though no causal link is claimed.

## 3. GC info: partially vs fully interruptible code, no loop polls

1. **Source:**
   - `src/coreclr/inc/gcinfoencoder.h`: `TGcInfoEncoder` (`DefineCallSites`, `DefineInterruptibleRange`, `GetRegisterSlotId`, `GetStackSlotId`, `SetSlotState`, `Build`, `Emit`), with typedefs `GcInfoEncoder` and `InterpreterGcInfoEncoder`. Implementation in `src/coreclr/gcinfo/gcinfoencoder.cpp`.
   - `jit/flowgraph.cpp`: `Compiler::fgSetBlockOrder` → `fgHasCycleWithoutGCSafePoint()` (a cycle that avoids every `BBF_GC_SAFE_POINT` block) → `SetInterruptible(true)`.
   - `jit/compiler.h`: `GCPOLL_CALL`/`GCPOLL_INLINE` and `fgCreateGCPoll`.
   - `docs/design/coreclr/botr/threading.md` (hijacking) and `botr/clr-abi.md` ("GC Interruptibility and EH").
2. **Work eliminated:** Loops pay nothing per iteration for GC.
   - In a loop with a call, the call's return address is already a safepoint.
   - A loop with no safepoint makes the whole method fully interruptible, with GC info for every instruction. The thread is stopped by OS suspension plus an IP redirect.
   - A partially interruptible method is stopped by hijacking its return address.
3. **Hot/cold path:** Zero instructions on the hot path. The cold path is: suspend the thread, decode the GC info, then redirect or hijack.
4. **Invariants:**
   - At each reported offset, every live ref is in a described register or stack slot, and interior pointers are flagged.
   - Methods with EH must be fully interruptible, "or at a minimum all try bodies" (`clr-abi.md`).
   - Catch funclets need an interruptible point right after the prolog.
5. **GC/exception/deopt:** This gives a moving GC precise register roots. The encoding is chunked by `NUM_NORM_CODE_OFFSETS_PER_CHUNK` = 64 (`gcinfotypes.h`), so a lookup is bounded.
6. **Memory and compile cost:** Fully interruptible info is larger. The JIT chooses per method. The encoder picks between RLE and bit-vector encodings (`SizeofSlotStateVarLengthVector`).
7. **AOT:** The info is offset-based and position-independent. The same encoder serves the JIT, R2R, NativeAOT and the CoreCLR interpreter.
8. **JS applicability and limits:** JS needs interrupts for more than GC: termination, CPU budgets and tier-up. A signal-based redirect to a safepoint can serve all of these.
9. **Otter experiment:** The two tiers poll differently today:
   - The Template tier's `emit_backedge_poll` (`crates/otter-jit/src/template/arm64.rs`) costs 9 instructions per back edge, including a read-modify-write store to the fuel counter. The emitter's comment says the fuel batch is attributed to the loop header for OSR tier-up.
   - The Machine numeric poll (`machine/numeric/arm64.rs`) is batched: `subs w29`/`b.ne` per iteration, plus a reserved register.

   Try two changes:
   - (a) Drop the poll in loops whose body has a non-inlined call, and let the callee's entry carry the interrupt check.
   - (b) Replace the Machine batch counter with an interrupt byte set by a signal or timer. First check whether the fuel also serves the CPU budget, besides tier-up; a timer can drive either.

   Measure retired instructions on zlib and crypto. Refute if the gain is under 2%.

## 4. One compiler with host-answered constant lookups

1. **Source:**
   - `corinfo.h`: `ICorStaticInfo`, `ICorDynamicInfo`, `CORINFO_CONST_LOOKUP`, and `InfoAccessType` {`IAT_VALUE`, `IAT_PVALUE`, `IAT_PPVALUE`, `IAT_RELPVALUE`}.
   - `corjit.h`: `ICorJitInfo::allocMem`, `allocGCInfo`, `setEHinfo`, `recordRelocation`.
   - `corjitflags.h`: `CORJIT_FLAG_AOT` = 11 ("ReadyToRun or NativeAOT"). `jit/compiler.h`: `IsAot`, `IsNativeAot`, `IsReadyToRun`.
   - `CEEJitInfo::recordRelocation` patches in place and adds jump stubs when a REL32 overflows. `PublishCode` in `CorInfoImpl.cs` stores relocations into `ObjectNode.ObjectData` instead.
2. **Work eliminated:** One optimizer, register allocator, GC-info encoder and EH encoder serve three products. Across importer, importercalls, compiler, gentree, morph, codegencommon and flowgraph, only 45 lines test for AOT mode (my grep, not re-checked).
3. **Hot/cold path:** JIT code uses an immediate (`IAT_VALUE`). AOT code loads from a cell (`IAT_PVALUE`), and that load can be CSE'd and hoisted like any other load.
4. **Invariants:** The JIT embeds only what the host returns, and relocation kinds are typed (for example ARM64 `PAGEBASE_REL21`/`PAGEOFFSET_12A`). The answers depend on the mode:

   | Query | VM (JIT) | R2R | NativeAOT |
   |---|---|---|---|
   | `getExactClasses` | -1 ("currently implemented only on NativeAOT") | -1 ("Not implemented for R2R yet") | closed-world answer: 0 if `CanNeverHaveInstanceOfSubclassOf`, 1 if effectively sealed, else the `GetImplementingClasses` list (up to the max) |
   | `getStaticFieldContent` | live value of an init-only static, once the class is initialized or preinitialized | RVA data only, and only for types inside the version bubble | init-only statics: RVA data or preinitialized values |

5. **GC/exception/deopt:** GC info, EH clauses and unwind data come back through the same callbacks, and the host serializes them.
6. **Memory and compile cost:** About one pointer-sized cell per distinct target. Compile time is unchanged.
7. **AOT:** This interface is the AOT design.
8. **JS applicability and limits:** Otter already gives every materialized address a semantic name: `RelocationTarget` in `crates/otter-jit/src/artifact/relocation.rs` (`RuntimeStub`, `GcCageBase`, `GlobalLexicalCell`, `StringConstantCell`, `PropertySourceCell`, ...). Installed code still bakes these as MOVZ/MOVK immediates. The limit is that far fewer JS facts are compile-time constants.
9. **Otter experiment:** Put a single `resolve(RelocationTarget) -> Imm | Cell(slot)` query in front of every Machine-tier address materialization. Run fib, ts and crypto with every target in Cell mode. A slowdown under 3% validates cell-based AOT code. Above that, use an ADRP+LDR per-image layout instead.

## 5. R2R lazy binding, fixup-validated entry, version bubbles

1. **Source:**
   - `docs/design/coreclr/botr/readytorun-format.md`: `READYTORUN_IMPORT_SECTION`, `ReadyToRunImportSectionFlags::Eager`, `READYTORUN_FIXUP_*`, `READYTORUN_HELPER_DelayLoad_MethodCall`/`_Helper`, and GC ref maps in `AuxiliaryData`.
   - `vm/prestub.cpp`: `ExternalMethodFixupWorker`, `DynamicHelperWorker`, `DynamicHelperFixup`. `ExternalMethodFixupWorker` patches the cell with `*(TADDR *)pIndirection = pCode`. For direct calls it does this through `PatchNonVirtualExternalMethod`; on the virtual-stub path it stores directly.
   - `vm/readytoruninfo.cpp`: `ReadyToRunInfo::GetEntryPoint` → `FixupDelayList` → `SetReadyToRunRejectedPrecompiledCode()`.
   - `jitinterface.cpp`: `LoadDynamicInfoEntry` handles `Check_TypeLayout`, `Check_FieldOffset` and `Check_InstructionSetSupport`. crossgen2 uses `RequiresRuntimeJitException`.
   - Version bubbles: `readytorun-overview.md`, `EncodeFieldBaseOffset`/`IsLayoutFixedInCurrentVersionBubble`, and `--inputbubble`, `--composite`, `--opt-cross-module` with `Check_IL_Body`.
2. **Work eliminated:** There is no eager relocation pass. Each external reference is resolved once, lazily, and then costs one load. Code pages stay clean and shared; only data cells are written.
3. **Hot/cold path:** The hot path is `call [cell]`. On the cold path, a delay-load thunk gets the cell address in a scratch register; the worker decodes the signature, patches the cell, then jumps to the target.
4. **Invariants:**
   - Patching a cell is an idempotent single-word store.
   - A method's fixup list (its checks) runs before its code is first published. A failed `Check_*` fixup returns FALSE, and the method goes to the JIT instead. `Verify_*` fixups fail hard through `EEPOLICY_HANDLE_FATAL_ERROR_WITH_MESSAGE`.
   - Inside a version bubble, offsets and inlining are constants. Across bubbles, code loads a base-size cell (`SIZE_OF_BASECLASS`), and cross-bubble inlining is off by default.
5. **GC/exception/deopt:** A delay-load thunk is a GC point with arguments still in flight, so each cell carries a GcRefMap (`GCRefMapDecoder`).
6. **Memory and compile cost:** One cell plus one signature blob per unique target. Startup pays only for the code that actually runs.
7. **AOT:** This is how an image stays portable and needs few relocations, while still being able to fall back to the JIT.
8. **JS applicability and limits:** Suits runtime stubs, atoms/strings, global lexical cells, module bindings and builtin entry points. The Otter binary plus its builtins snapshot is the natural version bubble. The limit is that JS code can mutate builtins and prototypes. Cells must therefore hold identities, and any baked value needs protector cells plus deopt, which .NET never needs.
9. **Otter experiment:** Build an AOT template-code cache with lazily bound cells for `RuntimeStub`, `StringConstantCell` and `GlobalLexicalCell`. Give each function a check list: Otter version, snapshot hash, CPU features. Measure `ts` startup and cold `node_modules` loads against today's bytecode cache. Refute if template compilation is under 5% of startup.

## 6. Pure helper calls, expanded late (runtime lookups, static init)

1. **Source:**
   - `corinfo.h`: `CORINFO_RUNTIME_LOOKUP` (`indirections`, `offsets[CORINFO_MAXINDIRECTIONS]`, `testForNull`, `helper`) and `CORINFO_LOOKUP_THISOBJ`/`METHODPARAM`/`CLASSPARAM`.
   - `jit/utils.cpp`: `HelperCallProperties::init` marks `CORINFO_HELP_RUNTIMEHANDLE_METHOD`, `CORINFO_HELP_RUNTIMEHANDLE_CLASS` and `CORINFO_HELP_READYTORUN_GENERIC_HANDLE` as `isPure`, `noThrow` and `nonNullReturn`.
   - `jit/helperexpansion.cpp`: `fgExpandRuntimeLookups`, `fgExpandStaticInit` ("Always expand for NativeAOT") and `fgExpandThreadLocalAccess` (which also always expands for NativeAOT).
   - NativeAOT: `EmitDictionaryLookup` in `Target_ARM64/ARM64ReadyToRunGenericHelperNode.cs` emits one `LDR` at a slot taken from `GenericDictionaryLayout(...).TryGetSlotForEntry` (not re-checked).
2. **Work eliminated:** During optimization a lookup is one pure call, so CSE and LICM deduplicate and hoist it. After loop optimization it becomes a load chain, a null test, and a helper call in a cold block.
3. **Hot/cold path:** The hot path is up to 4 dependent loads plus a branch. The cold helper fills the slot.
4. **Invariants:** Once filled, a slot never changes. The lookup context comes from `this`'s MethodTable or from a hidden argument.
5. **GC/exception/deopt:** Only the cold call is a GC point.
6. **Memory and compile cost:** The dictionary slots.
7. **AOT:** The JIT and R2R fill dictionaries lazily. NativeAOT precomputes every slot, so a lookup is one load with no check.
8. **JS applicability and limits:** Maps to declarative and global-lexical cells, module bindings, realm intrinsics and context-chain walks. Only bindings that never change after initialization qualify; `var` globals, `with` and deletable properties do not.
9. **Otter experiment:** In Machine HIR, model cell and context lookups as pure `LookupCell` nodes carrying an "initialized" fact. Let GVN/LICM hoist them, then expand them during lowering into a load plus a TDZ check. Count lookups executed per loop iteration on ts and earley, before and after.

## 7. Tiering with AOT code as tier 0; speculation without deopt

1. **Source:**
   - `docs/design/features/tiered-compilation.md` ("the precompiled code is the tier0 version").
   - `src/coreclr/inc/clrconfigvalues.h`: `TC_CallCountThreshold` 30 and `TC_CallCountingDelayMs` 100 in release builds. `_DEBUG` builds use 2 and 1.
   - `docs/design/features/DynamicPgo-InstrumentedTiers.md`: R2R → Tier1Instrumented → Tier1; "only 10-20% of methods make it to Tier1".
   - `docs/design/features/OnStackReplacement.md`: patchpoints and `CORINFO_HELP_PATCHPOINT`; "the OSR method frame incorporates the (live portion of the) original method frame". Deopt appears only as future work (§6).
2. **Work eliminated:** No JIT work for cold code. Only hot methods get an instrumented version. OSR does not rebuild the frame.
3. **Hot/cold path:** Guarded devirtualization is `cmp [obj],MT; jne fallback_call`, with the fallback kept inside the method, so a wrong guess is just a slower branch.
4. **Invariants:** `CodeVersionManager` switches between code versions. Calls are counted outside the method body (prestub/call counting, `vm/callcounting.cpp`). Patchpoints exist only in Tier0 code, and final Tier1 code carries no counters.
5. **GC/exception/deopt:** There is no deopt, so correctness never depends on invalidating code.
6. **Memory and compile cost:** The instrumented tier temporarily roughly doubles Tier1 code. The doc's example service has 8.22 MB of Tier1 code, so the instrumented tier adds about 13 MB, against a 10 GB working set.
7. **AOT:** AOT code only has to be a correct, reasonably fast tier 0.
8. **JS applicability and limits:** AOT JS code with inline caches profiles itself. But optimized JS code must speculate on shapes and number kinds. A .NET-style in-place fallback suits AOT code, not the top JIT tier.
9. **Otter experiment:** Measure what share of functions reaches the Machine tier on ts, earley and crypto. If it is near 10-20%, design AOT output as template-quality code with inline caches and in-place fallbacks, and JIT only the hot code. Also prototype an OSR that reuses the frame for Template→Machine, and compare transition cost.

## 8. Static PGO in the image, one schema for JIT and AOT

(The citations in this card were not re-checked in the verification pass.)

1. **Source:**
   - `corjit.h`: `allocPgoInstrumentationBySchema`, `getPgoInstrumentationResults`, and `enum class PgoSource` {`Static` ("embedded R2R profile data"), `Dynamic`, `Blend`, `Text`, `IBC`, `Sampling`, `Synthesis`}.
   - `readytorun-format.md`: `PgoInstrumentationData` (v5.2+, encoding marked TODO) and `HotColdMap` (v8.0+).
   - `tools/aot/crossgen2/Crossgen2RootCommand.cs`: `--mibc`, `--embed-pgo-data`.
   - `DynamicPgo.md`: methods are identified by "token, hash, and il size".
2. **Work eliminated:** One consumer handles counts and class histograms from any source. AOT code therefore gets guarded devirtualization, block layout and inlining decisions without a warm-up.
3. **Hot/cold path:** No run-time cost.
4. **Invariants:** A stale profile can only affect speed, never correctness, because the fallbacks remain.
5. **GC/exception/deopt:** None.
6. **Memory and compile cost:** One profile blob per image.
7. **AOT:** Enables profile-guided AOT with no JIT present.
8. **JS applicability and limits:** Otter feedback (shapes, call targets, number kinds) must be stored structurally, such as property-name sequences and builtin names, never addresses. In JS a profile mismatch needs guards or deopt.
9. **Otter experiment:** Serialize ts feedback keyed by source hash, function id and pc, and seed the next run's Machine compiles with it. Measure time to steady state and deopt counts. Refute if deopts rise or warm-up is under 10% of run time.

## 9. Frozen objects and build-time preinitialization

1. **Source:**
   - `vm/frozenobjectheap.h/.cpp`: `FrozenObjectHeapManager::TryAllocateObject`. The header comment says the JIT can bake in the object's address directly ("actual string object") instead of loading through a pinned handle (`mov rax,[rax]`).
   - `CORJIT_FLAG_FROZEN_ALLOC_ALLOWED` and `READYTORUN_HELPER_NewMaybeFrozenObject`.
   - NativeAOT (not re-checked): `ILCompiler.Compiler/Compiler/TypePreinit.cs` ("computes the initial state of static fields ... by interpreting the static constructor"; `TryScanMethod` threads an instruction counter), `PreinitializationManager.cs`, and `DependencyAnalysis/FrozenObjectNode.cs`/`FrozenStringNode.cs`.
   - At startup (not re-checked), `nativeaot/Common/src/Internal/Runtime/CompilerHelpers/StartupCodeHelpers.cs` runs:
     - `InitializeStatics`, which copies the preinit blob with `RhBulkMoveWithWriteBarrier`;
     - `InitializeModuleFrozenObjectSegment` → `RhRegisterFrozenSegment`;
     - `RehydrateData`.
2. **Work eliminated:** Running static initializers, handle indirection, GC moving and marking of immortal data, and loads of `static readonly` values (the JIT folds them via `getStaticFieldContent`/`isObjectImmutable`).
3. **Hot/cold path:** The hot path is an immediate object address or a folded value. The cold path is one registration per segment.
4. **Invariants:**
   - Frozen objects never move or die.
   - A folded reference can be embedded directly only if the object is frozen. `CEEInfo::getStaticObjRefContent` returns the object (as a JIT object handle) only if `ignoreMovableObjects` is false or `IsInFrozenSegment` is true. With the flag set, a movable object makes the call return false, and the load is not folded.
   - The preinit interpreter bails on anything it cannot model, and the runtime static constructor runs instead.
5. **GC/exception/deopt:** The segment is registered with the GC.
6. **Memory and compile cost:** Image pages stay clean and shareable.
7. **AOT:** Needs relocatable data. NativeAOT uses relative pointers (`MethodTable.SupportsRelativePointers`).
8. **JS applicability and limits:** Builtin functions and prototypes are mutable in JS, so only strings/atoms, shapes, bytecode and constant pools qualify.
9. **Otter experiment:** Classify the bootstrap snapshot heap by immutability. Move the immutable bytes into a read-only segment that is never moved or scanned. Measure major-GC mark time, zlib RSS and startup. Refute if the immutable share is under 20%.

## 10. Funclet EH with a table-driven two-pass dispatcher

(The `ExceptionHandling.cs` and `exceptionhandling.cpp` citations were not re-checked in the verification pass.)

1. **Source:**
   - `botr/clr-abi.md`: "Funclets", "Cloned finallys", "How EH affects GC info/reporting", "Filter GC semantics".
   - `nativeaot/Runtime.Base/src/System/Runtime/ExceptionHandling.cs`: `RhThrowEx`, `DispatchEx`, `FindFirstPassHandler`, `InvokeSecondPass`, `InternalCalls.RhpCallCatchFunclet`/`RhpCallFilterFunclet`.
   - CoreCLR's `vm/exceptionhandling.cpp` calls the same entry point (`METHOD__EH__RH_THROW_EX`).
2. **Work eliminated:** A `try` costs nothing on the normal path: no handler push or pop and no try-state. A `finally` on the normal path is cloned inline.
3. **Hot/cold path:** The hot path has no EH instructions. On the cold path, pass 1 walks unwind info and clause tables to find the handler. Pass 2 runs the finally/fault funclets, then the catch funclet, which returns the resume address.
4. **Invariants:**
   - A funclet shares its parent's frame register.
   - Non-volatile registers are not carried back from funclets, so state shared with the method body lives on the stack.
   - Clauses are ordered inner to outer.
   - The leaf funclet reports shared locals (`WantsReportOnlyLeaf`), and slots that are live in a filter are pinned.
5. **GC/exception/deopt:** One GC-info blob covers the parent and all its funclets.
6. **Memory and compile cost:** EH and unwind tables.
7. **AOT:** The tables are position-independent. R2R also has `READYTORUN_HELPER_PersonalityRoutine`.
8. **JS applicability and limits:** Maps one-to-one onto try/catch/finally. JS has no filters, so a one-pass unwinder is enough. The limit is that JS code throws often (iterator close, parse errors), which makes the unwinder itself hot.
9. **Otter experiment:** Microbenchmark `try{f(i)}finally{s++}` in a loop on the Template and Machine tiers and count instructions against an empty loop. If there is any overhead, move try regions into side tables with out-of-line handlers and re-measure.

---

## What .NET does not spend that Otter does

- **Closures:** Otter allocates one heap cell per captured mutable binding, plus a ref per capture in each closure. .NET allocates one environment per scope, merged where safe (in Release builds) or placed in a stack struct.
- **Allocation:** Otter uses Rust-ABI calls and old-space placement for short-lived closures and cells. .NET uses a gen0 bump of 11 instructions for small, non-finalizable types.
- **Loop polls:** Otter does back-edge work on every iteration: 9 instructions including a store in the Template tier, and a 2-instruction batch counter plus a reserved register in the Machine numeric tier. .NET pays nothing, using hijack/redirect and fully interruptible code.
- **Top-tier counters:** Otter keeps a counter in top-tier loops (the Machine batch fuel). .NET counts calls outside the method body, and only Tier0 code has patchpoints.
- **Deopt for AOT code:** Otter needs deopt metadata and frame reconstruction. .NET keeps the guarded-devirtualization fallback inline and never deopts.
- **Baked addresses:** Otter bakes process addresses as immediates into installed code. In .NET the host supplies addresses, and the same compiler emits cells for AOT.
- **Two backends:** Otter has two code generators (the Template and Machine emitters). .NET uses one RyuJIT for JIT, R2R and NativeAOT.
- **Immortal startup data:** Otter's GC spends work tracing immutable startup objects. .NET puts them in frozen segments.

---

Downloaded sources are in `/private/tmp/claude-501/-Users-alexanderstreltsov-work-octofhir-otter/a2e9abdb-0d8f-437a-8cf1-f2cad155594e/scratchpad/clr/`.