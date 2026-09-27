## Citation check

| # | Citation | Verdict | Note |
|---|---|---|---|
| 1 | Otter baseline: `capture.rs::analyze_function`, `upvalue.rs::alloc_upvalue`, `frame_state.rs` / `call_ops.rs` `alloc_old_batch_with_roots` | **Corrected** | `analyze_function` returns `own.names.intersection(&inner.refs)` and never tracks assignment. It also always adds `arguments` to the own names, and `analyze_arrow` uses the same rule. `alloc_upvalue` calls `heap.alloc_old` (also true at HEAD). **But `alloc_old_batch_with_roots` exists only in uncommitted working-tree edits.** At HEAD `7e0bd47b`, both `frame_state.rs` and `call_ops.rs` allocate one cell at a time through `alloc_upvalue_with_roots` → `heap.alloc_old_with_roots`. Either way, all cells are allocated eagerly in old space on entry. They are skipped only when `own_upvalue_count == 0` (early return in `build_upvalues_for_exec`), so the cost falls on every activation of a function that has at least one captured own binding, not on every activation. |
| 2 | Keep/Hearn/Dybvig, "Optimizing Closures in O(0) time" (Scheme Workshop 2012) | **Corrected (minor)** | Read in full. These all match: the boxing sentence, Table 1 "All" row (56.94% closures, 44.89% FV, 58.58% mem. ref., 58.25% alloc), "negligible up to 20%, … average decrease of 3.6%", Cases 1a–1d and 2a/2b, §2.2's global/constant/alias/self/mutual cases, "linear in the size of the code" (§3.1), and §2.3 on sharing and safe-for-space. Wording fixes: the text is "three instructions plus one store **to initialize each field**" and "slower **or even** out-of-line allocation". "One closure per strongly connected set" is too loose. §3.3 gives exactly as many closures as there are not-well-known bindings, with the well-known ones folded in. The sharing row contributes only 1.91% closures and 0.58% alloc. Chez tried multi-code-pointer sharing, found it affected <1% of letrec bindings, and dropped it (§4). |
| 3 | Go `escape.go` `(*batch).flowClosure` at go1.25.0 (plus `solve.go` `outlives` and `walk/closure.go`) | **Verified** | Exact comment "Capture by value for variables <= 128 bytes that are never reassigned" and `n.SetByval(!loc.addrtaken && !loc.reassigned && n.Type().Size() <= 128)`. Non-byval captures take `k.addr(cv, "reference")`. `solve.go` has `(*batch).outlives` with "l = new(int) // must heap allocate: outlives for loop". `walkClosure` emits "closure converted to global" when `!clofn.IsClosure()` (a debug warning) and propagates `clo.Esc()` / `Prealloc`. `directClosureCall` says "This avoids allocation of a closure object." |
| 4 | QuickJS `quickjs.c` / `qjsc.c` at a38171d3 | **Verified** | `JSVarRef` has a GC header, `is_detached`, `is_lexical`, `is_const`, `pvalue` ("either on stack or to 'value'") and a union of `value` with `{var_ref_idx, stack_frame}`. `get_var_ref` reuses `sf->var_refs[vd->var_ref_idx]` or `js_malloc`s a new one. `close_var_ref` (which exists), `close_var_refs` and `close_lexical_var` behave as described; `close_lexical_var` also clears the slot to NULL. `sf->var_refs` is carved from the frame's stack buffer. `JS_AddIntrinsicEval` sets `eval_internal = __JS_EvalInternal`, and `JS_EvalInternal` throws "eval is not supported" when it is NULL. qjsc `feature_list[]` matches, and it emits `JS_AddIntrinsic%s`, `JS_WRITE_OBJ_BYTECODE` and `js_std_eval_binary`. |
| 5 | Hermes at 8eff1a6a: `SimpleStackPromotion.cpp` and related passes/docs | **Verified (one nuance)** | All cited identifiers and quotes exist. That covers `tryPromoteConstVariable`, `tryCopyToStack`, `tryDeleteStoreOnlyVariable`, `eliminateScopeIfEmpty`, `tryHoistScope`, `hoistFunctionToTopLevel`, "no capturing stores", `canEscapeThroughCall`, `tryPromoteObject` and `DefineOwnPropertyInst` → `AllocObjectLiteral`. It also covers `CreateScopeInst %variablescope, %parentScope` and `ResolveScopeInst`, the LazyEvalCompilation quote, and Features.md "operates only on the global scope". Nuances: `tryCopyToStack` keeps the frame stores. Also, "all accesses go through the same scope instruction" is a precondition of `FrameLoadStoreOpts`, not a general invariant. |
| 6 | OCaml 5.2.0 `asmcomp/amd64/emit.mlp` `Ialloc` and `asmcomp/comballoc.ml` | **Corrected (minor)** | The `sub r15; cmp young_limit; jb lbl_call_gc; lea 8(r15)` sequence and `record_frame_label` are exact, and `caml_call_gc` is reached out of line through `emit_call_gc`. The inline path is used only when `env.f.fun_fast`. Otherwise it calls `caml_alloc1/2/3/caml_allocN`. Comballoc's header matches, as do `combine`, `combine_restart` and `fundecl`. It restarts at calls, tail calls, `Iextcall` and `Ipoll`, ends at `Iraise`/`Ireturn`/`Iexit`, and restarts on every branch arm, switch case, loop and try. The cap is `(Config.max_young_wosize + 1) * Arch.size_addr` bytes. Tags upgraded from [V~] to [V]. |
| extra | Porffor at 08ac7ee1: `semantic.js` / `codegen.js` identifiers | **Verified** | All identifiers exist. `directCallOnlyFunctionBinding` also requires a `const` binding (or a `var` whose direct calls all come after its declarator), not `selfAware`, at least one direct-call ref, and not a closure-own local. `closureOwnLocalReadIsLocal` = `name in scope.locals && !closureOwnLocals[name].node._writes`. |

---

# Closures, allocation and AOT: design cards for Otter from Go, OCaml, Chez, Hermes, QuickJS, Hopc and Porffor

**Otter baseline (checked read-only in the repo, HEAD `7e0bd47b` plus working tree):**
- `crates/otter-compiler/src/capture.rs::analyze_function` turns every own name that a nested function references into a cell (`own.names.intersection(&inner.refs)`). The own names always include `arguments`, and `analyze_arrow` applies the same rule. It does not track whether the name is ever assigned. When there is a nested direct eval, every own name gets a cell.
- `upvalue.rs::alloc_upvalue` calls `heap.alloc_old`.
- On frame entry, `frame_state.rs` and `call_ops.rs` allocate all `own_upvalue_count` cells up front in old space.
  - At HEAD they go one cell at a time through `alloc_upvalue_with_roots` → `heap.alloc_old_with_roots`.
  - The uncommitted working tree switches this to one `alloc_old_batch_with_roots` call.
  - This happens on every activation of a function with `own_upvalue_count > 0`, whether or not the closure-creating code actually runs. Functions with no captured own bindings return early and allocate nothing.
- A lead to check, not a finding: 110.9M cells for 10.2M closures is about 10.9 cells per closure. Card 5 tests this.

Tags: **[V]** means I read it at the pinned revision. **[V~]** means the identifiers come from a fetch-tool summary of the file. **[NV]** means not verified. **(cv)** marks items rechecked in the citation-verification pass.

---

## 1. Chez Scheme: assignment conversion and O(0) closure optimization (earley, ts)
1. **Source.** [V] (cv) Keep, Hearn, Dybvig, "Optimizing Closures in O(0) time", Scheme Workshop 2012, https://www.schemeworkshop.org/2012/papers/keep-hearn-dybvig-paper-sfp12.pdf (read in full). [V~] Code: https://github.com/cisco/ChezScheme/blob/v10.0.0/s/cpnanopass.ss
   - `np-convert-assignments`: helper `partition-assigned` uses `uvar-assigned?`, and the box is a `cons` whose cdr is `$unbound-object`.
   - Closure passes: `np-convert-closures`, `np-optimize-direct-call`, `np-identify-scc`, `np-lift-well-known-closures`, `np-expand/optimize-closures` (uses `info-lambda-well-known?` and `closure-free*`).
2. **Work eliminated.** Assignment conversion boxes "the locations of assigned variables (but not unassigned variables)". Unassigned captures are copied straight into a flat closure. The optimizer then:
   - removes the closure entirely for a well-known procedure with no free variables (Case 1a);
   - replaces the closure with the single free variable when there is one (1b), or with a pair when there are two (1c);
   - drops globals, constants, aliases, self-references and unnecessary mutual references from free-variable sets (§2.2);
   - shares closures within a strongly connected set of letrec bindings (§2.3, §3.3). Well-known bindings fold into the closure of a not-well-known binding, so the set ends up with one closure per not-well-known binding, or one in total if all are well-known.

   Chez tried closures with several code pointers, found they would affect less than 1% of letrec bindings, and dropped them. In Table 1 the sharing row itself is small: 1.91% of closures and 0.58% of allocation.

   Results (Table 1, averages over 67 R6RS benchmarks): statically 56.94% of closures and 44.89% of free variables are gone; dynamically 58.25% of closure allocation and 58.58% of the memory references attributable to closure access. Run time improved by "negligible up to 20%", 3.6% on average. That is with allocation "averaging around three instructions plus one store to initialize each field". The paper adds that "for implementations with slower or even out-of-line allocation, the decrease in run time due to reduction in closure allocation would be greater". Otter is in that position.
3. **Hot path.** An unassigned free-variable read is one load (illustratively `ldr v,[cp,#8+8i]`). An assigned one is two loads. Otter today loads a 4-byte handle, decompresses it, then loads the value. It also pays an old-space allocation per captured binding per activation and a store plus barrier per write.
4. **Invariants.** A by-value copy is legal only if no write to the binding can run after the capture. Chez uses "ever assigned" (flow-insensitive, after alpha-renaming; the §3 algorithm requires that variables are not assigned and are uniquely named).
5. **GC / exceptions / deopt.** Closure slots are ordinary traced Values. Deopt needs a new location kind: "closure slot i". Initializing stores into a young closure need no barrier.
6. **Memory and compile time.** A by-value slot is 8 bytes instead of a 4-byte handle, but each saves a whole cell (header plus 8 bytes) per activation. The well-known analysis is "linear in the size of the code" (§3.1). The paper reports under 1% compile-time difference.
7. **AOT.** Purely lexical and per function. JIT and AOT get the same result, stored per binding in the bytecode artifact. No closed-world assumption.
8. **JS applicability.** "Assigned" must also cover:
   - a `var` or a TDZ `let` that a hoisted function declaration can capture before its initializer runs; by-value is legal only when the initializer dominates every closure-creation site, otherwise keep the hole check or box it;
   - parameters aliased by a sloppy-mode mapped `arguments`;
   - every name visible to direct `eval` or `with` (Otter already promotes on `nested_direct_eval`).

   `for(let…)` bindings need a flow-sensitive check, because `i++` writes the next iteration's copy. Removing well-known closures requires that the function object (`prototype`, `name`, identity) is never used as a value.
9. **Experiment.** Add `written_after_init` and "initializer dominates capture" to `analyze_function`. Add a by-value capture kind to `Op::MakeClosure` plus a closure-slot load op. Step 0 is instrumentation only: count cells that are stored exactly once, on earley and ts. The idea is refuted if fewer than about 30% qualify.

## 2. Go escape analysis: by-value capture and stack closures
1. **Source.** [V] (cv) https://github.com/golang/go/blob/go1.25.0/src/cmd/compile/internal/escape/escape.go, `(*batch).flowClosure`: "Capture by value for variables <= 128 bytes that are never reassigned", implemented as `n.SetByval(!loc.addrtaken && !loc.reassigned && n.Type().Size() <= 128)`. By-reference captures flow through `k.addr(cv, "reference")`.
   - (cv) `escape/solve.go` `(*batch).outlives`: its comment gives the loop rule, "l = new(int) // must heap allocate: outlives for loop".
   - (cv) `walk/closure.go` `walkClosure`: a literal that captures nothing (`!clofn.IsClosure()`) is returned as its plain function name, with the debug message "closure converted to global". Otherwise the closure takes `clo.Esc()` and uses `Prealloc` when escape analysis provided stack space.
   - (cv) `directClosureCall`: rewrites a direct call of a function literal so that captured variables become arguments (by-reference ones as `&var`). "This avoids allocation of a closure object."
2. **Work eliminated.** Heap allocation of both the captured variable and the closure whenever the closure doesn't outlive its frame. The closure object for immediately invoked literals. Any allocation at all for capture-less literals.
3. **Hot path.** A stack closure costs a few frame-slot stores. A by-reference variable stays in the frame and the closure holds `&var` (two loads). Only escaping cases call `runtime.newobject`.
4. **Invariants.** No heap-to-stack pointers. No stack object outlives its frame. Values stored across loop iterations go to the heap.
5. **GC.** Stack objects are described by `FUNCDATA_StackObjects` / `stackObjectRecord` and scanned precisely. When `copystack` moves a stack, `adjustframe` / `adjustpointers` fix up pointers into it. For Otter, stack cells need frame-slot tracing, and deopt must materialize them (the Machine JIT's partial escape analysis already materializes).
6. **Memory and compile time.** Whole-package graph solve, run once at build.
7. **AOT.** Needs statically known callees; indirect and interface calls count as escaping.
8. **JS applicability.** JS callees are almost always dynamic (`a.forEach(cb)` is a property load), so a static proof rarely succeeds. What works instead: a guarded optimistic version (Hopc, card 8) or partial escape analysis in the JIT after inlining. Generators and async functions make captures outlive the frame. Region allocation doesn't rescue this either: Go's arena proposal (golang/go#51317) is "on hold indefinitely due to serious API concerns" [V].
9. **Experiment.** In the Machine JIT, after inlining `forEach`/`map`/`sort` and their callbacks, run partial escape analysis over `MakeClosure` and cell allocations. Count the allocations removed on earley and ts.

## 3. OCaml: inline nursery bump allocation, Comballoc and frametables (earley GC share, ast_ctor, zlib RSS)
1. **Source.** [V] (cv) https://github.com/ocaml/ocaml/blob/5.2.0/asmcomp/amd64/emit.mlp, the `Ialloc` case: `I.sub (int n) r15; I.cmp (domain_field Domainstate.Domain_young_limit) r15; I.jb (label lbl_call_gc) … I.lea (mem64 NONE 8 R15)`. This inline sequence is emitted only when `env.f.fun_fast`. Otherwise the code calls `caml_alloc1`/`caml_alloc2`/`caml_alloc3`/`caml_allocN`. `emit_call_gc` calls `caml_call_gc` out of line, and `record_frame_label` records live registers and stack slots.
   - [V] (cv) `asmcomp/comballoc.ml`, header "Combine heap allocations occurring in the same basic block". Functions `combine`, `combine_restart`, `fundecl`.
     - It restarts at calls, tail calls, `Iextcall` and `Ipoll`, and ends at `Iraise`, `Ireturn` and `Iexit`.
     - Each branch arm, switch case, loop body and try handler is combined separately.
     - It caps the combined size at `(Config.max_young_wosize + 1) * Arch.size_addr` bytes.
2. **Work eliminated.** A runtime call and free-list search per allocation. Promotion. Write barriers on initializing stores. Remembered-set entries created by old closures or cells pointing at young values. N limit checks become one when several objects are allocated in the same block.
3. **Hot path.** 4 instructions (`sub`, `cmp`, `jb`, `lea`) plus field stores. The GC call point is out of line.
4. **Invariants.** Objects are fully initialized before the next GC point. Every GC point has a frame descriptor.
5. **GC / deopt.** The minor GC copies (moves), like Otter's nursery, and needs precise roots at allocation points; Otter's register-map safepoints serve here. A combined allocation must not span a deopt point.
6. **Memory and compile time.** One frametable entry per call or allocation site; negligible compile cost.
7. **AOT.** Frametables are static data keyed by code labels. The young pointer lives in a fixed register (r15 on amd64), so JIT and AOT must share the same ABI.
8. **JS applicability.** Object sizes depend on shape slack, so the allocated size has to be known at the site (from feedback, or Hopc's `constrsize`). Otter keeps cells in old space for "permanent global lexical proofs". That argument covers global-lexical cells only.
9. **Experiment.** Keep global-lexical cells in old space. Move function-local cells and closures to inline nursery bump allocation in the interpreter, the Template tier and the Machine tier, and merge `MakeClosure` with the cell allocations in the same block. Measure earley GC-sample share, instructions and remembered-set size, plus zlib RSS.

## 4. OCaml closure blocks, sets of closures and ref elimination
1. **Source.** [V] https://github.com/ocaml/ocaml/blob/5.2.0/runtime/caml/mlvalues.h
   - `Closure_tag 247`, `Closinfo_val` ("top 8 bits are the arity … field number for the first word of the environment").
   - `Infix_tag 249` ("an infix header inside a closure", and it "must be odd").

   [V] `lambda/simplif.ml`: comment "To transform let-bound references into variables", `exception Real_reference`, `check_function_escape`, and `eliminate_ref`, which produces `Lmutlet`/`Lmutvar`.

   [V] `4.14.2/middle_end/flambda/flambda.mli`: "a [set_of_closures] corresponds to an OCaml value with tag [Closure_tag] (possibly with inline [Infix_tag](s))". Constructors `Project_closure`, `Move_within_set_of_closures`, `Project_var`. [V~] `unbox_free_vars_of_closures.ml` `run`: "Don't let the closure grow too large."
2. **Work eliminated.** One block per recursive group, with no mutual pointers between its functions. Environment values stored inline in the block. `ref`s not captured by any closure become registers. Projections out of captured tuples are hoisted to closure creation.
3. **Hot path.** A free-variable read is `mov r,[clos+8*(start_env+i)]`. Reaching a sibling function in the same set is pointer arithmetic, not a load.
4. **Invariants.** OCaml variables are immutable; mutation goes through explicit `ref`, which the compiler removes only when no function body mentions it (`Real_reference`).
5. **GC.** The GC must map infix interior pointers back to the enclosing block (`Infix_offset_val`). With Otter's compressed 32-bit handles, share environments rather than code blocks.
6. **Memory.** A shared block keeps the union of free variables alive. The paper in card 1 (§2.3) notes that sharing a closure between procedures with different lifetimes breaks safe-for-space. Chez therefore shares only within strongly connected sets, or among well-known procedures with identical free-variable sets.
7. **AOT.** Toplevel closures are static constants.
8. **JS applicability.** Every JS function needs its own identity, own properties and a mutable `[[Prototype]]`, so infix-style shared closures are out. A shared environment still works. JS `let` is an implicit `ref`, so JS needs the inverse direction: Chez's analysis.
9. **Experiment.** In scheme2js-style earley code, measure the free-variable overlap among mutually recursive function declarations in one scope.

## 5. QuickJS: lazy open var-refs, and qjsc packaging
1. **Source.** [V] (cv) https://github.com/bellard/quickjs/blob/a38171d37357edff256c28a73eaeb41d2466d1d5/quickjs.c
   - `JSVarRef` has a GC object header, `is_detached`, `is_lexical`, `is_const`, `pvalue` ("pointer to value, either on stack or to 'value'") and a union of `value` with `{var_ref_idx, stack_frame}`.
   - `get_var_ref` reuses `sf->var_refs[vd->var_ref_idx]`, bumping its refcount, or `js_malloc`s a new ref.
   - `close_var_ref`/`close_var_refs` copy `*pvalue` into the ref's `value` on frame exit, repoint `pvalue` and set `is_detached`.
   - `sf->var_refs` itself is carved out of the frame's stack buffer.
   - `JS_AddIntrinsicEval` sets `ctx->eval_internal = __JS_EvalInternal`; if it is NULL, `JS_EvalInternal` throws "eval is not supported".

   [V] (cv) `qjsc.c`: `feature_list[]` (date, eval, string-normalize, regexp, json, proxy, map, typedarray, promise, module-loader, weakref) maps to `-fno-<feature>`. The generated `JS_NewCustomContext` calls `JS_AddIntrinsic%s`. Bytecode comes from `JS_WriteObject(…JS_WRITE_OBJ_BYTECODE)` and is emitted as C arrays that `js_std_eval_binary` loads at startup.
2. **Work eliminated.** No cell for any activation that never creates a closure. The owning function uses a plain frame slot. All closures of one activation share one ref per variable.
3. **Hot path.** Owner access is one slot access. A closure access loads the ref, then `pvalue` (two loads). Costs move to closure creation (allocate the ref) and frame exit (walk the refs).
4. **Invariants.** At most one open ref per slot. Close before the frame is reused. Per-iteration bindings are closed each iteration: `close_lexical_var` closes the ref and clears the slot, so the next iteration gets a fresh ref.
5. **GC.** QuickJS uses refcounting with cycle collection and does not move objects, so a raw `pvalue` pointer is safe. Otter's moving GC needs `(frame, slot)` addressing while a ref is open, and write barriers for parked generator frames.
6. **Memory.** One small object per captured slot, and only for activations that actually capture.
7. **AOT.** qjsc produces bytecode plus a runtime, not native code. The only closed-world knob is feature stripping; eval works exactly when the compiler is linked in.
8. **JS applicability.** Fully spec-compatible.
9. **Experiment.** Count cells allocated at frame entry that no closure ever references on earley. If they are most of the 110.9M, switch to lazy materialization at `MakeClosure`. This is independent of card 1 and composes with it.

## 6. Hermes: per-scope environments and scope optimizations
1. **Source.** [V] (cv) https://github.com/facebook/hermes/tree/8eff1a6ac16c336ca7cf7ad4cc947e420bfc8ef2
   - `doc/IR.md`: `CreateScopeInst %variablescope, %parentScope`, `LoadFrameInst`, `StoreFrameInst`, `ResolveScopeInst`.
   - `lib/Optimizer/Scalar/SimpleStackPromotion.cpp`:
     - `tryPromoteConstVariable` ("only ever stored to with a single literal value"; replaces loads with the literal and deletes the stores);
     - `tryCopyToStack` ("only ever stored to by directly writing to a CreateScopeInst"; the frame stores are kept so inner functions still see the value);
     - `tryDeleteStoreOnlyVariable` (no loads).
   - `ScopeElimination.cpp`: `eliminateScopeIfEmpty`.
   - `ScopeHoisting.cpp`: `tryHoistScope`, `hoistFunctionToTopLevel` (for a function "known to never use its parent scope").
   - `FrameLoadStoreOpts.cpp`: applies only "in functions where all accesses to a given variable go through the same scope instruction", and deduplicates loads across calls "as long as we know that there are no capturing stores".
   - `FunctionAnalysis.cpp`: `canEscapeThroughCall`.
   - `ObjectStackPromotion.cpp`: `tryPromoteObject`.
   - `ObjectMergeNewStores.cpp`: merges `DefineOwnPropertyInst`s into one `AllocObjectLiteral`.
   - `doc/LazyEvalCompilation.md`: "rely on optimizations not running … because we rely on everything being in Environments at runtime".
   - `doc/Features.md`: local `eval()` "operates only on the global scope".
2. **Work eliminated.**
   - One environment per scope instance instead of one cell per binding.
   - No environment for empty scopes.
   - Flattened scope chains.
   - Owner-side reads served from a stack copy; the environment store remains.
   - Constant captures folded away.
   - Stores to variables that are never loaded deleted.
3. **Hot path.** Load the environment from the closure, then load the slot, usually at depth 1 after hoisting.
4. **Invariants.** Load/store forwarding requires every access to a variable in the function to go through one scope instruction, and load deduplication tracks capturing stores.
5. **GC.** One N-slot object, barriered on store; it should be allocated young.
6. **Memory and compile time.** Not safe-for-space: the environment keeps every captured variable alive. Module-level passes.
7. **AOT.** Everything runs at HBC build time. Lazy compilation applies only when running from source.
8. **JS applicability.** Hermes gives up local eval to get these passes. Otter can't, so any function with direct eval (and its enclosing chain) must keep unoptimized environments.
9. **Experiment.** On earley and ts, statically count captured bindings that are single-literal-store or owner-only-store. Prototype one environment block per scope instance, measure cells→envs, instructions and GC share, and compare with cards 1 and 5.

## 7. Static Hermes native backend (the AOT reference point)
1. **Source.**
   - [V~] `lib/BCGen/SH/SH.cpp`, fetched at the `static_h` branch head (8eff1a6a when checked). Identifiers:
     - frame and roots: `_sh_enter`/`_sh_leave`, locals as `struct { SHLocals head; SHLegacyValue t[N]; }`;
     - environments: `_sh_ljs_create_environment`, `_sh_ljs_create_closure`, `_sh_ljs_load_from_env`;
     - property access: `_sh_ljs_get_by_id_rjs` with `get_read_prop_cache(shUnit) + index`;
     - exceptions: `_sh_try`, `SHJmpBuf`, `_sh_throw`.
   - [V] `doc/TypedLanguage.md`: `any` "casts implicitly to other types with a runtime type check"; objects are exact; classes are nominal and may be `final`.
   - [V] Discussion #1685 (tmikov): "compiling **untyped** JS to native AOT isn't a performance improvement over a high tier JIT like JSC or v8… It is more about getting predictable performance."
   - [V] Blog "How to speed up a micro-benchmark 300x" (2023): untyped native 2388 ms vs interpreter 2086 ms; `fc = +fc` brings it to 278 ms; wrapping in a module brings it to 6 ms.
2. **Work eliminated.** Dispatch. With sound types, also tag checks, boxing and inline-cache probes.
3. **Hot path.** Untyped code is a chain of helper calls (`__sh_ljs_mul_rjs`). Typed code is native `fmul`.
4. **Invariants.** Soundness is enforced by checked casts at `any` boundaries.
5. **GC / exceptions.** A shadow-stack of locals needs no stack maps and survives a moving GC, but the C compiler can't keep values in registers across calls. `try` costs a setjmp on entry. There is no deopt.
6. **Memory.** C output per function makes code large.
7. **AOT.** Portable C. Caches are data arrays (no code patching, W^X-friendly), so relocations are data pointers only.
8. **JS applicability.** No local eval. The module-scope closed world is what enabled the ~348x (2086 ms → 6 ms vs the interpreter) microbenchmark result.
9. **Experiment.** Compile ts/earley hot functions through the Machine pipeline without speculation (generic operations become calls). Compare instructions against speculative code to size what AOT without feedback loses.

## 8. Hopc (JS→Scheme→C AOT): hint typing, closure elimination, prototype-cache epochs
1. **Source.**
   - [V] Serrano, "JavaScript AOT Compilation", DLS 2018, https://www-sop.inria.fr/members/Manuel.Serrano/publi/serrano-dls18.pdf. Covers occurrence typing, hint typing ("Hint types are unsound … main negative consequence is a waste of space") and integer range analysis (untagged int32/uint32).
   - [V] Serrano, "Of JavaScript AOT Compilation Performance", ICFP 2021, http://www-sop.inria.fr/members/Manuel.Serrano/publi/serrano-icfp21.pdf. Uses Boehm-Weiser GC.
     - §4.6 functions: "Mutated closed variables are boxed"; functions "never used as values … are not allocated at all"; 0CFA leaves "only a Scheme closure".
     - §4.6 constructors: `constrmap` and `constrsize` fields.
     - §5.5 optimistic stack allocation: `js-array-maybe-foreach-procedure` guards; the slow path calls `scheme-stack-procedure->js-function`.
     - §4.5 prototype caches: global invalidation when an object marked as being on a prototype chain changes shape.
     - Eval: "direct eval has no access to its lexical environment"; eval-heavy code runs in the Bigloo interpreter.
   - [V] Poirier, Rohou, Serrano, "The False Lead of Optimizing Inline Caches", Programming 2025, arXiv 2502.20547. An AOT inline cache is `mov icache.hclass(%rip)`, `cmp`, `mov offset(%rip)`, load. Patching it at run time removed memory accesses but "does not shorten execution times".
2. **Work eliminated.** JS function objects for non-escaping functions. Heap closures for callbacks passed to builtins. Tags and overflow checks in uint32 loops. Prototype identity in hidden classes, and the polymorphism it causes.
3. **Hot path.** Cache hit: two RIP-relative loads, a compare and the field load. Prototype-chain hits carry no per-access validation.
4. **Invariants.** Marking happens on the cache-miss path; once marked, an object stays conservatively "on a prototype chain" forever.
5. **GC.** Conservative and non-moving, so stack allocation is trivial there. Otter would need precise stack objects.
6. **Memory.** Specialized function versions add code. ICFP21 reports "significantly less memory" than JITs.
7. **AOT.** Whole-module fixpoints give a closed world per module. Caches are data.
8. **JS applicability.** Near-complete ES support; loses local eval.
9. **Experiment.** Otter's shapes don't include the prototype, so add Hopc's epoch: mark objects on inline-cache miss traversal, bump a global epoch when a marked object changes shape, and have non-own caches check it. Measure mega_method. Also switch Template JIT caches to the data-driven form and check that cycles don't change (instructions will).

## 9. Go ABIInternal and precise stack maps (fib, JIT/AOT metadata)
1. **Source.** [V] https://github.com/golang/go/blob/go1.25.0/src/cmd/compile/abi-internal.md
   - Argument registers: amd64 integer RAX, RBX, RCX, RDI, RSI, R8–R11 and float X0–X14; arm64 R0–R15 and F0–F15.
   - Closure context in RDX / R26; g in R14 / R28.
   - "There are no callee-save registers."
   - The caller reserves spill space "so that the compiler can generate a stack growth path that spills into this reserved space".

   [V] Go 1.17 release notes: about 5% faster, about 2% smaller binaries.

   [V] `internal/abi/symtab.go`: `PCDATA_UnsafePoint=0`, `PCDATA_StackMapIndex=1`, `FUNCDATA_ArgsPointerMaps=0`, `FUNCDATA_LocalsPointerMaps=1`, `FUNCDATA_StackObjects=2`.

   [V] `runtime/stack.go`: `copystack`, `adjustframe`, `getStackMap`, `adjustpointers`. [V] `runtime/mgcmark.go` `scanframeworker` scans async-preempted frames conservatively. `cmd/compile/internal/liveness/plive.go` builds the liveness bitmaps.
2. **Work eliminated.** Stack traffic for arguments. Register maps at safepoints, since no value lives in a register across a call. Callee-save spill and restore.
3. **Hot path.** Prologue: compare the stack guard and branch to morestack. Arguments and result stay in registers.
4. **Invariants.** At every call site, live pointers are only in stack slots described by a bitmap. Unsafe points are marked.
5. **GC.** Precise, and stacks move. Conservative scanning is safe only because the heap never moves. Otter must preempt only at polls, or pin objects.
6. **Memory.** Varint-compressed tables; size figures [NV].
7. **AOT.** The tables are static (pclntab). The JIT can emit the identical format.
8. **JS applicability.** A JS call also needs a closure register, argc, `this` and `new.target`, and must support arity mismatch and deopt frames.
9. **Experiment.** Prototype a direct self-call in the Machine tier: arguments in registers, closure in a fixed register, no callee-saves, caller-reserved spill space. Measure fib instructions per call.

## 10. Porffor: pure AOT, eval only for statically known source
1. **Source.** [V] (cv) https://github.com/CanadaHonk/porffor/tree/08ac7ee1077c05da2bec18dcca15197051e87b62. README: "Porffor compiles JS to C (with an IR inbetween)". `compiler/semantic.js`: `knownEvalSource`, `parseEval`, `evalCallKind`. `compiler/codegen.js`:
   - "eval('known/literal string') -> inline the parsed program";
   - `directCallOnlyFunctionBinding` requires all of:
     - a `const` binding, or a `var` whose direct calls all come after its declarator;
     - a function that is not `selfAware`;
     - at least one direct-call reference, zero value references and zero writes;
     - a binding that is not a closure-own local;
   - `closureOwnLocalReadIsLocal`: reads stay local when the name is a local and its closure-own entry has no `_writes`;
   - `mirrorToClosureEnv`.
2. **Work eliminated.** Function objects for bindings that are only ever called directly. Environment loads for captured locals that are never written.
3. **Hot path.** A plain local read.
4. **Invariants.** Driven by write counts.
5. **GC / exceptions / deopt.** Nothing specific.
6. **Memory and compile time.** Nothing notable.
7. **AOT.** Fully closed world.
8. **JS applicability.** How dynamic `eval` and `Function` with unknown source are handled: [NV].
9. **Experiment.** Count direct-call-only function bindings in earley and ts. This cross-checks card 1's well-known rule.

## 11. Typed shapes (Higgs)
1. **Source.** [V] Chevalier-Boisvert & Feeley, "Extending Basic Block Versioning with Typed Object Shapes", arXiv 1507.02437: typed shapes "eliminate on average 48% of type tests, reduce code size by 17% and reduce execution time by 25%".
2. **Work eliminated.** Tag checks on values loaded from fields, and method-identity checks.
3. **Hot path.** One shape guard then covers the types of the loaded values.
4. **Invariants.** Generalizing a field's type must invalidate dependent code.
5. **Deopt.** Dependent code needs a deopt hook.
6. **Memory.** A type lattice per shape transition.
7. **AOT.** Only usable through guarded code versions.
8. **JS applicability.** Fine for plain fields; accessors and Proxy fall back to generic paths.
9. **Experiment.** Add field-type tags to Otter shape transitions. Count Machine-JIT checks removed on ast_ctor and ts.

---

## Costs these systems avoid that Otter pays
- **An old-space object per captured binding, allocated eagerly on every activation of a function that has captured own bindings**, whether or not a closure is created. Chez, OCaml, Go, QuickJS, Hopc and Porffor all avoid it.
- **Boxing bindings that are never reassigned.** The other systems copy the value into the closure.
- **A runtime call per allocation, plus barriers and remembered-set entries on initializing stores.** OCaml's optimized code allocates inline in the nursery with 4 instructions (`sub`/`cmp`/`jb`/`lea`).
- **Closure objects for functions that are only called directly.** Chez Case 1a, Go's global/direct-call conversion, Hopc and Porffor all drop them.
- **One object per binding instead of one per scope.** Hermes allocates one environment per scope instance.
- **Callee-save and register-map bookkeeping at safepoints.** Go keeps nothing in registers across calls.
- **Code patching for inline caches.** Hopc and Static Hermes keep caches as data, which is identical in JIT and AOT.
- **Per-access prototype-chain validation, if Otter's non-own caches do it.** Hopc invalidates by epoch instead.