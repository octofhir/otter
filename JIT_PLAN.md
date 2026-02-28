# JIT Plan - OtterJS

## Context

OtterJS is a Rust-based JS/TS runtime with NaN-boxing, hidden classes (shapes), a bytecode interpreter (~78 opcodes) and a **baseline Cranelift JIT** (`otter-vm-jit`) that compiles ~28 of 78+ instructions (only arithmetic, local variables, control flow). Property access, function calls, objects, closures all bail out to the interpreter.

This plan is informed by V8 (2025-2026: Ignition -> Sparkplug -> Maglev -> Turbolev/Turboshaft) and JSC (LLInt -> Baseline -> DFG -> FTL/B3) architectures.

---

## Current JIT State

### What exists
- **Cranelift baseline JIT** (`otter-vm-jit/`): ~3,351 lines
  - `compiler.rs` -- Cranelift JIT wrapper
  - `translator.rs` -- bytecode -> Cranelift IR (28 instructions)
  - `type_guards.rs` -- NaN-boxing type checks, int32/f64 fast paths
  - `bailout.rs` -- sentinel `0x7FFC_0000_0000_0000`, DEOPT_THRESHOLD=10
  - `runtime_helpers.rs` -- 36 helper kinds (GetProp, Call, NewObject, etc.)
- **Hot function detection**: `call_count: AtomicU32`, HOT_FUNCTION_THRESHOLD=1000
- **JIT queue**: `jit_queue.rs` -- dedup, eligibility filtering
- **JIT runtime**: `jit_runtime.rs` -- execute/bailout/deopt, env vars (OTTER_DISABLE_JIT, OTTER_JIT_STATS, OTTER_JIT_EAGER)
- **FeedbackVector**: IC state (Uninitialized -> Mono -> Poly(4) -> Mega), TypeFlags, proto_epoch
- **Shape system**: transition tree, property_map, keys_ordered, transitions (RefCell)
- **Bytecode optimizations**: constant folding, peephole optimizer (copy propagation, dead code elimination, register coalescing)

### Limitations
1. **28 of 78+ opcodes** -- property access, function calls, closures, try/catch all bail out
2. **No IC-guided quickening** -- feedback is collected but doesn't affect bytecode (DONE in Phase 0.1)
3. **No speculative optimization** -- only guard-based fast paths
4. **Single tier** -- no intermediate level between interpreter and Cranelift
5. **No OSR** -- hot loops can't migrate to JIT mid-execution
6. **No background compilation** -- compilation is synchronous on main thread
7. **Runtime helpers not inlined** -- complex ops always call via extern "C"

---

## Phase 0: Interpreter Acceleration (no JIT, maximum ROI)

### Phase 0.1: Bytecode Quickening / Specialization -- DONE
- Rewrite bytecode in-place based on observed types from FeedbackVector
- `Add` -> `AddInt32Quickened` (both operands int32, no type checks)
- `GetPropConst` -> `GetPropQuickened` (IC hit -> direct offset load, skip proxy/string/array checks)
- `SetPropConst` -> `SetPropQuickened` (same for writes)
- **Files**: `instruction.rs` (new specialized opcodes), `interpreter.rs` (handlers), `function.rs` (quickening logic)
- **Estimate**: +15-25% on property-heavy code

### Phase 0.2: Superinstructions -- DONE (GetLocalProp)
- `GetLocalProp`: fused `GetLocal + GetPropConst` — single dispatch for `localObj.prop` pattern
- Peephole optimizer detects `GetLocal R, idx; GetPropConst R2, R, name, ic` → `GetLocalProp R2, idx, name, ic`
- Interpreter handler: IC fast path (mono+poly with MRU), fallback to de-fused GetPropConst
- JIT translator: read local slot → call GetPropConst helper (same bailout pattern)
- **Files**: `instruction.rs`, `peephole.rs` (fusion rule), `interpreter.rs` (handler), `translator.rs` (JIT support)
- Future candidates: `LoadInt8 + Add`, `GetLocal + Call`

### Phase 0.3: Improve IC in Interpreter -- DONE
- MRU reordering for polymorphic IC (promote most-recently-hit shape to front)
- Cache proto_epoch on VmContext (avoid atomic load per IC access)
- Polymorphic support in SetPropQuickened handler
- Enable quickening for polymorphic IC (not just monomorphic)
- **Files**: `context.rs`, `interpreter.rs`

### Phase 0.4: Interpreter Dispatch Optimization -- DONE
- Removed 22 `std::env::var()` calls from hot interpreter paths (each one: syscall + String alloc, ~100-500ns)
- Removed dead computed variables in GetPropConst (Array ctor/proto lookups on every property access)
- Removed `DUMPED_ASSERT_RT` static and `last_pc_by_frame_id` HashMap allocation per run_loop
- Removed ~500 lines of temporary debug scaffolding
- Dispatch mechanism: Rust match statement compiles to jump table via LLVM -- already optimal for the language
- Computed goto / direct threading: not applicable to Rust (no `goto` statement)
- **Files**: `interpreter.rs` (removed debug traces from hot paths)

---

## Phase 1: Expand Baseline JIT (medium ROI, medium complexity)

### 1.1: Property Access in JIT (most impactful missing feature) -- DONE
- JIT function signature changed: `fn(ctx, args_ptr, argc) -> ret` with JitContext (function_ptr + proto_epoch)
- GetPropConst/SetPropConst: emit runtime helper calls from Cranelift IR, check BAILOUT_SENTINEL, branch to bail/continue
- Runtime helpers (`jit_helpers.rs`): extract object from NaN-boxed bits, verify GC header tag == OBJECT, IC fast path (mono+poly), MRU reordering
- SetPropConst bails on heap-typed write values (can't reconstruct HeapRef safely without GC)
- `can_translate_function_with_helpers()` -- extended eligibility for JIT queue
- **Files**: `translator.rs`, `compiler.rs`, `runtime_helpers.rs` (now public), `jit_helpers.rs` (new), `jit_runtime.rs`, `jit_queue.rs`, `interpreter.rs`

### 1.2: Function Calls in JIT -- DONE
- JitContext expanded with interpreter/vm_ctx pointers for re-entrant calls
- `Value::from_raw_bits_unchecked()` — reconstructs full Value (with HeapRef) from NaN-boxed bits via GC header tag
- `otter_rt_call_function` helper: reconstructs callee + args → `interpreter.call_function()` → result bits
- Translator emits Call: builds argv on Cranelift stack slot, calls helper, checks BAILOUT_SENTINEL
- Handles native functions, closures, bound functions, proxies (via interpreter dispatch)
- **Files**: `value.rs`, `jit_helpers.rs`, `jit_runtime.rs`, `translator.rs`, `interpreter.rs`

### 1.3: Object/Array Creation in JIT -- DONE
- NewObject, NewArray via runtime helpers (don't inline, too complex)
- But allow JIT to not bail out on these instructions
- Runtime helpers: `otter_rt_new_object` (creates with Object.prototype), `otter_rt_new_array` (creates with Array.prototype)
- **Files**: `translator.rs` (translation handlers), `jit_helpers.rs` (runtime helpers), `runtime_helpers.rs` (HelperKind enum)

### 1.4: Closure Support
- Support functions with upvalues (GetUpvalue, SetUpvalue)
- Remove "no closures" restriction from `can_translate_function()`
- **Files**: `translator.rs`, `compiler.rs` (context pointer passing)

### 1.5: Global Variable Access in JIT -- DONE
- JitContext expanded with `constants: *const ConstantPool` for name resolution
- `otter_rt_get_global` helper: IC fast path on global object shape, fallback to `get_global_utf16`
- `otter_rt_set_global` helper: sets property on global object via `PropertyKey`
- Both bail to interpreter on error (ReferenceError, strict mode violations)
- Quickened arithmetic variants (AddInt32, SubInt32, MulInt32, DivInt32, AddNumber, SubNumber) also added to JIT
- **Files**: `jit_helpers.rs`, `translator.rs`, `jit_runtime.rs`, `interpreter.rs`

---

## Phase 2: Speculative Optimization in Cranelift JIT (high complexity, high ROI)

### 2.1: Type-specialized Compilation via FeedbackVector -- DONE
- Snapshot FeedbackVector at compile time, derive `SpecializationHint` per instruction
- `Int32` hint: emit i32-only guard (1 branch). `Float64`: f64-only guard. `Numeric`: full i32+f64 cascade. `Generic`: immediate bailout
- Add/Sub/Mul use `emit_specialized_arith` dispatcher. Div uses Float64 or Numeric only (JS div returns f64 on zero/non-exact)
- Quickened arithmetic variants (AddInt32, SubInt32, MulInt32, DivInt32, AddNumber, SubNumber) also supported in JIT
- **Files**: `translator.rs` (feedback snapshot + specialized dispatch), `type_guards.rs` (emit_specialized_arith/cmp dispatchers)

### 2.2: Improved Deoptimization
- Current: bail from entire function, re-execute from scratch in interpreter
- Goal: per-instruction deopt points (like V8 eager deopt), resume in interpreter at PC + frame state
- **Prerequisite**: Frame layout compatibility between JIT and interpreter (like Sparkplug)
- **Files**: `bailout.rs`, `jit_runtime.rs`, `interpreter.rs`

### 2.3: Monotonic Feedback Lattice (following V8's approach) -- DONE
- `BailoutAction` enum: `Continue | Recompile | PermanentDeopt`
- After DEOPT_THRESHOLD bailouts, JIT code invalidated and function re-enqueued for recompilation
- Feedback vector retains wider type observations across recompilations (monotonic)
- After MAX_RECOMPILATIONS (3) cycles, permanently deoptimized (prevents deopt loops)
- `needs_recompilation()` helper on Function for re-enqueue detection
- **Files**: `function.rs` (BailoutAction, recompilation_count, MAX_RECOMPILATIONS), `jit_runtime.rs` (recompile flow), `interpreter.rs` (NeedsRecompilation handling at 4 call sites)

---

## Phase 3: OSR and Background Compilation (high complexity)

### 3.1: On-Stack Replacement
- Hot loops: counter check on loop back-edge
- At threshold -> compile + replace frame
- **Prerequisite**: compatible frame layout (Phase 2.2)
- **Files**: `interpreter.rs` (back-edge counter), `jit_runtime.rs` (OSR entry), `translator.rs` (OSR-aware compilation)

### 3.2: Background JIT Compilation
- Move Cranelift compilation to background thread
- Snapshot FeedbackVector + constants at enqueue
- Result: code pointer atomically published
- **Files**: `jit_queue.rs` (thread pool), `jit_runtime.rs` (async publication), `compiler.rs`

---

## Phase 4: Mid-Tier Optimizing Compiler (long-term, optional)

### 4.1: Maglev-style SSA Compiler
- SSA IR on top of Cranelift (or custom lightweight IR)
- Specialize at graph building time from FeedbackVector
- Linear-scan register allocator
- **When**: Only if baseline JIT + quickening is insufficient
- **Alternative**: Use Cranelift's built-in optimization passes (already SSA, has regalloc)

### 4.2: Inlining
- Inline hot monomorphic callees
- Most powerful single optimization for JS (V8: inlining gives 2-5x on microbenchmarks)
- **Prerequisite**: SSA IR + speculative optimization

---

## Recommended Sequence

```
Now (max ROI, min effort):
  -> Phase 0.1: Bytecode Quickening (+15-25%) -- DONE
  -> Phase 0.3: Improve IC in interpreter -- DONE
  -> Phase 0.4: Dispatch optimization -- DONE

Next quarter:
  -> Phase 1.1: Property access in JIT -- DONE
  -> Phase 1.2: Function calls in JIT -- DONE
  -> Phase 0.2: Superinstructions (GetLocalProp) -- DONE
  -> Phase 2.3: Monotonic feedback lattice -- DONE

Medium term (3-6 months):
  -> Phase 1.3: Object/Array creation in JIT -- DONE
  -> Phase 2.1: Type-specialized compilation -- DONE
  -> Phase 2.2: Per-instruction deoptimization
  -> Phase 1.4: Closure support in JIT
  -> Phase 3.2: Background compilation

Long term (6+ months, if needed):
  -> Phase 3.1: OSR
  -> Phase 4: Mid-tier optimizing compiler + inlining
```

---

## Verification

### How to measure progress
```bash
# Microbenchmarks (arithmetic, property access, function calls)
cargo bench -p otter-vm-core

# JIT statistics
OTTER_JIT_STATS=1 cargo run -p otterjs -- run examples/bench_loop.js

# Existing JIT benchmarks
cargo run -p otterjs -- run examples/bench_jit_overhead.js
cargo run -p otterjs -- run examples/bench_pure_loop.js

# Test262 conformance (should not degrade)
just test262

# Profiling
cargo build --release -p otterjs && hyperfine './target/release/otterjs run benchmark.js'
```

### Per-change protocol
1. Measure baseline performance (before)
2. Implement the change
3. `cargo test --all --all-features` -- don't break conformance
4. Measure after performance
5. Report: delta in % on specific benchmark

---

## References

- [V8: Land ahoy -- leaving the Sea of Nodes](https://v8.dev/blog/leaving-the-sea-of-nodes)
- [V8: Maglev -- V8's Fastest Optimizing JIT](https://v8.dev/blog/maglev)
- [V8: Sparkplug -- a non-optimizing JavaScript compiler](https://v8.dev/blog/sparkplug)
- [Turbolev -- new top tier JIT compiler in V8](https://blog.seokho.dev/development/2025/07/15/V8-Expanding-To-Turbolev.html)
- [Profile-Guided Tiering in V8 (Intel)](https://community.intel.com/t5/Blogs/Tech-Innovation/Client/Profile-Guided-Tiering-in-the-V8-JavaScript-Engine/post/1679340)
- [JSC Documentation (WebKit)](https://docs.webkit.org/Deep%20Dive/JSC/JavaScriptCore.html)
- [Speculation in JavaScriptCore](https://webkit.org/blog/10308/speculation-in-javascriptcore/)
- [JSC Internals: LLInt and Baseline JIT](https://zon8.re/posts/jsc-internals-part2-the-llint-and-baseline-jit/)
- [JSC Internals: DFG JIT Graph Building](https://zon8.re/posts/jsc-part3-the-dfg-jit-graph-building/)
- [B3 JIT Compiler (WebKit)](https://webkit.org/blog/5852/introducing-the-b3-jit-compiler/)
- [Copy-and-Patch Compilation (Stanford)](https://dl.acm.org/doi/abs/10.1145/3485513)
- [Cranelift](https://cranelift.dev/)
- [JavaScript engine fundamentals: Shapes and Inline Caches](https://mathiasbynens.be/notes/shapes-ics)
- [Speculative Optimization in V8 (Benedikt Meurer)](https://benediktmeurer.de/2017/12/13/an-introduction-to-speculative-optimization-in-v8/)
