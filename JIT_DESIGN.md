# Otter JIT Design

> Status: design proposal (no implementation). Read-only recon grounded in real
> `file:line` citations against the current tree. All recommendations are
> single-choice with rationale and rejected alternatives, per the brief.
>
> Author note: written in English to match existing repo design docs
> (`ES_CONFORMANCE.md`, `OTTER_VM_PLAN.md`, `NODE_COMMONJS_DESIGN.md`).

---

## 1. Executive summary

Otter today runs **only an interpreter** with a plain `match op` dispatch loop
that performs an **O(log n) binary search to fetch every instruction**
(`crates/otter-vm/src/executable.rs:181-186`) and runs four bookkeeping calls
per opcode before the work even starts (`crates/otter-vm/src/lib.rs:3778-3809`).
It is 29–252× slower than Node/Deno/Bun (`benchmarks/results/latest.md`), worst
on `prop-access` (97× node / 252× deno) and dispatch/arithmetic-bound loops
(`fib` 40×, `mandelbrot` 47×, `nbody` 42×, `typed-array` 66×).

**Strategy in three sentences.** First, fix the interpreter's self-inflicted
dispatch tax (binary-search fetch, per-op accounting, no threading) — cheap,
zero GC risk, 2–4× across the board. Second, add a **single baseline JIT tier**
(Sparkplug-style, bytecode→machine code via **Cranelift**, no IR, no
speculation, no deopt) for hot functions, reusing the existing inline-cache
feedback table and the existing precise `FrameRoots` rooting mechanism so the
moving GC needs **no stack maps in v1**. Third — and only after the first two
land and hold — add an optimizing speculative tier (SSA IR, type feedback,
deopt, register promotion across safepoints) to close the residual gap.

**Target.** Phase 0 → worst case from ~250× to ~80×. Phase 1 (baseline JIT)
→ call-heavy benches (`fib`, `prop-access`, `array-ops`) to single-digit ×.
Phase 1.5 (loop OSR) → loop-bound benches (`mandelbrot`, `nbody`,
`typed-array`) to single-digit ×. Phase 2 (optimizer) → approach 2–4× of Node
on numeric kernels.

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

### 3.2 Backend — recommendation: **Cranelift**

| Option | Compile latency | Code quality | Multi-arch | GC stack maps | Deopt support | Verdict |
|---|---|---|---|---|---|---|
| **Cranelift** | Fast (designed for JIT/Wasmtime) | Good (regalloc, basic opts) | **arm64 + x64 free** | **User stack maps supported** | Build via metadata/traps | **Chosen.** |
| Custom template assembler (Sparkplug/JSC-style) | Fastest | Hand-tuned ceiling | Must hand-write each arch | Hand-roll | Hand-roll | Rejected — dual-arch hand assembly is a team-sized burden; otter targets `darwin arm64` today and x64 elsewhere. |
| LLVM (ORC/MCJIT) | **Terrible** (100×+ Cranelift) | Best | Yes | Statepoints exist but heavy | Heavy | Rejected — compile latency disqualifies it for any JIT tier short of a far-future top tier. |

**Why Cranelift, unambiguously.** It is Rust-native (no FFI/build friction in a
Rust codebase), it gives **arm64 and x64 register allocation and relocation for
free** — critical since the bench host is `darwin arm64`
(`benchmarks/results/latest.md:5`) but production spans x64 — its compile
latency is built for exactly this use case, and it supports **user-defined stack
maps** (`ir::UserStackMap` declared at safepoint instructions), which is the
mechanism the optimizing tier will need to keep live references in registers
across a moving-GC safepoint. The `cranelift-jit` crate handles executable
memory management.

**What Cranelift costs us.** Its compile time is higher than a pure template
assembler, so we **only JIT hot functions** (counter-triggered, never eager) and
we do **not** put Cranelift on the cold path. Deopt is not turnkey — we build it
in tier 2 from our own side metadata plus Cranelift traps. Both are acceptable
and addressed by phasing.

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

---

## 4. Implementation plan (ordered by ROI/risk)

Each phase lists: what is built, crates/modules touched, target bench + expected
delta, risks, and a rollback checkpoint. **Gate rule for every phase: not closed
until the target bench moves AND no other bench regresses** (§5).

### Phase 0 — Interpreter dispatch surgery (cheapest, no GC risk)

**Build:**
- Replace per-instruction `binary_search_by_key` fetch
  (`executable.rs:181-186`) with O(1) access: precompute a `byte_pc → ExecInstr`
  index, or store `current` instruction index alongside `pc` and advance it
  directly. (Largest single win.) **VM rework is permitted here** — if the
  variable-width encoding (`encoding.rs:102-112`) blocks O(1) fetch + cheap
  decode, re-encode the bytecode to fixed-width (single binary, no external ABI
  to preserve). This also pays off later: a JIT decodes fixed-width far cheaper.
- Gate the per-op envelope: fold `record_runtime_reductions` +
  `enforce_runtime_budget_checkpoint` + `observe_runtime_stack_depth`
  (`lib.rs:3783-3785`) behind a single branch or batch them at back-edges/calls
  instead of every op; compile out the tracer check (`lib.rs:3790`) when no
  tracer is installed (feature/cfg or a fast/slow loop split).
- Convert dispatch to **tail-call threaded** dispatch (one handler fn per op,
  `become`/explicit tail calls) or a computed-goto-equivalent, replacing the
  single `match op` (`lib.rs:3813`).

**Touches:** `crates/otter-vm/src/lib.rs` (dispatch loop), `executable.rs`
(fetch), `operand_decode.rs`, `runtime_budget.rs` (move accounting off hot path).

**Target / delta:** all compute benches; `fib` 40×→~15×, `prop-access`
97×→~35×, `mandelbrot` 47×→~20×. Whole-table 2–4×.

**Risks:** tail-call dispatch is unstable to express in Rust without care
(stack growth, `become` availability) — fallback is computed-goto via a function
table + loop. Budget-accounting relocation must preserve enforcement semantics.

**Rollback checkpoint:** each change is independently revertable; fetch-index
and dispatch-threading are separate commits. Baseline captured before Phase 0
(§5) is the regression oracle.

### Phase 1 — Baseline JIT (Cranelift), function-entry tier-up

**Build:**
- New crate `crates/otter-jit` (Cranelift + `cranelift-jit`), invoked from the
  runtime integration layer. Depends on `otter-bytecode`, `otter-vm` types;
  **no dependency from parked shims** (CLAUDE.md rule).
- CFG reconstruction from bytecode (jump targets are recoverable: relative
  byte-offset deltas, `encoding.rs:155-172`) → Cranelift IR per function.
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
| Backend | **Cranelift** | Custom template assembler; LLVM |
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
- Benchmarks: `benchmarks/results/latest.md`
</content>
</invoke>
