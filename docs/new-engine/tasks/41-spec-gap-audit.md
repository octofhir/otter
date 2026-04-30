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
| ~~45~~ | ~~`var` hoisting + function-scope semantics audit~~ | **Shipped.** Three-stage entry-point pre-pass per §10.2.11 FunctionDeclarationInstantiation: (1) `hoist_var_names` collects var names through blocks / `if` / loops / `switch` / `try` / labels and `pre_declare_var_bindings` binds them to `undefined`; (2) `hoist_lexical_names` + `pre_declare_lexical_bindings` pre-declare top-level `let` / `const` / `class` (TDZ) so hoisted nested functions can capture forward references; (3) `hoist_function_declarations` runs in three passes — last-wins index, pre-declare every surviving name, then compile bodies and store closures, so mutual-recursion declarations bind correctly. The `var` arm + for-init / for-in / for-of / export-var heads + class-decl arm + function-decl arm all reuse pre-hoisted bindings. `declare function` is skipped (TypeScript erasure preserved). |

## §13 — Expressions

| # | Task | Why |
|---|------|-----|
| ~~46~~ | ~~`in` operator (string + symbol membership) + private-name `#x in obj` check~~ | **Shipped.** New `Op::HasProperty` opcode lowered from `BinaryOperator::In`; runtime walks own + proto chain, honours symbol keys, treats Array indexed `in` as length-bounded per §10.4.2. `Expression::PrivateInExpression` lowers `#name in obj` against the current class's private namespace (mangled key + `Op::HasProperty`). |
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
| ~~53~~ | ~~Class fields (instance + static + private) + static blocks~~ | **Shipped (full).** `compile_class` accepts `PropertyDefinition` (instance + static, public + private) and `StaticBlock`. Instance fields run inside `compile_class_constructor` per §15.7.10 — base classes inject at body start, derived classes detect the user's top-level `super(...)` call and inject right after it (or at body end if there is no super-call). Static fields + `static { … }` blocks run in source order against the statics object after methods install; `compile_static_block` synthesises a parameterless function called via `Op::CallWithThis`. The class binding is pre-stored to the statics object before static elements run so `static x = C.something` resolves (§10.2.1.4 step 24). **Private fields + private methods + private-name `#x in obj`** lower through a per-class namespace counter (`ModuleBuilder::next_private_namespace` + `Compiler::private_namespaces` stack) that mangles `#name` into `__priv_<n>_<name>` so distinct lexical class declarations get distinct private slots; access from outside the class body fails the lookup. Capture analysis treats field initialisers + static blocks as nested so outer-scope reads upgrade to upvalues. |

## §16 — Modules

| # | Task | Why |
|---|------|-----|
| ~~54~~ | ~~`import.meta.resolve` + non-literal `import()`~~ | **Shipped.** New `Op::ImportNamespaceDynamic` lowers non-literal `import(spec)` — runtime reads the specifier string and resolves through the linker's per-referrer table; missing edges raise `TypeError`. New `Op::ImportMetaResolve` + compile-time interception lowers `import.meta.resolve(spec)` to a sync URL join against the active frame's `module_url` (relative `./foo`, `../bar`, `/abs`, and absolute scheme passthrough). |
| ~~55~~ | ~~JSON modules (`import x from "./y.json" with { type: "json" }`)~~ | **Shipped.** `ModuleLoader::load` recognises `.json` extension, wraps raw text as `export default (<json>);` so the parsed value lands as the module's default export. `.json` added to `FOUNDATION_EXTENSIONS` for resolver lookup. Import-attributes syntax is parsed by OXC and ignored — the JSON wrapping happens unconditionally on the extension. |
| ~~56~~ | ~~Top-level await~~ | **Shipped (script entry).** `module_body_uses_top_level_await` walks the program scanning for `await` outside any function / class body; `<main>` and `<module-init>` upgrade to `is_async = true` when found. `run_inner` allocates an `AsyncFrameState` on the entry frame, drains microtasks after the dispatch returns, and unwraps the result promise's settlement to surface as the program's completion value. |

## §19 — Globals / language built-ins

| # | Task | Why |
|---|------|-----|
| ~~57~~ | ~~Error hierarchy (TypeError / RangeError / SyntaxError / ReferenceError / URIError / EvalError)~~ | **Shipped (user surface).** `error_classes::ErrorClassRegistry` per Interpreter, `Op::NewBuiltinError` / `Op::LoadBuiltinError`, compiler intercepts `new <Kind>(msg)` and bare-identifier reads of all 7 names; `Op::Instanceof` widened to read `rhs.prototype` per §13.10.2. Internal-error → throwable conversion deferred — see [25-internal-error-throwability.md](./25-internal-error-throwability.md). |
| ~~58~~ | ~~`globalThis` + parseInt / parseFloat / isNaN / isFinite / encodeURI*~~ | **Shipped.** New `Op::GlobalCall` lowers `parseInt` / `parseFloat` / `isNaN` / `isFinite` / `encodeURI` / `encodeURIComponent` / `decodeURI` / `decodeURIComponent` against `crates-next/otter-vm/src/global_functions.rs` (a single dispatcher used both by bare-identifier calls and by `Number.<name>` static lowering — `Number.parseInt` / `Number.parseFloat` alias the global form, `Number.isNaN` / `Number.isFinite` / `Number.isInteger` / `Number.isSafeInteger` route to the same module with strict-no-coerce semantics). New `Op::LoadGlobalThis` returns the per-Interpreter shared `globalThis` JsObject (seeded with the `globalThis` self-reference); user code can stash properties on it. Local shadowing wins — every interception consults `lookup_binding` first. |
| ~~59~~ | ~~`eval` + `new Function(...)` (sandboxed, capability-gated)~~ | **Shipped.** New `Op::Eval` + `Op::NewFunction` opcodes; compiler intercepts bare-identifier `eval(x)` (indirect-eval semantics — fresh global scope) and `new Function(args, body)` / `Function(args, body)`. Runtime exposes an `EvalHook` (`Rc<dyn Fn(&str) -> Result<BytecodeModule, String>>`); the runtime layer wires the otter-compiler's `compile` + `parse` into it at `Runtime::build`. `Op::Eval` recursively dispatches the inner module on a sub-stack; `Op::NewFunction` returns a `NativeFunction` that holds the inner `Rc<BytecodeModule>` and replays calls against it via `Interpreter::invoke_eval_function`. Embedders that want to ban dynamic code call `set_eval_hook(None)` — both opcodes then surface a `SyntaxError`. |

## §20 — Object / Function

| # | Task | Why |
|---|------|-----|
| ~~60~~ | ~~`Object` static surface (keys/values/entries/assign/freeze/…)~~ | **Shipped.** Lowered through `Op::ObjectCall` + `crates-next/otter-vm/src/object_statics.rs`: `keys` / `values` / `entries` / `assign` / `fromEntries` / `hasOwn` / `getOwnPropertyNames`. `defineProperty / freeze / seal / preventExtensions / is*` already shipped under task 44. |
| ~~61~~ | ~~`Object.prototype` methods (hasOwnProperty/toString/valueOf/isPrototypeOf/propertyIsEnumerable)~~ | **Shipped (foundation surface).** `object_prototype_intercept` in `crates-next/otter-vm/src/lib.rs` handles `hasOwnProperty / propertyIsEnumerable / isPrototypeOf / toString / valueOf` against ordinary `JsObject` receivers when the user hasn't shadowed the name. Real `Object.prototype` installation (with `[[Prototype]]` chain auto-link from object literals) is a follow-up under the prototype-tree work. |
| ~~62~~ | ~~`Function.prototype` completion (.name, .length, .toString, [Symbol.hasInstance])~~ | **Shipped.** `Op::LoadProperty` resolves `.name` / `.length` against every callable shape via `function_intrinsic_property` / `bound_function_intrinsic_property` (Function / Closure / NativeFunction / BoundFunction / ClassConstructor). `f.toString()` intercepts in `do_call_method_value` (callable receivers route to `Function.prototype.toString` ahead of the property-lookup probe so ClassConstructor wins), `function_to_string` builds the canonical `function <name>() { [native code] }` placeholder. `[Symbol.hasInstance]` keeps the §13.10.2 OrdinaryHasInstance default (already in via `Op::Instanceof`). Bonus: `Error.prototype.toString` (§20.5.3.4) gets a single shared implementation in `error_classes::render_error_to_string` — both `e.toString()` (via `object_prototype_intercept`) and the unwind diagnostic (`render_thrown_value`) share it, so uncaught throws print `<Name>: <message>` instead of `[object Object]`. |

## §21 — Number / BigInt / Math / Date / Boolean

| # | Task | Why |
|---|------|-----|
| ~~63~~ | ~~Number completion (statics + prototype: toExponential, toPrecision, isFinite, isInteger, MAX_SAFE_INTEGER, …)~~ | **Shipped.** Static constants (`MAX_SAFE_INTEGER`, `MIN_SAFE_INTEGER`, `MAX_VALUE`, `MIN_VALUE`, `EPSILON`, `POSITIVE_INFINITY`, `NEGATIVE_INFINITY`, `NaN`) inline at compile time when the user reads `Number.<NAME>` outside a local shadow (`number_static_constant` table → `Op::LoadNumber`). Prototype gains `toExponential` (§21.1.3.3), `toPrecision` (§21.1.3.5), `valueOf` (§21.1.3.7) added to `NUMBER_PROTOTYPE_TABLE`. Exponential output normalised through `normalise_exp` so `1500 .toExponential(2) === "1.50e+3"` matches spec sign formatting. NaN / ±Infinity edge cases handled per §21.1.3.3 / §21.1.3.5. `Number.isNaN / isFinite / isInteger / isSafeInteger / parseInt / parseFloat` were already routed through `crate::number::parse` under #58. |
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

4. ~~**`globalThis` plumbing (#58):** Blocks the global functions
   that real-world code reaches without intermediate identifiers.~~ ✅

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
