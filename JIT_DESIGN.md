# Otter JIT Design

> Status: **living plan — implementation in progress.** Phase 0 (interpreter
> dispatch surgery), Phase 1 (baseline arm64 JIT), and Phase 1.5 (loop OSR) have
> shipped; the optimizing tier (Phase 2) is not started. §1.2 is the current
> status; §1.3 is the prioritized forward plan; §2–§7 are the standing
> architecture rationale and verification contract. Benchmark numbers live in
> `benchmarks/results/latest.md` (source of truth); numbers quoted here are
> directional and may lag a node version bump.

---

## 1. Executive summary

Otter runs a register bytecode **interpreter** plus a **single-tier baseline
JIT** (Sparkplug-style: bytecode→arm64 machine code via a dynasm-rs template
macro-assembler — one linear pass, no IR, no register allocation, no
speculation, no deopt; §3). The baseline is on by default (`OTTER_JIT=0`
disables it) and shares the interpreter's frame register array as its precise GC
root set, so the moving collector needs **no stack maps**. Two tier-up triggers
exist: **function-entry** (call-count threshold) for hot *called* functions, and
**loop OSR** (back-edge threshold) for hot loops in functions entered once.

The remaining gap is concentrated and structural. Against node (see
`benchmarks/results/latest.md`): the JIT has pulled call/loop-bound numeric
kernels close — `mandelbrot` to ~1.6× of node (our nearest compute bench),
`nbody` ~7×, `fib` ~9× — while benches still bottlenecked on the *interpreter*
(property loads through a stub, allocation, the separate regex engine) remain
13–75×: `prop-access` ~50×, `typed-array` ~75× (its loop body uses an
unimplemented opcode), `regex` ~67× (separate engine), `sort`/`array-ops` ~20×.

**Strategy.** The dispatch envelope (the original dominant cost) is gone for
compiled code. The path forward is (a) **widen the baseline opcode subset** so
more hot loops compile at all (the cheapest remaining wins), (b) **inline the
property IC** so `prop-access` stops paying a stub call per load, then (c) a
**speculative optimizing tier** (Phase 2: SSA, type feedback, deopt, Cranelift +
stack maps) for true numeric parity. §1.3 is the ordered plan.

### Scope: VM rework and GC are in scope; nothing is cut

Three project constraints shape this plan:

1. **VM internals are fully reworkable.** Otter ships a single self-contained
   binary — there is no external ABI or embedder API to preserve across the JIT
   work. The bytecode ISA, frame layout, dispatch mechanism, object model, and
   even the GC algorithm are all fair game. Where a rework (not a patch) is the
   right call, take it. This is why Phase 0 may *re-encode* bytecode to
   fixed-width rather than only patching the fetch (§4).
2. **The GC is not sacred.** If the collector is the bottleneck or a stability
   risk, improve the collector. GC is a **first-class parallel workstream**
   (§3.6, Track G), not a fixed constraint the JIT must route around. It carries
   its own perf and stability gates.
3. **Nothing is deferred — only sequenced.** Every item here (baseline JIT, loop
   OSR, optimizing tier, deopt, stack maps, GC hardening) is **committed scope**.
   The phase ordering is *execution order* (you cannot build the optimizer before
   the baseline it tiers up from), not a scope cut. "Tier 2" means "after tier 1
   is stable," never "maybe later." Stability is a **co-equal gate** with perf on
   every phase (§5): a phase that improves a bench but destabilizes the engine is
   not closed.

---

## 1.2 Status — what has shipped

Gated per §5 (test262 failing-set unchanged JIT-on vs interpreter + no bench
regression + `OTTER_GC_STRESS` clean). Full iteration history is out of this
doc by design; this is the standing summary.

**Phase 0 — interpreter dispatch surgery. DONE.** O(1) instruction fetch
replaced the per-op `binary_search_by_key` (`executable.rs`); the single biggest
interpreter win. Per-op metering collapsed into one inlined accumulate. Threaded
dispatch was dropped (stable Rust already lowers `match op` to a jump table) and
a frame-resolution hoist was tried and reverted (measured wash). Conclusion:
interpreter micro-optimization is exhausted — the remaining envelope is
structural and only the JIT removes it.

**Phase 1 — baseline arm64 JIT + function-entry tier-up. DONE.** New
`crates/otter-jit` (lifts the workspace `forbid(unsafe_code)` like `otter-gc`,
encapsulating dynasm executable buffers + fn-ptr transmute behind a safe API).
The emitter lowers a `JitFunctionView` in one linear pass; operands/results flow
through the interpreter's `Frame.registers` window (already a `FrameRoots`
provider) so there are no GC pointers in machine registers across a safepoint.
Supported opcode subset: int32 + **float (f64)** arithmetic (`Add`/`Sub`/`Mul`/
`Div`) with guard + slow-path, the comparisons incl. **double compare**,
`Increment`, branches, register moves, `Return`; `Call`/`CallMethodValue`/
`MakeFunction`/`LoadElement`/`StoreElement`/`LoadGlobalOrThrow`/`LoadProperty`/
`StoreProperty` via safe re-entry stubs that reload from frame slots after the
call (the moving-GC discipline). Whole-function opt-in: any unsupported opcode →
silent interpreter fallback. Tier-up at function entry on a call-count
threshold. This tiered up `nbody.advance()` (−82%) and `fib`.

**Phase 1.5 — loop OSR (on-stack replacement at back-edges). DONE.** A function
entered once but looping heavily never reached the call-count threshold, so its
loop ran entirely on the interpreter. OSR fixes this: a back-edge counter tiers
the function up mid-loop and **enters compiled code at the loop-header PC**, not
at PC 0. The emitter emits one prologue **trampoline per loop header** (back-edge
target) that branches to the header's body label; `JitFunctionCode::osr_entry`
enters there. Correct by construction: the baseline keeps every live value in the
frame array at each instruction boundary, and a loop header is a basic-block
boundary, so the interpreter's live registers are exactly what compiled code
reads — no frame reconstruction, no stack maps. The per-back-edge cost is one
branch + add + compare on an interpreter-resident counter (not the `Frame`, which
is cache-line-capped with no slack), `==`-gated so a frame that cannot tier
attempts OSR exactly once. Result: `mandelbrot` ~19×→~1.6× of node, `prop-access`
/`array-ops`/`sort` improved; verified failing-set-identical to the interpreter
with OSR forced (`OTTER_JIT_OSR_THRESHOLD=1`).

## 1.3 Forward plan (prioritized)

Ordered by ROI/risk. Each item is gated by §5 before it closes.

1. **Widen the baseline opcode subset — cheapest wins, unblocks whole functions.**
   A whole-function opt-in means one unsupported opcode disables the entire body.
   - **`Shl`/`Shr`/`UShr`/`Mod` (int32).** `typed-array.js` (~75× — our worst
     compute bench) is flat purely because its loop body uses `Shl`; the function
     never compiles. Mirror the existing `BitwiseOr` emit (guard_int32 ×2 →
     `lsl`/`asr`/`lsr`/`sdiv`+`msub` → box int32). Highest single ROI right now.
   - Sweep `OTTER_JIT_TRACE=1` over each compute bench, fix the first reported
     `Unsupported(op)` per hot function, repeat until it compiles.
2. **Inline the monomorphic property IC (§3.3).** `prop-access` (~50×) loops over
   property loads that currently each make a stub call into the interpreter IC.
   Emit the inline shape-guard + slot-load (up to 4 polymorphic entries) sharing
   the interpreter's `(fn,pc)` IC table, falling to the shared miss handler. This
   is the highest-leverage feature for `prop-access` and helps every property-
   heavy body.
3. **Inline allocation + write barrier (Track G2/G3, §3.6).** Replace the
   out-of-line allocator/barrier calls in compiled code with the inlined bump
   path + header-granular card-mark. Helps allocation-bound JIT bodies once (1)
   and (2) expose them.
4. **Loop OSR for call-containing loops.** The current back-edge counter uses a
   top-frame-index proxy that resets on intervening calls, so loops whose bodies
   call out (`sort` comparator, `array-ops` callbacks) do not OSR — their callees
   rely on function-entry tier-up instead. Revisit with a cheaper per-frame
   identity if (1)/(2) leave these benches as the bottleneck.
5. **Phase 2 — optimizing tier.** Only after the baseline subset is wide and the
   IC is inlined. SSA IR + type feedback + speculation + deopt on the Cranelift
   backend with user stack maps (§3.4, §4 Phase 2). This is what approaches 2–4×
   of node on numeric kernels; it is also the project's highest-risk surface
   (deopt × moving GC × stack maps), so it is sequenced last.

**Standing constraints (unchanged):** VM internals are fully reworkable (single
binary, no ABI); the GC is improvable scope, not a fixed constraint (Track G,
§3.6); nothing is cut, only sequenced; stability is a co-equal gate with perf on
every phase (§5).

## 2. Bottleneck profile (measured against code)

### 2.1 Dispatch overhead is structurally large

The hot loop is `dispatch_loop_inner` (`crates/otter-vm/src/lib.rs:3722`,
inner `loop {` at `:3735`). Per opcode, before any real work:

| Per-instruction cost | Location | Note |
|---|---|---|
| **Binary search to fetch instruction** | `executable.rs:181-186` (`instr_at_byte_pc` → `binary_search_by_key`) | **O(log n) every dispatch.** The single worst offender; should be O(1). |
| Reduction accounting | `lib.rs:3783` (`record_runtime_reductions`) | Static cost lookup + add, every op. |
| Budget checkpoint | `lib.rs:3784` (`enforce_runtime_budget_checkpoint`) | Branch every op; enforcement currently Observe-only (`runtime_budget.rs:16`). |
| Stack-depth observe | `lib.rs:3785` | Write every op. |
| Tracer `Option` check | `lib.rs:3790` | One `Option` test every op (body cold). |
| Plain `match op` + `continue` | `lib.rs:3813` | **Not** direct-threaded / computed-goto / tail-dispatch. Branch-predictor-hostile single indirect jump. |
| Variable-width operand decode | `crates/otter-bytecode/src/encoding.rs:102-112` | Per-operand kind byte + LE decode; no fixed-width fast path. |

Dispatch is register-based (`Frame.registers: SmallVec<[Value; 8]>`,
`frame_state.rs:53`), which is good — but the fetch+decode+bookkeeping envelope
around each op dominates on tight loops like `fib`/`mandelbrot` where the actual
op (an add, a compare, a branch) is a few ns and the envelope is multiples of
that. This is why `fib` (pure call+arith) is 40× and `prop-access` 97×.

### 2.2 Property access has no machine-code fast path

ICs exist and are good (`crates/otter-vm/src/property_ic.rs`): up to 4
polymorphic entries + sticky megamorphic terminal (`property_ic.rs:49,154`),
keyed per `(function_id, pc)` in interpreter-side tables
(`lib.rs:423,427,430`; `execution_context.rs:285-289`), guarded by a cheap
`u64` shape-id + `u32` atom-id compare (`property_ic.rs:471,488`). Shape ids are
VM-local integers (`object.rs:251`), transitions live in an interpreter-owned
side table (`shape_body.rs:20-21`). **But every IC hit still pays the full
interpreter dispatch envelope** around the load. `prop-access` being the single
worst bench (252× deno) is dispatch overhead stacked on top of an otherwise-fine
IC. Not cached: accessors, proxy, symbols, computed keys, deep prototype hits,
dictionary-mode objects >128 props (`property_ic.rs:20-21`, `object.rs:865`).

### 2.3 Arithmetic is value-tagged but envelope-bound

`Value` is NaN-boxed `u64` with a **distinct int32 tag** (`TAG_INT32 = 0x7FF9`,
`value/tag.rs:46-86`) separate from f64 — a real SMI fast path exists. `Add`
(`arithmetic_dispatch.rs:80-123`) checks string first, then `to_numeric_kind`;
Number stays tag-packed (0 allocations), only BigInt/string concat allocate.
So arithmetic itself is cheap — the cost on `fib`/`mandelbrot`/`nbody` is again
the dispatch envelope and the lack of register-resident, type-specialized
inlined arithmetic. No integer-specialized opcode path exists; every `Add` goes
through the generic `to_numeric_kind` funnel even in a monomorphic int loop.

### 2.4 No profiling/tiering infrastructure exists

There is **no** back-edge counter, hotness counter, or tier signal anywhere
(confirmed across `lib.rs`, `runtime_budget.rs`). The only loop-level hook is a
cooperative interrupt poll on negative branch offsets
(`operand_decode.rs:50-52`). Any JIT must add hotness instrumentation from
scratch — but the back-edge site already exists as the natural hook point.

### 2.5 What this means

The cheapest, highest-certainty wins are **not** the JIT — they are removing the
binary-search fetch and threading the dispatch. The JIT's job is to delete the
envelope entirely for hot code and to keep JS values type-specialized and
(eventually) register-resident.

---

## 3. Research: approach comparison and final recommendations

### 3.1 Tiering — recommendation: **2 tiers now (interp + baseline), optimizer deferred**

| Option | Verdict |
|---|---|
| Single baseline tier only | **Chosen for v1.** Maximum ROI/risk. Baseline never speculates → never deopts → no frame-reconstruction machinery needed. Mirrors V8 Sparkplug / JSC Baseline philosophy. |
| Jump straight to optimizing tier (Maglev/DFG-style SSA) | **Rejected for v1.** Requires SSA IR, type feedback collection, deopt, OSR exit, lazy/eager deopt state maps — months of work and the highest-risk interaction with the moving GC. Wrong first step. |
| 3+ tiers (Ignition→Sparkplug→Maglev→TurboFan analog) | **Rejected as a starting point, adopt incrementally.** Otter has exactly one tier today; adding two at once is unmanageable. Land baseline, prove it, then add the optimizer as tier 2. |

**Sparkplug-style "baseline without IR" vs Maglev/DFG-style speculative SSA.**
Baseline wins as the first tier decisively: it is a near-mechanical
bytecode→machine-code translation (otter's register bytecode maps almost 1:1 to
machine ops), it shares the interpreter's IC feedback verbatim, and it has no
deopt surface. It removes the entire dispatch envelope (§2.1) — which is the
dominant cost — without touching semantics. The speculative optimizer buys
type-specialization and LICM/inlining on top, but only matters *after* the
envelope is gone, and it is where all the GC-interaction risk concentrates.
Sequence them; do not merge them.

### 3.2 Backend — RECOMMENDED, split by tier, confirmed against the GC contract

> **Status: recommendation committed; dependency not yet added.** Earlier drafts
> deferred this, then briefly leaned copy-and-patch. Recon of the real GC
> contract (§3.5) plus a 2026 survey of **what actually ships in production** make
> the split decisive: **a Sparkplug-style template macro-assembler (dynasm-rs)
> for the baseline tier, Cranelift for the optimizing tier.** Copy-and-patch was
> evaluated and **rejected for the baseline** (rationale below). The
> recommendation is firm; only the *dependency* is gated on the prototype (the §5
> gate applies to infrastructure too). Do **not** add any backend crate until the
> prototype gate confirms the numbers and the GC-stress contract.

**Two tiers, two different jobs.** The *baseline* tier wants the lowest possible
compile latency (it runs on warm functions; latency is user-visible), the
simplest mapping from register bytecode to machine code, and **the cleanest
interaction with the moving GC**. The *optimizing* tier wants good register
allocation, SSA optimization, and stack-map support, and tolerates slow compile
(it runs rarely, on the hottest code). These pull in opposite directions; one
backend cannot be optimal for both.

| Option | Compile latency | Code quality | Multi-arch | GC interop fit | Verdict |
|---|---|---|---|---|---|
| **Template assembler (dynasm-rs)** | **Lowest** — single linear pass, no IR/regalloc; "almost instantaneous" (V8 Sparkplug) | Hand-tuned; V8 Sparkplug +45% JetStream over interpreter | **Hand-write each arch** (x64 + aarch64); dynasm-rs supports both, ARMv8.4 | **Cleanest** — reuse the interpreter frame; no ptrs in regs across safepoints; **zero stack maps** | **Baseline — CHOSEN** |
| Copy-and-patch (stencils) | Lowest — `memcpy` + patch | ~2–5× a switch-interpreter | Recompile C stencils per arch with clang | Same clean fit (memory-array) | **Rejected for baseline** — toolchain/artifact friction, no mature Rust crate, weak production track record |
| Cranelift | ~150 µs/fn (real regalloc), ~10× faster than LLVM | Good (~14% slower than LLVM) | **arm64 + x64 free** | **Forces stack maps**: regalloc makes ptrs register-resident across safepoints → spill via `ir::UserStackMap` | **Optimizing tier — CHOSEN**; rejected for baseline |
| LLVM (ORC/MCJIT) | **~2811 µs/fn** (~19× Cranelift) | Best | Yes | Heavy statepoints | **Rejected for any tier** |

#### Why a template macro-assembler for the baseline (the decisive case)

This is the **production-proven** baseline shape. V8's **Sparkplug** (the
"Sparkplug-style baseline" this doc already invokes as its philosophy, §3.1) is,
in its real implementation, a hand-written template macro-assembler: **no IR, no
register allocation, a single linear pass over bytecode, reusing the
interpreter's frame layout**, compiling "almost instantaneously" (+45% JetStream,
+41% Speedometer over the Ignition interpreter). JSC's Baseline JIT is the same
shape. dynasm-rs is the mature Rust realization of this approach (x86/x64 +
aarch64 to ARMv8.4, purpose-built for JITs, Wasmer-sponsored).

1. **Lowest compile latency, by construction.** One linear pass emitting machine
   code per bytecode op, **no IR, no regalloc** — the same "almost instantaneous"
   property as Sparkplug. Cranelift runs a real regalloc pass (~150 µs/fn);
   LLVM ~2811 µs/fn. For a *warm-function* baseline where tier-up latency is on
   the user's critical path, this is the right axis to optimize.
2. **Cleanest possible fit for the moving GC — the deciding factor, and it is
   Sparkplug's own trick.** Emitted code reads operands from, and writes results
   to, the JIT frame's **value array in memory** — which *is* the interpreter's
   `Frame.registers` window (`frame_state.rs:46-96`), reused exactly as Sparkplug
   reuses the Ignition frame. That array is already registered as a `FrameRoots`
   provider (`frame_roots.rs:19-58`) and traced precisely
   (`trace_frame_slots`, `frame_state.rs:428`), **so there are no live
   `Gc`/`Value` pointers in machine registers across an allocation safepoint at
   all.** The use-after-move hazard (`heap.rs:176-195`) cannot arise in v1
   baseline code, and **no stack-map infrastructure is needed** (§3.5). 1:1 with
   the rooting discipline the project already enforces.
3. **Full control where the design needs it.** The IC inline guard/load/store
   shapes (§3.3), the int32 guard + slow-path fall-through (§2.3), and the
   header-granular write barrier (§3.5) are all emitted as exact instruction
   sequences. A direct assembler expresses these precisely; copy-and-patch's
   coarse "holes" make per-site IC/barrier shaping clumsier.
4. **Near 1:1 with otter's bytecode.** Register-based ISA, pre-decoded
   `ExecInstr` (`executable.rs:446-468`), recoverable jump targets
   (`encoding.rs:155-172`) → one emit routine per hot opcode, branch fixups via
   dynasm labels.
5. **No build-time toolchain, no generated artifacts.** dynasm-rs assembles at
   Rust **compile time** (proc-macro) into emit calls — no clang/LLVM at build,
   no checked-in per-arch stencil tables, no Mach-O/ELF relocation-extraction
   tooling. `cargo build` stays toolchain-clean for contributors.

**Costs and risks (accepted, with mitigations):**

- **Per-arch hand-asm.** The one real downside: the hot-opcode emitter is written
  twice (x64 and aarch64), as V8/JSC do (they hand-maintain 4+ arches; otter needs
  2). **Mitigation:** keep all control flow / IC / barrier *logic* arch-neutral;
  only the final instruction emit is arch-specific. Start **arm64-only** (the dev
  target is `darwin/arm64`), add x64 once the shape is proven — the §5 gate runs
  per-arch.
- **`unsafe` is mandatory** (mmap RWX/`mprotect` W^X, transmute bytes → fn ptr).
  otter-vm **forbids** `unsafe` (`Cargo.toml` workspace `unsafe_code = "forbid"`);
  only otter-gc lifts it. So the JIT **cannot live in otter-vm** — the new
  `crates/otter-jit` crate must lift the ban exactly as otter-gc does (documented
  `[lints.rust]` opt-out), keeping all `unsafe` (including dynasm's executable
  buffers) encapsulated there behind a safe API to otter-vm.

#### Why copy-and-patch was rejected for the baseline

Copy-and-patch (Xu & Kjolstad, PLDI 2021) is technically elegant and has the same
clean memory-array GC story. It loses to the template assembler on **risk and
fit**, not on theory:

- **Toolchain + artifact burden.** Stencils are generated by compiling C
  templates with clang/LLVM at build time and extracting bytes + relocations.
  Keeping `cargo build` clang-free means committing **generated per-arch stencil
  tables** (CPython PEP 774's direction) plus a maintainer `xtask` and a
  per-format (Mach-O vs ELF) relocation extractor. That is a standing maintenance
  surface the template assembler simply does not have.
- **No mature Rust implementation.** 2026 survey: `dynasmrt` is a template
  assembler, not C&P; there is **no production copy-and-patch crate**. otter would
  build the stencil generator + patcher from scratch.
- **Weak production track record to date.** CPython 3.13/3.14's copy-and-patch
  JIT delivered **0–5% (sometimes negative)** in practice and was described as "a
  proof of concept dressed in a release"; Microsoft cut the Faster-CPython funding
  in 2025. The technique's troubles there were largely frontend/economics-specific
  (trace projection on stale monomorphic ICs; heavyweight refcounted ops leaving
  little dispatch to remove) — *not* a refutation of the backend — but it is a
  clear signal that copy-and-patch in practice has been finicky, while the
  template-assembler baseline (V8/JSC) has shipped and performed for years.
- **No `become` dependency either way.** (Rust lacks stable guaranteed tail
  calls; neither chosen path needs them — copy-and-patch can concatenate
  straight-line, and the template assembler emits ordinary branches.)

Copy-and-patch stays on the table only as a **future codegen experiment** for the
baseline if hand-asm maintenance ever becomes the bottleneck — not as v1.

#### Why Cranelift for the optimizing tier (and why not the baseline)

Cranelift is Rust-native, gives **free arm64 + x64 register allocation and
relocation**, and ships **user stack maps** (`ir::UserStackMap`, stable since
Wasmtime v25, 2024; in production for moving-GC wasm through 2026). Right
optimizing-tier backend: its compile latency is irrelevant on the hottest code,
and its regalloc + SSA optimization are exactly what tier 2 needs.

**Cranelift is the wrong baseline backend, for a precise reason.** Its value
*is* its register allocator — keeping JS values in machine registers across many
ops. But under the moving GC, a `Gc` pointer held in a register across an
allocation safepoint is a use-after-move bug. Cranelift's user stack maps solve
this by **spilling every live GC reference to a stack slot at each safepoint**
(confirmed: the frontend inserts spills/reloads; refs are *not* kept in registers
across the safepoint — fitzgen, "New Stack Maps for Wasmtime", 2024). So using
Cranelift *properly* at baseline drags the entire stack-map machinery — the
tier-2 GC complexity, the project's single highest-risk surface — into the first
JIT. The only alternative is to force all values into a memory array and not use
Cranelift's regalloc, which **throws away its sole advantage while still paying
its ~150 µs compile latency**. Both are strictly worse than the template
assembler for a baseline. Hence the split.

#### GC-interop risk, per backend (the §3.5 contract, made concrete)

| Backend (as baseline) | Live ptr in regs across safepoint? | Stack maps needed in v1? | Net risk |
|---|---|---|---|
| **Template assembler (memory-array)** | **No** (operands read/written to traced frame array each op) | **No** | **Lowest** — Sparkplug's frame-reuse trick; matches `FrameRoots` 1:1 |
| Copy-and-patch (memory-array) | No | No | Low GC risk, but toolchain/artifact + Rust-immaturity tax |
| Cranelift (regalloc on) | **Yes** | **Yes** (`UserStackMap` spill at every safepoint) | **High** — pulls tier-2 GC surface into tier 1 |
| Cranelift (regalloc neutered to array) | No | No | Pays Cranelift latency for interpreter-grade codegen — pointless |

#### Prototype gate (do this before adding any backend dependency)

A **throwaway** prototype (scratch branch, behind a `cfg`, never merged) that
compiles **`fib`** — already a bench; exercises call + int32 arith + compare +
back-edge branch + return, the canonical baseline target — via **two paths**, on
**arm64 (the dev target) first**. `fib` needs only ~8 opcodes (`LoadImm`/move,
`Add`/`Sub` with int32 guard + slow-path call, `LessThan`, `JumpIfFalse`, `Call`
into the existing `call_ops.rs:789` `invoke` path, `Return`), so both paths are
small.

**Path A — template assembler (dynasm-rs, the leading candidate):** one emit
routine per op into a dynasm `Assembler`; operands/results flow through the reused
interpreter frame array registered as a `FrameRoots` provider; reload-after-
safepoint on the `Call`/alloc sites; branch fixups via dynasm labels. Emit into an
mmap'd buffer flipped W^X (RW→RX) before execution.

**Path B — Cranelift (the optimizing-tier backend, sanity-checked at baseline):**
lower the same ~8 ops to CLIF via `cranelift-jit`, live values in SSA registers,
`UserStackMap`-marked so the frontend spills them at the `Call` safepoint.

*(Copy-and-patch is not built in the gate — it was rejected on toolchain/risk
grounds above, not on a number the prototype would produce. Revisit only if Path A
hand-asm maintenance proves intolerable.)*

**Measure (record the numbers back into this section):**
1. **Compile latency** — wall-clock µs to produce executable code for `fib`'s
   bytecode, each path. Expectation: template-asm ≤10 µs; Cranelift ~100–200 µs.
2. **Steady-state ns/op** — `fib(32)`, compiled vs the current interpreter, each
   path (§5 min-of-N methodology). Expectation: both crush the interpreter
   (envelope gone); measure Cranelift's code-quality edge over template-asm here.
3. **GC-rooting complexity & correctness** — implement each path's rooting, run
   under **`OTTER_GC_STRESS=full`** (`heap.rs:236-256`). Record: did template-asm
   pass with the reused-frame / traced-array model and **zero stack maps**? Did
   Cranelift require stack maps to pass? LOC + `unsafe` surface of each.

**Decision criteria (falsifiable, in priority order):**
- **PRIMARY (GC simplicity).** If template-asm passes `OTTER_GC_STRESS=full` with
  the reused-frame model and no stack maps, while Cranelift needs stack maps to
  pass → **template-asm wins baseline.** This is the dominant risk axis (§3.5) and
  the expected outcome.
- **SECONDARY (compile latency).** If template-asm compiles ≥10× faster
  (expected) → reinforces the choice for a warm-function tier.
- **TERTIARY (code quality).** Only overrides if Cranelift's baseline ns/op beats
  template-asm by **>2×** *and* its stack-map rooting passes stress cleanly —
  unlikely, and even then the tier-2 GC surface in tier 1 is a poor trade.
- **KILL SWITCH.** If hand-written dynasm emit for the hot-opcode set proves
  unexpectedly costly on the *second* arch (x64), reconsider copy-and-patch
  (clang generates the second arch) before considering Cranelift-everywhere — but
  only with the toolchain/artifact cost above eyes-open.

Only after this gate passes: add the chosen baseline backend (no dependency lands
before it), record the measured numbers here, and begin Phase 1 (§4).

#### Prototype gate — live results (recorded as milestones land)

Throwaway crate `jit-proto/` (standalone, outside the workspace, **not merged**).
Host: `darwin/arm64` (Apple Silicon), release build.

- **Milestone 1 — Path A toolchain + compile latency + codegen ceiling. DONE.**
  dynasm-rs 3.2.1 (`dynasmrt::aarch64`). Results:
  - **Toolchain executes JIT code on Apple Silicon** — emit `ret 42` and a native
    recursive `fib` both run correctly. dynasm-rs's `ExecutableBuffer` handles
    `MAP_JIT` + `pthread_jit_write_protect_np` W^X; no manual unsafe mmap needed.
    This clears the single biggest Path-A unknown. (Local unsigned binary; the
    `allow-jit` entitlement is a *signed-distribution* concern only, §3.7.)
  - **Compile latency: min 7.25 µs / median 9.17 µs** to assemble+finalize the
    ~14-op fib body — and that *includes* a fresh `Assembler` allocation + mmap
    each compile (production would reuse). ~15–20× under Cranelift's expected
    ~150 µs/fn. Confirms the template-asm "lowest latency" premise.
  - **Codegen ceiling: native fib 1.17 ns/call** vs **interpreter ~532 ns/call**
    (otter `fib.js` 2328 ms min − 10 ms startup, over 4 356 617 calls). The
    1.17 ns is pure-int native (no tagged Values / VM call / GC) — the absolute
    floor, ~455× headroom; the realistic tagged baseline lands well above it but
    far below the interpreter.
- **Milestone 2 — tagged NaN-box codegen cost. DONE.** Re-emitted fib on tagged
  `u64` Values (otter layout: `TAG_INT32 = 0x7FF9` in top 16 bits, payload in low
  32, `value/tag.rs:46-86`): int32 guard on entry (`lsr`/`cmp`/predicted `b.ne`
  to a trap stub), checked sub/add with re-boxing (`orr`), self-recursive
  compiled→compiled calls. Result: **tagged-jit 1.18 ns/call ≈ native 1.17
  ns/call — tag overhead vs native ≈ 1.0×.** The guard and box/unbox are absorbed
  by the pipeline (the always-not-taken guard predicts perfectly). vs interpreter
  ~532 ns/call = **~450× faster** on this path. **Key finding: NaN-box tagging is
  not the cost — the dispatch envelope was.** Caveat: this models direct
  compiled→compiled recursion (optimistic floor); real recursion through the VM
  call path adds frame-setup cost on top (M2b/M3).
  - *Pending:* **M3 (PRIMARY axis)** — frame value array as a `FrameRoots`
    provider, reload-after-safepoint, correctness under `OTTER_GC_STRESS=full`
    (links real `otter-gc`); then **Path B** (Cranelift) for the same, to settle
    the tertiary code-quality comparison.

**Provisional gate verdict (after M1+M2): template assembler CONFIRMED for the
baseline, pending the M3 GC-stress check.**

- **SECONDARY axis (compile latency) — measured, decisive.** Template-asm 3–9 µs
  vs Cranelift's expected ~150 µs/fn — ~20–50× in template-asm's favor, exactly
  as predicted for a warm-function tier.
- **Codegen quality — measured, sufficient.** Tagged fib at ~native cost
  (1.0× overhead) means a baseline template assembler already produces tight code
  on the hot path; Cranelift's optimizer edge is a tier-2 concern, not a baseline
  differentiator.
- **PRIMARY axis (GC rooting) — answered structurally, M3 confirms.** The
  template-asm baseline **reuses the interpreter's own frame array**, which is
  already a `FrameRoots` provider (`frame_roots.rs`, `heap.rs:289`) that already
  survives moving scavenge under `OTTER_GC_STRESS` in production. The JIT adds
  **no new rooting mechanism** — only the "reload pointer from its slot after a
  safepoint" codegen discipline, which is the same rule the project already
  enforces by hand (memory: prototype-chain / CommonJS-loader use-after-move
  fixes). Cranelift, by contrast, would force `UserStackMap` spills into the
  baseline (§3.2). So the GC axis favors template-asm by construction; M3 is a
  confirmation test, not an open question.
- **Path B (Cranelift baseline) — deprioritized.** It cannot win the two
  higher-priority axes for a baseline tier, so building it would only refine a
  tertiary number. Cranelift remains the committed optimizing-tier backend.
- **Where M3 belongs.** Re-cloning otter's `Value → *mut RawGc` tracer inside the
  throwaway risks testing the clone, not the engine. The faithful GC-stress check
  is **Phase 1 step 1** against the real `otter-vm` frame + real tracer, gated by
  §5 (`OTTER_GC_STRESS=full`). The throwaway has served its purpose: toolchain,
  latency, and codegen cost are all de-risked.

### 3.3 Inline caches in JIT — recommendation: **share the interpreter IC table, emit inline guards + shared miss handler**

The interpreter already keys ICs by `(function_id, pc)` into side tables
(`execution_context.rs:285-289`, `lib.rs:423/427/430`). The JIT must read and
write the **same** `PropertyIcEntry` storage so interpreter and JIT see one
unified feedback stream (no double-warmup, no divergence).

Evolution of a load site in JIT machine code:

```
; monomorphic (1 cached shape)            ; reuses property_ic.rs entry data
  load   r_shape   = [obj + shape_off]    ; object::shape_id, object.rs:814-817
  cmp    r_shape, <cached_shape_id:u64>   ; guard, property_ic.rs:471 (u64 cmp)
  jne    .miss
  load   r_dst     = [obj + <slot_off>]   ; cached PropertySlot offset
  jmp    .done
.miss:
  call   ic_miss_load(site_id, obj, key)  ; shared runtime fn; updates the
                                          ; SAME PropertyIcEntry, returns value
.done:
```

- **Monomorphic → polymorphic**: emit a short chain of up to 4 guard/load pairs
  (matching `MAX_PIC_ENTRIES = 4`, `property_ic.rs:49`), fall to miss handler.
- **Megamorphic**: when the shared entry is `Megamorphic` (`property_ic.rs:154`),
  skip inline guards and emit a direct call to a megamorphic lookup stub
  (hash probe by shape+atom), same terminal state the interpreter uses.
- **Store** sites mirror this, including the add-transition records
  (`StorePropertyIc::OwnAddTransition`, `property_ic.rs:558-560`) — the JIT
  emits the shape-transition write inline, with a write barrier (§3.5).
- **Not cached in interpreter ⇒ not inlined in JIT**: accessors, proxy, symbols,
  computed keys, deep prototype, dictionary mode all fall straight to the shared
  runtime path. No new fast paths invented at the JIT layer in v1.

This is the highest-leverage JIT feature for `prop-access` (§2.2): same IC
logic, zero dispatch envelope.

### 3.4 Speculative optimization + deopt — recommendation: **baseline does not speculate; the optimizing tier (committed, sequenced after baseline) owns deopt**

This is *sequencing*, not scope-cutting (see Scope §). The optimizing tier and
its deopt machinery are committed work; they are built **after** the baseline
because the baseline is what they tier up from and what a deopt exits *to*.

**The baseline tier does not speculate**, therefore it **does not deopt**. Every typed
fast path it emits (int32 arithmetic, monomorphic IC) has an inline guard with a
fall-through to the existing shared runtime path — a slow path, not a deopt.
This removes the single largest risk from the first JIT.

What tier 2 needs, minimally:
- **Type feedback**: extend IC entries / add lightweight per-site type profiles
  collected by *both* interpreter and baseline (the back-edge and value sites
  are the collection points). No feedback collection is needed for v1.
- **OSR (on-stack replacement)**: see §4 phasing. v1 baseline tiers up at
  **function entry** (call-count trigger) only. **Loop OSR** (enter compiled code
  mid-loop at a back-edge) is Phase 1.5 — it is what `mandelbrot`/`nbody` need,
  since they iterate heavily inside a function that is entered once.
- **Deopt**: only tier 2. Recommendation: **lazy deopt** as the default (mark the
  frame, exit at the next safepoint/return) with **eager deopt** only where a
  guard's continuation is unsafe. Frame reconstruction rebuilds an interpreter
  `Frame` (`frame_state.rs:47`) from the compiled frame using a per-safepoint
  side map (compiled-location → bytecode register/pc). Because the interpreter
  frame format is explicit and stable, reconstruction is tractable.

### 3.5 Moving-GC compatibility — the real blocker, and why it is tractable

Facts that shape everything (all confirmed in recon):

1. **GC is cooperative, not preemptive.** Collection happens **only at
   allocation slow paths** (`heap.rs:846-853`, `:1227-1265`, `:520-544`) — never
   at arbitrary PCs. **Consequence: the JIT needs safepoints only at allocation
   sites and calls, not everywhere.** This is the single fact that makes a
   baseline JIT feasible without a full stack-map infrastructure.
2. **Rooting is precise via a `FrameRoots` provider stack**
   (`frame_roots.rs:19-58`): providers are pushed on dispatch-loop entry and the
   GC calls `trace(&mut |slot: *mut RawGc|)` to visit exact root slots. **There is
   no conservative native-stack scan** (`frame_roots.rs:1-15`). The interpreter's
   `Frame.registers` are traced precisely through this mechanism.
3. **Objects move on young scavenge** (Cheney copy, `scavenger.rs:1-11`,
   semispace flip `:206-210`); old-gen is non-moving. Pointers are **32-bit cage
   offsets** decompressed via `cage_base() + offset` (`compressed.rs:164-179`,
   `:119-122`).
4. **A `Gc`/`Value` held in a native local across an allocation, unrooted, is a
   use-after-move bug** — documented and weaponized via `OTTER_GC_STRESS`
   (`heap.rs:176-195`). This is the exact hazard the project already fights in
   the interpreter (see memory: prototype-chain corruption, CommonJS-loader
   corruption).
5. **Write barrier required on every heap pointer store** (`barrier.rs:18-99`):
   old→young store marks the parent **header's** card dirty (header-granular,
   `barrier.rs:22-36`); card size 512 B (`page.rs:62-64`). The insertion barrier
   is dormant in Phase 1 (`marking.rs:49-53`).
6. **Bump allocation is inlinable** (`page.rs:298-313`, `#[inline]`): load
   cursor, `cursor + size <= PAGE_SIZE`, bump, return offset; cold slow path is
   `#[cold] #[inline(never)]` (`heap.rs:518-519`).

**Recommendation for v1 — the "traced register array" model (no stack maps).**
Compiled functions keep all live JS values in a **fixed register array owned by
the JIT frame and registered as a `FrameRoots` provider** — exactly the
mechanism the interpreter already uses for `Frame.registers`. Implications:

- The GC traces the JIT frame's value array precisely via the existing provider
  contract. **No Cranelift stack maps in v1.**
- At any allocation/call (the only safepoints), live values are already in the
  traced array, so they survive a move. **After** the allocation returns, the
  JIT **reloads** any object pointers it needs from the array (they may have been
  rewritten in place by the scavenger). This is the machine-code analog of the
  interpreter's "read the relocated value back after alloc" discipline that the
  project already enforces.
- **Write barrier**: every store of an object pointer into a heap object emits a
  call to the shared `write_barrier` (`heap.rs:1653-1693`) — or an inlined
  card-mark — with the parent **header**, never the slot address
  (`barrier.rs:22-36`). v1 may start with an out-of-line barrier call and inline
  it later.
- **Inline allocation**: v1 calls the shared allocator (it is already a cheap
  bump path); inlining `bump_alloc` (§3.5.6) is a later optimization.

**Why not stack maps in v1.** Keeping values in machine registers across
safepoints is what *requires* stack maps and is where the moving GC bites
hardest. Deferring register-residency-across-safepoints to tier 2 (where
Cranelift user stack maps carry the live-reference set at each safepoint) lets v1
ship correctly against the moving GC with the rooting tools that already exist.
The cost is that v1 spills/reloads around safepoints — acceptable, because
removing the dispatch envelope is the dominant win and most hot inner work
(arithmetic, comparisons, branches) sits *between* safepoints where values stay
in registers.

### 3.6 GC as a first-class workstream (Track G) — committed, parallel

The GC is not a fixed constraint; it is improvable scope. Track G runs in
parallel with the JIT phases and carries its own stability + perf gates. It
serves two masters at once: **engine stability** (the use-after-move bug class
is the project's recurring crash source — see prototype-chain and CommonJS-loader
corruption in history) and **JIT throughput** (inline allocation, cheap
barriers, register-resident roots).

**Current GC state (verified, not assumed):**
- Moving young-gen (Cheney copy, `scavenger.rs:1-11`), non-moving old-gen,
  32-bit pointer compression (`compressed.rs:164-179`).
- **Old-space IS bounded.** A growth-ratio major-GC trigger already exists
  (`heap.rs:70-87` `MAJOR_GC_GROWTH_NUM/DEN = 3/2`, fired by `maybe_major_gc`
  `heap.rs:1227-1265`, clamped to a ~92% cage softcap). Earlier notes of
  "unbounded old space / collect_full only on cap path" are **stale** — that
  hole is closed.
- Young-gen retention OOM handled via overflow-to-old.

**Track G items (all committed):**

- **G1 — Rooting-hazard static lint (highest stability ROI).** The use-after-move
  hazard ("a `Gc`/`Value` held in a native local across an allocation, unrooted"
  — `heap.rs:176-195`) is the single recurring crash class, and it is *exactly*
  the invariant the JIT must also honor. Build a Rust MIR-level lint (clippy-style
  driver or a custom dylint) that flags a live `Gc`/`Value` held across a call
  that may allocate, without a rooting scope. This permanently retires the bug
  class for both the interpreter and the JIT and removes the chief risk of Phase 1.
  Keep `OTTER_GC_STRESS` (`heap.rs:236-256`) as the dynamic oracle alongside it.
- **G2 — Inline allocation for JIT.** Promote the `#[inline] bump_alloc`
  (`page.rs:298-313`) into a JIT-emitted fast path: load cursor, `cursor + size
  <= PAGE_SIZE`, bump, return offset; branch to the shared `#[cold]` slow path
  (`heap.rs:518-519`) on page-full. Removes a call per allocation in hot code.
- **G3 — Inline write barrier.** Inline the header-granular card-mark
  (`barrier.rs:22-36`, `page.rs:62-64`) into JIT pointer stores instead of an
  out-of-line `write_barrier` call (the v1 baseline starts out-of-line, §4
  Phase 1; G3 inlines it once correct).
- **G4 — Keep the moving collector; reject conservative scan.** Pointer
  compression (the 4-byte `Gc`) depends on precise rooting; a JSC-style
  conservative native-stack scan is incompatible with compaction + compression
  and is rejected. The path forward is *better precise rooting* (G1 + Cranelift
  stack maps in the optimizing tier), not abandoning the moving design.
- **G5 — GC throughput tuning (measured, not speculative).** Only after G1–G3:
  revisit promotion age, young-space sizing, and major-GC growth ratio against
  the `json`/`array-ops` allocation-heavy benches, gated by §5. No blind tuning.

**Sequencing.** G1 lands **before or alongside Phase 1** (it de-risks the JIT's
rooting). G2/G3 land **with Phase 1** (the JIT needs them). G4 is a standing
decision. G5 follows once the allocator/barrier shape is stable.

**Capability model.** The JIT changes *how* code runs, not *what* it may do.
All capability checks (`fs_read`/`net`/`env`/`subprocess`/`ffi`) live behind the
same runtime entry points the JIT calls for any non-trivial operation; the JIT
emits no syscall or capability-gated operation inline. No bypass is introduced.

### 3.7 Why the JIT is mandatory, and the deployment constraint it carries

**Mandatory for competitiveness.** After Phase 0, otter is still 24–71× slower
than Node on compute (`benchmarks/results/latest.md`: mandelbrot 24×, nbody 24×,
fib 27×, typed-array 39×, prop-access 71×), and Phase 0 is now exhausted (§1.5
step 3: interpreter micro-opts measure as a wash). Every leading JS runtime is
JIT-based — V8 (Sparkplug→Maglev→TurboFan), JSC (Baseline→DFG→FTL; Bun is JSC).
An interpreter-only engine does not compete on compute; that gap is structural,
not a tuning problem. The baseline JIT closes most of it (call/IC-heavy benches
to single-digit ×); the optimizing tier (Phase 2) is what approaches parity on
numeric kernels. JIT is therefore committed scope, not optional polish — for
compute-bound workloads. (Startup/IO-bound work is already served by the
interpreter + GC and does not need the JIT.)

**Deployment constraint — executable memory, and the entitlement myth.**
Executing JIT code needs writable→executable memory, which several platforms
gate. This is **not** a user-facing permission prompt (no TCC dialog; users grant
nothing), and it is **backend-independent** (a template assembler, copy-and-patch,
and Cranelift all emit+execute machine code and all hit the same constraint — it
does not affect the §3.2 choice). What it requires, by platform:

- **macOS (desktop/server, the primary target):** the binary must be code-signed
  with the `com.apple.security.cs.allow-jit` entitlement under hardened runtime;
  this is a notarization-approved exception and is exactly how Node, Deno, and Bun
  ship. It is a *build/signing* concern otter controls, invisible to users. On
  Apple Silicon the JIT pages use `MAP_JIT` + `pthread_jit_write_protect_np` W^X
  toggling (the dynasm-rs `ExecutableBuffer` handles this).
- **Locked-down platforms (iOS, some sandboxes, SELinux/W^X-enforced containers):**
  JIT may be forbidden outright.

**Required design response — runtime-optional JIT with silent interpreter
fallback.** otter is interpreter-complete today, so the JIT is purely additive.
The engine must **detect at runtime whether executable memory can be obtained and
fall back to the interpreter** when it cannot (missing entitlement, iOS, locked
sandbox, the macOS 26 page-protection bug, etc.) — no hard failure, just slower.
This makes the deployment constraint a non-blocker: the signed desktop/server
build runs the JIT; everything else still runs correctly on the interpreter. The
fallback path is the same code that exists now.

---

## 4. Implementation plan (ordered by ROI/risk)

> The current status and ordered next work are §1.2 / §1.3 — those are
> authoritative. This section is the **original per-phase detail** (build list,
> modules touched, risks, rollback) kept as reference; Phases 0, 1, and 1.5 have
> shipped, Phase 2 has not.

Each phase lists: what is built, crates/modules touched, target bench + expected
delta, risks, and a rollback checkpoint. **Gate rule for every phase: not closed
until the target bench moves AND no other bench regresses** (§5).

### Phase 0 — Interpreter dispatch surgery (cheapest, no GC risk) — IN PROGRESS

**Build:**
- ✅ **DONE** — Replace per-instruction `binary_search_by_key` fetch with O(1)
  `byte_to_instr` dense map (`executable.rs`). Largest single win (see §1.5).
  Fixed-width re-encode was *not* needed — the VM already executes from a
  pre-decoded `ExecInstr` array, so the search was pure overhead.
- ✅ **DONE (partial)** — Per-op envelope: the three metering calls
  (`lib.rs`) are inlined into one `#[inline]` accumulate + inlined depth-max +
  a hoisted `enforce_budget`-gated checkpoint; dead helpers deleted. The tracer
  `Option` check is left (one predicted branch; cheap). Full register-resident
  batching was rejected — hundreds of `?`-exits make a guaranteed flush
  infeasible without a large restructure, for ~5% on the best case.
- ⏭️ **Threaded dispatch — DROPPED for now.** In stable Rust the `match op` over
  the `#[repr(u8)]` opcode is *already a jump table*; true token-threading needs
  unstable `become`/explicit tail calls. Limited upside, high cost/risk. Revisit
  only if profiling shows dispatch misprediction dominates after simplification.
- ✅ **DONE (negative result) — frame-resolution hoist tried, reverted.** Caching
  the top frame's `(function_id, &ExecutableFunction)` to skip the per-op
  `covers_function`/`exec_function` lookups measured as a net wash (fib −2%,
  others flat-to-+1.5% noise; §1.5 step 3). The lookups were already cheap and
  inlined under release LTO. Reverted. **This closes Phase 0** — interpreter
  micro-optimization is exhausted; the next real win is the baseline JIT (Phase 1
  / §3.2 prototype gate), not more dispatch tuning.

**Touches:** `crates/otter-vm/src/lib.rs` (dispatch loop), `executable.rs`,
`runtime_budget.rs`, `arithmetic_dispatch.rs`.

**Achieved so far:** mandelbrot 47→24×, nbody 42→24×, fib 40→27×, typed-array
66→39×, typescript 36→23×, prop-access 97→71× (§1.5).

**Rollback checkpoint:** each change is an independent, verified commit; the
pre-Phase-0 binary + `benchmarks/results/baseline-pre-phase0.md` are the
regression oracle.

### Phase 1 — Baseline JIT (backend TBD per §3.2), function-entry tier-up

**Build:**
- New crate `crates/otter-jit`, invoked from the runtime integration layer.
  Backend chosen by the §3.2 prototype gate (Sparkplug-style template
  macro-assembler via dynasm-rs leading). The crate **lifts the workspace
  `forbid(unsafe_code)`** like otter-gc (documented `[lints.rust]` opt-out),
  encapsulating all `unsafe` (executable buffers, fn-ptr transmute) behind a safe
  API — the JIT cannot live in otter-vm, which forbids `unsafe`. Depends on
  `otter-bytecode`, `otter-vm` types; **no dependency from parked shims**
  (CLAUDE.md rule).
- CFG reconstruction from bytecode (jump targets are recoverable: relative
  byte-offset deltas, `encoding.rs:155-172`) → backend IR / stencil selection per
  function.
- Bytecode→Cranelift lowering for the hot opcode set: arithmetic with int32/f64
  guards (reusing `TAG_INT32`, `value/tag.rs`), comparisons, branches, register
  moves, calls (into the existing call path), and **inline IC guard/load/store
  stubs sharing the interpreter IC table** (§3.3).
- **Traced register-array frame** registered as a `FrameRoots` provider
  (§3.5); reload-after-safepoint discipline; out-of-line `write_barrier` calls
  on pointer stores.
- Hotness counter (function call count) → tier-up trigger at **function entry**;
  compiled code installed and dispatched in place of the interpreter for that
  function.

**Touches:** new `crates/otter-jit/`; `crates/otter-vm/src/` (call entry to
dispatch compiled code, `call_ops.rs`; expose IC table + `FrameRoots` provider
for JIT frames; expose shared `ic_miss_*` and allocator/barrier entry points);
runtime integration layer (tier-up policy).

**Target / delta:** call/IC-heavy benches. `fib` →~5×, `prop-access` →~6×,
`array-ops` →~8×, `json` →~4×. (Loop-bound benches largely unmoved until 1.5.)

**Risks (highest in the project):** moving-GC correctness — mitigated by the
no-stack-map model and by running the **entire JIT test suite under
`OTTER_GC_STRESS=1`** (`heap.rs:236-256`), which deterministically surfaces any
unrooted-across-alloc bug. Write-barrier omission → old→young edges lost →
silent heap corruption; mitigated by emitting the barrier on *every* pointer
store and stress-testing. Cranelift compile latency → only hot functions
compiled, cold path untouched.

**Rollback checkpoint:** JIT is feature-gated and per-function opt-in; disabling
the tier-up trigger reverts to pure interpreter with zero semantic change. Keep
the flag default-off until the gate (§5) passes.

### Phase 1.5 — Loop OSR (on-stack replacement at back-edges)

**Build:**
- Back-edge counter at the existing negative-offset branch site
  (`operand_decode.rs:50`) → trigger compilation + **OSR entry** mid-loop.
- OSR entry: build a compiled frame from the live interpreter `Frame`
  (`frame_state.rs:47`) at the loop header and resume in compiled code.

**Touches:** `crates/otter-jit/` (OSR entry generation), `crates/otter-vm/src/`
(back-edge instrumentation, frame handoff).

**Target / delta:** loop-bound benches. `mandelbrot` →~8×, `nbody` →~7×,
`typed-array` →~12×.

**Risks:** OSR frame handoff must map every live interpreter register to the
compiled frame's traced array exactly; an off-by-one loses a root. Stress mode
is the oracle.

**Rollback checkpoint:** OSR trigger is a separate flag from Phase 1 entry
tier-up; disabling reverts to entry-only tier-up.

### Phase 2 — Optimizing tier (speculative SSA, deopt, register-resident roots)

**Build:**
- Type-feedback collection (extend IC + per-site value-type profiles).
- SSA optimization in Cranelift IR with speculation (monomorphic inlining,
  int-specialization, LICM) guarded by type checks.
- **Deopt**: lazy-default + eager-where-needed (§3.4); interpreter-frame
  reconstruction via per-safepoint side maps.
- **Cranelift user stack maps** at safepoints → keep live references in machine
  registers across allocation/calls (removes the v1 spill/reload), precise roots
  reported to the GC at each safepoint.

**Touches:** `crates/otter-jit/` (optimizer, deopt, stack-map integration),
`crates/otter-vm/src/` (feedback hooks, deopt frame rebuild), `otter-gc`
integration for stack-map root reporting alongside `FrameRoots`.

**Target / delta:** numeric kernels toward 2–4× of Node; `mandelbrot`/`nbody`
the primary movers.

**Risks:** highest code-quality and correctness complexity; deopt + moving GC +
stack maps interacting. Only attempt after Phases 0/1/1.5 are stable and gated.

**Rollback checkpoint:** optimizer is a distinct tier above baseline; disabling
it falls back to baseline (still correct, still fast).

---

## 5. Continuous performance verification (built into every phase)

**Baseline-before-change discipline (non-negotiable).** Before any phase begins,
capture `benchmarks/results/latest.md` as a named baseline
(`benchmarks/results/baseline-pre-phaseN.md`). After changes, re-run and diff.
**Never close a regression** — a bench moving the wrong way blocks the phase even
if the target bench improved. This mirrors the project's existing rule of
verifying test262 failing-sets against a stashed baseline.

**Per-phase gate criteria:**
1. Target bench hits its stated × goal (§4).
2. **No** other bench in `benchmarks/` regresses beyond noise (use the existing
   min-of-5-runs metric, `benchmarks/results/latest.md:3`).
3. Full `cargo test --all --all-features` green.
4. **`OTTER_GC_STRESS=1` (and `=full`) green** for all JIT phases — the
   deterministic use-after-move oracle (`heap.rs:236-256`).
5. test262 failing-set unchanged vs the last committed run (no conformance
   regression from JIT semantics).

**Lightweight dispatch/IC microbench harness (proposed).** The full
`benchmarks/` run includes process startup and is coarse. Add a Criterion bench
in `crates/otter-vm/benches/` that isolates the signals a JIT moves:
- **Dispatch ns/op**: a tight bytecode loop (add + branch) measured in ns per
  iteration — directly tracks Phase 0 and the envelope.
- **IC hit rate**: dump the existing `PropertyIcStats` (`lib.rs:431`) hit/miss
  counters after a monomorphic and a polymorphic property loop.
- **Tier-up latency**: once Phase 1 lands, time-to-compile and
  interpreter-vs-baseline ns/op for the same hot function.
- **Back-edge counter trace**: once instrumented, expose OSR trigger counts.

These run fast enough for every commit and catch dispatch/IC regressions that
the full suite would average away.

---

## 6. Summary of decisions

| Decision | Choice | Rejected |
|---|---|---|
| First work | Interpreter dispatch surgery (Phase 0) | Jumping straight to JIT |
| Tiers (now) | 2: interpreter + baseline | Single optimizing tier; 3+ tiers at once |
| First JIT tier | Sparkplug-style baseline, no IR, no deopt | Speculative SSA first |
| Backend | **Split by tier**: Sparkplug-style template assembler (dynasm-rs) for baseline, Cranelift for optimizing tier; commit dependency only after the §3.2 prototype gate | Copy-and-patch for baseline (toolchain/artifact friction, no mature Rust crate, weak track record); one backend for both tiers; LLVM |
| IC in JIT | Share interpreter `(fn,pc)` IC table; inline guards + shared miss handler | Separate JIT IC; new fast paths |
| Speculation/deopt | Baseline never speculates; optimizing tier owns lazy-default deopt | Speculation in baseline |
| OSR | Function-entry first; loop OSR in Phase 1.5 | Loop OSR in baseline |
| GC roots (baseline) | Traced register array via existing `FrameRoots` provider; reload-after-safepoint | Cranelift stack maps in baseline |
| GC roots (optimizing tier) | Cranelift user stack maps at safepoints | — |
| Safepoints | Only at allocation sites + calls (GC is cooperative) | Pervasive safepoint polling |
| Write barrier | Emit shared `write_barrier` (header-granular) on every heap pointer store | Eliding barriers |
| GC scope | First-class parallel Track G: rooting lint (G1), inline alloc/barrier (G2/G3), keep moving collector (G4), measured tuning (G5) | Treating GC as fixed; conservative stack scan |
| VM internals | Reworkable (incl. fixed-width bytecode re-encode) — single binary, no ABI | Preserving bytecode ISA for its own sake |
| Deferral | Nothing cut; phases are execution order; stability is a co-equal gate | "Tier 2 = maybe later" |

---

## 7. Key citations index

- Dispatch loop & envelope: `crates/otter-vm/src/lib.rs:3722,3735,3778-3809,3813`
- Per-instruction binary-search fetch: `crates/otter-vm/src/executable.rs:181-186`
- Frame & registers: `crates/otter-vm/src/frame_state.rs:47-96,53`
- Arithmetic fast path: `crates/otter-vm/src/arithmetic_dispatch.rs:80-123`
- Calls: `crates/otter-vm/src/call_ops.rs:374-481,789-952`
- Back-edge hook: `crates/otter-vm/src/operand_decode.rs:41-55`
- No tiering infra: `crates/otter-vm/src/runtime_budget.rs:16,72-133`
- Value / NaN-box / int32 tag: `crates/otter-vm/src/value/tag.rs:46-86`, `value/mod.rs:1016-1029`
- Bytecode encoding / jump targets: `crates/otter-bytecode/src/encoding.rs:102-112,155-172`
- No IR: `crates/otter-compiler/src/compiler.rs:24-28`
- IC structure & states: `crates/otter-vm/src/property_ic.rs:49,139-154,471,488`
- IC keying & tables: `crates/otter-vm/src/execution_context.rs:285-289`, `lib.rs:423,427,430`
- Shapes / transitions: `crates/otter-vm/src/object.rs:251,814-817,865`, `shape_body.rs:20-21,184-196`
- GC algorithm & triggers: `crates/otter-gc/src/scavenger.rs:1-11,206-210`, `heap.rs:846-853,1227-1265,520-544`
- Pointer compression: `crates/otter-gc/src/compressed.rs:119-122,164-179`
- Precise rooting / no conservative scan: `crates/otter-gc/src/frame_roots.rs:1-58`, `handle.rs:44-56,123-139`
- Write barrier: `crates/otter-gc/src/barrier.rs:18-99,22-36`, `page.rs:62-64`
- Inlinable bump alloc: `crates/otter-gc/src/page.rs:298-313`, `heap.rs:518-519`
- Use-after-move oracle (GC stress): `crates/otter-gc/src/heap.rs:176-195,236-256`
- `unsafe` policy (JIT crate must lift it like otter-gc): workspace `Cargo.toml`
  `[workspace.lints.rust] unsafe_code = "forbid"`; `crates/otter-gc/Cargo.toml`
  `[lints.rust]` opt-out + `crates/otter-gc/src/lib.rs:39-41`
- Benchmarks: `benchmarks/results/latest.md`
</content>
</invoke>
