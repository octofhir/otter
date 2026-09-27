# Completeness critique of the 8-reader Otter architecture map

I only read code. Nothing was built or run. I checked 20 claims against the code. **VERIFIED** means the code path reads as described. **REFUTED** means the code says otherwise. **PLAUSIBLE** means it is consistent with the code but I could not confirm it.

## 1. Load-bearing claims checked against code

| # | Claim (report) | Verdict | Evidence |
|---|---|---|---|
| 1 | A `Value` cell stores the full `cage_base\|offset` address. JIT header decodes rebuild that address anyway (values, artifacts). The machine-jit report calls it "a 32-bit cage offset in the low word, decoded as cage_base + w". | **VERIFIED; machine-jit wording REFUTED** | `value/tag.rs:5-7` says heap pointers are "stored verbatim and need no unmask before a dereference" (also `:24-31`). `template/arm64/ic_probe.rs:236-247` still runs `mov w12,w9`, loads the cage base and does `add`. Every Template and Machine property or binding decode repeats 3–5 redundant instructions that the Value design was meant to remove. |
| 2 | Page `survival_age` is never reset (gc) | **VERIFIED, and worse than reported** | It is zeroed only in `PageHeader::init` (`page.rs:178`). `reset_bump` (`page.rs:379-385`) and `flip` (`space.rs:167-177`) leave it alone. It is incremented at `scavenger.rs:296-301`, and `PROMOTE_AFTER_SURVIVALS = 1` (`scavenger.rs:78`). There is a second problem even if the counter were reset. After a flip, the mutator keeps bump-allocating into the new from-pages, and those are the pages that already hold survivors with age ≥ 1. Fresh objects therefore share an aged page and get promoted at their first scavenge (`scavenger.rs:692-695`). Per-page aging is broken by design, not only by the missing reset. |
| 3 | Children of remembered parents are promoted immediately (gc) | **VERIFIED** | `scavenger.rs:692` has `promote = ctx.in_dirty_scan \|\| …`. |
| 4 | The old-space allocation path has no `maybe_major_gc` and no stress hook, but does a full barrier scan (gc) | **VERIFIED** | `heap.rs:1625-1720` has only `account_or_collect_with_roots` (`:1664`) plus `T::trace_slots` with a barrier on every slot (`:1712`). `maybe_major_gc` is called only at `heap.rs:682, 724, 1439` (reserve-bytes and young allocation). The WIP `old_batch.rs:65` also calls only `account_or_collect`. With `max_heap_bytes == 0`, a phase that allocates only cells and closures never triggers a major GC. |
| 5 | "The VM only calls `mark_phase`+`sweep_phase` back to back (`exec.rs:189-213`)" (gc) | **REFUTED on location, and it hides a correctness bug** | That code is `force_gc`, documented "Debug / test only" (`exec.rs:184-189`). Production full GCs go `maybe_major_gc` → `collect_full` (`heap.rs:1878-1886, 1904-1917`). **`collect_full` never runs `run_ephemeron_fixpoint` or `process_weak_refs_and_finalizers`.** Their only non-test callers are `exec.rs:195-197`. Full marking traces only strong edges (`marking.rs:187-214`). `WeakMapBody`'s strong trace is empty (`collections.rs:1247-1256`) and `WeakRefBody.target` is `#[pelt(skip)]` (`weak_refs.rs:59-69`). Consequences on allocation-triggered full GCs: WeakMap values reachable only through a live key are swept; dead keys and WeakRef targets stay as dangling `RawGc`; FinalizationRegistry callbacks never run. This contradicts the heap's own contract (`heap.rs:33-35`). Confirmed by reading; not reproduced. test262 likely hides it because `$262.gc` goes through `force_gc` (*speculation*). |
| 6 | A Template IC site that was empty at compile time always calls Rust (template; values open question 1) | **VERIFIED, and values open question 1 is answered** | `ic_probe.rs:394-397` emits an unconditional `b miss`. The only Template refresh is for **direct-call targets**, and it is tried once per function id (`jit_call.rs:1492-1518`, `jit_feedback_refresh_attempted`). Property-IC fills never trigger a Template recompile. |
| 7 | The ordinary-state guard is emitted twice (values, artifacts) | **VERIFIED** | `ic_probe.rs:214` (inside `emit_load_header`) and again at `:267` (inside `emit_check_shape`). |
| 8 | Machine uses x29 as the back-edge counter and has no frame chain (machine) | **VERIFIED** | Machine: `numeric/arm64.rs:249` (`subs w29`) and `:4932` (`stp x29,x30` with no `mov x29,sp`). Template does set it (`template/arm64.rs:1924-1927`). **Cross-cutting consequence:** frame-pointer stack walks by macOS `sample` stop at Machine frames. Self-sample counts (every figure the readers used) are still valid, but no caller or inclusive attribution through Machine code can be trusted. |
| 9 | Back-edge poll from Machine may collect with unrooted registers (machine open question 1) | **RESOLVED (safe)** | `runtime_activation/control.rs:47-52` wraps non-baseline frames in `always_allocate_scope`. That scope is honoured at `heap.rs:1459` (young overflow) and `:1908` (major GC). |
| 10 | "PC-keyed `SafepointRecord`s are resolved only for inline-frame deopt" (gc) | **REFUTED** | Allocating leaf stubs resolve them: `runtime_stubs.rs:358-376` (`alloc_safepoint_record`) and `:2466-2476` (`alloc_value_stub_call_roots`). They expose frame and spill slots as roots. |
| 11 | Every throw eagerly snapshots the stack and renders the value (interp) | **VERIFIED** | `dispatch.rs:727-745` snapshots when `pending_uncaught_frames` is `None`; the snapshot is dropped if a catch absorbs the throw. `render_thrown` runs unconditionally at `async_ops.rs:559`. |
| 12 | Per-site property feedback is boxed eagerly, with an atomic update on every hit (values) | **VERIFIED** | `executable.rs:1150` → `jit_feedback.rs:945-960` allocates a `Box` for every LoadProperty, StoreProperty and CallMethodValue in every loaded function, whether or not it ever runs (`:416-431`). There is also a per-instruction `InstructionFeedback`. The hit counter is a `fetch_add` (`:518-523`). |
| 13 | Opcode count: 191 (compiler) vs 192 (machine) | **192** | The schema table in `opcode_schema.rs` has 192 `(Op::…, 0x..)` rows. |
| 14 | `STUB_JIT_INLINE_CLOSURE_UPVALUES` is dead (interp) | **PLAUSIBLE** | Its only reference outside the descriptor table is the binding at `entry.rs:326-329`. No emitter references it. |
| 15 | The quadratic Template `instruction_imm32` `find` might explain zlib's 3.8 s compile time (compiler) | **REFUTED** | The linear `find` exists (`template/arm64/binding.rs:51-55`) and is Template-only. The template report measured all 13 zlib Template compiles at 15.3 ms. The 3.88 s is Machine. |
| 16 | Block-lexical cells are allocated twice (compiler) | **VERIFIED** | Each block declaration bumps `own_upvalue_count` (`function_context.rs:300-315`), so the prologue batch allocates it, and then `FreshUpvalue` replaces it at block entry (`hoist.rs:395-400`). |
| 17 | The interpreter boxes the inherited spine on every closure call (interp) | **VERIFIED** | `call_ops.rs:1422` and `:1454`. The own-cell path adds a `Vec` (`frame_state.rs:505-526, 540-555`). |
| 18 | `TailCall` is unsupported by both JIT tiers (template, machine) | **VERIFIED; the impact was missed** | The compiler emits it for every strict `return f(x)` whose callee is a free identifier (`statements.rs:851-856`, `calls.rs:785-815`). Template lowers it to `UnsupportedBail`, which makes the function `osr_only` (`plan.rs:531-533, 1795-1801`). Machine declines the whole function (`hir.rs:4180-4183`). **All ESM modules and class bodies are strict**, so any such function never enters compiled code at function entry. The measured benchmarks are not affected: the only `use strict` in `ts-fixed.js` sits inside the `compiler_input` string. |
| 19 | Realm prototypes baked into code may be young (values open question 4) | **Partly resolved** | The main realm is built under `set_tenure_all(true)` (`interp/init.rs:94-345`, `runtime/lib.rs:3016-3264`). I found no tenure scope for realms created later. `method_ops/jit_snapshot.rs:86-98` bakes `proto.offset()` with no old-space check. This is **PLAUSIBLE** as a stale-offset hazard for child realms only. |
| 20 | `JsClosureBody` size: 112 B (compiler), about 96 B payload (interp), 104 + 4n (gc) | **Unresolved** | There is no `size_of` assertion; `closure.rs:261-318` pins offsets only. My own field-order calculation from `closure.rs:178-213` gives a 96 B payload, but that depends on `Option<UpvalueCell>` having a niche and on the size of `ClosureConstructHeader`. |

Also confirmed:
- The scavenger's drop pass visits every from-space object (`scavenger.rs:322-345`).
- `cage_base()` is an Acquire atomic load (`compressed.rs:134-136`).
- The default cage-size doc is stale: it says 256 MiB, the value is 2 GiB (`compressed.rs:46-52`).
- `free_page` writes 256 KiB of zeros and then calls `MADV_FREE` (`compressed.rs:694-706, 739-746`).
- The CommonJS wrapper re-compiles a `format!` copy of the whole source (`eval_ops.rs:705-707`).
- `NEXT_SHAPE_ID` is a process-global static (`object.rs:123`), which breaks the repository's no-process-global rule.

## 2. Contradictions between reports

1. **Value decoding.** Machine-jit describes cells as offsets. Values, artifacts and GC say they are full addresses. The code says full addresses (row 1). Any redesign built on the machine-jit description would be wrong.
2. **Pinned-register conventions.** These do not contradict each other; the difference is simply not mentioned. Template uses x19 = register window, x20 = `JitCtx`, x21 = `NativeFrame` (`template/arm64.rs:1924-1930`; `binding.rs:68` loads through x20). Machine uses x19 = `JitCtx` and x17 = register base (`numeric/arm64.rs:250`). Several reports say the two tiers "share" `emit_direct_call_with_access`, so that emitter must be parameterised over two ABIs. That is an unlisted duplication.
3. **GC driver.** The GC report places the production driver in `force_gc` (row 5). The runtime report says, correctly, that major GC is triggered by allocation.
4. **Safepoint records.** The GC report says they are unused for GC. The template and machine reports rely on them, and the code agrees with the latter (row 10).
5. **Compile-time attribution for zlib.** Compiler blames Template; template measured Template at 15 ms; machine gives 3.88 s but "tier split not established". The data resolves it: Template code is 1.51 MB of the 15.26 MB, so Machine accounts for about 13.7 MB and about 3.86 s (*arithmetic from the reports*).
6. **Closure size** (row 20) and **opcode count** (row 13).
7. **Upvalue-spine forms.** GC counts 4, interp and compiler count 6. Interp and compiler also list `UpvalueSource` and `EvalEnvBody.cells`. The count of 6 is the complete one.
8. **Where marking runs.** GC says "`is_marking` never observed by the mutator". Template and machine describe barriers with a marking-byte test as live. Both are true: the test is always emitted and always false, since the collector is stop-the-world only. This supports the GC report's proposal to drop the test.

## 3. Cross-report findings no single report states

- **The nursery is largely bypassed.** Five facts combine:
  - closures and binding cells are old (compiler, gc);
  - object slabs for more than 3 properties are old (values; `heap.rs:2628`);
  - remembered-parent children are promoted immediately (row 3);
  - aging is broken (row 2);
  - the old allocation path never triggers a major GC (row 4).

  Together, almost anything reachable from a closure or a large object is tenured at its first scavenge. That fits Earley's 244 full vs 626 minor GCs and the sweep and mark-start samples.
- **The rejected young-bindings experiment was confounded.** `2026-09-27-young-bindings.md` tested young cells while closures stayed old. A cell captured by an (old) closure is a child of a remembered parent, so it is promoted at the next scavenge. The experiment could not show a benefit, and "do not repeat" in that note rests on a confounded measurement. The note itself also says the pilot's rooting was unsafe.
- **Template never learns.** Frozen property ICs (row 6), plus callees that stay in Template (Earley `sc_cons`; zlib `u`, `z`, `b`), plus direct-call refresh being attempted only once, mean these callees make a Rust call for every property site that was cold when they were compiled.
- **The profile data is limited to self samples** (row 8). Statements such as "~470 samples in JIT code" are fine; nothing inclusive is.

## 4. Subsystems and contracts no reader covered

| Gap | Why it matters |
|---|---|
| **Elements, arrays, typed arrays, ArrayBuffer**: Machine `LoadElement`/`StoreElement`, element kinds, holes, bounds checks, typed-array views, int32 `\|0` chains | zlib (11.8×) and crypto (5.9×) are dominated by these. Only fragments were reported: Template `StoreElement` sends cell values to the stub (`ic_probe.rs:1276-1290`), and `JitElementAccess` is named. zlib's 795 MB RSS also runs through `array_buffer.rs:401`. |
| **Strings**: representation, concat/rope policy, `charCodeAt`/`substring`, how string constant cells relate to atoms | ts (9.4×) scans strings heavily. The only mentions are four copies of each constant (artifacts) and `AddGeneric` calling concat. |
| **Global-object bindings** | zlib's Emscripten payload runs through indirect `eval` at global scope (`zlib-fixed.js:426, 633`). Every top-level `var` and function is a deletable global-object property (`tearDownZlib` deletes them). Template guards through the global-lexical epoch plus the dictionary layout (`binding.rs:57-104`). The Machine lowering and the dictionary-mode cost were not reviewed. |
| **Call-target feedback and method dispatch**: `CallMethodValue` with 8 targets, `.call`/`.apply` forwarding, bound functions in the JIT | mega_method and ast_ctor (`_super.call(this, …)`). Covered only by one-liners. |
| **Invalidation and dependency contract**: what bumps `jit_registry` epochs; whether prototype mutation, shape changes or global-lexical epochs invalidate Machine code; deopt → recompile loops | Values says "nothing is invalidated"; machine says a `ShapeGuard` exit disables speculation for good. Nobody traced the epoch producers. |
| **Weak collections, ephemerons, finalization** | Row 5 is a probable soundness bug. |
| **Exceptions inside compiled code**: Machine try/catch lowering (`hir.rs:947-963`); Template exits to the interpreter at a same-frame handler | Nobody measured how often hot functions contain try, or what it costs. |
| **Generators and async/await** | Both tiers decline, so these always run in the interpreter. Resume, promise and microtask paths were not analysed. |
| **Native builtins reached from the JIT**: guarded leaf stubs (array push/pop leaves, `jit_snapshot.rs:80-83`), static-native probe, intrinsics | Only mentioned in passing. |
| **Code memory lifecycle**: one `ExecutableBuffer` per function, W^X switching cost, retirement, the 64 MiB cap | Relevant to zlib (15 MB of code) and to the ts compile loops (function 1377 compiled 15 times). |
| **RSS attribution** | No reader measured it. Verified candidates: zero-then-`MADV_FREE` pages (`compressed.rs:694-746`), promotion-driven old-space growth (rows 2–4), eager feedback boxes (row 12), about 15 MB of code, and the ArrayBuffer. Nobody produced a per-space breakdown. |
| **ESM / strict-mode cliffs** | The `TailCall` issue (row 18); module-path costs in general. |
| **x86_64 parity** | Every reader skipped it; the x86 backends are about 10k lines. |

## 5. Open items the critique could not close

1. Whether row 5 shows up at run time: allocate a WeakMap value reachable only through the map, force a growth-triggered major GC, then read the value back. This needs a test run.
2. What drives the 15 Template recompiles of ts function 1377. The feedback refresh is once-only (`jit_call.rs:1497`). The relink path in `control.rs:33-35` / `take_backedge_relink` is the likeliest candidate (*speculation*).
3. The exact `size_of` for `JsClosureBody`, `CodeBlockPropertyFeedback`, `CacheStub` and `ColdFrame`. None of them is asserted.
4. Whether realms created after bootstrap ever reach JIT prototype baking (row 19).
5. Whether any emitter reaches `STUB_JIT_INLINE_CLOSURE_UPVALUES` through a numeric stub id rather than the descriptor constant.