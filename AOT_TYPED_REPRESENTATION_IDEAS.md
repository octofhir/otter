# Ideas from AOT-compiled JS engine research

Notes on techniques observed in an ahead-of-time JS→Wasm/native compiler line of work,
filtered for what is actually applicable to Otter (bytecode VM + template/optimizing JIT).
Each item states the technique, why it works, and the concrete Otter mapping.

Items already landed have been removed; what follows is the open remainder.

## 1. Type annotations as *hints*, never as contracts ("progressive typing")

**Technique.** Parse TS annotations and feed them to codegen as optimization hints.
Untyped code keeps full dynamic behaviour; typed code drops redundant type checks and
type-dispatch branches. Crucially the annotations are **unsound by construction**, so
they may only be used where a guard/deopt exists — never as a proof.

**Measured shape of the win** (bf-mandelbrot interpreter, AOT setting):
untyped 272s → hints on 184s (1.5×) → int-specialized number repr 84s (3.2×) →
elided boundary checks 67s (4×), i.e. parity with a JIT engine.

**Otter mapping.** Otter has an optimizing tier with real exact-PC deopt, which makes
this *sound* in a way an AOT compiler cannot achieve: use TS annotations as an extra
feedback source seeding the typed-feedback epochs, guarded by the existing deopt frame
state. Concretely: annotation `number` → seed the numeric repr-selection instead of
waiting for profile warmup; annotation `string`/class type → seed the shape/method
inline cache. The value is *faster tier-up*, not extra assumptions — a wrong annotation
just deopts once.

Second-order note: the same table shows that after removing type checks, the next
biggest single lever was the **number representation** (int-vs-double), not more
inlining. That matches Otter's repr-selection work being the right next lever.

**Status.** Landed for the numeric half (`f00c7bdf`). `number` annotations on
parameters, variable declarators, and `as number` assertions become a per-binding
static hint; arithmetic and comparison sites with statically-Number operands are
recorded on `Function::number_hint_sites`, carried into `CodeBlock` as a bitset, and
read by `jit_compile_snapshot` **only when the feedback cell is still empty**. A real
observation always supersedes the seed, so a warmed-up site still narrows from
`Float64` to `Int32`. Confined to opcodes that already emit a representation guard, so
a wrong annotation costs one deopt.

Measured: a hot function with a cold arithmetic branch takes 998 optimizing-tier bails
unannotated, 0 annotated. Steady-state wall time is unchanged — the win is warmup, as
the item predicts.

The method-IC half is **not** done, but it is more tractable than first assumed. The
blocker was stated as "a class annotation cannot be resolved to a `ShapeId`, since
shapes are runtime identities" — true at *compile* time, but the seed is applied at
*snapshot* time, which runs against a live interpreter. `Interpreter` already keeps
`simple_constructor_shape_cache: FxHashMap<function_id, ShapeHandle>`, so the missing
pieces are only:

1. the compiler resolving a class-typed annotation to the declaring class's function
   id (locally declared `class` only — interfaces and aliases have no runtime identity),
2. carrying that id on the site the way `number_hint_sites` carries the numeric one,
3. a bake step that looks the id up in that cache and seeds the property IC with the
   resulting shape plus the slot resolved from it.

Roughly the size of the numeric half plus the class-resolution step.

## 2. Self-hosted builtins in a compiled JS/TS subset

**Technique.** Write the standard library (String, Array, Math, …) in a restricted,
typed JS/TS dialect with escape-hatch intrinsics (raw wasm ops, raw pointers, explicit
type tags). Precompile them at build time into the engine binary, so at runtime there
is *zero* interpreted prelude and no JS shim evaluation cost.

**Payoff.**
- Builtins get the same optimizer, inliner and type specialization as user code.
- No FFI/bridge boundary between user JS and library code — the hottest thing in a
  bridge-bound workload disappears by construction.
- Startup does not pay for shim parsing/eval.

**Otter mapping.** Remaining piece: **snapshot the shim work** at build time
(`build.rs`) instead of evaluating the JS shims (`web_bootstrap.js`, the
`crates/otter-node/src/*.js` set, …) per isolate.

**Status.** Measured, and the earlier deprioritisation rested on a wrong number. The
share is *not* small — it is nearly all of startup:

| phase (hello-world, best of 3) | cost |
| --- | --- |
| whole process | 18.5 ms |
| `runtime_build` | 18.3 ms |
| ↳ `with_web_apis` | 16.2 ms |
| ↳↳ parse + compile of the 4 web shims (128 KB) | 6.2 ms |
| ↳↳ shim execution + native class installs | ~10 ms |
| `with_node_apis`, `with_otter_modules` | ~0.1 ms each (lazy) |

What a build-time snapshot can actually remove is the 6.2 ms compile, not the ~10 ms of
execution — that would need a serialized *heap* image, not serialized bytecode. And the
saving is the compile cost minus rehydration: a `serde_json` round-trip of the same
modules costs 5.3 ms, so JSON is a net loss; a compact binary encoding would have to
land near 1.5 ms to make the item worth ~4.5 ms of an 18.5 ms startup.

So the item stays open but is correctly ranked last: it needs a binary `BytecodeModule`
encoding plus a `build.rs` codegen pipeline in `otter-web` to buy ~25 % of startup, and
nothing else in the file is blocked on it. Note that the lazy alternative is closed: the
lazy web-globals path was deliberately removed because a shim eval nested inside a
native getter frame is a rooting hazard.

The second half of this item — letting the JIT see through native builtins instead of
crossing the bridge — has landed for the string probe family, collection leaf reads,
and now dense Array `push` / `pop` / `shift` / `unshift`. The blocker named here was real and is now gone: a
`MutatingLeafValue2` signature (`*mut GcHeap`, class still `LeafNoAlloc`) carries `pop`,
and `push` reuses the existing `AllocValue3` entry because appending may grow the
buffer. Machine code guards the receiver as an `ArrayBody` with no exotic sidecar and
reuses the prototype/builtin identity guards; the preconditions it cannot see (writable
`length`, extensibility, an accessor override in range, the indexed-accessor protector)
are re-checked by the entry, which misses so the site falls through to ordinary
dispatch. The dense-buffer representation change turned out to be unnecessary — only the
ABI was missing.

## 3. Deliberate memory-footprint target

**Technique.** A native-compiled binary with no JIT and no interpreter lands around
~1 MB RSS versus 40 MB+ for JIT runtimes, which unlocks embedded/edge deployment.

**Otter mapping.** Not adoptable as an architecture (Otter needs the tiers), but usable
as a **metric**: track baseline RSS of `otter` on a hello-world and treat regressions as
first-class, since serverless/edge is a target surface. Cheap to add to the bench
harness alongside the existing timing numbers.

**Status.** Landed. `benchmarks/run-startup.sh` reports best-of-N wall time and peak RSS
for `otter`, `node`, and `bun` on a hello-world, and `run-all.sh` runs it first (it is
cheap). `measure_wall_and_rss` in `common.sh` reads both numbers out of `/usr/bin/time`
itself, handling the BSD (`-l`, seconds + bytes) and GNU (`-v`, `h:mm:ss` + kilobytes)
dialects, so the harness's own fork/exec is excluded.

Current numbers on this machine: otter 20 ms / 38 MB, node 20 ms / 44 MB, bun 10 ms /
20 MB. Startup wall is at parity with node; peak RSS is between bun and node.

## 4. Explicitly *not* adoptable

- **AOT-only, no interpreter/JIT.** Removes deopt entirely, which forces every type
  assumption to be conservative or unsound. Otter's tiered design is strictly stronger.
- **Prototype-chain shortcuts.** Skipping/pruning prototype semantics to simplify
  codegen is a conformance debt Otter should not take on.
- **Compile-to-C backend.** Interesting for embedded distribution, orthogonal to
  current perf goals.
- **Conformance plateau + polyfill.** Otter already gates on failing-set diffs, which
  is the stronger form. One transferable expectation only: suite wall time *grows* as
  conformance improves, because failing tests exit early — a slower suite is not a
  regression signal.

## Ranked take-aways for Otter

1. ~~TS annotations as feedback seed for faster tier-up~~ — numeric half landed; the
   method-IC half needs a compile-time class → shape registry first.
2. ~~Array `push`/`pop` inlining~~ — landed through a new mutating-leaf ABI; the
   dense-buffer representation change proved unnecessary.
3. Build-time snapshot of builtin shim setup — the only item still open. Worth ~4.5 ms
   of an 18.5 ms startup, and only with a compact binary `BytecodeModule` encoding;
   see the measured breakdown above before starting it.
4. ~~RSS as a tracked benchmark metric~~ — landed as `benchmarks/run-startup.sh`.

`shift` / `unshift` followed, and needed more than an entry: neither had a dense path at
all, so both ran the generic per-index protocol. On identical work a dense receiver now
takes 8.9 ms where the generic path takes 4617 ms.

Wiring those fast paths surfaced four real defects, all fixed alongside: an in-bounds
hole not consulting the prototype chain for an inherited *data* property (the protector
only latched on accessors); a numeric index reaching the exotic arms of the element-load
path still spelled as a Number and failing the family match; `Object.preventExtensions`
/ `isExtensible` carrying a second receiver-family list that had diverged from the
internal methods; and the promise combinators continuing to iterate after an abrupt
per-element step, which hung on an infinite iterable. Suite effect: `built-ins/Array`
and `built-ins/Object` to 100 %, `built-ins/Promise` 656 -> 676 with 20 timeouts gone.
