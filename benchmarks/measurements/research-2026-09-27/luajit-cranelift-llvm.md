## Citation check: the 6 most load-bearing citations

I fetched every source at the pinned revision the report names: LuaJIT `c6ffc141…`, wasmtime tag `v49.0.1` and LLVM tag `llvmorg-23.1.2`. I read the code directly (raw files plus grep) rather than relying on summaries.

| # | Citation | Verdict | Note |
|---|---|---|---|
| 1 | `LJ/lj_record.c` `lj_record_ins`: `BC_UCLO` and `BC_FNEW` abort with `LJ_TRERR_NYIBC` (card 1: "LuaJIT never compiles closure creation") | **Verified** | Lines 2769–2772 are `case BC_UCLO: case BC_FNEW: setintV(&J->errinfo, op); lj_trace_err_info(J, LJ_TRERR_NYIBC);`. The `default:` case falls through into them. This is inside `lj_record_ins` (line 2227). The card 2 helpers also exist (`rec_upvalue_constify` at 1760, `rec_upvalue` at 1785, the "Aliases an SSA slot?" comment at 1818, `PROTO_CLC_POLY` at 802 and 1795). |
| 2 | `LJ/lj_gc.c` `lj_gc_step_jit` and `LJ/lj_asm_arm64.h` `asm_gc_check`: "Exit trace if in GCSatomic or GCSfinalize. Avoids syncing GC objects" (card 3) | **Verified, with a precision added** | `lj_gc_step_jit` (lj_gc.c:768) returns `state == GCSatomic \|\| state == GCSfinalize` under the comment "Return 1 to force a trace exit". The quoted comment is in `asm_gc_check` (lj_asm_arm64.h:1853). Two details the report left out: `gc_onestep` has its own guard, `if (tvref(g->jit_base)) /* Don't run atomic phase on trace. */`, and `asm_gc_check` calls `ra_evictset(as, RSET_SCRATCH)` before the call. |
| 3 | `WT/cranelift/codegen/src/ir/user_stack_maps.rs` and `frontend/src/frontend/safepoints.rs`: "every non-tail call is a safepoint", spill at the definition and reload at each use, entries of 8 to 128 bits (card 6 and the backend table) | **Verified, quote location clarified** | The quote is in the module docs of `ir/user_stack_maps.rs`: "Currently all non-tail call instructions are considered safepoints. (This does *not* allow… skipping safepoints for calls that are statically known not to trigger collections…)". `SafepointSpiller::rewrite` does three things: it spills definitions, adds stack-map entries at safepoints, and replaces uses with reloads. `SlotSize` covers 1, 2, 4, 8 and 16 bytes. `declare_value_needs_stack_map` and `declare_var_needs_stack_map` exist in `frontend.rs`. |
| 4 | `WT/cranelift/codegen/src/machinst/buffer.rs` `MachPatchableCallSite` and `isa/call_conv.rs` `PreserveAll` (card 6 ICs and the backend table) | **Verified, with a nuance** | The record has two fields: `ret_addr` and `len`. `len` is documented as "The length of the region to be patched by NOP bytes." `PreserveAll`: "It does not support tail-calls. It also does not support return values." Cranelift only describes how to NOP the call out (the embedder can put the original bytes back). It offers nothing for retargeting a call or rewriting embedded immediates. "Can only be turned into no-ops" should be read as "Cranelift only supports toggling the call off". |
| 5 | `WT/crates/wasmtime/src/runtime/code_memory.rs`: relocations are rejected outside `.debug_*` (card 7, the backend table and the suggested direction) | **Verified** | Lines 178–194: "Check that we don't have any relocations, which would make loading precompiled Wasm modules slower and also force them to get paged into memory from disk… We avoid using things like Cranelift's `floor`, `ceil`… This also ensures that all builtins use the same trampoline mechanism", followed by `ensure!(target_name.starts_with(".debug_"))`. The section constants in `environ/src/obj.rs` are `.wasmtime.stackmap`, `.wasmtime.traps`, `.wasmtime.exceptions`, `.wasmtime.frames` and `.wasmtime.addrmap`, and `ELFOSABI_WASMTIME` is `OsAbi(200)`. |
| 6 | `LLVM/…/StatepointLowering.cpp` option defaults and `LLVM/llvm/docs/Statepoints.rst` quotes and platform list (card 8 and the backend table) | **Verified; one related claim corrected** | `use-registers-for-deopt-values` is `cl::Hidden, cl::init(false)`. `max-registers-for-gc-values` is `cl::Hidden, cl::init(0)`. Statepoints.rst: "known to be somewhat poor… hot-statepoints are almost always inliner bugs", "large amount of IR… higher than expected memory usage and compile times", "Today, only Aarch64 and X86_64 are supported", and the non-integral pointer and speculation hole at lines 118–126. **Corrected:** the stack-map section is not called `__LLVM_StackMaps`. Per `StackMaps.rst` it is `.llvm_stackmaps` on ELF and `__llvm_stackmaps` on Mach-O, both in segment `__LLVM_STACKMAPS`. The platform difference is documented in `StackMaps.rst`, not in the B3 post. |

### Other spot checks (outside the 6)

| Citation | Verdict | Note |
|---|---|---|
| arXiv 2301.03982 Table 1 (HPCG) | Verified | 52 ms / 0.3769, 150 ms / 1.3240, 2811 ms / 1.5426 GFLOP/s. The ratios hold: 18.7× compile time and +16.5% speed. |
| Tarantool wiki (allocation sinking) | Verified | 26.9 s without sinking, 0.2 s with sinking, 0.2 s for C++ and 1.2 s for HotSpot. |
| cfallin regalloc2 post | Verified | "was and is dominated by regalloc time", about 20% faster compiles, and up to 10–20% faster code on register-pressure benchmarks. |
| Fitzgerald post (2024-09-10) | Verified | The post names three motivations: compressed 32-bit references, moving GC, and a mid-end bug caused by reloads the optimizer could not see. |
| B3 post (webkit.org/blog/5852) | Verified | "three quarters or more", 4.7×, JetStream +4.5%, Kraken +11.6% (MacBook Air) and +8.2% (Mac Pro), Octane +5.2%. IRC is about 1300 lines against about 5000 for LLVM's Greedy allocator. One fact was missing: on the Mac Pro, JetStream and Octane showed no difference. |
| Falcon keynote PDF | Verified; one characterization corrected | Confirmed: 4–6 developers and about 20 person-years, 3–5× larger code than C2, "not suitable for a first tier JIT", about 100 ms typical compiles, libLLVM about 40 MB, a custom pass order, and "normal object files". The correction concerns the "2+ person years", which the slide attributes to the gc.statepoint design ("Major goal: allow in register updates") under the heading "BE WARY OF OVER DESIGN". It was spent on keeping GC pointers in registers, not on building spill-and-reload. So "Falcon's biggest cost was supporting object relocation" is not supported, and the closing sentence of the report is wrong. |
| `snap_dedup` (card 1) | Corrected | `snap_dedup` is used by `lj_snap_replay`, which records side traces. De-duplication at exit time is an inline loop in `lj_snap_restore` ("De-duplicate sunk allocations."). |
| `RID_SINK` (card 1) | Refined | The sink pass tags instructions `RID_SINK`. The assembler retags sunk allocations as `RID_SUNK` (lj_asm.c:960), and restore checks for `RID_SUNK`. |
| `hotexit` (card 3) | Refined | The value 10 is defined in `lj_jit.h`. The comparison happens in `trace_hotside` (lj_trace.c:805), which `lj_trace_exit` reaches. |
| LuaJIT parameters, line counts and names | Verified | `maxrecord` 4000, `maxirconst` 500, `maxsnap` 500, `hotexit` 10. `lj_opt_sink.c` 258 lines, `lj_snap.c` 1034, `lj_asm.c` 2643. All `src/*.c`+`*.h` total 75,195 lines. The LICM, "soooo easy" and "linear backwards order" comments are present, as is the `sink_phidep` budget of 64. |
| Cranelift details | Verified | `InstructionData` size assertion of 16 bytes. `lower.isle` is 5200 lines on x64 and 3336 on aarch64. `inline.rs` describes "inlining as a library". `debug_tags.rs` carries tags through inlining. |
| Julia v1.12.0 | Verified | `gc.md` says "non-moving". `LateLowerGCFrame::{runOnFunction, ComputeLiveness, ColorRoots, PlaceGCFrameStores}` exist. |
| Otter facts | Partly checked | The four cited files exist. My count of all `.rs` lines under `crates/otter-jit` is 90,174 in today's working tree, against 89.9k in the report. I did not re-check the mechanisms. |
| Not fetched | Unverified | The Riptide post (7122), regalloc2 at `2fe490bc`, and `compare-llvm.md`. |

---

## Corrected report

# Research cards for Otter: LuaJIT 2.1, Cranelift/Wasmtime and LLVM as sources for the redesign

I read all the source files named below at the commits listed here. Everything else is web material, fetched and cited where it is used.
- **LuaJIT:** `github.com/LuaJIT/LuaJIT/tree/c6ffc141a8762b41703f9287d63d93622a13dd8f/src` (head of the v2.1 branch, 2026-09-08). Written as `LJ/` below.
- **Wasmtime and Cranelift:** `github.com/bytecodealliance/wasmtime/tree/v49.0.1` (commit 46c23a87). Written as `WT/` below.
- **LLVM:** `github.com/llvm/llvm-project/tree/llvmorg-23.1.2` (commit 2d567403). Written as `LLVM/` below.
- **Julia:** tag `v1.12.0`.

**Otter facts checked in the repo** (only the files' existence and the line count were re-checked for this revision):
- The optimizing (Machine) tier already uses regalloc2 (`crates/otter-jit/src/machine/regalloc.rs`).
- Before a call that can allocate, it copies GC roots to save slots and reloads them afterwards (`machine/safepoint.rs`).
- Its deopt frames record every interpreter register ("register-count wide", `machine/deopt.rs`).
- JIT code builds absolute addresses into instructions (MOVZ/MOVK on ARM64, `mov r64, imm64` on x86_64). `artifact/relocation.rs` only records them for diagnostics.
- `otter-jit` is about 90k lines (90.2k counting all `.rs` files in the current working tree). All of LuaJIT's `src/*.c` and `*.h` files together are 75.2k lines.

---

## 1. LuaJIT allocation and store sinking (targets earley, ast_ctor)
1. **Source:**
   - `LJ/lj_opt_sink.c`: `lj_opt_sink`, `sink_checkalloc`, `sink_mark_ins`, `sink_checkphi`, `sink_phidep`, `sink_mark_snap`, `sink_remark_phi`, `sink_sweep_ins`.
   - Unsinking is in `LJ/lj_snap.c`: `snap_unsink`, `snap_sunk_store`, and the de-duplication loop inside `lj_snap_restore`. The side-trace counterparts are `lj_snap_replay` and `snap_dedup`.
   - The design page on wiki.luajit.org is offline. A copy survives at github.com/tarantool/tarantool/wiki/LuaJIT-Allocation-Sinking-Optimization.
2. **Work eliminated:** the allocation, its initializing stores, and the GC pressure for objects that escape only on exit paths.
   - This is not escape analysis. An object that escapes only on a cold path is still removed from the fast path.
   - The wiki's point-class example drops from 26.9 s to 0.2 s, the same as C++ (HotSpot: 1.2 s).
3. **Hot path vs cold path:**
   - Hot: nothing runs. The allocations (TNEW/TDUP) and their stores are tagged `RID_SINK` by the sink pass, and the assembler retags the allocations as `RID_SUNK`. None of them emit code. Field values stay in registers or spill slots.
   - Cold: on a trace exit, `lj_snap_restore` calls `snap_unsink`. It allocates the object and replays the stores that sit between the allocation and the snapshot.
4. **Invariants** (what `sink_mark_ins` treats as disqualifying):
   - Only table and FFI-data allocations (TNEW/TDUP/CNEW/CNEWI) can be sunk.
   - Every store must use a constant key (`irref_isk(ir->op2)`), reached through HREFK/AREF/HREF/NEWREF/FREF.
   - These disqualify the object:
     - any load that was not forwarded away;
     - a metatable read (`IRFL_TAB_META`);
     - guards, arguments to calls, and stores with non-constant keys;
     - every value that gets stored.
   - Objects carried around a loop need a PHI with the same allocation opcode on both sides. Their stored values must be PHIs, or loop-invariant with no PHI dependency (`sink_phidep`, budget of 64 steps). `sink_remark_phi` repeats until nothing changes.
   - If the trace links to another trace, everything in its last snapshot is disqualified.
   - An object referenced from several slots is rebuilt only once. At exit, `lj_snap_restore` de-duplicates sunk allocations inline. `snap_dedup` does the same when `lj_snap_replay` replays a snapshot into a side trace.
5. **GC and deopt:** rebuilding allocates inside the exit handler.
   - That is safe in LuaJIT only because its GC never moves objects.
   - In Otter, the saved exit state must be a precise GC root and must be re-read after each rebuilt object.
   - Nested sunk objects must be rebuilt in dependency order, and the references can form cycles.
6. **Memory and compile cost:** three linear passes over the IR plus a PHI fixpoint. Marks live in the instruction's type byte during analysis, and the final tag lives in its register field, so there are no side tables. Stored values stay live up to every snapshot that can see the object, which adds register pressure.
7. **AOT:** each exit needs a serialized recipe for rebuilding virtual objects. Otter's `MachineFrameSlot` already has one.
8. **JS applicability and limits:**
   - A fresh Lua table has no metatable, so its stores cannot trigger hooks.
     - In JS, `this.x = v` in a constructor is an ordinary property set. Sinking it requires proof that no prototype has a setter or a read-only `x`.
     - Otter's shapes do not include the prototype, so that proof needs a separate prototype-validity guard.
     - Object literals define properties directly and are safe.
   - Proxies and getters only matter once the object escapes.
   - **LuaJIT never compiles closure creation.** `lj_record_ins` aborts on `BC_UCLO` and `BC_FNEW` with `LJ_TRERR_NYIBC`.
     - So LuaJIT is no evidence that closures or upvalue cells can be sunk.
     - Otter would have to extend the rules, treating a closure as an allocation whose cell references are stored at constant slots.
9. **Experiment:**
   - Make Otter's partial-escape pass log, per allocation site and weighted by execution count (earley, ast_ctor), the first reason each object gets materialized:
     - passed to a runtime call;
     - live at an exit;
     - loop PHI mixing different allocations;
     - non-constant key;
     - stored into an object that already escaped.
   - Also count how many of the 110.9M cell and 10.2M closure allocations run in Machine-tier code at all.
   - If fewer than half do, sinking cannot close the gap, and card 2 is the lever.

## 2. Lua upvalues: cells created lazily, pointing at stack slots; immutable upvalues become constants
1. **Source:**
   - `LJ/lj_func.c`: `func_finduv`, `lj_func_closeuv`, `lj_func_newL_gc`.
   - The flags `PROTO_UV_LOCAL` and `PROTO_UV_IMMUTABLE` are defined in `LJ/lj_obj.h` and set in `LJ/lj_parse.c`.
   - Trace side: `LJ/lj_record.c`, functions `rec_upvalue` and `rec_upvalue_constify`.
2. **Work eliminated:**
   - No cell exists until a closure is actually created (`func_finduv` runs at closure creation).
   - While the frame is alive, the variable stays a plain stack slot. The "open" cell only points at that slot (`setmref(uv->v, slot)`).
   - All closures over the same variable share that one open cell, found through the sorted `L->openupval` list.
   - When the scope exits, `lj_func_closeuv` copies the value into the cell.
   - Traces turn immutable upvalues into constants after a guard on the closure's identity.
   - An open upvalue of the current frame compiles to the stack slot's SSA value itself (the "Aliases an SSA slot?" branch).
3. **Hot path vs cold path:**
   - Hot: an interpreter read is three dependent loads (`fn->uvptr[i]`, then `uv->v`, then the value). That is the same as Otter.
   - Cold: closure creation walks the open list, and scope exit or return closes the cells.
4. **Invariants:**
   - The open list is kept sorted by stack address.
   - Reallocating the stack must fix up the open pointers.
   - Turning an upvalue into a constant needs the closure-identity guard. It is skipped when the prototype is marked polymorphic (`PROTO_CLC_POLY`).
5. **GC:** an open cell is a pointer into the middle of the stack. In Otter it would have to name a frame and register rather than an address, because frames move or get stored inside generator objects.
6. **Memory cost:**
   - One small object per captured variable, per scope instance that creates a closure.
   - That is the same order as Otter, but it is paid only on paths that actually create a closure.
7. **AOT:** this is purely a compiler/runtime convention, so it is fully portable.
8. **JS applicability and limits:**
   - JS's fresh binding per loop iteration matches Lua closing its upvalues at the end of each iteration.
   - JS-only hazards:
     - direct `eval` and `with`, where any binding may be captured;
     - sloppy-mode `arguments` that alias parameters;
     - temporal-dead-zone holes;
     - generator and async frames that outlive the call.
   - `const` gives JS the equivalent of Lua's "never reassigned" flag for free.
9. **Experiment:**
   - For each capture site in earley, count the cells where:
     - (a) the binding is never written after the closure is created, so the value could be copied into the closure and no cell is needed;
     - (b) no closure was created before the scope exited, so a lazy/open cell would have avoided the allocation.
   - Prototype (a) in the bytecode compiler, then measure allocation counts and fixed-work instructions on earley and fib.

## 3. LuaJIT snapshots, side exits, and GC only at exits
1. **Source:**
   - `LJ/lj_snap.c`: `lj_snap_add`, `snapshot_slots`, `snap_usedef`, `lj_snap_purge`, `lj_snap_shrink`, `lj_snap_regspmap`, `lj_snap_restore`.
   - `LJ/lj_jit.h`: `SnapEntry` (a `uint32_t`), the flags `SNAP_FRAME`, `SNAP_CONT`, `SNAP_NORESTORE`, and the parameter `hotexit` = 10.
   - `LJ/lj_trace.c`: `lj_trace_exit`, which calls `trace_hotside` to compare the exit's count against `hotexit`.
   - `LJ/lj_asm_arm64.h`: `lj_asm_patchexit`, `asm_gc_check`. `LJ/lj_gc.c`: `lj_gc_step_jit`, `gc_onestep`.
2. **Work eliminated:**
   - **Small snapshots.** A snapshot records only the slots the trace changed (`snapshot_slots` skips unmodified loads). Dead slots are dropped using a bytecode liveness analysis (`snap_usedef`). Adjacent snapshots merge when no guard lies between them.
   - **No separate deopt recipe.** An entry is just a slot number, flags and an IR reference. Where the value lives at exit comes from that IR instruction's own register-allocation fields.
   - **No recompilation.** After an exit is taken 10 times, LuaJIT compiles a side trace for it and patches the exit branch to jump there. Nothing is invalidated.
   - **No GC stack maps.**
     - Compiled code calls `lj_gc_step_jit`, after evicting scratch registers. It returns 1 when the GC reaches its atomic or finalize phase, which forces a trace exit.
     - `gc_onestep` also refuses to run the atomic phase while `jit_base` is set ("Don't run atomic phase on trace.").
     - The source comment in `asm_gc_check`: "Exit trace if in GCSatomic or GCSfinalize. Avoids syncing GC objects".
3. **Hot path vs cold path:**
   - Hot: a guard is a compare plus a branch to an exit stub.
   - Cold: the stub dumps registers, `lj_snap_restore` walks the entries, then sunk objects are rebuilt.
4. **Invariants:**
   - A snapshot holds the state before the guarded instruction.
   - `SNAP_NORESTORE` marks slots that need no restore: read-only slots and unmodified slots inherited from a parent trace.
   - Frame links encode inlined frames.
5. **GC:** the "no stack maps" trick depends on a GC that never moves objects and never runs its atomic phase while trace code holds references in registers. Otter's moving nursery could copy the trick only by restricting collections to exits or explicit poll points.
6. **Memory cost:** 4 bytes per entry, at most 500 snapshots per trace (`maxsnap`). Restoring is linear in the number of entries.
7. **AOT:** entries refer to IR instructions. For AOT they would be serialized as program counter → (slot, physical location).
8. **JS applicability:** JS frames carry more hidden state: `this`, `new.target`, the callee, `arguments`, the finally/handler stack, and generator state. Otter's `MachineFrameState` already models all of it.
9. **Experiment:**
   - Measure the deopt-table size per compiled function (ts, earley).
   - Then recompute it keeping only registers that are live at the exit (using `code_liveness`) and changed since function entry.
   - Report the bytes saved and how much register live ranges shrink.
   - Also log exits by (function, exit id). If a few exits dominate, compiling a continuation from the exit point should beat re-profiling and recompiling the whole function.

## 4. LuaJIT single IR, fold/CSE chains, loop peeling, backward assembler, call specialization
1. **Source:**
   - `LJ/lj_ir.h`:
     - `IRIns` is an 8-byte union: two 16-bit operands `op1`/`op2`, an opcode+type field `ot`, and a `prev` field that later holds the register and spill slot (`r`/`s`).
     - `REF_BIAS` splits the array: constants grow downward, instructions grow upward.
   - `LJ/lj_opt_fold.c`: `lj_opt_fold` (fold rules looked up in a hash table), `lj_opt_cse` (one chain per opcode in `J->chain[op]`).
   - `LJ/lj_opt_loop.c`: `lj_opt_loop`.
   - `LJ/lj_asm.c`: `lj_asm_trace` ("Assemble a trace in linear backwards order").
   - `LJ/lj_record.c`: `rec_call_specialize`.
2. **Work eliminated:**
   - **One representation** from recording to machine code. The register and spill slot live inside the instruction itself.
   - **Cheap CSE.** It walks one chain per opcode, stopping at the operands' positions.
   - **Free DCE.** Dead code is dropped during backward emission. The source comment: "Dead-code elimination can be soooo easy".
   - **Peeling instead of LICM.**
     - Loop-invariant code motion is replaced by copying one iteration ahead of the loop.
     - The file says LICM "is mostly useless for compiling dynamic languages" because guards block hoisting.
     - With peeling, invariant type and shape guards are eliminated from the loop body by ordinary CSE.
   - **Specialized calls.** A call gets a guard on the exact closure, or on the prototype when the function is polymorphic (`PROTO_CLC_POLY`). The call then becomes straight-line code.
3. **Hot path:** guards are fused into instructions, and inlined calls have no frames.
4. **Invariants:**
   - Strict SSA with every definition before its uses, which the copy-ahead peeling depends on.
   - No folding across values carried between loop iterations.
   - PHIs are emitted below the loop.
5. **Exceptions and deopt:** traces contain no exception handling; errors exit the trace, and deopt goes through snapshots.
6. **Compile cost:** every pass is linear. Limits: `maxrecord` 4000 instructions, `maxirconst` 500 constants. `lj_opt_sink.c` is 258 lines, `lj_snap.c` 1034, `lj_asm.c` 2643.
7. **AOT:** which traces exist depends on the runtime profile, so traces are not portable. The IR design itself is.
8. **JS applicability:** traces are fragile on polymorphic, branchy code, and ts and earley-boyer are exactly that. Borrow the data structures and peeling, not tracing as the unit of compilation.
9. **Experiment:**
   - (a) Peel innermost loops in HIR (Otter's high-level IR) this way. Count the guards left per iteration in the crypto and zlib hot loops, using hotasm.
   - (b) Profile Machine-tier compile time per stage on the function that takes about 10 s to compile.
   - If IR conversions or register allocation dominate, a single flat-array IR is the lever.

## 5. Cranelift pipeline: CLIF, then e-graph rewrites, then ISLE lowering, then regalloc2
1. **Source:**
   - `WT/cranelift/docs/compare-llvm.md` (not re-fetched in this revision): Cranelift uses a single IR, against LLVM's IR, SelectionDAG, MachineInstr and MC layers.
   - `WT/cranelift/codegen/src/ir/instructions.rs`: a test asserts each instruction (`InstructionData`) is 16 bytes.
   - `codegen/src/context.rs`: `Context::optimize`, `egraph_pass`, `compile`, `inline`.
   - Rewrite rules are in `codegen/src/opts/*.isle`. Lowering rules are in `isa/x64/lower.isle` (5200 lines) and `isa/aarch64/lower.isle` (3336 lines).
   - `codegen/src/inline.rs`: "inlining as a library". The embedder decides what to inline.
   - regalloc2: `bytecodealliance/regalloc2`, main at 2fe490bc (commit not re-checked); design post at cfallin.org/blog/2022/06/09/cranelift-regalloc2.
2. **Work eliminated compared with LLVM:** multiple IR levels and long pass pipelines.
3. **Hot/cold path:** does not apply to a compiler.
4. **Invariants:** SSA with block parameters, checked by a verifier.
5. **GC/deopt:** see card 6.
6. **Compile cost:** measured on the HPCG benchmark compiled with Wasmer's three backends (Chadha et al., PPoPP'23, arXiv 2301.03982, Table 1):

   | Backend | Compile time | Generated-code speed |
   |---|---|---|
   | Singlepass | 52 ms | 0.377 GFLOP/s |
   | Cranelift | 150 ms | 1.324 GFLOP/s |
   | LLVM | 2811 ms | 1.543 GFLOP/s |

   - LLVM takes 18.7× the compile time for code that is 16.5% faster.
   - The regalloc2 post says Cranelift's compile time "was and is dominated by regalloc time".
   - Switching to regalloc2 made compiles about 20% faster and sped up register-pressure benchmarks by up to 10–20%.
7. **AOT:** see card 7.
8. **JS applicability:** CLIF knows nothing about NaN-boxing, shapes, inline caches (ICs) or speculation. Guards become conditional branches to cold blocks.
9. **Experiment:**
   - Otter already uses regalloc2, so it already pays Cranelift's largest compile cost.
   - Translate the lowered Machine IR of the 20 hottest zlib and crypto functions into CLIF. Runtime calls become opaque calls; guards branch to cold blocks.
   - Compare compile time, code size and static instruction count against Otter's emitter on the same input.
   - If Cranelift wins by less than 10%, the backend is not where the gap is.

## 6. Cranelift hooks for runtime integration: stack maps, exceptions, debug tags, patchable calls
1. **Source:**
   - `WT/cranelift/codegen/src/ir/user_stack_maps.rs`.
   - `cranelift/frontend/src/frontend.rs`: `FunctionBuilder::declare_value_needs_stack_map` and `declare_var_needs_stack_map`.
   - `frontend/src/frontend/safepoints.rs`: `SafepointSpiller::run` and `rewrite`.
   - `ir/exception_table.rs` (the `try_call` instruction) and `ir/debug_tags.rs`.
   - `machinst/buffer.rs`: `MachPatchableCallSite`, `MachBufferFrameLayout`.
   - `isa/call_conv.rs`: the `PreserveAll` and `Tail` calling conventions.
   - Rationale: N. Fitzgerald, "New Stack Maps for Wasmtime and Cranelift" (bytecodealliance.org, 2024-09-10). The redesign was driven by 32-bit compressed GC references, moving GC, and an optimizer bug caused by reloads the optimizer could not see.
2. **Work eliminated:** the register allocator no longer tracks GC references. The frontend computes liveness only for values flagged as GC references.
3. **Hot path:**
   - A flagged value that is live across a call is stored to the stack where it is defined and reloaded at each use.
   - The module docs of `ir/user_stack_maps.rs` say "Currently all non-tail call instructions are considered safepoints", and state explicitly that calls known not to trigger GC cannot be exempted. So even a call that can never trigger GC pays this cost.
4. **Invariants:**
   - A flagged value is never kept in a register across a call.
   - Stack-map entries can be 8, 16, 32, 64 or 128 bits, so compressed references work.
5. **Deopt, OSR and exceptions:**
   - There is no built-in deopt or OSR (entering optimized code mid-loop). Two ways to build deopt:
     - A cold block that calls a deopt stub with the live values as arguments. Values stay in registers until the exit.
     - Wasmtime's debugging approach: state stored to dedicated stack slots, debug tags, and a frame table (`.wasmtime.frames`). This costs stores on the hot path.
   - Debug tags are carried through inlining specifically so "virtual" frames can be reconstructed.
   - `try_call` plus exception tables provide catch landing pads.
6. **Inline caches:**
   - A patchable call-site record holds only a return address and "the length of the region to be patched by NOP bytes". Cranelift supports switching such a call off (and back on by restoring the original bytes), but not retargeting it. Shape IDs or field offsets embedded in instructions cannot be rewritten.
   - The `PreserveAll` convention cannot return values and does not support tail calls.
   - So ICs would have to be data-driven: load the IC entry, compare, call. That likely means extra loads per IC hit; this is my estimate, not a measurement.
7. **AOT:** stack maps come out as (program counter, stack-slot offsets) data, and the embedder writes its own object-file section.
8. **JS applicability:** with NaN-boxing almost any value can be a reference, so almost everything live across a call would be spilled.
9. **Experiment:**
   - Otter already spills and reloads. Use hotasm to measure what share of retired instructions on ts and earley are these saves and reloads around calls.
   - Then mark calls that cannot allocate as non-safepoints and measure the difference.
   - That difference is what switching to Cranelift would give up.

## 7. AOT artifacts: cranelift-object and Wasmtime's precompiled `.cwasm`
1. **Source:**
   - `WT/cranelift/object/src/backend.rs`: `ObjectBuilder`, and `ObjectModule::finish`, which returns an `ObjectProduct` wrapping an `object` crate file. Relocations include `Arm64Call`, `Aarch64AdrGotPage21`, `X86GOTPCRel4` and Mach-O thread-local relocations.
   - `cranelift/module/src/module.rs`: the `Module` trait.
   - `crates/cranelift/src/obj.rs`: `ModuleTextBuilder::append_func` resolves calls between functions when the file is built.
   - `crates/wasmtime/src/runtime/code_memory.rs`: refuses to load a file with any relocation outside the `.debug_*` sections.
   - `crates/environ/src/obj.rs`: the ELF ABI marker `ELFOSABI_WASMTIME` and the metadata sections `.wasmtime.stackmap`, `.wasmtime.traps`, `.wasmtime.exceptions`, `.wasmtime.frames`, `.wasmtime.addrmap`.
   - `crates/environ/src/stack_map.rs`: stack maps stored as sorted program counters, looked up by binary search.
   - `crates/wasmtime/src/engine/serialization.rs`: `check_compatible`, `append_compiler_info`. `engine.rs`: `Engine::precompile_module`.
2. **Work eliminated:** relocation at load time.
   - Loading is just mmap plus mprotect, so code pages stay shared between processes and load lazily.
   - Relocations are banned explicitly because they would make loading slower and "force them to get paged into memory from disk".
3. **Hot path:**
   - Calls between compiled functions are direct PC-relative calls.
   - Runtime builtins go through Wasmtime's trampoline mechanism; that is why its compiler avoids CLIF operations (such as `floor` and `ceil`) that would lower to library calls.
4. **Invariants:**
   - The code section is position-independent.
   - Every runtime address comes from a context structure.
   - An engine and flags fingerprint is checked before the code is used.
5. **GC and deopt metadata:** traps, exceptions, stack maps and frame data all live in side sections sorted by program counter.
6. **Memory cost:** the side tables grow with the number of sites.
7. **AOT impact:**
   - This is the template for Otter's JS-to-executable path: one code generator where JIT mode may embed absolute addresses and portable mode may not.
   - WebAssembly is closed-world (typed imports). JS AOT would still need ICs and deopt into an interpreter embedded in the executable.
8. **JS applicability:** shapes, atoms and strings are created at runtime, so references to them must be relative to a context or to a snapshot. Otter's bootstrap snapshot could supply stable IDs.
9. **Experiment:**
   - From `relocations.json`, count the absolute address constants per target kind in ts and zlib Machine-tier code.
   - Build a mode that loads each one from a context register instead (`ldr x, [ctx, #k]` on ARM64).
   - Measure the fixed-work instruction change on fib and mega_method. If it is under 2%, make portable code the only mode.

## 8. LLVM statepoints, stack maps, patchpoints and deopt bundles
1. **Source:**
   - `LLVM/llvm/docs/Statepoints.rst` and `StackMaps.rst`. These cover `llvm.experimental.stackmap`, `llvm.experimental.patchpoint`, the `anyregcc` convention, and the stack-map section. That section is named `.llvm_stackmaps` on ELF and `__llvm_stackmaps` on Mach-O, both in segment `__LLVM_STACKMAPS`.
   - `llvm/lib/Transforms/Scalar/RewriteStatepointsForGC.cpp` and `PlaceSafepoints.cpp`.
   - `llvm/lib/CodeGen/SelectionDAG/StatepointLowering.cpp`, whose hidden options `use-registers-for-deopt-values` and `max-registers-for-gc-values` default to false and 0.
   - `llvm/lib/CodeGen/FixupStatepointCallerSaved.cpp`.
2. **What it provides:**
   - GC references are modeled as a special pointer type: non-integral pointers, `addrspace(1)` in the docs' examples. So the optimizer runs unchanged.
   - A late pass then makes safepoints explicit as `gc.statepoint` calls with `gc.relocate` results.
   - Locations named in "deopt" operand bundles are written into the stack map.
3. **Hot path:**
   - By default every GC pointer live across a safepoint is spilled to the stack.
   - The docs say the lowering "is known to be somewhat poor… hot-statepoints are almost always inliner bugs".
   - Patchpoints pad a fixed number of bytes with no-ops.
4. **Invariants:**
   - Every derived pointer needs a live base pointer.
   - The docs note a known hole in the non-integral pointer model, so speculative loads must be restricted.
5. **Platforms:** statepoints support only AArch64 and x86_64.
6. **Memory and compile cost:** the docs acknowledge "a large amount of IR… higher than expected memory usage and compile times". Field costs are in card 9.
7. **AOT:** LLVM's object emission is the best of the three. The stack-map section's name differs by object format, per `StackMaps.rst`.
8. **JS applicability and limits:**
   - Otter's NaN-boxed 64-bit value holding a 32-bit cage offset is not a pointer, so `gc.relocate` does not apply.
   - The fallback is listing stack slots as GC-live, where the docs make it the frontend's job to spill and fill. That is Otter's current approach plus LLVM's compile cost.
   - "Hot statepoints are inliner bugs" is a Java assumption, where class-hierarchy analysis makes most calls inlinable. Hot JS code calls into the runtime often.
9. **Experiment:**
   - Hand-lower one kernel (the zlib inner loop, or crypto's `am3`) to LLVM IR, with guards exiting through `llvm.experimental.deoptimize`.
   - Compare instruction count and compile time from `llc -O2` against Otter's Machine tier.
   - That puts an upper bound on the code quality any backend switch could buy.

## 9. What happened to others: Azul Falcon, WebKit's FTL moving to B3, Julia
1. **Source:**
   - Reames, "Falcon: An optimizing Java JIT", LLVM Developers' Meeting 2017 keynote (llvm.org/devmtg/2017-10/slides/Reames-FalconKeynote.pdf).
   - Pizlo, "Introducing the B3 JIT Compiler" (webkit.org/blog/5852, 2016), and the Riptide GC post (webkit.org/blog/7122, not re-fetched in this revision).
   - Julia's `src/llvm-late-gc-lowering.cpp` (`LateLowerGCFrame::runOnFunction`, `ComputeLiveness`, `ColorRoots`, `PlaceGCFrameStores`), plus `doc/src/devdocs/llvm.md` and `gc.md`.
2. **Evidence:**
   - **Falcon** is the only one of the three with a moving GC.
     - It took about 20 person-years with a team of 4–6.
     - The gc.statepoint design took "2+ person years". Its "major goal" was allowing GC pointers to be updated in registers. The talk lists this under "Be wary of over design" and concludes "hot safepoints are inliner bug", meaning the effort bought little.
     - Generated code was 3–5× larger than HotSpot's C2. The talk says "LLVM is not suitable for a first tier JIT."
     - Typical compiles took about 100 ms, with extremes from seconds to minutes. The LLVM library added about 40 MB.
     - Falcon needed a custom pass order and its own fixes to LLVM.
   - **WebKit FTL to B3:**
     - LLVM was "three quarters or more" of FTL compile time, and LLVM took 4.7× longer to compile than B3.
     - B3 was faster on a MacBook Air: JetStream +4.5%, Kraken +11.6% (+8.2% on a Mac Pro), Octane +5.2%. On the Mac Pro, JetStream and Octane showed no difference.
     - B3 has a native `Patchpoint` that can pin any argument to any register or stack slot and needs no no-op padding. Its `Check` instruction handles OSR exits without splitting blocks, because it is not a block terminal.
     - B3's register allocator (IRC) is about 1300 lines, against about 5000 for LLVM's Greedy allocator.
   - **They never had to relocate roots:**
     - JavaScriptCore scans stacks conservatively (Riptide post).
     - Julia's GC is "non-moving" (`gc.md`). It stores roots into a shadow GC frame late in the pipeline.
3. **Hot path:** B3 patchpoints let the engine's own IC generator emit code inside optimized code.
4. **Invariants:** the dynamic-language successes with LLVM avoided moving-GC stack maps entirely.
5. **GC:** the one GC cost the Falcon talk puts a number on is the 2+ person-years spent on in-register pointer updates at statepoints. The talk presents that as over-design, not as a required cost of relocation. It gives no breakdown of the ~20 person-years that would show relocation support was Falcon's biggest cost.
6. **Compile cost:** LLVM compiles about 4.7× (vs. B3) to 18.7× (vs. Cranelift on HPCG) slower.
7. **AOT:** Falcon planned to cache compiled code as object files ("does produce normal object files").
8. **JS applicability:** B3's reason for leaving LLVM is specific to JavaScript: compile time dominated FTL.
9. **Experiment:**
   - Build a histogram of Machine-tier compile time per function across Octane and ts.
   - If the median is already well above 5 ms, apply B3's lesson (compact IR, fewer passes) before considering any backend change.

---

## Backend comparison for Otter

| Requirement | Otter's own emitter today | Cranelift | LLVM |
|---|---|---|---|
| Precise moving-GC stack maps with 32-bit refs inside NaN-boxed values | Already has it: save slot plus reload, using regalloc2 | Spill at definition, reload at every use; **every** non-tail call is a safepoint | Its GC pointer model does not fit NaN-boxing; must list spill slots manually; GC values in registers off by default |
| Deopt and OSR metadata | Already has it (`MachineFrameState`, virtual objects) | Nothing built in; cold stub calls, or state slots plus debug tags | "deopt" bundles plus stack maps; OSR entry built by hand |
| Patchable ICs | Full control | Patchable call sites can only be NOP'd out and restored; no retargeting | Fixed-size no-op patchpoints, which B3 found inadequate |
| JIT compile latency | Own passes plus regalloc2 | About 3× Singlepass and 1/19 of LLVM (HPCG) | 4.7× slower than B3; about 100 ms typical (Falcon) |
| AOT object files | Missing; code embeds absolute addresses | `cranelift-object` writes ELF, Mach-O and COFF with the needed relocations; Wasmtime shows the zero-relocation model | Best (MC layer, LTO, DWARF) |
| x86_64 and ARM64 | Both | Both, plus riscv64 and s390x | Both; statepoints support only these two |
| Code quality | Depends on JS-level optimizations | LLVM's code is 16.5% faster on HPCG | Best on C-like code, but 3–5× larger code (Falcon) |
| Integration cost | Already paid | Pure Rust; Otter already uses regalloc2 | About 20 person-years for Falcon; large C++ dependency |

**Suggested direction (my inference from the above):**
- Keep one compiler foundation: HIR → Machine IR → regalloc2 → Otter's own emitter, with two output modes.
  - JIT mode may embed absolute addresses.
  - Portable mode follows Wasmtime's zero-relocation rule and writes object files through the `object` crate (the same crate `cranelift-object` uses), with side tables sorted by program counter.
- A second backend IR would contradict the goal of fewer representations.
- The measured gaps (cells, closures, GC share, RSS) are not instruction-selection problems.
- Use Cranelift and LLVM only as offline code-quality yardsticks (experiments in cards 5 and 8).

## What these systems do NOT spend that Otter spends
- **LuaJIT:**
  - Allocates no cell for a captured local until a closure exists. While the frame is alive, the value stays in a stack slot, or in an SSA register inside traces.
  - Turns immutable upvalues into constants.
  - Removes temporary objects from the fast path even when they escape on exits.
  - Keeps no GC root maps in compiled code, because the GC's atomic phase is forced to happen at trace exits.
  - Snapshots only modified, live slots at 4 bytes each, with no separate deopt recipe.
  - Uses one 8-byte IR from recording to assembly. One backward pass does register allocation, dead-code elimination and emission, and peeling replaces LICM.
  - Patches a frequently taken exit into a side trace instead of invalidating and recompiling.
- **Wasmtime/Cranelift:**
  - No relocation at load time and no runtime addresses embedded in code.
  - No GC knowledge inside the register allocator or optimizer.
  - Metadata is mmapped sorted tables.
- **LLVM users (WebKit's FTL, Julia):** no root relocation at all, because JavaScriptCore scans stacks conservatively and Julia's GC never moves objects. By design they avoid the spill-and-reload cost Otter pays at every safepoint. Falcon, the moving-GC case, spent 2+ person-years on the opposite design, keeping GC pointers in registers across statepoints. It concluded that lowering quality matters little, because hot safepoints are inliner bugs.

Downloaded sources are in `/private/tmp/claude-501/-Users-alexanderstreltsov-work-octofhir-otter/a2e9abdb-0d8f-437a-8cf1-f2cad155594e/scratchpad/cite/`.