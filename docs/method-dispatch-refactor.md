# Method & Prototype Resolution — Unification Plan

## Status / execution order

Work proceeds strictly in this order; each item lands green and
conformance-gated before the next starts.

- [x] **Audit** — cross-type spec probe of method/prototype resolution
  (this doc's two audit tables).
- [x] **Side spec-bug: for-of `break` IteratorClose** (§14.7.5.6) —
  fixed in `compile_for_of_statement` (commit `f4a30331`). Separate from
  the dispatch scatter; `for await` IteratorClose still pending.
- [x] **Crash gate A: finally invalid operand + private brand re-eval**
  (§14.15 `try`, §15.7 private names/brands, §10.5 proxy internal
  methods) — fixed the real VM abort in the try/finally handler target
  and made private names per class evaluation by capturing runtime
  `Symbol("#name")` keys. The six private
  `multiple-evaluations-of-class-function-ctor` tests now pass; the
  try/finally filter has 0 crash/timeout/OOM. `S12.14_A11_T4.js` now
  passes; `completion-values-fn-finally-abrupt.js` is downgraded from
  process abort to ordinary semantic fail pending completion-value
  semantics. Full project gate (`cargo test --all --all-features`,
  `cargo clippy --all-targets --all-features -- -D warnings`) is green.
- [x] **Stage 1** — collapse `indexOf`/`lastIndexOf`/`includes` to one
  `Interpreter::array_indexed_search` entry shared by both call sites
  (was 4 duplicated interception blocks). Pure structural refactor,
  conformance unchanged (indexOf 176/21, lastIndexOf 174/21,
  includes 23/4). The intrinsic-table dense array arms of `impl_*` are
  now reachable only on the context-less fallback path; their removal
  is folded into Stage 5 (table collapse).
- [~] **Stage 2** — give every builtin a re-entrant handle (replace
  `IntrinsicArgs`); fold context-carrying interceptions back into impls.
  Fixes the String/RegExp/join receiver-coercion gaps as a class.
  - [x] **String receiver coercion** — `native_string_method` now runs
    `RequireObjectCoercible` + `ToString(this)` uniformly for every
    method except `toString`/`valueOf` (was HTML-methods only), so
    `String.prototype.X.call(obj)` observes a user `toString`.
    built-ins/String 1067→1147 pass (+80), 0 crash.
  - [x] **RegExp `exec`/`test` argument `ToString`** — pre-coerce arg 0
    through `ToPrimitive(String)` at the regexp dispatch arm so an
    Object argument's user `toString` fires (was matched against
    `"[object Object]"`). built-ins/RegExp 1147→1155 pass.
  - [x] **TypedArray integer-indexed string-key `[[Get]]`/`[[HasProperty]]`**
    — `ordinary_get_value` now resolves a CanonicalNumericIndexString
    key to the element (§10.4.5.4 IntegerIndexedElementGet) instead of
    `undefined`, and `in` (`run_has_property_regs`) delegates a
    TypedArray receiver to `ordinary_has_property_value`. Fixes
    `Reflect.get/has(ta,"i")`, `n in ta` (was a TypeError crash), and
    generic `Array.prototype.indexOf/includes.call(ta)`. built-ins/
    TypedArray 1498→1508 pass.
  - [x] `Array.join` generic receiver — `Interpreter::array_join`
    runs the §23.1.3.16 ladder with re-entry (LengthOfArrayLike →
    ToString(sep) → per-index Get + ToString), routed from the
    `.call` bridge for non-Array receivers; observes a `get length()`
    accessor and boxes a primitive (string) receiver. Plus
    `length_of_array_like` now ToNumber-coerces a wrapper-object /
    `valueOf` length (§7.1.20), shared by join / indexOf / lastIndexOf
    / includes. built-ins/Array 2230→2245, 0 crash. (Dense-Array
    `.join` still uses `impl_join`: separator/element ToString and
    inherited `Array.prototype[k]` indices on a dense receiver are the
    remaining ~3 join tests — fold the dense path into `array_join`.)
  - [~] Mechanical migration toward one re-entrant dispatch per type,
    per type (String first):
    - [x] **String unified** — extracted the shared argument coercion
      into `Interpreter::coerce_string_method_args`; both the
      primitive-string fast path in `do_call_method_value` and the
      `.call`/property bridge now funnel through the JS-visible
      `String.prototype` native methods with identical
      receiver + argument coercion. Fixed a `replaceAll` slice OOB
      panic (`"a".replaceAll("aa", fn)`) exposed by the funnel.
      built-ins/String 1147→1152, 0 crash. (The 46 `impl_*` bodies
      still take `IntrinsicArgs` — invoked once from
      `native_string_method` — so the receiver/arg coercion is no
      longer duplicated; folding coercion into each body is the
      remaining polish.)
    - [x] Array — migrated to re-entrant drivers, including search,
      join, callback methods, prototype-aware indexed access, concat,
      sort, mutators, splice/slice/copyWithin, and change-by-copy.
      Latest broad gate: built-ins/Array 2776 pass, 222 fail,
      332 skip, 1 timeout, 0 crash.
    - [ ] TypedArray, Date, … same treatment.
- [x] **Stage 3** — Array callback methods use one live
  `run_callable_sync` path with per-index `HasProperty`/`Get`.
  Function/exotic receivers and callback-side mutations are observed;
  array `length` shrink now preserves non-configurable indexed
  properties. `map`/`filter`/`flatMap` now create their result with
  `ArraySpeciesCreate` before callback iteration and define outputs via
  `CreateDataPropertyOrThrow`, so species/proxy target failures are
  observed in spec order. Focused gates: `map` 210/210 runnable,
  `reduce` 512/512 runnable, `reduceRight` 256/256 runnable,
  `forEach` 186/186 runnable, `every` 214/214 runnable, `some` 215/215
  runnable, `filter` 235/236 runnable. Broad gate:
  built-ins/Array/prototype 2591 pass, 127 fail, 92 skip, 1 timeout,
  0 crash (95.29%). Remaining callback failures are the Object.prototype
  getter edge in `filter`, 2 `find` edge cases, and broader `flatMap`
  proxy-flatten/new.target semantics.
- [~] **Stage 4** — `do_call_method_value` → GetMethod + Call with a
  call IC; receiver type-switch retires.
  - [x] Slow fallback bridge extracted as `get_method_value_for_call`;
    property-bearing receivers, class statics, functions, native
    functions, and primitive wrappers now share one getter-observing
    method lookup helper before the final `Call`.
  - [x] Nullish method calls reject before the intrinsic fallback:
    `(undefined).foo()` now reports `TypeError: Cannot read properties
    of undefined` instead of the internal `unknown intrinsic method`.
    Missing primitive / native-function methods likewise fall through to
    the shared non-callable TypeError path instead of `UnknownIntrinsic`.
  - [x] Promise expando methods now shadow Promise.prototype even when
    the own property is non-callable: `p.then = 1; p.then()` reports the
    shared non-callable TypeError instead of falling through to the
    builtin `then`.
  - [x] Array own data/accessor methods now shadow Array.prototype
    before the specialized Array builtin arms: `arr.map = 1; arr.map()`
    reports the shared non-callable TypeError instead of dispatching the
    builtin callback path.
  - [x] RegExp expando methods now shadow RegExp.prototype before the
    intrinsic table: `re.exec = 1; re.exec()` reports the shared
    non-callable TypeError instead of calling the builtin matcher.
  - [x] RegExp prototype methods now resolve through the
    `RegExp.prototype` property path before the intrinsic table, so
    non-callable prototype shadows report the shared non-callable
    TypeError.
  - [x] Non-mutating Date prototype methods now resolve through the
    shared `GetMethod` path before the intrinsic table, so non-callable
    prototype shadows report the shared non-callable TypeError.
  - [x] Date setter methods now probe `Date.prototype` before the
    intrinsic setter path, so non-callable prototype shadows report the
    shared non-callable TypeError while default native setters keep the
    existing captured-time coercion path.
  - [x] TypedArray expando methods now shadow `%TypedArray%.prototype`
    before callback/slice/subarray/intrinsic arms: `ta.map = 1; ta.map()`
    reports the shared non-callable TypeError.
  - [x] TypedArray callback prototype methods now resolve through the
    shared `GetMethod` path before the opcode-local callback dispatcher,
    so non-callable per-kind prototype shadows report the shared
    non-callable TypeError.
  - [x] TypedArray `slice` / `subarray` now probe the prototype path
    before their opcode-local species/coercion dispatchers, so
    non-callable per-kind prototype shadows report the shared
    non-callable TypeError while default natives keep the specialized
    path.
  - [x] Iterator helper and generator method calls now probe the
    prototype path before the opcode-local iterator dispatchers, so
    non-callable `%Iterator.prototype%` shadows report the shared
    non-callable TypeError while default natives keep the specialized
    path.
  - [x] Function and closure calls to inherited
    `Object.prototype.{hasOwnProperty,propertyIsEnumerable,isPrototypeOf}`
    now probe the function property path before the opcode-local
    object-method intercept, so own non-callable function shadows
    report the shared non-callable TypeError.
  - [x] Ordinary object calls to inherited `Object.prototype`
    methods now probe the real prototype path before the opcode-local
    object-method intercept, so null-prototype objects and deleted /
    non-callable prototype shadows report the shared non-callable
    TypeError.
  - [x] Native and bound function calls to inherited
    `Object.prototype` methods now probe the real prototype path before
    their opcode-local object-method intercepts, so non-callable
    prototype shadows report the shared non-callable TypeError.
  - [x] Primitive receiver calls to inherited `Object.prototype`
    methods now probe the wrapper prototype path before the opcode-local
    primitive-shape intercept, so non-callable prototype shadows report
    the shared non-callable TypeError.
  - [x] Map/Set `forEach` now resolve through prototype `GetMethod`
    instead of the opcode-local callback helper, so non-callable
    prototype shadows on Map and Set report the shared non-callable
    TypeError.
  - [x] ES Set methods (`union`, `intersection`, `difference`, and
    predicates) now resolve through prototype `GetMethod` before the
    re-entrant native body, so non-callable prototype shadows report the
    shared non-callable TypeError.
  - [x] Map and ordinary Set prototype methods now resolve through
    prototype `GetMethod` before invoking the native method body, so
    non-callable prototype shadows report the shared non-callable
    TypeError.
  - [x] WeakMap and WeakSet prototype methods now resolve through
    prototype `GetMethod` before invoking the native method body, so
    non-callable prototype shadows report the shared non-callable
    TypeError.
  - [x] ArrayBuffer and DataView prototype methods now resolve through
    prototype `GetMethod` before invoking the native method body, so
    non-callable prototype shadows report the shared non-callable
    TypeError.
  - [x] Callable receivers now let own/prototype properties shadow
    `Function.prototype.{call,apply,bind,toString}` before falling back
    to the canonical Function prototype dispatch.
  - [x] Promise prototype methods now resolve through the shared
    `GetMethod` path; expando and prototype shadows are observed before
    the native Promise method body runs.
  - [x] Primitive String method calls now resolve `String.prototype`
    through the shared `GetMethod` path before invoking the native
    method body, so prototype shadows are observed.
  - [x] Primitive Number method calls now resolve `Number.prototype`
    through the shared `GetMethod` path before invoking the native
    method body, so prototype shadows are observed.
  - [x] Primitive Boolean method calls now resolve `Boolean.prototype`
    through the shared `GetMethod` path before invoking the native
    method body, so prototype shadows are observed.
  - [x] Primitive BigInt method calls now resolve `BigInt.prototype`
    through the shared `GetMethod` path before invoking the native
    method body, so prototype shadows are observed.
  - [x] Primitive Symbol method calls now resolve `Symbol.prototype`
    through the shared `GetMethod` path before invoking the native
    method body, so prototype shadows are observed.
  - [x] WeakRef and FinalizationRegistry prototype methods now resolve
    through the shared `GetMethod` path before invoking the native
    method body, so prototype shadows are observed.
- [ ] **Stage 5** — collapse the 13 per-type `lookup(name)` tables into
  prototype-installed callables.
- [ ] **Follow-ups (not dispatch)**: `for await` IteratorClose; `return`
  / `throw` IteratorClose in non-generator frames (needs unwind
  integration with `active_iterator_closers`).

## Problem

Method-call dispatch and property/prototype resolution are spread across
many sites that each re-implement parts of the same ECMA-262 algorithm.
`Op::CallMethodValue` (`method_ops.rs::do_call_method_value`) is a single
~1100-line function that *resolves and executes* a method inline,
branching on the receiver's runtime type. Adding or fixing one method
means touching 2–3 unrelated locations, and the branches drift apart
(e.g. `arr.indexOf()` and `Array.prototype.indexOf.call(o)` historically
ran different code with different correctness).

### Current surface (inventory)

- **`do_call_method_value`** — 27 receiver-type branches: Promise, Map/Set
  `forEach`, Set methods, iterator helpers, generators, Array callbacks,
  Array `indexOf`/`includes`, TypedArray callbacks/`slice`/`subarray`,
  String `replace`, then a central `if recv.is_string()/is_array()/…`
  intrinsic-table switch, then post-table Object.prototype / function /
  primitive-wrapper intercepts, then a property-get fallback.
- **13 per-type intrinsic lookup tables** (`array_prototype::lookup`,
  `string::prototype::lookup`, `number::prototype_lookup`, …) returning
  `IntrinsicEntry` impls that take `IntrinsicArgs` and **cannot re-enter
  the interpreter** (no `ExecutionContext`).
- **2 callback re-entry mechanisms**: `run_callable_sync` (21 call sites,
  synchronous, no real frame) vs `invoke` (5 sites, pushes a VM frame).
- **6 `NativeCtx` construction sites** with inconsistent `context`
  threading — the reason some paths observe user getters / proxies and
  some do not.

The same method therefore exists in up to three forms: the
context-carrying native (`native_array_method` → driver), the
context-free intrinsic-table impl (`impl_index_of`), and an inline
type-switch arm.

### Cross-type spec gaps (measured)

The scatter is not array-specific. Probing generic-receiver and
observability invariants across builtin types shows the same class of
deviation everywhere the context-free intrinsic table runs:

| Probe | Spec | Actual | Root |
|---|---|---|---|
| `String.prototype.slice.call({toString:()=>"hello"},1,3)` | `"el"` | `"ob"` | receiver not run through `ToString`; the wrapper object is stringified (`"[object Object]"`) |
| `Array.prototype.join.call({0,1,get length})` | `"a-b"` | `""` | array-like `length` getter not observed; generic receiver dropped |
| `/\d+/.exec({toString:()=>"x42y"})` | `"42"` | *throws* | `RegExp.prototype.exec` does not `ToString` its argument |
| `Array.prototype.indexOf.call(new Uint8Array([5,6,7]),6)` | `1` | `-1` | TypedArray receiver invisible to generic Array methods (`[[HasProperty]]`/`[[Get]]` on integer-indexed exotics) |
| `(undefined).foo()` | TypeError *"Cannot read properties of undefined"* | **fixed** | nullish method calls now reject before intrinsic fallback |

Each row is the same failure mode arrays already had: a builtin runs in a
context-free intrinsic path that cannot re-enter user code, so
`ToObject` / `ToString` / `LengthOfArrayLike` / accessor observation is
either skipped or bolted on per-method. Fixing them one method at a time
reproduces the scatter. The unification below fixes the class.

### Extended audit (other receiver types — mostly conformant)

Probing the rest of the surface, these paths already behave per spec, so
the scatter's *correctness* damage is concentrated, not uniform:

- **PASS**: `Object.keys` integer-then-insertion order; `Object.assign`
  source-getter + target-setter sequencing; `Reflect.get` with a
  `receiver` accessor `this`; `Reflect.ownKeys` (string + symbol);
  `Proxy` `get`/`has` traps and the non-writable/non-configurable `get`
  invariant; `Date.prototype.getTime` brand TypeError and
  `toISOString` RangeError on an invalid Date; `Promise.prototype.then`
  ordering; `Set.prototype.union` via `GetSetRecord`; `ToPropertyKey`
  coercing a computed key exactly once; `Array.from` with a map fn.
- **FAIL — separate spec bug (not dispatch scatter)**: `for-of` with an
  early `break` does not invoke the iterator's `return()`
  (§14.7.5.6 IteratorClose). Tracked independently of this refactor.

Conclusion: the dispatch unification targets the *array-like generic +
receiver/argument coercion* cluster (String / RegExp / Array generics)
and the call-on-missing-method error class. The proxy / reflect / date /
promise machinery is correct and should be preserved as-is.

## Target model (ECMA-262 §7.3.11 GetMethod + §7.3.14 Call)

`obj.m(args)` should lower to the spec's two steps:

1. `func = ? GetMethod(obj, "m")` — ordinary `[[Get]]` walking the
   prototype chain (already implemented once in `ordinary_get_value` with
   proxy/accessor/string-exotic support).
2. `? Call(func, obj, args)` — one uniform `[[Call]]` that dispatches on
   the *callable's* kind (native / closure / bound), **not** on the
   receiver's kind.

Builtins are plain callables with a single signature
`fn(this, args, &mut NativeCtx /* always carries context */) -> Result`.
Each performs its own `ToObject(this)` + `LengthOfArrayLike` and may take
an internal fast path (dense array, no accessors, prototype unmodified)
as an opt-in invariant check — never as a separate dispatch site.

This removes the receiver type-switch from the call opcode, collapses the
13 tables into ordinary prototype properties, and leaves exactly one
callback re-entry path.

## Why it's currently split (constraints to preserve)

- **Performance**: the inline type-switch avoids a property `[[Get]]` +
  callable check on hot `arr.push()` / `str.slice()`. Replacement must
  keep an inline-cache fast path so the common monomorphic call stays
  allocation-free (see `PERFORMANCE_PLAN.md` Phase 2.4 Call IC).
- **`IntrinsicArgs` has no interpreter handle**, so context-sensitive
  steps (user `valueOf`, species, getters) were bolted on as separate
  context-carrying interceptions. Unification requires every builtin to
  receive a re-entrant handle.
- **Two callback mechanisms**: `invoke` (frame push) vs
  `run_callable_sync`. Pick `run_callable_sync` as the single path unless
  a measured stack-depth/perf reason survives.

## Staged migration (each stage independently green + measured)

1. **Collapse the search trio** (done for behavior, not yet structure):
   `indexOf`/`lastIndexOf`/`includes` already share one driver
   (`array_linear_search` / `array_includes`). Remove the now-dead
   array arm of `impl_index_of`/`impl_last_index_of`/`impl_includes` and
   route both call sites through one `Interpreter` entry helper. Net
   structural cleanup, no behavior change.
2. **Give every builtin a re-entrant handle**: change `IntrinsicArgs` (or
   replace it) so impls can call `run_callable_sync` / abstract ops.
   Fold the context-carrying interceptions back into the impls.
3. **Single callback path**: migrate the `invoke`-based array/typed-array
   callback dispatch to `run_callable_sync`; delete the duplicate.
4. **GetMethod→Call lowering**: make `do_call_method_value` resolve via
   `ordinary_get_value` + a uniform `Call`, guarded by a call IC for the
   monomorphic builtin fast path. Receiver-type branches become builtin
   internals or disappear.
5. **Collapse the 13 lookup tables** into prototype-installed callables;
   `lookup(name)` tables retire as the IC + ordinary `[[Get]]` subsume
   them.

Conformance (`built-ins/*`) is the regression gate at every stage; no
stage lands with new crash/timeout/OOM or a net pass-rate drop.
