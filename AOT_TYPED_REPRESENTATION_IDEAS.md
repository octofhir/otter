# Ideas from AOT-compiled JS engine research

Notes on techniques observed in an ahead-of-time JS→Wasm/native compiler line of work,
filtered for what is actually applicable to Otter (bytecode VM + template/optimizing JIT).
Each item states the technique, why it works, and the concrete Otter mapping.

## 1. Latin-1 ("byte") string representation

**Technique.** Strings are immutable in JS, so the full code-point range of a literal
(and of most derived strings) is known at creation time. If every code unit is `< 256`,
store one byte per char instead of two. Layout stays `[u32 length][data]`, only the
element width changes; the string's *type tag* distinguishes the two forms.

**Payoff.** ~50% memory cut on ASCII-dominant workloads (JSON keys, identifiers, HTTP
headers, source text), plus better cache behaviour on scan-heavy ops (`indexOf`,
`charCodeAt`, comparison, hashing).

**Cost / pitfall.** Every string op becomes a 2×2 matrix (byte×byte, byte×wide,
wide×byte, wide×wide). Without care this explodes the builtin surface. Mitigations:
- Make the *only* mixed-form path a widening promotion, then run one wide algorithm.
- Keep a monomorphic fast path for byte×byte (the common case) and one generic fallback.
- Concat result form = `byte if both operands byte`, cheap to compute from the tags.

**Otter mapping.** `JsStringBody` already carries a `utf16_cache`; a `Latin1` variant
next to the existing representation would let the cache be skipped entirely for ASCII
(the cache exists to avoid re-decoding). Also directly benefits the regex engine:
an ASCII-only subject enables the byte-level prefilter/LUT prescan already on the
regex backlog.

## 2. Type annotations as *hints*, never as contracts ("progressive typing")

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

## 3. Self-hosted builtins in a compiled JS/TS subset

**Technique.** Write the standard library (String, Array, Math, …) in a restricted,
typed JS/TS dialect with escape-hatch intrinsics (raw wasm ops, raw pointers, explicit
type tags). Precompile them at build time into the engine binary, so at runtime there
is *zero* interpreted prelude and no JS shim evaluation cost.

**Payoff.**
- Builtins get the same optimizer, inliner and type specialization as user code.
- No FFI/bridge boundary between user JS and library code — the hottest thing in a
  bridge-bound workload disappears by construction.
- Startup does not pay for shim parsing/eval.

**Otter mapping.** Otter's builtin JS shims (`setup-builtins-*.js`) are today real JS
evaluated at boot, and its native builtins sit behind the native bridge (documented as
the bottleneck for `serve`, map/set, and several benches). Two separable steps:
1. **Snapshot the shim work** at build time (`build.rs`) instead of evaluating shims
   per isolate — pure startup win, no semantic change.
2. **Let the JIT see through builtins**: for the hottest natives, provide an inlinable
   IR body (or an intrinsic node) so the optimizing tier can splice it instead of
   emitting a bridge call. This is the same lever as the existing inline work, applied
   to the library rather than to user methods.

## 4. Precompiled/generated Unicode and regex tables

**Technique.** Unicode property data and regex character-class tables are *generated*
into source at build time rather than computed or loaded at runtime.

**Otter mapping.** Aligns with the ASCII-class bitmap item already on the regex list;
extend to case-folding tables for the case-insensitive prefilter.

## 5. Conformance strategy: measure the *set*, accept a plateau, then polyfill

**Technique.** Track a conformance percentage continuously, but treat coverage as a
curve with a knee — past a threshold the remaining tail is cheaper to cover by
transpilation/polyfill than by engine work. Also observed: **test-suite wall time grows
as conformance improves**, because failing tests exit early. Do not read a slower suite
as a regression.

**Otter mapping.** Otter already gates on failing-set diffs rather than a raw
percentage — keep that; it is the stronger form. The transferable bit is the runtime
expectation: a rising suite duration is an expected artifact, not a signal.

## 6. Deliberate memory-footprint target

**Technique.** A native-compiled binary with no JIT and no interpreter lands around
~1 MB RSS versus 40 MB+ for JIT runtimes, which unlocks embedded/edge deployment.

**Otter mapping.** Not adoptable as an architecture (Otter needs the tiers), but usable
as a **metric**: track baseline RSS of `otter` on a hello-world and treat regressions as
first-class, since serverless/edge is a target surface. Cheap to add to the bench
harness alongside the existing timing numbers.

## 7. Explicitly *not* adoptable

- **AOT-only, no interpreter/JIT.** Removes deopt entirely, which forces every type
  assumption to be conservative or unsound. Otter's tiered design is strictly stronger.
- **Prototype-chain shortcuts.** Skipping/pruning prototype semantics to simplify
  codegen is a conformance debt Otter should not take on.
- **Compile-to-C backend.** Interesting for embedded distribution, orthogonal to
  current perf goals.

## Ranked take-aways for Otter

1. Latin-1 string representation — largest structural win, touches memory *and* regex.
2. Builtin inlining through the native bridge (bridge cost is a measured bottleneck).
3. Build-time snapshot of builtin shim setup — startup only, low risk.
4. TS annotations as feedback seed for faster tier-up (guarded by existing deopt).
5. Generated Unicode/class tables for the regex engine.
6. RSS as a tracked benchmark metric.
