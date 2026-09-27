| # | Citation | Verdict | Note |
|---|---|---|---|
| 1 | `C/beam_ssa_codegen.erl` `need_heap/1`, `need_heap_allocs/2`, `need_heap_is/3`, `need_heap_terminator/3` (L235–349) | Verified, wording tightened | The functions start at L239/244/296/323; L235 is the header comment. Walking backwards, `need_heap_is` puts the accumulated `#cg_alloc` straight after each op classified `gc` (`[I]++need_heap_need(N)++Acc`). One reservation therefore covers every constructor up to the next GC point, not just "after the last gc op". A block that falls through unconditionally passes its need to its predecessor. A successor with several predecessors, or one that is not the next block (counted as 42), gets a forced `cg_alloc`. `D/GarbageCollection.md` L58 quote is exact. |
| 2 | `J/arm/instr_common.cpp` `emit_gc_test` (L137) + `J/arm/beam_asm_global.cpp` `emit_garbage_collect` (L99) + `J/arm/beam_asm.hpp` `emit_enter_runtime(int live)` (L460) | Corrected | The hot path `add ARG3,HTOP,#bytes; cmp ARG3,E; b.ls` and the cold path `mov ARG4,#Live; bl garbage_collect` → `erts_garbage_collect_nobump` are exact, and `S_RESERVED = CP_SIZE + S_REDZONE` (`B/erl_vm.h` L117) is right. **Wrong:** the report says the GC cold path spills only `min(Live,6)` registers. The shared `garbage_collect` fragment calls `emit_enter_runtime<eStack|eHeap|eXRegs>()` with the default `live = 6`, so it always stores all six register-backed X registers. `Live` goes only to the collector. The `min(live,6)` trim applies to module-code runtime calls that pass their own count. |
| 3 | `B/erl_gc.c` `setup_rootset` (L2616), minor root loop (~L1676–1715) | Corrected | `setup_rootset` is at L2616. The loop is in `do_minor` (L1639) and switches on `primary_tag`, handling only BOXED/LIST; everything else hits `default: break`. `is_CP` is at `B/erl_term.h` L1379. **Incomplete:** the root set is not just "X[0,Live) + stack". It also holds the process dictionary, `seq_trace_token`, `group_leader`, `parent`, `fvalue`, `ftrace`, the receive-marker block, NIF-frame roots and message-queue terms. The validator functions are at the cited lines (`test_heap/3` L2062, `verify_y_init/1` L3293, `verify_live/2` L3315; `prune_x_regs` L2095 and `kill_fregs` L2145 are called from `test_heap`). |
| 4 | `B/erl_vm.h` `HAllocX` (L154), `B/utils.c` `erts_heap_alloc` (L135), `B/erl_gc.h` `ERTS_IS_GC_DESIRED_INTERNAL` (L132), `B/erl_gc.c` `erts_gc_after_bif_call_lhf` (L418) / `collect_live_heap_frags` (L151 cited; actual L2489), `J/arm/instr_bif.cpp` (L567) | Corrected | The mechanism is right: compare-and-bump, else a malloc'd `ErlHeapFragment` that reuses spare room in the current one. The test is `mbuf_sz` vs stop−htop, `off_heap.overhead > bin_vheap_sz`, and `F_FORCE_GC`. The `collect_live_heap_frags` line was wrong (L151 → L2489), and L2489 is the function's definition. L567 is in the **shared** `call_light_bif_shared` fragment (L378; test at ~L491–513), not per-site code. **Refuted (strong form):** the report says C runtime code never roots locals or re-reads after allocation. `pd_hash_put` (`B/erl_process_dict.c` ~L693–702) roots `Eterm root[3]` of C locals, calls `erts_garbage_collect` and re-reads them. `erts_gc_new_map` (`B/beam_common.c` L1864), `B/erl_record.c` L278/L403 and `B/beam_bp.c` also GC with explicit roots. BIFs are what never GC. `erts_gc_after_bif_call_lhf` even allows for a BIF that did GC ("Must have GC:d in BIF call"). |
| 5 | `J/beam_jit_common.hpp` `always_small` (L267), `is_sum_small_if_args_are_small` (L368); `J/arm/instr_arith.cpp` `emit_i_plus` (L145), `emit_are_both_small` (L62); `B/beam_file.c` `init_fallback_type_table` (L607); blog `type-based-optimizations-in-the-jit` | Verified; blog figure corrected | The proven path is exactly `and TMP1,rhs,#~0xF; add dst,lhs,TMP1`. The unknown path is `adds`, then the type test, then `plus_body_shared` (or `erts_mixed_plus` when there is a fail label). `parse_type_chunk` (L696) accepts `BEAM_TYPES_VERSION` **or 3** and falls back for anything else. There is no re-verification; `safe` `call_fun2` is only a debug `ASSERT`. The blog says OTP 25 encodes in "80 percent of the time" (50→62 it/s, about +24% throughput), not "+20%". The `base64` module was "not modified". |
| 6 | `J/arm/beam_asm.hpp` L83–136, `emit_enter_erlang_frame` (L429); `J/arm/instr_call.cpp` L38–117 (+ L49 comment); `J/arm/ops.tab` `i_test_yield` (~L929–944); `B/erl_vm.h` `CONTEXT_REDS` (L53); `D/BeamAsm.md` | Verified | E=x20, c_p=x21, FCALLS=w22, HTOP=x23, code index=x24, XREG0–3=x25–x28, XREG4–5=x15–x16. This is the release build; DEBUG makes XREG3–5 caller-save; x19 holds scheduler registers. The prologue is `emit_enter_erlang_frame` (str x30,[E,#-8]!) + `b next` + reserved `bl` (`emit_i_breakpoint_trampoline`, `beam_asm_module.cpp` L231), then `adr; subs FCALLS,#1; b.le` (`emit_i_test_yield`, L3097). Return is `ldr x30,[E],#8; subs; b.mi; ret`. The L49 dialyzer comment is exact. The ops.tab rule is at L929–938 and the "ingress" comment at L943. `CONTEXT_REDS 4000` is right. BeamAsm.md L93–104 has "about double … about 10% more" and load time "almost linearly". The x86 table at L74–81 also lists rbp. |

Other spot checks:
- **Verified:** `emit_i_make_fun3`, `emit_i_lambda_trampoline` (ldp unpack into `x[arity-nfree..arity)`), `emit_i_call_fun2`, `ErlFunThing { … Eterm env[]; }`, `make_op` L692–701, `patchImport/Lambda/Literal` L533/545/557 with `LLONG_MAX` asserts, `beamasm_patch_import/lambda` L1205/1242, `erts_is_literal` L1572, and `ERTS_LITERAL_VIRTUAL_AREA_SIZE` = 1 GB (`sys/common/erl_mmap.h`, fetched from GitHub; not in the sparse clone).
- **Corrected:**
  - `check_process_code` L71 is the declaration; the definition is at L1149.
  - `erts_literal_area_collector` (L1538) is a `Process *` global, not a function.
  - The literal test is one subtract plus one unsigned compare (`ErtsInArea`), not two compares.
  - The road-to-the-JIT blog treats "tracing" and "LLVM" as one project, BEAMJIT: a tracing JIT that compiled with LLVM. It was dropped because tracing lowered base interpreter speed and LLVM compilation was slow; three projects in total, the last ended in 2019. HiPE's 2–3× is right, and "projects within Ericsson that tried HiPE found that it did not improve performance".
  - `D/GarbageCollection.md`: +20% growth starts at about 1 M words, and young-heap shrink applies only when the heap is "big".
- **Otter paths exist:** `benchmarks/measurements/2026-09-27-frame-cell-batches.md`, `emit_save_safepoint_roots` / `emit_load_safepoint_root` / `emit_allocating_call` (`crates/otter-jit/src/machine/numeric/arm64.rs` L4482/4506/4418), `alloc_upvalue_with_roots` (`crates/otter-vm/src/upvalue.rs` L92), and `crates/otter-vm/src/work_budget.rs` L41 ("checks enforcing budgets before every instruction").
- **Unverifiable here:** the Otter sample counts (94 samples, 110.9M cells, +6.27%).

---

# BEAM / BeamAsm mechanism cards for Otter

**Pinning.** I read every name below in a sparse clone of `erlang/otp` at tag **OTP-29.1.1** (commit `ad05823719d77c8faee87348ea39513d4e2f99c5`). URL form: `https://github.com/erlang/otp/blob/OTP-29.1.1/<path>#L<n>`. Abbreviations: `J/` = `erts/emulator/beam/jit/`, `B/` = `erts/emulator/beam/`, `C/` = `lib/compiler/src/`, `D/` = `erts/emulator/internal_doc/`. Line numbers are for that tag. The clone is at `/private/tmp/claude-501/-Users-alexanderstreltsov-work-octofhir-otter/a2e9abdb-0d8f-437a-8cf1-f2cad155594e/scratchpad/otp`.

**Shape of the system.** BeamAsm is a one-instruction-at-a-time template JIT that runs at load time. It has no IR, no speculation, no deopt and no OSR. Its speed comes from two places. First, the ahead-of-time compiler (`erlc`) does the SSA work: types, register allocation and grouping of heap needs. Second, there is a fixed contract between generated code and the runtime (pinned registers, a live-register count, tagged stack words). Each card marks what is portable to JS and what only follows from Erlang semantics.

---

### 1. Grouped heap reservation plus inline bump allocation
1. **Source:** `C/beam_ssa_codegen.erl` `need_heap/1` (L239), `need_heap_allocs/2` (L244), `need_heap_is/3` (L296), `need_heap_terminator/3` (L323); the whole pass spans L235–349. `J/arm/instr_common.cpp` `emit_gc_test` (L137), `emit_allocate_heap` (L206), `emit_test_heap` (L231), `emit_put_list` (L707). `J/arm/instr_fun.cpp` `emit_i_make_fun3` (L211). `J/arm/beam_asm_global.cpp` `emit_garbage_collect` (L99). `D/GarbageCollection.md` L58 ("All the allocations are combined to a single instruction").
2. **Work eliminated:** a limit check and a runtime call for every object. The compiler walks each block backwards and adds up the words for every constructor (`put_list`, tuple, `make_fun`, float). Each time it reaches an op classified `gc` (a call or BIF), it inserts the running total as one `test_heap Words Live` directly after that op and resets the total. One check therefore covers every constructor up to the next GC point. A block's remaining need is passed to its predecessor when that predecessor falls through to it unconditionally. A block with several predecessors, or one reached by a jump rather than fall-through, gets its own forced `cg_alloc`.
3. **Hot/cold:** the hot path is `add x2,HTOP,#bytes; cmp x2,E; b.ls ok`, once per group. After that, a cons is `stp hd,tl,[HTOP],#16; sub dst,HTOP,#(16-tag)`, 2 instructions. A closure is a header `stp`, env `stp` pairs, then `orr` for the tag and `add HTOP`. The cold path is `mov x3,#Live; bl garbage_collect`, which leads to `erts_garbage_collect_nobump`.
4. **Invariants:** no GC point may occur between the reservation and the initialization of the last object. Heap and stack share one block and grow toward each other, so one compare covers both (`allocate_heap` adds the stack words). `S_RESERVED` (`B/erl_vm.h` L117, `CP_SIZE + S_REDZONE`) leaves room for a pushed return address.
5. **GC/exc/deopt:** only the check is a safepoint. Nothing can throw between reservation and initialization, and there is no deopt. In JS, any exit (throw or deopt) inside a group would leave reserved memory uninitialized, so it must be filled or trimmed.
6. **Cost:** no metadata. Code is 3 instructions per group plus the stores. Compile cost is one linear backward pass.
7. **AOT:** a pure codegen contract. HTOP and the limit need to be pinned registers or TLS. No relocations.
8. **JS fit:**
   - *Portable:* reservation within a block for object and array literals, closures, cells, `arguments` and boxed doubles when the size is static.
   - *Limits:* any op that can run user code (getters, Proxy traps, `valueOf`, constructors) ends a group. JS objects need a shape header and barrier state. BEAM needs neither because old→young pointers cannot exist in Erlang.
   - The mechanism only pays off for nursery bump allocation. Otter's frame-cell batch (`benchmarks/measurements/2026-09-27-frame-cell-batches.md`) applies reservation in the runtime to old space. BEAM applies it inline in generated code, in the young generation.
9. **Experiment:** the rejected young-binding pilot (+6.27% on Earley; not re-verified here) combined two costs: allocation through the Rust allocator and survival or scavenge. Measure them separately.
   - (a) Record the scavenge survival rate of cells and closures on Earley and ast_ctor.
   - (b) Add a Machine-JIT HIR op that reserves Σsize in the nursery for runs of allocations that contain no user code, followed by k inline bump stores. Track retired instructions and dynamic `emit_allocating_call` counts.
   - Reject the idea if fewer than about 25% of allocation sites share a group or survival is above about 50%.

### 2. Flat closures: captured values copied inline, lambda-lifted body
1. **Source:** `J/arm/instr_fun.cpp` `emit_i_make_fun3` (L211), `emit_i_lambda_trampoline` (L167), `emit_i_call_fun2` (L402). `B/erl_fun.h` `ErlFunThing { … Eterm env[]; }` (L65–85). `C/beam_asm.erl` `make_op({make_fun3,…})` and `make_op({call_fun2,…})` (L692–701).
2. **Work eliminated:** binding cells, environment chains, per-binding allocations, and the indirection on every read. Free variables are copied by value into the fun's tail. On call, the lambda trampoline unpacks `env[]` into X registers `x[arity..arity+nfree)` (`ldp` pairs) and branches to an ordinary function. Inside the body, captured variables are just argument registers.
3. **Hot/cold:** creation is part of the grouped reservation (card 1), with no call. A call of a known lambda (`call_fun2` with a lambda operand) goes straight to its trampoline, with no fun or arity check. The `safe` tag skips the type test; it is trusted, and only a debug-build `ASSERT` checks it.
4. **Invariants:** captured values never change after capture. This is guaranteed by single assignment.
5. **GC/exc/deopt:** the env is traced as part of the fun object, with no barriers.
6. **Cost:** a fun is `ERL_FUN_SIZE + nfree` words. There are no cells.
7. **AOT:** the lambda entry is a relocation (`patchLambda`). Otherwise it is static.
8. **JS fit:**
   - *Erlang-only:* everything being immutable.
   - *Portable subset:* a captured binding that is never assigned after the closure is created can be copied by value. This covers `const`, and parameters, `let` and `var` whose assignments all dominate capture creation.
   - The analysis is purely syntactic (oxc AST). It must be disabled when there is a direct `eval` or `with` in scope, or when the binding is observable through a sloppy-mode mapped `arguments`.
   - TDZ needs care: capture before initialization requires a cell or a hole check.
   - Mutable-after-capture bindings still need shared identity (a cell, or a V8-style per-scope context).
9. **Experiment:** this is already Otter's stated next hypothesis. Before building it, instrument the compiler to classify each of the 110.9M Earley binding cells (figure not re-verified here) as never-written-after-capture or mutated. If more than 50% are never written after capture, implement it and measure Earley instructions, GC share and closure bytes.

### 3. GC roots from one live count: no stack maps
1. **Source:** `B/erl_gc.c` `setup_rootset` (L2616) and the minor root loop in `do_minor` (L1639; loop around L1676–1715), which switches on `primary_tag` and skips everything that is not boxed or a list. `B/erl_term.h` `is_CP` (L1379). `emit_gc_test` passes `Live`; `emit_garbage_collect` calls `erts_garbage_collect_nobump(c_p, need, xregs, live, fcalls)`. `C/beam_validator.erl` `test_heap/3` (L2062), `verify_live/2` (L3315), `verify_y_init/1` (L3293), `prune_x_regs/2` (L2095).
2. **Work eliminated:** safepoint tables, PC→map lookups, and per-site lists for spilling and reloading roots. Register liveness at a GC point is a single immediate. The execution-state roots are the X array `[0,Live)` plus a uniform scan of every stack word. `setup_rootset` adds a fixed set of per-process fields: process dictionary, `seq_trace_token`, `group_leader`, `parent`, `fvalue`, `ftrace`, the receive-marker block, NIF-frame roots and message-queue terms. None of these depend on the PC.
3. **Hot/cold:** the hot path costs nothing. On the cold path, the shared `garbage_collect` fragment runs `emit_enter_runtime<eStack|eHeap|eXRegs>()` with the default `live = 6`. Because one fragment serves every call site, it always stores all six register-backed X registers (x0–x5) to the X array. `Live` is passed only to the collector, which scans `[0,Live)`. The `min(live,6)` trim in `emit_enter_runtime(int live)` (`J/arm/beam_asm.hpp` L460) applies to module-code runtime calls that pass their own live count. After the GC, the registers are reloaded.
4. **Invariants:**
   - Live values occupy a dense prefix `x0..x(Live-1)`. The register allocator ensures this and the validator checks it.
   - Every stack word is a valid tagged term, a CP (tag `00`) or a catch tag. Y slots are initialized by `allocate`/`init_yregs` before any GC point.
   - No untagged derived pointers live across a GC point.
   - Float registers are killed across GC (`kill_fregs`, L2145, called from `test_heap/3`).
5. **GC/exc/deopt:** the moving GC rewrites the X array and stack in place. Dead values that are still valid cause retention; the compiler emits `trim`.
6. **Cost:** zero metadata. Slots must be initialized at frame allocation.
7. **AOT:** very good. There are no per-safepoint tables to emit or relocate. Any backend that honors the contract (interpreter, JIT, AOT) is GC-compatible.
8. **JS fit:**
   - *Portable:* Otter's NaN-boxed `Value` is self-describing, as an Eterm is. A frame of Values can be scanned uniformly if every slot always holds a valid Value (`undefined`-initialized).
   - *Limits:* an optimizing tier wants unboxed int32/f64 and derived pointers in registers. Those must be kept in a separate area that is not scanned and killed or re-boxed at safepoints, as BEAM does with float registers.
   - A shared spill fragment that saves a fixed register set, as BEAM does, is simpler than per-site spill lists at the cost of a few extra stores.
9. **Experiment:** count the instructions executed in `emit_save_safepoint_roots`/`emit_load_safepoint_root` (`crates/otter-jit/src/machine/numeric/arm64.rs`) on Earley and ast_ctor. Prototype the contract: at every safepoint, tagged values live in one boxed spill area that is scanned as a whole, and unboxed values live in a separate area. Compare instructions, safepoint-table bytes, and `OTTER_GC_STRESS` failure counts.

### 4. BIFs never trigger GC: heap fragments and deferred GC
1. **Source:** `B/erl_vm.h` `HAllocX` (L154). `B/utils.c` `erts_heap_alloc` (L135). `B/erl_gc.h` `ERTS_IS_GC_DESIRED_INTERNAL` (L132). `B/erl_gc.c` `erts_gc_after_bif_call_lhf` (L418) and `collect_live_heap_frags` (L2489). `J/arm/instr_bif.cpp` `emit_call_light_bif_shared` (L378; GC-desired test ~L491–513, call at L567). `D/GarbageCollection.md` §"The young heap".
2. **Work eliminated:** BIF bodies, which are most of the C runtime surface, never root their locals, never re-read after an allocation, and need no `*_with_roots` variants. The GC runs after the BIF returns, with the result, the BIF arguments and the stack as roots.
   - **This is not absolute.** A few runtime helpers that need contiguous space call `erts_garbage_collect` directly with an explicit root array and re-read afterwards. Examples: `pd_hash_put` in `B/erl_process_dict.c` (~L693–702, `Eterm root[3]` of C locals), `erts_gc_new_map` in `B/beam_common.c` (L1864), `B/erl_record.c` (L278, L403) and `B/beam_bp.c`.
   - So BEAM keeps a small `*_with_roots` pattern, but only in those helpers, not across the BIF surface. `erts_gc_after_bif_call_lhf` also handles a BIF that did GC ("Must have GC:d in BIF call").
3. **Hot/cold:** `HAlloc` is a compare and a bump. When the heap is full, `erts_heap_alloc` uses spare room in the current `ErlHeapFragment` or mallocs a new one, linked into the young generation. After a light BIF returns, the shared `call_light_bif_shared` fragment tests the `ERTS_IS_GC_DESIRED` conditions: `mbuf_sz` against free space, vheap overhead, and `F_FORCE_GC`/`F_DISABLE_GC`. If they fire, it calls `erts_gc_after_bif_call_lhf`, which collects with the result and arguments as roots.
4. **Invariants:** fragments count as young (above the high-watermark). At the next GC, fragments known to be completely live are copied wholesale into the new young heap (`collect_live_heap_frags`); their garbage survives one more cycle. Other fragments are scanned normally. The GC is guaranteed to run soon because of the flag and the `mbuf_sz` test.
5. **GC/exc/deopt:** requires BIFs to hold no heap pointers when they return. That holds trivially.
6. **Cost:** fragment overhead until the next GC.
7. **AOT:** a runtime-library contract, fully portable.
8. **JS fit:**
   - *Portable* to **leaf** natives that never re-enter JS: closure, cell and array creation, string concatenation, number→string, known-shape literals.
   - *Not portable* to natives that re-enter JS (getters, Proxy, `toString`, species constructors). Re-entry is a GC point, so values held across it still need handles. BEAM's own explicit-root helpers show that some residual rooting is expected even in the best case.
   - That leaf/re-entrant split lines up with Otter's recorded stale-handle bugs (species constructor, Promise captures).
9. **Experiment:** add a mode to `otter-gc` in which allocation inside a leaf stub overflows into a chunk attached to the nursery instead of collecting, and collection is flagged for the next safepoint. Measure how many `*_with_roots` sites can be deleted, `OTTER_GC_STRESS` failures, and Earley samples in `alloc_upvalue_with_roots` (currently 94; not re-verified here).

### 5. Types proven by the compiler, trusted by the JIT with no guards
1. **Source:** `C/beam_types.erl` external format (L1752–1783), `decode_ext/1` (L1849). `C/beam_asm.erl` `Type` chunk (L208–210) and `encode_arg(#tr{…})` (L726, L736). `B/beam_file.c` `init_fallback_type_table` (L607), `parse_type_chunk` (L696). `J/beam_jit_common.hpp` `always_small` (L267), `is_sum_small_if_args_are_small` (L368). `J/arm/instr_arith.cpp` `emit_i_plus` (L145), `emit_are_both_small` (L62). Blog: https://www.erlang.org/blog/type-based-optimizations-in-the-jit/ (OTP 25). With the `base64` module source unchanged, encoding "is done in 80 percent of the time that OTP 24 needs": 50→62 iterations/s, about +24% throughput.
2. **Work eliminated:** tag tests, overflow checks, and fun/arity checks, all without any runtime profile.
3. **Hot/cold:** `a+b` with both operands proven small and the sum proven in range is `and tmp,rhs,#~0xF; add dst,lhs,tmp`. Unknown types cost `adds`, `and/ccmp`, `b.eq`, then a slow call to `plus_body_shared`, or to `erts_mixed_plus` when there is a fail label.
4. **Invariants:** the types must be sound. They are, because terms are immutable and analysis is closed within a module. The loader does not re-verify them. `parse_type_chunk` accepts the current `BEAM_TYPES_VERSION` or version 3 and uses the fallback table for any other version (or a stripped chunk). A wrong type means memory unsafety.
5. **GC/exc/deopt:** none. With no speculation there are no deopt maps and no exits.
6. **Cost:** 16-bit type bits plus optional bounds per table entry, looked up at emit time.
7. **AOT:** ideal. A JIT and an AOT backend consuming the same facts emit the same code.
8. **JS fit:** only facts JS semantics can prove transfer:
   - results of `|0`, `>>>`, `&`, `>>`; comparisons; literals; `typeof` refinement;
   - `Uint8Array` reads (0..255). This is exactly the base64 case, and it matches the zlib and crypto byte loops;
   - loop ranges;
   - never-reassigned local function bindings (the `call_fun2` analog) when there is no `eval`/`with`.

   Typed-array length is not provable under detach or resizable buffers. Object property types (getters, Proxy) and TS annotations (unsound) are not provable and can only be feedback.
9. **Experiment:** add an otter-compiler range and type pass that writes per-operand facts into bytecode (as `{tr,Reg,Type}` does). Have the Template JIT drop proven checks and the Machine JIT seed facts without guards. Measure zlib-fixed and crypto instructions and the number of guards eliminated. Reject if fewer than 10% of dynamic arithmetic/bitwise ops have provable facts.

### 6. The register file is the machine ABI: native call/ret, tail calls, catch tags
1. **Source:** `D/BeamAsm.md` L74–81 (x86: rbx scheduler registers, rbp frame pointer or E save slot, r12 code index, r13 process, r14 reductions, r15 HTOP, all callee-save). `J/arm/beam_asm.hpp` L83–136: x19 scheduler registers, E=x20, c_p=x21, FCALLS=w22, HTOP=x23, code index=x24, and in release builds XREG0–3=x25–x28, XREG4–5=x15–x16 (DEBUG builds make XREG3–5 caller-save). `emit_enter_erlang_frame` (L429). `J/x86/beam_asm.hpp` L107–110 (`NATIVE_ERLANG_STACK`: E=rsp). `J/arm/instr_call.cpp` `emit_i_call` (L73), `emit_i_call_last` (L77), `emit_move_call_last` (L83), `emit_i_call_only` (L117), `emit_dispatch_return` (L38). `J/arm/instr_common.cpp` `emit_catch` (L2855), `emit_try_end` (L2951). `B/beam_common.c` `next_catch` (L581).
2. **Work eliminated:** argument marshalling (the caller's `x0..x(n-1)` are the callee's arguments in the same machine registers), frame headers, and interpreter-frame materialization. Tail calls reuse the frame. Exceptions need no unwind tables or landing-pad metadata.
3. **Hot/cold:**
   - On ARM, a call is `bl f`.
   - The prologue is `str x30,[E,#-8]!; b next; [bl trampoline, skipped]; adr; subs w22,#1; b.le yield` (`emit_i_breakpoint_trampoline` + `emit_i_test_yield`).
   - Frame allocation is `sub E,#8N`.
   - Return is `ldr x30,[E],#8; subs w22,#1; b.mi frag; ret`.
   - A tail call is `add E,#8N; ldr x30,[E],#8; b f`.
   - On x86, real `call`/`ret` on rsp, so the return-stack buffer predicts returns.
   - For `try`, entry is `catches++` plus storing a catch tag in a Y slot. A throw goes to a shared fragment and scans the stack for catch tags.
4. **Invariants:** Erlang-visible state lives only in X, Y and pinned registers. C calls switch stacks (`emit_enter_runtime`/`emit_leave_runtime` with matching `Update` flags). An `S_REDZONE` is always free. "Exceptions must not be thrown when the return address is reserved." (`D/BeamAsm.md` L137)
5. **GC/exc/deopt:** CPs and catch tags are tagged stack words, so the GC and the unwinder share one stack format. There is no deopt.
6. **Cost:** a frame is N words plus the CP.
7. **AOT:** one ABI for every tier, so AOT code and JIT code can call each other freely.
8. **JS fit:**
   - *Portable:* first-K arguments in pinned registers, callee-sized frames, native call/ret.
   - *JS limits:* variable arity (missing arguments filled with `undefined`, rest parameters, `arguments`), hidden `this`/`new.target`/callee arguments, generators needing frames on the heap, and stack traces or `Function.caller` needing introspectable frames.
   - Proper tail calls are not required by practice, and internal self-tail-calls are allowed.
9. **Experiment:** count retired instructions per call for fib and mega_method in each tier. Prototype direct calls to a known callee using a pinned-register argument convention with a CP-only frame, against the current `NativeFrame` ABI. Reject if most of the per-call cost is target resolution or ICs rather than frame setup.

### 7. A one-pass JIT over optimized bytecode, with shared global fragments
1. **Source:**
   - `J/arm/beam_asm_global.hpp.pl` `@beam_global_funcs` (`garbage_collect`, `raise_exception`, `dispatch_return`, `i_test_yield_shared`, `plus_body_shared`, `call_light_bif_shared`, `new_map_shared`, …). `J/arm/instr_arith.cpp` `emit_plus_body_shared` (L114).
   - `D/BeamAsm.md` §"Reducing code size and load time" (L93–104): early prototypes used about double the interpreter's code memory, current versions about 10% more, and module load time "scales almost linearly" with code memory.
   - Counter-rule at `J/arm/instr_call.cpp` L49: "The reduction test is kept in module code because moving it to a shared fragment caused major performance regressions in dialyzer."
   - History: https://www.erlang.org/blog/the-road-to-the-jit/.
     - HiPE "can often speed up sequential code by a factor of two or three", but "projects within Ericsson that tried HiPE found that it did not improve performance".
     - BEAMJIT, a tracing JIT that compiled with LLVM, was dropped for two reasons: tracing lowered the interpreter's base speed, and LLVM compilation was slow. It was the last of three research projects and ended in 2019.
2. **Work eliminated:** duplicated slow-path code, runtime optimization passes, and tier transitions (the JIT replaces the interpreter).
3. **Hot/cold:** the fast path is inline. The slow path is a `bl`/`b.cond` into a fragment under a fixed register contract (inputs in ARG2/ARG3, result in ARG1; module code runs `emit_enter_runtime` first).
4. **Invariants:** fragments are emitted once per VM. A fragment reached by a call satisfies the exception rule about the return address. On ARM, far branches go through veneers (`resolve_fragment(…, disp128MB)`).
5. **GC/exc/deopt:** error fragments rebuild the X registers from saved arguments to report the exception.
6. **Cost:** roughly interpreter-sized code. Compile time is a single emit per instruction.
7. **AOT:** fragments become a runtime library linked once, and every call site is a fixed symbol relocation.
8. **JS fit:** fully portable. Rule: short hot checks stay inline; anything that calls into the runtime goes to a fragment. The BEAM lesson is that optimizations not needing runtime feedback (types, heap grouping, register allocation) live **before** the bytecode, so every backend, including AOT, inherits them.
9. **Experiment:** on ts-fixed (the largest code), measure generated bytes per bytecode op per emitter and per tier, plus compile time. Move the slow paths of the top 10 emitters by bytes into shared stubs. Track ts-fixed instructions, i-cache misses and compile seconds.

### 8. Reduction checks only on function entry and return
1. **Source:** `J/arm/ops.tab` rule `int_func_start … => … i_test_yield` (L929–938) with the comment "Handles yielding on function ingress (rather than on each call)" (L943). `J/arm/instr_common.cpp` `emit_i_test_yield` (L3097), `emit_i_test_yield_shared` (L3086). `B/erl_vm.h` `CONTEXT_REDS 4000` (L53). `emit_garbage_collect` subtracts the GC's reduction cost from FCALLS.
2. **Work eliminated:** per-instruction budget checks. The counter never leaves its register (w22/r14).
3. **Hot/cold:** 2 instructions on entry and 2 on return. On yield, the process resumes by re-entering the function, with arguments already in X registers.
4. **Invariants:** Erlang has no loops, so every loop passes through a function entry.
5. **GC/exc/deopt:** GC and BIF work are charged to the same counter.
6. **Cost:** negligible.
7. **AOT:** trivial.
8. **JS fit:** loops are not calls, so JS also needs loop back-edge checks. Yield points must be resumable, which in Otter means entry or OSR back-edges. `crates/otter-vm/src/work_budget.rs` (L41) states that the interpreter checks enforcing budgets before every instruction.
9. **Experiment:** with enforcement on, move the interpreter check to entry and back-edge ops. Measure the instruction delta on fib (call-heavy) and crypto (loop-heavy).

### 9. Load-time relocation, a non-moving literal area, code-index hot swap
1. **Source:**
   - `J/asm_load.c` (literal patch loop around L1030–1062, `beamasm_patch_import` L1205, `beamasm_patch_lambda` L1242).
   - `J/beam_jit_common.cpp` `patchImport` (L533), `patchLambda` (L545), `patchLiteral` (L557); placeholder slots assert `LLONG_MAX`.
   - `J/arm/beam_asm.hpp` `emit_setup_dispatchable_call` (L392, loads `addresses[code_ix]`). `J/arm/beam_asm_module.cpp` `emit_i_breakpoint_trampoline` (L231).
   - `D/CodeLoading.md` (prepare/finish, three code indexes, thread progress). `D/BeamAsm.md` §"Updating code" (dual-mapped W^X, `erts_writable_code_ptr`, `erts_unseal_module`).
   - `B/global.h` `erts_is_literal` (L1572). `B/beam_bif_load.c`: the `erts_literal_area_collector` process pointer (L1538) and `check_process_code` (declared L71, defined L1149). `erts/emulator/sys/common/erl_mmap.h` `ERTS_LITERAL_VIRTUAL_AREA_SIZE` (1 GB).
2. **Work eliminated:**
   - Version checks at call time. Local calls are a direct `bl`; remote calls are `ldr [export, code_ix, lsl #3]` + `br`; a code switch is one atomic store after thread progress.
   - GC work on constants. Literals sit in a reserved 1 GB virtual range that is never copied or scanned and is shared by all processes.
   - Per-call hook checks. Tracing and NIF hooks work by flipping one branch in a fixed prologue.
3. **Hot/cold:** a local call is 1 instruction, a remote call 2–3, and a literal is one PC-relative load of a patched slot.
4. **Invariants:** the module is the unit of linking. The literal area is immutable. On purge, processes copy old literals onto their own heaps, and old code stays alive until no frame references it.
5. **GC/exc/deopt:** on 64-bit, GC skips literals with one subtract and one unsigned compare (`ErtsInArea` via `erts_is_in_literal_range`).
6. **Cost:** virtual reservation only; untouched pages cost no RSS.
7. **AOT:** the BEAM loader is effectively a linker. Code contains placeholder slots plus relocation lists for literals, imports, lambdas, strings and catches. An AOT artifact can reuse the same patch kinds: resolved statically inside the artifact, GOT-style for imports.
8. **JS fit:**
   - *Portable:* per-chunk constant pools (strings, numbers, frozen template objects) placed in a non-moving immortal range. The ESM export table works like the export table.
   - *Limits:* regexp and object literals are mutable. `eval`/`new Function` load code incrementally (Otter `code_space`). Unloading needs a liveness census (Otter already has `code_liveness.rs`).
9. **Experiment:** measure the bytes of GC-marked or scanned old space that are chunk constants on ts-fixed and zlib. Prototype a literal range excluded from marking and scavenge-root scans. Measure GC time, RSS, and the number of relocation kinds a Machine-JIT artifact would need.

---

## What BEAM does not spend that Otter spends
- **Write barriers and remembered sets.** Erlang-only: immutability makes old→young pointers impossible.
- **One heap cell per captured binding.** Mostly Erlang-only, but the portable subset is card 2.
- **Stack maps and per-safepoint root spill/reload.** Portable via card 3.
- **Handles and root lists in runtime code.** BIFs never GC; only a few helpers root explicitly (card 4). Portable to leaf natives.
- **Deopt metadata, feedback, ICs, shape guards.** Erlang-only: no objects and no speculation. The sound static types are portable only as proofs (card 5).
- **Runtime optimizing compile time.** Portable: optimization happens in `erlc`, and the JIT is one pass (card 7).
- **Old-space free-list allocation from generated code.** Portable: every term is bump-allocated young (card 1). Two sizing policies are relevant to zlib's RSS gap (795 MB against 80 MB for V8):
  - The young heap is sized by live ratio. It grows along a Fibonacci-like series from 233 words, then in 20% steps from about 1 M words. It shrinks when less than 25% is live: after a young GC only if the heap is "big", and always after a fullsweep (`D/GarbageCollection.md` §"Sizing the heap").
  - Off-heap binaries count toward a per-process "virtual binary heap" that triggers GC.
- **Global stop-the-world collection.** Erlang-only: heaps are per process.
- **Per-instruction interrupt and budget checks.** Portable with added back-edges (card 8).
- **Scanning or copying constants.** Portable through the literal area (card 9).