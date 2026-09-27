# Machine optimizing tier (Otter JIT): IR pipeline, hot paths and what to collapse

Path abbreviations: **J** = `crates/otter-jit/src`, **M** = `J/machine`, **N** = `M/numeric`, **V** = `crates/otter-vm/src`.

## 1. Components and key files

The only production entry point is `try_compile` (N/mod.rs:327-613), called from `compile_optimized` (J/optimizing/mod.rs:283). A function the pipeline cannot handle returns `Unsupported`, and the Template body stays installed as its baseline (optimizing/mod.rs:1-8).

| # | Stage / representation | Code |
|---|---|---|
| 0 | `JitCompileSnapshot`: bytecode plus about 12 baked feedback maps and layout constants. Built synchronously on the mutator thread. | V/interp/jit_compile.rs:466-535 (bake_* 481-497, hook call 527); fields V/jit.rs:273-467 |
| 1 | `InstructionSemantics`: each op classified as speculative or committed | N/semantics.rs:88-127, 134-251 |
| 2 | `RawBlock` CFG plus register liveness | N/hir.rs:851-860, 1829-1958, 2003-2150 |
| 3 | **Numeric HIR**: `NumericFunction` / `NumericNode` (85 kinds), SSA with block params, `NumericFrameState` | hir.rs:189-422, 799-822. Rebuilt whole for phi convergence up to 3×N times (869-896) |
| 3a | HIR passes: `boxed_arithmetic::relax`, `inlining::splice`, `partial_escape::optimize`, `truncation::optimize`, `plan_loop_entries` | hir.rs:883; N/mod.rs:341-344 |
| 4 | **Machine IR**: `InstructionSequence`, `MachineInstruction`, `MachineOpcode` (118 variants), `CallDescriptor`, `MachineFrameState`. Selection also expands the property, element, native-call and committed-probe CFGs. | M/mod.rs:799-1456; N/mod.rs:615ff, 674-830, 3005, 3414 |
| 4a | Machine passes: trivial_phi → block_merge → GVN → LICM → GVN → DCE | M/mod.rs:1808-1831 |
| 5 | regalloc2 **Ion** → `AllocatedSequence` (locations, edits, metadata) | M/regalloc.rs:308-322 |
| 6 | Metadata: `MachineSafepointTable`→`SafepointRecord`; `lower_deopt_table`→`DeoptTable`+`DeoptExitDescriptor`→`DeoptRuntime`; `MachineFrameLayout` | N/mod.rs:389-474; M/deopt.rs:91ff; M/frame.rs |
| 7 | dynasm emission. AArch64 N/arm64.rs (5.9k lines), x86-64 N/x86_64.rs (5k lines). Emission is re-run with veneer islands when a branch relocation fails. | arm64.rs:1004-1117, 1072 |
| 8 | `OptimizedCode` → registry `CodeEntryCell` | optimizing/mod.rs:73-127; jit_compile.rs:555-570 |

**How many IRs.** Six main ones: snapshot/bytecode, RawBlock CFG, Numeric HIR, Machine IR, regalloc2 `Function`, and post-allocation metadata. Three sub-IRs are embedded in them:
- VM CacheIR programs (`MachineCacheIrSite`, lowered at N/mod.rs:3414);
- `JitElementAccess` layout programs;
- the generic `DeoptFrame<Slot>` schema.

That makes about 9 lowering transitions plus about 11 rewrite passes.

**VM side.**
- Cost model (V/tier_policy.rs:153-178): optimizing compile is estimated at 15 µs + 1.75 µs per bytecode instruction, and 320 B + 38 B per instruction of code.
- Code cap is 64 MiB (tier_policy.rs:36).
- Registry, entry cells and dependency epochs: V/jit_registry.rs:1-45.
- OSR (V/interp/jit_call.rs:428-520) and bail policy (jit_call.rs:758-830).

**Opcode coverage.** Supported ops are:
- the explicit arms of `lower_instruction` (hir.rs:2760-4183);
- the binding family (hir.rs:2501);
- the committed family (semantics.rs:88-127).

**77 of 192 opcodes have no lowering, and the first one found declines the whole function** (hir.rs:4180-4183). Structural declines: async/generator bodies, and `finally` or catch-less regions (hir.rs:947-963).

Notable declines, with where the compiler emits them:
- `TailCall`: every strict-mode `return f(x)` (otter-compiler statements.rs:851-856).
- `FreshUpvalue`: per-iteration `let` capture (statements.rs:1346-1353).
- `JumpIfNullish`: `??`.
- `GetIterator` / `IteratorNext`: for-of.
- `ForInKeys`, `CollectRest`, `ArrayPush`, `MathLoad`, `DefineDataProperty`, `CopyDataProperties`, `DeleteProperty`, `MakeClass`, `TdzError`, `SetFunctionName`, `GetTemplateObject`.

The parity report independently confirms that null-prototype literals fall out of Machine on `DefineDataProperty` (benchmarks/measurements/2026-09-27-parity-checkpoint.md, null-prototype section).

## 2. Data layouts

**Value encoding.** NaN-boxed. Int32 carries a tag in the high 16 bits (`NUMBER_TAG_HI16`); doubles are offset by `DOUBLE_OFFSET`; a cell is a 32-bit cage offset in the low word, decoded as cage_base + w (arm64.rs:4825-4863; J/template/arm64/ic_probe.rs:234-247).

**Runtime structures.**
- `NativeFrame`, 72 B (V/native_abi/frame.rs:194-205, size assert :407): {fid u32, pc u32, reg_count u16, kind, flags}, register_base, upvalue_base, this, new_target, self, upvalue_count, eval_env, argument_count, arguments_object.
- `CodeEntryCell`, 88 B (V/native_abi/code_entry.rs:94-110, :286): entry address, code-object id, flags, stack bytes, active count, frame-header template, tiering break-even/enabled, entry/deopt/throw counters.
- `JitMachineRootRecord`, 32 B (V/jit.rs:1184-1194): previous, root_base, code_object_id, count, safepoint_id. Pushed on the stack at each safepoint call.
- `PropertySourceCell`: `Option<(u32,u32)>` (J/entry/runtime_ops/vm_ops.rs:34-46).

**Machine frame** (arm64.rs:4925-4962; M/frame.rs:1-20), in order:
- saved x29/x30;
- x19 plus the used pairs of x20..x28;
- used d8..d15;
- 8 B spill slots;
- 8 B tagged root-save slots;
- untraced raw packet words;
- inline-frame words.

Register roles: x19 = `JitCtx`, x17 = interpreter register base at entry. **x29 is used as the backedge poll counter; `mov x29, sp` is never emitted**, so there is no frame-pointer chain.

**Direct-call linkage frame** (≤4080 B, J/arm64/direct_call.rs:111): a copy of the NativeFrame header, an upvalue area, and a full tagged callee register window.

**Deopt metadata.**
- `DeoptSlot` = {`DeoptLocation`, `DeoptRepr` u8}, estimated at about 24 B (not measured) (V/deopt.rs:189-230, 272-313).
- Every lowered frame is register_count slots wide (M/deopt.rs:11-13; hir.rs:4210-4239).
- The exit register dump is 46 words = 368 B (M/target.rs:26-27; arm64.rs:4247-4280).

**IR records.**
- `MachineOperand` = {u32 value, constraint, role, timing, purpose}, estimated at about 12 B (M/mod.rs:249-260).
- `MachineInstruction` owns `Vec` operands, a `Vec` clobber list, boxed exits and boxed inline frames (mod.rs:1349-1368). The clobber list is cloned into every instruction; the AArch64 call clobber set is 23 registers (target.rs:307-311).

## 3. Hot paths (AArch64)

Instruction counts below come from reading the code, not from measurement.

**(a) Machine→Machine call** (`emit_direct_call_with_access`, direct_call.rs:717-1644)
1. Activation-depth check: 5 instructions (≈771-777).
2. Callee identity (≈818-1007): load; up to 4 movz/movk for `box_function_id`; cell test; type tag; flags test; function-id compare; load upvalue base/count/eval env. About 25 instructions, plus 5-15 for `this` binding.
3. Inherited-upvalue count check: 3 (1008-1014).
4. Reserve frame; about 8 header stores; one load and one store per argument (1026-1085).
5. Entry cell: 4 movz/movk + add + `ldar` + cbnz (1087-1101). Stack and generation checks: about 14.
6. **Fill every non-argument callee register with `undefined`** (≈1150-1172). Locals are skipped only when the callee has the parameter-prefix flag.
7. **If the callee has its own upvalues, a Rust call to `STUB_JIT_INITIALIZE_UPVALUES` on every call** (1470-1520). This matches the Earley profile rows `alloc_upvalue_with_roots` (94) and generated upvalue init (54).
8. Publish the activation: about 17 (1567-1586). Tiering counter and break-even test in the caller: 7-15 (J/arm64/direct_call/tiering.rs:19-37). Then `blr`.
9. Callee prologue: 8-12 stores. Parameters are re-read with `ldr [x17,#8i]` (arm64.rs:1282-1288).
10. Completion: a status compare chain (direct_call/completion.rs:66-82), then a reload of every live tagged root.

**Total estimate: about 100-150 instructions, plus one store per callee register, plus two memory operations per live tagged value** (my estimate, not measured). V8 does this in about 15-25 (also an estimate). This is consistent with fib at 4.0×.

**(b) Monomorphic named load** (`PropertyShapeLoad` → `PropertyShapeProof` + `GuardCondition` + `PropertySlotLoad`, arm64.rs:2505-2561)
- Proof: cell test, cage base (1-4 instrs), add, type-byte compare, mode/opaque/attrs byte guards (6), shape compare (5), boolean materialization (3), `cbz` exit.
- Slot load decodes the receiver again: cage base (1-4), add, slab-base branch (4, J/template/arm64/values.rs:329-357), `ldr`.
- **About 35-40 instructions, against about 4 in V8.**
- Sites that are not speculated use the CacheIR probe chain plus a committed cold call to `STUB_JIT_LOAD_PROPERTY`.

**(c) Upvalue read** (binding guard/hit, arm64.rs:720-1001)
- ldr NativeFrame; ldr upvalue_base; ldr cell offset; cage base (≤4); 2 adds; TDZ check (ldr, hole constant ≤4, cmp, b.eq); materialize condition (4); `cbz`; hit ldr + store.
- **About 20 instructions.** Writes also run `BindingWriteBarrier`.

**(d) Committed runtime operation** (e.g. `MakeClosure`: semantics.rs:123 → `CallTarget::CommittedRuntime`; arm64.rs:565-703)
- Generated side: store R live roots; push the root record (about 15); load arguments; store `logical_pc` into the NativeFrame; `blr`; pop (4); **reload all R roots unconditionally**; three-way `BranchNativeStatus`.
- Rust side: `jit_scalar_value_stub` (J/entry/runtime_ops/reentry.rs:1026) → `runtime_call()` → **`published_opcode()` decodes the bytecode again** → `ScalarValueOp::from_opcode` → `make_closure_value` (V/runtime_activation/committed_values.rs:511-533).
- About 30 + 2R generated instructions plus the Rust dispatch. Earley `make_closure_value` shows 127+36 samples.

**(e) Scalar operations.** Int32 add is `adds; b.vs` (arm64.rs:1704-1722). Boxing a Number is about 17 instructions (4865-4894); decoding is about 8 (4825-4849).

**(f) Backedge poll** (arm64.rs:236-284). `subs w29; b.ne` per iteration. Every 16th iteration it loads the interrupt byte and the fuel cell (about 10 instructions), and only rarely calls the Rust poll.

**(g) Deopt exit.**
- Per site: `movz w17,#i; b shared`. The shared handler does 23 `stp` and calls `jit_deopt_writeback_stub` (arm64.rs:4232-4304; reentry.rs:220).
- The stub walks `DeoptRuntime`, writes the interpreter window and returns `Bail(pc)`. **Execution resumes in the interpreter.**
- Inlined chains are constructed and run to completion by the interpreter (reentry.rs:200-215).
- If the deopt happens in a callee entered through generated linkage, `STUB_JIT_DEOPT_STACK_CALL` resumes it (direct_call.rs:39-41).

**(h) OSR.**
- Path: interpreter back-edge → `maybe_osr` → synchronous whole-body compile (jit_call.rs:428-520; jit_compile.rs:583-637).
- The per-header trampoline runs the full prologue and decodes each live input from the window (arm64.rs:4306-4330).
- If Machine declines, Template OSR is the fallback.

**Interpreter and Template.** Template shares the same direct-call linkage and the `ic_probe`/`values` helpers (arm64.rs:139-147), so (a) and (b) have the same shape there. The interpreter is the only tier deopts land in.

**Compile cost and code size.**
- Measured: Earley, 159 compiles / 1.66 MB / 86 ms (benchmarks/measurements/2026-09-27-arguments-reads.md:146-148). zlib, **57 compiles / 15.26 MB / about 3.88 s** (…/2026-09-27-indexed-access.md:152-156). The tier split of those numbers was not established.
- Superlinear points in the code:
  - HIR is fully rebuilt on each phi retry (hir.rs:875-895).
  - `function.clone()` plus a callee HIR build per inline candidate (inlining.rs:130, 201).
  - Frame states are O(ops×registers) (hir.rs:4210-4239).
  - `resume_pc` does a linear scan per exit frame (N/frame_state.rs:70-76, called at N/mod.rs:443-450).
  - GC root liveness is recomputed after each pass.
  - Emission runs twice for functions over 1 MB (arm64.rs:1072).
- Code size drivers: inline call linkage (a) and completion, 30+2R per committed operation, and inline cold CFG blocks.

## 4. Invariants the tier relies on

- **Moving GC.** Tagged SSA values live in registers or spill slots. Every allocating or re-entrant call saves them to the root area, publishes the `JitMachineRootRecord` chain, and reloads everything afterwards (arm64.rs:4482-4624; N/mod.rs:79-81). Roots are derived from CFG liveness plus frame states and checked by a verifier (M/mod.rs:1777-1797).
- **No raw pointers across safepoints.** Owner/storage addresses, element addresses and property owners never cross a safepoint, backedge or re-entry (M/mod.rs:61-65; N/mod.rs:48-54; M/gvn.rs:17-18).
- **Leaf calls are unrooted.** Leaf probes and the backedge poll declare no safepoint (M/native_leaf.rs:13-22; M/effects.rs:411-414). This relies on leaves never collecting.
- **Deopt discipline.** Deopt happens only before effects, at an exact PC. Committed operations never deopt or replay (hir.rs:23-27, 50-57).
- **Metadata operands are late uses**, so deopt/root values cannot sit in a clobbered register (M/mod.rs:34-36).
- **Code-owned baked addresses.** `DeoptRuntime` and the IC source cells are boxed for the code's lifetime (optimizing/mod.rs:81-92).
- **Stable entry cells.** Function entry cells are stable and callers are never invalidated (direct_call.rs:44-51; V/jit_registry.rs:27-41).
- **Single-threaded isolate.** Compilation runs synchronously on the mutator.

## 5. Duplication and collapsible transitions

1. **`DeoptFrame<Slot>` exists in five forms:** `NumericFrameSlot` (frame_state.rs:24-35), `MachineFrameSlot` (M/deopt.rs:35-53), `Option<MachineValue>` inline frames (M/mod.rs:1365), `DeoptSlot` (runtime), and `Option<u16>` in `SafepointRecord` (V/native_abi/safepoints.rs:171-184). On top of that, per-instruction frame-state operand lists (N/mod.rs:4263-4284). All of them are full register windows.
2. **Unused safepoint map.** V/deopt.rs:710-800 defines `StackMap` / `SafepointTable` keyed by byte PC; a search found no users outside that file. `SafepointRecord` is the one actually used.
3. **Two or three lowerings per operation family:**
   - properties: `PropertyShapeLoad` / CacheIR probe with cold call / polymorphic / megamorphic;
   - bindings: `BindingGuardedRead` / guard-hit-cold-join;
   - elements: guarded / committed / `ElementUnseenExit`;
   - arithmetic: numeric / `BinaryNumberProbe` with committed call (semantics.rs:158-186; hir.rs:2708-2753).
4. **The committed boundary re-interprets bytecode.** It recovers the operation from bytecode at run time (committed_values.rs:511-517), although `CommittedValueOperation` is known at compile time (hir.rs:256-262). `PropertySourceCell` likewise re-derives (fid, pc).
5. **Three frame formats:** interpreter window, NativeFrame plus stack window, and Machine spill/root frame. Calls box SSA values into a window that the callee then reloads. The root-save area duplicates values that are already in callee-saved registers.
6. **Two preheader mechanisms:** HIR `plan_loop_entries` (packed-double loops only, hir.rs:900-937) and LICM's own preheader split (M/licm.rs:51-110).
7. **Repeated analyses:** GVN runs twice; `complete_gc_root_liveness` runs five times (trivial_phi.rs:181, gvn.rs:627, licm.rs:108, dce.rs:148, mod.rs:1794).
8. **Parallel emitters:** AArch64 and x86-64 Machine emitters, plus Template's. The shared slab helper forces the fixed registers x13/x14 (values.rs:338).
9. **Cage base re-materialized at every use:** 64 emission sites load it with movz/movk instead of a pinned register.
10. **Stale documentation:**
    - hir.rs:84-86 says an attempted but unplanned plain call keeps the function on Template, but the code lowers a generic `CallWithThis` (hir.rs:3395-3441).
    - optimizing/mod.rs:86-87 describes the IC cells as "self-patched", but `PropertySourceCell` is immutable.
    - target.rs:407-411 disclaims x86 lowering, yet N/x86_64.rs exists.

## 6. What would have to change

### (a) For a much cheaper hot path
- **Calling convention:** pass arguments in registers and publish the NativeFrame lazily. Remove the undefined-fill, the caller-side tiering counters and the per-call activation-array stores.
- **Inlining limits:** admit callees with their own upvalues, and callees with residual calls (inlining.rs:163-183).
- **Captured bindings:** create cells in generated nursery code, or scalar-replace them, instead of the per-call Rust `INITIALIZE_UPVALUES`. Support `FreshUpvalue`.
- **`MakeClosure`:** inline bump allocation, and add closures to escape analysis. Today `VirtualObjectKind` has only `PlainObject` and `FixedArray` (V/deopt.rs:238-245).
- **Pointer handling:** pin the cage base in a register. Expose the decoded object pointer as a `Cell` value (M/mod.rs:153-154) so the shape proof and the slot load share it.
- **Roots:** use safepoint stack maps instead of saving and reloading every root around every call.
- **Opcode coverage:** add `TailCall` (as Call+Return), `JumpIfNullish`, for-of over arrays, `ArrayPush`, `MathLoad`, `DefineDataProperty`, `CollectRest`.
- **Deopt states:** store only live registers, and share states between exits.
- **Optimizer reach:** GVN across merges using memory SSA; LICM for loops that contain calls with narrow alias sets.

### (b) For a portable AOT artifact
Already relocated symbolically (J/artifact/relocation.rs:108-160): cage base, runtime stubs, `DeoptRuntime`, IC source cells, direct-call entry cells, global lexical cells, string constant cells, lookup tables.

Still raw immediates that would need symbolic relocations or a per-code constant pool validated at load:
- hidden-class shape tokens (ic_probe.rs:286; arm64.rs:917-920, 2835);
- atom ids (N/arm64/megamorphic_property.rs:92, 228, 390);
- global-lexical and dictionary epochs (arm64.rs:889);
- intrinsic prototype identity (arm64.rs:2623);
- boxed function ids (direct_call.rs:≈778-782);
- native bootstrap identities and receiver-allocation plans.

The deopt, safepoint and dependency tables and the feedback facts would also need to be serialized. The snapshot layout constants would effectively become a build ABI, which conflicts with the project's "no versioning" rule.

## 7. Open questions

1. **Possible GC soundness gap.** The backedge poll calls `jit_backedge_poll` → `promote_hot_generated_callee` → a full compile (V/interp/stats.rs:204-229; jit_call.rs:1143-1154). That compile's `prewarm_string_constant_cells` can allocate strings (jit_compile.rs:1693-1696), while the polling Machine frame declares no safepoint. I could not determine whether a collection is impossible there.
2. **zlib split.** How the 57 compiles / 15.26 MB / 3.88 s split between Template and Machine, and how much deopt-table memory (O(exit states × registers × about 24 B)) contributes to the 795 MB RSS — not measured.
3. **Decline rates.** How often Earley and TS hot functions decline on `TailCall`, `FreshUpvalue` or `JumpIfNullish` — this needs JIT event logs.
4. **Root reload.** Reloading after committed calls looks unconditional (arm64.rs:692-701), with no cheaper path when no GC ran; not confirmed at run time.
5. **x86-64.** The x86-64 backend has only been cross-compiled, never executed on this host.
6. **IR record sizes.** Exact `NumericNode` / `MachineInstruction` byte sizes were not measured.