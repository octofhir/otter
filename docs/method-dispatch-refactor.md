# Method & Prototype Resolution — Unification Plan

## Status / execution order

Work proceeds strictly in this order; each item lands green and
conformance-gated before the next starts.

- [x] **Audit** — cross-type spec probe of method/prototype resolution
  (this doc's two audit tables).
- [x] **Side spec-bug: for-of `break` IteratorClose** (§14.7.5.6) —
  fixed in `compile_for_of_statement` (commit `f4a30331`). Separate from
  the dispatch scatter; `for await` IteratorClose still pending.
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
  - [ ] `Array.join` / other array-likes generic `length`-getter
    (needs the IntrinsicArgs→re-entrant migration so `impl_join` reads
    `length` through the interpreter).
  - [ ] Mechanical `IntrinsicArgs` → re-entrant-context signature
    migration, per type (String 46 fns, …).
- [ ] **Stage 3** — single callback re-entry path (`invoke` →
  `run_callable_sync`).
- [ ] **Stage 4** — `do_call_method_value` → GetMethod + Call with a
  call IC; receiver type-switch retires.
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
| `(undefined).foo()` | TypeError *"Cannot read properties of undefined"* | TypeError *"unknown intrinsic method"* | call on a missing/undefined method reports an internal error class/message |

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
