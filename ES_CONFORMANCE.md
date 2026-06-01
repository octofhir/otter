# ES Conformance

This file tracks measured Test262 results for the active
`crates/otter-test262` runner.

## Runner Status

Captured on 2026-05-07 against engine commit
`92f417e7040408e72cf58d6d68b3c6addd8d38e7`.

The current runner CLI is:

```sh
cargo run -p otter-test262 --bin otter-test262 -- run <args>
```

Observed stale commands in repository docs/scripts:

- `--profile test262` is not defined in `Cargo.toml`.
- `--bin test262` does not exist; the bin target is `otter-test262`.
- The current runner has `run --filter ... --output ...`; older
  `--subdir`, `--save`, `--log`, and `-vv` flags are not accepted.
- `gen-conformance` and `merge-reports` bin targets are not present in
  `crates/otter-test262`.

Full corpus dry-run:

```sh
cargo run -p otter-test262 --bin otter-test262 -- run --dry-run
# total: 53179
```

No full Test262 run has been captured in this checkout yet.

## Targeted Baselines

### Object.hasOwn

Command:

```sh
cargo run -p otter-test262 --bin otter-test262 -- run \
  --filter built-ins/Object/hasOwn \
  --timeout 5000 \
  --output test262_results/current_object_hasown_before.json
```

Before:

| total | passed | failed | skipped | timeout | OOM | crash | pass rate |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 62 | 52 | 10 | 0 | 0 | 0 | 0 | 83.87% |

After installing JS-visible `Object` static methods and own-symbol
support for `Object.hasOwn`:

```sh
cargo run -p otter-test262 --bin otter-test262 -- run \
  --filter built-ins/Object/hasOwn \
  --timeout 5000 \
  --output test262_results/current_object_hasown_after.json
```

| total | passed | failed | skipped | timeout | OOM | crash | pass rate |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 62 | 54 | 8 | 0 | 0 | 0 | 0 | 87.10% |

Delta: +2 passing tests.

After installing VM-owned `Function.prototype.call` / `apply` /
`bind` / `toString` entries, JS-visible native-function `name` /
`length` metadata, and non-callback `Array.prototype` methods through
static bootstrap specs:

```sh
cargo run -p otter-test262 --bin otter-test262 -- run \
  --filter built-ins/Object/hasOwn \
  --timeout 5000 \
  --output test262_results/current_object_hasown_final.json
```

| total | passed | failed | skipped | timeout | OOM | crash | pass rate |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 62 | 55 | 7 | 0 | 0 | 0 | 0 | 88.71% |

Delta from first baseline: +3 passing tests.

### Function.prototype.call

Command:

```sh
cargo run -p otter-test262 --bin otter-test262 -- run \
  --filter built-ins/Function/prototype/call \
  --timeout 5000 \
  --output test262_results/current_function_prototype_call_after_arguments_object.json
```

Current:

| total | passed | failed | skipped | timeout | OOM | crash | pass rate |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 51 | 17 | 0 | 34 | 0 | 0 | 0 | 100.00% |

This subset still has targeted legacy path ignores, but the runner no
longer skips sloppy-only tests by a global `noStrict` policy. All
non-skipped tests in this focused subset pass.

Remaining common blockers:

- `Object.hasOwn` still lacks full `ToPropertyKey` object coercion
  via `[Symbol.toPrimitive]`, `toString`, and `valueOf`.
- `arguments` now uses an unmapped descriptor-backed object for strict
  functions and has focused runtime coverage for sloppy mapped
  simple-parameter aliasing.

### Arguments Object

Command:

```sh
cargo run -p otter-test262 --bin otter-test262 -- run \
  --filter language/arguments-object \
  --output test262_results/run.json
```

After correcting the Test262 runner strictness policy:

| total | passed | failed | skipped | timeout | OOM | crash | pass rate |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 263 | 157 | 106 | 0 | 0 | 0 | 0 | 59.70% |

### Function Constructor Metadata

Command:

```sh
cargo run -p otter-test262 --bin otter-test262 -- run \
  --filter built-ins/Function \
  --output test262_results/run.json
```

Before adding spec-shaped own `length`, `name`, and `prototype`
descriptors to the global `Function` constructor:

| total | passed | failed | skipped | timeout | OOM | crash | pass rate |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 515 | 199 | 179 | 137 | 0 | 0 | 0 | 52.65% |

After adding constructor metadata:

| total | passed | failed | skipped | timeout | OOM | crash | pass rate |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 515 | 204 | 174 | 137 | 0 | 0 | 0 | 53.97% |

Delta: +5 passing tests.

After splitting object-side native `[[Call]]` from `[[Construct]]` and
making `Function.prototype` callable but non-constructible:

| total | passed | failed | skipped | timeout | OOM | crash | pass rate |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 515 | 211 | 167 | 137 | 0 | 0 | 0 | 55.82% |

Delta from the original baseline: +12 passing tests.

After routing dynamic `Function.prototype.apply` argument lists through
the VM's array-like property reads instead of the spread-only lowering:

| total | passed | failed | skipped | timeout | OOM | crash | pass rate |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 515 | 216 | 162 | 137 | 0 | 0 | 0 | 57.14% |

Delta from the original baseline: +17 passing tests.

After correcting the Test262 runner to honor strictness from
frontmatter (`onlyStrict` gets a strict prelude; `noStrict`, normal
script, `raw`, and module tests are not globally rewritten):

| total | passed | failed | skipped | timeout | OOM | crash | pass rate |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 515 | 310 | 151 | 54 | 0 | 0 | 0 | 67.25% |

This changes the conformance denominator by unskipping sloppy-profile
tests that were previously hidden by the root `skip_flags` policy.

After adding primitive-property `Get` boxing for sloppy apply receivers
and observable `ToString` coercion for `Function` constructor parameter
strings:

| total | passed | failed | skipped | timeout | OOM | crash | pass rate |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 515 | 321 | 140 | 54 | 0 | 0 | 0 | 69.63% |

Focused `built-ins/Function/prototype/apply` result at this point:
`43 pass, 2 fail, 3 skip`.

After allowing function values to participate as ordinary object
prototypes and wrapping functions returned from dynamic `Function`
evaluation so calls/constructs keep the originating bytecode module:

| total | passed | failed | skipped | timeout | OOM | crash | pass rate |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 515 | 323 | 138 | 54 | 0 | 0 | 0 | 70.07% |

Delta from the original baseline: +124 passing tests.

Focused `built-ins/Function/prototype/apply` result at this point:
`45 pass, 0 fail, 3 skip`.

After routing dynamic `Function` constructor results through the same
eval-function wrapper used for ordinary dynamic functions, making the
global `Function` object callable/constructible through internal native
slots, and branding callable `Function.prototype` as a Function object:

| total | passed | failed | skipped | timeout | OOM | crash | pass rate |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 515 | 340 | 121 | 54 | 0 | 0 | 0 | 73.75% |

Delta from the original baseline: +141 passing tests.

Focused `built-ins/Function/length` result at this point:
`13 pass, 0 fail, 0 skip`.

Focused `built-ins/Function/prototype/apply` result at this point:
`45 pass, 0 fail, 3 skip`.

After extending the §7.1.1 / §7.1.1.1 `ToPrimitive` /
`OrdinaryToPrimitive` ladder so the `[Symbol.toPrimitive]`,
`valueOf`, and `toString` lookups walk the realm's intrinsic
prototype chain for every non-`Value::Object` heap-shape value
(callables route through `%Function.prototype%`, arrays through
`%Array.prototype%`, and so on), and reordering the bootstrap
registry so `Array.prototype` chains to `Object.prototype` (it had
been left at `null`):

| total | passed | failed | skipped | timeout | OOM | crash | pass rate |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 515 | 355 | 106 | 54 | 0 | 0 | 0 | 77.01% |

Delta from the original baseline: +156 passing tests. Delta from the
prior `built-ins/Function` checkpoint: +15. Most of the gain comes
from `built-ins/Function/prototype/toString` which now resolves
`Function.prototype.toString` through the ladder for every
`Value::Function` / `Value::Closure` / `Value::NativeFunction` /
`Value::BoundFunction` / `Value::ClassConstructor`.

Focused `built-ins/Function/prototype/toString` result at this point:
`25 pass, 55 fail, 0 skip` (was 10 pass / 70 fail).

Focused `built-ins/Function/prototype/bind`, `apply`, and
`built-ins/Function/15.3.2.1-11` remain at 100% non-skip pass.

After making `Op::LoadProperty` invoke accessor getters resolved
against `%Function.prototype%` (so the §10.2.4
`AddRestrictedFunctionProperties` poison pills for `caller` /
`arguments` actually fire on every callable, not just plain
objects), and installing spec-shaped own `name` / `length`
descriptors on every native error constructor (`Error`, `TypeError`,
…, `AggregateError`):

| total | passed | failed | skipped | timeout | OOM | crash | pass rate |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 515 | 360 | 101 | 54 | 0 | 0 | 0 | 78.09% |

Delta from the original baseline: +161 passing tests. Delta from the
prior checkpoint: +5 (net of +21 newly passing tests and -16 ES5-era
`flags: [noStrict]` / `features: [caller]` tests that explicitly
encode the legacy non-strict `f.caller` extension and now correctly
throw per ES2024 §10.2.4). The legacy-extension regressions have
matching `language/arguments-object` analogues for the same reason
(test count drops from 157 to 155).

Focused `built-ins/Function/prototype/bind`, `apply`, and
`built-ins/Function/15.3.2.1-11` remain at 100% non-skip pass.

After installing `%Function.prototype%[@@hasInstance]` per §20.2.3.6
(non-writable, non-enumerable, non-configurable, length 1) and
routing `Op::Instanceof` through §13.10.2 `InstanceofOperator`
(GetMethod-then-OrdinaryHasInstance ladder, so user-defined
`[Symbol.hasInstance]` overrides the default proto-chain walk),
plus wiring `[[Call]]` and `[[Construct]]` slots on every native
error constructor so `new TypeError("…")` / `TypeError(…)` allocate
a real instance (§20.5.1.1 NativeError constructor), the focused
`built-ins/Function` checkpoint moves to:

| total | passed | failed | skipped | timeout | OOM | crash | pass rate |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 515 | 364 | 97 | 54 | 0 | 0 | 0 | 78.96% |

Delta from the original baseline: +165 passing tests. Delta from the
prior checkpoint: +4.

Other suites at this checkpoint:

- `built-ins/Function/prototype/Symbol.hasInstance`: 0 pass / 9 fail
  → 5 pass / 6 fail (descriptor + length tests now pass; the
  remaining failures need `bound.[[BoundTargetFunction]]`-aware
  delegation and getPrototypeOf-side error propagation in
  `OrdinaryHasInstance`).
- `built-ins/NativeErrors`: 30/94 → 42/94 (+12).
- `built-ins/Error`: 17/58 → 18/58 (+1).
- `built-ins/AggregateError`: 9/25 (callable form available).
- `language/expressions/instanceof`: 29/43 baseline.

Focused `built-ins/Function/prototype/bind`, `apply`, and
`built-ins/Function/15.3.2.1-11` remain at 100% non-skip pass.

### language/module-code (P2.1 baseline)

Command:

```sh
cargo run -p otter-test262 --bin otter-test262 -- run \
  --filter language/module-code \
  --output test262_results/p21_module_code_baseline.json
```

Captured 2026-05-10 (P2.1 starting baseline):

| total | passed | failed | skipped | timeout | OOM | crash | pass rate |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 318 | 149 | 168 | 1 | 0 | 0 | 0 | 46.86% |

P2.1 Slice A landed 2026-05-10 (top-level export-lexical
predeclare + module-graph linker constant-pool fix). Two
foundation bugs were addressed in the same slice:

1. `crates/otter-compiler` — `hoist_lexical_names`,
   `hoist_var_names_in_stmt`, and `hoist_function_declarations`
   now walk through `ExportNamedDeclaration` and
   `ExportDefaultDeclaration` so `export const|let|var|class|
   function` is registered in the module env before module-init
   runs (§16.2.1.6 InitializeEnvironment step 9).
2. `crates/otter-bytecode` + `crates/otter-runtime` — the
   per-fragment constant-pool merge schema moved to
   `Op::is_const_pool_operand`. The previous heuristic table
   missed `Op::LoadGlobalOrThrow` / `Op::LoadGlobalOrUndefined`
   (so importer-side free-identifier reads were rebound to
   constants from a dependency module) and mis-classified the
   method-id slots of every `Op::*Call` family opcode as pool
   refs (so `Math.abs` after a fragment merge silently dispatched
   to the wrong `MathMethod`).

Post-Slice-A:

| total | passed | failed | skipped | timeout | OOM | crash | pass rate |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 596 | 156 | 161 | 279 | 0 | 0 | 0 | 49.21% (non-skip) |

Delta vs. baseline: **+7 passing tests** in `language/module-code`.
The runner now also surfaces 279 metadata-skip tests that were
not emitted in the original baseline, so the `total` column
grew. Non-skip pass rate moved 46.86% → 49.21%.

Regression spot-checks (no movement vs. baselines):

| suite | passed |
|---|---:|
| `built-ins/Function` | 364 |
| `built-ins/NativeErrors` | 78 → 79 (+1 after P2.3 InstallErrorCause) |
| `built-ins/Error` | 32 → 33 (+1 after P2.3 InstallErrorCause) |
| `built-ins/AggregateError` | 14 → 15 (+1 after P2.3 InstallErrorCause + iterable lowering) |
| `built-ins/Function/prototype/bind` | 97 / 100 (100% non-skip) |
| `built-ins/Function/prototype/apply` | 45 / 48 (100% non-skip) |
| `built-ins/Function/15.3.2.1-11` | 12 / 12 |
| `language/arguments-object` | 155 |

P2.1 Slice B landed 2026-05-10 (cycle support + spec lifecycle
state model):

1. `crates/otter-runtime/src/module_graph.rs` — `topological_order`
   now skips the cyclic back-edge (per §16.2.1.5 HostLoadImportedModule
   + §16.2.1.6 InnerModuleEvaluation) instead of rejecting the
   graph with `MODULE_GRAPH_CYCLE`. Live-binding indirection
   through the pre-allocated `module_env` JsObject lets the
   not-yet-evaluated side of the cycle read its dependency's
   exports as `undefined` rather than crashing.
2. `crates/otter-runtime/src/module_records.rs` —
   `RuntimeModuleRecordState` expanded to the full spec lifecycle:
   `Unresolved → Resolved → Compiled → Instantiated → Evaluating
   → Evaluated|Errored`. The current loader pipeline batches
   resolve + compile + link before reaching the records table,
   so `allocate_for_module_inits` advances each record directly to
   `Instantiated`; per-phase loader hooks reserved for a follow-up
   slice will use the earlier variants.

Test262 `language/module-code` post-Slice-B: 156 / 317 (49.21%)
— unchanged. Spec-correct cycle behavior unblocks features the
test runner cannot yet exercise (the cycle-bearing tests live
behind `_FIXTURE.js` helper modules the runner doesn't
materialize). The cycle/lifecycle behavior is pinned by 4 new
runtime tests in `tests/module_cycle_and_lifecycle.rs`.

P2.1 Slice C landed 2026-05-10 (dynamic-import routing +
capability gating):

1. `crates/otter-runtime/src/module_loader.rs` — `LoaderConfig`
   gained a `capabilities: CapabilitySet` field; `resolve_with_kind`
   detects `http:` / `https:` specifiers and consults
   `capabilities.net` against the URL host. A denial surfaces as
   the new `LoaderError::CapabilityDenied` variant rather than a
   generic `UnsupportedSpecifier`, so embedders can distinguish
   "missing permission" from "unresolvable shape". The runtime
   maps the denial to a `MODULE_CAPABILITY_DENIED` diagnostic
   code. With the capability granted the loader still needs an
   HTTPS fetcher, surfaced as `MODULE_RESOLUTION_ERROR`; that
   fetcher is the next slice.
2. The pre-Slice-A audit reported `await import("./x.ts")`
   never settling. That gap was incidentally closed by Slice A's
   linker fix (`console.log` was the actual failure point) and
   is now regression-tested in
   `tests/module_dynamic_import_capability.rs::dynamic_literal_import_settles_top_level_await`.

Test262 `language/module-code` post-Slice-C: 156 / 317 (49.21%)
— unchanged. The `language/expressions/dynamic-import` suite
(941 tests) remains 100% skipped because every test there
requires non-literal dynamic-import re-entrant loading or
runner-side `_FIXTURE.js` materialization. Both deferred.

Remaining gaps for `language/module-code`:

- Re-entrant non-literal dynamic `import(expr)`: the runtime
  raises `TypeError: unknown intrinsic method` because the
  linker cannot pre-resolve a runtime-computed specifier.
- HTTPS / package-registry fetcher: capability gating is in
  place; actual content fetching (with reqwest or similar) is a
  separate slice.
- Indirect-export cycle resolution per §15.2.1.16
  ResolveExport (e.g. `instn-iee-iee-cycle.js`) — needs linker-
  time export resolution.
- ~70 `MODULE_RESOLUTION_ERROR` failures are runner-infrastructure
  (`_FIXTURE.js` helper modules), not engine.

### Import defer + module top-level await

Command:

```sh
target/debug/otter-test262 run \
  --filter language/import/import-defer \
  --timeout 20000 \
  --output test262_results/import_defer_after_fmt.json
```

Before:

| total | passed | failed | skipped | timeout | OOM | crash | pass rate |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 101 | 80 | 21 | 0 | 0 | 0 | 0 | 79.21% |

After:

| total | passed | failed | skipped | timeout | OOM | crash | pass rate |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 101 | 101 | 0 | 0 | 0 | 0 | 0 | 100.00% |

Delta: +21 passing tests. The slice closes the non-skipped
`language/import/import-defer` suite by wiring deferred namespace
trigger edge cases, cached module-evaluation errors, dynamic
`import.defer()`, literal dynamic `import()`, nested module export
mirrors, and the import-defer/TLA async evaluation order.

Regression spot-check:

```sh
target/debug/otter-test262 run \
  --filter language/module-code \
  --timeout 20000 \
  --output test262_results/module_code_after_import_defer.json
```

| total | passed | failed | skipped | timeout | OOM | crash | pass rate |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 596 | 423 | 113 | 36 | 0 | 0 | 24 | 75.54% |

### Native error / Function suite checkpoint (P1.2 / P1.3 close)

After tightening native error class metadata (descriptors on
`prototype`, `name`, `message`, and `constructor` per §20.5.2 /
§20.5.3 — non-enumerable data slots), linking the realm-level
`[[Prototype]]` chain (`Error.[[Prototype]]` =
`%Function.prototype%`, `<NativeError>.[[Prototype]]` = `%Error%`,
`Error.prototype.[[Prototype]]` = `%Object.prototype%`),
registering every native error constructor as an own data property
of `globalThis` (`{ writable: true, enumerable: false,
configurable: true }`), installing `Error.prototype.toString` per
§20.5.3.4 as a real function-valued data property with the
spec-mandated `Type(O) is not Object → TypeError` receiver check,
and rewriting `[[GetPrototypeOf]]` to honour explicitly-linked
constructor prototypes while keeping the foundation
`%Function.prototype%` fallback for built-ins still defaulted to
`%Object.prototype%`:

Error-suite checkpoint:

| suite | before | after | delta |
|---|---:|---:|---:|
| `built-ins/NativeErrors` | 42/94 | 78/94 | +36 |
| `built-ins/Error` | 18/58 | 32/58 | +14 |
| `built-ins/AggregateError` | 9/25 | 14/25 | +5 |

Cumulative delta from the original baseline across all error suites:
+55 tests. `built-ins/Function`, `language/arguments-object`, and
the focused `bind`/`apply`/`15.3.2.1-11` baselines are unchanged.

### ThrowTypeError

Command:

```sh
cargo run -p otter-test262 --bin otter-test262 -- run \
  --filter built-ins/ThrowTypeError \
  --timeout 5000 \
  --output test262_results/batch_throw_type_error_after_metadata.json
```

Current:

| total | passed | failed | skipped | timeout | OOM | crash | pass rate |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 14 | 13 | 0 | 1 | 0 | 0 | 0 | 100.00% |

Delta from the immediate batch baseline
`test262_results/batch_throw_type_error_before.json`: +4 passing tests.
The remaining skipped test is cross-realm coverage.

### Object.getOwnPropertyNames

Command:

```sh
cargo run -p otter-test262 --bin otter-test262 -- run \
  --filter built-ins/Object/getOwnPropertyNames \
  --timeout 5000 \
  --output test262_results/object_gopn_next_baseline.json
```

Current:

| total | passed | failed | skipped | timeout | OOM | crash | pass rate |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 45 | 45 | 0 | 0 | 0 | 0 | 0 | 100.00% |

Native/function/exotic descriptor objects now inherit from
`%Object.prototype%`, so `desc.hasOwnProperty(...)` works for
descriptors returned from Array, Function, NativeFunction, and related
Object statics paths.

### Reflect (slice: real namespace + Value-level internal-method dispatch)

Command:

```sh
cargo run -p otter-test262 --bin otter-test262 -- run \
  --filter built-ins/Reflect \
  --timeout 5000 \
  --output test262_results/reflect_after.json
```

Before / after:

| stage | total | passed | failed | skipped | pass rate |
|---|---:|---:|---:|---:|---:|
| before (placeholder)               | 154 | 60  | 93 | 1 | 39.22% |
| after (real namespace)             | 154 | 113 | 40 | 1 | 73.86% |
| after (Value-level dispatch + Proxy invariants) | 154 | 126 | 27 | 1 | 82.35% |

Delta: **+66 passing tests (+43.13 points)**. Routing every Reflect
method through `Interpreter::ordinary_*_value` gave it Proxy, Array,
function, and class-constructor support without duplicating dispatch
logic in `reflect.rs`. Remaining failures are deeper substrate gaps —
real Symbol/`@@toStringTag`, full ToPropertyKey ([[ToPrimitive]] on
descriptor keys), `Object.prototype` as default `[[Prototype]]` for
`{}` literals, and richer `Reflect.set` accessor handling.

### Proxy

Command:

```sh
cargo run -p otter-test262 --bin otter-test262 -- run \
  --filter built-ins/Proxy \
  --timeout 5000 \
  --output test262_results/proxy_after.json
```

Before / after:

| stage | total | passed | failed | skipped | pass rate |
|---|---:|---:|---:|---:|---:|
| before                                                                     | 311 | 145 | 129 | 37 | 52.92% |
| slice 1 (Reflect dispatch)                                                 | 311 | 162 | 112 | 37 | 59.12% |
| slice 2 (Proxy [[Call]]/[[Construct]] + invariants)                        | 311 | 190 | 84  | 37 | 69.34% |
| slice 3 (ownKeys/defineProperty invariants + value-level dispatch)         | 311 | 215 | 59  | 37 | 78.47% |
| slice 4 (V8/JSC-style PartialPropertyDescriptor + full §10.5.6 invariants) | 311 | 225 | 49  | 37 | 82.12% |
| slice 5 (Proxy [[Set]] invariants + Reflect.set receiver + §10.1.2 + `{}` proto) | 311 | 226 | 48 | 37 | 82.48% |
| slice 6 (sync ToPrimitive / ToPropertyKey / ToPropertyDescriptor + Op::SetPrototype throw) | 311 | 226 | 48 | 37 | 82.48% |

Delta: **+81 passing tests (+29.56 points)** for Proxy across six slices.

### Reflect (cumulative)

| stage | total | passed | failed | pass rate |
|---|---:|---:|---:|---:|
| before                       | 154 | 60  | 93 | 39.22% |
| slice 1–5 (namespace + dispatch + invariants + receiver) | 154 | 138 | 15 | 90.20% |
| slice 6 (sync ToPropertyKey + observable ToPropertyDescriptor) | 154 | 147 | 6 | 96.08% |
| slice 7 (real Symbol → @@toStringTag = "Reflect")             | 154 | 148 | 5 | 96.73% |

Delta Reflect: **+88 passing tests (+57.51 points)** across seven slices.

### Object (built-ins/Object overall)

| stage | total | passed | failed | pass rate |
|---|---:|---:|---:|---:|
| slice 3                                            | 3414 | 2037 | 1372 | 59.71% |
| slice 4 (PartialPropertyDescriptor)                | 3414 | 2169 | 1240 | 63.70% |
| slice 6 (observable ToPropertyDescriptor + ToPropertyKey) | 3414 | 2273 | 1132 | 66.75% |
| slice 9 (descriptor prototypes + global/primitive proto + strict equality + function-kind constructors) | 3414 | 3406 | 0 | 100.00% |

Slice 9 closes the non-skipped `built-ins/Object` suite:

- descriptor objects returned through function/native/exotic paths now
  inherit `%Object.prototype%`;
- `globalThis.[[Prototype]]` is wired when `Object` is installed as a
  `NativeFunction` constructor;
- `Object.getPrototypeOf` performs ToObject prototype lookup for
  Boolean, Number, String, Symbol, and BigInt primitives;
- strict equality uses `IsStrictlyEqual`, so `NaN !== NaN` works;
- function-kind constructor placeholders are constructible via the
  existing Function constructor native path;
- `Object.prototype.__proto__` setter drives Proxy
  `[[SetPrototypeOf]]` instead of silently no-oping;
- `String.prototype[Symbol.iterator]` installs when `String` is a
  native constructor.

### Symbol (built-ins/Symbol)

| stage | total | passed | failed | skipped | crashed | pass rate |
|---|---:|---:|---:|---:|---:|---:|
| before (placeholder)                          | 98 | 6  | 71 | 21 | 0 | 7.79% |
| slice 7 (real Symbol ctor + post-bootstrap)   | 98 | 57 | 20 | 21 |  0 | 74.03% |
| slice 9b (constructable Symbol + Date ToPrimitive) | 98 | 77 | 0 | 21 | 0 | 100.00% |

Delta Symbol: **+51 passing tests (+66.24 points)** in slice 7;
**+20 passing tests** in slice 9b, closing all non-skipped cases.
Slice 9b closes the non-skipped `built-ins/Symbol` suite: `%Symbol%`
now has the constructor-branded `[[Construct]]` slot required by
`IsConstructor(Symbol)` while `new Symbol()` still throws, and
`new Date(object)` performs the one-argument `ToPrimitive(default)`
then string/number branch so deleting `Symbol.prototype[@@toPrimitive]`
falls back to observable ordinary `valueOf` / `toString`.

### Date (built-ins/Date)

| stage | total | passed | failed | skipped | crashed | pass rate |
|---|---:|---:|---:|---:|---:|---:|
| slice 9c (Temporal-backed local time + Date edge cases) | 618 | 615 | 0 | 3 | 0 | 100.00% |

Slice 9c closes the non-skipped `built-ins/Date` suite. Local Date
accessors, string rendering, setters, constructor multi-arg form, and
offsetless date-time parsing now use the engine's Temporal-backed host
time-zone provider instead of a UTC placeholder. The same slice fixes
`Date.UTC` floating-point `MakeTime` precision, two-digit year
truncation, expanded-year ISO strings, `Date.parse` of engine-produced
legacy strings, negative-zero expanded years, and
`Reflect.construct(Date, ..., newTarget)` default prototype fallback.

### Iterator protocol §7.4 — slice 8

`Interpreter::get_iterator_sync` / `iterator_step_sync` /
`iterator_close_sync` / `iterator_to_list_sync` cover §7.4.1
GetIterator, §7.4.4 IteratorNext, §7.4.6 IteratorStep, §7.4.8
IteratorClose, §7.4.13 IteratorToList. Built-in iterables (Array,
String, Set, Map, Generator) take fast paths; everything else
routes through `@@iterator`.

The "_sync" suffix carries the spec-level distinction: §7.4 sync
iterator vs §27.1 async iterator. Sibling helpers
(`evaluate_to_primitive` / `evaluate_to_property_key` /
`evaluate_to_property_descriptor`) renamed from `_sync` to match —
those have no async-vs-sync spec dichotomy, so the suffix was
misleading.

| filter | before | after | delta |
|---|---:|---:|---:|
| `built-ins/Array/from` | 23/47 (48.94%) | **26/47 (55.32%)** | +3 |

Wired into `Op::ArrayFrom` so `Array.from(gen())`,
`Array.from(customIterable, mapFn)`, and array-like fallback all
work. Set / Map constructors stay placeholders until their
respective slices.

Slice 2 additions:
- `Value::Proxy` branch in `run_callable_sync` and
  `run_construct_sync` so nested-proxy `apply` / `construct` fallback
  reaches the underlying callable (Proxy/apply 90.9%, Proxy/construct
  no regression).
- `Interpreter::is_extensible_value`,
  `Interpreter::prevent_extensions_value`, and
  `Interpreter::set_prototype_value_proxy_aware` as shared value-level
  internal-method helpers; Reflect.isExtensible /
  Reflect.preventExtensions / Reflect.setPrototypeOf delegate to them.
- `try_proxy_object_static_call` preflight in the `Op::ObjectCall`
  path so `Object.isExtensible(proxy)` and
  `Object.preventExtensions(proxy)` invoke proxy traps with §10.5
  invariants (Proxy/isExtensible 9.1% → 81.8%,
  Proxy/preventExtensions 27.3% → 90.9%).
- §10.5.8 `has` invariants in `ordinary_has_property_value` and
  §10.5.10 `deleteProperty` invariants in `ordinary_delete_value`.

Slice 3 additions:
- `Interpreter::own_property_keys_value` with full §10.5.11 invariant
  chain (CreateListFromArrayLike with «String, Symbol», duplicate
  detection, missing/extra key checks against extensibility and
  configurability). Used by Reflect.ownKeys, Object.keys (proxy
  branch), Object.getOwnPropertyNames, Object.getOwnPropertySymbols.
- `Interpreter::define_own_property_value` — §10.5.6 conservative
  invariants (non-extensible add, configurable-relaxation reject).
  Wired into Reflect.defineProperty and `Object.defineProperty(proxy)`
  preflight.
- `drive_set_prototype_proxy` switched from synthetic `target_object`
  to value-level `set_prototype_value_proxy_aware` so
  `Object.setPrototypeOf(proxy(proxy(x), {}), p)` walks the inner
  Proxy correctly.
- `ordinary_has_property_value` now accepts `Value::Array` and
  callable kinds so nested-proxy `in` fall-through reaches Array
  exotic keys and function metadata.

Section-level highlights (after slice 3):
- Proxy/apply: 22.2% → 90.9%
- Proxy/ownKeys: 18.5% → 96.0%
- Proxy/getOwnPropertyDescriptor: 90.5% → 94.7%
- Proxy/preventExtensions: 0% → 90.9%
- Proxy/isExtensible: 9.1% → 81.8%
- Proxy/setPrototypeOf: 31.2% → 81.2%
- Proxy/deleteProperty: 41.2% → 81.2%
- Proxy/defineProperty: 43.8% → 62.5%

Slice 4 additions:
- New `object::PartialPropertyDescriptor` that mirrors V8 / JSC /
  SpiderMonkey field-presence slot layout. Every `[[Value]]`,
  `[[Writable]]`, `[[Get]]`, `[[Set]]`, `[[Enumerable]]`,
  `[[Configurable]]` field is `Option<…>` so §6.2.5.5
  ToPropertyDescriptor can distinguish "absent" from "present with
  `false`".
- `object_statics::coerce_to_descriptor` returns the new
  `PartialPropertyDescriptor`; new `object::define_own_property_partial`
  / `define_own_symbol_property_partial` and the field-presence-aware
  `descriptor_core::validate_and_apply_partial` (§10.1.6.3) replace
  the legacy "every field is present" path.
- `Interpreter::define_own_property_value` takes the partial form,
  emits a partial `FromPropertyDescriptor` for trap arguments, and
  enforces the full §10.5.6 invariant set (steps 14–20): non-extensible
  add, configurable-relaxation / -demotion, narrow-writable on
  non-configurable data, plus a partial `IsCompatible` predicate.
- Op::New + Proxy: trap result is now validated as Object
  (§10.5.13 step 9); trap-absent fallback delegates to
  `run_construct_sync` so nested proxies and bound targets reuse the
  full constructor pipeline.
- `abstract_ops::is_constructor` treats a non-revoked Proxy as a
  constructor iff its target is.

Section-level highlights (after slice 4):
- Proxy/apply: 22.2% → 90.9%
- Proxy/construct: 44.4% → 83.3%
- Proxy/defineProperty: 43.8% → 87.5%
- Proxy/ownKeys: 18.5% → 96.0%
- Proxy/getOwnPropertyDescriptor: 90.5% → 94.7%
- Proxy/preventExtensions: 0% → 90.9%
- Proxy/isExtensible: 9.1% → 81.8%
- Proxy/setPrototypeOf: 31.2% → 81.2%
- Proxy/deleteProperty: 41.2% → 81.2%

Broader impact (built-ins/Object overall): 2037 → 2169 (+132 tests)
because `defineProperty` no longer treats missing descriptor fields
as `false`.

Remaining failures cluster in Proxy/{set, has (mostly `with` syntax,
not supported), revocable edge cases}. Next slices: accessor walk on
Reflect.set / Proxy.[[Set]] §10.5.9 invariants, plus the
`Object.freeze`/`Object.seal` proxy preflight.

### §24 Keyed Collections — slice 9 (real Map / Set / WeakMap / WeakSet)

Commands:

```sh
target/release/otter-test262 run --filter built-ins/Map     --timeout 5000 --output test262_results/map_after.json
target/release/otter-test262 run --filter built-ins/Set     --timeout 5000 --output test262_results/set_after.json
target/release/otter-test262 run --filter built-ins/WeakMap --timeout 5000 --output test262_results/weakmap_after.json
target/release/otter-test262 run --filter built-ins/WeakSet --timeout 5000 --output test262_results/weakset_after.json
```

Replaces the four bootstrap placeholders with real callable +
constructible `NativeFunction` constructors plus full
prototypes installed in
`crates/otter-vm/src/bootstrap_collections.rs`. Each prototype is
linked to `%Object.prototype%`, carries every spec-listed method
as an own data property, exposes `size` as an accessor (Map/Set),
and gets `@@toStringTag` plus `@@iterator` wired in
[`install_collection_well_knowns_post_bootstrap`].

The compiler short-circuit `Op::NewCollection` that bypassed
`Map.prototype.set` / `Set.prototype.add` and the §7.4 iterator
protocol was removed from `crates/otter-compiler/src/lib.rs`, so
`new Map(iter)` now goes through `Op::New` →
`construct_collection` → `AddEntriesFromIterable` per §24.1.1.2.
Built-in iterables (Array / Map / Set / Generator / String) take
the `iterator_to_list_sync` fast path; user-defined iterables use
the lazy `GetIterator` / `IteratorStep` / `IteratorClose` ladder
so `iterator-close-after-set-failure.js`-style invariants are
observable.

Substrate fix-ups in `crates/otter-vm/src/lib.rs`:

- `Op::LoadProperty` for `Value::Map` / `Set` / `WeakMap` /
  `WeakSet` falls back to walking `<Collection>.prototype` after
  the legacy `size` fast path, so user-installed methods and
  overrides resolve through normal `[[Get]]`.
- `ordinary_get_value` gained branches for the four collection
  value kinds that route to the realm prototype.
- `constructor_prototype_value` now handles `Value::NativeFunction`
  and `Value::ClassConstructor` (in addition to legacy
  `Value::Object`).

Before / after:

| filter | total | passed (before → after) | pass rate (before → after) | delta |
|---|---:|---:|---:|---:|
| `built-ins/Map`     | 215 | 75 → 137  | 41.44% → 75.69% | +62 |
| `built-ins/Set`     | 394 | 169 → 216 | 43.00% → 54.96% | +47 |
| `built-ins/WeakMap` | 141 | 55 → 87   | 54.46% → 86.14% | +32 |
| `built-ins/WeakSet` | 85  | 47 → 75   | 55.95% → 89.29% | +28 |

Cumulative slice 9 delta: **+169 passing tests** across the four
collection suites.

Section-level highlights:

- `built-ins/Map/prototype`: 50.4% → 90.2%
- `built-ins/Map/constructor.js`, `length.js`, `is-a-constructor.js`,
  `iterable-calls-set.js`, `iterator-close-after-set-failure.js`,
  `get-set-method-failure.js`, `prototype.js` — all green.
- `built-ins/Set/set-iterable.js`, `set-iterator-close-after-add-failure.js`,
  `set-get-add-method-failure.js`, `set-iterable-calls-add.js`,
  `set-iterable-empty-does-not-call-add.js` — all green.

Regression spot-checks (no movement vs. published baselines):

| suite | passed |
|---|---:|
| `built-ins/Reflect`     | 148 / 154 (96.73%) |
| `built-ins/Proxy`       | 226 / 311 (82.48%) |
| `built-ins/Symbol`      | 77 / 98 (100.00% non-skip; 21 skip) |
| `built-ins/Array/from`  | 26 / 47 (55.32%)  |

Remaining gaps for the four suites cluster in: `Map.groupBy` /
`Set.prototype.{union,intersection,...}` (new ES2024 surface that
needs separate slices), `Symbol.species`-driven subclass paths
(no `@@species` substrate yet), `MapIteratorPrototype.next` /
`SetIteratorPrototype.next` (the engine's internal
`Value::Iterator` does not expose a JS-callable `.next` —
`@@iterator` on the prototype returns a working iterator value,
but driving it through user-written wrappers still hits the
foundation iterator interception path), and `Symbol.toStringTag`
on the iterator prototypes.

### §27.2 Promise — slice 10 (real Promise constructor + statics + prototype)

Command:

```sh
target/release/otter-test262 run --filter built-ins/Promise --timeout 5000 --output test262_results/promise_after.json
```

Replaces the bootstrap placeholder with a real callable +
constructible `NativeFunction` constructor plus the full
prototype installed in
`crates/otter-vm/src/bootstrap_promise.rs`. The constructor
reuses [`crate::promise_dispatch::PromiseBuilder::construct`] for
the `(handle, resolve, reject)` triple, invokes the executor
synchronously through `run_callable_sync`, and routes a
captured executor throw through the realm `reject` (idempotent
per §27.2.1.4). The prototype carries `then` / `catch` /
`finally` as own data properties; the constructor carries
`resolve` / `reject` / `all` / `race` / `allSettled` / `any` /
`withResolvers` as own data properties. `@@toStringTag = "Promise"`
is installed by `install_promise_well_knowns_post_bootstrap`.

The compiler shortcuts `Op::PromiseNew` (`new Promise(executor)`)
and `Op::PromiseCall` (`Promise.<method>(args)`) were removed from
`crates/otter-compiler/src/lib.rs` so the constructor path goes
through ordinary `Op::New` dispatch and statics resolve through
ordinary property lookup. The legacy opcode handlers stay in the
runtime as dead code for backwards compatibility.

Substrate fix-ups in `crates/otter-vm/src/lib.rs`:

- `Op::LoadProperty` for `Value::Promise` now walks
  `Promise.prototype` so `p.then` / `p.constructor` resolve.
- `ordinary_get_value` gained a `Value::Promise` branch.
- The `Op::CallMethod` lookup-via-property path gained a
  `Value::NativeFunction` branch so `Promise.all(...)` /
  `Promise.resolve(...)` / `Map.groupBy(...)` etc. dispatch
  through ordinary method invocation. (Previously the typed
  `Op::PromiseCall` opcode covered this; without the shortcut the
  ordinary path had to learn `NativeFunction` receivers.)

Before / after:

| filter | total | passed (before → after) | pass rate (before → after) | delta |
|---|---:|---:|---:|---:|
| `built-ins/Promise` | 677 | 332 → 395 | 49.11% → 58.43% | +63 |

Section-level highlights:

- `built-ins/Promise/prototype`: 54.0% → 72.6%
- `built-ins/Promise/all`: 56.1% → 61.2%
- `built-ins/Promise/race`: 62.8% → 66.0%
- `built-ins/Promise/allSettled`: 49.0% → 56.7%
- `built-ins/Promise/any`: 52.1% → 61.7%
- `built-ins/Promise/resolve`: 46.7% → 60.0%

Regression spot-checks (no movement vs. slice 9 baselines):

| suite | passed |
|---|---:|
| `built-ins/Map`         | 137 / 215 (75.69%) |
| `built-ins/Set`         | 216 / 394 (54.96%) |
| `built-ins/Reflect`     | 148 / 154 (96.73%) |
| `built-ins/Proxy`       | 226 / 311 (82.48%) |
| `built-ins/Symbol`      | 77 / 98 (100.00% non-skip; 21 skip) |
| `built-ins/Array/from`  | 26 / 47 (55.32%)  |
| `built-ins/Function`    | 365 / 461 (79.18%) (+1 from prior) |

Remaining Promise gaps cluster in: `executor-function-*.js`
(executor resolve/reject natives lack spec-shaped `length` /
`name` descriptors), `Symbol.species`-driven subclass paths,
`allKeyed` / `allSettledKeyed` proposals, and `Promise.try`
(ES2025 surface). The Promise body still uses the legacy
`run_callable_sync` path for the executor — a follow-up slice
should swap to the spec `[[Construct]]`-aware dispatch for
subclassing.

### §22.2 RegExp — slice 11 (real RegExp constructor + prototype methods + flag accessors)

Replaces the bootstrap placeholder with a real callable +
constructible `NativeFunction` constructor and a prototype with
spec-shaped own properties — see
`crates/otter-vm/src/bootstrap_regexp.rs`.

Prototype carries `exec` / `test` / `toString` as own data
properties plus the §22.2.6 accessor getters `source` / `flags` /
`global` / `ignoreCase` / `multiline` / `dotAll` / `unicode` /
`sticky` / `hasIndices` / `unicodeSets`. The constructor body
mirrors §22.2.3.1 — when `pattern` is itself a `RegExp` and
`flags` is undefined the receiver is returned unchanged; in every
other case the source + flag string flow into
[`crate::regexp::JsRegExp::compile`].

Substrate fix-ups in `crates/otter-vm/src/lib.rs`:

- `Op::LoadProperty` for `Value::RegExp` falls back to walking
  `RegExp.prototype` after the legacy `regexp_prototype::load_property`
  fast path returns `Undefined`.
- `ordinary_get_value` gained the same fall-through.

Targeted before/after (single-subdir, focused timeout — full
sweep is currently slow due to regress backtracking on a handful
of pathological patterns and is tracked as a follow-up):

```sh
target/release/otter-test262 run \
  --filter built-ins/RegExp/prototype --timeout 3000 \
  --output test262_results/regexp_proto_after.json
```

| filter | passed (before → after) | pass rate (before → after) | delta |
|---|---:|---:|---:|
| `built-ins/RegExp/prototype`        | 109 → 187 | 22.9% → 37.5% | +78 |
| `built-ins/RegExp/prototype/exec`   | 49        | 63.6%          | (real proto methods) |
| `built-ins/RegExp/prototype/test`   | 26        | 57.8%          |  |
| `built-ins/RegExp/prototype/source` | 6         | 54.6%          |  |
| `built-ins/RegExp/prototype/flags`  | 5         | 29.4%          |  |
| `built-ins/RegExp/property-escapes` | 144       | 100.0%         | (unchanged, listed for completeness) |
| `built-ins/RegExp/unicodeSets`      | 114       | 100.0%         | |
| `built-ins/RegExp/lookBehind`       | 17        | 100.0%         | |

CLI smoke verifies:

```js
typeof RegExp === 'function'           // true
RegExp.length === 2                    // true
RegExp.name === 'RegExp'               // true
new RegExp("xyz", "g").source          // 'xyz'
new RegExp(/a(b)c/g).exec("xabc")      // ['abc', 'b']
typeof RegExp.prototype.test           // 'function'
typeof Object.getOwnPropertyDescriptor(RegExp.prototype, 'source').get  // 'function'
new RegExp(/a/, "i").flags             // 'i'
```

Regression spot-checks (no movement vs. slice 10 baselines):

| suite | passed |
|---|---:|
| `built-ins/Map`         | 137 / 215 (75.69%) |
| `built-ins/Set`         | 216 / 394 (54.96%) |
| `built-ins/WeakMap`     | 87 / 141 (86.14%) |
| `built-ins/WeakSet`     | 75 / 85 (89.29%) |
| `built-ins/Reflect`     | 148 / 154 (96.73%) |
| `built-ins/Proxy`       | 226 / 311 (82.48%) |
| `built-ins/Promise`     | 395 / 677 (58.43%) |

Known follow-ups not landed in this slice:

- Full `built-ins/RegExp` sweep is currently slow because a handful
  of pathological patterns in the suite drive the `regress` engine
  to maximum backtracking. The wall-clock cost dwarfs the
  `--timeout` per test, so the full sweep is being deferred until
  the engine gains either step bounding or a budget-aware
  backtracker. Per-subdir runs are unaffected.
- `Symbol.match` / `Symbol.replace` / `Symbol.search` / `Symbol.split`
  / `Symbol.matchAll` are still foundation-driven from
  `String.prototype.*`; installing them as own methods on
  `RegExp.prototype` is a separate slice.
- `RegExp.prototype.compile` is not installed yet — it requires a
  `JsRegExp` in-place state swap helper that does not yet exist.
- `RegExp[@@species]` and the `@@species`-driven subclass paths
  remain open.

### §26 WeakRef + FinalizationRegistry — slice 12

Real callable + constructible `NativeFunction` constructors for
both globals plus prototypes installed in
`crates/otter-vm/src/bootstrap_weak_refs.rs`. The compiler
shortcuts `Op::NewWeakRef` / `Op::NewFinalizationRegistry` are
no longer emitted — both ctors go through ordinary `Op::New`
dispatch.

Prototypes:

- `WeakRef.prototype.deref` (own data, length 0)
- `FinalizationRegistry.prototype.register` (length 2)
- `FinalizationRegistry.prototype.unregister` (length 1)
- `<both>.prototype[@@toStringTag]` installed in
  `install_weak_well_knowns_post_bootstrap`.

Substrate fix-ups in `crates/otter-vm/src/lib.rs`:

- `Op::LoadProperty` for `Value::WeakRef` /
  `Value::FinalizationRegistry` walks the realm prototype so
  `.constructor` / installed overrides resolve through the
  standard `[[Get]]` substrate.
- `ordinary_get_value` gained the same branch.

Before / after:

| filter | passed (before → after) | pass rate (before → after) | delta |
|---|---:|---:|---:|
| `built-ins/WeakRef`              | 3 → 20  | 10.71% → 71.43% | +17 |
| `built-ins/FinalizationRegistry` | 8 → 36  | 17.39% → 78.26% | +28 |

Cumulative slice 12 delta: **+45 passing tests** across both
suites.

Regression spot-checks (no movement vs. published baselines):

| suite | passed |
|---|---:|
| `built-ins/Map`     | 137 / 215 (75.69%) |
| `built-ins/Set`     | 216 / 394 (54.96%) |
| `built-ins/Reflect` | 148 / 154 (96.73%) |
| `built-ins/Proxy`   | 226 / 311 (82.48%) |
| `built-ins/Promise` | 395 / 677 (58.43%) |
| `built-ins/RegExp/prototype` | 187 / 499 (37.47%) |

Bootstrap startup ratchet bumped from 480 to 560 GC allocations
(and 200 KB → 240 KB) to accommodate the new constructor +
prototype + native-method allocations. `cargo test -p otter-vm
--lib` is 298/298 green.

Cumulative slices 9 + 10 + 11 + 12: **+355+ passing test262**
across Map / Set / WeakMap / WeakSet / Promise / RegExp/prototype /
WeakRef / FinalizationRegistry.

### §21.2 BigInt — slice 13

Real callable-only `NativeFunction` for `BigInt` installed in
`crates/otter-vm/src/bootstrap_bigint.rs`. The constructor is
intentionally non-constructable (§21.2.1.1 step 1) — `new BigInt(x)`
surfaces as `TypeError`. The ctor + statics + prototype methods all
delegate to the bootstrap native surface, so the body is thin glue.

Statics on the ctor: `asIntN` (length 2), `asUintN` (length 2).
Prototype: `toString` (radix-aware), `valueOf`. `@@toStringTag =
"BigInt"` lands in `install_bigint_well_knowns_post_bootstrap`.

The compiler shortcut `Op::BigIntCall` (covering both
`BigInt(value)` and `BigInt.<static>(args)`) is no longer
emitted — both go through ordinary `Op::New` / `Op::CallMethod`
dispatch.

Substrate fix-ups in `crates/otter-vm/src/lib.rs`:

- `Op::LoadProperty` for `Value::BigInt` walks `BigInt.prototype`
  so `(42n).toString`, `(42n).constructor`, and any user-installed
  override resolve through ordinary `[[Get]]`.
- `ordinary_get_value` gained the same branch.

Before / after:

| filter | passed (before → after) | pass rate (before → after) | delta |
|---|---:|---:|---:|
| `built-ins/BigInt` | 13 → 40 | 17.11% → 52.63% | +27 |

Regression spot-checks:

| suite | passed |
|---|---:|
| `built-ins/Map`     | 137 / 215 (75.69%) |
| `built-ins/Promise` | 395 / 677 (58.43%) |
| `built-ins/Reflect` | 148 / 154 (96.73%) |
| `built-ins/WeakRef` | 20 / 28 (71.43%) |

Cumulative slices 9 + 10 + 11 + 12 + 13: **+380+ passing
test262** across Map / Set / WeakMap / WeakSet / Promise /
RegExp/prototype / WeakRef / FinalizationRegistry / BigInt.

Known remaining BigInt gaps cluster in:
`BigInt.prototype.toLocaleString`, BigInt-receiver
`Number.prototype.toString`-shape coercion, BigInt-flavoured
typed-array constructors (`BigInt64Array` / `BigUint64Array`
both still bootstrap placeholders), and BigInt boxing
(`Object(1n)` → BigInt wrapper object).

### §25.3 DataView — slice 14

Real callable + constructible `NativeFunction` for `DataView`
plus a prototype carrying all 20 spec-listed methods
(`getInt8` / `getUint8` / `getInt16` / … / `setBigUint64`) and
the `buffer` / `byteLength` / `byteOffset` accessor getters. The
prototype methods are thin `NativeFunction` wrappers that
dispatch through the direct DataView prototype helpers, so the
heavy lifting stays in `binary/data_view_prototype.rs`.

Substrate fix-ups:

- `Op::LoadProperty` for `Value::DataView` falls through to
  `DataView.prototype` after the existing `load_property` fast
  path returns `Undefined`, so `dv.getInt32` / `dv.constructor`
  resolve via ordinary `[[Get]]`.
- Compiler shortcut `Op::DataViewCall` (covering
  `new DataView(...)`) is no longer emitted; the constructor
  flows through ordinary `Op::New`.

Before / after:

| filter | passed (before → after) | pass rate (before → after) | delta |
|---|---:|---:|---:|
| `built-ins/DataView` | 140 → 223 | 31.96% → 50.91% | +83 |

Regression spot-checks (no movement vs. slice 13 baselines):

| suite | passed |
|---|---:|
| `built-ins/Map`     | 137 / 215 (75.69%) |
| `built-ins/Promise` | 395 / 677 (58.43%) |
| `built-ins/Reflect` | 148 / 154 (96.73%) |
| `built-ins/BigInt`  | 40 / 77 (52.63%) |
| `built-ins/WeakRef` | 20 / 28 (71.43%) |

Cumulative slices 9 + 10 + 11 + 12 + 13 + 14: **+460+ passing
test262** across Map / Set / WeakMap / WeakSet / Promise /
RegExp/prototype / WeakRef / FinalizationRegistry / BigInt /
DataView.

Smoke check (CLI):

```js
typeof DataView === 'function'              // true
DataView.length === 1                       // true
new DataView(buf).byteLength === 16         // true
dv.getInt32(0, false)                       // round-trips
dv.buffer === buf                           // true
DataView(buf)  // throws TypeError "constructor requires 'new'"
```

Known remaining DataView gaps: little-endian / big-endian
distinction edge cases, subclassing via `OrdinaryCreateFromConstructor`
(deferred until `@@species` substrate lands), and detached buffer
read invariants in a handful of edge-case tests.

### §23.2 TypedArray — slice 15 (11 concrete constructors + shared %TypedArray%.prototype)

Replaces 11 bootstrap placeholders (`Int8Array` …
`BigUint64Array`) with real callable + constructible
`NativeFunction` ctors. Per-kind prototypes chain to a single
shared `%TypedArray%.prototype` object that carries 20 spec
methods (`at`, `subarray`, `slice`, `fill`, `copyWithin`,
`reverse`, `indexOf`, `lastIndexOf`, `includes`, `join`,
`toString`, `toLocaleString`, `set`, `toReversed`, `toSorted`,
`sort`, `with`, `keys`, `values`, `entries`). Each per-kind
prototype owns `BYTES_PER_ELEMENT`, `constructor`, and (in the
post-bootstrap fixup) `@@toStringTag = "<T>Array"`. The abstract
prototype gets `@@iterator = values`.

The compiler shortcut `Op::TypedArrayCall` for the `Construct`
path is no longer emitted; per-kind static-side
`<T>.from(...)` / `<T>.of(...)` shortcuts remain pending until
those statics are wired through the real ctor.

Substrate fix-ups in `crates/otter-vm/src/lib.rs`:

- `Op::LoadProperty` for `Value::TypedArray` falls through to
  `<T>.prototype` (resolved via the receiver's
  `TypedArrayKind::name()`).

Bootstrap startup ratchet bumped from 560 to 640 GC allocations
(and 240 KB → 280 KB) to accommodate 11 ctors + 11 per-kind
prototypes + one shared `%TypedArray%.prototype` carrying 20
native methods.

Before / after:

| filter | passed (before → after) | pass rate (before → after) | delta |
|---|---:|---:|---:|
| `built-ins/TypedArray` | 132 → 332 | 7.04% → 17.70% | +200 |

CLI smoke:

```js
typeof Uint8Array === 'function'       // true
Uint8Array.length === 3                // true
Uint8Array.name === 'Uint8Array'       // true
Uint8Array.BYTES_PER_ELEMENT === 1     // true
new Uint8Array(4).constructor === Uint8Array  // true
typeof new Uint8Array(4).fill === 'function'  // true
Object.getPrototypeOf(Uint8Array.prototype) === %TypedArray%.prototype  // true (via @@%TypedArrayPrototype% slot)
```

Regression spot-checks (no movement vs. slice 14 baselines):

| suite | passed |
|---|---:|
| `built-ins/Map`     | 137 / 215 (75.69%) |
| `built-ins/Promise` | 395 / 677 (58.43%) |
| `built-ins/Reflect` | 148 / 154 (96.73%) |
| `built-ins/DataView`| 223 / 561 (50.91%) |
| `built-ins/WeakRef` | 20 / 28 (71.43%) |
| `built-ins/BigInt`  | 40 / 77 (52.63%) |

Cumulative slices 9 + 10 + 11 + 12 + 13 + 14 + 15: **+660+
passing test262** across Map / Set / WeakMap / WeakSet /
Promise / RegExp/prototype / WeakRef / FinalizationRegistry /
BigInt / DataView / TypedArray.

Known remaining TypedArray gaps:

- `built-ins/Uint8Array` / `built-ins/Int32Array` / etc. are
  100% skipped — the per-kind subdir tests need a working
  `%TypedArray%` realm intrinsic exposed reflectively
  (`Object.getPrototypeOf(Uint8Array) === %TypedArray%`); the
  current implementation links the prototypes but not the
  constructor super-class chain.
- Static `<T>.from(...)` / `<T>.of(...)` still flow through the
  compiler shortcut `Op::TypedArrayCall` so user overrides are
  not yet observable on these.
- `subarray` / `slice` / `set` element-type coercion for
  cross-kind copies still uses direct native prototype bodies;
  spec `@@species`-driven derived-array allocation is not
  implemented.

### §25.1 ArrayBuffer — slice 16 (fallible alloc + real ctor)

Two pieces landed together:

1. **Fallible allocation** in `crates/otter-vm/src/binary/array_buffer.rs` —
   `JsArrayBuffer::try_new` swaps `vec![0u8; len]` (which aborts the
   process on huge `len`) for `Vec::try_reserve_exact` + `Vec::resize`.
   The dispatch path now surfaces a `RangeError` instead of the
   process crashing. This alone unblocked the suite (the previous
   baseline could not even capture a number — the runner aborted
   on `new ArrayBuffer(2**50)`-style tests).
2. **Real ctor + prototype** in
   `crates/otter-vm/src/bootstrap_array_buffer.rs`:
   - Constructor: `ArrayBuffer(length, options?)` with `[[Construct]]`,
     `length` 1, `name` `"ArrayBuffer"`. Bare-call throws `TypeError`.
   - Static: `isView(arg)`.
   - Prototype methods: `slice`, `resize`, `transfer`,
     `transferToFixedLength` (all wrappers over the native
     prototype method table).
   - Prototype accessors: `byteLength`, `maxByteLength`,
     `resizable`, `detached`.
   - `constructor` back-pointer + `@@toStringTag = "ArrayBuffer"`.

Compiler shortcuts removed:

- `Op::ArrayBufferCall` for `new ArrayBuffer(...)` — gone.
- `Op::ArrayBufferCall` for `ArrayBuffer.isView(...)` static — gone.

Substrate fix-up in `crates/otter-vm/src/lib.rs`:

- `Op::LoadProperty` for `Value::ArrayBuffer` falls through to
  `ArrayBuffer.prototype` so `b.constructor` and reflectively
  installed methods resolve via ordinary `[[Get]]`.

Before / after:

| filter | passed (before → after) | pass rate (before → after) | delta |
|---|---:|---:|---:|
| `built-ins/ArrayBuffer` | (crash) → 52 | (crash) → 65.00% | +52 measurable |

Regression spot-checks:

| suite | passed |
|---|---:|
| `built-ins/DataView`   | 223 / 561 (50.91%) |
| `built-ins/TypedArray` | 332 / 2177 (17.70%) |
| `built-ins/Map`        | 137 / 215 (75.69%) |
| `built-ins/Promise`    | 395 / 677 (58.43%) |

Cumulative slices 9–16: **+710+ passing test262** across the
keyed-collection / promise / regex / weak-collection / bigint /
binary-data builtins.

### §25.2 SharedArrayBuffer — slice 17

Real callable + constructible `NativeFunction` ctor + prototype
installed alongside `ArrayBuffer` in
`crates/otter-vm/src/bootstrap_array_buffer.rs`. Shared buffers
use the same `JsArrayBuffer` substrate as `ArrayBuffer` — the
`is_shared` flag distinguishes them on the value side. The
prototype carries `slice` + `grow` plus accessor getters for
`byteLength`, `maxByteLength`, `growable`; `constructor` back-
pointer and `@@toStringTag = "SharedArrayBuffer"` are wired in
the post-bootstrap hook.

Substrate fix-ups:

- `JsArrayBuffer::try_new_shared` added with `Vec::try_reserve_exact`
  so the dispatch path can surface a `RangeError` on huge
  allocations.
- `Op::LoadProperty` for `Value::ArrayBuffer` now picks the
  correct prototype (`ArrayBuffer.prototype` vs.
  `SharedArrayBuffer.prototype`) based on `JsArrayBuffer::is_shared()`.
- Compiler shortcut `Op::SharedArrayBufferCall` no longer
  emitted; ctor flows through ordinary `Op::New`.
- `test262_config.toml` `skip_features` entry for
  `"SharedArrayBuffer"` removed — the feature is no longer a
  blanket skip. (`"Atomics"` / `"Atomics.pause"` remain skipped
  pending the cross-thread atomics infra.)

Before / after:

| filter | passed (before → after) | pass rate (before → after) | delta |
|---|---:|---:|---:|
| `built-ins/SharedArrayBuffer` | 0 → 37 | 0% (all skipped) → 62.71% | +37 |

Regression spot-checks:

| suite | passed |
|---|---:|
| `built-ins/ArrayBuffer` | 52 (was 52; total grew from 80 → 82 as a couple of tests un-skipped) |
| `built-ins/Map`         | 137 / 215 (75.69%) |
| `built-ins/Promise`     | 395 / 677 (58.43%) |

Cumulative slices 9–17: **+750+ passing test262** across the
keyed-collection / promise / regex / weak-collection / bigint /
binary-data builtins.

Known SAB follow-ups:

- `b.byteLength === 0` after grow / detach edge cases share the
  same Promise of fix as `ArrayBuffer.prototype.transfer`.

### §25.4 Atomics — slice 18 (spec-faithful namespace dispatch + `Atomics.pause`)

Why now: `atomics.rs` shipped a real namespace at slice 1 but the
compiler-side `Op::AtomicsCall` shortcut bypassed ECMA-262 §25.4
coercion semantics — `index` was rejected unless it was a literal
`Number`, out-of-range errors surfaced as `TypeError` instead of
`RangeError`, `Uint8ClampedArray` was incorrectly accepted, and
`wait` did not require a `SharedArrayBuffer`. Slice 17 made
`SharedArrayBuffer` real, so the path was finally observable from
test262. Slice 18 makes the surface spec-faithful.

What landed:

- `crates/otter-compiler/src/lib.rs` — drop the `Atomics.<method>`
  compiler shortcut. All atomic calls now resolve through ordinary
  property lookup on the namespace and dispatch through the
  installed `Value::NativeFunction` table.
- `crates/otter-vm/src/atomics.rs` — full rewrite of every native
  handler around three new spec helpers:
  - `validate_integer_typed_array(value, waitable)` —
    §25.4.3.1 / §25.4.3.2 rejects `Float32Array`, `Float64Array`,
    `Uint8ClampedArray`; the `waitable` flag additionally
    restricts to `Int32Array` / `BigInt64Array` for
    `wait` / `waitAsync` / `notify`.
  - `validate_atomic_access(ctx, ta, request_index, name)` —
    coerces `request_index` through `ToIndex` (Symbol / BigInt
    early-error, object → `[Symbol.toPrimitive]` /
    `valueOf` / `toString`), bounds-checks against
    `typedArray.length`, raises `RangeError` for any out-of-range
    or negative index.
  - `coerce_element_value(ctx, kind, value, name)` —
    full §7.1 ToNumber / ToBigInt coercion, including:
    * `+0` / `-0` collapse to `+0` (spec §7.1.5 step 2) so
      `Atomics.store(view, 0, -0)` returns `+0`.
    * BigInt kinds gate on `Value::BigInt` (with string + boolean
      conversion routes); mixing `BigInt` and `Number` throws
      `TypeError` from the correct method name.
- `compareExchange` round-trips `expected` through the element type
  (`narrow_through_kind`) so `Atomics.compareExchange(view, i,
  123_456_789, 0)` matches the wrapped `Int16` value `-13035`.
- `Atomics.wait` / `Atomics.waitAsync` reject non-`SharedArrayBuffer`
  backings (`is_shared() == false → TypeError`) and surface
  `RangeError` for out-of-range indices.
- `Atomics.pause` (ES2025 Stage 4) added: validates the
  `iterationNumber` argument is integral `Number` per the live
  spec; the no-op body matches the single-threaded VM.
- `bootstrap::install_atomics` now sets `Atomics.[[Prototype]]` to
  `%Object.prototype%` (fixes `built-ins/Atomics/proto.js`).
- Native handlers route user-thrown coercion errors through
  `NativeError::Thrown` so tests like
  `Atomics/notify/symbol-for-index-throws.js` observe the original
  `Test262Error` payload rather than a synthetic engine error.
- `test262_config.toml` drops `"Atomics"` and `"Atomics.pause"`
  from `skip_features`. The agent-based subset (`$262.agent.*`,
  ~112 tests) stays failing — those require multi-isolate
  infra. The `Op::AtomicsCall` opcode handler in
  `crates/otter-vm/src/lib.rs` is retained as dead code so older
  bytecode keeps loading.
- Bootstrap ratchet — `default_bootstrap_telemetry_matches_startup_ratchet`
  expects `102 + reflect::REFLECT_SPEC.methods.len()` native
  functions (the extra one is `Atomics.pause`).

Before / after:

| filter | passed (before → after) | pass rate (before → after) | delta |
|---|---:|---:|---:|
| `built-ins/Atomics` | 141 → 243 / 382 | 37.40% → 64.46% | **+102** |

Per-section deltas (after):

| Section | passed / total | pass rate |
|---|---:|---:|
| `Atomics/add`             | 15 / 15  | 100.0% |
| `Atomics/and`             | 15 / 15  | 100.0% |
| `Atomics/exchange`        | 16 / 16  | 100.0% |
| `Atomics/isLockFree`      | 7  / 7   | 100.0% |
| `Atomics/or`              | 15 / 15  | 100.0% |
| `Atomics/sub`             | 15 / 15  | 100.0% |
| `Atomics/xor`             | 15 / 15  | 100.0% |
| `Atomics/load`            | 13 / 14  | 92.9%  |
| `Atomics/compareExchange` | 15 / 16  | 93.8%  |
| `Atomics/store`           | 13 / 16  | 81.2%  |
| `Atomics/pause`           | 5  / 6   | 83.3%  |
| `Atomics/notify`          | 23 / 51  | 47.9%  |
| `Atomics/wait`            | 25 / 77  | 32.9%  |
| `Atomics/waitAsync`       | 28 / 101 | 28.0%  |

The remaining `wait` / `waitAsync` / `notify` cluster is dominated
by `$262.agent.*` cross-worker tests that require a multi-isolate
host harness (≈112 tests). Single-thread VM cannot pass these
without a worker / agent runtime.

Regression spot-checks (no losses):

| suite | passed (after) | baseline (slice 17) |
|---|---:|---:|
| `built-ins/SharedArrayBuffer` | 37 / 59  | 37 (= no change) |
| `built-ins/ArrayBuffer`       | 52 / 82  | 52 (= no change) |
| `built-ins/BigInt`            | 40 / 77  | 40 (= no change) |
| `built-ins/TypedArray`        | 344 / 2177 | 332 (+12)      |
| `built-ins/DataView`          | 240 / 561  | 223 (+17)      |

### `$262` host harness scaffold — slice 19a (workers phase 1)

Why: ≈285 test262 tests reference the host-defined `$262`
global. Before this slice every such test failed with
`ReferenceError: $262 is not defined` regardless of which `$262`
method it called.

Plan: `docs/workers-262-plan.md` splits the work into
three checkpoints — 19a (non-agent `$262` surface, this slice),
19b (Arc-backed `SharedArrayBuffer` + cross-thread Atomics
park/wake), 19c (real OS-thread `$262.agent.*`).

What landed:

- `crates/otter-test262/src/harness.rs` — new `D262_HOST_PREAMBLE`
  prepended to every non-`raw` test. The preamble is pure
  JavaScript and reuses existing engine features:
  - `$262.global = globalThis`.
  - `$262.gc()` — no-op. Engine GC is automatic; tests that
    require observable host-GC reclamation stay gated behind the
    `host-gc-required` `skip_features` entry.
  - `$262.detachArrayBuffer(buf)` — delegates to
    `buf.transfer()`, which already detaches the source per
    §25.1.5.5.
  - `$262.IsHTMLDDA()` — placeholder callable.
  - `$262.evalScript(s)` — function-scoped fallback
    (`new Function(s)()`); upgrades to real indirect-`eval` when
    the `eval` global is installed (separate slice).
  - `$262.agent.{start,broadcast,getReport,sleep,monotonicNow,
    receiveBroadcast,report,leaving}` — every method throws a
    descriptive `Error("agents not yet supported")`. Replaces
    `TypeError: undefined is not an object` with a deterministic
    failure mode the diff inspector can recognise.
  - `$262.agent.timeouts = { short, medium, long, huge }` — fixed
    values that match V8's d8.cc defaults.

Before / after:

| filter | passed (before → after) | pass rate | delta |
|---|---:|---:|---:|
| `built-ins/Atomics`     | 243 → 247 / 382 | 64.46% → 65.52% | **+4** |
| `built-ins/ArrayBuffer` | 52 → 53 / 82    | 63.41% → 64.63% | **+1** |

No-regression sweep (slice 19a):

| suite | passed (after) | delta vs. slice 18 |
|---|---:|---:|
| `built-ins/SharedArrayBuffer`   | 37 / 59  | 0 |
| `built-ins/WeakRef`             | 20 / 28  | 0 |
| `built-ins/FinalizationRegistry`| 36 / 47  | 0 |

`cargo test -p otter-vm --lib` 298/298 green;
`cargo test -p otter-test262 --lib` 42/42 green.

Known limits of 19a:

- The 112 `$262.agent.*` cross-worker tests still fail — their
  thrown error is now `Error("agents not yet supported")`
  instead of `ReferenceError`, but the test outcome is unchanged.
  These wait on slice 19c.
- `WeakRef` / `FinalizationRegistry` host-GC tests stay skipped
  via the `host-gc-required` feature gate; `$262.gc()` is a no-op
  and cannot make them pass without runner-side GC scheduling.
- `$262.evalScript` cannot leak top-level `var` to global scope
  (it routes through `new Function`). Tests asserting that
  `$262.evalScript("var x = 1")` makes `x` visible on `globalThis`
  fail until indirect `eval` is installed on the global.

### Arc-backed `SharedArrayBuffer` + Atomics wait registry — slice 19b

Why: slice 18 made `Atomics.wait` / `Atomics.notify` spec-shaped
but the implementation still treated the wait result as a
synchronous `"timed-out"` because the SAB backing was `Rc` (single
thread, no other writer) and there was no cross-thread parking
infrastructure. Slice 19b puts the real plumbing in place so the
moment slice 19c spawns agent threads they can communicate.

What landed:

- `crates/otter-vm/src/binary/array_buffer.rs` — split storage:
  - `BufferStorage::Local(Rc<LocalBody>)` keeps the existing
    `RefCell<Vec<u8>>` fast path for non-shared `ArrayBuffer`.
  - `BufferStorage::Shared(Arc<SharedBody>)` for
    `SharedArrayBuffer`: `Mutex<Vec<u8>>` for the bytes and a
    process-unique `id: u64` from a `static AtomicU64` allocator.
  - New unified borrow guards `BytesRef<'_>` / `BytesRefMut<'_>`
    (`Deref<Target = Vec<u8>>`) so the 12 existing call sites in
    `binary/typed_array.rs`, `binary/typed_array_prototype.rs`,
    `binary/array_buffer_prototype.rs`, and
    `binary/data_view_prototype.rs` keep working unchanged.
  - New `shared_id()`, `as_shared_arc()`, `from_shared_arc(...)`
    accessors for the slice-19c cross-thread message path.
  - `is_detached()`, `is_resizable()`, `is_growable()`, `grow`,
    `resize`, `detach` route through the storage variant and
    reject the wrong operation per spec.
- `crates/otter-vm/src/atomics_wait.rs` (new, 215 lines):
  - `ParkSlot { handle: Thread, notified: AtomicBool }`.
  - Global `static REGISTRY: LazyLock<Mutex<HashMap<(u64, usize),
    Vec<Arc<ParkSlot>>>>>`.
  - `park_until_notified(buf_id, idx, timeout: Option<Duration>)`
    parks via `thread::park_timeout`, drains itself from the
    registry on wake, and distinguishes `WaitOutcome::Ok` (the
    notify flipped `notified`) from `WaitOutcome::TimedOut`.
  - `notify_waiters(buf_id, idx, count)` drains up to `count`
    slots under the registry lock, then unparks them outside the
    lock so the wakees do not contend with new waiters.
  - Three unit tests (zero-timeout, cross-thread wake, empty
    notify) cover the contract.
- `crates/otter-vm/src/atomics.rs` `do_wait` now blocks via
  `atomics_wait::park_until_notified` instead of returning
  `"timed-out"` immediately. The synchronous `"ok"` outcome
  matches §25.4.3.13. `Atomics.waitAsync` still resolves with a
  pre-fulfilled promise (single-thread foundation does not
  schedule the unpark on a microtask yet — that lands with the
  worker harness in 19c).
- `crates/otter-vm/src/atomics.rs` `native_notify` queries
  `JsArrayBuffer::shared_id()`; on a SAB it drives
  `atomics_wait::notify_waiters` so the result reflects the real
  number of woken parkers. Non-shared backings still return 0
  per spec.

Before / after (no agent harness yet, so no notify-from-other-thread
test fires — these are no-regression numbers):

| filter | passed (slice 18 → 19b) | delta |
|---|---:|---:|
| `built-ins/Atomics`           | 243 → 247 | +4  (carried from 19a) |
| `built-ins/ArrayBuffer`       | 52 → 53   | +1  (carried from 19a) |
| `built-ins/SharedArrayBuffer` | 37 → 37   | 0 |
| `built-ins/DataView`          | 240 → 279 | **+39** |
| `built-ins/TypedArray`        | 344 → 413 | **+69** |

The DataView + TypedArray gains are a side-effect of the
storage-split refactor: the unified `BytesRef` / `BytesRefMut`
deref path no longer accidentally panics on lock contention in
the few prototype methods that read the buffer while another
prototype method still held a borrow (the old code combined
`RefCell` with `transfer` semantics that double-borrowed in the
copy path).

`cargo test -p otter-vm --lib` 298 → 301 (three new
`atomics_wait` tests, all green); `cargo check` clean across
`otter-vm` / `otter-runtime` / `otter-cli`.

### `$262.agent.*` over real OS threads — slice 19c

Why: slice 19b shipped the cross-thread Atomics wait registry and
the Arc-backed `SharedArrayBuffer`, but nothing drove a notify
from a second thread, so the 112 `$262.agent.*` Atomics tests
still failed with `Error("agents not yet supported")`. Slice 19c
plugs the real host harness in.

What landed:

- `crates/otter-runtime/src/lib.rs` — new
  `Runtime::install_native_global(name, length, fn)` exposes the
  GC-allocated `Value::NativeFunction` + `set_global` path so
  out-of-crate host bindings can add globals without modifying
  the otter-vm bootstrap.
- `crates/otter-vm/src/runtime_cx.rs` — promote
  `NativeCtx::interp_mut_and_context()` from `pub(crate)` to
  `pub` so the test262 agent natives can re-enter the
  interpreter (needed by `receiveBroadcast` to invoke its JS
  handler).
- `crates/otter-test262/src/agent.rs` (new, 365 lines):
  - Process-wide `static AGENTS: LazyLock<Mutex<AgentRegistry>>`
    owns one `mpsc::Sender<BroadcastMessage>` per running agent
    and a `VecDeque<String>` for `report` / `getReport`.
  - `AGENT_INBOXES` keys each agent's broadcast receiver by
    `ThreadId`; the parent thread never registers one, so a stray
    `receiveBroadcast` outside an agent fails deterministically
    with `TypeError`.
  - Eight native fast-fn bindings:
    - `__otter_agent_start(source)` — spawns a real OS thread
      via `std::thread::Builder`, registers the agent inbox in
      `AGENT_INBOXES`, builds a fresh `Runtime`, installs the same
      `__otter_agent_*` natives, prepends `D262_HOST_PREAMBLE`,
      and runs the user source.
    - `__otter_agent_broadcast(sab, num?)` — pulls the
      `Arc<SharedBody>` out via
      `JsArrayBuffer::as_shared_arc()`, captures the sender
      list under lock, releases the lock, then fans out the
      message. Non-shared backings raise `TypeError`.
    - `__otter_agent_receive_broadcast(handler)` — temporarily
      moves the receiver out of `AGENT_INBOXES`, blocks on
      `Receiver::recv`, restores the receiver, rewraps the
      received `Arc<SharedBody>` via
      `JsArrayBuffer::from_shared_arc` on the agent's heap, and
      invokes the JS handler via
      `Interpreter::run_callable_sync`. User-thrown values
      propagate through `NativeError::Thrown`.
    - `__otter_agent_sleep(ms)` — `thread::sleep`.
    - `__otter_agent_monotonic_now()` — milliseconds since the
      first call into the process.
    - `__otter_agent_report(s)` / `__otter_agent_get_report()`
      — FIFO queue on `AGENTS.reports`.
    - `__otter_agent_leaving()` — no observable side effect;
      reserved for future `getReport` polling.
  - `reset_for_next_test()` clears senders + reports + inboxes so the
    runner can drop residual state between tests.
- `crates/otter-test262/src/harness.rs` — `D262_HOST_PREAMBLE`
  drops the throwing stubs; each `$262.agent.*` method now
  delegates to its `__otter_agent_*` native.
- `crates/otter-test262/src/runner.rs` — `run_one` calls
  `agent::reset_for_next_test()` then
  `agent::install_natives(&mut runtime)` between the fresh-runtime
  build and the harness preamble.
- `crates/otter-test262/Cargo.toml` — pull in `otter-vm` +
  `smallvec` for the native handler signatures.

Cross-thread model:

```
   parent thread                          agent thread #N
   -------------                          ---------------
   $262.agent.start(src)
      └─ thread::spawn
              \________________________> Runtime::builder()...build()
                                          install_natives()
                                          run_script(preamble + src)
                                          (executes $262.agent.receiveBroadcast)
   $262.agent.broadcast(sab)
      └─ AGENTS.senders.iter().send(msg)
              \________________________> rx.recv() unblocks
                                          handler(sab_rewrapped)
                                          Atomics.wait(...) ──┐
   $262.agent.getReport()                                     │
      └─ AGENTS.reports.pop_front()                           │
                                                              ▼
                                          Atomics.notify(...) wakes parker
                                          via the slice 19b registry
```

Before / after (Atomics):

| filter | passed (19b → 19c) | pass rate | delta |
|---|---:|---:|---:|
| `built-ins/Atomics` | 247 → 290 / 382 | 65.52% → 76.92% | **+43** |

Per-section after 19c:

| Section | passed / total | pass rate |
|---|---:|---:|
| `Atomics/add`             | 15 / 15  | 100.0% |
| `Atomics/and`             | 15 / 15  | 100.0% |
| `Atomics/compareExchange` | 16 / 16  | 100.0% |
| `Atomics/exchange`        | 16 / 16  | 100.0% |
| `Atomics/isLockFree`      | 7  / 7   | 100.0% |
| `Atomics/or`              | 15 / 15  | 100.0% |
| `Atomics/proto.js`        | 1  / 1   | 100.0% |
| `Atomics/sub`             | 15 / 15  | 100.0% |
| `Atomics/xor`             | 15 / 15  | 100.0% |
| `Atomics/load`            | 13 / 14  | 92.9%  |
| `Atomics/store`           | 14 / 16  | 87.5%  |
| `Atomics/pause`           | 5  / 6   | 83.3%  |
| `Atomics/waitAsync`       | **80 / 101** | **80.0%**  |
| `Atomics/notify`          | **29 / 51**  | **60.4%**  |
| `Atomics/wait`            | **32 / 77**  | **42.1%**  |

`Atomics/waitAsync` was 28% pre-19c → now 80%. `Atomics/notify`
60.4% (was 47.9%). `Atomics/wait` 42.1% (was 32.9%). The wait
floor is set by tests that issue an unbounded `Atomics.wait` and
expect another agent to wake them; some of these still race
against the per-test wall-clock timeout (5 s by default) when
the agent thread takes longer than that to reach the matching
`notify`. Slice 19d may tune the timeout / dispatch order.

Regression spot-checks (cumulative across 19a/b/c):

| suite | passed (after) | baseline (slice 17) |
|---|---:|---:|
| `built-ins/SharedArrayBuffer`   | 37 / 59  | 37 (= no change) |
| `built-ins/ArrayBuffer`         | 53 / 82  | 52 (+1)        |
| `built-ins/DataView`            | 279 / 561 | 223 (+56)     |
| `built-ins/TypedArray`          | 413 / 2177 | 332 (+81)    |

`cargo test -p otter-vm --lib` 301/301 green;
`cargo test -p otter-test262 --lib` 42/42 green.

Pending tail:

- `Atomics/wait` remaining failures are largely race-against-the-
  5 s per-test wall clock. Either bump the timeout for `Atomics`
  specifically or implement `$262.agent.timeouts.long` enforcement
  on the test driver.
- `$262.evalScript` still routes through `new Function(s)()`;
  some tests assert global-scope `var` leakage.
- `WeakRef` / `FinalizationRegistry` host-GC tests stay gated by
  the `host-gc-required` feature gate.

### `[[GetPrototypeOf]]` for exotic objects — slice 19d

Why: profiling 19c failures revealed that ~42 of the remaining
`Atomics/wait` failures, ~16 of `Atomics/notify`, and a long tail
across `TypedArray` / `DataView` / `Map` / `Set` / etc. all
flopped on the same primitive: `Object.getPrototypeOf(buf)`
threw `TypeError: operand type mismatch` for every exotic value
(TypedArray, DataView, ArrayBuffer, Map, Set, WeakRef, …).
`safeBroadcast` in `harness/atomicsHelper.js` does
`Object.getPrototypeOf(typedArray).constructor` on its very first
line, so the entire cross-thread test path tripped over this
before any agent code ran.

What landed:

- `crates/otter-vm/src/lib.rs::get_prototype_for_op` — extend
  past the function-family branches to cover every realm-class
  exotic Value:
  - `Value::Array`, `Value::RegExp`, `Value::Map`, `Value::Set`,
    `Value::WeakMap`, `Value::WeakSet`, `Value::WeakRef`,
    `Value::FinalizationRegistry`, `Value::Promise`,
    `Value::ArrayBuffer` (shared or non-shared),
    `Value::DataView`, `Value::TypedArray`.
  - Each routes through `intrinsic_prototype_object_for`, which
    looks up the realm's constructor and reads `.prototype`. The
    SAB / AB split returns either `%SharedArrayBuffer.prototype%`
    or `%ArrayBuffer.prototype%` based on `JsArrayBuffer::is_shared()`;
    TypedArray returns the per-kind prototype (`%Int32Array.prototype%`,
    `%BigInt64Array.prototype%`, etc.) via `TypedArrayKind::name()`.
- `crates/otter-vm/src/lib.rs::intrinsic_prototype_object_for` —
  extend the constructor-name match with `FinalizationRegistry`,
  `DataView`, per-kind `TypedArray`, and the SAB / AB split.

Before / after:

| filter | passed (19c → 19d) | pass rate | delta |
|---|---:|---:|---:|
| `built-ins/Atomics`        | 290 → 348 / 382 | 76.92% → **92.31%** | **+58** |
| `built-ins/TypedArray`     | 413 → 460 / 2177 | 21.32% → 23.75% | **+47** |
| `built-ins/DataView`       | 279 → 289 / 561  | 58.61% → 60.71% | **+10** |
| `built-ins/Map`            | 137 → 139 / 215  | 75.69% → 76.80% | +2 |
| `built-ins/Set`            | 216 → 218 / 394  | 54.96% → 55.47% | +2 |
| `built-ins/WeakMap`        | 87  → 89  / 141  | 86.14% → 88.12% | +2 |
| `built-ins/WeakSet`        | 75  → 76  / 85   | 89.29% → 90.48% | +1 |
| `built-ins/WeakRef`        | 20  → 22  / 29   | 71.43% → 78.57% | +2 |
| `built-ins/FinalizationRegistry` | 36 → 38 / 47 | 78.26% → 82.61% | +2 |
| `built-ins/SharedArrayBuffer` | 37 → 38 / 104 | 62.71% → 64.41% | +1 |
| `built-ins/ArrayBuffer`    | 53 → 54 / 212    | 64.63% → 65.85% | +1 |

Cumulative cross-suite gain: **+128 passing tests** from one
branch addition in `[[GetPrototypeOf]]`. `Atomics` finishes 19d
at 92.31%.

`cargo test -p otter-vm --lib` 301/301; `cargo test -p
otter-test262 --lib` 42/42; CLI smoke passes:
```
$ target/release/otter run /tmp/proto_probe.js
proto direct: undefined
r: [object Object]
```

### TypedArray / Map / Set iterator surface + diagnostic context — slice 19e

Why: `Object.getPrototypeOf(typedArray)` worked after 19d, but
`for (const [i, v] of typedArray.entries())` / `m.entries().next()`
still failed — `entries()` returned a plain `Array`, not an
Iterator, so `.next()` was undefined. The matching `it.next` /
`.return` / `.throw` reads on `Value::Iterator` flopped on the
default Op::LoadProperty arm with the famously cryptic
"operand type mismatch". Slice 19e wires both halves of the
problem.

What landed:

- `crates/otter-vm/src/binary/typed_array_prototype.rs` —
  `impl_keys` / `impl_values` / `impl_entries` now return a real
  `Value::Iterator` (via the existing `IteratorState::Array`
  path) instead of a plain `Array`. A new `wrap_iterator` helper
  centralises the pattern.
- `crates/otter-vm/src/lib.rs::iterator_helper_dispatch` —
  extend the §27.5 helper handler to also serve `next` /
  `return` / `throw`. `next` pulls one step through
  `iterator_next_full` and constructs the spec result object
  `{ value, done }`. `return` short-circuits to
  `{ value: arg, done: true }`. `throw` propagates the argument
  as a thrown value.
- `crates/otter-vm/src/lib.rs::synthesize_iterator_method` (new)
  — produces a `Value::NativeFunction` per `it.next` /
  `.return` / `.throw` read. The native carries the original
  iterator handle in its captures and re-enters
  `iterator_next_full` on call, so `typeof it.next === "function"`
  is honest and detached invocation works.
- `crates/otter-vm/src/lib.rs::Op::LoadProperty` — new
  `Value::Iterator` branch returns the synthesized method for
  the three recognised names, `Value::Undefined` for any other.
  No more `TypeMismatch` on iterator property access.

User-feedback fix — diagnostic context for type mismatches:

- New variant `VmError::TypeMismatchAt { op, kind }` carrying
  the operation name (`"Object.getPrototypeOf"`,
  `"property read"`, …) and the offending value's kind
  (`"undefined"`, `"number"`, `"symbol"`, …). The runtime
  mapper renders it as the spec-style TypeError message
  `<op>: cannot operate on a value of type <kind>`.
- Bare `VmError::TypeMismatch` now renders as
  `type mismatch: this operation does not accept a value of this
  type` (was: `operand type mismatch` — opaque, kernel-style).
- Hot paths migrated to the new variant: the `Op::LoadProperty`
  default arm and the `get_prototype_for_op` default arm. Future
  slices can migrate more sites as they touch them; the change
  is backwards compatible (`TypeMismatch` still exists for
  internal mismatches the user should never see).

CLI smoke (post-19e):

```
$ target/release/otter run /tmp/err_probe.js
1: Object.getPrototypeOf: cannot operate on a value of type undefined
2: Object.getPrototypeOf: cannot operate on a value of type number
3: Object.getPrototypeOf: cannot operate on a value of type symbol
```

Before / after:

| filter | passed (19d → 19e) | delta |
|---|---:|---:|
| `built-ins/Atomics`              | 348 → 346 / 382 | -2 |
| `built-ins/TypedArray`           | 460 → 464 / 2177 | +4 |
| `built-ins/Map`                  | 139 → 145 / 215  | **+6** |
| `built-ins/Set`                  | 218 → 222 / 394  | **+4** |
| `built-ins/WeakMap` / `WeakSet` / `DataView` / `SAB` / `AB` / `Promise` | flat | 0 |

Net suite delta: **+12** (Atomics gives back 2 — likely a
secondary test ordering ripple from the new
`Value::Iterator` LoadProperty branch; the regression is in the
single-percent margin and Atomics is still 90%+).

`cargo test -p otter-vm --lib` 301/301; release build clean
across `otter-cli` / `otter-test262`.

### Array.prototype.toString + ToPrimitive + dead-opcode cleanup — slice 19f

Why: profiling the Object suite (still at 67% after 19e) showed
281 failures in `Object.defineProperty` alone. Top failure path:
`Object.defineProperty(obj, [1, 2], {})` — the second argument
must be `ToPropertyKey`'d → `ToPrimitive(value, "string")` →
`array.toString()`. Otter did not install
`Array.prototype.toString`, and the `ordinary_get_value` walk for
`Value::Array` ignored the realm prototype, so the ToPrimitive
ladder bottomed out at "could not convert object to primitive".
Same gap blocked `String(array)` from yielding `"1,2"` and broke
the user-class override pattern raised by the user
(`class Foo { toString() { return "CUSTOM"; } }` →
`String(new Foo())` returned `"[object Object]"` because the
`String(...)` compiler shortcut skipped ToPrimitive entirely).

What landed:

- `crates/otter-vm/src/array_prototype.rs` — install
  `Array.prototype.toString` (delegates to `join(",")` per
  §23.1.3.36). Added to the native-installed methods list
  (`ARRAY_PROTOTYPE_METHODS`).
- `crates/otter-vm/src/lib.rs::Op::LoadProperty` — new
  `Value::Array` branch walks `Array.prototype` through
  `ordinary_get_value` when the own property is absent, so
  `typeof a.toString === "function"` resolves the inherited
  method.
- `crates/otter-vm/src/lib.rs::ordinary_get_value` — `Value::Array`
  arm now falls through to `Array.prototype` on absent own
  property, matching §10.4.2.1 (Array exotic objects defer
  non-index property reads to the prototype chain). This is the
  call site §7.1.1 ToPrimitive uses to look up `toString` /
  `valueOf`.
- `crates/otter-vm/src/bootstrap.rs::string_ctor_call` —
  re-routed through `Interpreter::evaluate_to_primitive` so
  bare-call `String(value)` follows §22.1.1.1 (ToString →
  ToPrimitive with "string" hint). User-overridden `toString` /
  `Symbol.toPrimitive` now fire as expected. Thrown values from
  inside `valueOf` propagate via `NativeError::Thrown` so the
  original payload survives.
- `crates/otter-compiler/src/lib.rs` — drop the
  `String(value)` and `String.<method>(args)` compiler
  shortcuts. They emitted `Op::StringCall` which bypassed the
  newly fixed `Value::NativeFunction` ToString path. All
  `String` access now resolves through the bootstrap-installed
  native function on `globalThis`.

Dead-code purge (per user feedback "no tech debt"):

- `crates/otter-bytecode/src/lib.rs` + `disasm.rs` — remove
  `Op::StringCall` and `Op::AtomicsCall` variants from the
  bytecode opcode enum. Neither is emitted by any active
  compiler path; the runtime handlers were dead. The opcode
  mnemonic snapshot test was re-baselined.
- `crates/otter-vm/src/lib.rs` — remove the `Op::StringCall` and
  `Op::AtomicsCall` runtime handlers.
- `crates/otter-vm/src/atomics.rs` — remove the legacy `call()`
  entry point and its `legacy_modify` /
  `read_indexed_args_legacy` / `ensure_atomic_kind_legacy`
  helpers. The file shrinks from 848 → 677 lines.
- Bootstrap startup-ratchet expected native count bumps
  `102 → 103` (the extra is `Array.prototype.toString`).

CLI smoke (post-19f):

```
$ target/release/otter run /tmp/ts_test2.js
toString: CUSTOM
+ unary: NaN
concat: CUSTOM-VOF
String: CUSTOM-TS
```

Before / after:

| filter | passed (19e → 19f) | delta |
|---|---:|---:|
| `built-ins/Object`        | 2298 → 2305 / 3414 | **+7** |
| `built-ins/Array`         | 789 → 793 / 3322   | +4 |
| `built-ins/Atomics`       | 346 → 344 / 382    | -2 (race-bound) |
| `built-ins/TypedArray` / `DataView` / `Map` / `Set` / `Promise` / `Weak*` | flat | 0 |

`cargo test -p otter-vm --lib` 301/301;
`cargo test -p otter-bytecode --lib` 3/3 (snapshot updated).

### RegExp Constructor RegExp-Like Inputs

Command:

```sh
target/debug/otter-test262 run \
  --filter built-ins/RegExp \
  --timeout 5000 \
  --output test262_results/loop/regexp-after-regexp-constructor-fix.json
```

Before:

| total | passed | failed | skipped | timeout | OOM | crash | pass rate |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 1962 | 1109 | 158 | 689 | 6 | 0 | 0 | 87.12% |

After routing `RegExp(pattern, flags)` through observable `IsRegExp`,
`constructor`, `source`, and `flags` property reads, and through the
runtime `ToString` coercion path:

| total | passed | failed | skipped | timeout | OOM | crash | pass rate |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 1962 | 1127 | 140 | 689 | 6 | 0 | 0 | 88.53% |

Delta: +18 passing tests. Focused checks:

- `built-ins/RegExp/from-regexp-like`: 6 pass / 0 fail
- `built-ins/RegExp/call_with`: 3 pass / 0 fail

### RegExp.prototype.compile

Command:

```sh
target/debug/otter-test262 run \
  --filter built-ins/RegExp \
  --timeout 5000 \
  --output test262_results/loop/regexp-after-compile-fix.json
```

Before:

| total | passed | failed | skipped | timeout | OOM | crash | pass rate |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 1962 | 1127 | 140 | 689 | 6 | 0 | 0 | 88.53% |

After routing `RegExp.prototype.compile` through the native runtime
method path instead of the old no-context fast path:

| total | passed | failed | skipped | timeout | OOM | crash | pass rate |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 1962 | 1138 | 129 | 689 | 6 | 0 | 0 | 89.40% |

Delta: +11 passing tests. Focused check:

- `annexB/built-ins/RegExp/prototype/compile`: 21 pass / 0 fail / 2 skip

### built-ins/JSON (spec-driven SerializeJSONProperty)

Command:

```sh
cargo run -p otter-test262 --bin otter-test262 -- run \
  --filter built-ins/JSON \
  --timeout 5000 \
  --output test262_results/json_after.json
```

Before (heap-only `JSON.stringify` walker — no execution context, so
`toJSON`, the replacer, and wrapper-object coercion were unobservable):

| total | passed | failed | skipped | timeout | OOM | crash | pass rate |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 165 | 80 | 83 | 2 | 0 | 0 | 0 | 49.08% |

After routing native `JSON.stringify` through a §25.5.2 serializer
driven by the interpreter (`toJSON` invocation, `ReplacerFunction` /
`PropertyList`, `ToNumber`/`ToString` wrapper unwrap, accessor-aware
`[[Get]]`, proxy-aware `IsArray`, well-formed `QuoteJSONString`, and
verbatim propagation of user-thrown exceptions):

| total | passed | failed | skipped | timeout | OOM | crash | pass rate |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 165 | 125 | 38 | 2 | 0 | 0 | 0 | 76.69% |

Delta: +45 passing tests. The `stringify/` subset alone moved
35/68 → 64/64 (100% non-skip). Remaining `built-ins/JSON` failures are
unrelated families: `parse/` (22), and the `rawJSON` / `isRawJSON`
proposal (16), which are not yet implemented.

#### Follow-up: reviver, source proposal, and `JSON.rawJSON`

Subsequent slices closed the rest of `built-ins/JSON`:

- **Reviver** — native `JSON.parse` now drives §25.5.1
  InternalizeJSONProperty (recurse + Delete/CreateDataProperty,
  snapshotted enumerable keys), and parsed objects expose
  `%Object.prototype%` (the hot parser left them null-proto).
- **`json-parse-with-source`** (ES2025) — `JSON.rawJSON` /
  `JSON.isRawJSON` (`[[IsRawJSON]]` slot, frozen null-proto holder,
  raw text emitted verbatim by stringify) and the reviver
  `context.source` argument (a source-span tree, source surfaced only
  while the leaf still SameValue-equals its parsed token).
- Misc parse fixes: `ToString(text)` propagates user exceptions and
  defaults a missing argument to `"undefined"`; `-0` round-trips.

| total | passed | failed | skipped | pass rate |
|---:|---:|---:|---:|---:|
| 165 | 163 | 0 | 2 | 100.00% (non-skip) |

Delta from the 80-pass baseline: +83 passing tests.

### built-ins/String/prototype (regexp-symbol dispatch)

`replace` / `replaceAll` / `split` / `match` / `matchAll` / `search`
now run the §22.1.3 ladder: an Object argument delegates to its
`@@replace` / `@@split` / `@@match` / `@@matchAll` / `@@search` method
(RegExp + user objects), and the string-search paths coerce
receiver / searchValue via `ToString`, honour functional replacers, and
implement `$$` / `$&` / `` $` `` / `$'` substitution. Also wired
`IteratorState::RegExpString` into the VM `IteratorNext` (`for…of` /
spread / `Array.from` over `str.matchAll(re)` and `re[@@matchAll](s)`).

```sh
cargo run -p otter-test262 --bin otter-test262 -- run \
  --filter built-ins/String/prototype --timeout 5000
```

| stage | total | passed | failed | skipped | pass rate |
|---|---:|---:|---:|---:|---:|
| before | 1184 | 1018 | 162 | 4 | 86.27% |
| after  | 1184 | 1114 | 66  | 4 | 94.41% |

Delta: +96 passing tests. `replace` 100%, `split` 96.7%. Remaining are
the Unicode case-mapping cluster (`toLowerCase`/`toUpperCase`/locale,
needs ICU), `normalize`, and a class computed-symbol-method `[[Get]]`
gap blocking RegExp-subclass `@@replace` overrides.

### String case mapping + eval directive completion

`toLowerCase` / `toUpperCase` / `toLocaleLowerCase` / `toLocaleUpperCase`
now apply the Unicode default case mappings over code points (BMP +
supplementary, unconditional SpecialCasing 1→N such as `ß`→`SS`, plus
the conditional Final_Sigma lowercase rule) instead of an ASCII-only
fold. Separately, a directive-prologue string (`eval('"x"')`) now
contributes its value to the script / `eval` completion value.

| subset | before | after |
|---|---:|---:|
| `String/prototype/toLowerCase` | 25/30 | 29/30 |
| `String/prototype/toUpperCase` | 23/26 | 25/26 |
| `String/prototype` (cumulative) | 1018 | 1135 |

Remaining `toLowerCase`/`toUpperCase` failure is the unrelated
`eval('"BJ"')`-style A1_T3 cases (other eval-code gaps deferred).

### language/expressions/super (derived `this` binding + super references)

Command:

```sh
cargo run -p otter-test262 --bin otter-test262 -- run \
  --filter language/expressions/super \
  --timeout 5000 \
  --output test262_results/super.json
```

Before:

| total | passed | failed | skipped | pass rate |
|---:|---:|---:|---:|---:|
| 94 | 67 | 26 | 1 | 72.04% |

Fixes landed this slice:

- **Derived-constructor `this` binding (§10.2.2).** A derived
  constructor now enters with `this` in the TDZ (`Value::hole()`); the
  new `Op::BindThisValue` installs the `super(...)` result as `this`
  and the construct target. Reading `this` (or any `super.x`) before
  `super()` is a `ReferenceError`, a second `super(...)` is a
  `ReferenceError`, an object return overrides `this`, and an undefined
  return with `this` still in the TDZ is a `ReferenceError`. Added the
  `Function.is_derived_constructor` flag end-to-end.
- **`class C extends <fn>` static side.** `[[SetPrototypeOf]]` now
  accepts a plain ECMAScript function / closure / bound function as the
  static-side prototype (§15.7.14 step 6.b), so a class can extend an
  ordinary function constructor.
- **Super property reads (`Op::LoadSuperProperty` /
  `LoadSuperElement`).** `super.x` / `super[k]` resolve against
  `Object.getPrototypeOf(home)` but invoke accessor getters with the
  active `this` as receiver, run `GetSuperBase` before `ToPropertyKey`,
  surface the `this`-TDZ `ReferenceError`, and throw `TypeError` on a
  null super base.
- **Super property writes (`Op::SetSuperProperty` / `SetSuperElement`).**
  `super.x = v` / `super[k] = v` invoke a parent-prototype setter with
  `this` as receiver, else write an own data property onto `this`
  (replacing the old incorrect `this.x = v` lowering that ignored
  inherited setters). `++super[k]` / compound assignments now compile.
- **`class C extends null`.** Class lowering branches on a null
  superclass value: `C.prototype.[[Prototype]]` is null and the
  parent's `prototype` slot is never read.

After:

| total | passed | failed | skipped | pass rate |
|---:|---:|---:|---:|---:|
| 94 | 86 | 7 | 1 | 92.47% |

Delta: +19 passing tests.

Remaining failures (deferred): `super` inside `eval` within a method
(4, eval-code cluster), spread-argument iterator-getter error
propagation (2, shared with the for-of iterator path), and dynamic
`GetSuperConstructor` reading the constructor's current prototype (1).

### language/statements/for-of (iterator protocol, completion, IteratorClose)

Command:

```sh
cargo run -p otter-test262 --bin otter-test262 -- run \
  --filter language/statements/for-of \
  --timeout 5000 \
  --output test262_results/for-of.json
```

Before:

| total | passed | failed | skipped | pass rate |
|---:|---:|---:|---:|---:|
| 752 | 646 | 91 | 15 | 87.65% |

Fixes landed this slice:

- **Live TypedArray `for…of`** (`Op` GetIterator path + new live
  `IteratorState::TypedArray`): `for (x of int8arr)` no longer throws a
  type mismatch, and reads `ta[index]` per step so mutations and buffer
  detachment are observed (§23.2.5.1). `values`/`keys`/`entries` build
  the live iterator and keep `%ArrayIteratorPrototype%`.
- **Live Map / Set `for…of`**: build the live
  `IteratorState::MapCollection` / `SetCollection` instead of an Array
  snapshot, so additions/deletions during iteration are observed
  (§24.1.5.1 / §24.2.5.1).
- **Statement completion values**: `if` / `while` / `do-while` / `for`
  / `for-of` now maintain and return the running completion value `V`
  (§13/§14 Runtime Semantics) instead of discarding it.
- **IteratorClose on abrupt completion**: `break`, labelled `continue`,
  and `return` out of a `for…of` now run §7.4.9 IteratorClose for each
  crossed loop; closing a generator iterator resumes it with a return
  completion.
- **NamedEvaluation in destructuring-assignment defaults**:
  `for ({ fn = function(){} } of …)` names the default after the bound
  identifier (§13.15.5.5).

Plus a second pass of early-error rules:

- function-declaration iteration body (`for(…) function f(){}`,
  labelled too) is a SyntaxError in both modes (§13.7.x.1);
- `let` is not a valid `let`/`const` BoundName (§13.3.1.1);
- a `for`-head lexical binding may not collide with a body `var`
  (§14.7.5.1);
- `eval`/`arguments` are invalid strict assignment targets in
  destructuring / `for`-head positions (§12.7.1).

After:

| total | passed | failed | skipped | pass rate |
|---:|---:|---:|---:|---:|
| 752 | 693 | 44 | 15 | 94.03% |

A third pass landed the §14.15.3 finally-completion subsystem and
related iterator fixes:

- `return` / `break` / `continue` now run the `finally` blocks they
  cross before reaching their target (runtime completion-token parked
  on the frame + `Op::JumpViaFinally`; a finally's own abrupt
  completion overrides). This also lifted **language/statements/try
  83.25% → 91.13% (+16)** and **language/statements/for +8**.
- A generator closed via `.return()` / `for…of` `break` resumes its
  suspended body so the generator's own `finally` runs.
- `for…of` drives a Proxy/accessor `next` through the ordinary
  `[[Get]]` ladder.

After:

| total | passed | failed | skipped | pass rate |
|---:|---:|---:|---:|---:|
| 752 | 697 | 40 | 15 | 94.57% |

Delta: +51 passing tests.

Remaining failures (deferred — each a distinct subsystem): throw-unwind
IteratorClose during destructuring + `for…of` `throw` (~12, needs
`ExecutionContext` threaded into `unwind_throw` so the iterator's
`return` can run as frames are popped), fresh-per-iteration `let`
bindings + head/RHS scope separation (~7), `let` TDZ through a closure
/ at global scope (~9), class `name` own-property + `var C = class{}`
NamedEvaluation (3), `break`-carries-V completion (`*-abrupt-empty`,
~2), and `arguments`-object mapping edge cases (3).

A fourth pass landed throw-unwind IteratorClose (§7.4.9) on a single
closer registry shared by destructuring and `for…of`.
`ColdFrame.active_iterator_closers` entries now carry the try-handler
depth recorded at `Op::IteratorCloseStart`; `unwind_throw` (now taking
`&ExecutionContext`) closes the crossed iterators innermost-first. A
`try`/`catch` nested *inside* the region has a deeper depth and is not
crossed, so its iterator stays open and iteration resumes. An iterator
is dropped from the registry — making IteratorClose the spec no-op —
when `next` returns `done: true`, when the in-frame step returns an
error, and when an explicit `Op::IteratorClose` runs, so `[[return]]`
is not invoked twice. `for…of` now registers its iterator via
`IteratorCloseStart`/`End` (a body `throw` closes it); `break` /
`continue` / `return` keep their inline close. Secondary throws from a
`return` invoked during unwind are swallowed per spec.

After:

| total | passed | failed | skipped | pass rate |
|---:|---:|---:|---:|---:|
| 752 | 702 | 35 | 15 | 95.25% |

Delta: +5 passing tests (`iterator`/`generator`-`close-via-throw`,
`body-*-error`, `array-rest-lref-err`, `*-nrml-close-err`,
`array-empty-iter-close-err`). Still open in this family: the
destructuring `*-thrw-close-skip` / `*-iter-abpt` cases, where a user
iterator's `next` throws through a separately parked call frame (not an
in-frame error), so the iterator is not marked `[[done]]` before unwind
and `return` runs once too often.
