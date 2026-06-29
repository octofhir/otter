# JS Engine Research — Cross-Engine Insights for the Otter VM Rewrite

Synthesis of how V8, JavaScriptCore, SpiderMonkey, Hermes, QuickJS, Boa, Nova, and
the BEAM design the pieces we are rewriting. Goal: validate or correct our VM
decisions with evidence from shipped engines. JIT stays off/untouched this phase;
findings about JIT tiers are recorded for later.

Our corner: register-based bytecode, NaN-boxed 8-byte `Value`, moving generational
GC with pointer compression (32-bit offsets + `cage_base`), shape/hidden-class
objects with a contiguous slot slab, per-frame register windows.

---

## Decisions settled (2026-06-29)

1. **Value: flip to JSC pointer-cheap NaN-box** (pointers verbatim top16=0, free
   deref; doubles `+2^49`; int32 tagged) — hot gaps are object benches. → Plan
   Phase 0 (bedrock).
2. **Heap object slabs: 32-bit compressed + boxed heap doubles** (V8/Hermes) —
   density ×2. Registers keep full 64-bit pointers. → Plan Phase 1.
3. **GC: precise + moving + safepoint stack-maps** — keep compaction + compression,
   cure the per-call rooting tax. JSC conservative+non-moving NOT taken. → Plan
   Phase 4.

## 0. The headline — what the research changed

1. **Flat register stack: CONFIRMED, do it.** Hermes, Boa (v0.21), JSC, BEAM all
   put the register file in ONE contiguous stack; frames are windows into it. Our
   Phase 2 is the industry consensus, not a gamble.
2. **NaN-box in registers: CONFIRMED, keep.** Boa migrated enum→NaN-box for perf;
   Hermes/JSC use it. Nova's enum-index Value is slower. No change.
3. **NEW forced decision — TWO value representations.** Combining NaN-box +
   pointer compression means you cannot use one representation everywhere. Hermes
   (`SmallHermesValue`) and V8 both do: **64-bit NaN-box in stack/registers; 32-bit
   compressed slots in heap object slabs, with doubles BOXED on the heap.** Our
   slab today holds 8-byte `Value`s — halving it is a real lever (slab density ×2,
   GC scan bytes ÷2) but forces boxed heap doubles. **Decide explicitly.**
4. **Don't rewrite dispatch to tail-threading.** Blocked on stable Rust
   (`become`/`preserve_none` are nightly); honest payoff 1–5% even when it works.
   Register-VM under a `match` already banks the bigger Ertl 1.48× win. **Our
   earlier "dispatch rewrite" idea is RETIRED.** Real stable levers below.
5. **The per-call rooting tax is OPTIONAL, not inherent to moving GC.** Our biggest
   pain (per-call `HandleScope`, use-after-move bug class, fib boxing) comes from
   *incremental per-call rooting*, not from "moving." V8, Hermes, and BEAM keep
   moving GC **and** cheap roots via **safepoint stack-maps + compiler-emitted
   live-register metadata** — roots are discovered lazily at GC by scanning the
   contiguous stack window, with zero bookkeeping on the hot call path. This is the
   structural cure, and it does NOT require abandoning compaction/compression.
   (JSC's alternative — conservative scan + non-moving — is the other corner; §6.)
6. **One IC representation across interpreter + future JIT (CacheIR).** SM's
   biggest maintainability/perf lesson; V8's FeedbackVector echoes it. Build a
   linear guard-IR with constants in side "stub data" now, even with JIT off, so
   the interpreter IC and the future JIT IC are never written twice.

---

## 1. Value representation

| Engine | Scheme | Hot case made free | Cost case |
|---|---|---|---|
| Otter (now) | NaN-box, ptr tag `0x7FFC..`, doubles verbatim | doubles (float math) | **pointer deref needs unmask** |
| JSC | doubles offset `+2^49`, **pointers at bottom (top16=0)** | **pointer deref (verbatim)** | doubles (± offset) |
| Hermes | NaN-box, tag top16 `[0xfff9..]` | doubles verbatim | pointer unmask |
| V8 | low-bit Smi tag + 32-bit compressed ptr | Smi int | double boxed (HeapNumber) |
| QuickJS | NaN-box or tagged union; **refcounted tags negative** | `tag<0` = "is heap" one branch | — |
| Nova | enum + 32-bit typed index (rejected NaN-box) | type-safety | wider, slower |

- **Keep NaN-box** (Boa/Hermes/JSC confirm over Nova's enum). 
- **JSC's pointer-at-bottom layout** is the sharpest contrast: our hot gaps are
  object/property benches (tree, prop-access), and JSC makes `LoadProperty`/method
  IC fast paths skip the per-access pointer unmask by storing pointers verbatim
  (top16=0), paying a `±offset` on the rarer doubles instead. **Worth evaluating a
  flip** to pointer-cheap encoding — it compounds across every IC hit. Trade-off:
  costs float-heavy benches (nbody/mandelbrot), where we're already competitive.
- **int32 guard = 1 AND + 1 CMP** (JSC `isInt32`); keep a dedicated int32 tag so
  guards stay 2 instructions.
- **Two-level tag** (BEAM `00/01/10/11`): make the single hottest guard
  (is-pointer / is-int) one compare, finer subtype tests only on slow path.
- **ARM/Android caveat (Hermes):** never NaN-box *raw native* pointers — ARM MTE
  sets top byte, breaking the box. Box or side-table them. Relevant if we target
  Android.

## 2. Object model & shapes — universal consensus

V8 (Map), JSC (Structure), SM (Shape+PropMap), QuickJS (JSShape) **all agree** on
the model we already have. Refinements to adopt:

- **Values out of the shape; shape owns name→offset only.** (We do this.)
- **Fixed inline slots + out-of-line overflow — do NOT realloc the object body.**
  V8 `inobject_properties` + `unused_property_fields`; JSC `inlineCapacity~6` +
  butterfly; SM `numFixedSlots ≤31` + dynamic-slots array. A single slab we
  `realloc` on every add relocates the object and breaks IC offset stability across
  the inline boundary. **Cap inline slots at construction, spill the rest to a
  separate growable array.** Directly fixes our R2 overflow cliff.
- **Separate elements (integer-indexed) from named properties** into distinct
  backing stores (V8: ~20 ElementsKinds; JSC butterfly grows elements rightward,
  named leftward). Mixing them thrashes named-property shapes on indexed writes.
- **Shared BaseShape** = interned `{class, realm, proto}` (SM, JSC). Factor
  proto/class out of the transition tree so trees stay compact and
  compression-friendly. (Our memory notes a real bug here: constructor installs
  that dict-flip the prototype kill fast-shape inlining — V8/SM rule: **always
  advance the shape via define, never dict-flip**.)
- **Dictionary-mode-on-churn = fresh shape per mutation** (SM), so IC shape-guards
  self-invalidate. Keep dictionary mode the lone isolated slow path.
- **Compact 32-bit ShapeID compared as an immediate** in IC guards (JSC
  StructureID, 32-bit) — smaller header, cheaper guard than a 64-bit pointer
  compare. With our pointer compression, cache the **32-bit compressed shape
  offset** as the IC key.
- **Hot/cold field split + keyed side-table** (Nova): demote rare/optional/exotic
  fields (detach-key, max-byte-length, dictionary metadata) to a side table keyed
  by the object's GC offset; keep the inline object struct to ~one cache line.
  Continues our god-struct shrink (424B→88B→lower).

## 3. Inline caches — build ONE IR now (CacheIR)

SM's CacheIR is the strongest single recommendation for IC architecture:

- A **linear guard IR** (no branches/loops): `GuardShape Op0 Field0;
  LoadFixedSlotResult Op0 Field1`. Concrete `Shape*`/slot-offset live in per-stub
  **"stub data", not baked into the IR.**
- The **same IR is consumed by three tiers**: a CacheIR *interpreter* (baseline
  interpreter ICs), the baseline JIT (compiles to shared stubs — identical IR
  auto-shares code), and Ion (bakes constants into native code).
- **Why it matters for us, with JIT off:** design this IR now. The interpreter
  executes it via a tiny CacheIR interpreter; the future optimizing JIT lowers the
  *same* IR. We never write interpreter ICs and JIT ICs twice and keep them in
  sync — exactly the divergence class that produced our JIT crash (`VM_ABI_AUDIT.md`
  area D), applied to ICs.
- **Moving-GC synergy:** constants in stub data make IC roots *enumerable and
  traceable* — one updatable `Shape*` per stub, not pointers scattered through
  emitted machine code. Strictly better for a moving collector.
- V8 corroborates: **FeedbackVector** off-bytecode, indexed by a bytecode operand;
  interpreter *writes* feedback, optimizing tier *reads* it. One feedback
  structure, both tiers.

## 4. Frame / register stack — the concrete layout (Hermes + JSC + BEAM)

Our Phase 2 target, with the exact shipped layout to copy:

- **One contiguous tagged-value stack IS the register file** (Hermes
  `PinnedHermesValue` stack; JSC `cfr`-relative register file; Boa shared
  `Vec<JsValue>`). No per-frame `Vec`. Register `rN` = `base + N*8`, single
  base+index addressing, slot size = our 8-byte `Value`.
- **Frame metadata straddles caller/callee; args in consecutive registers ⇒
  zero-copy calls.** Hermes layout (callee view): header at negative offsets
  (`previousFrame`, `savedIP`, `savedCodeBlock`, `argCount`, `newTarget`, `callee`,
  `this`, `arg0..argN`), locals above. The callee's incoming args ARE the caller's
  top registers — no argument copy per call. JSC: header at positive `cfr` offsets,
  locals at negative. **This removes our "per-call frame build = fib 56%" tax and
  the frameless-self-call boxing.**
- **BEAM X/Y register split — adopt it.** Global small **temp bank** ("X",
  caller-saved, never spilled to a frame, reused every call) for transient
  operands + argument staging; **frame slots** ("Y", callee-saved) ONLY for values
  live across a call/yield/GC point. Our bytecode compiler already knows which
  temps die before the next call (liveness for reg-alloc) — route those to the
  global bank, keep them out of the frame. Shrinks frames, cuts per-call push/pop,
  **and shrinks the root set the GC scans at each safepoint** (§5).
- **8-bit register operands + long-form escape** (Hermes: no function uses >256
  regs across all FB mobile JS); linear-scan allocation in the compiler.
- **Identical frame layout across interpreter and JIT tiers** (JSC via offlineasm)
  so OSR needs no frame translation — bears on our `jit_osr_disabled` bail class
  (later, JIT phase).

## 5. GC — keep moving, but fix roots and the remembered set

The research says our moving generational GC is the right shape (Hades, Orinoco,
BEAM Cheney all agree); the problems are HOW we root and HOW we remember.

- **Root scanning: safepoint stack-maps + live-register metadata, NOT per-call
  rooting.** BEAM annotates the live X-register count at every GC point; V8/Hermes
  use safepoint stack maps. Roots = the live window of the contiguous stack,
  discovered lazily at GC. **Zero bookkeeping on the hot call path.** This is the
  cure for our per-call `HandleScope` tax and the whole use-after-move bug class —
  and it KEEPS moving + compression. Our flat-stack rewrite (§4) is precisely what
  makes "scan the stack linearly with a frame-descriptor table" possible.
- **Remembered set: precise per-page SlotSet, NOT a card table.** V8 field-logging
  SlotSet = 1 bit per pointer-slot, 1024-bit buckets, lazily allocated, ~3%
  overhead; minor GC iterates set bits → *exact* old→young slots. Our current
  card-table dirty-walk produced the swept-corpse use-after-free
  (`scan_old_dirty_cards`): a precise SlotSet **avoids that bug class** (no walking
  arbitrary headers on a dirty card). With 32-bit compressed slots, each entry is
  4 bytes and bit-density doubles. **Strong migration target for Phase 4.**
- **Two-level write barrier + Smi-skip** (V8): (1) bail if value is a small-int
  (NaN-box test, no decompress); (2) test a "from-here-interesting" bit in the host
  page header; (3) only then record the slot. Cheaper than our unconditional
  young/old card-set; the Smi-skip eliminates the majority of stores. Keep the fast
  path inline (matches our no-bridge rule), slow path a shared stub.
- **Off-heap refcounted large blobs** (BEAM refc-binaries, JSC): keep large JS
  strings/ArrayBuffers off the moving heap so the copier never relocates multi-MB
  payloads — cuts copy time and root-scan of big objects.
- **Explicit forwarding state in the header**, checked by BOTH dirty-slot walks
  (the `skip is_swept()` fix we already applied) — required so a half-copied object
  is never mistaken for live. NaN-box values must not be confused with a forwarding
  word: keep forwarding in the header, not the Value words.

## 6. The one genuine fork — moving+precise vs conservative+non-moving (JSC)

Engines diverge in exactly one place, and it is worth a deliberate decision:

- **JSC corner:** conservative stack scan + **non-moving** heap. The register file
  on the native stack *is* the root set, scanned conservatively → **no rooting
  protocol at all**, values stay unboxed in registers, simpler JIT (no safepoint
  root maps), and the entire use-after-move bug class disappears. **Cost:** no
  compaction (fragmentation, size-segregated free lists) and **no pointer
  compression on stack-reachable objects** (can't relocate/compress what an
  ambiguous stack word pins). "Conservative GC can be faster than precise GC"
  (Kniss/wingolog) is an empirical result, not folklore.
- **V8 / Hermes / BEAM / Nova corner (ours):** precise + moving → compaction +
  compression, root cost paid via stack-maps (V8/Hermes/BEAM) or lifetime handles
  (Nova, which self-reports an "800 bind/unbind soup").

**Resolution / recommendation.** The fork is real but our pain is NOT the moving
collector — it is that we root *incrementally per call* instead of via
*stack-maps at safepoints*. V8/Hermes/BEAM prove you keep moving + compression AND
get cheap roots by adopting §5's stack-map rooting. So:

- **Default recommendation: stay precise+moving, adopt stack-map rooting (§5).**
  Keeps compaction + our 32-bit compression investment, kills the per-call tax,
  matches our existing direction. This is the lower-risk path to the same win.
- **Only if stack-map rooting proves too invasive** for the JS/native boundary,
  the JSC conservative+non-moving hybrid (conservative on stack, precise on heap,
  non-moving) is the proven escape hatch — but it forfeits compaction and
  stack-reachable compression. Document it as the fallback, do not start there.

## 7. Dispatch & interpreter — stable-Rust levers (dispatch rewrite RETIRED)

- **Keep the big `match`.** Tail-call threading needs `become`/`preserve_none`
  (nightly); on stable a fn-ptr table loses register residency and often *loses* to
  a well-laid-out match. CPython's tail-call interp is ~1–5% real, not 10–15%.
  Register-VM under switch already banks Ertl's 1.48× (the register advantage is
  *largest* under switch dispatch). **Do not rewrite dispatch.**
- **Highest-ROI stable levers (all pure-stable, low-risk):**
  1. **Load-time pre-decode to a typed instruction array** (BEAM's real win):
     rewrite the byte stream once into `Vec<DecodedInsn>` with operands already as
     register indices/immediates and a dense opcode enum — eliminates per-dispatch
     decode.
  2. **Verify-then-`get_unchecked` register access**: validate every operand index
     `< nregs` at bytecode load, then `get_unchecked` in the hot loop (documented
     unsafe invariant). Removes a compare+branch per operand read.
  3. **Superinstructions / opcode fusion** for hot pairs from the
     `dispatch_loop_inner` profile (stable substitute for computed-goto sequence
     prediction).
  4. **Accumulator register** (V8 Ignition): one implicit in/out register halves
     operand encoding and removes a reg read/write per arithmetic op.
  5. **Next-op prefetch** (BEAM `NextPF`): read `code[pc+1]` discriminant early.
  6. **Lean match arms**: move cold/panicking/slow paths to `#[cold]
     #[inline(never)]` out-of-line fns so LLVM keeps a tight jump table.
  7. **`#[inline(always)]` NaN-box decode** so masks fold into handlers; branchless
     int-int fast path (`OR` the two values, one mask-compare, branch once to slow).

## 8. Reduction counting (BEAM) — make metering work-proportional

- BEAM charges reductions per call (budget `CONTEXT_REDS=4000`); long native BIFs
  **charge proportional reductions (~1 per 1000 bytes) and trap/resume** rather
  than counting as 1.
- **Adopt:** charge proportional reductions for O(n) native/builtin calls (string
  ops, JSON, regex, big array copies); make long native loops trap/resume. Keeps a
  single fat `JSON.parse` from blowing a preemption window. Check the reduction
  budget at call/back-edge opcodes, not every instruction.

## 9. Later (JIT off now) — template-per-opcode tier is the cheap first JIT

- BEAM's **BeamAsm** (asmjit, one machine-code template per instruction,
  concatenated at load, no IR/no deopt) gives a large win for modest engineering
  *because the bytecode is already close to the machine model*. V8 Sparkplug and
  copy-and-patch (Xu/Kjolstad OOPSLA'21) are the same idea. Our existing baseline
  template tier is the right shape; keep interpreter handlers self-contained with
  VM state passed by argument so a future copy-and-patch tier is a drop-in. The
  optimizing/speculative tier is the separate large effort.

## 10. Kiesel — conformance methodology (orthogonal to perf, high ROI)

Kiesel (Zig, Linus Groh / ex-LibJS) hits **93.4% test262** (between Boa 95.5% and
JSC 89.6%) on discipline, not codegen. Directly adoptable in our Rust engine:

- **Verbatim spec-step transcription.** Every abstract op carries a `/// 23.1.3.1 …`
  + spec URL doc-comment, and the body interleaves the exact numbered steps as
  `// 1.` `// 2.` comments above the implementing code (e.g. `arrayCreate` in
  `src/builtins/array.zig`). Completions → `Result<Value, Throw>` + `?` for
  `?`-prefixed steps. Most test262 failures are subtle step *ordering* (when
  ToNumber/getters fire); verbatim comments make every line auditable. **Adopt as a
  coding standard for `crates/otter-vm` AOs — highest conformance ROI, zero infra.**
- **Mark every spec deviation / fast-path** with a greppable `// OPTIMIZATION:` /
  `// FAST PATH (spec-equivalent):` tag (~31 in Kiesel) so IC/JIT shortcuts never
  silently desync from the algorithm they approximate.
- **Gate = committed full PASS/FAIL/SKIP snapshot vs a pinned test262 SHA**, `diff`
  fails on ANY change (regression *or* unrecorded improvement). Stronger than a
  failing-set: recording PASS too catches a fix in area X that breaks area Y.
  strict+sloppy mismatch ⇒ FAIL. Small declared SKIP list for flaky/slow, not env
  flags. Build the snapshot in a checks-on build (debug-assert/overflow) so a panic
  = deterministic FAIL. **Upgrade our failing-set gate to this.**
- **Intl → ICU4X, Temporal → `temporal_rs`, regex → (we have our own).** Both are
  Rust libs (temporal_rs is the Boa team's, the path V8/Node use); as a Rust engine
  we take them as **direct deps, no FFI** — cleaner than Kiesel's Zig↔Rust bridge.
  Highest-leverage Intl/Temporal conformance move; do not hand-roll CLDR/calendar.
- **VM corroboration:** Kiesel folds `[[Prototype]]` + extensible + internal-methods
  **into the shape identity** (one shape-guard covers proto-chain) and keeps
  integer-index props in a dense side-table separate from the shape — confirms §2.
  Descriptor packing 80B→32B via data-vs-accessor tagged variants (free win if our
  `PropertyDescriptor` carries nullable accessor+data together). Its biggest perf
  wins were **allocation reduction** (single-`ArrayList` parser, lazy intrinsic
  init), not algorithmic — same cheap-win class as our P3.
- **GC:** Kiesel uses conservative `bdwgc` and thereby skips precise rooting — the
  same tax/simplicity trade as the §6 fork. We are ahead here (moving + generational).

---

## How this maps to the refactor plan

| Plan phase | Research verdict |
|---|---|
| **P1 object model** | CONFIRMED + sharpen: fixed-inline + out-of-line overflow (no body realloc), separate elements store, shared BaseShape, 32-bit ShapeID IC, hot/cold side-table. **Decide: 32-bit compressed slabs (boxed heap doubles)?** |
| **P2 flat register stack** | CONFIRMED — copy Hermes/JSC straddling-frame zero-copy-call layout + BEAM X/Y temp-bank/frame split. **Drop the dispatch-rewrite sub-item** (retired); add load-time pre-decode + verify→get_unchecked + accumulator + superinstructions as the interpreter levers. |
| **P3 thin Result** | unchanged (cheap broad win) |
| **P4 GC roots** | sharpen: **precise SlotSet remembered set** (not card table — kills swept-corpse bug class) + two-level Smi-skip barrier + **safepoint stack-map rooting** (the per-call-rooting-tax cure that keeps moving+compression) + off-heap large blobs |
| **NEW cross-cutting** | **CacheIR-style one-IC-IR** for interp+future JIT (build now); **reduction metering** work-proportional (BEAM) |
| **NEW decision** | the §6 GC fork — recommend precise+moving+stack-maps; JSC conservative+non-moving as documented fallback |

## Sources

V8: v8.dev/blog/{ignition-interpreter,fast-properties,pointer-compression,
trash-talk,orinoco-parallel-scavenger}, mathiasbynens.be/notes/shapes-ics,
wingolog.org/archives/2024/01/05 (precise field-logging remembered set).
JSC: webkit.org/blog/{7122,9329,10308,12967}, opensource.apple.com JSCJSValue.h,
caiolima.github.io/jsc, wingolog.org/archives/2024/09/07 (conservative GC).
SpiderMonkey: searchfox Shape.h SMDOC, spidermonkey.dev/blog 2021/04/22,
jandemooij.nl/blog/cacheir, cfallin.org/blog/2023/10/11 (PBL).
Hermes: github.com/facebook/hermes {Design.md, StackFrameLayout.h, HermesValue.h,
SmallHermesValue.h}, deepwiki.com/facebook/hermes.
QuickJS: deepwiki.com/bellard/quickjs, carl-vbn.dev quickjs internals.
Boa: boajs.dev/blog/2025/10/22/boa-release-21 (register VM + NaN-box migration).
Nova: trynova.dev/blog/{what-is-the-nova-javascript-engine,why-build-a-js-engine,
garbage-collection-is-contrarian}.
Dispatch: Ertl/Gregg "VM Showdown" (scss.tcd.ie), sillycross.github.io (Deegen),
mattkeeter.com/blog/2026-04-05-tailcall, blog.nelhage.com/post/cpython-tail-call,
fredrikbk.com (copy-and-patch), lua.org/doc/jucs05.pdf.
BEAM: blog.stenmans.org/theBeamBook, erlang.org/blog/a-brief-beam-primer,
beam-wisdoms.clau.se, erlang.org/doc/apps/erts/beamasm.html.
