# Otter Architecture Audit — 2026-05

Audit scope: active `crates/*` only. Parked `crates-legacy/*` excluded per
AGENTS.md §Repository Layout.

## Executive summary

1. **Value representation is the single biggest architectural mistake.**
   `otter_vm::Value` is a `#[derive(Debug, Clone)] pub enum` with **30+
   variants** (`crates/otter-vm/src/lib.rs:217-401`). Layout is a tagged-union
   ≥24 bytes per Value (worst variant `Closure` carries
   `Rc<[UpvalueCell]> + Option<Box<Value>>` = 32 bytes). Boa defaults to
   NaN-boxing in 8 bytes (`/tmp/boa/core/engine/src/value/inner/nan_boxed.rs:147-174`),
   with the enum form behind opt-in feature `jsvalue-enum`. V8 SMIs fit in
   a tagged 32-bit slot; JSC `JSValue` is 8 bytes. **Cache footprint of a
   single register is 3× Boa, ~3-4× V8/JSC.** MEMORY.md describes a prior
   8-byte NaN-boxed Value model that is no longer present — the current
   crates-next stack has regressed on value layout. **Severity: blocker for
   any "competitive with Bun" perf target (ROADMAP §P).**
2. **Bytecode is not bytecode.** `otter_bytecode::Instruction` is a
   serde-Serialize struct holding `pc: u32, op: Op, operands: OperandList`
   (`crates/otter-bytecode/src/lib.rs:1379-1417`); the VM-internal
   `ExecInstr` (`crates/otter-vm/src/executable.rs:248-261`) is still a
   ~32-byte struct (`op: Op, operand_len: u8, inline_operands: [Operand;3],
   side_start: u32, property_ic_site: u32`). Boa stores opcodes as `Vec<u8>`
   and dispatches through `OPCODE_HANDLERS_BUDGET[opcode as usize]`
   (`/tmp/boa/core/engine/src/vm/mod.rs:956-971`). V8 Ignition uses 1–4
   byte instructions with prefix bytes for wider operands. Otter's
   instruction stream is 4–8× larger than necessary, kills i-cache density,
   and the giant single-`match` dispatch in `dispatch_loop_inner`
   (`crates/otter-vm/src/lib.rs:3974, 4350, 5254, 5320`) forfeits
   threaded-code / computed-goto throughput. **Severity: blocker for steady-
   state interpreter speed.**
3. **Inline caches are crippled by design.** `property_ic.rs` defines only
   `PropertyIcEntry::{Empty, Monomorphic, Disabled}`
   (`crates/otter-vm/src/property_ic.rs:117-131`), with
   `MONOMORPHIC_MISS_DISABLE_THRESHOLD = 4`
   (`crates/otter-vm/src/property_ic.rs:36`). After four shape misses the
   site is **permanently** disabled — no polymorphic table, no megamorphic
   fallback stub. ROADMAP P1 is checked `[x]` and labelled "polymorphic IC:
   4 shapes, fallback to megamorphic probe" — **the ROADMAP claim is
   false**. V8/JSC carry 4-entry PIC + megamorphic probe; Boa wires a real
   IC table (`/tmp/boa/core/engine/src/vm/inline_cache`).
   **Severity: blocker for any non-toy JS workload (idiomatic JS hits 2-4
   shapes within tens of allocations).**
4. **CallFrame is fat and cold.** `Frame` (`crates/otter-vm/src/frame_state.rs:35-160`)
   carries 20+ fields including `SmallVec<[Value;8]> registers`,
   `Rc<[UpvalueCell]> upvalues`, `Rc<str> module_url`, 5 `Option<Pending*>`
   protocol state slots, and Async/generator/handler vectors inline. Every
   `Call` pushes one of these; `pop_frame` drops it. Cold and hot data live
   together — every interpreter mutator touches a ~250–400 byte frame for
   the simplest call. V8 frames are ~10–12 pointer slots plus a register
   window in a shared stack; JSC similarly. VM_REFACTOR_PLAN already calls
   this out as P1 ("Split hot frame state from cold side records, move
   register storage toward a contiguous VM value stack"). **Severity:
   major; blocks the cold-start <20ms target (ROADMAP P10).**
5. **GC is the strongest subsystem.** `otter-gc` already provides
   pointer-compressed `Gc<T>` (`u32` offset in a 4 GiB cage,
   `crates/otter-gc/src/lib.rs:43-47`), generational Cheney scavenger,
   tri-color mark-sweep, Dijkstra insertion barrier, incremental marking
   driver (`crates/otter-gc/src/heap.rs:929-1017`), ephemerons for WeakMap,
   external-memory accounting, RAII handle scopes, DevTools `.heapsnapshot`
   export. This is V8 Orinoco / JSC Riptide shape. Boa's `boa_gc` is a
   simpler tracing collector. **Otter's GC is ahead of Boa and competitive
   with the design intent of V8 ca. 2018.** It is also the one place
   `unsafe` is concentrated (per AGENTS.md, only `crates/otter-gc` and
   `crates/otter-modules/src/ffi.rs` carry `unsafe`; verified by grep).
6. **Macro surface is dead code today.** `otter-macros` defines
   `js_namespace`, `js_class`, `raft`
   (`crates/otter-macros/src/lib.rs:61, 167, 443`), but no `crates/otter-vm`,
   `crates/otter-runtime`, `crates/otter-modules`, or `crates/otter-web`
   source file uses any of them. Only doc-comments mention the macros
   (`crates/otter-vm/src/runtime_cx.rs:7-8, 140-141`). Builtins are
   installed through `BuiltinIntrinsic` trait + bootstrap thunks
   (`crates/otter-vm/src/intrinsic_install.rs:52-72`,
   `crates/otter-vm/src/bootstrap.rs:184-369`). **Either drop the macros,
   or finish wiring them — but don't carry an unused, undocumented API
   surface.** AGENTS.md still lists them as the recommended path.
7. **VM kitchen sink: `otter-vm/src/lib.rs` is 9 791 lines.** It contains
   `Value`, helper traits, dispatch loops, tests, and async glue. AGENTS.md
   §"Module Size And Boundary Hygiene" explicitly forbids this pattern.
   The crate also contains 105 865 LoC, much of it inside per-builtin
   `bootstrap_*.rs` files. Most files are *fine*; `lib.rs` is the
   outlier and the canonical hard-to-review bottleneck. **Severity: major
   for maintainability; minor for correctness.**
8. **Spec correctness: no published full-suite Test262 baseline.**
   `ES_CONFORMANCE.md` (lines 6-33) records a 2026-05-07 snapshot but
   explicitly states "No full Test262 run has been captured in this
   checkout yet." MEMORY.md cites historic per-suite numbers (Annex B
   100%, Generator 38–40%, etc.) but these refer to a different VM stack
   (the parked `crates-next` stage). **There is no end-to-end number for
   the active stack today.** ROADMAP E10 target is 95% language / 90%
   built-ins. Without a baseline there is no gating signal.
9. **Capability security looks correct at the boundary.** Module loader,
   filesystem, net, env, and FFI gates are enforced in
   `crates/otter-runtime` and `crates/otter-modules`; `unsafe` is bounded;
   no obvious leak path. The dynamic-import path roots `import_meta`
   through host-rooted helpers (per VM_REFACTOR_PLAN). FFI is gated by
   `--allow-ffi` (`crates/otter-modules/src/ffi.rs:158`). **Severity:
   minor — full audit still needs structured fuzz harness (ROADMAP S10).**
10. **JIT is absent.** The codebase has *no* JIT, *no* baseline tier, *no*
    code cache. ROADMAP J track is entirely `[ ]`. This is reasonable for
    the current product stage (interpreter-first), but ROADMAP P track
    ("steady-state interp ≥ 300 Mops/s, JIT ≥ 1.5 Gops/s") cannot be hit
    on the current interpreter design (see §1, §2, §3).

**Recommendation: partial-refactor of value model, bytecode encoding, and
ICs before any further intrinsic work. The current foundation will not
support the ROADMAP P, J, or E10 targets.** Specific plan in §"Refactor
plan" below.

---

## 1. Value & object model

### 1.1 What is there

- `Value` is a tagged enum, `crates/otter-vm/src/lib.rs:216-401`. 30+
  variants spanning primitives + 18+ heap types (`String`, `Symbol`,
  `Function {function_id}`, `Object`, `Array`, `Closure {function_id,
  upvalues: Rc<[UpvalueCell]>, bound_this: Option<Box<Value>>}`,
  `BoundFunction`, `NativeFunction`, `Iterator`, `RegExp`, `Promise`,
  `Map`, `Set`, `WeakMap`, `WeakSet`, `WeakRef`, `FinalizationRegistry`,
  `Temporal`, `Intl`, `ArrayBuffer`, `DataView`, `TypedArray`,
  `Generator`, `Proxy`, `ClassConstructor`).
- `NumberValue` is `enum { Smi(i32), Double(f64) }`
  (`crates/otter-vm/src/number/mod.rs:66-71`).
- `JsString` is `Arc<StringRepr>` with `Flat / Cons / Sliced / Thin`
  variants (`crates/otter-vm/src/string/mod.rs:195-232`).
- `JsObject` is `#[repr(transparent)] Gc<ObjectBody>` (4-byte offset into
  the GC cage, per `crates/otter-vm/src/object.rs:18-21`).
- Shapes: GC-allocated `ShapeBody` nodes with parent pointer + transition
  key (`crates/otter-vm/src/object/shape_body.rs:46-58`); transition table
  + own-offset cache live outside the GC body (`shape_runtime.rs`,
  `shape_cache.rs`, `shape_transition.rs`). ECMA-262 §10 ordinary object
  semantics are honoured (`object.rs` invariants comment lines 40-56).

### 1.2 Comparison

| Engine | Value size | Tag scheme | Notes |
|---|---|---|---|
| Otter (today) | ≥24 B, prob. 32 B | Rust tagged enum | `Clone` not `Copy`; heap refs use `Rc` for Closure |
| Boa (default) | 8 B | NaN-box | Top 4-bit tag inside NaN payload; opt-in legacy enum form |
| V8 | 4 B (SMI) / 8 B (handle) | LSB-tagged SMI + Handle indirection | Compressed-pointer 4 B on `--shared-string-table`/cage builds |
| JSC | 8 B | NaN-box on 64-bit; tag bits in HIGH word | "PureValue" + structure ID |
| LuaJIT | 8 B | NaN-box | Reference design for compact VMs |

The Value enum approach is **objectively worse** than NaN-boxing for an
embedded interpreter — every register slot is 3× larger, every clone
copies a discriminant + variant data, and every `match` carries a
mispredictable branch tree.

### 1.3 Problems

| # | Issue | Severity | Location |
|---|---|---|---|
| 1.A | `Value` is ≥24 B and `Clone` (not `Copy`). Every interpreter register copy is a heap-touching memcpy. | Blocker | `crates/otter-vm/src/lib.rs:216` |
| 1.B | `Closure` variant holds `Rc<[UpvalueCell]>` — every function value copy is an atomic refcount. | Major | `crates/otter-vm/src/lib.rs:259-282` |
| 1.C | `Value::Hole` sentinel lives in the same enum as `Value::Undefined`, leaking internal state into the type system. Comment admits "User code never observes this variant" — model issue, not bug. | Minor | `crates/otter-vm/src/lib.rs:220-235` |
| 1.D | MEMORY.md describes a `Value(u64)` NaN-box with sub-tags `0x7FFC..0x7FFF` — that model is **not present** in the current code. Either MEMORY.md is stale or there was a regression. | Major (process) | n/a |
| 1.E | `NumberValue::Smi(i32)` canonicalises through `f64.fract() == 0.0` (`number/mod.rs:92-108`). Correct, but every arithmetic op re-canonicalises. Boa's `Integer32 ↔ Float64` IC-feedback path skips this. | Minor | `crates/otter-vm/src/number/mod.rs:73-108` |
| 1.F | Shape lookup walks parent chain (`shape_offset_of_str`, `shape_body.rs:184-196`) — O(N) per uncached lookup. There's an `own_offset_map` cache in `shape_runtime` for hot sites but the walk-up version exists and is called from object helpers without a runtime borrow. | Major | `crates/otter-vm/src/object/shape_body.rs:184-196` |

### 1.4 Recommendations

- **P0**: Move `Value` to NaN-boxing (`#[repr(transparent)] Value(u64)`,
  `Copy`). Pointer sub-tags as MEMORY.md already describes (
  `TAG_PTR_OBJECT=0x7FFC`, `TAG_PTR_STRING=0x7FFD`, `TAG_PTR_FUNCTION
  =0x7FFE`, `TAG_PTR_OTHER=0x7FFF`). Use `GcHeader::tag()` for type
  discrimination on `TAG_PTR_OTHER`. **This is not abstraction for its
  own sake — it is the single largest perf lever in the interp.** Boa
  ships this opt-out today; Otter shipped it once and lost it.
- **P0**: Eliminate `Rc<[UpvalueCell]>` from `Closure`. Store closures
  through a GC body that owns the upvalue array (the same way
  `ClassConstructorBody` works at `lib.rs:411-425`). Refcount overhead is
  observable in any function-heavy benchmark.
- **P1**: Extract `Value::Hole` into a typed `ArrayElement` wrapper used
  only inside `JsArray::elements`. Top-level `Value` should not carry an
  unobservable variant.

---

## 2. Bytecode & interpreter

### 2.1 What is there

- `Op` is a single Rust enum of all opcodes
  (`crates/otter-bytecode/src/lib.rs:50-1376`, ~1300 lines of opcode
  doc-comments). Includes `Op::JsonCall`, `Op::PromiseCall`,
  `Op::MathCall`, `Op::CallMethodValue` — variadic, by-name dispatch ops
  that *replace* the spec's normal property-load → call sequence with a
  bytecode shortcut.
- `Instruction = (pc: u32, op: Op, operands: OperandList)`,
  `OperandList = Inline{len:u8, ops:[Operand;3]} | Spill(Box<[Operand]>)`
  (`crates/otter-bytecode/src/lib.rs:1379-1497`). `Operand = Register(u16)
  | ConstIndex(u32) | Imm32(i32)` — each operand is 8 bytes (u8 tag + 4
  data + 3 pad).
- `ExecInstr` (`crates/otter-vm/src/executable.rs:248-294`) re-encodes the
  same shape: `op: Op (1B) + operand_len (1B) + inline_operands ([Operand;3]
  = 24B) + side_start (4B) + property_ic_site (4B)` ≈ 34 B per instruction.
- Dispatch lives in `Interpreter::dispatch_loop_inner`
  (`crates/otter-vm/src/lib.rs:3935-5320+`), three large `match op {}`
  blocks (lines 3974, 4350, 5254, 5320) that handle stack-modifying,
  in-frame, and pending-state instructions separately.
- IC: `LoadPropertyIc`, `StorePropertyIc`, `HasPropertyIc` — monomorphic
  only, disable after 4 misses (`property_ic.rs:36, 117-131`).
- Per-instruction work: 5+ `Option::ok_or` operand decodes, then the work
  (`read_register`, `write_register`, etc., e.g. `lib.rs:4400-4427`).

### 2.2 Comparison

| Engine | Instruction encoding | Dispatch | IC tiers |
|---|---|---|---|
| Otter | `Vec<ExecInstr>` ~34 B/insn | Three nested `match op {}` | Monomorphic only, disable after 4 misses |
| Boa | `Vec<u8>` byte-encoded, varying operands | `OPCODE_HANDLERS_BUDGET[opcode]` fn-pointer table (`/tmp/boa/core/engine/src/vm/mod.rs:956-971`) | Real PIC in `vm/inline_cache` |
| V8 Ignition | 1-4 B/insn with `Wide` / `ExtraWide` prefix bytes | Computed-goto (release) / fn-table (debug) | 4-shape PIC + megamorphic |
| JSC LLInt | LLInt assembly, hand-written threaded code | Threaded code via offlineasm | Polymorphic IC + structure chain caches |
| LuaJIT (interp) | 4-byte fixed | Computed-goto | n/a |

Otter is **at minimum 4× larger** per instruction than Boa, **at minimum
10×** larger than V8 Ignition. The dispatch loop's three-tier `match`
forfeits branch-predictor friendliness because each opcode hits three
match arms (stack-modifying check, primary handler, pending-state guard)
per tick. Computed-goto on stable Rust requires the `core::hint` family
or a tail-call optimisation pattern not yet stable; on current Rust the
fn-pointer table (Boa's approach) is the practical optimum.

### 2.3 Problems

| # | Issue | Severity | Location |
|---|---|---|---|
| 2.A | `Vec<ExecInstr>` ~34 B/insn is the wrong order of magnitude. i-cache + d-cache pressure dominate; data shows up immediately on Octane/Sunspider-class workloads. | Blocker | `crates/otter-vm/src/executable.rs:248-261` |
| 2.B | Three sequential `match op` blocks in the same dispatch loop iteration. Each tick visits 1 of N arms per match → 3× branch misprediction surface. | Major | `crates/otter-vm/src/lib.rs:3974, 4350, 5254` |
| 2.C | Monomorphic-only IC with hard disable. Real-world JS sites (idiomatic React/Express code) often see 2-3 shapes — Otter permanently disables them. | Blocker | `crates/otter-vm/src/property_ic.rs:36` |
| 2.D | ROADMAP P1 ("polymorphic IC: 4 shapes, fallback to megamorphic probe") is marked `[x]`. **This is false** — the code is monomorphic. Either the table is misclassified or the work was rolled back without removing the mark. | Major (process) | `ROADMAP.md:73` |
| 2.E | Bytecode contains `Op::JsonCall` / `Op::MathCall` / `Op::PromiseCall` — by-name shortcut opcodes that skip spec property lookup. Compiler-side recognition of literal `JSON.<name>(...)` shape (per the doc comment at `lib.rs:86-93`). This works as long as no one shadows `JSON` in the source — **but the compiler has to be perfect about this or it diverges from spec.** Confirm by inspection in `crates/otter-compiler/src/builtins_call.rs`. | Major (spec) | `crates/otter-bytecode/src/lib.rs:86-93` |
| 2.F | No `Op::Wide` / `Op::ExtraWide` prefix — Otter's u16 register index limits frames to 65 535 registers (fine) but the constant-pool u32 limit kills programs with >4 B constants (fine in practice). Still: a packed bytecode lets the operand width go to 8 / 16 / 32 bits on demand and matters for code size. | Minor | `crates/otter-bytecode/src/lib.rs:1391-1398` |

### 2.4 Recommendations

- **P0**: Replace `Vec<ExecInstr>` with `Vec<u8>` + per-opcode argument
  layouts. Generate handler functions via a macro (the Boa pattern,
  `opcode/mod.rs:557-2200`). Decode-then-dispatch at the loop head;
  individual handlers know their argument layout. *Realistic delta:
  3-5× steady-state interp throughput, plus a real path to JIT later.*
- **P0**: Promote IC to **polymorphic-4 + megamorphic probe**. Match the
  ROADMAP's own claim. Implementation pattern is well-known (V8's
  `FeedbackVector` + `LoadIC_Megamorphic` stub) and the existing
  `property_ic.rs` is the right place to extend.
- **P1**: Collapse the three-tier `match` dispatch into one. Move
  "stack-modifying" opcodes (`Return`, `Call`, `Throw`, etc.) to a normal
  arm that returns a typed `DispatchAction`; let the loop top advance pc
  or pop a frame based on the action. MEMORY.md describes a previous
  attempt to do exactly this on the parked stack (`Phase 1.2 (Flatten
  InstructionResult)`). **Carry that work forward.**
- **P2**: Investigate computed-goto via tail calls or `become` (when
  stable). Until then, the fn-pointer table buys 1.3-1.7× over the
  match.

---

## 3. Compiler frontend

### 3.1 What is there

- `otter-compiler/src/lib.rs` is a façade over a per-syntactic-form
  module tree (`assignment.rs`, `calls.rs`, `expr/`, `for_loops.rs`,
  `functions.rs`, `class/`, `destructuring.rs`,
  `hoist.rs`, `strict_validation.rs`, `try_catch.rs`, `ts_erasure.rs`).
- Entry points `compile_script_program`, `compile_module_program`
  (`crates/otter-compiler/src/entry.rs:76, 248, 677`). Uses oxc AST
  borrowed from `otter-syntax`.
- No IR between AST and bytecode — single-pass lowering. `Compiler`
  carries a stack of `FunctionContext` (`crates/otter-compiler/src/compiler.rs:23-35`)
  with private-name namespace tracking.
- Scope analysis + capture analysis live in
  `capture.rs` and `scope.rs`; hoisting in `hoist.rs`.
- Strict-mode early errors in `strict_validation.rs` (1252 lines — fine,
  spec-driven).
- Internal use of `Rc<RefCell<ModuleBuilder>>` for module-level state
  (`entry.rs:90`). AGENTS.md §"Module Size And Boundary Hygiene" notes
  this is "legacy implementation detail; do not expand it"; OK.

### 3.2 Comparison

- Boa has a similar single-pass `ByteCompiler`
  (`/tmp/boa/core/engine/src/bytecompiler/mod.rs:495`) plus an `optimizer/`
  pass over a separate IR. V8/JSC have a real graph IR (TurboFan
  sea-of-nodes; B3) used only in the optimising tier — the baseline
  compiler is single-pass like Boa and Otter.
- No JIT, so no need for an IR today.

### 3.3 Problems

| # | Issue | Severity | Location |
|---|---|---|---|
| 3.A | Single-pass AST→bytecode with no IR. **For an interp-only foundation this is fine and matches Boa/V8 baseline.** Limits future JIT, but that's deferred. | Minor (design) | `crates/otter-compiler/src/entry.rs:76-180` |
| 3.B | `assignment.rs`, `destructuring.rs`, `try_catch.rs` are each <1500 LoC — sized appropriately. `strict_validation.rs` at 1252 LoC is large but spec-bound. **Largest non-test file is `statements.rs` at 941 LoC.** Compiler is the most consistently well-modularised crate in the codebase. | n/a | n/a |
| 3.C | No source-map emission inside the compiler — span entries are attached to instructions and surfaced via `SpanEntry` (`crates/otter-bytecode/src/lib.rs:1579-1585`), but the V3 source-map generation lives in `otter-runtime`. Fine — separation of concerns. | n/a | n/a |
| 3.D | Compiler-side `JsonCall`/`MathCall`/`PromiseCall` shortcut emission (cross-ref §2.E) bypasses normal property lookup. Risk: if user shadows `JSON` / `Math` / `Promise` in the script scope and then writes `JSON.parse(x)`, does the compiler emit `JsonCall` or normal `LoadProperty/Call`? Audit `builtins_call.rs`. | Major (spec) | `crates/otter-compiler/src/builtins_call.rs` |

### 3.4 Recommendations

- **P1**: Verify §3.D by reading `builtins_call.rs` + writing one Test262
  fixture covering `let JSON = { parse(x){ return 42 } }; JSON.parse("x")`.
  If shadowing is honoured, the shortcut opcodes are sound. If not,
  remove the shortcut.
- **P2**: Keep single-pass for now; revisit when a JIT enters scope
  (track J in ROADMAP).

---

## 4. GC

### 4.1 What is there

- `otter-gc/src/lib.rs:1-129`: pointer-compressed `Gc<T>` (4 GiB cage,
  u32 offsets), 256 KiB pages, card table, generational nursery + old
  gen, ephemeron support, write barrier (generational + Dijkstra
  insertion), incremental marking driver
  (`heap.rs:929 start_incremental_mark_phase`,
  `heap.rs:997 incremental_mark_step`,
  `heap.rs:1017 finish_incremental_mark_phase`), Cheney scavenger
  (`scavenger.rs`), RAII handle scopes, external-memory accounting,
  finalize/weak-ref bookkeeping, Chrome `.heapsnapshot` writer.
- Per-Traceable `TYPE_TAG`; `trace.rs` defines the trait + safe/unsafe
  contracts. `unsafe_code` is forbidden everywhere else.
- `branded::GcSession` + `Root<T>` + `Weak<T>` provide branded handle
  rooting (`crates/otter-gc/src/branded.rs`).
- VM_REFACTOR_PLAN tracks the rooting contracts in detail — root contract
  is rigorously enforced (no public rootless allocators in production
  code; computed `LoadElement`/`StoreElement` is the documented remaining
  bypass).

### 4.2 Comparison

| Engine | GC shape | Otter status |
|---|---|---|
| V8 Orinoco | Parallel mark, concurrent mark, parallel evacuation, generational | Otter has gen + incremental mark, no parallel/concurrent yet |
| JSC Riptide | Concurrent mark, copying eden, mark-sweep tenured | Comparable design; Otter lacks concurrent mark |
| Boa boa_gc | Simple tracing collector | Otter is **ahead** |
| `gc` crate | Reference counting + tracing | Otter is ahead |

### 4.3 Problems

| # | Issue | Severity | Location |
|---|---|---|---|
| 4.A | No parallel mark / concurrent mark (per ROADMAP G2-G4). Sub-10ms pauses at 1 GB heap (G goal) not yet achievable. | Major (perf, not correctness) | `crates/otter-gc/src/heap.rs:929-1017` (incremental driver hooks in place) |
| 4.B | `forbid(unsafe_code)` is documented on every non-GC, non-FFI crate (AGENTS.md), **but not enforced via `#![forbid(unsafe_code)]` attribute** in any of `otter-vm/src/lib.rs`, `otter-runtime/src/lib.rs`, `otter-compiler/src/lib.rs`, `otter-bytecode/src/lib.rs`. Single audit grep would catch a regression. | Minor | `crates/{otter-vm,otter-runtime,otter-compiler,otter-bytecode}/src/lib.rs` headers |
| 4.C | Frame tracing path: every `Frame` is traced through `trace_frame_slots`; the 5 `Option<Pending*>` slots each carry GC-bearing payloads. Tracing cost is correlated with frame fat-ness (§5.4). | Major | `crates/otter-vm/src/frame_state.rs` |

### 4.4 Recommendations

- **P1**: Add `#![forbid(unsafe_code)]` to the 4 listed crates so the
  documented invariant is mechanically enforced. Trivial change, blocks
  whole class of regressions.
- **P2**: Parallel/concurrent mark (ROADMAP G3) is a real engineering
  project; the incremental driver shape suggests the team is aware. Not
  needed for the next 6 months unless workloads exceed ~256 MB live set.

---

## 5. Runtime & intrinsics

### 5.1 What is there

- `otter-runtime` exposes `Otter` (Layer A) + `Runtime`/`RuntimeBuilder`
  (Layer B) (`crates/otter-runtime/src/lib.rs:1-100`).
- Bootstrap: deterministic `BOOTSTRAP_ENTRIES` static slice
  (`crates/otter-vm/src/bootstrap.rs:369-450`); each entry runs a
  function-pointer installer. Some intrinsics use the trait
  (`BuiltinIntrinsic`, `intrinsic_install.rs:52`); others are still
  bespoke `install_array`, `install_number`, `install_object`, etc.
- Module loader (`module_loader.rs`, 1507 lines): supports `file://`,
  `node:`, `https://`, with capability gates; oxc_resolver-based
  resolution.
- Event loop: Tokio handle owned by runtime, microtasks drained via
  `interp.drain_microtasks_with_default` after every macrotask
  (`crates/otter-runtime/src/lib.rs:1469`).
- Promise registry, structured clone (parked-stack-safe), worker stub
  (`crates/otter-runtime/src/worker.rs:533` LoC).
- Web APIs sit in `otter-web` (1062 LoC) — small. URL is the only
  shipped Web API (ROADMAP W1).
- Console, timers, microtask queue all on the VM side
  (`crates/otter-vm/src/console.rs, timers.rs, microtask.rs`).

### 5.2 Comparison

- Bun: JSC + Zig runtime layer; intrinsics installed as native bindings.
- Node: V8 + libuv; large surface area, Node-specific modules (e.g.
  `node:fs`) not fully ECMAScript.
- Deno: V8 + Rust runtime + tokio event loop; capability model close to
  Otter's design.
- Boa runtime: smaller, `boa_runtime` crate provides timers, fetch.
- Otter sits **closest in shape to Deno**: tokio event loop + capability
  gates + URL/fetch-style Web APIs.

### 5.3 Problems

| # | Issue | Severity | Location |
|---|---|---|---|
| 5.A | Two intrinsic installation styles coexist: the `BuiltinIntrinsic` trait + bootstrap_entry! macro **and** the bespoke `install_*` function pointers. Half-migration. | Major (DX) | `crates/otter-vm/src/bootstrap.rs:369-2480` + `intrinsic_install.rs` |
| 5.B | `crates/otter-vm/src/bootstrap.rs` is 3938 LoC. AGENTS.md forbids this. Per-class intrinsics (Array, Number, Object, Date, …) all live in one file as `install_<thing>` functions. | Major | `crates/otter-vm/src/bootstrap.rs` |
| 5.C | `otter-macros` defines `js_namespace`, `js_class`, `raft` but **no production caller exists** (grep'd against `crates/`). The macros are documented as the recommended path in AGENTS.md §Macros. | Major (DX) | `crates/otter-macros/src/lib.rs` |
| 5.D | Module loader is 1507 LoC; mixes node-style resolution, file://, https://, and the package-graph DTO. Boundaries are clear in the doc-comments but a future re-split (loader vs resolver vs cache) is plausible. | Minor | `crates/otter-runtime/src/module_loader.rs` |
| 5.E | Microtask draining policy: drained inside `run_script` / `run_module` / timer fires (`lib.rs:1469, 1923, 2081, 2127`, `interp.drain_microtasks_with_default`). Spec §HostEnqueuePromiseJob expects draining *between* macrotasks; current draining looks correct. Verify by running Test262 `built-ins/Promise` suite once the runner is stable. | Major (spec) | `crates/otter-runtime/src/lib.rs:1469-2127` |
| 5.F | `MEMORY.md` documents the `BuiltInBuilder` / `NamespaceBuilder` migration (`Built-in Registration` section). The reality: builders exist (`crates/otter-vm/src/js_surface.rs:268, 497, 612, 745`), are used for some intrinsics (`Math`, `JSON`, `Reflect`), but a third path (`install_<thing>` in `bootstrap.rs`) handles the rest. **Three intrinsic-registration patterns in production.** | Major | `crates/otter-vm/src/js_surface.rs` + `bootstrap.rs` |

### 5.4 Recommendations

- **P0 decision** (owner-level): pick ONE intrinsic registration path —
  - (A) `BuiltinIntrinsic` trait + per-class installer file (the path
    already started in `intrinsic_install.rs`); migrate everything to
    it, delete `install_*` functions from `bootstrap.rs`, OR
  - (B) `js_class!`/`js_namespace!`/`raft!` macros (drop the trait,
    move per-class state into a macro invocation), OR
  - (C) `BuiltInBuilder`/`NamespaceBuilder` chained builders (the path
    MEMORY.md says was adopted). Pick one. Document the deprecation of
    the other two. Migrate.
- **P0**: Split `bootstrap.rs` into per-intrinsic files under
  `crates/otter-vm/src/intrinsics/<class>.rs`. AGENTS.md already demands
  this; the work has been deferred.
- **P1**: Verify microtask ordering against Test262 `built-ins/Promise`
  once full-suite runner produces a number.

---

## 6. Module system & event loop

### 6.1 What is there

- Module loader resolves `file://`, `node:`, `https://`; canonicalises
  through `oxc_resolver`; supports import maps and package-graph DTO
  (`module_loader.rs:48, 561-1500`).
- Module graph + records owned by `Runtime` (`module_graph.rs`,
  `module_records.rs`).
- Event loop: `tokio::runtime::Handle` ownership, spawns timer tasks via
  `tokio::time::sleep` (`event_loop.rs:170-184`).
- Microtask queue lives on the VM side; the runtime drains after each
  script/module run + after every timer fire.
- Dynamic import: `import_meta` allocated through rooted host-object
  helpers (per VM_REFACTOR_PLAN P0 closure).

### 6.2 Comparison

- Node: libuv loop with phases (timers → pending → idle → poll → check
  → close). Otter uses Tokio's reactor — simpler, async/await first.
- Deno: same pattern as Otter; uses tokio.
- HostEnqueuePromiseJob (§9.4.1): both Node and Deno drain microtasks
  between turn boundaries. Otter does the same.

### 6.3 Problems

| # | Issue | Severity | Location |
|---|---|---|---|
| 6.A | `module_loader.rs` mixes resolver, fetcher, cache, and capability gate. 1507 LoC is large but coherent. | Minor | `crates/otter-runtime/src/module_loader.rs` |
| 6.B | No published HostEnqueuePromiseJob ordering test against Test262. | Major (spec) | n/a |
| 6.C | `module_graph.rs` is 854 LoC; readable. Reasonable. | n/a | `crates/otter-runtime/src/module_graph.rs` |

### 6.4 Recommendations

- **P1**: Run Test262 `built-ins/Promise/all` + `built-ins/Promise/race`
  + `built-ins/Promise/then` once the runner is producing numbers. Lock
  in the % delta.

---

## 7. Public API (macros + embedding)

### 7.1 What is there

- `otter-macros` defines `js_namespace`, `js_class`, `raft`
  (`crates/otter-macros/src/lib.rs:61, 167, 443`). `#[dive]`, `burrow!`,
  `lodge!` are **declared deferred** in AGENTS.md §"Macro and Async
  Agreements" but are mentioned throughout CLAUDE.md as available —
  CLAUDE.md is out of date with AGENTS.md.
- The macros generate static `NamespaceSpec` / `ClassSpec` /
  `ConstructorSpec` / `MethodSpec` data with
  `NativeCall::Static` function pointers.
- Public API: `Otter` (layer A) + `Runtime` / `RuntimeBuilder` (layer B)
  in `otter-runtime`. `NativeCtx`, `IntrinsicArgs`, `NativeFunction`,
  `JsObject` exposed from `otter-vm`.
- Embedder error model: single `OtterError` enum, `Result<_, OtterError>`
  on every public call (`otter-runtime/src/error.rs`).

### 7.2 Comparison

| Engine binding pattern | Otter equivalent |
|---|---|
| napi-rs `#[napi]` derive on Rust fn | `#[dive]` (declared deferred — not built) |
| deno_core `op2!` macro | `raft!` (built — zero callers) |
| JSC C API `JSObjectMake*` | direct `ObjectBuilder` / `ConstructorBuilder` (in active use) |
| Boa `#[boa_macros::js_class]` | `#[js_class]` (built — zero callers) |

### 7.3 Problems

| # | Issue | Severity | Location |
|---|---|---|---|
| 7.A | `otter-macros` ships a public macro API with zero production callers. Code rot risk; embedders reading AGENTS.md will expect macros to work but `crates/otter-vm` itself doesn't use them. | Major (DX) | `crates/otter-macros/src/lib.rs` vs `grep js_class crates/` |
| 7.B | Three competing intrinsic-installation paths (see §5.4). No documentation tells the contributor which to use. | Major (DX) | n/a |
| 7.C | CLAUDE.md describes `#[dive]`, `#[dive(deep)]`, `burrow!`, `lodge!` as if implemented. AGENTS.md says they're deferred. Code: no implementations. CLAUDE.md is wrong. | Minor (docs) | `CLAUDE.md` |
| 7.D | Public re-exports from `otter-vm` are wide (`crates/otter-vm/src/lib.rs:127-189`). Almost every internal type is exported. Stability story unclear: what's pub-API vs pub-for-runtime? | Major (API) | `crates/otter-vm/src/lib.rs:127-189` |

### 7.4 Recommendations

- **P0**: Resolve §7.A/B/C: either (i) wire macros into production (port
  3-5 intrinsics to `#[js_namespace]`/`#[js_class]` as a forcing function),
  or (ii) delete `otter-macros` and remove macro references from
  AGENTS.md and CLAUDE.md.
- **P1**: Sweep `crates/otter-vm/src/lib.rs:127-189` for over-exports.
  Use `pub(crate)` aggressively; mark VM-internal types `#[doc(hidden)]`
  if they must remain `pub`.

---

## 8. Spec correctness gaps

### 8.1 What is published

- `ES_CONFORMANCE.md` — empty for full corpus. Targeted baselines only:
  Object.hasOwn 83.87% → after fix.
- `MEMORY.md` — historic per-suite numbers for the **parked** stack:
  Annex B 100%, Generators 38–40%, Arguments 78%, GeneratorPrototype
  72.1%, WeakMap 77.3%, WeakSet 89.0%, class/definition 86.2%,
  class/super 87.5%, etc.
- No active-stack number.

### 8.2 Architectural spec risks

| # | Issue | Severity | Location |
|---|---|---|---|
| 8.A | `Op::JsonCall` / `Op::MathCall` / `Op::PromiseCall` / `Op::CallMethodValue` — compiler-side shortcuts for `JSON.x(...)`, `Math.x(...)`, etc. If the compiler does not check whether `JSON` / `Math` / `Promise` were lexically shadowed at the call site, these opcodes violate §13.3.6 (CallExpression → MemberExpression → resolution). | Major | `crates/otter-bytecode/src/lib.rs:86-93` + `crates/otter-compiler/src/builtins_call.rs` |
| 8.B | Monomorphic-only IC (§2.C, §2.D) is a correctness concern only because the on-disable fallback path must be 100% spec-compliant. Audit `property_dispatch.rs` for the disabled-IC path. | Major | `crates/otter-vm/src/property_dispatch.rs` |
| 8.C | `Value` clone semantics: `Value::String` holds `JsString { repr: Arc<StringRepr> }` — every register copy bumps an atomic refcount. Correctness OK; perf bad. | Minor | `crates/otter-vm/src/string/mod.rs:228-232` |
| 8.D | No Test262 baseline → no regression gating. Adding ICs or refactoring the value model with no number breaks the project's own §"Track E" acceptance criterion. | Blocker (process) | `ES_CONFORMANCE.md` |

### 8.3 Recommendations

- **P0**: Capture a Test262 baseline against the active stack. Even
  `--filter "language"` + `--filter "built-ins"` separately, with the
  10-second timeout. Without a number, every subsequent change is
  unmeasurable.
- **P0**: Verify §8.A. One Test262 fixture per intercepted name.

---

## 9. Security

### 9.1 What is there

- `unsafe` confined to `otter-gc` and `otter-modules/src/ffi.rs:158, 167,
  219` (verified by grep). FFI requires `--allow-ffi` capability.
- Capability gates: fs_read, fs_write, net, env, subprocess, ffi
  documented in AGENTS.md §Security and enforced at module loader +
  module entry points.
- Module loader rejects non-allowlisted hosts before fetch
  (`module_loader.rs:528, 562-627`).
- Env-secret deny patterns documented (`AWS_*`, `*_SECRET*`).
- Heap caps catchable as `RangeError` via `--max-heap-bytes`.

### 9.2 Risks

| # | Issue | Severity | Location |
|---|---|---|---|
| 9.A | `#![forbid(unsafe_code)]` NOT present on `otter-vm`, `otter-runtime`, `otter-compiler`, `otter-bytecode` despite AGENTS.md claiming the whole workspace forbids `unsafe` outside `otter-gc`. | Minor (process) | crate headers |
| 9.B | No regex DoS budget — `regress` is used (not Irregexp); per its README it lacks catastrophic-backtracking protection. Atomics for cancellation in regex executor unclear. | Major | `crates/otter-vm/src/regexp.rs, regexp_prototype.rs` |
| 9.C | No capability-fuzz harness (ROADMAP S10). | Major | n/a |
| 9.D | FFI `unsafe { Library::new(&path) }` (`ffi.rs:167`) takes a path string. Capability check happens upstream — verify the path is the *resolved* path, not user-supplied attacker-controllable string. | Major | `crates/otter-modules/src/ffi.rs:158-167` |

### 9.3 Recommendations

- **P0**: Add `#![forbid(unsafe_code)]` to the four crate roots.
- **P1**: Add a regex execution budget (instructions counted in
  `JsRegExp::exec`), matching the `--max-heap-bytes` cap pattern. Atomics
  cancellation via `InterruptFlag`.
- **P2**: Audit FFI path canonicalisation.

---

## 10. Tooling, CI, docs

### 10.1 What is there

- `Justfile` shortcuts for fmt/lint/test/test262.
- `cargo test --all --all-features` is the unit test runner.
- Test262 runner lives in `crates/otter-test262`; runtime is set up.
- Trace + profile + heap-snapshot tooling exists (AGENTS.md §Debugging).

### 10.2 Risks

- `lib.rs` files (`otter-vm/src/lib.rs` 9 791 LoC, `bootstrap.rs` 3 938
  LoC, `object.rs` 3 292 LoC, `property_dispatch.rs` 3 952 LoC,
  `promise_dispatch.rs` 3 289 LoC, `object_internal_ops.rs` 4 163 LoC,
  `object_statics.rs` 3 117 LoC, `cli/main.rs` 3 343 LoC) all violate
  AGENTS.md §"Module Size And Boundary Hygiene".

### 10.3 Recommendations

- **P1**: Split `crates/otter-vm/src/lib.rs` — `Value` enum + impl into
  `value.rs`; the dispatch loop into a `dispatch/` submodule (split by
  opcode family). The current file is a hard-to-review hot spot.

---

## Refactor plan

Each item: problem → solution → effect (test262 % / perf / DX) → risk →
blast radius → flag-day or incremental.

### Phase 0 — foundational (blocks further productive work)

| # | Problem | Solution | Effect | Risk | Blast | Mode |
|---|---|---|---|---|---|---|
| F1 | No Test262 baseline on active stack | Run `cargo run -p otter-test262 -- run --timeout 10` against full corpus; commit `ES_CONFORMANCE.md` snapshot | Establishes regression gating | Low | None (read-only) | Flag-day (publish number) |
| F2 | MEMORY.md describes a `Value(u64)` model that does NOT exist; CLAUDE.md describes `#[dive]` macros that do NOT exist | Reconcile both files against actual code | Removes false architectural claims; prevents wasted work | Low | docs only | Incremental |
| F3 | Three intrinsic-installation paths coexist | Pick one (recommend the trait + per-class installer file pattern at `intrinsic_install.rs:52`), document the decision, migrate the rest | Single contributor path | Medium (migration touches every builtin) | High in `crates/otter-vm/src/bootstrap.rs` | Incremental (one builtin at a time) |
| F4 | `#![forbid(unsafe_code)]` documented but not enforced | Add the attribute to `crates/{otter-vm,otter-runtime,otter-compiler,otter-bytecode}/src/lib.rs` | Mechanical safety invariant | Low | None (currently compliant) | Flag-day |
| F5 | ROADMAP P1 marked `[x]` but IC is monomorphic-only | Either implement polymorphic-4 + megamorphic and keep `[x]`, or revert the mark | Truthful tracker | Low | docs/code aligned | Incremental |

### Phase 1 — quick wins (< 1 week each)

| # | Problem | Solution | Effect | Risk | Blast |
|---|---|---|---|---|---|
| Q1 | `lib.rs` is 9 791 LoC | Move `Value` enum + impls into `value.rs`, dispatch into `dispatch/{control,loads,arith,calls,...}.rs` | Reviewability | Low | `crates/otter-vm/src/` only |
| Q2 | `bootstrap.rs` is 3 938 LoC mixing many builtins | Split per-class into `intrinsics/{array,number,object,date,...}.rs`. Each migrates to the trait pattern under F3. | DX | Low | Per-builtin |
| Q3 | Macro crate has zero callers | Port `Math` and `JSON` to `#[js_namespace]` as forcing function. If macros prove ergonomic, lock them in. If not, delete the crate. | Decision unstuck | Low | `otter-vm` + `otter-macros` |
| Q4 | Frame holds `Rc<str> module_url` cloned every push | Move module URL into the executable function metadata; frames carry a `function_id` already | One Rc clone per call eliminated | Low | `crates/otter-vm/src/frame_state.rs` |
| Q5 | Three-tier match dispatch | Merge into one match returning a `DispatchAction` typed enum (the MEMORY.md "Phase 1.2 Flatten InstructionResult" that already shipped on the parked stack — port it) | 5-15% interp throughput | Low | `crates/otter-vm/src/lib.rs` |

### Phase 2 — medium (1–4 weeks each)

| # | Problem | Solution | Effect | Risk | Blast |
|---|---|---|---|---|---|
| M1 | Monomorphic-only IC | Promote `PropertyIcEntry` to `Polymorphic{shapes: [ShapeId;4], records: [T;4]}` + `Megamorphic{stub: fn}`. Replace `record_guard_miss` to add a poly entry before disabling. | +5-15 % Test262 perf-sensitive subsuites; matches ROADMAP P1 claim | Medium (correctness of slow path) | `crates/otter-vm/src/property_ic.rs`, `property_dispatch.rs` |
| M2 | `Value` enum is 24-32 B, `Clone` not `Copy` | Reintroduce NaN-boxed `Value(u64)` with sub-tag scheme already documented in MEMORY.md. `Closure` body migrates to a GC payload so the enum can shrink. | 2-3× interp throughput, smaller register windows | High (touches every register read/write site) | Whole VM. Recommend a `Value64` parallel type behind a feature flag, then flag-day once parity hit |
| M3 | Fat `Frame` | Per VM_REFACTOR_PLAN §"P1 Frame and Root Scanning". Split hot (registers, pc, function_id, this) from cold (pending_*, async_state, generator_owner). Cold state moves to side records keyed by frame id. | Smaller working set, cheaper GC tracing | High | Whole VM |
| M4 | Regex DoS surface | Wire `regress` into `InterruptFlag`; add instruction budget. | Hardened embedding | Medium | `crates/otter-vm/src/regexp.rs` |
| M5 | Bytecode is 34 B/insn | Re-encode `ExecutableFunction.code` to `Vec<u8>` + per-opcode argument layout. Dispatch via `OPCODE_HANDLERS[op]` fn-pointer table (Boa pattern). | 3-5× interp throughput. **This is the biggest single lever.** | High | `crates/otter-bytecode`, `crates/otter-vm/src/executable.rs`, dispatch loop |

### Phase 3 — large (> 1 month, requires RFC)

| # | Problem | Solution | Risk | RFC |
|---|---|---|---|---|
| L1 | No JIT (ROADMAP J track) | Build baseline JIT atop the bytecode redesign in M5. Cranelift backend; deopt to interp at safepoints; type feedback from M1 IC. | Very High | Yes |
| L2 | GC: parallel + concurrent mark (ROADMAP G3, G4) | Extend the incremental driver to multi-threaded marking. JSC Riptide pattern. | High | Yes |
| L3 | Polymorphic-shape dictionary mode for delete-heavy objects (VM_REFACTOR_PLAN P1 "Object Storage Follow-Ups") | Add dictionary-mode storage when an object hits N transitions after a delete. | Medium | No |
| L4 | Async/generator state: continuations cost a heap allocation today (`async_state: Option<AsyncFrameState>`). Move to a continuation chain keyed by promise. | Match V8's `JSAsyncFunctionObject` shape. | Medium | Yes |

---

## Open questions

1. **Macro decision (F3 / Q3).** Pick one of three competing
   intrinsic-installation paths. Owner choice; the audit can recommend
   the trait + per-class installer file (which is already adopted for
   `string::intrinsic`), but a different decision (full macro adoption,
   or full builder-chain adoption) is equally valid. **Required before
   Q2 / F3 can start.**
2. **Bytecode redesign (M5) vs JIT roadmap (L1).** M5 is the largest
   single interp perf lever and is also the prerequisite for L1. Should
   M5 be a hard prerequisite to any further ROADMAP P / E work? The
   audit's view: yes. Owner confirmation needed.
3. **Value-model redesign (M2).** Re-introducing NaN-boxing means
   touching every register-read/write site (probably 4 000-6 000 hits).
   Acceptable for the perf delta. Owner confirmation that this is in
   scope.
4. **Macro crate fate (Q3).** Delete or finish? The audit recommends
   "finish or delete" because carrying an unused public API surface that
   AGENTS.md documents as recommended is the worst of both worlds.
5. **MEMORY.md as a source of truth.** Currently a mix of "what we did
   on the parked stack" + "what we did on the active stack" + "advice".
   Suggest splitting active-stack runtime invariants out into
   `docs/runtime-invariants.md` and parking the historical entries.

---

*Audit performed against commit at HEAD `4bf09bd4`. Boa reference:
`/tmp/boa` `main` shallow clone.*

---
---

# Part II — Refactor Plan (Phase 2)

The executable companion to Part I (the audit above). Part I = diagnosis;
Part II = the road map and the deep-dive sections the owner asked for
after reading the audit. Where Part I already named a problem, this
section does not repeat it — it links and moves on to design and
ordering.

> Auditor's note. Part I was right that the value model + bytecode
> encoding + IC depth are the three biggest mistakes in the current
> stack. It was *slightly* wrong on intrinsic registration: there is a
> single unified entry point (`BuiltinIntrinsic` trait at
> `crates/otter-vm/src/intrinsic_install.rs:52`, all 30+ intrinsics use
> it via `bootstrap_entry!` at `crates/otter-vm/src/bootstrap.rs:369-412`).
> The duplication is one level down — the installer **bodies** are
> split between `bootstrap.rs::install_<thing>` free functions and
> per-class modules. The trait is the right top-level shape; the
> failure is that `bootstrap.rs` is still 3 938 lines of body. The
> macros (`crates/otter-macros/src/lib.rs:61-491`) still have zero
> production callers (confirmed by repository-wide grep on
> 2026-05-21). Plan adjusted accordingly.
---

## Principles

1. **Correctness > DX > perf > new features.** Test262 % is the gating
   signal; perf without correctness is malpractice (cf. JSC reviews
   under Pizlo: "if it's not spec-correct, your benchmark is a lie").
2. **No backward-compatibility shims during the refactor.** Otter is
   pre-1.0. Every removed type goes — no `#[deprecated]` chains, no
   parallel old API. The cost of carrying both is paid for years; the
   cost of cutting over once is one PR.
3. **Feature flag risky migrations, hard cut-over once tests are
   green.** The Value-redesign (Phase 1 below) and bytecode redesign
   (Phase 2 below) are large enough to need a `--features=value64` /
   `--features=bytecode2` build flag and a parallel type during
   construction. Both flags die in the same commit that flips the
   default — no permanent dual-build matrix.
4. **No work without a baseline.** Every phase ships a Criterion micro
   + Test262 macro number before/after. The Test262 baseline
   (Audit §8.3 F1) is a hard prerequisite for Phase 1.
5. **Architect; do not patch.** Per CLAUDE.md feedback: no
   hidden-property hacks, no "this is fine because the test passes."
   If the design doesn't honour ECMA-262 §X by construction, it is the
   wrong design.
6. **Active crates only.** No new dependency on parked
   `crates-legacy/*`. (AGENTS.md §Repository Layout.)
7. **Spec link per module.** ADR-0001 §6: every module that
   implements a spec algorithm cites the §-section in its doc-comment.
   New modules created by this plan honour this from day one.

---

## Phase 0 — Foundational (blocks every other phase)

Same set as Audit §"Refactor plan / Phase 0" with one addition. Listed
here because every later phase reads regression deltas off the
baselines this phase establishes.

### 0.1 Test262 baseline on the active stack

- **Why** Without a number, the impact of every later phase is
  unverifiable. Audit §8.3 F1.
- **Touches** `ES_CONFORMANCE.md`; new `docs/test262-baseline.md` with
  per-subsuite numbers; `Justfile` recipe `just test262-baseline`.
- **Acceptance** Published `language/` + `built-ins/` + `annexB/`
  per-subdirectory pass rates, committed.
- **Risk** Low (read-only). **Effort** S. **Depends on** —.

### 0.2 Reconcile `MEMORY.md` and `CLAUDE.md`

- **Why** Audit §F2: `MEMORY.md` documents an 8-byte NaN-box Value
  that does **not exist** in the current code; `CLAUDE.md` documents
  `#[dive]` / `burrow!` / `lodge!` macros that AGENTS.md describes as
  deferred and that have zero production callers.
- **Touches** `MEMORY.md`, `CLAUDE.md`, `AGENTS.md` §Macros.
- **Acceptance** Three files agree on what exists. Historical
  per-suite numbers move to `docs/historical-test262.md` so the
  active-stack memory doesn't keep masquerading as current truth.
- **Risk** Low. **Effort** S. **Depends on** 0.1 (need real numbers to
  put in MEMORY.md).

### 0.3 `#![forbid(unsafe_code)]` on four crate roots

- **Why** Audit §4.B / §9.A. Documented invariant; not enforced.
- **Touches** `crates/{otter-vm,otter-runtime,otter-compiler,otter-bytecode}/src/lib.rs`.
- **Acceptance** Compiles green.
- **Risk** Low. **Effort** S. **Depends on** —.

### 0.4 Drop the `[x]` lie on ROADMAP P1

- **Why** Audit §2.D. ROADMAP says polymorphic IC is shipped; code
  is monomorphic-only. Tracker truthfulness blocks every later
  decision that quotes ROADMAP status.
- **Touches** `ROADMAP.md:73`.
- **Acceptance** Mark P1 `[ ]`; add note pointing to Phase 5.1 below.
- **Risk** Low. **Effort** S. **Depends on** —.

### 0.5 Pick one decision on the macro crate

- **Why** Audit §7.A: `otter-macros` ships 819 lines of attribute /
  declarative macro code; **zero callers in production**
  (`grep js_namespace|js_class|raft! crates/ → 0 hits` outside
  `crates/otter-macros` and one comment in `runtime_cx.rs:141`).
  AGENTS.md says they're the recommended path; reality says no one
  uses them.
- **Decision required from owner** Two options:
  - **(A) Adopt.** Port `Math`, `JSON`, `Reflect` to `#[js_namespace]`
    as forcing function (Audit §Q3). If it works, the macros become
    the recommended path for new intrinsics and the Phase 4 macro
    extension below builds on top of them.
  - **(B) Delete.** Drop the crate, remove references from AGENTS.md
    / CLAUDE.md. The `BuiltinIntrinsic` trait + builder pattern
    (`crates/otter-vm/src/js_surface.rs:268,497,612,745`) becomes the
    only DX for new intrinsics.
  - **Recommended:** (A). Owner explicitly wants macros for module
    install, third-party intrinsics, and custom-prefix modules (see
    Phase 4). Killing the crate now is wasted optionality.
- **Depends on** —. **Blocks** Phase 4.

---

## Phase 1 — Value layout & allocation reduction (direction C)

> The single biggest perf lever. Audit §1 already named it.
> This phase plans how to land it without rewriting the world
> twice.

### 1.0 Why the previous attempt regressed

`MEMORY.md` lines 10-20 describe an 8-byte NaN-box Value with sub-tags
`0x7FFC..0x7FFF` and `UpvalueCell = GcRef<UpvalueData>` that was
explicitly shipped earlier. The current `Value` is a 30-variant Rust
enum (`crates/otter-vm/src/lib.rs:216-401`) measuring ≥24 B, **`Clone`
but not `Copy`**, with `Closure` holding `Rc<[UpvalueCell]> +
Option<Box<Value>>`. Somewhere between the "C2 string hierarchy" work
(MEMORY.md, 2026-04-25) and the present, the NaN-box was abandoned.
The plan does not reconstruct that history; it commits to
re-introducing the 8-byte form **once** and never regressing.

### 1.1 Audit every "fat" Value variant — full inventory

Catalogued by Value-enum position in `lib.rs:217-401`:

| Variant | Owned data | Target home |
|---|---|---|
| `Undefined`, `Hole`, `Null`, `Boolean(bool)` | nil | NaN payload immediate |
| `Number(NumberValue)` | `enum {Smi(i32), Double(f64)}` 16 B | SMI: 32-bit immediate w/ LSB tag (V8 pattern). Double: NaN-box payload OR boxed `GcRef<HeapNumber>` only for non-canonical NaN |
| `BigInt(BigIntValue)` | enum (inline i64 + heap fallback per MEMORY.md "P1 perf pass") | `GcRef<JsBigInt>` (heap only); inline i64 fast-path stays on the heap node, NOT the Value |
| `String(JsString)` | `Arc<StringRepr>` (8 B inline ptr) | `GcRef<JsString>` — move `JsString::repr` to a fully-GC body. **Kill the `Arc`** (atomic refcount on every register copy is the slow path Phase 1 in MEMORY.md called out and never finished) |
| `Symbol(JsSymbol)` | `Rc<SymbolBody>` | `GcRef<JsSymbol>` |
| `Function {function_id}` | `u32` | NaN-box payload (function_id encoded in 32 bits of the 51-bit NaN payload) |
| `Object(JsObject)` | already `#[repr(transparent)] Gc<ObjectBody>` (4 B offset) | NaN-box payload, sub-tag `TAG_PTR_OBJECT=0x7FFC` |
| `Array(JsArray)` | `Gc<ArrayBody>` | NaN-box payload, **same** sub-tag as Object — disambiguate via `JsObject::kind()` on the body (matches Boa's `vtable: &'static InternalObjectMethods`, `/tmp/boa/core/engine/src/object/jsobject.rs:80-84`). This kills one variant; reuse the existing `is_array()` body bit |
| `Closure { function_id, upvalues: Rc<[…]>, bound_this: Option<Box<Value>> }` | `Rc<[UpvalueCell]>` + heap-boxed Value | `GcRef<JsClosure>` where `JsClosure = { function_id: u32, upvalues: GcRef<[UpvalueCell]>, bound_this: Value }` — sub-tag `TAG_PTR_FUNCTION=0x7FFE` |
| `BoundFunction(BoundFunction)` | `Rc<BoundFunctionBody>` | `GcRef<JsBoundFunction>`, `TAG_PTR_FUNCTION` |
| `NativeFunction(NativeFunction)` | `Rc<NativeFunctionBody>` | `GcRef<JsNativeFunction>`, `TAG_PTR_FUNCTION` |
| `Iterator`, `RegExp`, `Promise`, `Map`, `Set`, `WeakMap`, `WeakSet`, `WeakRef`, `FinalizationRegistry`, `Temporal`, `Intl`, `ArrayBuffer`, `DataView`, `TypedArray`, `Generator`, `Proxy`, `ClassConstructor` | various `Rc<…>` / `Gc<…>` handles | All collapse to `TAG_PTR_OBJECT`; type disambiguation through `GcHeader::tag()` (per MEMORY.md, already in place for GC). The 19 enum variants become 1 sub-tag + 19 `GcHeader::tag()` values |

**Result:** From 30 enum variants × ≥24 B to a single `Value(u64)` with
4 sub-tags + per-`HeapRef` `GcHeader::tag()`. `Copy`. Cache footprint
of a register file is 1/3.

### 1.2 Why this is not academic

Hot paths that pay register-copy cost on every iteration:

- `read_register` / `write_register` (`crates/otter-vm/src/lib.rs`, every
  opcode handler). Today: enum `Clone` → discriminant byte + memcpy of
  the widest variant (`Closure` = ~32 B). Target: 8-byte memcpy +
  no `Rc::clone`.
- Argument passing on call. Pre-Audit `pending_args` was already
  migrated to `SmallVec<[Value; 8]>` (per MEMORY.md "Phase 3.1") so
  the structural shape is right; only the per-element size is wrong.
- GC tracing. Frame slot tracing walks `&[Value]`; a `Value(u64)` can
  be inspected via sub-tag without dispatching through a Rust enum.

### 1.3 Rc/RefCell on hot paths to remove

| Where | Today | Replacement |
|---|---|---|
| `Closure.upvalues: Rc<[UpvalueCell]>` | atomic-free refcount, still a heap touch per copy | `GcRef<[UpvalueCell]>` on the closure body. `UpvalueCell` itself stays as `GcRef<UpvalueData>` per MEMORY.md design |
| `Frame.upvalues: Rc<[UpvalueCell]>` (`crates/otter-vm/src/frame_state.rs:50`) | refcount on call | Inherit from the closure body — frame holds `GcRef<JsClosure>` and indexes into its upvalues slot. One handle, no clone |
| `Frame.module_url: Rc<str>` (frame_state.rs:113) | string clone on every push | Drop. Frame already has `function_id`; module URL lives on the executable function metadata. Audit Q4 |
| `JsString::repr: Arc<StringRepr>` (string/mod.rs) | atomic refcount on every Value clone today | `JsString = GcRef<StringBody>` where `StringBody` owns the repr inline. MEMORY.md "C2 string hierarchy" already laid the groundwork; finish the move from `Arc` to `Gc` |
| `JsPromise.inner: Rc<…>`, `JsSymbol.inner: Rc<…>`, `JsRegExp.inner: Rc<…>`, etc. | refcount per Value copy | Same pattern: each becomes a thin `GcRef` to a GC body. Already the case for `JsObject` (Audit §1.1) — apply uniformly |

**This is the work MEMORY.md "Phase 2.2 (Arc→Rc for Shape)" started.**
It is not done.

### 1.4 Blast radius

`rg "Value::" crates/otter-vm/src/ --type rust | wc -l` ≈ several
thousand match arms. Approach:

1. Introduce `Value64` as a parallel type behind
   `#[cfg(feature = "value64")]`. Every public function with
   `Value` in its signature gains a parallel `value64` variant. The
   compiler is one or both signatures, never silent fallback.
2. Port the interpreter in opcode-family chunks: arithmetic →
   load/store → calls → property → iterator → async/generator.
   Each chunk lands with green tests under `--features=value64`.
3. Once all opcode handlers parity-pass, flip the default. Same PR
   deletes the old `Value` enum and the feature flag. **No
   permanent dual build.**

### 1.5 Micro-benchmarks (acceptance)

Criterion benches required to land before the flag flips:

- `register_copy_1m`: 1 M `read_register / write_register` cycles, no
  allocation. **Must show ≥2× throughput.**
- `closure_call_heavy`: tight loop calling a closure 10 K times.
  **Must show ≥1.5× and zero atomic refcount events** (verified by
  `loom` or `parking_lot::lock_api`-style poison harness, or simply
  zero `AtomicUsize::fetch_*` in the closure-call path under
  cargo-asm).
- `string_concat_heavy`: 100 K `+` of two strings. **Must not regress.**
- `gc_pause_p99`: hot allocation loop, 95th percentile pause.
  **Must not regress** (tracing cost is the only worry; smaller
  Values reduce slot count).

### 1.6 Failure modes to avoid

- **Naive rooting → finalizer reentrance.** Migrating `Promise`,
  `WeakRef`, `FinalizationRegistry` to `GcRef` means their reactions
  / cleanup callbacks must be re-rooted before any finalizer dispatch.
  This is the same trap `otter-gc` already documents
  (VM_REFACTOR_PLAN §P0). Pattern: the migration PR for each variant
  ships its rooting tests alongside the type-shape change.
- **Hole leak.** `Value::Hole` is observable in array internals only
  (Audit §1.C). In NaN-box form, give it a distinct payload (e.g.
  payload `0x0000_0000_0000_0001` under `TAG_PTR_OTHER`) and check
  for it before every public coercion. The "user code never observes
  this variant" comment becomes a hard runtime guard, not a wish.
- **NaN canonicalisation.** ECMA-262 §6.1.6.1 requires every NaN to
  compare equal but distinct payloads to be preserved across
  `ArrayBuffer` writes. NaN-boxing reuses NaN payloads for non-double
  values; the canonical NaN reserved for `Number(NaN)` must never
  collide with any sub-tag. V8 reserves a specific bit pattern
  (`0x7ff8_0000_0000_0000`); JSC same. Otter must pick exactly one
  canonical NaN encoding and document it.

### 1.7 Effort + risk

- **Effort** L (3-6 weeks). The interpreter dispatch loop reads
  ≈4 000 Value sites; mechanical edits per opcode family.
- **Risk** High. Touches every register operation. Mitigation: the
  feature-flag parallel build catches regressions early; the Test262
  baseline from 0.1 catches behavioural regressions.

### 1.8 Open RFC for owner

| Decision | Recommendation |
|---|---|
| Tag-bit assignment: V8-style top-tag vs LuaJIT-style payload-tag | LuaJIT-style NaN-box (per MEMORY.md historical design). Closer to JSC, simpler than V8's compressed pointers, fits Otter's 4 GiB GC cage exactly |
| `Value` ordering: SMI fast path vs unified double | Hybrid: `Number(Smi(i32))` payload = LSB-tagged into a 32-bit half of the NaN payload (the existing `NumberValue` Smi already exists; map it onto bit layout). `Number(Double)` = canonical NaN payload encoding |
| Migration mode | Feature-flag parallel build, single-PR cut-over |

---

## Phase 2 — Bytecode redesign & dispatch & JIT-readiness (direction D)

> Audit §2 is the diagnosis. This phase says **what to build** so a
> baseline JIT (Sparkplug-class) becomes plausible in 12 months
> without re-doing the bytecode again.

### 2.1 Bytecode encoding target

Move from `Vec<ExecInstr>` (34 B/insn,
`crates/otter-vm/src/executable.rs:248-261`) to `Vec<u8>` byte-encoded
streams with variable-width arguments. **The Boa shape**
(`/tmp/boa/core/engine/src/vm/mod.rs:987-1004`):

```
loop {
    let byte  = code.bytes[pc];
    let opcode = Opcode::decode(byte);
    OPCODE_HANDLERS[opcode as usize](context, pc, …);
}
```

V8 Ignition refinement: prefix bytes `Wide` / `ExtraWide` for
16-/32-bit operands. Otter does NOT need V8 prefix bytes from day one;
matching Boa's flat encoding is enough for 5-10× density and is the
prerequisite for Sparkplug-class baseline JIT (the only reason
Sparkplug is fast is that each bytecode maps to ~5-30 machine
instructions emitted in a single pass — feasible only with a stable
1-byte opcode + small operand prefix).

### 2.2 Dispatch shape

Single `match op { … }` returning `DispatchAction` (the MEMORY.md
"Phase 1.2 Flatten InstructionResult" work — already shipped on the
parked stack, never carried forward). Three-tier matching
(`crates/otter-vm/src/lib.rs:3974, 4350, 5254, 5320`) collapses to
one.

For per-opcode work, **two-step migration**:

1. Single match → `DispatchAction` enum, no fn-pointer table yet.
   This alone removes ~5-15 % overhead (Audit §Q5).
2. Then `OPCODE_HANDLERS: [Handler; 256]` fn-pointer table. Each
   handler decodes its own operands, advances `pc`, returns
   `DispatchAction`. Boa-shaped.

**Skip computed-goto.** Stable Rust does not support it. `become`
(tail-call) is unstable. The fn-pointer table gets within 70-80 % of
computed-goto throughput per published JSC and Boa numbers.

### 2.3 Spec correctness cleanup that lands with bytecode redesign

`Op::JsonCall`, `Op::MathCall`, `Op::PromiseCall`, `Op::CallMethodValue`
(`crates/otter-bytecode/src/lib.rs:86-93`) are compiler-side shortcut
opcodes. Audit §3.D and §8.A flagged these. Decision required while
designing the new opcode set:

- **(A) Keep but prove sound.** Compiler emits the shortcut only when
  it can statically prove `JSON` / `Math` / `Promise` / etc. are not
  shadowed at the binding site. Test262 fixture per name verifies
  shadowing works.
- **(B) Remove.** A polymorphic IC (Phase 5.1) on the normal
  `LoadProperty` → `Call` sequence reaches the same speed without the
  spec risk. V8/JSC don't have these — that should tell us something.

**Recommendation: (B).** Removing them simplifies the new opcode
encoding (4 fewer variadic opcodes that need by-name operand
decoding) and removes the spec-divergence surface. Keep them out of
the redesigned ISA entirely.

### 2.4 JIT-readiness, as concrete interface contracts

What a baseline JIT (Sparkplug pattern) needs from the interpreter
foundation. Each item below is a contract the bytecode redesign must
honour:

| Contract | Why a JIT needs it | Otter today | Fix-in-Phase-2 |
|---|---|---|---|
| **Stable, versioned, packed bytecode** | Sparkplug emits machine code as a 1:1 mapping from bytecode; bytecode shape must not change across versions or you re-JIT every cache hit | `ExecInstr` struct, no version field | New byte-encoded format has a 4-byte header `(magic, version, flags, reserved)` per function; bumping `version` invalidates JIT cache cleanly |
| **Fixed register-window layout** | OSR (on-stack replacement) requires the JIT to materialise interpreter frame state from a JIT-frame at any safepoint. Layout must be agreed | Frame holds `SmallVec<[Value; 8]>` registers, `function_id`, `pc`, plus 5 `Option<Pending*>` cold slots (`crates/otter-vm/src/frame_state.rs:35-160`) — fat and not fixed | Phase 3 below splits hot from cold. The hot record is the JIT's deopt target. Once split, the layout offsets become part of the JIT ABI |
| **IC slots as patch-points** | JIT code does `cmp shape_reg, [ic_site.shape]; jne slow_path`. IC site must be an addressable, mutable, single-shape-or-array slot | Today: `LoadPropertyIc` / `StorePropertyIc` are interpreter-owned `Vec<PropertyIcEntry<…>>` keyed by `(function_id, pc)` (`property_ic.rs:117-131`). Site address is **not stable** — the `Vec` reallocates | Move IC sites to a flat `Box<[PropertyIcSlot]>` per ExecutableFunction. Each slot is a fixed-size struct (cache line aligned) the JIT can hardcode pointers into |
| **GC stack maps** | JITed code must declare which machine registers hold tagged GC roots at every safepoint, so the GC traces JIT-frame registers correctly | Otter-GC has root scanning for interpreter frames (VM_REFACTOR_PLAN §P0); no stack-map abstraction for JIT yet | Phase 2 itself does NOT need stack maps — but it must NOT introduce GC root-handling that is interpreter-specific. Rooting via `GcRef` + `Cell<Value>` (per Phase 1) makes the GC API tier-agnostic |
| **ABI for native ↔ JS calls** | JIT-emitted call sites must know register/stack assignments, return-value location, exception protocol | Today: `NativeFunction` carries a `Rc<NativeFunctionBody>` with a Rust fn-pointer; the interpreter does the marshalling each call | Freeze the `NativeCall` ABI in Phase 2: typed argument vector, typed return slot, typed exception slot. The interpreter already conforms; documenting it makes it JIT-callable later. **Do not change** native ABI in Phase 4 macro work — Phase 4 generates code that matches this ABI, not a new one |
| **Profile / feedback collection** | Speculative JIT needs type feedback per call site (V8 `FeedbackVector`, JSC `LLIntProfile`) | Audit P1 IC sites already collect shape hits/misses but only for property opcodes. Arithmetic profiling is absent | Phase 2 reserves space in each `ExecutableFunction` for a `FeedbackVector` (initially used by Phase 5.1 polymorphic IC; later read by the JIT). Existing `PropertyIcStats` is collapsed into one entry per FB slot |

**Net:** Phase 2's bytecode redesign locks down enough ABI surface
that a baseline JIT is a separate (large) project — but **does not
require revisiting Phase 2's decisions**. Without this, the JIT ships
on top of an ABI that is changing under it, which is the worst
outcome.

### 2.5 What Phase 2 deliberately does NOT do

- **Pick a JIT backend.** Cranelift vs B3-port vs hand-roll. That is
  the Open RFC at the bottom; Phase 2 does not commit.
- **Build an IR layer between AST and bytecode.** Single-pass
  AST → bytecode matches Boa, V8 Ignition, JSC LLInt baselines.
  A graph IR is appropriate **only at the optimising tier** (V8
  TurboFan, JSC B3). Adding one now is yak-shaving.
- **Computed-goto.** Stable Rust won't, fn-pointer table is the
  contemporary equivalent.

### 2.6 Acceptance + effort

- **Acceptance** Steady-state interp ≥ 1.5× on
  micro-benchmarks (`fib(30)`, `octane-richards`, `octane-deltablue`)
  + zero Test262 regression. **3-5× on dispatch density** is
  achievable per Boa published numbers.
- **Effort** L (4-8 weeks). Touches every opcode handler; needs
  parallel build flag like Phase 1.
- **Risk** High. **Depends on** Phase 1 landed (the new opcode
  handlers are written against the new Value).

---

## Phase 3 — Bootstrap, IntrinsicRegistry, snapshotting (direction B)

> Audit §5 + owner's "помойка из инициализации" comment.

### 3.1 What's actually wrong

Not the trait choice — the trait works
(`crates/otter-vm/src/intrinsic_install.rs:52-72`, 30+ implementors).
The wrong is:

- **`bootstrap.rs` is 3 938 LoC of body.** Adapter structs
  (`ObjectIntrinsic`, `ArrayIntrinsic`, …) at lines 2483-2640 each
  delegate to a `install_<thing>` free function defined elsewhere in
  the same file. This is single-file split-personality.
- **Two-and-a-half registration styles in practice:**
  - Free `install_<thing>` function pointed at by an adapter
    struct (`Object`, `Array`, `Number`, `Symbol`, `Date`, `Proxy`,
    `Function`, `Intl`, `Temporal`, `AggregateError`,
    `Iterator` — all in `bootstrap.rs`).
  - Per-module `BuiltinIntrinsic` impl, body lives next to the type
    (`crates/otter-vm/src/math/mod.rs:64`,
    `crates/otter-vm/src/json/intrinsic.rs`,
    `crates/otter-vm/src/string/intrinsic.rs`,
    `crates/otter-vm/src/reflect.rs:480`,
    `crates/otter-vm/src/console.rs:56`,
    `crates/otter-vm/src/timers.rs:223`,
    `crates/otter-vm/src/atomics.rs:63`, `bootstrap_*.rs` files).
  - `BuiltInBuilder` / `NamespaceBuilder` chained-builder API
    (`crates/otter-vm/src/js_surface.rs:268,497,612,745`), called by
    the per-module installers above. This is a helper, not a
    competing registration system — but MEMORY.md treats it as one,
    which compounds the confusion.

### 3.2 Target architecture

- **One trait, one body location, one builder.**
  - Adopt the `crates/otter-vm/src/intrinsics/` directory layout. Each
    file owns one intrinsic end-to-end: spec doc-comment + adapter
    struct + `BuiltinIntrinsic` impl + installer body.
  - `bootstrap.rs` shrinks to (a) the `BootstrapEntry` /
    `BootstrapFeatures` types, (b) `BOOTSTRAP_ENTRIES` static slice,
    (c) `build_global_this`. **Target size: ≤ 500 LoC.**
  - The `BuiltInBuilder` / `NamespaceBuilder` chain stays — it's the
    correct internal helper. The two `install_<thing>` patterns
    collapse into one.

- **Topological init order.** Today the order in `BOOTSTRAP_ENTRIES`
  (`bootstrap.rs:369-412`) is hand-curated. Boa is the same
  (`/tmp/boa/core/engine/src/builtins/mod.rs:241-353`,
  `Realm::initialize` is a 100-line linear init list). This is fine
  **as long as the order is right.** Otter has known forward-refs:
  Object → Function (Function.prototype is an Object), Function →
  Object (Object.prototype methods are Functions). These are resolved
  today by `build_global_this_impl` post-loop fixing up
  `Object.prototype` parent (`bootstrap.rs:510-514`).

  **Recommended:** keep the linear init list; do NOT introduce a real
  topological sort. The dependencies are static + dozens, not
  hundreds + dynamic. V8 / JSC / Boa all use linear init for the same
  reason. Document the order constraints as code comments next to
  `BOOTSTRAP_ENTRIES`.

- **Per-realm intrinsics struct.** Boa exposes
  `Intrinsics` (`/tmp/boa/core/engine/src/context/intrinsics.rs`,
  1843 LoC) with strongly-typed slots for each well-known intrinsic
  object. Otter's equivalent lives implicitly on the global. **Worth
  copying.** A `RealmIntrinsics` struct with typed fields for every
  well-known intrinsic (`%Object.prototype%`, `%Array.prototype%`,
  …) replaces "look it up by string at install time." Required
  for §B `%TypedArray%` correctness anyway (MEMORY.md "2026-05-18"
  hack already routes `prototype_override` through a string match).

### 3.3 Snapshotting — defer with a documented checkpoint

V8 startup snapshot saves ~50 ms cold start; Bun uses JSC snapshot;
Deno bundles a V8 snapshot. Audit owner asked: should Otter ship one?

**Recommendation: defer until ROADMAP P10 (cold start <20 ms)
becomes the gating signal.** Snapshotting is real engineering:

1. The GC must support relocatable mark/sweep (Otter has it —
   pointer-compressed `Gc<T>` u32 offsets,
   `crates/otter-gc/src/lib.rs:43-47`). **Otter is structurally ready.**
2. Every intrinsic installer must be deterministic given the same
   inputs. Today's installers mostly are; capability-gated ones
   (FFI) aren't, but they're not on the snapshot path.
3. Snapshot format itself: V8 ships an opaque memory-blob; Bun
   inherits from JSC. Cranelift code cache (would be useful for
   JIT later) is a separate beast.

**Prerequisite for snapshotting** is Phase 1 + Phase 3 done — Value
must be stable, intrinsics must be single-pattern. Until those land,
snapshot work compounds rather than saves work.

When the time comes:
- Save: serialize the GC cage state + intrinsic root table.
- Load: mmap the snapshot blob into a fresh cage; relocate `Gc<T>`
  offsets (they're already relative); fix up native-fn pointers from a
  manifest. (V8 does this; the manifest is the standard tricky part.)

### 3.4 Effort + risk

- **`bootstrap.rs` split** — Effort M (1-3 weeks); per-file move, one
  builtin at a time, mechanical. Risk Low.
- **`RealmIntrinsics` struct** — Effort M; touches every site that
  currently looks up an intrinsic by string. Risk Medium.
- **Snapshotting** — out of scope for this plan. Re-RFC when needed.

### 3.5 What this phase makes possible

- A new intrinsic = one new file in `crates/otter-vm/src/intrinsics/`
  + one line in `BOOTSTRAP_ENTRIES`. The macro work in Phase 4 reduces
  this to ~30 lines of Rust per intrinsic.
- Third-party intrinsic = same pattern, exposed publicly via the
  `BuiltinIntrinsic` trait, registered via `RuntimeBuilder::add_intrinsic`.

---

## Phase 4 — Macros (direction E)

> Owner: "макросы нам точно нужны, использовать повсеместно."

### 4.1 The case for macros (and against the current ones)

The current `otter-macros` crate (819 LoC, three macros, zero
production callers) is the wrong shape:

- `#[js_namespace]` and `#[js_class]` generate static `NamespaceSpec` /
  `ClassSpec` data with `NativeCall::Static` function pointers. The
  static spec then has to be installed through the builder somewhere —
  which is exactly what an `BuiltinIntrinsic` impl does today, by
  hand, in 30-50 lines.
- The generated static data **doesn't compose** with capability
  gates, doesn't honour the `RealmIntrinsics` slot (because Phase 3.2
  doesn't exist yet), and doesn't run any compile-time validation
  beyond "did you pass `length = N`."
- `raft!` exists for "grouped" namespace declarations and is even
  less complete (only methods, no accessors or constants).

### 4.2 What macros should do

Looking at the strongest production macro DX in the ecosystem:

| Tool | Pattern | Why it works |
|---|---|---|
| **napi-rs** `#[napi]` | Attribute on a function; macro generates the N-API marshal layer; types deduced from Rust signature | Sig drives everything — no separate `length = N` |
| **deno_core** `op2!` | Attribute on a function; emits a strongly-typed Op vtable entry; supports async / sync / fast-call variants | Compile-time argument-type checks; capability-gate insertion is a single attribute |
| **embed-anyhow / serde** | Derive macro; struct shape drives generated code | Same: declarative source of truth |
| **JSC IDL bindings** | Generator reads `.idl` file; emits C++ binding | Separate language for bindings; not idiomatic in Rust |
| **Boa** | No macros (`boa_macros` is GC `Trace` derive only). Hand-written `IntrinsicObject` impls | Works for Boa because they accept the boilerplate cost |

**Otter target shape (subject to owner sign-off — see Open RFC):**

```rust
#[otter_intrinsic(
    name = "Math",
    module = "otter:builtins",
    kind = namespace,
    feature = "core",
)]
mod math {
    /// Math.abs(x). ECMA-262 §21.3.2.1.
    #[otter_method(name = "abs", length = 1)]
    pub fn abs(ctx: &mut NativeCtx<'_>, x: f64) -> f64 { x.abs() }

    /// Math.PI. ECMA-262 §21.3.1.1.
    #[otter_constant(name = "PI")]
    pub const PI: f64 = std::f64::consts::PI;
}
```

What the macro should generate (and does not today):

1. The `BuiltinIntrinsic` impl with `NAME` / `FEATURE` / `install`.
2. A `RealmIntrinsics::math` typed slot (Phase 3.2 dependency).
3. Auto-marshalled argument coercion from `Value` → typed Rust
   (`f64`, `&JsString`, `JsObject`, `Option<…>`). Today every native
   function hand-writes this — the boilerplate is the actual cost.
4. Spec-link doc-comment carried through to the runtime introspection
   output (used by the Inspector — Phase 5 below).
5. Compile-time arity check vs ECMA spec when the spec section is
   declarable.

### 4.3 Module-install macro (owner ask)

The owner specifically asks for a macro that installs **modules with
prefixes** — `otter:`, `node:`, custom `myapp:`. Today this lives
across `crates/otter-runtime/src/module_loader.rs` (1 507 LoC) and
ad-hoc tables.

Proposed shape:

```rust
#[otter_module(prefix = "otter", name = "kv", capability = "kv")]
pub mod otter_kv {
    use otter_runtime::prelude::*;

    #[otter_export(name = "open")]
    pub fn open(ctx: &mut NativeCtx<'_>, path: &str) -> JsResult<KvHandle> { … }
}
```

Generated:
- ESM-style export table the loader can resolve `import { open } from "otter:kv"` against.
- Capability check at module open time (`--allow-kv` gate).
- Compile-time check that every `#[otter_export]` function returns
  `JsResult<T>` where `T: IntoJs`.

This is the **single highest-leverage macro** for third-party
embedders. Without it, custom modules require touching the loader.

### 4.4 What NOT to generate

- **No `NativeCall::Closure`.** The static fn-pointer surface is a
  JIT-readiness contract from Phase 2.4. The macro emits
  `NativeCall::Static` only.
- **No automatic global binding for `#[otter_intrinsic]`.** The
  bootstrap registry remains the single source of truth for what
  appears on `globalThis`. The macro produces the implementation; the
  registry decides whether to install it.
- **No `unsafe`.** The macro is purely declarative; everything it
  emits is forbid-unsafe-clean (Audit §0.3).

### 4.5 Compile-time validation

- Function signature parameter types must implement `FromJs`; macro
  emits a static assertion. (Same pattern as `napi-rs` and `pyo3`.)
- Return type must implement `IntoJs` or be `JsResult<T> where T: IntoJs`.
- For accessor methods: getter has 0 args, setter has 1 arg, both
  share the same name — checked at expansion time.
- `length = N` matches the declared parameter count, **or** is set
  to ECMA-spec value when divergent (e.g. `Array.prototype.push.length === 1`
  despite variadic).
- Spec link attribute `#[spec(section = "21.3.2.1", url = "…")]`
  is enforced for `#[otter_intrinsic]` modules; macro errors at
  expansion if missing. Cf. ADR-0001 §6.

### 4.6 Migration

1. Wire `Math`, `JSON`, `Reflect` to the new attribute pattern as
   forcing-function candidates (Audit Q3). Each is small, has a
   known spec, fits one file.
2. If the macro proves correct, port `Object`, `Array`, `Number`,
   `String`. These are the largest installers.
3. The old `#[js_namespace]` / `#[js_class]` / `raft!` macros are
   deleted in the same PR that lands the new ones. **No old + new
   coexistence period.**

### 4.7 Effort + risk

- **Effort** M (2-4 weeks). Macro work is mostly syn-parsing +
  quote-emission. The hard part is the `FromJs`/`IntoJs` traits, but
  those should mirror the existing `NativeCtx` coercion helpers.
- **Risk** Medium. Macro errors are notoriously bad UX; needs good
  span attribution.
- **Depends on** Phase 3.2 (`RealmIntrinsics` typed slots).

### 4.8 Open RFC for owner

- **Trait vs macro for intrinsic registration:** keep both. Macro is
  the DX-preferred path for new builtins; trait remains for cases
  where the installer needs imperative control (capability check at
  install time, lazy intrinsics, etc.).
- **Macro name prefix:** `#[otter_*]` vs `#[js_*]`. Recommend
  `#[otter_*]` to namespace from any other JS-binding crate the user
  may pull in.

---

## Phase 5 — Inspector & introspection (direction A)

> Boa parity+ for VM trace, plus disassembly, plus IC/GC snapshot.

### 5.1 What Boa actually has

`/tmp/boa/core/engine/src/vm/mod.rs:679-767` — gated behind
`#[cfg(feature = "trace")]`:

- `trace_call_frame` prints the disassembled code block when entered
  for the first time
  (`code_block.rs:941-1100` impl Display for CodeBlock — pc, opcode,
  operands, exception handlers, constants, bindings, source map).
- `trace_execute_instruction` prints per-step
  `time | opcode | operands | stack` after each instruction.
- CLI flag `--trace` flips the runtime flag
  (`/tmp/boa/cli/src/main.rs:115, 576`).

The cost when feature is off: zero (cfg-gated). When feature is on but
runtime flag is off: one boolean branch per instruction, kept out of
the hot path by branch prediction.

### 5.2 What Otter has

- Disassembly: `crates/otter-bytecode/src/disasm.rs:23` `pub fn
  disassemble(module: &BytecodeModule) -> String`. Called from
  `otter-cli` (`crates/otter-cli/src/main.rs:1144`). One-shot module
  dump — no per-frame, no per-instruction stepping.
- `RUST_LOG=debug cargo run …` — Rust-side log lines from the
  runtime, no opcode-level visibility.
- No trace flag; no step-trace; no IC snapshot dump.

### 5.3 Target — "OtterVM Inspector"

Single new crate (or feature flag on `otter-vm`): `otter-vm-inspect`.
Provides:

1. **Step trace** (Boa parity). `--features=inspector` + runtime
   `--trace` flag prints per-instruction trace lines. Format
   prescribed by Boa table; reuse the existing
   `crates/otter-bytecode/src/disasm.rs` formatter for the opcode
   column.
2. **Disassembly with IC + constants + source-map annotations.**
   Today's disasm is opcode + operands. Inspector adds per-pc:
   - Source span (`crates/otter-bytecode/src/lib.rs:1579-1585`
     `SpanEntry`).
   - Property IC site state (shape, key, miss count).
   - Polymorphic IC table for the site (Phase 5.1 below).
   - Inline constant resolution.
3. **Snapshot commands** (REPL-style, behind feature flag):
   - `shapes` — dump every live shape + transition tree.
   - `ic` — dump every IC site state; useful for "why is this site
     megamorphic" debugging.
   - `heap` — Chrome `.heapsnapshot` export. **Already exists**
     (`crates/otter-gc/src/heap.rs` per Audit §4.1). Inspector wires
     a CLI surface to it.
   - `frames` — current frame stack + register window.
4. **Breakpoint** — break on PC / opcode / shape-transition.
   Implemented in the dispatch loop's runtime-flag branch
   (already there for trace; cheap to add break-condition).
5. **Time-travel** — explicit non-goal for v1. Requires journaling
   every register write; cost is high. Defer.

### 5.4 Integration points

- **Bytecode redesign (Phase 2) must allocate a `breakpoint` flag
  per ExecutableFunction** so the inspector can install breakpoints
  without re-encoding the bytecode. One byte per function. Free.
- **The dispatch loop's branch check** matches Boa: one bool inside
  the inner loop, branch-predicted false in production. Gated by
  `#[cfg(feature = "inspector")]` so production builds don't carry
  even the branch.
- **No new allocations on the hot path** when the flag is off. Every
  inspector data structure lives behind `Option<InspectorState>` on
  the runtime.

### 5.5 CLI surface

Add to `crates/otter-cli/src/main.rs`:

- `otter --trace run script.ts` — step trace to stdout.
- `otter --inspect run script.ts` — interactive REPL on script
  completion; `shapes`, `ic`, `frames`, `heap-snapshot path.json`.
- `otter --break-at file:line run script.ts` — break in inspector at
  the first instruction emitted for that span.

### 5.6 Effort + risk

- **Effort** M (2-3 weeks).
- **Risk** Low. Feature-flagged, doesn't affect production.
- **Depends on** Phase 2 (bytecode redesign — much easier to step
  through a flat `Vec<u8>` than the current `Vec<ExecInstr>`).

---

## Phase 6 — Object / Promise polish (direction F)

> What to copy (and what NOT to copy) from Boa.

### 6.1 Worth borrowing from Boa

**(F-borrow-1) `vtable: &'static InternalObjectMethods` per object.**
`/tmp/boa/core/engine/src/object/jsobject.rs:80-84`. Boa attaches a
static method-table pointer to each JsObject; ordinary, array,
arguments, proxy, typed-array, …, each have their own vtable. Otter
currently does this via `JsObject` shape `kind` field + a single
dispatch through `crate::object::*` free functions. The vtable approach
is faster (one indirect call instead of `match kind`) and is the
standard pattern in JSC. **Action:** evaluate during Phase 1's object
GC migration. Cost: one cache-line on each object body. Benefit:
removes the per-call kind dispatch. **Likely net positive once
proxies / typed arrays are hot.**

**(F-borrow-2) `IntrinsicObject` static trait.**
`/tmp/boa/core/engine/src/builtins/mod.rs:128-137`. Boa keeps the trait
**stateless** — `init(realm: &Realm)` takes a realm reference; the
intrinsic's identity comes from looking it up in `Intrinsics` afterward.
Otter's `BuiltinIntrinsic::install(heap, global)` is functionally
equivalent. The trait shapes are nearly identical; the difference is
Boa exposes a `get(intrinsics: &Intrinsics)` method on the same trait
so consumers can find the installed object back. Otter does this via
`object::get(global, "Math")` — string lookup.
**Action:** the `RealmIntrinsics` struct (Phase 3.2) already takes
care of this — Boa's pattern is the right one.

**(F-borrow-3) `PromiseCapability` as a strict Spec record type.**
`/tmp/boa/core/engine/src/builtins/promise/mod.rs:176-190` — boa
models the spec PromiseCapability literally (promise + resolve fn +
reject fn) with a manually-implemented `Trace`. Otter's promise model
is more ad-hoc — `JsPromiseHandle` is `Rc`-based, capability
materialisation happens implicitly. **Action:** when Phase 1 migrates
`JsPromise` to `GcRef<JsPromiseBody>`, redesign the capability path to
match the spec record exactly. Reduces the bug surface by a lot
(promise correctness is notorious — Test262 `built-ins/Promise` is
~600 tests, every spec divergence is one of them).

**(F-borrow-4) `boa_macros::Trace` derive for GC traceability.**
Otter today implements GC `Traceable` by hand per type. Boa derives
it. For new types added by Phase 1's migration, a derive macro
reduces error surface (forgetting to trace a field is silent
use-after-free; the audit memory entry "GC Shape Key Tracing Fix
(2026-02-17)" is the canonical example of how easy it is to miss).
**Action:** add a `#[derive(Trace, Finalize)]` macro to
`otter-macros`. Mirrors Boa exactly. Low effort, high safety yield.

### 6.2 Do NOT copy from Boa

**(F-skip-1) `boa_gc` design.** Boa's GC is a simpler tracing
collector. Otter-GC is ahead (generational, incremental,
ephemerons — Audit §4.2). Do not regress.

**(F-skip-2) Boa's `Vec<u8>` opcode + decode-per-byte exactly.**
The Boa shape is right (Phase 2.1). But Boa's specific opcode layout
(`/tmp/boa/core/engine/src/vm/opcode/`) bakes in some bytecode
choices that are sub-optimal for Otter — specifically, Boa has
separate `LoadName` / `GetName` / `GetNameOrUndefined` opcodes for
binding lookup that V8 handles via one `LdaGlobal` with feedback.
Phase 2's opcode set should look more like V8 Ignition's than Boa's.

**(F-skip-3) Boa's `JobQueue` trait as the host async API.**
`/tmp/boa/core/engine/src/job.rs:577` defines a `HostEnqueuePromiseJob`
trait the embedder implements. Otter runs Tokio directly
(`crates/otter-runtime/src/event_loop.rs:170-184`), and the microtask
queue is per-isolate (`crates/otter-vm/src/microtask.rs`). The
Tokio-direct design is correct for a server runtime (it matches
Deno's pattern); Boa's pluggable job queue is appropriate for an
embeddable engine that wants to defer to GUI run loops etc. Different
goals — keep Otter's.

**(F-skip-4) Boa's `JsValue` enum form (`jsvalue-enum` feature).**
Boa offers an opt-in enum-form `JsValue` as a fallback for platforms
where NaN-boxing is awkward (e.g. wasm32-unknown-unknown). Otter
target is server runtime + native CLI. Stick with NaN-box only;
don't carry the dual-form complexity.

### 6.3 Microtask ordering vs ECMA §9.4.1

Audit §6.B: no published HostEnqueuePromiseJob test against Test262.
Otter drains microtasks `after every macrotask`
(`crates/otter-runtime/src/lib.rs:1469`) plus inside each `run_script`
boundary. This is spec-compliant by inspection. **Action:** once
Phase 0.1's Test262 baseline produces a `built-ins/Promise` number,
flag any divergence here; otherwise don't change anything.

### 6.4 Effort + risk

- **Effort** M total (mostly Phase-1-coupled work).
- **Risk** Low to Medium; promise correctness is the highest-risk
  individual subtask. Mitigate with Test262 `built-ins/Promise` as
  the gating signal.

---

## Open RFCs — decisions the owner must make

Decisions sequenced by which phase they unblock.

### RFC-1 (blocks Phase 0.5) Macro crate: adopt or delete

Recommended **adopt**, predicated on the owner's stated ask
("commonly used, third-party intrinsics, custom prefixes"). Adopt
path means Phase 4 builds a new attribute-macro family; the existing
three macros are deleted in the same PR. **Alternative:** delete the
crate; new intrinsics use the `BuiltinIntrinsic` trait directly.

### RFC-2 (blocks Phase 1) Value-redesign go/no-go

Audit §1 and Phase 1 above. Recommend **go**. Required for any
ROADMAP P-track target. Owner confirms scope (≈4 000 call sites
touched; 3-6 weeks; feature-flag parallel build).

### RFC-3 (blocks Phase 2) JsonCall / MathCall / PromiseCall shortcut opcodes

Audit §2.E + §8.A + §3.D. Recommend **remove** in the new opcode
encoding. Alternative: prove static-no-shadowing at compile time and
keep. Spec-divergence risk vs perf delta of one polymorphic IC site.

### RFC-4 (blocks Phase 2 RFC selection) Polymorphic IC topology

Three viable shapes:

- **Boa-style PIC.** `ArrayVec<CacheEntry, 4>` with megamorphic flag
  (`/tmp/boa/core/engine/src/vm/inline_cache/mod.rs:17-90`). Simple,
  correct, ~1 cache line per site.
- **V8 Ignition-style chain.** Hidden-class chain walked at the IC
  site; each guard adds an entry; chain-too-long triggers
  megamorphic stub. More work, slightly faster steady state.
- **LLInt-style dispatch table.** Hash-table-of-shapes per site.
  Used in JSC. Larger but no order-dependency.

Recommend **Boa-style PIC**. Matches `Otter's existing IC slot
shape, fits in cache line, sized right for typical JS (4 shapes).

### RFC-5 (defer; blocks Phase 6 long-term) JIT backend candidate

Three credible options:

- **Cranelift.** Mature, Rust-native, wasm-tested. Codegen quality
  ~LLVM-12 level. Used by Wasmtime, Spidermonkey BaselineCompiler
  has historic Cranelift port. Trade-off: dependency, dynamic
  registry. Otter is already on a Tokio + oxc + tokio-uring stack;
  one more big crate.
- **B3 port.** JSC's optimising-tier IR. Public, but C++; porting to
  Rust is multi-engineer-year work. Cranelift gets you 80 % at 5 % of
  the cost.
- **Hand-roll baseline-only.** A Sparkplug-style baseline JIT emits
  one machine fn per bytecode; the per-opcode emitter is ~50-200
  lines. Total surface ~10-15 K lines of asm-emit per architecture.
  No third-party IR. JSC LLInt is hand-roll (offlineasm), Sparkplug
  is hand-roll. **For an interp-baseline transition, hand-roll wins
  on simplicity** — but it ships per-arch.

**Recommendation: defer.** Pick after Phase 2 lands. The bytecode
redesign there must NOT be designed around any one of these — it
must be designed for all three (which is what Phase 2.4's interface
contracts make sure of).

### RFC-6 (defer; blocks future cold-start work) Snapshot pipeline

See Phase 3.3. Recommend defer until Phase 1 + Phase 3 done. Otter-GC
is structurally ready (relocatable `Gc<T>`); the rest is engineering
not architecture.

### RFC-7 (blocks Phase 4 macro work) Macro DX shape

Two options on whether the macro generates the global binding:

- **(A)** `#[otter_intrinsic(name = "Math")]` auto-registers in
  `BOOTSTRAP_ENTRIES` via `inventory` or `linkme` crate. Pro: less
  code per intrinsic. Con: install order becomes implicit.
- **(B)** Macro emits the impl; the developer adds one line to
  `BOOTSTRAP_ENTRIES`. Pro: install order explicit (matches Boa).
  Con: one extra line.

**Recommendation: (B).** Explicit order matters for forward-refs
(Object ↔ Function). Boa keeps the same shape for the same reason.

---

## Migration order (topological)

```
Phase 0  (foundational)
  0.1  Test262 baseline                              [no deps]
  0.2  MEMORY.md / CLAUDE.md reconcile               [after 0.1]
  0.3  forbid(unsafe_code)                           [no deps]
  0.4  ROADMAP P1 truthfulness                       [no deps]
  0.5  Macro crate decision (RFC-1)                  [no deps; owner]
  parallelizable: 0.1, 0.3, 0.4, 0.5

Phase 1  (Value layout)                              [after 0.1, 0.3]
  1.1  Inventory + design freeze
  1.2  Feature-flag Value64 parallel build
  1.3  Migrate arith/load/store opcode handlers
  1.4  Migrate property/call opcode handlers
  1.5  Migrate iter/async/generator opcode handlers
  1.6  Flip default; delete old Value
  must be sequential within phase

Phase 2  (bytecode + dispatch + JIT-ready)           [after Phase 1]
  2.1  Drop JsonCall/MathCall shortcuts (RFC-3)
  2.2  Single-match DispatchAction (MEMORY.md unfinished work)
  2.3  Polymorphic IC implementation (RFC-4)
  2.4  Vec<u8> bytecode encoding + handler table
  2.5  FeedbackVector reservation; IC slot patch-point ABI
  sequential within phase

Phase 3  (bootstrap + RealmIntrinsics)               [parallelizable with Phase 2]
  3.1  Split bootstrap.rs into crates/otter-vm/src/intrinsics/
  3.2  RealmIntrinsics struct; replace string lookups
  3.3  (deferred: snapshotting; see RFC-6)
  3.1 → 3.2 sequential

Phase 4  (macros)                                    [after 0.5, 3.2]
  4.1  New attribute-macro family (#[otter_intrinsic], #[otter_method], …)
  4.2  Module-install macro (#[otter_module])
  4.3  Port Math/JSON/Reflect as forcing function
  4.4  Migrate larger builtins (Object/Array/Number/String)
  4.5  Delete old js_namespace/js_class/raft! macros

Phase 5  (inspector)                                 [after Phase 2.4]
  5.1  Step trace + disassembly extension
  5.2  Shape / IC / heap snapshot commands
  5.3  Breakpoint support
  5.4  CLI surface

Phase 6  (object/promise polish)                     [coupled with Phase 1]
  6.1  vtable: &'static InternalObjectMethods (during Phase 1)
  6.2  Trace/Finalize derive macro (during Phase 4)
  6.3  PromiseCapability as spec-shaped record (during Phase 1's promise migration)
```

Parallelizable: Phase 3 alongside Phase 2; Phase 6 work is mostly
inlined into the phases above.

---

## Estimated effort (gross)

| Phase | Effort | Risk |
|---|---|---|
| Phase 0 | S (1-2 weeks total) | Low |
| Phase 1 (Value) | L (4-8 weeks) | High |
| Phase 2 (bytecode + IC) | L (4-8 weeks) | High |
| Phase 3 (bootstrap) | M (2-4 weeks) | Medium |
| Phase 4 (macros) | M (2-4 weeks) | Medium |
| Phase 5 (inspector) | M (2-3 weeks) | Low |
| Phase 6 (polish) | M (subsumed into Phase 1 + Phase 4) | Low |
| **Total** | **4-7 calendar months** assuming serial execution and one engineer; ≈ 2-3 months with two engineers operating Phase 1 + Phase 3 in parallel | — |

Numbers are honest, not optimistic. Phase 1 and Phase 2 are
foundational; treat their estimates as floors. Phase 3-6 can compress
once 1+2 land.

---

## Out of scope for this plan (explicit)

- **Parallel / concurrent GC.** ROADMAP G3-G4. Re-RFC after Phase 1 is
  in. The incremental driver is ready; the engineering is large.
- **Optimising tier JIT.** ROADMAP J1-J7. Re-RFC after Phase 2 lands
  and baseline JIT becomes the bottleneck.
- **WebAssembly bridge.** ROADMAP J10. Cross-cutting; orthogonal.
- **Source-map V3 emission redesign.** Mentioned in Audit §3.C. Works
  today; do not touch.
- **`oxc_resolver` replacement.** Module loader (1 507 LoC) is fine;
  re-split when it grows past 2 000.

---

*Refactor plan written against commit at HEAD `4bf09bd4` and Boa
reference `/tmp/boa` `main` shallow clone, in companion to
`docs/architecture-audit-2026-05.md` (Phase 1). 2026-05-21.*

---
---

# Part III — Deep-dive supplements: §G Bytecode + §H VM Debugging

Two areas the owner called out after Part II: **bytecode must be a real
byte stream** (folded into Phase 2 above; the format spec is here) and
**first-class VM debugging** (Phase 5 above covers Inspector; the full
tier breakdown is here).

## §G Bytecode format specification (companion to Phase 2.1)

### G.1 Current state — two parallel representations

1. **Compiler/debug DTO** — `otter_bytecode::Instruction` (`crates/otter-bytecode/src/lib.rs:1379-1386`).
   `OperandList::Inline { len: u8, operands: [Operand; 3] }` or
   `OperandList::Spill(Box<[Operand]>)` (`lib.rs:1400-1418`). Serde-Serialize
   required for JSON dump (`otterBytecodeDumpVersion: 1`).
2. **VM execution view** — `ExecInstr` (`crates/otter-vm/src/executable.rs:248-261`):
   `op: Op (~1 B padded), operand_len: u8, inline_operands: [Operand; 3]
   (~36 B), side_start: u32, property_ic_site: u32`. Effective size with
   alignment ≈ 48 B (Part I quoted 32 B — verify; either way 8-32×
   wider than V8/JSC/Boa).

148 opcode variants in `Op` enum (`grep -c "^    [A-Z]" lib.rs`).
No `#[repr(u8)]` lock-in (compiler picks u8 because <256, but format is
not contractual).

PC is `u32` *array index*, not byte offset. Module-level
`side_operands: Box<[Operand]>` for spilled variadics.

### G.2 Engine comparison

| Engine | Shape | Width/insn | Dispatch | Wide form |
|---|---|---|---|---|
| V8 Ignition | byte stream, register+accumulator | 1 B op + 1-4 B operands | computed-goto in C++ | `Wide` (0xFA) / `ExtraWide` (0xFB) prefix → 16/32-bit operand widths |
| JSC LLInt | byte stream | narrow / op_wide16 / op_wide32 | generated asm via offlineasm | wide16/wide32 prefix opcodes |
| Boa | `Vec<u8>` | 1 B op + variable operand widths (U8/U16/U32) | `OPCODE_HANDLERS[op]` fn-pointer table | three opcode variants per logical op |
| Lua 5.x | fixed 32-bit | 4 B | switch / computed-goto | none |
| **Otter today** | `Vec<ExecInstr>` AoS | ≥32 B | 3 nested `match instr.op` (`lib.rs:3974,4350,5254,5320`) | none — `Operand` enum carries width |

**i-cache density**: at ~32 B/insn vs V8/JSC ~2 B/insn, a 1 KiB i-cache
line holds 32 Otter insns vs ~500 V8 insns. Hot loop of 20 insns
crosses 1+ cache lines in Otter, fits in <1 in V8.

**Decode cost**: Boa `read_u8/read_u16/read_u32` = one branchless load.
Otter: read `instr.op` → branch on `operand_len ≤ 3` → index
`inline_operands` (bounds check) or `side_operands` (bounds check)
per operand. **3-5× decode work per insn.**

### G.3 Proposed format

Keep `otter_bytecode::Instruction` as the **DTO** for JSON dump and
compiler-internal building. Redefine `ExecutableFunction.code` as a
true byte-stream:

```rust
pub(crate) struct ExecutableFunction {
    // metadata as today
    pub(crate) bytecode: Box<[u8]>,
    pub(crate) const_indirect: Box<[u32]>, // wide-form indirection when k > 0xFFFF
    pub(crate) ic_sites: u32,              // count for IC vector allocation
    pub(crate) source_map: Box<[(u32, SourceLoc)]>, // pc → source, sorted
}
```

**Encoding:**

```
narrow form:   [op: u8] [operands per OPCODE_SCHEMA]
wide16 prefix: [0xFE] [op: u8] [operands using u16 widths]
wide32 prefix: [0xFF] [op: u8] [operands using u32 widths]
```

**Operand schema** — single declarative source of truth (`otter-bytecode/src/schema.rs`):

```rust
#[derive(Copy, Clone)]
pub enum OperandKind { Reg, Const, Imm, Pc, IcSite, ArgCount }

pub struct OpSchema {
    pub mnemonic: &'static str,
    pub operands: &'static [OperandKind],
}

pub const OPCODE_SCHEMA: [OpSchema; 256] = { /* generated or hand-written, 148 entries */ };
```

This is **the JSC `BytecodeList.rb` pattern ported to a Rust const
table**. Used by:
- Emitter (compiler) — picks narrow/wide based on operand widths
- Decoder (VM dispatch) — advances PC
- Disassembler — symbolic dump
- JSON DTO conversion — DTO ⇄ byte-stream round-trip for `otter-test`
- JIT IR builder — walks insns when lowering

**Operand widths:**

| Kind | Narrow | Wide16 | Wide32 |
|---|---|---|---|
| `Reg` | u8 | u16 | u24 (written as u32 LE, top byte zeroed) |
| `Const` | u16 | u32 | u32 (via `const_indirect`) |
| `Imm` | i16 | i32 | i32 |
| `Pc` (relative) | i16 | i32 | i32 |
| `IcSite` | u16 | u32 | u32 |
| `ArgCount` | u8 | u16 | u16 |

**PC = byte offset** (`u32`), not array index. `Frame.pc: u32` already fits.

**Constants pool**: stays where it is. Narrow form indexes by u16; wide
form indexes by u32 via lazy `const_indirect` side table (only allocated
when a function overflows u16 consts).

**Magic header** per module:

```
b"OTBC"     // magic, 4 B
u32 version // bump on layout change; current = 2 (legacy AoS = 1)
u32 features // bitset: has_source_map, has_ic_sites, has_debug_locals
```

Snapshot (RFC-6) becomes trivial: byte-stream + constants pool + IC layout descriptor.

### G.4 Dispatch strategy choice (refines Phase 2.2)

| Strategy | Rust feasibility | Speedup vs current 3-tier match | Notes |
|---|---|---|---|
| Single `match op` on byte stream | works now | 1.5-2× | LLVM optimises single match into jump table. Current nested matches defeat it. **Land first.** |
| Fn-pointer table `[fn(&mut Vm); 256]` | works now | slight regression vs single match | LLVM can't inline through fn-pointer in Rust. Boa pattern — worse for interp, good for JIT IR walker. **Skip for interp.** |
| Threaded code (`become` tail-calls) | needs Rust 1.83+ stable `become` | 1.5-3× over big match | RFC 2603 stabilised. Each handler tails into next via `become Vm::op_load_local(...)`. **Highest-throughput safe-Rust option.** Stretch goal post-M2. |
| Computed-goto inline-asm | not in stable Rust | ~1.5× | What V8/JSC use in C++. **Skip.** |

**Decision:** ship M5 with single big-match (G.5 below). Re-evaluate
`become`-threaded after baseline established and Value model lands —
both compound. JIT (RFC-5) is the next perf tier above any interp strategy.

### G.5 Migration sub-tasks (refinement of Phase 2 M5)

| # | Task | Status flag |
|---|---|---|
| M5a | Define `OPCODE_SCHEMA` table in `otter-bytecode/src/schema.rs`. 148 entries. | hand-written once OR `build.rs` from a `.txt` schema |
| M5b | Implement `BytecodeBuilder` + `DecodedInsns` iterator behind `--features=bytecode2`. Compiler keeps emitting legacy DTO; converter writes DTO → byte-stream | parallel path |
| M5c | Add `ExecutableFunction.bytecode: Box<[u8]>` next to existing `code: Box<[ExecInstr]>`. Dispatch reads byte-stream when flag on; AoS path kept for parity testing | parallel path |
| M5d | Rewrite `dispatch_loop_inner` against byte-stream: single big match, `pc: u32` byte offset, `pc += op_width(op, prefix)` at end | the redesign |
| M5e | Cut over: remove `ExecInstr`, `side_operands`, `OperandList::Spill`. Bump dump version to 2. Keep legacy JSON DTO for snapshot tests | flag day |
| M5f | (Stretch) `become`-threaded dispatch behind `--features=threaded-dispatch`. Keep big-match default until measured win | optional |

### G.6 Contributor ergonomics

Three deliverables that turn byte-fiddling into boring:

1. **Emitter builder** (V8 `BytecodeArrayBuilder` pattern):
   ```rust
   let mut b = BytecodeBuilder::new(&pool);
   b.emit_load_local(dst, src);
   b.emit_call(callee, &args);
   let body = b.finish();
   ```
   Builder picks narrow/wide based on operand widths; emits prefix byte
   automatically; tracks IC site allocation. **No contributor writes raw bytes.**

2. **Decoder iterator** for tooling:
   ```rust
   for insn in DecodedInsns::new(&fn.bytecode) {
       insn.op; insn.pc; insn.operands;
   }
   ```
   Used by disasm, JSON-DTO export, debugger source map, test harness.

3. **Roundtrip property test**: `Emit(decode(stream)) == stream` for
   every test262 fixture. Catches schema drift.

### G.7 Disassembler as Inspector primitive

Today `otter_bytecode::disasm::disassemble(module) -> String`
(`disasm.rs:23`). Refactor to streaming/option-driven:

```rust
pub fn render_function(out: &mut dyn Write, fn_: &Function, opts: &DisasmOptions) -> io::Result<()>;
pub struct DisasmOptions {
    pub show_source: bool,
    pub show_ic_sites: bool,
    pub show_pc: bool,
    pub annotate: Option<&dyn Fn(/*pc*/ u32) -> Option<String>>, // for IC hits, breakpoints
}
```

Shared by **Inspector mode (Phase 5)**, CLI `disasm` subcommand, and
**crash dump (§H.7 below)**. One rendering codepath, one source of truth.

---

## §H VM debugging — first-class tier breakdown

Scope: debugging **the VM implementation** (engine devs), separate from
a future debug-protocol (DAP/CDP) for user JS. Overlaps with Phase 5
Inspector; this section is the full tier breakdown the owner asked for.

### H.1 Goals

1. Engine devs reproduce, narrow, and fix VM bugs in minutes, not hours.
2. Diagnostics survive into production binaries (≤1% perf cost when
   enabled, free when disabled).
3. Crashes always dump enough state to triage without re-running.
4. Triage paths exist for the three failure modes that dominate:
   shape-cache divergence, GC rooting bug, IC corruption.

### H.2 Inventory — what we already have

- Test262 watchdog with stack-depth / PC / instruction / function-name /
  module-URL dump on timeout (CLAUDE.md "Test262 Watchdog").
- `RUST_LOG` via `tracing-subscriber` plumbing.
- S5-b `catch_unwind` protection in production (`panic = "unwind"` in release).
- DevTools heap snapshot via `OtterRuntime::take_heap_snapshot`.
- `Error.captureStackTrace` + lazy `err.stack`.
- `otter-test262 --filter <pattern> --verbose` for narrowing.

### H.3 Inventory — what is missing

| # | Surface | Have | Gap |
|---|---|---|---|
| 1 | Opcode trace | no | `RUST_LOG=otter::vm::dispatch=trace` printing `(pc, op, top-3 regs)` |
| 2 | Disassembler from CLI | `disassemble` exists, no CLI | `otterjs disasm script.ts [--function f --pc N]` |
| 3 | Bytecode source map in errors | compiler emits `spans`; VM doesn't carry into errors | wire spans into `VmError`, Inspector, crash dumps |
| 4 | IC hit/miss telemetry | no | `RUST_LOG=otter::vm::ic=debug` per-site counters, dump on demand |
| 5 | Shape transition log | no | `RUST_LOG=otter::vm::shape=debug` log every transition; replay for divergence |
| 6 | GC mark-phase trace | tracing calls exist, unstructured | structured events for mark start/complete/sweep with byte counts |
| 7 | Frame dump on crash | watchdog only | panic hook walks `stack: &SmallVec<[Frame; 8]>` for any panic |
| 8 | Deterministic replay | no | seedable PRNG, mockable clock, fixed map-iteration order in trace mode |
| 9 | GDB/LLDB pretty-printers | no | `tools/lldb/otter.py`, `tools/gdb/otter.py` for `Value`, `Frame`, `Shape`, `GcRef<T>` |
| 10 | Differential against Boa | no | optional `otter-test diff --compare boa script.ts` |
| 11 | Crash-on-divergence assertions | limited | `debug_assert!(ic.shape == obj.shape, …)` ladder behind `#[cfg(debug_assertions)]` |
| 12 | DevTools/CDP frontend | no | out of §H scope; covered by user-debug protocol later |

### H.4 Tier breakdown — what ships when

**Tier 1 (Phase 1 quick-wins, ≤1 week each):**

- **H4.1** — CLI: `otterjs disasm <file>` with `--function <id>`,
  `--source-map`. Wraps existing disasm. Blast: `crates/otter-cli`.
- **H4.2** — Structured `tracing` targets: `otter::vm::dispatch`,
  `otter::vm::ic`, `otter::vm::shape`, `otter::gc::{mark,sweep,alloc}`,
  `otter::compiler::scope`. **Free at INFO level** (no events emitted);
  pay only when enabled. ~50 `tracing::event!` insertion sites across crates.
- **H4.3** — Panic hook installed by `OtterRuntime::new()`. Dumps top
  frame, last opcode, `pc`, function id + URL, GC state summary.
  Blast: `crates/otter-runtime/src/lib.rs`.

**Tier 2 (Phase 2 medium, alongside M5 bytecode rework):**

- **H4.4** — Spans into `VmError`. Every error carries `(pc, function_id,
  source_loc)`. Crash dump and Inspector both render line+col. Requires
  source map in `ExecutableFunction` — folds with G.5 M5c.
- **H4.5** — IC telemetry: `PropertyIcEntry` records `(hits, misses,
  last-seen shapes)`. Exposed via Inspector and `__otter_dump_ic_stats()`
  global in debug builds. Pairs with Phase 2 RFC-4 polymorphic IC —
  without telemetry, the new IC has no observability.
- **H4.6** — Assertion ladder: invariant checks behind `debug_assert!`
  in dispatch hot path (frame layout, GC rooting at safepoints, IC shape
  consistency). **Production-cheap** (no-op in release). Catches bugs in
  CI runs and contributor dev builds. Estimate: 20-40 assertions across
  VM/GC/IC.
- **H4.7** — Pretty-printers: `tools/lldb/otter.py` (Python LLDB ext) +
  `tools/gdb/otter.py` (Python GDB ext) for post-Phase-1 NaN-boxed
  `Value(u64)`. Until Phase 1 lands, ship printer for current enum form.
  Update `Justfile` to launch with them loaded. **Write twice acceptable**
  — second version (post-NaN-box) is shorter than first.

**Tier 3 (Phase 5 Inspector — already in Part II):**

- **H4.8** — Inspector mode: opcode trace UI, IC site explorer, shape
  inspector, GC state, allocation profiler. Built on H4.4/H4.5. TUI via
  `ratatui` sufficient for v1; web UI optional.
- **H4.9** — Deterministic replay: `--record <log>` writes opcodes +
  clock reads + PRNG seeds; `--replay <log>` re-executes. Pairs with
  H4.4 spans for bisection. Requires:
  - Seedable `Math.random` (mark `crate::math::random_source`).
  - Time freezing (`Date.now`, `performance.now`) via clock-source trait.
  - `BTreeMap`/`IndexMap` everywhere iteration order leaks (CLAUDE.md guideline).
- **H4.10** — Differential testing: `otter-test diff` runs a fixture
  under Otter and a reference (Boa, V8 via `d8`); compares stdout, throw
  value, final object shape sequence. Opt-in CI extension.

### H.5 Production debugging contract

`panic = "unwind"` in release (S5-c) makes H4.3 the safety net. Concrete
per-panic contract:

1. Capture panic message + location (`PanicHookInfo`).
2. Read active `Frame` from thread-local `Interpreter` (use `try_lock` /
   raw pointer; never block).
3. Format with same renderer as watchdog: `stack_depth, pc, instruction,
   function_name, module_url`.
4. Optionally dump last N opcodes from thread-local ring buffer (size 64,
   ~1 KiB; cost: 1 store per dispatched opcode behind
   `#[cfg(feature = "opcode-history")]`).
5. Write to stderr or `OTTER_CRASH_DUMP_DIR` if set.

Same contract applies to OOM (`VmError::OutOfMemory`) — test262 heap-cap
path already plumbs this; reuse the format.

### H.6 Dependencies on §G (bytecode) and Phase 1 (Value)

**§H depends on §G for:**
- **Stable byte-stream PCs** (G.3): source maps key on `pc: u32` byte
  offsets. Today's PC-is-array-index works but breaks when wide-form
  changes the byte offset.
- **`OPCODE_SCHEMA` table** (G.5): pretty-printers, opcode trace,
  disassembler, replay log all share this.

**§H depends on Phase 1 (Value redesign) for:**
- **NaN-boxed `Value(u64)`** has trivial pretty-printer: 3-line Python.
  Current 24-32 B enum needs printer that knows every variant's layout.
  Ship printer twice (first for today's enum, then for u64 form post-Phase-1).

**§H does NOT block §G or Phase 1** — Tier 1 ships immediately.

### H.7 1.0-eligibility contract

A 1.0-eligible build must:

- [ ] Panic in any crate of `crates/otter-*` produces a triageable dump (H4.3).
- [ ] `otterjs disasm` works on any module the runtime accepts (H4.1).
- [ ] `RUST_LOG=otter=trace` produces correctly-namespaced structured
      output across VM/GC/IC/compiler (H4.2).
- [ ] LLDB and GDB pretty-print `Value`, `JsObject`, `Frame`, `GcRef<T>` (H4.7).
- [ ] Every `VmError` carries `(function_id, pc, optional source_loc)` (H4.4).
- [ ] CI runs with `debug_assertions = true` for at least one job
      covering test262 (H4.6).
- [ ] Heap snapshot + CPU profile work as documented (already shipped;
      regression-test).

Nice-to-have but not blocking 1.0:
- Inspector TUI (H4.8) — internal-only acceptable.
- Deterministic replay (H4.9) — engine-dev tool.
- Differential testing (H4.10) — CI extension.

### H.8 Risk — debug-build perf

Twenty `debug_assert!` calls in dispatch hot path could pessimise
debug-build test262 by 5-15 %. **Mitigation:** time-box assertions by
category:

```rust
#[cfg(debug_assertions)]
if cfg!(feature = "vm-paranoid") { /* innermost dispatch checks */ }
```

Default-on for cold-path assertions, default-off for innermost dispatch
checks. Resurface as `cargo test --features vm-paranoid` in CI.

---

## Updated decision summary (Part I + Part II + Part III)

**Top-3 risks** (after §G/§H folded in):

1. **Phase 2 M5 (bytecode redesign) is now the largest single change in
   Phase 2.** Interacts with: snapshot format (RFC-6), JIT (RFC-5),
   source maps (H4.4), Inspector (Phase 5), test262 JSON dump version.
   **High blast radius.** Mitigation: feature-flagged parallel path
   during M5b/M5c — both old `ExecInstr` and new byte-stream live until
   cut-over (M5e).
2. **M5 vs Phase 1 (Value redesign) ordering.** Both flag-day-ish.
   Parallel = doubled bisection difficulty. **Land M5 first** — interp
   throughput win is bigger, JIT prerequisite, doesn't require touching
   every register slot. Phase 1 follows on the new byte-stream.
   *(This contradicts Part II §"Migration order" which schedules Phase 1
   before Phase 2. Owner sign-off needed — see RFC-8 below.)*
3. **Debug-build perf regression risk** (H4.6 + H4.8). See §H.8 mitigation.

**Top-5 owner decisions** (extends Part II Open RFCs):

1. **RFC-1** Macro crate adopt/delete — recommended adopt.
2. **RFC-2** Phase 1 Value-redesign go/no-go — recommended go.
3. **RFC-3** Drop `JsonCall`/`MathCall`/`PromiseCall` shortcut opcodes — recommended yes.
4. **RFC-4** Polymorphic IC topology — recommended Boa-style 4-entry PIC.
5. **RFC-7** Macro DX shape (auto-register vs explicit `BOOTSTRAP_ENTRIES` line) — recommended explicit.
6. **RFC-8 (NEW from Part III)** Phase ordering: Part II says Phase 1 → 2;
   Part III §H risk-analysis suggests **Phase 2 → 1** because byte-stream
   is the easier-to-bisect refactor and unblocks more downstream work
   (Inspector, source maps in errors, JIT prerequisites). **Owner decides.**

---

*Part III deep-dive supplements written against same commit `4bf09bd4`
and Boa ref `/tmp/boa main`. 2026-05-21.*
