# Tasks 41–64 — ECMA-262 spec-gap audit

This is the master tracker created after walking ECMA-262 section
by section against `crates-next/*` once Phase G shipped. The intent
is **maximal ECMA-262 surface coverage on the existing `Rc` value
model** — every gap below is something a real-world npm package
will hit. GC and JIT are explicitly deferred to their own dedicated
architectural tracks once spec coverage is complete.

Tasks below are filed under their dedicated files (one task per
file once the planning batch is sliced into individual task
files). Some entries land as expansions of the existing
infrastructure tasks; others are new.

## §7 — Type-conversion abstract operations

| # | Task | Why |
|---|------|-----|
| ~~41~~ | ~~ToPrimitive ladder + Symbol.toPrimitive + valueOf + toString~~ | **Shipped.** `Op::ToPrimitive` + multi-stage dispatch ladder driving `[Symbol.toPrimitive]` / `valueOf` / `toString`; compiler emits `ToPrimitive(default)` before `Op::Add`; `run_add` widens to §13.15.4 string-or-numeric. Comparison + template-literal interpolation deferred to #42 / #68. |
| ~~42~~ | ~~IsLooselyEqual (`==`/`!=`) + AbstractRelationalComparison~~ | **Shipped.** `Op::LooseEqual` / `Op::LooseNotEqual` + compiler maps `==`/`!=`; relational ops emit `ToPrimitive(number)` pre-coercion; runtime helpers `is_loosely_equal` / `abstract_relational_comparison` in `abstract_ops.rs`. |
| ~~43~~ | ~~SameValue / SameValueZero / IsArray / IsCallable / IsConstructor as canonical helpers~~ | **Shipped.** `crates-next/otter-vm/src/abstract_ops.rs` + `Op::SameValue` / `Op::IsArray` + `Object.is` / `Array.isArray` lowering. |

## §9 — Environments + property descriptors

| # | Task | Why |
|---|------|-----|
| ~~44~~ | ~~Property descriptors (writable / enumerable / configurable) + accessor pairs~~ | **Shipped.** `crates-next/otter-vm/src/object.rs` rebuilt around `PropertyFlags` + `PropertyDescriptor` + `PropertyLookup` + `SetOutcome`. `Op::LoadProperty` / `Op::StoreProperty` route through the §10.1.8 / §10.1.9 ladders (accessor getters / setters dispatched as call frames). New `Op::ObjectCall` opcode + `crates-next/otter-vm/src/object_statics.rs` lower `Object.defineProperty` / `defineProperties` / `getOwnPropertyDescriptor` / `getOwnPropertyDescriptors` / `freeze` / `seal` / `preventExtensions` / `isFrozen` / `isSealed` / `isExtensible`. `delete` now respects `[[Configurable]]`. JSON.stringify walks `enumerable_data_iter` so non-enumerable props are skipped. |
| ~~45~~ | ~~`var` hoisting + function-scope semantics audit~~ | **Shipped.** New `hoist_var_names` walker (per §8.1.6 VarScopedDeclarations) collects every `var`-declared name across blocks / `if` / loops / `switch` / `try` / labels without crossing function or class boundaries. `pre_declare_var_bindings` runs before any statement compile in `<main>` (§16.1.7), the module-init function (§16.2.1.7), and every nested function (§10.2.11 step 28), pre-binding each name to `undefined` with no TDZ. The `var` arm in `compile_statement` plus the `for(var ...; ;)` / `for-in` / `for-of` / `export var` heads now reuse the hoisted binding instead of rejecting. Function-declaration hoisting (separate from var-hoisting) is filed as a follow-up since it also requires the §10.2.11 functions-list pass. |

## §13 — Expressions

| # | Task | Why |
|---|------|-----|
| ~~46~~ | ~~`in` operator (string + symbol membership) + private-name `#x in obj` check~~ | **Shipped (string + symbol).** New `Op::HasProperty` opcode lowered from `BinaryOperator::In`; runtime walks own + proto chain via `JsObject::lookup`, honours symbol keys via `get_symbol`, and treats Array indexed `in` as length-bounded per §10.4.2. Private-name `#x in obj` is filed alongside the broader private-fields work in #53. |
| ~~47~~ | ~~Tagged template literals + `String.raw`~~ | **Shipped.** `compile_template_literal` lowers interpolated quasis as `Op::Add` chains over `ToPrimitive(default)` operands; `compile_tagged_template` builds the cooked + raw string arrays, attaches `strings.raw`, and invokes the tag with `(strings, ...exprs)`. `String.raw\`...\`` recognised at compile time and inlined as a raw-text concat (no Standard `String` namespace install needed). `JsArray` gained an optional `named_properties` bag (lazily allocated) so `arr.foo = bar` and `strings.raw` route through `Op::StoreProperty`/`Op::LoadProperty` per §10.4.2. |
| ~~48~~ | ~~Optional chaining + nullish coalescing precedence audit~~ | **Shipped.** New `compile_chain_expression` lowers `Expression::ChainExpression`, walking each `?.` step, emitting `Op::JumpIfNullish` for short-circuit, and joining at an `Op::LoadUndefined` writer. Covers static / computed / call positions in arbitrary mixes. Nullish coalescing (`??`) was already in via `LogicalOperator::Coalesce` lowering — the precedence audit confirmed correct grouping under the existing OXC parse tree. |
| ~~49~~ | ~~`delete` audit (strict-mode reject, non-configurable, computed)~~ | **Shipped (foundation surface).** Member + computed-element delete already worked; added §13.5.1.2 — `delete` on a non-Reference returns `true`; §13.5.2 `void` operator; §10.1.10 OrdinaryDelete now returns `true` for missing properties (was `false`). Strict-mode TypeError for non-configurable rejection is filed against task 25 alongside the broader implicit-error → throwable conversion. |
| ~~50~~ | ~~Comma operator + conditional precedence audit~~ | **Shipped.** `Expression::SequenceExpression` now lowers `(a, b, c)` per §13.16 (evaluate each, return last). Conditional (`?:`) and short-circuit (`&&` / `\|\|` / `??`) already grouped correctly under OXC's precedence — verified by walking the existing `nullish-coalescing` + `logical-ops` fixtures. |

## §14 — Statements

| # | Task | Why |
|---|------|-----|
| ~~51~~ | ~~`switch`, `for-in`, labelled break/continue~~ | **Shipped.** `compile_switch_statement` lowers §14.12 SwitchStatement (strict-equality dispatch, fall-through, default clause); `compile_for_in_statement` lowers §14.7.5 ForInStatement (foundation snapshots own enumerable keys via `Object.keys` + iterator protocol — full chain enumeration filed); `compile_labeled_statement` + `LoopFrame::label` thread `break label;` / `continue label;` per §14.13. Switch frames also collect `break` patches (§13.10.1 — `continue` skips switch and targets the enclosing loop). |
| ~~52~~ | ~~`with` rejected + strict-mode enforcement audit~~ | **Shipped.** `Statement::WithStatement` now rejects with an explicit "forbidden in strict mode / ES modules (§14.13)" diagnostic instead of the generic "unsupported" arm. The foundation is always strict (no source-level "use strict" pragma needed) so `with` is unconditionally illegal — verified by the `with-rejected` fixture. |

## §15 — Functions / classes

| # | Task | Why |
|---|------|-----|
| 53 | Class fields (instance + static + private) + static blocks | Spec §15.7. |

## §16 — Modules

| # | Task | Why |
|---|------|-----|
| 54 | `import.meta.resolve` + non-literal `import()` (extends task 58) | Stage 4. |
| 55 | JSON modules (`import x from "./y.json" with { type: "json" }`) | Stage 4 import-attributes. |
| 56 | Top-level await | Spec §16.2.1.7. |

## §19 — Globals / language built-ins

| # | Task | Why |
|---|------|-----|
| ~~57~~ | ~~Error hierarchy (TypeError / RangeError / SyntaxError / ReferenceError / URIError / EvalError)~~ | **Shipped (user surface).** `error_classes::ErrorClassRegistry` per Interpreter, `Op::NewBuiltinError` / `Op::LoadBuiltinError`, compiler intercepts `new <Kind>(msg)` and bare-identifier reads of all 7 names; `Op::Instanceof` widened to read `rhs.prototype` per §13.10.2. Internal-error → throwable conversion deferred — see [25-internal-error-throwability.md](./25-internal-error-throwability.md). |
| 58 | `globalThis` + parseInt / parseFloat / isNaN / isFinite / encodeURI* | Spec §19.2 / §19.3. |
| 59 | `eval` + `new Function(...)` (sandboxed, capability-gated) | Spec §19.4. |

## §20 — Object / Function

| # | Task | Why |
|---|------|-----|
| ~~60~~ | ~~`Object` static surface (keys/values/entries/assign/freeze/…)~~ | **Shipped.** Lowered through `Op::ObjectCall` + `crates-next/otter-vm/src/object_statics.rs`: `keys` / `values` / `entries` / `assign` / `fromEntries` / `hasOwn` / `getOwnPropertyNames`. `defineProperty / freeze / seal / preventExtensions / is*` already shipped under task 44. |
| ~~61~~ | ~~`Object.prototype` methods (hasOwnProperty/toString/valueOf/isPrototypeOf/propertyIsEnumerable)~~ | **Shipped (foundation surface).** `object_prototype_intercept` in `crates-next/otter-vm/src/lib.rs` handles `hasOwnProperty / propertyIsEnumerable / isPrototypeOf / toString / valueOf` against ordinary `JsObject` receivers when the user hasn't shadowed the name. Real `Object.prototype` installation (with `[[Prototype]]` chain auto-link from object literals) is a follow-up under the prototype-tree work. |
| 62 | `Function.prototype` completion (.name, .length, .toString, [Symbol.hasInstance]) | Spec §20.3. |

## §21 — Number / BigInt / Math / Date / Boolean

| # | Task | Why |
|---|------|-----|
| 63 | Number completion (statics + prototype: toExponential, toPrecision, isFinite, isInteger, MAX_SAFE_INTEGER, …) | Spec §21.1. |
| 64 | BigInt completion (constructor + asIntN/asUintN + toString radix) | Spec §21.2. |
| 65 | Math completion (log/exp/sin/cos/atan2/random/cbrt/hypot/sign/clz32/imul + constants) | Spec §21.3. |
| 66 | `Date` object (full §21.4 surface) | Independent of Temporal; many libraries assume it. |
| 67 | Boolean constructor + Boolean.prototype | Spec §21.5. |

## §22 — Strings / RegExp

| # | Task | Why |
|---|------|-----|
| 68 | String constructor + missing prototype (localeCompare/normalize/fromCharCode/fromCodePoint/raw) | Spec §22.1. |
| 69 | RegExp completion (flags/source accessors, named groups, [Symbol.match] hooks) | Spec §22.2. |
| 70 | RegExp engine: lookbehind / `\p{...}` / `v` flag set notation | Verify regress backend. |
| 71 | String iterator: code-point semantics (surrogate-pair combine) | Spec §22.1.5. |
| 72 | `IsRegExp` + Symbol.match/replace/search/split consultation in String methods | Spec §22.1.3.13 et al. |

## §23 — Arrays + TypedArrays

| # | Task | Why |
|---|------|-----|
| ~~73~~ | ~~Array completion (every/some/find/forEach/map/filter/reduce/sort/splice/at/keys/values/entries/from/of/isArray)~~ | **Shipped (foundation surface).** Pure-functional methods (`at`, `lastIndexOf`, `reverse`, `fill`, `flat`, `splice`, default `sort`) extend `array_prototype.rs`'s intrinsic table. Callback-driven methods (`forEach`, `map`, `filter`, `reduce`, `reduceRight`, `find`, `findIndex`, `every`, `some`, `flatMap`, comparator-driven `sort`) intercept in `do_call_method_value` and run synchronously via `run_callable_sync`. New `Op::ArrayCall` opcode + `array_statics.rs` lower `Array.from` (array / Set / Map / String iterables) and `Array.of`. `keys`/`values`/`entries` returning live iterators is filed as a follow-up. |
| 74 | TypedArray family + ArrayBuffer + DataView | Spec §23.2 / §25.1 / §25.3. Required for crypto / fetch / wasm interop. |

## §24 — Keyed collections + Atomics

| # | Task | Why |
|---|------|-----|
| 76 | Atomics + SharedArrayBuffer (single-thread subset) | Spec §24.4 / §25.1. |

> §24.5 WeakRef + §24.6 FinalizationRegistry are intentionally
> **not** in this batch — both depend on tracing GC and ship in
> the dedicated GC track once spec coverage is otherwise done.

## §25 — Promises / iterators

| # | Task | Why |
|---|------|-----|
| 77 | Promise completion (allSettled, any, withResolvers, finally, Symbol.species) | Spec §25.4. |
| 78 | Iterator helpers (Stage 4: map / filter / take / drop / flatMap / reduce / toArray / forEach) | Spec §7.4 + iterator-helpers proposal. |
| ~~79~~ | ~~Iterator-protocol consultation in for-of / spread / destructuring~~ | **Shipped.** `IteratorState::User`, multi-stage `Op::GetIterator` and `Op::IteratorNext` ladders that call `[Symbol.iterator]()` and `iter.next()` on user objects, unpack `{ value, done }`. for-of and array spread now traverse user iterables. |

## §27 — Generators / async generators

| # | Task | Why |
|---|------|-----|
| 80 | Generators (`function*` + `yield`) + async generators + `for await … of` | Spec §27.5–6. Resumable frames + AsyncIterator. |

## §28 — Reflect + Proxy

| # | Task | Why |
|---|------|-----|
| 81 | Reflect (full surface) + Proxy (all 13 traps) | Spec §28. Invasive: every property load / store / call goes through. |

## ECMA-402 — Internationalisation

| # | Task | Why |
|---|------|-----|
| 82 | Intl.PluralRules / RelativeTimeFormat / ListFormat / DisplayNames / Segmenter | Spec ECMA-402 §13–17. |

## Sequencing notes

The tasks are not strictly ordered by section number — implementation
order should respect dependencies:

1. **Foundations (do first):**
   - ~~§7.4 Iterator-protocol consultation (#79)~~ ✅
   - ~~§7.2.10 SameValue / SameValueZero canonical helpers (#43).~~ ✅
   - ~~§7.1 ToPrimitive ladder (#41)~~ ✅ + ~~IsLooselyEqual (#42)~~ ✅ — many
     downstream tasks call into these.

2. ~~**Property descriptors (#44):** Blocks Object.freeze / defineProperty
   / Reflect / Proxy / TypedArrays. Land before §20.2 / §28.~~ ✅

3. **Error hierarchy (#57):** Blocks every other built-in that's
   supposed to throw a typed error rather than the generic
   `Uncaught` shape.

4. **`globalThis` plumbing (#58):** Blocks the global functions
   that real-world code reaches without intermediate identifiers.

5. ~~**Object.prototype (#61) + Object statics (#60):** Unblocks
   `obj.hasOwnProperty(...)` patterns.~~ ✅

6. ~~**Array completion (#73):** Highest user-impact missing surface.~~ ✅

7. **TypedArrays (#74) + Atomics (#76):** Crypto / fetch interop.

8. ~~**Switch / for-in / labels (#51):** Fixes the most-frequently-hit
   compiler-rejection.~~ ✅

9. **Generators (#80) + Reflect/Proxy (#81):** Largest scope; ship
   after the bulk of the smaller tasks.

10. **ECMA-402 expansion (#82):** Lowest priority — most apps tolerate
    missing PluralRules / Segmenter.

After this batch lands, the runtime is feature-complete on the
`Rc` value model and the GC + JIT architecture tracks open as
separate, dedicated efforts.

## Status

- Tracker created. Individual task files split out as work begins.
