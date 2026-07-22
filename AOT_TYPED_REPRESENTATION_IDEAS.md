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

**Status.** Not started. `crates/otter-compiler` ignores TS type annotations entirely
(no `type_annotation` reference anywhere), so this is greenfield: carry the annotation
from the oxc AST through the compiler into `CodeBlock`, then seed `ArithFeedback` /
method ICs from it at compile-snapshot time.

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

**Status.** Deprioritised on measurement: hello-world startup is ~30 ms wall total, so
the shim-eval share cannot be a large absolute win. Revisit if startup becomes a target
(edge/serverless) or if the shim set grows.

The second half of this item — letting the JIT see through native builtins instead of
crossing the bridge — has landed for the string probe family and collection leaf reads.
Extending it further means Array `push`/`pop`, which needs either a mutating-leaf ABI
(`*mut GcHeap`, currently every leaf takes `*const`) or a representation change: the
dense element buffer is a `Vec`, whose length cannot be adjusted from machine code, so
the `array_methods` feedback the VM already bakes stays unconsumed.

## 3. Deliberate memory-footprint target

**Technique.** A native-compiled binary with no JIT and no interpreter lands around
~1 MB RSS versus 40 MB+ for JIT runtimes, which unlocks embedded/edge deployment.

**Otter mapping.** Not adoptable as an architecture (Otter needs the tiers), but usable
as a **metric**: track baseline RSS of `otter` on a hello-world and treat regressions as
first-class, since serverless/edge is a target surface. Cheap to add to the bench
harness alongside the existing timing numbers.

**Status.** Measured once — hello-world is ~40 MB max RSS, ~30 ms wall. Not yet wired
into the bench harness as a tracked number.

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

1. TS annotations as feedback seed for faster tier-up (guarded by existing deopt).
2. Array `push`/`pop` inlining — blocked on a mutating-leaf ABI or a dense-buffer
   representation change.
3. Build-time snapshot of builtin shim setup — startup only, small absolute win.
4. RSS as a tracked benchmark metric.
