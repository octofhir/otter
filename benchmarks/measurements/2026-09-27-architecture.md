# Engine architecture decision: environments first, one native contract for JIT and AOT

Revision `cbb6e741` (after `894f3ffe` frame-cell batches), macOS ARM64 (Apple M1),
Rust 1.97.1 / LLVM 22.1.6. Node v25 (V8), Bun 1.3 (JSC). All raw data lives in
ignored `benchmarks/results/arch-2026-09-27/`; primary-source research and the
internal subsystem maps are committed in `research-2026-09-27/`.

Parity has not been reached. This document fixes the facts, the main measured
gap, the alternatives, the chosen architecture and its success criteria.

## 1. Facts

### 1.1 Fixed-work baseline (HEAD code, `otter-before-young-bindings`, SHA-256 8885cc24…)

Retired instructions and peak RSS from `/usr/bin/time -l`; whole process,
startup and compilation included (`scripts/dev/fixed-work.py`).

| Workload | Otter | Node/V8 | Bun/JSC | Otter/V8 | Otter/JSC | RSS Otter / V8 / JSC (MB) |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| ts | 198.6G | 21.2G | 20.6G | 9.38× | 9.65× | 663 / 503 / 468 |
| zlib | 273.4G | 23.1G | 23.4G | 11.82× | 11.70× | 795 / 80 / 177 |
| crypto | 17.0G | 2.88G | 2.23G | 5.89× | 7.62× | 48 / 53 / 30 |
| fib | 3.82G | 0.95G | 0.54G | 4.01× | 7.08× | 57 / 48 / 18 |
| mega_method | 10.1G | 4.03G | 1.64G | 2.51× | 6.15× | 57 / 50 / 19 |
| ast_ctor | 31.7G | 3.30G | 2.06G | 9.60× | 15.39× | 124 / 52 / 30 |
| earley-boyer | 229.1G | 18.7G | 21.7G | 12.27× | 10.56× | 128 / 190 / 86 |

Notes:
- V8 compiles zlib's `"use asm"` module through its asm.js→Wasm validator.
  `node --no-validate-asm` retires **35.6G** (85 MB RSS): the fair JS-JIT
  comparison for zlib is 23–36G. JSC has no asm.js path (23.4G is its JS JIT).
- Startup (`console.log(1)`): Otter 819M instructions / 59 MB; Node 269M / 48 MB;
  Bun 81M / 10 MB. Startup is 21% of fib and 8% of mega_method.
- After `894f3ffe`, Earley is 215.4G (−5.95%); other workloads unchanged.
- zlib RSS is JIT memory: `--jitless` peaks at 224 MB (601G instructions).
  The GC allocation census for zlib is ~0 bytes; ~610 MB is compiler/code memory.

### 1.2 Execution counters (production tier, per run)

From `otter-allocation-probe script` (counters added in `cbb6e741`):

| Counter | ts | zlib | crypto | fib | mega | ast_ctor | earley |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| generated JS calls | 70.7M | 5.3M | 4.8M | 13.5M | 0 | 2.0M | 154.0M |
| reentrant stub transitions | 34.7M | **44.3M** | 3.1M | 128 | 0 | 28.0M | 14.7M |
| runtime property stubs | 22.0M | 12K | 3.0M | 0 | 0 | 24.0M | 5.6M |
| alloc stub transitions | 2.1M | 0 | 153 | 0 | 0 | 2.0M | 10.2M |
| JIT→Rust call transitions | 10.0M | 1.1M | 17K | 128 | 0 | 4.0M | 0.6M |
| inline receiver allocations | 1.0M | 0 | 4.3K | 0 | 0 | 0 | 73.0M |
| template entries | 17.9M | 13 | 266K | 0 | 0 | 134 | 4.0K |

fib: (3.815G − 0.82G startup) / 13.46M calls = **222 instructions per
invocation** vs V8's 50.5 (both include compare/sub/add of the body).

### 1.3 Allocation census

| Workload | GC bytes | minor/full GC | pauses | dominant objects |
| --- | ---: | ---: | ---: | --- |
| earley | 10.81 GB | 626 / 244 | 2.56 s + 2.74 s | objects 73.4M × 96 B; binding cells 110.9M × 16 B; closures 10.2M × 151 B |
| ts | 1.02 GB | 38 / 5 | 0.43 s + 0.27 s | objects 3.2M × 96 B; slot slabs 2.0M × 158 B; strings 3.5M |
| ast_ctor | 0.40 GB | 13 / 13 | 0.06 s | slot slab 2.0M × 104 B + object 2.0M × 96 B per `new` |
| zlib, crypto, fib, mega | ~0 | ~0 | — | — |

Earley GC pauses are 5.3 s of 15.7 s. An `ObjectBody` is 96 B for any object
with ≤3 properties (V8: 12 B header + 4 B per field); ≥4 properties add an
old-space slab. A closure is ~112 B + 4 B per capture (V8: 28–32 B).

### 1.4 Where time goes (sample shares, not instruction shares)

Clean native profiles (`scripts/dev/cleanprof.sh`, no artifacts/events):

| Workload | Generated code | GC | closures/cells | other runtime |
| --- | ---: | ---: | ---: | --- |
| earley | 36% | ~27% | ~15% | `===` on non-numbers in Rust 8% |
| ts | 39% | ~2% | — | string-compare property lookup ~9%, generic call paths ~6%, interpreter ~1% |
| zlib | 69% | — | — | generic coercion/bitwise stubs + handle scopes ~9%, Machine compile ~8% |
| crypto | 80% | — | — | element stubs |
| mega_method | 78% | — | — | startup compile |
| fib | 61% | — | — | startup compile |
| ast_ctor | 11% | — | — | string-compare lookup 8.5%, `_super.call`/construct in Rust, runtime stores |

Sampling attributes time; instruction shares differ with IPC. Machine frames
have no frame-pointer chain, so only self samples are trustworthy.

Generated-code composition (artifact runs, `scripts/dev/hotasm.py`):
- zlib hottest function (attributed regions): typed element View/Address/Load/
  Store/Guard 80%, captured-binding guard/hit 17%. One `HEAP16[x>>1]` costs
  ~75 instructions: ~50 to re-derive the typed-array view on every access,
  bounds/address ~15, load ~10, each region materializing a success boolean
  consumed by the next region's `cbz`.
- earley hottest functions: `instanceof` probes, receiver allocation, shape
  proofs, store dispatch; 24.5 KB of code for 951 B of bytecode (≈250 B/op).
- fib call site (Machine): binding guard on the captured `fib`, callee identity
  guards, `ldar` entry cell, two stack checks, ~15 NativeFrame header stores plus
  undefined-fill, a push onto the thread frame array, a caller-side invocation
  counter, a root-record push, a `(value,status)` return with a 4-way status
  dispatch, then unpublish. Three parallel structures are maintained per call.

## 2. Primary-source research

Eight families were researched from source at pinned revisions and every
report's six most load-bearing citations were independently re-fetched and
verified or corrected (`research-2026-09-27/*.md`). Load-bearing findings:

| Mechanism | Primary source | What Otter spends instead |
| --- | --- | --- |
| One environment per scope instance; closure = {context, code} | V8 `Scope::MustAllocateInContext` (`src/ast/scopes.cc`, tag 15.5.35.10), `contexts.h`; JSC `JSLexicalEnvironment.h`, `JSCallee::m_scope`; SM `EnvironmentObject.h`; Hermes `CreateScopeInst` | one old-space cell per captured binding per activation (110.9M on Earley) and a per-binding reference array in every closure |
| Young inline closure/context allocation, barrier-free init stores | V8 `FastNewClosure`, `JSCreateLowering::ReduceJSCreateClosure` (forced `kYoung`), Turboshaft `MemoryOptimizationReducer::SkipWriteBarrier`; JSC DFG `compileNewFunctionCommon` ("activation must be young") | Rust runtime call, old-space free-list allocation, barrier scan of every new edge |
| Singleton scopes / context specialization fold captured constants | JSC `SymbolTable::singleton()`, `Graph::tryGetConstantClosureVar`; V8 `JSContextSpecialization::ReduceJSLoadContext`, `ContextCell::State` | a 14–20-instruction guarded load for every read of a module/IIFE-level binding (`fib`, zlib `HEAP*`, crypto helpers, TS namespaces) |
| Allocation sinking of closures + activations | JSC `DFGObjectAllocationSinkingPhase` (`PhantomNewFunction`, `PhantomCreateActivation`); SM `ScalarReplacement.cpp` (`MNewCallObject`); LuaJIT `lj_opt_sink.c` | all closures and cells materialized |
| Prototype in the map; validity cells | V8 `Map` `[prototype]`/`prototype_validity_cell` (`map.h`), `InvalidatePrototypeChains`; JSC Structure; SM Shape | per-hop prototype guards, shared transitions keyed by proto shape |
| Walkable frames, one frame layout across tiers | V8 Sparkplug; SM Baseline Interpreter = Baseline JIT codegen; JSC LLInt/Baseline; HotSpot OopMaps; Go funcdata/PCDATA | three frame records, per-call publication into three structures |
| One compiler serving JIT and AOT | .NET RyuJIT (JIT, crossgen2 ReadyToRun, NativeAOT ILCompiler); Wasmtime `.cwasm` zero-relocation code | JIT code embeds absolute addresses as movz/movk immediates; no object output |
| Backend | Cranelift (regalloc2, stack maps, `cranelift-object`), LLVM statepoints (Falcon, WebKit FTL→B3: 4.7× compile time) | own emitter already has regalloc2 + precise safepoints; lacks portable output |
| AOT of untyped JS | Static Hermes `SH.cpp` + tmikov discussion #1685: "not a performance improvement over a high tier JIT… predictable performance" | — |

BEAM/BeamAsm contributes shared runtime fragments and grouped allocation
checks (applied in `894f3ffe`); its isolated process heaps and immutable terms
are not transferable to JS. HotSpot/Graal Native Image contributes the image
heap and closed-world constraints; JS `eval`, `Function` and dynamic `import`
keep an explicit runtime compiler path in any Otter AOT mode.

## 3. The main measured gap

Excess instructions over V8 (Otter − V8), approximate decomposition. Every row
is an estimate from counters × per-event cost read from generated code, or
from sample shares; none is a measured instruction share.

| Mechanism | Estimate | Evidence |
| --- | ---: | --- |
| Per-operation cost of generated code (decode/guard/boolean-probe chains, element views, binding guards, property proofs) | ~45% | zlib 69%, crypto 80%, earley 36%, ts 39% of time in JIT code; element access ~75 vs ~5 instructions; property load ~38–40 vs ~4 |
| Environments, closures and the allocations/GC they cause | ~15% | earley: cells + closures + old-space GC ≈ 40% of time; binding reads inside the JIT rows above |
| Runtime transitions and generic runtime paths | ~12% | 44M (zlib), 35M (ts), 28M (ast) reentrant stubs at ~300 instructions of protocol each |
| Call linkage | ~6% | 222 vs 50 instructions per fib call; 154M Earley calls |
| Compilation time/memory | ~4% | zlib Machine compile ≈3.9 s, ~610 MB RSS |

The largest *single* architectural cause that cuts across rows 1, 2 and 4 is
the **environment model**: captured bindings are individual old-space cells,
closures carry per-binding reference tails with an absolute base pointer
(which pins closures in old space), frames copy spines, and every captured
read is a guarded multi-load chain. It is the biggest measured cost on the
largest-ratio workload (Earley), it sits on the hot path of every other
workload through module-level captured bindings (the CLI runs scripts in a
CommonJS wrapper, so top-level `var`/`function` are captured bindings: `fib`,
crypto's helpers, TS namespace objects, zlib's `HEAP*` views), it forces the
nursery bypass that makes GC expensive, and constant folding of captured
bindings (the only way zlib's typed-array views become hoistable constants)
needs an environment representation first.

## 4. Alternatives for the environment gap

| | E1: per-scope contexts (V8/JSC/SM/Hermes) | E2: flat closures + assignment conversion (Chez/OCaml/Go) | E3: keep per-binding cells, make them cheap (batches, young cells, tagged immutable slots, sinking) |
| --- | --- | --- | --- |
| Allocations (Earley) | ~20M contexts + 10.2M closures (measured count of scope instances with captures in the rejected grouping experiment: 20.3M) | boxes only for assigned captures; closures grow to ~12 captured Values each (≈130 B) | 110.9M cells remain; closures keep 4 B/capture tails |
| Closure creation | O(1): two stores | O(captures) copies | O(captures) copies |
| Captured read | ctx → depth hops → slot (2–3 loads); 0 with singleton folding | 1 load (+1 boxed) | 3–4 dependent loads + hole check |
| Memory | 32–40 B closures; context retains all captured slots of its scope | no false retention; duplicated values per closure | many 16 B objects; 4 B/capture/closure |
| JS semantics | exactly ES environment records: TDZ = hole, per-iteration copy, eval scope info, mapped arguments alias slots, generators keep context | needs initialization-dominance proofs for hoisting/TDZ; eval/with force boxing everywhere | unchanged |
| Compile cost | scope analysis already exists; slot assignment is linear | whole-function assignment/dominance analysis | proof analysis for tagged slots |
| JIT/AOT | static offsets; singleton folding in JIT only | static offsets | status quo |
| Infrastructure removed | UpvalueCell for function/block scopes, six spine forms, frame spine copies, `upvalue_base`, `jit_initialize_generated_upvalues`, FreshUpvalue double allocation, `UpvalueSource`, per-binding capture operands | spines, most cells | little |
| Prior evidence | rejected Otter experiment kept per-binding references *under* grouped storage (+14.8% RSS); every production engine uses E1 | Scheme compilers; Earley closures capture ~12 bindings each | measured −4…−6% steps; young-cell pilot regressed (confounded by old closures) |

**Decision: E1.** E2 multiplies closure size and creation cost on the
closure-heavy workload (≈12 captures per Earley closure) and needs whole-function
proofs that JS hoisting/TDZ/eval make fragile. E3 is the incremental path whose
steps measure in single-digit percents and cannot remove the per-binding
object model. E1 replaces the model: one object per scope instance, closures
with one context reference, young movable allocation, and a representation
that singleton folding and allocation sinking can act on. The previous
grouping attempt failed precisely because it kept the per-binding reference
coordinates in closures; E1 changes compiler coordinates and closure ownership
together.

## 5. E1 contract

### Final invariants
1. A binding lives in a register unless it is captured by a nested function,
   visible to a sloppy direct `eval`/`with`, or a mapped-arguments parameter;
   then it is a slot of its scope's **Context**. Module environment bindings
   (live import/export) and the global declarative environment keep their
   existing record kinds; they are different ES environment-record kinds, not
   an alternate storage for the same scopes.
2. A Context is one GC object `{header, scope descriptor, parent context,
   slots[n] (8-byte Values)}`. TDZ is the hole value in a slot. Contexts are
   ordinary movable objects allocated young.
3. A closure stores exactly one context reference. It has no capture tail and
   no absolute derived address; closures are movable and allocated young.
4. Every frame (interpreter, Template, Machine, parked generator) holds its
   current context in one traced slot; there are no upvalue spines.
5. Bytecode addresses captured bindings by `(depth, slot)` resolved at compile
   time; per-iteration and block scopes push/pop/copy contexts explicitly.
6. Direct eval resolves names through scope descriptors on the context chain;
   no parallel name→cell tables.
7. Every allocation of a context or closure is visible to the GC with its full
   payload initialized before any safepoint.

### Success criteria
- Earley: binding-cell allocations 110.9M → 0 for function/block scopes;
  closure bytes/closure ≤ 48; full GCs and GC pause share fall; instructions
  fall by ≥20% versus `894f3ffe`.
- No workload regresses beyond noise in instructions; RSS not worse on Earley.
- Gates: difftest, otter-jit lib, GC stress 1..16 in all tiers on the closure,
  eval, arguments, generator, class and loop corpora; targeted Test262
  directories for scope, closures, eval, arguments, generators, classes, for-let
  show no new failures (`ES_CONFORMANCE.md` before/after).

### Slices after E1 (each measured before starting)
1. Inline young allocation of contexts and closures in Template/Machine
   (V8 `FastNewClosure`; JSC `compileNewFunctionCommon`).
2. Singleton-context constant folding with invalidation (JSC `singleton()` +
   watchpoints; V8 `ContextCell`), then typed-array view folding for constant
   views — the zlib element-access path.
3. Native call/frame contract: fp-walkable frames with the existing safepoint
   maps, no per-call publication, callee-side tier budget, exception status
   only on runtime transitions; pinned cage-base/thread registers; stub calls
   through a per-isolate table (also the portable-code form).
4. Object layout: prototype in shape, fixed in-object slots sized by slack
   tracking, no `values_ptr`, one guard per access.
5. AOT on the same pipeline (below).

## 6. One foundation for JIT and AOT

Modes assessed:
1. **Bytecode/snapshot serialization** — startup only (Otter startup is 3×
   Node, 10× Bun); the flat bytecode format is already address-free.
2. **AOT native JS functions with runtime helpers** — the Template tier is
   feedback-independent (ICs are data cells), so its code is the natural
   AOT tier once stubs are called through a table and every heap/cell address
   is a symbolic relocation (the relocation vocabulary already exists and is
   captured for artifacts; a completeness check and a loader are missing).
3. **Profile-guided (guarded) AOT** — Machine code compiled from a training
   profile keeps deopt metadata and falls back to the interpreter/Template;
   requires the relocation kinds for shapes, atoms and function ids.
4. **Closed-world** — only for explicitly declared programs without `eval`,
   `Function` or dynamic `import`; never reported as full-JS performance.
5. **Standalone executable** — runtime binary + appended image (bytecode,
   code, relocations, metadata), loaded at startup; the runtime compiler stays
   present for eval/Function/dynamic import.

Backend decision: keep the single compiler (HIR → Machine IR → regalloc2 →
own emitter) and add a portable output mode that writes Mach-O/ELF objects via
the `object` crate. Cranelift and LLVM would add a second IR and do not address
the measured costs (environments, calls, guards, allocation), which are not
instruction-selection problems; they remain offline code-quality yardsticks.
AOT is not assumed faster than JIT; it is measured separately (build cost,
executable/code/data size, startup, instructions, RSS, remaining runtime
compilation paths).

## 7. GC soundness defects found during this review

1. Allocation-triggered full collections (`GcHeap::collect_full`) never ran the
   WeakMap ephemeron fixpoint or WeakRef/FinalizationRegistry processing; only
   the debug `force_gc` did. WeakMap values reachable only through live keys
   were swept: all three tiers crashed on a 30-line program; `OTTER_GC_VERIFY`
   reported 9,731 corrupt slots. Fixed: the heap runs the VM's post-mark
   processor in every full collection.
2. FinalizationRegistry cell targets and unregister tokens were never rewritten
   when a scavenge moved them: `unregister` failed for every young token
   (0/500 vs Node 500/500) and dead-target checks read stale offsets. Fixed:
   registries are ephemeron tables with weak key walks.
3. `new WeakRef(target)` traced the caller's `&Value` through a shared
   reference across the body allocation and then re-read it; the compiler kept
   the pre-allocation bits. Under GC stress every WeakRef pointed at the object
   allocated next in the recycled nursery slot (`refs[i].deref() === held[i+1]`).
   Fixed: the target is rooted as a mutable local and read back from it.
Regression programs: `crates/otter-difftest/corpus/weakmap_growth_full_gc.js`
(old binary crashes in all tiers), `finalization_young_tokens.js` (old binary:
`unregister` 0/300, wrong `deref` identity under stress). Validation: difftest
61/61 (interpreter oracle, Template, production, stress 1/4/16 with slot
verification), otter-jit 283/283, VM weak/ephemeron 45/45, otter-gc 128/128.

Other verified findings that shape the slices: page survival age is never
reset and fresh objects share aged pages (premature tenuring); children of
remembered parents are promoted immediately; old-space allocation never
triggers growth-based major GC; `Value` cells hold full addresses yet every
JIT decode re-adds the cage base; Template property ICs never learn after
compilation; `TailCall` is unsupported in both JIT tiers (every strict
`return f(x)`).

## 8. E1a result: per-scope contexts replace upvalue cells

E1a lands the environment contract of
`2026-09-27-environments-contract.md` in every tier: per-scope context objects
in ordinary registers, one context word per closure, `Lookup*` operations over
an eval-extension chain, `DerivedThis` as a context slot. Upvalue cells, the
upvalue spine, `FreshUpvalue`, the `Shadowed*`/`EvalBinding*`/`*Dynamic`
families, the old-space cell batch allocator and the generated-upvalue
initializer are deleted, not bridged.

Fixed-work instructions (`scripts/dev/fixed-work.py`, release CLI, one
sequential run each; the whole process, JIT compile threads included):

| Workload | HEAD `ceb3ca18` | E1a | Δ | RSS HEAD → E1a |
|---|---:|---:|---:|---|
| ts-fixed | 198.6G | 184.7G | −7.0% | 695 → 535 MB |
| zlib-fixed | 273.4G | 243.1G | −11.1% | 834 → 722 MB |
| crypto-fixed | 16.98G | 16.82G | −0.9% | 51 → 52 MB |
| fib | 3.82G | 3.65G | −4.4% | 59 → 59 MB |
| mega_method | 10.12G | 10.01G | −1.1% | 59 → 59 MB |
| ast_ctor | 31.73G | 27.63G | −12.9% | 130 → 130 MB |
| earley-boyer | 229.1G | 184.5G | −19.4% | 134 → 154 MB |

(`benchmarks/results/arch-2026-09-27/e1a-final3` against `baseline-head`;
every stdout matches.)

Earley's RSS grows 15%: a context is a 32-byte body plus 8 bytes per slot
where a cell was 16 bytes, and contexts captured by old closures are promoted.
That is the first E1b target (below), not an accepted cost.

Defects surfaced while closing the E1a regressions; each has a regression
program in the difftest corpus:
1. Template routed every checked (TDZ) context access through the committed
   binding call: `mega_method` doubled (20.9G). The hit path is now inline on
   both targets (load, `hole` compare, barriered store); only a TDZ `hole`
   takes the committed call that throws.
2. Arguments elision treated the closure-context placeholder operand (`r0`
   until `finish_code` routes it) as a use of the `arguments` object whenever
   `arguments` lived in `r0`. Every CommonJS-wrapped function that reads a
   captured binding kept a materialized `arguments` object: earley spent 12%
   in `collect_arguments`. The analysis now skips placeholder operands.
3. Machine OSR entry (pre-existing, exposed by 2) is rebuilt on the Maglev /
   Warp model. The old design wrote each header value into its allocated
   location from a trampoline and landed on a marker instruction; with a
   versioned preheader the marker's parallel moves conflated two header
   values, and a spilled value's live register copy went stale
   (`osr_preheader_parameters.js`, `osr_entry_live_values.js`). Now the entry
   block is an `OsrDispatch` that reads the native frame's `OSR_ENTRY` bit and
   PC once, and the header gets an OSR block: an extra predecessor whose
   `OsrValue`s read and type-check every header parameter from the
   interpreter frame, so regalloc2 places every value and LICM's shared
   preheader needs no OSR special case. Like V8 (code per OSR offset) and
   SpiderMonkey (`osrPc`), a compile carries at most the one header whose
   back-edge requested it: an OSR block for every eligible header doubled the
   regalloc time of zlib's largest function (0.52 → 1.05 s per compile, +17%
   instructions), and an entry compile now has no dispatch at all. When the
   current code cannot enter a newly hot header, the VM recompiles for it
   (SpiderMonkey's `osrPc` mismatch recompile).
4. Forwarded calls (`f.apply(this, arguments)`) of a function whose mapped
   formals live in its context declined the Machine tier. The forward packet
   now ends with the formals context (Template and Machine publish it into
   the frame window, and the runtime reads context-held formals from it), so
   the mapped `arguments` aliasing stays exact
   (`forward_context_formals.js`).
5. An exceptional edge out of a call skipped the edits regalloc2 placed after
   the call and around the block terminator, so a catch received the SELF
   closure instead of the thrown value. Landing pads now run the same edit
   run as the normal path (`machine_catch_result_split.js`).
6. GC safepoints rooted only the location a root operand names. regalloc2
   assumes values are immutable: inside a block its redundant-move eliminator
   skips a move whose destination already holds a copy, so a context lived in
   both a callee-saved register (rooted, rewritten by the scavenger) and its
   spill slot (not rooted), and the edge into a loop preheader read the stale
   slot. Safepoint lowering now roots every location holding the same bits at
   the call (Ion's `populateSafepoints` rule), derived from a block-local walk
   of the allocator's edits, definitions and clobbers
   (`gc_root_register_spill_copy.js`; GC stress stride 4 used to crash the
   scavenger on a garbage header).

Two GC-stress defects older than E1a were fixed on the way: Map/Set natives
read their receiver before flattening a rope key (stale receiver after the
flattening allocation: Set iteration order diverged under stress), and the
`-p` completion value was unrooted across the event-loop drain.

## 9. E1b so far: equality, typed views, atoms, young closures

Fixed-work instructions (same method as section 8; stdout identical to E1a
for every workload):

| Workload | E1a `360a3244` | + inline `===`, typed views, atoms | + young closures `1903aa8e` | RSS E1a → now |
|---|---:|---:|---:|---|
| ts-fixed | 184.7G | 185.2G | 181.4G | 535 → 566 MB |
| zlib-fixed | 243.1G | 233.9G | 234.0G | 722 → 725 MB |
| crypto-fixed | 16.82G | 16.95G | 16.70G | 52 → 52 MB |
| fib | 3.65G | 3.65G | 3.65G | 59 → 59 MB |
| mega_method | 10.01G | 10.01G | 10.01G | 59 → 59 MB |
| ast_ctor | 27.63G | 27.62G | 27.63G | 130 → 130 MB |
| earley-boyer | 184.5G | 169.4G | 153.9G | 154 → 133 MB |

(`benchmarks/results/arch-2026-09-27/e1b-eq-view`, `e1b-young`.) Earley's
RSS is back under HEAD's 134 MB: a closure no longer drags its context into
old space, and short-lived closures die in the nursery.

- Strict equality is decided inline in both tiers (V8 `StrictEqual`): equal
  bits, then numbers, then distinct cells by type tag; only string and BigInt
  pairs call out.
- Typed-array element views (off-heap data pointer + length) are GVN'd and
  hoisted like any load keyed by a reentry epoch instead of memory versions:
  only a reentrant call or a shape/element-metadata write can invalidate them.
- Runtime property names are looked up by atom id; a name the interner has
  never seen cannot be an own property of a shaped object.
- Closures are allocated young. Moving closures exposed every Rust frame that
  held a user function, or a value next to one, across an allocation; each
  now reads it from a traced root afterwards (call-frame callee anchors,
  handle scopes in prototype materialization and closure property paths,
  anchors in the Array / Iterator / TypedArray callback natives, capability
  handles in the Promise combinators, rooted Proxy `ownKeys` validation).
  Some were older than this change (Promise combinators with a rejected
  element, Proxy `ownKeys`, `Iterator.zip` result getters); built-ins under
  `OTTER_GC_STRESS=16` went from 43 failures and crashes to 15 older ones.

### Where earley's time goes now

A `sample` of the isolate thread: generated code 59%, collector ~20%
(scavenger slot processing, copying, root scan), context allocation 4%,
closure allocation 1.6%. The allocation census explains the collector share:

| Body | Allocations | Bytes | Bytes / cell |
|---|---:|---:|---:|
| ordinary object | 73.4M | 7.04 GB | 96 |
| context | 20.3M | 1.54 GB | 76 |
| closure | 10.2M | 0.90 GB | 88 |
| string | 7.3M | 0.41 GB | 56 |

9.9 GB allocated per run, 695 scavenges (1.56 s of 9.8 s wall) and 34 full
collections (0.23 s). 73.0M of the 73.4M objects are constructor receivers
allocated by generated code, almost all `sc_Pair` with two fields: a 96-byte
cell where JSC's `JSFinalObject` needs 32 bytes (8-byte cell header, butterfly,
two inline slots) and V8 needs 20 (compressed map, properties, elements, two
fields). The ordinary-object layout is therefore the largest remaining lever
for allocation-heavy code, ahead of inline context allocation.

## 10. Ordinary-object layout

### How the reference engines lay out an ordinary object

| | Cell prefix before property slots | Where the prototype, extensibility, dictionary-ness live | In-object capacity |
|---|---|---|---|
| V8 | `map`, `properties_or_hash`, `elements` (12 bytes compressed) | Map: `prototype`, `bit_field3` (`IsExtensibleBit`, `IsDictionaryMapBit`, `IsPrototypeMapBit`) | Map `instance_size`; constructors start generous and shrink after 7 constructions (`kSlackTrackingCounterStart = 7`), literals get their property count |
| JSC | 8-byte cell header (StructureID + type/flags bytes) + `m_butterfly` | Structure: prototype, flags; dictionary Structures are per object | `JSFinalObject` inline storage right after the butterfly (`offsetOfInlineStorage() == sizeof(JSObjectWithButterfly)`), capacity from the allocation profile, default 6, max by cell size |
| SpiderMonkey | `shape`, `slots_`, `elements_` | BaseShape (proto, class, flags) | fixed slots by `AllocKind` |
| Otter today | 8-byte GC header + 80 bytes: shape, `values_ptr`, slab, 8-byte dictionary id, cache mode, `jit_proto`, three flag bytes, dictionary epoch, 8-byte exotic handle, 3 fixed inline slots, `slab_len` | object body | fixed 3; a fourth property moves every slot to the slab |

Every engine keeps one pointer to out-of-line storage and puts property
slots directly after a short prefix, sized per allocation site. None keeps a
cached pointer to the slot base: the hidden class decides at compile time
whether a slot is in-object (fixed offset) or out-of-line (one load, then a
fixed offset).

### Target

1. **L1 — capacity-sized in-object slots.** The fixed body shrinks to 32
   bytes (shape, slab, prototype, 4-byte exotic handle, dictionary epoch, the
   four flag bytes, dictionary id) followed by `N` trailing slots, `N` chosen
   per allocation site. The hidden class carries the capacity: one root shape
   per capacity, inherited by every transition, so slot `i` of shape `S` is
   in-object at a fixed offset when `i < S.capacity`, otherwise at
   `slab[i - S.capacity]` (V8's in-object / property-array split instead of
   Otter's all-or-nothing spill). `values_ptr` and `slab_len` go away; the
   slot count is the shape's (dictionary objects: their slot table's).
   Capacity per site: object literals their property count, `{}` 4 (V8's
   object function initial map), constructor receivers the learned field
   count (the existing profile, capped at JSC's 64 instead of 3). A 2-field
   `sc_Pair` goes from 96 to 56 bytes.
   **Landed.** Slot location stays all-in-object or all-in-slab (so the
   hidden class does not have to carry the capacity and one constructor's
   receivers never split across shapes by size); generated code branches on
   the slab handle and computes the spilled base as cage + handle + word
   offset instead of loading a cached pointer. The four flag bytes became one
   bit set, so a shape-state guard is one load and a bit test per flag.
   Fixed work (stdout identical):

   | Workload | young closures | L1 | Δ | RSS |
   |---|---:|---:|---:|---|
   | ts-fixed | 181.4G | 176.4G | −2.8% | 566 → 548 MB |
   | zlib-fixed | 234.0G | 234.0G | 0 | 725 → 759 MB |
   | crypto-fixed | 16.70G | 16.94G | +1.4% | 52 → 52 MB |
   | fib | 3.65G | 3.65G | 0 | 59 → 58 MB |
   | mega_method | 10.01G | 10.01G | 0 | 59 → 59 MB |
   | ast_ctor | 27.63G | 22.31G | −19.3% | 130 → 92 MB |
   | earley-boyer | 153.9G | 141.8G | −7.9% | 133 → 131 MB |

   crypto's objects spill; their slab base now materializes the cage base
   (four instructions) where it loaded one cached pointer — the case for a
   pinned cage-base register in the call/frame contract. Moving the
   receiver's fields in-object also admitted constructors with more than
   three `this.x` stores to the transition program, which exposed two older
   unrooted locals across its shape allocations (the prototype in
   `prepare_constructor_field_transitions`, the construct operands around
   `observe_class_constructor_field_transitions`).
2. **L2 — the hidden class owns the rest.** Prototype, extensibility,
   attribute/opacity state and dictionary identity move into the shape
   (per-object dictionary shapes as in JSC; `preventExtensions` is a shape
   transition as in V8), leaving shape + one backing handle + slots: a
   2-field object in 32 bytes, a prototype-chain guard becomes one shape
   compare per link.

### Closure body

A closure was an 88-byte cell: call header (function id, flags, 8-byte
context), bound `this` and `new.target` words on every closure, the
constructor record (own-property bag, prototype slot proof, learned size,
last receiver), three flag bytes and a 16-byte `[[Prototype]]` override.

- **V8** `JSFunction` holds map, properties, elements, `shared` (the
  `SharedFunctionInfo`: code, name, length), `context`, `feedback_cell` and
  `code`; `prototype_or_initial_map` lives in the function, everything rarer
  in `FunctionRareData` hung off the feedback cell only when needed.
- **JSC** `JSFunction` is the cell header, `m_executable` and `m_scope`;
  `FunctionRareData` (allocation profile, prototype cache, reified
  name/length state) is allocated on first construct or property reification.
- **SpiderMonkey** `JSFunction` is a `NativeObject` with fixed slots for
  flags/nargs, the environment, the script and the atom; bound and lexical
  state live in the environment chain, not the function.

Otter adopts the JSC shape: a 24-byte body (function id, flags — with the
per-instance `name`/`length` deletion, non-extensible and named-lookup bits
folded in —, the context word, one 4-byte handle to an out-of-line
`ClosureRareBody`, and the last-receiver observation) plus trailing bound
`this` / `new.target` words present only on arrows that capture them. The
rare record (own-property bag, prototype slot proof, learned instance size,
`[[Prototype]]` override) is allocated the first time a closure gets an
own property or a prototype override. A plain closure is a 32-byte cell.

## 11. Allocation fast path: a linear allocation buffer

Every young allocation used to run the whole policy per object: the GC
stress test, the growth-ratio major-GC test, heap-cap accounting (with the
runtime's default cap, a call that drained external releases and
recomputed the projection), then a page walk in `NewSpace::alloc`.
Generated receiver allocation repeated the page window checks inline: the
marking flag, the page's space kind, the cursor against the page size, and
the cap word.

- **V8** keeps a `LinearAllocationArea {top, limit}` per space; generated
  code bumps `top` through two external references and calls the runtime
  only when `top + size > limit`. Refill (`EnsureAllocation`) runs the
  allocation observers, the GC trigger and black allocation; while marking
  the area is shrunk so allocation reaches the slow path.
- **JSC** hands each `LocalAllocator` a bump interval from a block's free
  list; `allocate` is a compare and an add, and every policy (the eden GC
  trigger, `didAllocate` accounting) lives on the refill path.
- **SpiderMonkey** nursery allocation is `position + size <= currentEnd`
  inline in JIT code; chunk switching and the minor-GC trigger are the slow
  path.

Otter now matches that contract. `GcHeap` leads with a
`LinearAllocationArea` taken from the tail of the current from-space page;
the Rust fast path is one add, one compare and one store before the header
write, and generated receiver allocation loads `top`/`limit` through the
window pointer — the marking, space-kind and cap tests are gone because
the heap empties the buffer whenever they would fail (marking, GC stress,
bootstrap tenuring). A heap cap charges the whole buffer when it is carved
(shrunk to what the cap admits) and refunds the unused tail at retire.
Collections retire the buffer first; heap walks publish its `top` into the
page before reading it.

Fixed work against L1 (`benchmarks/results/arch-2026-09-27/e1b-lab`; also
includes `TestTypeOf`, the 32-byte closure and the extension-less context,
commit `055dbbc4`; stdout identical):

| Workload | L1 | now | Δ | RSS |
|---|---:|---:|---:|---|
| ts-fixed | 176.4G | 175.2G | −0.7% | 522 → 557 MB |
| zlib-fixed | 234.0G | 234.0G | 0 | 724 → 716 MB |
| crypto-fixed | 16.94G | 16.71G | −1.4% | 49 → 49 MB |
| fib | 3.65G | 3.64G | −0.2% | 55 → 55 MB |
| mega_method | 10.01G | 10.00G | −0.1% | 56 → 55 MB |
| ast_ctor | 22.31G | 21.99G | −1.4% | 87 → 88 MB |
| earley-boyer | 141.8G | 131.7G | −7.1% | 124 → 124 MB |

The buffer is also the substrate generated context and closure allocation
needs: both can now be carved inline with the same three instructions.

## 12. L2 staging: the object body down to shape + backing

Usage map (`research-2026-09-29/object-body-usage.md`) facts that shape the
plan: shapes are immortal old-space GC cells with one isolate-wide root and
no prototype, flags or dictionary state; generated guards compare the
compressed shape handle; every object is allocated at the root shape and
gets its prototype afterwards (73 call sites); the `ShapeEpoch` dependency
has no production dependents; and `slab_len: u16` wraps past 65,535 keys —
`o.k5` reads 65541 on a 70,000-key dictionary object.

Where the reference engines keep each piece:

| State | V8 | JSC | SpiderMonkey |
|---|---|---|---|
| property count | Map `NumberOfOwnDescriptors`; dictionary: the NameDictionary | Structure `m_offset`/`maxOffset`; dictionary: per-object Structure | Shape slot span; dictionary: `DictionaryPropertyMap` |
| per-cell mutable flags | Map bits (`is_extensible` via map transition) | cell header bytes (`indexingTypeAndMisc`, `inlineTypeFlags`, `cellState`) + Structure flags | BaseShape/ObjectFlags in the shape |
| in-object capacity | Map `instance_size` | Structure `inlineCapacity`, cell size | `AllocKind` of the cell |
| dictionary identity | the dictionary map + the NameDictionary in `properties` | per-object uncacheable dictionary Structure | dictionary Shape per object |
| prototype | Map `prototype` (prototype transitions are weak) | Structure `m_prototype` (poly-proto: inline slot) | BaseShape `proto` |

Stages, each landing whole:

1. **L2a — 16-byte body.** Fixed body = shape, slab, prototype, sidecar
   handles (4 × u32). Object flags and inline capacity move to the two
   reserved GC-header bytes (the JSC per-cell header bytes; the scavenger
   copies whole cells, so they travel with the object). The slot count
   becomes the shape's `property_count` for shaped objects and a `u32` in
   the sidecar for dictionary objects, together with the dictionary layout
   epoch and dictionary identity (state only a dictionary object has, as in
   V8's NameDictionary). Appending a property on a shaped object is then a
   slot store plus a shape store — the count store disappears from every
   generated add-transition. Two-field `sc_Pair`: 56 → 40 bytes.
2. **L2b — the shape owns the prototype.** Per-prototype roots cached on
   the prototype (V8 `PrototypeInfo::ObjectCreateMap`), prototype
   transitions, allocation takes the prototype, shapes become collectable
   (weak transition tables, code keeps embedded shapes alive), so a
   prototype-chain guard is one shape compare per link and `jit_proto`
   leaves the body.
3. **L2c — one backing handle.** Slab and sidecar merge behind one handle
   (V8 `properties_or_hash`, JSC butterfly): an 8-byte body, `sc_Pair` in
   32 bytes.

### L2a landed

The fixed body is 16 bytes. The flag byte and the in-object capacity are
the two body-owned GC-header bytes (`HEADER_BODY_BYTES_OFFSET`, written by
the allocation initializer, carried by whole-cell evacuation). A shaped
object's slot count is its shape's `property_count`, read without the heap
(shapes are immutable and non-moving); a dictionary object's count, layout
epoch and structural id live in its sidecar, entered through one
`enter_dictionary_mode` step that carries the count across. Every generated
add transition lost its count store and its exact-append check (the
receiver-shape guard implies both); the dictionary layout guard reads the
epoch through the sidecar handle.

Changes the count model forced:
- A shape now fixes the count, so installing a shape on a fresh object and
  writing its slots is one step after the slab reservation
  (`install_fresh_shape_with_slots`); the old install-then-reserve order
  would have exposed counted, unwritten slots to a collection.
- Dictionary keys past 65,535 read other keys' values: the count was a
  `u16` and dictionary offsets were truncated `as u16`. Counts and
  dictionary offsets are `u32`; cache slots stay `u16` and a wider key is
  simply not cached.
- An empty dictionary object without a sidecar shares one reserved
  identity (`ShapeId::EMPTY_DICTIONARY`), as every empty shaped object
  shares the root shape; the sidecar allocation gives it a fresh id.

Fixed work against the linear-allocation-buffer commit
(`benchmarks/results/arch-2026-09-27/e1b-l2a`, stdout identical):

| Workload | LAB | L2a | Δ | RSS |
|---|---:|---:|---:|---|
| ts-fixed | 175.19G | 174.32G | −0.5% | 557 → 397 MB |
| zlib-fixed | 233.96G | 233.87G | 0 | 716 → 715 MB |
| crypto-fixed | 16.71G | 16.87G | +0.9% | 49 → 49 MB |
| fib | 3.64G | 3.64G | 0 | 55 → 55 MB |
| mega_method | 10.00G | 10.00G | 0 | 55 → 55 MB |
| ast_ctor | 21.99G | 21.84G | −0.7% | 88 → 88 MB |
| earley-boyer | 131.69G | 128.24G | −2.6% | 124 → 124 MB |

The first measurement had ts at +31%: an empty dictionary object's
slot-layout epoch started at the unprovable `0` and appends never advance
it, so the global object and dictionary prototypes lost every generated
layout proof. The epoch now starts at 1 when the sidecar appears. crypto's
+0.9% is the extra sidecar load in its global-binding guards, which a
V8-style property cell with a code dependency removes.

Older defects the stress runs surfaced on the way (`built-ins/Object` at
`OTTER_GC_STRESS=4`: 8 failures → 0): the sidecar never traced a wrapper's
`[[StringData]]`/`[[SymbolData]]`/`[[BigIntData]]` handles and their
setters remembered the object instead of the old-space sidecar, and
`Object(primitive)` / sloppy-`this` wrapping used the primitive string read
before the wrapper allocation moved it.


### L2b research: prototype in the hidden class

| | V8 | JSC | SpiderMonkey |
|---|---|---|---|
| where the prototype lives | `Map::prototype` | `Structure::m_prototype` (poly-proto keeps it in an inline slot when one constructor sees many prototypes) | `BaseShape::proto` |
| fresh object for prototype P | `Map::GetObjectCreateMap` → `PrototypeInfo::ObjectCreateMap` cached on P (weak) | `Structure` cache per prototype (`StructureCache::emptyObjectStructureForPrototype`) | `SharedShape::getInitialShape(clasp, realm, proto)` in a weak zone table |
| `setPrototypeOf` | `Map::TransitionToPrototype`: prototype transitions on the source map (weak array) | `changePrototypeTransition` | new BaseShape → new shape lineage |
| chain guard | map check per link, or one prototype-validity cell for the whole chain | structure check per link + watchpoints | shape guard per link (`ShapeGuardProtoChain`) |
| hidden classes collectable | yes: transitions and code embed maps weakly; dead map deopts dependent code | yes: weak transition table, code jettisoned when a structure dies | yes: shape tables are weak sets swept each GC |

Otter's shapes are immortal (the shape runtime roots every handle), which
is harmless while a shape holds only keys: a shape holding its prototype
would keep every prototype — `Object.create(tmp)` targets, per-closure
`.prototype` objects — alive forever. L2b therefore has two steps:
collectable shapes first (weak side tables pruned by the post-mark pass,
compiled code keeping the shapes it embeds, shape-keyed caches flushed when
a full collection frees shapes), then the prototype moves into the shape
with per-prototype roots and prototype transitions.

### L2b-0 landed: collectable hidden classes

- The shape runtime's id, transition and offset tables are weak; a full
  collection's new `ExtraRootSource::sweep_weak` pass (after the ephemeron
  fixpoint, before sweep) forgets every shape the marking left unreached
  and hands the dead handles to the owner. The root shape and interned key
  strings stay strong.
- Compiled code names shapes only as immediates, so every compile-time bake
  goes through `bake_shape`/`bake_shape_id`: the shapes are rooted for the
  compilation and then held by the registered code object until it retires
  (invalidated code keeps running until then).
- Handle compares that could meet a later shape in a collected shape's cell
  also compare the never-reused id (Rust IC hits); the shared lookup cache,
  whose handles generated megamorphic probes compare, is pruned in the weak
  pass. Method-leaf feedback keeps ids instead of raw offsets; a closure's
  prototype-slot proof holds a traced shape; cached object layouts keep
  their shapes alive.
- A shape created during a runtime turn is pinned until the outermost turn
  ends, which covers every Rust window between creating a shape and
  installing it without rooting each local; shapes become collectable at
  the next full collection after their turn.

A 300-turn program creating objects with turn-unique key sets runs with a
bounded shape set (checksums identical under `OTTER_GC_STRESS=2/16/full`);
fixed work is unchanged (±0.3%, `benchmarks/results/arch-2026-09-27/e1b-l2b0`).
This is the precondition for L2b-1: a shape that holds its prototype can
now die with it.

### L2b-1 / L2c plan (specified, not started)

With shapes collectable, the remaining stages are:
1. **Prototype in the shape.** `ShapeBody` gains a traced `prototype`
   value; every object gets a shape — dictionary-mode objects share a
   per-prototype dictionary shape (V8 dictionary maps, SpiderMonkey
   dictionary shapes), so "dictionary" becomes a shape flag instead of the
   null handle (~25 Rust and 11 generated checks). Per-prototype roots are
   cached on the prototype (V8 `PrototypeInfo::ObjectCreateMap`): in the
   sidecar of an ordinary prototype, in the rare record of a closure, and
   uncached (a fresh, collectable root) for rarer prototype kinds.
   `setPrototypeOf` becomes a prototype transition; the 73
   allocate-then-set-prototype sites allocate at the right root; the
   constructor, arguments and literal shape caches key on the prototype.
2. **Guards.** The 29 generated `jit_proto` reads load the prototype from
   the (non-moving) shape; a chain link costs one load from a constant
   shape address plus the link's shape compare, and `instanceof` compares
   `shape.prototype` with the constructor's prototype.
3. **L2c.** With `jit_proto` gone, the slab and the sidecar merge behind
   one backing handle (V8 `properties_or_hash`, JSC butterfly): an 8-byte
   body, a two-field object in 32 bytes (JSC's `JSFinalObject` prefix).

Expected: earley −3…5% (instanceof and chain guards, 20% fewer object
bytes), ts −2…3%. The call/frame contract (fib 4.3x, every workload's call
path ≈150 instructions against V8's ≈20) is the larger lever and goes
first.

## 13. Inline context and closure allocation

`CreateContext`, `CopyContext`, `MakeClosure` and `MakeFunction` called
the VM in both tiers: an `AllocValue3` stub per context (a full-window
safepoint, root publication, the Rust allocator) and a committed runtime
call per closure (published frame, `function_kind_prototype_for`,
`alloc_closure`, `mark_closure_lookup`). earley-boyer creates ~20M
contexts and a closure per `sc_*` helper call.

- **V8** Maglev/TurboFan lower `CreateFunctionContext` and
  `FastNewClosure` to an inline bump of the new-space linear allocation
  area (`AllocateRaw` + field stores from the known `ScopeInfo` / the
  feedback cell's shared info); the builtin is only the refill path.
- **JSC** DFG/FTL `CreateActivation` and `NewFunction` allocate from the
  `LocalAllocator` with the `SymbolTable`/`FunctionExecutable` baked in.
- **SpiderMonkey** Warp `NewCallObject` / `LambdaArrow` allocate inline
  from the nursery with a template object's shape.

Otter now does the same on the buffer from section 11:
- The compile snapshot carries a plan per scope (`JitContextAllocationPlan`:
  cell size, complete header word, the body word with function id, scope
  index and slot count, each slot's initial `hole`/`undefined`) and per
  function-creation site (`JitClosureAllocationPlan`: the call-header word
  with the ordinary named-lookup bit, and whether the site binds an arrow's
  `this`/`new.target`). Generator and async functions get no plan; their
  kind prototype is not the ordinary lookup.
- One shared emitter per architecture (`arm64::allocation`,
  `x86_64::allocation`) serves both tiers: bump probe, every word written,
  cursor published, then the per-type statistics row the Rust allocator
  keeps. `CopyContext` reads the slot count from the source cell and copies
  the words; a scope with an eval extension or more than 32 words takes the
  call. The Machine tier reaches the carve through a
  `CallTarget::ContextAllocation` call whose emitter carves before the
  allocating call, and through a `ClosureAllocationProbe` committed probe
  whose miss is the unchanged committed call.
- The heap empties the buffer whenever marking, GC stress, tenuring or a cap
  needs the rooted path, so the bump is the only test; fresh young cells
  need no write barrier.

Fixed work (`benchmarks/results/arch-2026-09-27/e1b/ctx-carve`,
`.../closure-carve`; stdout identical):

| Workload | before | contexts | + closures |
|---|---:|---:|---:|
| earley-boyer | 128.2G | 117.5G | 112.3G (−12.4%) |
| ts-fixed | 174.7G | 173.7G | 174.2G |
| crypto-fixed | 16.9G | 17.0G | 16.95G |
| zlib-fixed | 233.8G | 233.7G | 233.8G |
| fib / mega / ast_ctor | 3.64 / 10.0 / 21.8G | same | same |

### Where the time goes next (samples, release, this tree)
- **earley**: JIT 80%, scavenges ~10%. The `instanceof` probe is ~8% of JIT
  samples: its target proof (closure → rare record → own-property bag →
  symbol table → bag shape → prototype slot) is a dozen dependent loads per
  test. V8 folds a constant target's `prototype` through a code dependency;
  Otter has no dependency invalidation, so the proof is re-derived each
  time.
- **zlib**: JIT 57%; ~11% is `&`/`|` with a non-Number operand going through
  the committed runtime operator (ToNumeric with a handle scope per call),
  ~11% is Machine compilation (regalloc, HIR, GVN, verification).
- **ts**: JIT 37%; generic calls re-enter compiled code through the Rust
  call path (`run_callable_sync_rooted`, `bind`, argument vectors), and
  keyed/string lookups hash or compare spellings.
- **ast_ctor**: a closure that owns a property bag (an assigned
  `prototype`) has no IC; `_super.call` resolves `call` by spelling on
  every call (`eq_str` over the shape chain).

## 14. Element accesses, Word32 truncation, JSCVT

Commits `bb18373a` (Number probe decodes undefined/null/Booleans),
`b4413eb1` (deopt-direct element accesses, dense proofs, truncation,
identities) and `b8b96e6d` (FJCVTZS).

- **Number probe.** A committed generic operator missed on any non-Number
  operand; asm.js-style `x | 0` over `undefined` (a read past a typed
  array, a missing argument) re-entered the VM per operation. Every probe
  operator applies ToNumeric to such an operand, so the probe decodes
  `undefined`, `null`, `false`, `true` to NaN, +0, 0, 1 in place.
- **Checked element operations.** A speculative access threaded Boolean
  hit flags through `ElementAddress`, `ElementValueLoad`/`Guard` and a
  `GuardCondition` (~25 instructions for a typed-array read). V8's
  `CheckTypedArrayBounds` + `LoadTypedArrayElement` is ~5.
  `ElementCheckedLoad` / `ElementCheckedAddress` own the access's single
  deoptimization and prove hit, bounds and slot in place; the index is
  extended to 64 bits so one unsigned compare also rejects a negative
  int32, and every proof branches with a ±1 MiB conditional branch.
- **Dense proofs.** An in-heap view materialized a raw element base, so it
  could not cross the loop backedge's safepoint. `ElementProof` carries the
  representation proof and length only (no address): LICM hoists it out of
  loops that cannot reenter or write element metadata and GVN reuses it
  across collections; the checked operation loads the base from the rooted
  receiver after the hit check (V8's elements-field load).
- **Word32 truncation.** An overflow-checked int32 `+`/`-` whose every
  reader truncates (bitwise ops, shifts, other such sums) and that no frame
  state names becomes wrapping (V8 representation selection). GVN folds
  `x|0`, `x^0`, `x&-1`, shifts by zero (V8 MachineOperatorReducer).
- **FJCVTZS.** ARMv8.3 JSCVT computes ToInt32 in one instruction (V8
  selects it under `JSCVT`); both tiers use it when the CPU reports it.

| Workload | before §14 | after | Node (JIT) |
|---|---:|---:|---:|
| zlib-fixed | 233.8G | 154.7G (−33.8%) | 23.1G (asm.js→wasm), **35.1G** `--no-validate-asm` |
| crypto-fixed | 16.95G | 13.73G (−19.0%) | 2.9G |
| earley-boyer | 112.3G | 111.5G | 18.7G |
| ts-fixed | 174.2G | 173.4G | 21G |

zlib is asm.js (`"use asm"`): Node validates it and runs it as wasm. The
fair JS-JIT comparison is `node --no-validate-asm` (35.1G), a 4.4x gap.

### Measured non-levers (numbers, no commit)
- **Residual calls inside spliced bodies** (allowing any monomorphic plain
  / method / explicit-receiver call, publishing the spliced parents around
  the linkage): inlined sites 80 → 88 in earley, but earley +0.3%, crypto
  +2.6%, ts −1.0%. Physical parent publication copies the spliced window
  per residual call; it only pays with stack-walk-described inline frames
  (V8 deopt data at the return address).
- **Hoisting/commoning typed views and context-slot reads across calls**
  (unsafe upper bound): zlib −3.7% only.
- **Caller-side tier counting removed** (upper bound): fib −1.6%.
- **Inline budget in instructions** (V8's 460/920 bytes ≈ 100/200 Otter
  instructions; Otter wordcode averages ~10 B/instruction): no gain
  (earley +0.1%, crypto +1.2%). Hot rejections are loops/throw CFG and
  residual calls, not size.
- **Runtime keyed loads in ts**: ~3M per run, 60% megamorphic string keys
  on shaped hash-table objects (keyword/identifier tables) — ≈1–2% of ts.

### Where the gaps are now
- Call path: ~200 instructions per generated call (fib: 437 per
  invocation vs V8's 114). No single part dominates (tier counting 1.6%,
  window fill, root records, activation publication, status dispatch each
  a few percent) — the fp-walkable frame contract (§5 slice 3) is the lever.
- earley: `instanceof` target proof ≈13% of the hot functions; scavenges
  ~10%.
- ast_ctor: closures owning a property bag have no IC (`_super.call`).

## 15. Call/frame contract

Inputs: `research-2026-09-29/call-frame-contract.md` (layout) and
`research-2026-09-29/call-frame-consumers.md` (every reader and writer of
frames, activation records, root records, `JitCtx` and statuses).

### What one generated call costs now
Machine `fib` → `fib` on arm64 (`hotasm`, 4020 samples, all in the one
code object). The caller-side sequence is ~130 executed instructions and
the callee adds ~25:

| Part | instr | Why it exists |
|---|---:|---|
| `JitMachineRootRecord` push + pop, root copies to homes and back | 17 + 2R..4R | GC finds Machine roots only through a linked record |
| activation-array limit check, push, pop | ~18 | GC, stack traces, deopt and retirement find frames only in the array |
| `ctx.native_frame`, `VmThread.current_frame`/`current_code_object_id` swap and restore, caller id save | ~12 | callee prologue, stubs and alloc safepoints find "the current frame" through them |
| sloppy `this` → global object | ~7 | receiver converted by the caller for every sloppy callee, used or not |
| entry cell `ldar`, frame-bytes and native-stack checks | ~14 | callee frame is sized and bounds-checked by the caller |
| header, `register_base`, `argument_count`, `self/this/new_target` | ~12 | eager frame record (deopt, GC, `arguments`) |
| window fill with `undefined` | 4 + ⌈L/2⌉ | GC traces the whole window of every frame; Machine callees only write it on deopt |
| caller-side tier counter + `feedback_clean` | 7 | tier-up is decided by callers |
| status dispatch (x1) before and after cleanup | ~8 | every return carries Success/SideExit/Throw/Fatal |
| callee: `ldr ctx.native_frame`, `ldr register_base`, argument loads from memory, SELF → context | ~8 | callee finds its frame through ctx; arguments passed in memory |

No part dominates; they exist because the frame is *published* (to an array,
to a root chain, to two "current" cells) instead of being *found*.

### How the reference engines do it
- **V8.** A JS frame is `[args, receiver][return pc][caller fp][context][JSFunction][argc]`
  (`StandardFrameConstants`); arguments/target/new.target/argc arrive in
  registers and on the stack; the callee builds its own frame and runs the
  stack check (the same compare serves interrupts). Nothing is published per
  call: runtime calls go through CEntry, which stores `c_entry_fp`, and
  `StackFrameIterator` walks fp from there, mapping each return pc to its
  code (`InnerPointerToCodeCache`) and `SafepointTable` entry (tagged stack
  slots at that return address; every register is caller-saved at calls, so
  roots live in spill slots). Exceptions: the runtime returns the exception
  sentinel, CEntry unwinds through `Isolate::UnwindAndFindHandler`; JS→JS
  returns carry no status. Lazy deopt patches return addresses to per-call
  lazy-deopt exits. Tiering: the feedback cell's interrupt budget is
  decremented in the callee (return and back-edges).
- **JSC.** `CallFrame` = `{callerFrame, returnPC, CodeBlock, Callee,
  ArgumentCount (tag half = CallSiteIndex), this, args…}`. Before calling an
  operation, JIT code stores `CallSiteIndex` into the frame and the frame into
  `vm.topCallFrame`; JS→JS calls publish nothing else. Exceptions are checked
  after operations only (`exceptionCheck`), then `genericUnwind` walks frames
  with the CallSiteIndex. Tier-up counters sit in the callee (prologue and
  loops). Jettisoned code keeps running until an invalidation point.
- **SpiderMonkey.** `JitFrameLayout = {returnAddress, descriptor(type|size),
  calleeToken, numActualArgs, this, args}`; `JSJitFrameIter` walks from the
  activation's `packedExitFP`; `SafepointIndex` per return displacement names
  GC slots and spilled registers; invalidation patches on-stack return
  addresses (`OsiIndex`); `HandleException` unwinds frames; JIT→JIT calls do
  not test a status (`callVM` tests the VM function's bool).

Common shape: (1) a frame is a fixed record at a fixed place relative to the
callee's entry sp, linked to its caller; (2) the *call site* identifies the
caller's safepoint/position (return pc or stored call-site index), so no per
call root publication exists; (3) "current frame" is stored only when
entering the runtime; (4) the callee owns its tier budget, stack check and
receiver conversion; (5) JS→JS returns carry only the value.

### Target contract
1. **Frame record** `NativeFrame` (72 B) at the callee's entry `sp`:
   `function_id, pc, register_count, kind, flags, code_object_id` (the
   generation running the frame), `register_base, this, new_target, self,
   argument_count, arguments_object, caller` (the calling frame's record),
   `depth` (generated frames below it, for the logical JS depth), `call_site`
   (the Machine safepoint index of the caller's in-progress call).
   Return address at `F-8`, caller fp at `F-16` in both tiers and both ISAs;
   every generated prologue starts with the fp/lr pair and sets fp.
2. **Frame chain instead of publication.** `F.caller` links frames; the
   innermost frame of an entry is `JitCtx.native_frame`; an entry frame's
   `caller` is the innermost frame of the enclosing entry, so one chain runs
   through every nesting level. The interpreter holds only the address of the
   innermost entry's cell. GC, stack snapshots, deopt caller checks and code
   retirement walk the chain; there is no activation array.
3. **Roots by call site.** A Machine frame stores its safepoint index in
   `call_site` before any call (JSC `CallSiteIndex`); GC resolves
   `(code_object_id, call_site)` to the safepoint record and the root area at
   a static offset below `F`. No `JitMachineRootRecord`.
4. **Current frame only at transitions.** Stubs and allocating calls get the
   frame from `ctx.native_frame`; the thread carries no per-call copy and the
   generation comes from the frame.
5. **Callee owns its entry.** Tier budget decremented in the Template
   prologue and back-edges (Machine code carries none); sloppy receivers
   converted where `this` is read; Machine callees publish only the parameter
   prefix of the window (deopt widens and fills it).
6. **Value-only JS→JS returns.** Throw becomes a pending exception unwound
   along the chain to the nearest handler or entry; a callee deopt finishes
   the callee in the interpreter from the callee's own exit and returns its
   value; statuses remain only on runtime-stub returns.
7. **Inline frames from data.** Spliced parents are described by the call
   site's safepoint recipe, never physically published, so residual calls in
   spliced bodies cost nothing extra (the §14 non-lever becomes a lever).

### Stages (each gated and measured in wall time and instructions)
- **C1 frame chain.** `caller`/`depth`/`code_object_id` in the record; the
  linkage writes them instead of pushing the activation array, swapping the
  thread's current frame and saving the caller id; GC, snapshots,
  `generated_caller_matches`, inline activations, retirement and logical
  depth walk the chain. Array, `jit_arena_activation_indices`,
  `VmThread.current_frame/current_code_object_id` deleted.
- **C2 roots by call site.** `call_site` stamp replaces root-record
  push/pop; GC, alloc-stub rooting and inline-frame decode resolve through
  the frame; `MACHINE_ROOT_RECORD_SIZE` bias deleted.
- **C3 callee-owned entry.** Lazy sloppy `this`, prefix-only windows for
  Machine callees, callee-side tier budget, `generated_feedback_clean`
  deleted.
- **C4 frame pointer.** Poll countdown off x29/rbp; every generated
  prologue sets fp; callee derives `F` from its entry sp (no `ctx` load).
- **C5 value-only returns.** Unwinder over the chain; callee-local deopt
  completion; status checks leave JS→JS call sites.
- **C6 inline frames from data.** Residual calls in spliced bodies.

### C1 landed: frame chain
`NativeFrame` is 72 bytes: `code_object_id` joins the register-shape word
(one copy from the entry cell's header), `caller` and `depth` close the
record. Linkage checks the caller's depth against `JitCtx`'s generated-depth
bound (the JS depth budget minus live interpreter frames), writes
`caller`/`depth` and makes the callee `JitCtx::native_frame`; cleanup makes
the caller innermost again. The activation array, its cursor, the arena
index list, the push/pop stubs, `VmThread::current_frame/code_object_id`
and the caller-id linkage slots are gone. The interpreter holds only the
live entry's frame cell (or a detached head for Rust-published frames);
GC tracing, stack snapshots, `generated_caller_matches`, inline
publication, retirement and logical depth walk the chain; allocating stubs
reach the frame through `VmThread::frame_cell`.

| Workload | instr before | after | wall before | after |
|---|---:|---:|---:|---:|
| fib | 3.64G | 3.38G (−7.2%) | 0.230s | 0.212s (−7.8%) |
| earley-boyer | 111.5G | 108.6G (−2.5%) | 6.72s | 6.53s (−2.9%) |
| ts-fixed | 173.7G | 172.0G (−1.0%) | 17.51s | 17.48s |
| ast_ctor | 21.75G | 21.58G (−0.8%) | 1.45s | 1.41s |
| crypto / zlib / mega_method | 13.68 / 154.5 / 10.0G | same | 0.86 / 9.47 / 0.48s | 0.81 / 9.36 / 0.48s |

Stdout identical; difftest 85/85; Test262 `language/` 24,077/0,
`built-ins/` 23,520 with the two `RGI_Emoji` budget failures; GC stress
1–16 in three tiers clean on 19 call/frame corpora
(`derived_class_fields_super_shapes` differs from Node in the interpreter
without stress: the known derived-field timing gap).

### C2 landed: roots by call site
The record grows to 80 bytes: `call_site` shares the depth word (linkage
writes `depth | NO_SAFEPOINT << 32` in one store) and `machine_roots` is
the optimized frame's root-home base, written once by its prologue. Before
every collecting call Machine code saves its roots to their homes (as
before) and stores the safepoint id in its own record: two instructions
where the `JitMachineRootRecord` push/pop took 17 and a `sub sp`. The
collector walks the frame chain and resolves `(code_object_id,
call_site)` to the safepoint's homes; inline recipe decode and the
generated caller check read the same pair from the frame. The record type,
the interpreter's root-chain head, `JitCtx::machine_roots_ptr` and the
32-byte stack bias baked into every in-call root and scratch offset are
gone; x86-64 parks a result pair in the red zone instead of the popped
record. A stamp may outlive its call: the homes it names were written by
that call and every later collection traces them, so they stay valid until
the frame's stack dies; a callee that side-exits to the stack-call
deoptimizer has its call site cleared first, because its machine stack is
gone while the record stays published.

| Workload | instr after C1 | after C2 | wall after C1 | after C2 |
|---|---:|---:|---:|---:|
| fib | 3.38G | 3.21G (−5.1%; −12.0% vs start) | 0.212s | 0.199s |
| earley-boyer | 108.6G | 106.6G (−1.8%; −4.3%) | 6.53s | 6.42s |
| crypto | 13.68G | 13.53G (−1.1%) | 0.811s | 0.805s |
| ts-fixed | 172.0G | 171.1G (−0.5%; −1.5%) | 17.48s | 17.27s |
| ast_ctor | 21.58G | 21.45G (−0.6%) | 1.41s | 1.40s |
| zlib / mega_method | 154.4 / 10.0G | same | 9.36 / 0.48s | same |

Stdout identical; difftest 85/85; Test262 `language/` 24,077/0,
`built-ins/` 23,520 + the two `RGI_Emoji` failures; GC stress 1–16, three
tiers, 21 corpora (1,008 runs) clean; `jit_machine_direct_call` and
`jit_call_lifecycle` pass.

### C3 (part): callee-owned window and receiver, success-first status
- **Prefix windows for optimizing callees.** Generated callers of an
  optimizing generation publish only its parameter prefix even when it has
  safepoints: locals live in allocator homes rooted by call site, and every
  exit widens the window first. Excluded: functions that materialize
  `arguments` (the actual window follows the complete register window) and
  forwarding callers (they write the formals context into a local
  register). The registry's old "no safepoints" install condition dated from
  whole-window safepoints; with it in place the first attempt silently
  refused to install such code and every call fell back to the generic
  stub (fib 3.2G → 31.8G) — caught by fixed-work, not by the gates.
  Inline publication bounds return registers by the function's complete
  window.
- **Unobserved `this`.** A sloppy callee whose body has no `LoadThis`,
  no closure creation (an arrow captures `this`) and no direct eval gets
  `StrictOrLexical` linkage: `undefined` instead of the global object.
- **Success first.** Success is status zero: one `cbnz` after the call and
  one after cleanup replace the SideExit-first compare chains (both ISAs).

| Workload | instr after C2 | after | wall after C2 | after |
|---|---:|---:|---:|---:|
| fib | 3.21G | 2.99G (−6.8%; −17.9% vs start) | 0.199s | 0.188s |
| earley-boyer | 106.6G | 104.8G (−1.7%; −6.0%) | 6.42s | 6.20s |
| ts-fixed | 171.1G | 170.8G (−0.2%; −1.7%) | 17.27s | 17.08s |
| crypto / ast_ctor | 13.53 / 21.45G | 13.49 / 21.43G | 0.805 / 1.40s | 0.798 / 1.40s |

Callee-side tier budget is not done: optimizing-callee entry counts feed
the generated-call statistics and bytecode-call accounting, so dropping
them changes observable counters, not only speed.

### C6 landed: inline parents described by the call site
Generated code no longer builds NativeFrames for spliced parents around a
call. A generated call's safepoint record carries the recipe
(`inline_frames`) and the call's PC in the code object's own
function (`call_pc`); stack snapshots read both from the physical caller's
call site, the callee-deopt caller check validates the physical caller's
recipe, and nothing is copied per call. Spliced bodies may therefore keep
monomorphic residual plain, method, explicit-receiver and construct calls
(previously only constructs, with physical publication). The record's
`call_pc` also fixes a stale-position bug: arm64 Machine direct calls never
stored their PC, so a trace through an optimized (e.g. OSR'd) caller showed
its entry or loop-header position; the new difftest corpus
`inline_residual_calls` pins positions against Node in every tier.
The first build regressed earley +5%: the Machine verifier still admitted
recipes only on construct calls, so four hot functions failed verification
and stayed in Template.

| Workload | instr after C3 | after C6 | wall after C3 | after C6 |
|---|---:|---:|---:|---:|
| ts-fixed | 170.8G | 168.9G (−1.1%; −2.8% vs start) | 17.08s | 17.00s |
| earley-boyer | 104.8G | 104.6G (−0.2%; −6.2%) | 6.20s | 6.19s |
| crypto | 13.49G | 13.45G (−0.3%) | 0.798s | 0.805s |
| fib / zlib / ast_ctor / mega_method | unchanged | | | |

`jit_machine_call_chain`'s rooting test assumed 200k two-field objects
outgrow the 4 MB young space; after the 16-byte object body they no longer
do. It now allocates a 32-element array per iteration.

### Where the call contract stands
Since the start of §15: fib −17.9% instructions (0.230 → 0.189 s), earley
−6.2% (6.72 → 6.19 s), ts −2.8% (17.5 → 17.0 s). Remaining stages measured
or judged against wall time:
- **Caller tier counting for optimizing callees** (skipping it by the
  entry cell's tier flag): fib −2.8% instructions but only −2% wall,
  earley −0.9% / −0.1%, crypto −0.8% / −0.6%; its sample share in fib was
  skid from neighbouring loads. Not taken: optimizing-entry counts feed the
  generated-call statistics, bytecode-call accounting and tests.
- **Frame pointer in optimized code (C4)**: walks use the record chain, so
  it only helps native profilers; it costs a register (the back-edge
  countdown lives in x29/ebp). Deferred.
- **Value-only JS returns (C5)**: status after a JS call is one `cbnz`; an
  unwinder would save it and the callee's status write. Deferred.
The generated call is now ~360 instructions per fib invocation (V8 ~114),
spread thinly: callable guard, entry-cell loads, frame-size and stack
checks, header copy, depth check and link, root homes, callee prologue.
The next larger levers are outside the call contract.

## 16. `f.call` and the function `prototype` slot

ast_ctor (TypeScript-style `__extends` classes) spent 82% of its samples
outside generated code. Every `_super.call(this, …)` went through the generic
method stub: `call` was looked up by spelling, the intrinsic
`Function.prototype.call` was entered through Rust, and that call then made
the same call again one Rust frame deeper. The root cause is `F.prototype = x`.
Before this change the assignment created a dictionary-mode property bag on the
closure, and a closure with a bag fails the exact-`ORDINARY` guard of the
closure CacheIR program, so no named lookup on it had an IC. The construct
and `instanceof` proofs had to walk closure → rare → bag → shape → slot. A
dictionary layout epoch starts at 1 in every object, so no bag proof could
tell two closures apart.

### How the reference engines do it
- **V8** keeps `prototype` out of the property backing store entirely:
  `JSFunction::prototype_or_initial_map` is a field of the function, and the
  `prototype` property is an accessor-info that reads it. Assigning it never
  changes the function's map. `JSCallReducer::ReduceFunctionPrototypeCall`
  rewrites `JSCall(%Function.prototype.call%, f, this, args…)` into
  `JSCall(f, this, args…)` once feedback shows the target is the intrinsic.
- **JSC**: the bytecode generator emits a guard for `f.call(…)`
  (`CallFunctionCallDotNode`): if `f.call` is the intrinsic, it makes a
  direct call with a shifted receiver; otherwise it makes the generic call.
  The prototype sits in the function's rare data (`FunctionRareData`,
  together with the allocation profile).
- **SpiderMonkey**: `CallIRGenerator::tryAttachFunCall` guards on the callee
  being `fun_call` and calls `this` as the target with shifted arguments, and
  Warp transpiles it. `prototype` is a lazily resolved own data property of
  the function object.

Otter follows V8 for storage (a slot, not the bag) and V8/SM for the call
(recognize the intrinsic at the site, call the receiver directly).

### Stage A landed (596d2c1c): direct call from the method-call stub
A generated `CallMethodValue` whose method resolves to the intrinsic calls
the receiver with the first argument as `this` instead of entering the
intrinsic. ast_ctor −2.7% instructions.

### F1 landed: `prototype` in the closure's rare record
`ClosureRareBody` has a `prototype` value (the hole until the default object
is allocated) and a `prototype_writable` bit. Bare function values keep the
same pair in an interpreter side table. The property is
`{writable, enumerable: false, configurable: false}` from creation for every
kind that owns one. It is materialized once, can only go from writable to
non-writable, and never enters the bag, so `F.prototype = x` leaves the
closure's lookups ordinary. The construct receiver allocation (both JIT
tiers) and the `instanceof` probe load one word from the rare record; the
hole fails their cell test. Kinds without an implicit `prototype` (arrows,
methods) keep a user-created one in the bag as before.

The move also fixed three spec bugs that came from lazily materializing the
property into the bag:
- `'prototype' in F` was false before the first read.
- `Reflect.ownKeys(F)` put `prototype` after later-added keys.
- `Object.defineProperty(F, 'prototype', {value})` reset `writable` to false.
  An attributes-only redefinition also lost the value; descriptor
  completion now covers the slot as it does `name` and `length`.
The difftest corpus `function_prototype_slot` pins all three, together with
assignment across tiers, per-closure slots and freezing.

| Workload | instr at C6 | after A + F1 | wall at C6 | after A + F1 |
|---|---:|---:|---:|---:|
| ast_ctor | 21.43G | 17.11G (−20.2%) | 1.396s | 1.105s (−20.8%) |
| earley-boyer | 104.6G | 101.4G (−3.1%) | 6.19s | 6.04s |
| ts-fixed | 168.9G | 167.5G (−0.8%) | 17.00s | 16.76s |
| crypto | 13.45G | 13.40G (−0.4%) | 0.805s | 0.805s |
| fib / zlib / mega_method | unchanged | | | |

### Stage B landed: `f.call(this, …)` as a direct call in the optimizing tier
A `CallMethodValue` site now also owns ordinary call feedback. When a call
on a closure receiver runs the receiver itself (`g.call` / `g.apply`), the
interpreter records the receiver's function there. For a `call` site with
one recorded target, the bake stores the closure program's proof:
- the receiver proof (exact ordinary closure of the active realm);
- the `%Function.prototype%` shape;
- the `call` slot;
- the intrinsic's identity.

VM intrinsics now own an external-reference identity (a byte of a static
table), so the builtin identity probe can name them exactly as it names a
static builtin.

Machine lowers the site as V8's `ReduceFunctionPrototypeCall` does:
- A `FunctionCallProof` node composes the existing intrinsic-prototype,
  shape, slot and identity probes and ends in one pre-operation exit.
- A direct `CallWithThis` of the receiver follows. Its argument words are
  already `[this, …]`; `f.call()` passes `undefined`.

The callee identity guard covers another target, and its miss completes
through the generic explicit-receiver call. That stub now admits a reduced
`CallMethodValue`: the first actual argument became the receiver. A failed
proof (own `call`, replaced or redefined intrinsic, non-closure receiver)
exits once, and the recompile keeps the generic method call.

Three defects surfaced on the way and are fixed:
- **Parameter speculation storm.** Machine infers entry parameter
  representations from the uses a parameter reaches, including uses on
  untaken paths (`c === undefined ? 0 : c` typed `c` Int32). An
  under-applied call then failed the entry guard, and every recompile
  repeated the speculation: nine deopts for one call site until the budget
  ran out. The failed guard's actual parameter values now widen a
  per-function parameter profile, which caps the next generation's entry
  representations for that parameter only. This follows JSC's argument
  value profiles updated from exit values. Disabling all parameter
  speculation after any entry exit was tried first. It made crypto bimodal
  (13.4G / 17G): one incidental `am3` entry exit untyped its hot
  parameters.
- **Inherited accessors on functions.** `f.m()`, where `m` is an accessor
  on `%Function.prototype%` (or a kind prototype), read only data
  properties and threw "not a function". Inherited lookup now walks
  OrdinaryGet from the function's `[[Prototype]]` with the function as
  receiver; the override path previously passed the override as `this`.
- The generic explicit-receiver stub rejected the reduced site (the
  `InvalidOperand` above was the first symptom).

Corpora: `function_prototype_call_sites` covers:
- targets and receivers;
- own `call`;
- replaced and getter `call`;
- throws inside and outside handlers.

`parameter_entry_widening` covers under-application and widening.

| Workload | instr after F1 | after B | wall after F1 | after B |
|---|---:|---:|---:|---:|
| ast_ctor | 17.11G | 10.94G (−36%) | 1.105s | 0.638s (−42%) |
| ts-fixed | 167.5G | 160.5G (−4.2%) | 16.76s | 16.37s |
| earley / crypto / zlib / fib / mega_method | unchanged | | | |

### The loaded form and exact feedback
A profile after stage B still showed most of ast_ctor's samples in the
generic explicit-receiver stub. `_super.call(this, k)` with a captured `k`
compiles to the loaded form (`LoadProperty _super.call` plus `CallWithThis`
whose callee is the intrinsic). There the intrinsic ran synchronously and
the site recorded nothing.

Call feedback now has a third target kind, `FunctionPrototypeCall(fid)`:
`%Function.prototype.call%` ran function `fid`. The interpreter and both
generic JIT stubs record it for either form, so an ordinary call of the same
function stays a distinct target. The kind also carries the plan: the
function's entry becomes the site's reduced callee, never an ordinary direct
callee that a tier would guard against the intrinsic value.

The loaded form proves only its callee's identity: no lookup, the same
`FunctionCallProof` node. The reduced call is a new Machine call kind,
`FunctionCall`. It uses explicit-receiver linkage with exactly one candidate,
and every miss exits instead of completing generically:
- On arm64, a miss takes the candidate bail.
- On x86-64, the generated-call miss reloads the roots and deoptimizes.

The interpreter then performs the site's own operation, and the recompile
keeps the generic call. A miss is therefore never a generic call whose
feedback or argument shape differs from the site's own. The generic
explicit-receiver stub again admits only `Call` and `CallWithThis`.

| Workload | instr after B | after loaded form | wall |
|---|---:|---:|---:|
| ast_ctor | 10.94G | 6.39G (−41%) | 0.638 → 0.385s |
| ts-fixed | 160.5G | 158.2G (−1.4%) | 16.4s |
| earley / crypto / zlib / fib / mega_method | unchanged | | |

Since §16 began, ast_ctor has gone from 1.40 s to 0.385 s (21.4G → 6.4G
instructions). Reduced targets are still only residual calls in a splice.
Admitting them as inline candidates is the next step for `_super.call`
chains.


### Int32 remainder
The ast_ctor profile after the loaded form showed `fmod`: Machine lowered
every `%` as a Float64 call, even with Int32-only feedback. `%` is now a
speculative `IntegerRem` when both operands are Int32 and the site's
feedback is Int32. It follows V8's `Int32ModulusWithOverflow`:
- The result takes the dividend's sign.
- The divisor's magnitude and an unsigned division of the dividend's
  magnitude cover every Int32, including `INT32_MIN % -1` and
  `INT32_MIN % INT32_MIN`, with no `idiv` trap on x86-64.
- A power-of-two divisor of a non-negative dividend is a mask.
- A zero divisor (NaN) exits as Int32 overflow, and a zero result of a
  negative dividend (−0) exits as negative zero. Both widen the site
  through the existing arithmetic-exit policy, like `IntegerMul`.

AArch64 uses its emitter scratch (x15–x17). x86-64 divides through
`edx:eax`, declared as the new `IntegerRemainder` clobber set. A
Machine test executes both targets' code for every sign combination and
both exits, and the corpus `int32_remainder` pins the edge cases against
Node in every tier.

ast_ctor −4% instructions (6.39G → 6.13G, 0.33 s), ts −0.5%, others
unchanged.


### Spliced reduced calls and virtual recipes for property calls
Reduced `f.call` targets now splice like plain callees. The inline guard
has an explicit-receiver form (`MachineCallGuard::Explicit`) that proves the
callable and binds `this` per §10.2.1.2:
- A bound-`this` closure keeps its own `this`.
- A strict callee takes the receiver as is.
- A sloppy callee binds an Object receiver, and the global object for a
  nullish one.
- A primitive receiver of a sloppy callee exits.

x86-64 gained the same Object test. The first build read the receiver after
the identity proof, whose scratch registers can hold it; ts caught the
resulting garbage `this`, and both targets now park the receiver first.

Splicing then cost ts +10% instructions. Every committed property load or
store inside a spliced body decoded its recipe and materialized the
parents as interpreter activations for the duration of the call, on every
execution. Those operations name their own function and PC and never read
the innermost activation. Their records are now virtual like generated
calls (with the outermost frame's suspended call PC), the stubs call the
runtime directly, and a stack walk expands a virtual recipe on the
innermost frame as well. Binding accesses, which do read the callee's
activation, still materialize it.

Found on the way (not fixed here):
- ts: 45k `runtimeTransition` exits from Template callees containing
  `for-in`, which the optimizing tier declines.
- Stack traces name accessor frames by path, and attribute a getter call to
  the member expression rather than the property name.

| Workload | instr before | after | wall |
|---|---:|---:|---:|
| ast_ctor | 6.13G | 5.71G (−7%) | 0.31s |
| ts-fixed | 157.3G | 156.5G (−0.5%) | |
| crypto | 13.46G | 13.24G (−1.7%) | |
| earley / zlib / fib / mega_method | unchanged | | |

## 17. The prototype lives in the hidden class

Before this section an object carried its `[[Prototype]]` itself (a flat
`jit_proto` word in the body, a boxed override in the sidecar for Proxy and
other non-ordinary prototypes), and a dictionary-mode object had a null shape.
Two objects with different prototypes could share a shape, so every
prototype-dependent proof (missing-key chains, method holders, receiver
allocation) had to guard the prototype separately, and the ordinary-object
prototype epoch existed only to paper over that.

### How the reference engines do it

- V8: `Map::prototype`; `Map::TransitionToPrototype` moves an object to a map
  of the new prototype, and `PrototypeInfo::ObjectCreateMap` caches the
  initial map of `Object.create(p)` objects on the prototype. Dictionary
  objects have dictionary maps (`is_dictionary_map`), one per prototype.
- JSC: `Structure::m_prototype` (stored in the structure); changing the
  prototype is a structure transition (`changePrototypeTransition`); dictionary
  structures are uncacheable/cacheable dictionary kinds of a structure.
- SpiderMonkey: `BaseShape::proto`; `SetProto` reshapes the object.

All three make shape identity imply prototype identity. Otter now does the same.

### What changed

- `ShapeBody` gained `kind` (dictionary bit), `prototype` (compressed ordinary
  object, else null), `prototype_value` (Proxy / non-ordinary object value)
  and `dictionary` (the lineage's dictionary shape; in that shape, the root).
  Every lineage starts at a prototype's root; an ordinary prototype caches its
  instances' root in its sidecar (`ExoticSlots::instance_root`, V8's
  `ObjectCreateMap`), the `null` root lives in a new heap embedder root slot
  (`GcHeap::embedder_root`, V8's roots table) so heap-only code reaches it.
- `ObjectBody` lost `jit_proto`; `ExoticSlots` lost `proto_override`. Every
  object has a shape; dictionary mode is the lineage's dictionary shape, never
  null. The dictionary shape is never registered by id, so no id-keyed cache
  or baked guard can resolve to it (all dictionary objects of a lineage share
  it whatever their keys).
- Creation picks the root: constructors (interpreter, runtime and generated
  receiver allocation), `Object.create`, arguments objects (their cached
  shapes are keyed by the `%Object.prototype%` root), JSON, dates, errors,
  iterators and function-kind prototypes allocate on their prototype's root.
- `[[SetPrototypeOf]]` is a shape change: a keyed object replays its keys on
  the new root (offsets unchanged), a dictionary object takes the new
  lineage's dictionary shape with a fresh structural id; the heap-only path
  keeps an empty object keyed and normalizes a keyed one to dictionary.
- Generated code reads a prototype as `object → shape → prototype` and tests
  dictionary mode with the shape's kind byte (both backends; receiver
  allocation, method guards, instanceof, CacheIR, megamorphic probes, global
  bindings). Generated receiver allocation proves the baked shape's prototype
  is the live `prototype`, or installs the root the live prototype caches.
- Deleted: `CodeDependencyKind::ShapeEpoch`, the ordinary-object prototype
  shape epoch, `jit_proto_byte`, `EMPTY_DICTIONARY`.

### Defects found on the way

- Dictionary-mode objects did not trace their shape (the old null-shape
  test survived a mechanical rewrite); with the prototype only in the shape,
  GC freed live lineages (difftest GC stress).
- The code-liveness census did not visit shapes, whose `prototype_value` can
  carry a function id.
- A prototype-changing shape install needs a marking barrier: the new root
  may be unmarked while the object is black.
- The shared add-property transition cache was direct-mapped. Per-prototype
  lineages give a base constructor reached from 24 subclasses 24 receiver
  lineages; a handful of colliding keys thrashed and every thrash re-ran the
  full capture (500k captures in `ast_ctor`). The table is now 512 sets × 4
  ways, most recent first, probed way by way in both backends (V8 backs its
  primary stub cache with a secondary table for the same reason).

### Measurements (instructions, fixed work)

| Workload | before (b77640ef) | after | |
|---|---:|---:|---|
| ast_ctor | 5.71G | 8.57G | +50%: 24 lineages make the base-constructor stores megamorphic (23.3G before the set-associative cache) |
| mega_method | 10.00G | 7.38G | −26% |
| ts-fixed | 156.5G | 155.1G | −0.9% |
| earley-boyer | 101.05G | 101.32G | +0.3% |
| crypto | 13.24G | 13.55G | +2.3% |
| zlib / fib | unchanged | | |

`ast_ctor` measured the old model's cross-prototype sharing; V8 sees the same
megamorphic stores on this hierarchy. The next step that removes it is
prototype validity cells (one guard per chain instead of per-hop shape
checks) and megamorphic stores fully in generated code.

Gates: difftest 90/90; Test262 `language/` 24,077/24,077, `built-ins/`
23,520 (2 known RGI_Emoji); GC stress strides 1–16 on 11 prototype/store
corpora 48/48 each; otter-vm lib 990, otter-jit lib 303, x86 machine 194.
`derived_class_fields_super_shapes` fails GC stress at every stride and
without stress: field initialization after an arrow/eval `super()` is the
open compiler item from the E1 checkpoint below, not a shape effect.

## 18. Prototype-chain validity cells

Design recorded before implementation, 2026-09-30, on `main` after
`d553b97d` put prototype identity in the hidden class.

### Reference mechanisms

- V8's [Map::GetOrCreatePrototypeChainValidityCell](https://github.com/v8/v8/blob/main/src/objects/map.cc)
  reuses a valid cell and allocates a new cell after invalidation. It first
  registers prototype users. [JSObject::InvalidatePrototypeChains](https://github.com/v8/v8/blob/main/src/objects/js-objects.cc)
  invalidates cells through those registrations when a prototype changes.
  The cell is held by a prototype map; `PrototypeInfo` records its users.
- JSC's [ObjectPropertyConditionSet](https://github.com/WebKit/WebKit/blob/main/Source/JavaScriptCore/bytecode/ObjectPropertyConditionSet.h)
  represents presence, absence and equivalence conditions. Its
  [watchpoint design](https://webkit.org/blog/10308/speculation-in-javascriptcore/)
  moves checks to mutation, validates dependencies before installation and
  invalidates compiled assumptions when a watched condition changes.
- SpiderMonkey's [CacheIR shape teleporting](https://github.com/mozilla-firefox/firefox/blob/main/js/src/jit/CacheIR.cpp)
  omits intermediate prototype guards while its mutation protocol supports
  that proof. [Watchtower](https://github.com/mozilla-firefox/firefox/blob/main/js/src/vm/Watchtower.h)
  observes property additions, removal, descriptor changes and prototype
  changes on watched objects. It is a mutation-driven proof, rather than
  V8's literal shared-cell representation.

### Chosen contract

One address-stable, monotonically invalidated cell proves an ordinary
prototype chain. Every receiver shape already identifies the first
prototype. The first prototype owns the current cell; every prototype in
the chain subscribes to it through weak registrations. A structural or
property-value mutation invalidates all subscribed cells before publishing
the change. Revalidation allocates a new cell; an invalid cell never becomes
valid again. Thus there is no generation wraparound or stale-proof revival.

The cell and its registrations carry no moving object addresses. IC records
and installed code retain the cells they reference. A holder is reached
through its rooted, pinned instance-root shape, whose traced prototype field
is updated by moving GC. This gives a constant-size proof and holder load
without keeping a raw moving object pointer in generated code.

VM ICs, CacheIR, method guards, constructor field transitions, receiver
allocation and the shared megamorphic transition table all consume this
contract. `MethodProtoChain`, prototype-shape arrays and the transition
table's `chain[8]` are removed. ARM64 and x86-64 emit the same cell check.
Generated stores into watched prototypes enter the canonical mutation
boundary before any effect, where dependent cells are invalidated. Ordinary
instance stores keep their generated path. Proxy/exotic chains cannot
establish an ordinary-chain proof and use the existing semantic operation.

No assumption may cross JavaScript reentry without a fresh validity check.
Compiler dependency ownership must also preserve cells for an executing
generation after its entry has been unlinked. Compile/install validation
will use the same cell contract when concurrent compilation is introduced.

Snapshot dependencies obey the same isolate ownership rule. V8's
[ContextSerializer](https://github.com/v8/v8/blob/main/src/snapshot/context-serializer.cc)
clears feedback slots when capturing a context. Otter's captured code directory
will preserve admitted bytecode and function/site ranges, while each restore
owns fresh CodeBlocks, feedback, atom tables and registry topology. Sharing
the donor's mutable `CodeSpace` would retain foreign transition shapes and
validity cells; that sharing is removed. Sidecar watchpoints start empty.

### Before measurement

`scripts/dev/fixed-work.py`, production release executable, sequential runs;
exact commands and executable/script hashes are in
`benchmarks/results/prototype-validity-2026-09-30-before-counters/`.

| Workload | Instructions before |
| --- | ---: |
| earley-boyer | 101,610,303,208 |
| ts | 153,817,567,165 |
| crypto | 13,522,144,158 |
| zlib | 154,602,584,918 |
| fib | 3,003,891,245 |
| ast_ctor | 8,596,244,899 |
| mega_method | 7,392,419,928 |

### Implementation findings

- Prototype preparation now walks the complete chain with rooted receiver and
  cursor handles. The former eight-hop preparation limit was a dependency of
  the deleted guard-array mechanism. The ordinary lookup safety bound remains.
- Empty objects made by heap-only `Object.create` paths could have unregistered
  receiver shape IDs. A shallow inherited load happened to register that shape
  while registering its holder root; a deeper holder has a different root.
  Capturing a property or method proof now registers both identities. The
  runtime regression asserts actual validity-cell relocations in Template and
  optimizing code, then mutates and shadows the inherited property.
- Admitting deep method proofs exposed a cold-call source bug in an inlined
  helper. Its guard miss decoded the method opcode using the physical outer
  function's identity. Boxed calls now resolve their source through the exact
  generation's safepoint recipe. Both emitters retain the enclosing call PC in
  the physical frame. No caller is copied, interpreted, or replayed.
- The new mutation corpus covers deep loads/methods, shadowing, deletion,
  accessors, prototype replacement, generated stores into watched prototypes,
  megamorphic reads, constructor transitions and global-object binding writes.
  Debug regression tests pass at GC strides 1–16 after the cold-call fix.
- Constructor closures can share a function ID while owning different
  prototype objects. A function-pair-only proof cache replaced its chain on
  each alternating closure and repeated constructor analysis. Proofs now use
  the prototype lineage as part of their key. Empty receiver allocation uses
  the live prototype root without an absence dependency; only preinitialized
  fields need an exact chain proof. Subsequent stores retain their own guards.
  The regression test checks distinct live roots and generated allocation.
- Heap snapshot restore must sever the sidecar's Rust-owned watchpoints.
  Copying their Mutex/Arc bytes would give restored isolates ownership of the
  donor's allocations. Restored sidecars now start with empty watchpoints;
  a regression captures an established proof, restores three isolates and
  checks that their mutations and destruction leave the donor proof valid.
- Restores also shared the donor's mutable code directory. Warm property ICs
  retained transition shapes from the donor heap, which failed under restored
  allocation pressure. Capture now freezes the code ranges and admitted code;
  each restore creates its own directory, CodeBlocks, empty feedback and
  resolved atom tables. A regression drops the donor, mutates one restore,
  collects another, and checks that its inherited values remain independent.

### Fixed-work measurements

Same seven workloads, sequential production runs through
`scripts/dev/fixed-work.py`. The after executable and commands are recorded in
`benchmarks/results/prototype-validity-2026-09-30-sealed-after-counters/`.

| Workload | Instructions before | Instructions after | Change |
| --- | ---: | ---: | ---: |
| earley-boyer | 101,610,303,208 | 100,148,068,574 | -1.44% |
| ts | 153,817,567,165 | 154,835,579,302 | +0.66% |
| crypto | 13,522,144,158 | 13,368,820,694 | -1.13% |
| zlib | 154,602,584,918 | 154,180,519,765 | -0.27% |
| fib | 3,003,891,245 | 2,999,652,229 | -0.14% |
| ast_ctor | 8,596,244,899 | 5,811,091,495 | -32.40% |
| mega_method | 7,392,419,928 | 7,404,560,244 | +0.16% |

The first after run exposed the constructor proof-key error: `ast_ctor`
reached 18,321,810,407 instructions. Sampling attributed the added work to
repeated receiver preparation and constructor analysis. Correct chain
ownership and live-root empty allocation remove that regression without
sharing shapes across prototypes. The other workloads were not tuned.

### Gates

The final executable is retained with SHA-256 hashes in
`benchmarks/results/prototype-validity-sealed-binaries/`.

- Difftest: 91/91.
- GC stress: 528/528 Node comparisons across 11 corpora, all three tiers,
  strides 1–16. The five runtime regressions also pass at every stride.
- VM library: 995/995; JIT library: 303/303; x86 Machine: 194/194.
- Runtime regressions: 5/5 on ARM64 and x86 under Rosetta. Snapshot suite:
  8/8, plus ten consecutive successful pressure-suite runs.
- `cargo clippy --all-targets --all-features -- -D warnings`, formatting
  and diff checks pass.
- Test262 `language/`: 24,077 passed, 495 skipped; no failures, crashes,
  timeouts or OOM. `built-ins/`: 23,520 passed, 536 skipped, two failures;
  no crashes, timeouts or OOM. Both use `--timeout 30000` and `--jobs 1`,
  sequentially through one runner.

The two RegExp backtrack-budget failures (`RGI_Emoji.js` and
`RGI_Emoji_ZWJ_Sequence.js`) reproduce on the preserved pre-change executable
with the identical error. Exact
commands, harnesses and output are in
`benchmarks/results/prototype-validity-regexp-comparison/`.

All seven fixed-work runs exited successfully. The initial sandboxed baseline
could not read macOS resource counters; the table uses the successful run
with access to those counters.

## 19. Shared JavaScript stack and call trampolines

Design recorded before implementation, 2026-09-30, on `main` after
`ca942fa6`. The seven final measurements in section 18 are this item's
before measurements; its preserved executable is the comparison baseline.

### Reference mechanisms

- V8's [ARM64 Call, Construct and interpreter-entry builtins](https://github.com/v8/v8/blob/main/src/builtins/arm64/builtins-arm64.cc)
  dispatch on callable kind in generated code. Bound calls prepend arguments
  and tail-dispatch to the target. Constructors select receiver creation or
  an uninitialized derived receiver. Interpreter entry consumes the same
  incoming JavaScript argument convention and constructs an interpreter
  frame on the native stack. Runtime calls handle semantic work and errors;
  they do not constitute the ordinary function-call dispatcher.
- JSC's [CallFrame](https://github.com/WebKit/WebKit/blob/main/Source/JavaScriptCore/interpreter/CallFrame.h)
  fixes caller/return address, code block, callee, argument count, receiver
  and argument positions. Its [call thunks](https://github.com/WebKit/WebKit/blob/main/Source/JavaScriptCore/jit/ThunkGenerators.cpp)
  select executable entries and transfer control directly. Native thunks
  establish the host boundary and perform exception handling around the
  native entry.
- SpiderMonkey's [JS ABI design in JitFrames.h](https://github.com/mozilla-firefox/firefox/blob/main/js/src/jit/JitFrames.h)
  explicitly covers Ion, Baseline, BaselineInterpreter and the C++ interpreter
  stub with one function ABI. Arguments, callee identity, constructor state
  and caller descriptors share a stack contract. Entry and exit frames mark
  host transitions; frame pointers and descriptors support stack walking.

### Existing Otter contract

Ordinary interpreted calls push a 56-byte `Frame` into a vector and allocate
registers in a separate segmented arena. Generated calls publish an 80-byte
`NativeFrame` with a native-stack register window. `ActiveFrameStorage` and
`RuntimeFrameIdentity` distinguish these physical forms. Compiled entry
copies the common frame state into a Rust-local native record. Generic
compiled calls enter boxed runtime operations, which can invoke
`run_callable_sync_rooted` and nest a Rust dispatch loop. Generated callee
deopt also materializes the other frame representation.

A shared header does not remove those ownership and dispatch boundaries.
This item replaces both physical activation forms and their call paths.

### Chosen contract

- One C-layout JavaScript frame owns the caller link, source position,
  executable identity, SELF, receiver, new.target, argument window, register
  window, lazy arguments identity and cold-record identity. Every active
  interpreter, Template and Machine invocation uses that frame. Generated
  linkage owns its native-stack extent. The materialized `Frame` carrier,
  independent activation vector and register-arena ownership are removed.
- One generated call/construct trampoline per architecture accepts the loaded
  callee, receiver, new.target and complete actual arguments. It dispatches
  closures, bound functions, class constructors and declared native entries
  through fixed layouts. Callable kind and invocation mode are runtime
  inputs, not feedback-specific alternative call implementations.
- Bound arguments have collector-owned storage with a fixed machine layout.
  The trampoline prepends them, preserves their order through nested binds,
  applies bound receiver/new.target rules, and tail-dispatches the target.
  Generated code never interprets Rust `SmallVec` or container internals.
- Stable function entries always identify a valid execution destination:
  interpreter, Template or Machine. Promotion changes the destination;
  callers keep the same frame and argument contract. An unseen bytecode
  callee needs no recursive Rust invocation merely because it has no JIT
  generation yet.
- The Rust interpreter runs the current activation until a call, completion,
  exception or tier transfer needs the trampoline. It returns explicit
  continuation state after releasing its Rust borrows. The trampoline owns
  callee entry and caller resumption, so alternating interpreter/JIT calls
  do not accumulate Rust dispatch loops. Pending object-protocol work keeps
  its continuation in the existing cold semantic state.
- Host natives retain the `NativeCtx`/handle-scope boundary. Their native
  entry runs behind an explicit exit frame; a host callback enters JS through
  the same trampoline. Declared fast ABI natives are selected directly from
  their callable metadata. A real host callback may retain its host stack;
  ordinary JavaScript call dispatch does not retain a Rust dispatcher.
- GC and stack inspection walk the same published frame chain. Precise
  Machine roots remain attached to their owning activation and safepoint.
  Frame publication precedes allocating or reentrant work; no borrowed slot
  slice survives it. Stack limits count the unified JavaScript chain once.
- Return, throw, constructor completion, local catches and finally resume
  through the common continuation contract. Exact deopt fills the existing
  frame's canonical slots and transfers to interpreter entry. Inline recipes
  reconstruct only missing logical activations in that same stack. The old
  materialized/native conversion and generic-call replay paths are deleted.
- Suspended generators and async functions retain owned parked state rather
  than live stack pointers. Resumption creates an activation through the
  same entry contract; suspension is not a second active-frame mechanism.

Both emitters, bytecode dispatch, native entry, GC tracing, exceptions,
arguments, deopt, stack inspection and artifact metadata change together.
No compatibility carrier, alternate call mode, feature flag or environment
switch preserves the former paths.

### Entry and continuation ownership

The VM owns `JitCtx` and the callable entry signature, so the interpreter
can enter the same stack machinery without depending on the compiler crate.
A fixed call request describes the selected entry, frame header, callee,
receiver, new.target, formals and actuals. The native trampoline reserves and
initializes the frame, register window and complete argument window before
publishing the caller link. It retains no Rust activation between JS calls.
An execution entry returns `Continue` in the execution result domain to
consume the next call request; the trampoline invokes the child and resumes
the parent entry with its completion. The existing two-word result carrier
is retained, with domain validation separating this continuation from a
committed exception transition. Request values are copied before any
safepoint. A resumed entry must home its completion before allocating.
Stack-limit checks precede reservation; ABI alignment and preserved
registers are part of both architecture implementations.

The activation view borrows the frame cell published by the execution context.
Its indexing and iteration derive from caller links; it owns neither frames
nor register storage. A prepared interpreted call owns only incoming values,
optional register seeds for suspension/eval entry, and cold-record identity.
The trampoline copies that packet into its native reservation before Rust
entry consumes the packet owner. Return and exception cleanup keep the physical
frame published until the trampoline releases its extent. The interpreter
executes only its current frame: its unwind floor excludes the caller, and
child completion is delivered before resuming that caller. Host callbacks may
open another entry extent over the same chain. Suspensions copy values into
owned snapshots and resume through the ordinary call packet.

### Validation contract

Regressions exercise mixed-tier recursion, unseen callees, nested bound calls
and constructs, different actual/formal arities, lexical receiver/new.target,
derived constructors, native callbacks, moving GC, local catches/finally,
OSR and exact deopt. Structural tests check that generic generated calls
reach the shared trampoline and that every tier publishes the one frame
layout. Semantic comparisons and the full requested gates follow the
coherent replacement; the seven fixed-work cases measure its result.

### Interpreter and compiled entry control transfer

An interpreter entry selects a live generation while its one native frame
is published, then returns a tier-entry continuation to assembly. The
trampoline invokes the selected entry after the Rust dispatcher has returned
and passes its completion back to the interpreter entry on the same frame.
Return, throw and exact side exit are interpreted only by the owning VM
continuation; the assembly does not reconstruct or copy an activation.
The tier-entry bit belongs to the request and is cleared from published frame
flags. This uses the same continuation mailbox as child calls and retains
one physical caller link, register window and actual-argument window.
V8's interpreter entry and JSC's entry thunks likewise transfer through
machine entry code rather than retaining the interpreter's C++ dispatch stack.
The generation remains retained by the native activation retirement epoch.

### OSR and cold resumption use the same continuation

Sources inspected for OSR: V8
[`OnStackReplacement` / `Generate_OSREntry`](https://github.com/v8/v8/blob/main/src/builtins/arm64/builtins-arm64.cc),
JSC [`prepareOSREntry`](https://github.com/WebKit/WebKit/blob/main/Source/JavaScriptCore/dfg/DFGOSREntry.cpp),
and SpiderMonkey
[`BaselineScript::nativeCodeForOSREntry`](https://github.com/mozilla-firefox/firefox/blob/main/js/src/jit/BaselineJIT.cpp).
V8 computes the target from the code entry and OSR offset; JSC validates live
values and frame capacity before returning entry data; SpiderMonkey resolves
a bytecode offset to the code object's native OSR address. Otter keeps exact
PC and layout validation in the VM and transfers through its shared native
continuation instead of executing through a code-object Rust method.

Loop OSR selects an address from installed immutable code metadata and yields
the same tier-entry request as ordinary entry. The published frame records
its OSR origin for exit diagnostics and retry policy. No Rust method invokes
the selected body; `JitFunctionCode` exposes addresses and metadata only.
Compiled completion is delivered to the VM on that same frame, including
OSR exits outside the initiating loop and exceptions.

Cold deoptimization enters the common trampoline with a tier-entry request
over the already-published activation. The trampoline keeps its frame and
register window, drives interpreter and compiled continuations, and restores
the enclosing frame cell on return. Only actual child calls reserve another
extent. This removes the separate Rust resumption dispatch loop as well as
the JIT-owned entry wrapper and its separate `JitExecOutcome` carrier.
Measurement observes validated completion at the VM boundary rather than
wrapping a Rust entry invocation. The completion mailbox identifies a tier
transfer explicitly, so a cold inline deopt that already completed the frame
returns its committed result without dispatching that frame again. V8's OSR builtin and JSC's OSR entry thunk
likewise transfer to code using the existing activation and explicit entry
metadata; code objects do not own an interpreter dispatch stack.

The completion mailbox retains the entered generation's scalar identity.
Assembly saves it before the tier transfer and delivers it with completion,
so measurement still names the exact generation when cold deopt has already
changed the frame back to interpreter state. This occupies the context's
existing trailing alignment word; it retains no moving pointer or code lease.

### Complete register extents at every entry

All tiers publish the function's full canonical register extent at entry.
Formal slots receive actual values or undefined; remaining slots are initialized
before publication. Machine allocator homes and precise safepoint roots remain
separate from this canonical extent. Neither an entry-cell flag nor an exit
helper changes its visible length. The generation's parameter-prefix capability,
caller skip-initialization branches and cold window-expansion helpers are
deleted together. V8/JSC/SpiderMonkey likewise describe a frame through its
function/code metadata and explicit roots, rather than changing its declared
extent between entry and a runtime transition.

Owned error stacks, native call-site capture and bounded CPU samples all use
one visitor over the published physical chain and code-owned inline recipes.
Each source function resolves through its owning code-space context. The
activation-view-only diagnostic walker is deleted, so an optimized inline
parent cannot disappear merely because inspection began in another script.

### Actual arguments belong to every activation

JSC's [`CallFrame`](https://github.com/WebKit/WebKit/blob/main/Source/JavaScriptCore/interpreter/CallFrame.h)
stores the actual count and exposes argument slots independently of whether
the body creates an arguments object. SpiderMonkey's
[`JitFrames.h`](https://github.com/mozilla-firefox/firefox/blob/main/js/src/jit/JitFrames.h)
frame-layout documentation likewise distinguishes actual count from formals;
missing formals are undefined and extra actuals remain on the stack. V8's
[`ARM64 builtins`](https://github.com/v8/v8/blob/main/src/builtins/arm64/builtins-arm64.cc)
preserve actuals through construct entry and prepend bound arguments before
tail dispatch. These are invocation data, rather than an optional layout
chosen from the body's use of `arguments`.

Otter's single contract therefore reserves every actual argument immediately
after the complete canonical register extent, for interpreter, Template and
Machine entries on both targets. The frame count is unconditional. Delete
`INCOMING_ARGUMENTS`, the call plan's `needs_incoming_arguments`, optional-count
decoding and parameter-only forwarding. All producers initialize the complete
window before publication; rest, arguments materialization, suspension, GC and
forwarding consume that same window. Compiler restrictions on splicing bodies
that need arguments setup use the CodeBlock's semantic metadata, without
reintroducing an optional physical frame layout. Spread linkage must account
for its runtime count rather than publishing a frame with absent actuals.

### Function linkage always has an execution destination

V8's [`JSFunction` dispatch handle](https://github.com/v8/v8/blob/main/src/objects/js-function-inl.h)
selects code through the isolate dispatch table and patches that entry on
promotion. JSC's [`ExecutableBase::entrypointFor`](https://github.com/WebKit/WebKit/blob/main/Source/JavaScriptCore/runtime/ExecutableBase.h)
selects a call or construct entry with its arity contract. SpiderMonkey's
[`BaseScript::jitCodeRaw`](https://github.com/mozilla-firefox/firefox/blob/main/js/src/vm/JSScript.h)
names compiled code, an interpreter entry or a lazy-link entry; its documented
JIT-enabled contract keeps that destination non-null. The dispatch identity
exists independently of an optimizing generation.

Create Otter's permanent function cells when bytecode links, with immutable
formal/register counts and callable semantics. An interpreter destination is
present from first publication. Installing or invalidating a generation
switches that destination; it never restores an unresolved call state. The
VM owns both function and generation cells, including the interpreter entry,
and generated callers retain only permanent function identities. Delete the
zero-generation cold resolver, reverse-address scan and caller-specific
rebuild paths when this contract is wired through both emitters.

Generic generated dispatch reads a machine-visible directory of these cells
by the global function ID. Its pointer/count header is address-stable; the
pointer array may grow when linking, so generated code reloads it for each
lookup and retains no array pointer across allocation or JavaScript reentry.
Individual function cells remain pinned. This keeps code-space ownership and
generation retirement explicit without exposing a Rust map to machine code.
Snapshot restore creates a new directory and interpreter destinations.

The interpreter destination is a generated entry trampoline for an already
published canonical frame. It writes a tier-transfer request and tail-enters
the shared stack trampoline, which executes the interpreter and any subsequent
tier continuation over that same frame. Ordinary interpreter entry consumes
owned pending inputs when present; a generated caller already supplies the
complete initialized frame and needs no copied input packet or Rust call
resolver. Both forms use the same current-activation dispatch. Interpreter
entry has generation ID zero and never owns compiled safepoint metadata.
Its fixed entry reservation comes from the trampoline's actual ABI prologue.

Linking publishes every function cell, and compiled installation changes its
destination in place. Invalidation chooses another installed tier or the
owned interpreter entry. Cold-resolver stubs, their telemetry and recursive
target-rebuild policy are deleted with this change. The private runtime-stub
inventory is renumbered in place to retain its dense contract; artifacts name
the interpreter destination explicitly.

A body reading or forwarding its activation's actual arguments retains a
physical argument frame. The immutable CodeBlock admission includes
`CallForwardArguments` alongside an explicit arguments binding. Neither a
spliced parent recipe nor its caller's window represents those actuals in the
current IR. Such bodies therefore use their linked entry; this preserves the
reference engines' per-activation actual-count/slot ownership without adding
a materialization adapter to forwarding.

Committed forwarding publishes current SSA register bindings and the formals
context into the canonical frame before materializing an observable arguments
object. The explicit operand packet already owns those current values. The
frame is the collector-visible activation owner, so this publication ends
before allocation; arguments-object construction then reads live bindings from
the same frame. Intrinsic forwarding continues to consume the explicit packet.

Generated-entry accounting includes each function's permanent interpreter
cell. Reconciliation keys observations by the owned cell address, because
interpreter destinations all carry generation ID zero. Interpreter entries
already run function hotness and call-budget accounting inside the VM; their
generated-entry deltas contribute dispatch diagnostics without charging those
same calls twice. Compiled generation deltas continue to drive tier policy.
Snapshot restoration walks its fresh CodeBlocks to republish interpreter cells.

Logical completion belongs to the physical Frame until assembly releases it.
A Rust-side completed-frame address can outlive that release and compare equal
to a subsequent activation reusing the same stack storage. Completion therefore
uses a frame-header state bit; every interpreter, Template and Machine entry
initializes that header, so reused storage starts live. ActivationStack owns no
completed-address field. Nested execution restores only its context pointer;
completion-scope restoration touches only the currently published live frame.
This follows the reference engines' physical stack ownership instead of retaining
an address after the activation's extent ends.

Caller linkage does not wait for a callee's compiled generation. The permanent
interpreter destination makes the old pending-target sets, forced backedge
relink and successful-baseline refresh unnecessary. Delete that complete
caller-rebuild policy and its telemetry. Callee promotion still patches its
own function cell; ordinary speculative exits still own their invalidation
policy. A baseline poll may leave for optimizing OSR, but never because a
callee acquired entry code. Generated-entry tier costing stays unchanged.

Inline source frames also have one representation. V8's
[optimized frame summaries](https://github.com/v8/v8/blob/main/src/execution/frames.cc),
JSC's [StackVisitor code origins](https://github.com/WebKit/WebKit/blob/main/Source/JavaScriptCore/interpreter/StackVisitor.cpp)
and SpiderMonkey's [inline-frame iteration](https://github.com/mozilla-firefox/firefox/blob/main/js/src/jit/JitFrames.h)
recover logical inline activations from compiled metadata. An inlined body
has no independent physical call. Otter currently reconstructs a `Vec<Frame>`
for binding cold calls while property and generated calls retain virtual
recipes. Replace that second active-frame publication mechanism: all inline
parents remain code-owned safepoint recipes, and committed binding operations
resolve their owning function/PC from those recipes. Their context/value inputs
are already explicit SSA roots. Delete the recipe decoder, temporary physical
frames, publication scopes and the physical/virtual metadata switch. Actual
deoptimization still reconstructs missing interpreter activations through the
common stack trampoline. Both emitters already publish the enclosing call PC;
safepoints now describe the same logical origins for every cold inline call.

### Contract verification

The unconditional actual-window change passes the 11 native trampoline tests
on each architecture and real Template/Machine entry checks on both. The new
runtime regression inspects extra actuals in a callee with no arguments
binding, after a nested call and numeric deopt; ARM64 and x86 match the Node
oracle. ARM64 also passes this regression at GC stride 1 with verification.
All 13 forwarding runtime tests pass. VM/JIT library and test targets pass
clippy with warnings denied. Evidence is in
`benchmarks/results/call-trampoline-actual-window-*.log`.

The former bounded spread emitters could enter without retaining actuals.
They now decline that incomplete layout. Generated spread coverage remains
required when the shared call/construct linkage consumes the runtime argument
count; the final item gate cannot pass with that linkage still missing.

Focused checks on 2026-10-01 cover the native continuation core on ARM64 and
x86 (11 tests each), complete Machine windows on both targets, optimizing OSR
(3 runtime tests), inline property success/throw/exact deopt (2 runtime tests),
bounded CPU samples and native stack diagnostics. The inline publication
fixtures pass 4 tests, including moving roots and nested publication scopes.
The OSR and inline runtime tests also pass with GC stride 1.

The cross-script tests exposed two wrong assumptions: virtual inline stack
recipes looked up functions only in the inspecting script, and deopt looked
up resume PCs only in the current script. Both now resolve the owner by global
function ID. Getter counts, throw identity, source stacks and arrow receiver
state match their asserted semantics. The compiled completion mailbox also
distinguishes an already-completed inline deopt from a frame needing dispatch.

Six corpus cases passed 18/18 serial stride-1 runs against Node on frozen
debug binary `f84ca12f7954ae0853d34adc5a6342462e65e41096b04bddca9ecfa4f6b0ae52`.
That binary precedes the complete-window and unified-inspection changes;
this is focused evidence, not the final item gate. Results are in
`benchmarks/results/call-trampoline-osr-stress-one-verified/results.json`.
The current VM/JIT library and test targets pass focused clippy with warnings
denied and x86 compilation checks. Final release gates and fixed-work remain
pending until generic generated call/construct linkage replaces the remaining
Rust callee classification and manual generated frame setup.

Permanent-entry checks cover 11 registry tests, including directory growth,
promotion/invalidation and independent feedback from interpreter generation-zero
cells. Two VM tests execute the generated interpreter destination over an
already-published frame with no pending input and restore fresh interpreter
cells from a snapshot. After deleting caller relink, 35 focused runtime cases
pass: actual arguments, lifecycle, forwarding, inline properties and optimizing
OSR. Evidence is in `call-trampoline-no-caller-relink-*.log` and
`call-trampoline-permanent-entry-registry.log` under `benchmarks/results/`.

After replacing temporary physical inline publication, 35 ARM64 runtime tests
pass. On x86, all six inline binding/property tests pass. The new committed
binding test proves Machine splicing and an inline safepoint recipe, then
checks getter count, parent effects, throw identity and source stacks. It also
passes with GC stress stride 1 and verification. The 11 native trampoline
checks pass on both targets, and real Template/Machine entry tests pass on both.
VM/JIT library and test targets pass clippy with warnings denied; both target
compilation checks and formatting pass. Evidence is in
`benchmarks/results/call-trampoline-inline-source-*.log`.

### Native callable dispatch storage

JSC's [native call trampoline](https://github.com/WebKit/WebKit/blob/main/Source/JavaScriptCore/jit/ThunkGenerators.cpp)
loads the host entry from NativeExecutable after publishing the CallFrame.
V8's [API callback builtin](https://github.com/v8/v8/blob/main/src/builtins/arm64/builtins-arm64.cc)
keeps the callback address and actual arguments in its generated exit-frame
contract. SpiderMonkey's [trampoline infrastructure](https://github.com/mozilla-firefox/firefox/blob/main/js/src/jit/Trampoline.cpp)
likewise owns native entry/exit code independently of JavaScript callee bodies.
These engine boundaries let generated linkage select an entry from callable
metadata and retain precise roots while the host body runs.

Otter's native body currently stores a Rust enum containing a function pointer,
intrinsic or host-reference index. A generated classifier cannot consume that
layout. Replace it with one 16-byte C header: external identity, entry kind,
callable flags, length and an eight-byte payload. The kind selects a static
function, captured static function, VM intrinsic, shared host reference or local
host reference. Function-pointer payloads retain their typed Rust member; index
payloads have fully initialized bytes. Constructor/extensibility and metadata
flags belong to this header, with no second policy copy in the body. Captured
values remain in the existing traced ValueSlab immediately after the header.

Delete NativeCallSlot. Allocation, invocation, host-reference release, census,
snapshot images and metadata mutation use the header in place. Replace the
JIT snapshot's isolated native-ref offset with the complete callable layout;
ARM64 and x86 identity guards consume that layout. This changes storage for the
shared classifier and does not complete item 2 until the old generic dispatch
and manual generated frame linkage are also deleted.

### Generated deopt continuation ownership

V8's [ARM64 deoptimization entry](https://github.com/v8/v8/blob/main/src/builtins/arm64/builtins-arm64.cc)
calls its C++ reconstruction helpers, replaces the output frames in generated
code, then branches to the saved continuation. JSC's
[DFG OSR exit](https://github.com/WebKit/WebKit/blob/main/Source/JavaScriptCore/dfg/DFGOSRExit.cpp)
restores baseline state and selects the continuation from the code origin.
SpiderMonkey's [bailout helper](https://github.com/mozilla-firefox/firefox/blob/main/js/src/jit/Bailouts.cpp)
produces BaselineBailoutInfo for its generated bailout tail. The reconstruction
helper returns before JavaScript continuation execution; it does not recursively
run the target tier from that helper.

Otter's generated stack-call deopt currently passes eight words to a Rust
stub, which recursively resumes the interpreter while retaining the Rust VM
borrow. Replace that boundary with one generated deopt continuation entry on
ARM64 and x86. Its only operand is the typed side exit. Caller/callee generation,
logical inline source, constructor mode and resume PC come from the published
physical chain and its code-owned recipes. The Rust preparation helper validates
these facts, applies exit policy, rebuilds catches, changes the same frame to
interpreter ownership and installs its continuation request. It returns before
the assembly entry invokes the common trampoline on that frame. Success, pure
throw and fatal completion return to the generated caller without replaying its
call. Delete the eight-word stub and synchronous VM stack-call deopt method;
all call, construct and forwarding emitters consume the new fixed contract.

The same ownership rule applies to inline deopt. The current writeback helper
reconstructs inline descendants and recursively dispatches them from Rust.
Replace that synchronous execution with preparation of owned descendant inputs
and catches, followed by a generated writeback entry which tail-transfers to
the common trampoline only after the helper returns. The enclosing physical
frame is restored in place; descendants become native activations when assembly
consumes their requests. Delete the VM method that materializes and runs the
inline chain from Rust. Single-frame writeback still reports its exact side exit
so the entry owner can apply its generation policy once.

Generated calls retain their suspended source PC in SafepointRecord.call_pc.
The Frame.pc is the exact side-exit resume field and does not necessarily mirror
a currently executing generated call. The shared source resolver must use the
owned call PC and inline recipe while a call is published, and the canonical
frame PC for boundaries with no call recipe. Both caller diagnostics and boxed
semantic kernels consume that same resolver; no source IDs cross the deopt ABI.

The generic generated callee-kind classifier, complete runtime spread linkage
and replacement of the remaining manual generated call emitters are still
required. Item 2 has no final release gate, fixed-work comparison or signed
commit yet; these focused checks do not establish whole-item completion.

### Generated callee classification and the one call linkage

Design recorded 2026-10-01, before the classifier code. Sources read from the
current trees and kept under `benchmarks/measurements/research-2026-10-01/`:

- V8 [`builtins-arm64.cc`](https://github.com/v8/v8/blob/main/src/builtins/arm64/builtins-arm64.cc).
  `GenerateCall` tests Smi, loads the map and dispatches on instance type:
  callable JSFunction range → `CallFunction`, `JS_BOUND_FUNCTION_TYPE` →
  `CallBoundFunction`, then the map's IsCallable bit, `JS_PROXY_TYPE` →
  `CallProxy`, `JS_CLASS_CONSTRUCTOR_TYPE` → `ThrowConstructorNonCallableError`,
  any other callable through the call-as-function delegate, and a runtime
  `ThrowCalledNonCallable`. `CallFunction` converts the receiver only for
  sloppy, non-native functions: `null`/`undefined` become the global proxy,
  objects pass, primitives call the `ToObject` builtin inside an internal
  frame. `Generate_PushBoundArguments` checks the real stack limit, claims
  slots, keeps the receiver, inserts `[[BoundArguments]]` before the caller's
  actuals and tail-calls `Call` on the target, so nested binds concatenate
  inner prefixes first. `Generate_Construct` checks the map's IsConstructor
  bit before dispatching functions, bound functions (patching `new.target`
  to the bound target when it equals the bound function) and proxies.
  `JSConstructStubGeneric` creates the implicit receiver for base kinds with
  `FastNewObject`, passes `TheHole` for derived kinds, and after the call keeps
  an object result, otherwise reloads the receiver and throws if it is still
  the hole. Native API callbacks run behind an exit frame that owns the
  argument pointer (`CallApiCallbackImpl`).
- JSC [`ThunkGenerators.cpp`](https://github.com/WebKit/WebKit/blob/main/Source/JavaScriptCore/jit/ThunkGenerators.cpp).
  `virtualThunkFor` checks cell and JSFunction type, loads the executable's
  arity-checked entry for call or construct and jumps; host functions and
  `InternalFunction` select a native trampoline, and anything else goes to
  `operationVirtualCall`, which may run JavaScript and returns the target to
  jump to. `nativeForGenerator` publishes the already-built `CallFrame` as
  `topCallFrame`, calls the host function with that frame, so its arguments
  are frame slots, and checks the VM exception after return.
- SpiderMonkey [`BaselineCacheIRCompiler.cpp`](https://github.com/mozilla-firefox/firefox/blob/main/js/src/jit/BaselineCacheIRCompiler.cpp).
  `pushBoundFunctionArguments` pushes the caller's actuals, then the bound
  arguments, then bound `this` (or the bound target as `new.target`),
  and pads underflow with `undefined`. `emitCallNativeShared` pushes `vp`
  (callee/result, `this`, actuals), builds a native exit frame and calls
  `JSNative(cx, argc, vp)`; failure branches to the exception tail.

The common rule: callable kind is decided by generated code from object
layout, one path per kind. Bound arguments become ordinary actuals before the
target is entered. Receiver conversion, receiver allocation, proxy traps and
host bodies are runtime work invoked by that generated path, with every input
in a traced frame; none of them is a second call dispatcher.

Otter currently classifies callees in Rust in `invoke`, `run_callable_sync`,
`dispatch_construct_with_new_target` and their window-specialized copies,
unwrapping bound functions through `SmallVec` concatenation. Generated code
has two other paths: baked direct-call linkage that builds each callee frame
inline per site (three emitter families, receiver allocation, forwarding and
completion code), and boxed value stubs (`CALL_WITH_THIS_VALUE`,
`CALL_METHOD_VALUE`, `CONSTRUCT_VALUE`, `CONSTRUCT`, the spread op) that
enter Rust and nest another dispatcher. Natives called from the interpreter
are invoked inline by Rust without a frame.

Chosen contract:

- `CallRequest.entry == 0` means *classify*. Producers supply only callee,
  receiver, `new.target` (construct), the complete actual span, the
  `CONSTRUCT`/`TAIL_CALL` request bits and the return destination. A non-zero
  entry remains for VM-owned transfers (tier entry, deopt continuation,
  parked resumption, script/module/eval entries).
- The trampoline classifies before reserving any stack: function-id immediate
  and closure → function path; `BoundFunctionBody` → accumulate its slab
  length, replace the receiver with `bound_this` (call) or patch `new.target`
  (construct) and continue with `target`, bounded by the depth limit;
  `ClassConstructorBody` → its inner callable for construct, the
  class-call error for call; every other cell → a host frame. No GC can occur
  during classification, so the producer's span only has to stay valid until
  the copy.
- Function path: the permanent `FunctionEntryCell` found through the code
  registry directory supplies formal/register counts and immutable call flags
  (no-conversion, derived, constructible, suspendable). Call mode binds an
  arrow's lexical `this`/`new.target` from the closure, keeps strict and
  object receivers, maps `null`/`undefined` to the realm global for sloppy
  functions and defers primitive conversion. Construct mode rejects
  non-constructible kinds, passes the hole to derived constructors and defers
  base receiver creation. Suspendable kinds always enter the interpreter
  destination, which performs their promise/generator prologue on the
  published frame. The current generation cell supplies entry, header and
  code-object id; SELF is the exact closure (a class wrapper's inner callable).
- Host frame: the same `Frame` layout with a fourth kind `Host`, an empty
  register prefix of fixed small size for host-staged values, and the
  complete actual window. `header.function_id` carries the host kind chosen
  by the classifier (native body, proxy, object with an internal native
  `[[Call]]`, not callable, class called without `new`, not a constructor).
  The Rust host entry dispatches on that code only. Natives receive their
  arguments as the frame's actual slice under `NativeCtx`, like JSC's host
  calls. A host body that must call JavaScript next (`Function.prototype.call`
  and `apply`, proxy traps or targets) stages the callee/receiver/actuals in
  its own frame and returns a child request; it resumes from the frame's PC
  state with the child's completion. Host frames are skipped by stack
  diagnostics and activation indexing; collectors trace them like any frame.
- Activation preparation: after the frame is published and before entry, one
  Rust prepare hook runs only when needed — sloppy primitive `ToObject` and
  base construct receiver creation, writing `frame.this` in place. Its inputs
  are traced frame slots, it never executes the callee, and a throw releases
  the frame before propagation.
- Bound prefixes: total actual count is `Σ len(bound slabs) + argc`. The
  caller's span is copied to the tail of the actual window, then each bound
  slab is copied in front of the previously copied region walking from the
  outer wrapper inward, producing `inner…outer, caller` order without
  temporary storage. Formals are seeded from the combined window.
- Constructor completion belongs to the trampoline for every tier: object
  result → result; derived with `undefined` → frame `this`, or the
  uninitialized-`this` error if it is still the hole; derived with another
  primitive → `TypeError`; base → frame `this`. Interpreter completion already
  produces an object or a throw, so the rule is idempotent.
- Generated callers write the request into `JitCtx.pending_call` and call the
  trampoline: fixed actuals from an outgoing area on the caller's stack,
  spread actuals as the dense array's element pointer and length, forwarded
  actuals from the caller frame's window. Callee identity guards remain only
  where the optimizing tier speculates on the target. The trampoline returns
  `Success`, `Throw` or `Fatal`; callee deopt has already resumed inside it.
  Per-site manual frame construction, receiver allocation, forwarded-binding
  copies, completion emitters and the boxed call/construct value stubs are
  deleted together on ARM64 and x86, in both tiers.
- The interpreter records call feedback from the callee value (as Ignition
  does before calling the `Call` builtin) and returns every call as a
  classify request. `run_callable_sync`/`run_construct_sync` enter the same
  request through the host entry boundary. All Rust bound unwrap loops and
  callee-kind matches are deleted; Rust retains only host-kind bodies.

Base receiver creation stays a prepare-hook runtime call in this item. The
generated initial-map receiver allocation belongs to in-object slack tracking
(item 4), which replaces the constructor receiver caches that the deleted
per-site emitters consumed.

### Linked generation entries: callee-built frames for known targets

Design recorded 2026-10-01 after measuring the classify-only linkage. With
every generated call routed through the classifying trampoline, the fixed-work
corpus regressed against the frozen item-1 binaries (retired instructions,
same inputs): fib 3.02G → 5.57G, earley-boyer 100.2G → 139.7G, ts 154.5G →
191.3G, crypto 13.2G → 15.0G; ast_ctor, mega_method and zlib held. A sampled
fib run spends 66% of its time in the trampoline: about 250 instructions per
call (classification, registry lookup, receiver binding, request reload,
generic window loops, header copies, status dispatch), against ~224 for the
whole call under the deleted per-site linkage and ~12 in Node. Sources read
from the current trees and kept under
`benchmarks/measurements/research-2026-10-02/`:

- JSC [`JITCall.cpp`](https://github.com/WebKit/WebKit/blob/main/Source/JavaScriptCore/jit/JITCall.cpp),
  [`CallLinkInfo.cpp`](https://github.com/WebKit/WebKit/blob/main/Source/JavaScriptCore/bytecode/CallLinkInfo.cpp),
  [`CallFrame.h`](https://github.com/WebKit/WebKit/blob/main/Source/JavaScriptCore/interpreter/CallFrame.h),
  [`JIT.cpp`](https://github.com/WebKit/WebKit/blob/main/Source/JavaScriptCore/jit/JIT.cpp).
  `compileOpCall` documents the split: the caller initializes the callee
  frame's `argumentCountIncludingThis`, `callee` and the argument slots; a JS
  callee initializes `ReturnPC`/`CodeBlock` itself in its prologue. The call
  IC (`emitFastPathImpl`) compares the callee with the linked
  `m_callee` and calls `m_monomorphicCallDestination` directly; a miss
  calls the LLInt default-call thunk, which links or goes virtual. The
  linked destination is the callee code block's entry with or without arity
  check (`addressForCall(ArityCheckMode)`); `privateCompile` emits the
  prologue (frame pointer, stack-limit check, callee saves) and an
  arity-check entry that calls `arityFixup` only when the actual count is
  below `numParameters`. `CallFrameSlot` is one layout for LLInt, Baseline,
  DFG and FTL frames.
- V8 [`maglev-ir.cc`](https://github.com/v8/v8/blob/main/src/maglev/maglev-ir.cc)
  `CallKnownJSFunction::GenerateCode`: push receiver and arguments, padding
  missing formals with `undefined` up to the known parameter count, set the
  argument count, target, new.target and context registers and call the
  function's dispatch-table entry. The callee's prologue builds its frame;
  only the interpreter entry trampoline fills a register file.
- SpiderMonkey [`CodeGenerator.cpp`](https://github.com/mozilla-firefox/firefox/blob/main/js/src/jit/CodeGenerator.cpp)
  `visitCallKnown`: arguments are already padded by Warp, the caller pushes
  the callee token and a frame descriptor with the actual count and calls the
  JIT entry; a class constructor called without `new` goes to the generic
  invoke path; a constructing call replaces a primitive result with the
  `CreateThis` object inline.

Common rule: generic dispatch (classification, bound unwrapping, proxies,
natives, arity adaptation) lives in one shared path, but a call whose target
is known enters the callee's own code entry, and the callee's prologue — not
a generic builder — completes the frame it already knows statically. One
frame layout is shared by every tier; who writes which part is fixed.

Chosen contract, replacing the generic frame build for known targets:

- Every compiled generation (Template and Machine, ARM64 and x86) owns a
  *linked entry*, emitted by one shared per-architecture emitter and
  published in its `CodeEntryCell`. Inputs: context, callee (SELF), the
  unbound receiver, `new.target`, the caller's actual span (pointer, count,
  padded by the caller with `undefined` up to the callee's formal count) and
  the request frame flags. It establishes exactly the trampoline's native
  frame and saved registers, binds the receiver by the callee's static mode
  (lexical from the closure, strict as given, sloppy `null`/`undefined` to
  the realm global, primitives and base construct receivers through the same
  prepare hook), counts the entry for tiering, checks depth and native stack,
  writes the `Frame` header from immediates and the generation header,
  copies the actual window, seeds formals with straight-line moves, fills the
  remaining registers with paired `undefined` stores, publishes the frame
  and continues in the trampoline's shared run tail.
- The trampoline is split into three entry points over one native frame and
  register contract: request classification, the generic frame builder for
  classified requests, and the run tail (continuation loop, tier transfers,
  side-exit deopt continuation, construct completion, unpublish, return).
  The run tail exists once; linked entries and the classifier both end in it,
  so completion, deopt and tier-up have one implementation.
- A generated call site whose feedback names one bytecode target guards the
  callee identity, pads its actual span and calls the target's permanent
  `FunctionEntryCell`: current generation, then its linked entry. An identity
  miss, a generation without a linked entry (the interpreter destination) or
  any non-bytecode kind writes the classify request and calls the trampoline;
  a call-target miss is not a deopt. Promotion republishes the cell, so
  callers switch generations without recompiling.
- Request round trips through `JitCtx.pending_call` remain only for the
  classifier, staging entries and VM-owned transfers.

This deletes the generic register-window and header code from the hot path
for known targets while keeping classification, completion and deopt in one
place. Machine-to-Machine frame building moves fully into the callee
prologue later (one prologue instead of linked entry plus body prologue);
that is a refinement of this contract, not a second mechanism.

## Checkpoint

Series E1 (environments), state at the time of writing (2026-09-29):
- Committed before E1a: `894f3ffe` frame-cell batches, `cbb6e741` tooling,
  `3c51767c` weak-semantics GC fixes, `ceb3ca18` nursery age mark,
  `a892e658` this document, `6a09147d` environment corpus + contract
  (`2026-09-27-environments-contract.md`).
- E1a (section 8) is committed with its defect fixes. Gates on the final
  tree: otter-jit lib 302/302; difftest 84/84 (interpreter oracle, Template,
  production, GC stress 1/4/16); Test262 `language/` 24,077/24,077; the seven
  fixed-work workloads above; the `otter-runtime` JIT tests were updated to
  the context model and checked one binary at a time.
- Open: 12 of 15 environment corpora match Node (remaining: Annex B
  `function arguments`, derived-class field initialization timing); `-e`/`-p`
  source runs in the CommonJS eval scope, where every loop scope is a context
  (a `CopyContext` allocation per iteration of a plain counting loop).
- E1b (section 9): inline `===`, typed-view hoisting, atom lookups and young
  closures are committed; earley RSS is back under HEAD.
- Next: the ordinary-object layout (section 9: 96-byte receivers), then
  context size (drop the per-context `extension` word for scopes without
  sloppy eval, V8-style), inline context/closure allocation in both JIT tiers,
  singleton-context folding.
