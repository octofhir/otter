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
| `built-ins/NativeErrors` | 78 |
| `built-ins/Error` | 32 |
| `built-ins/AggregateError` | 14 |
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
   so `allocate_for_bytecode` advances each record directly to
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
  --output test262_results/batch_object_gopn_after_primitives.json
```

Current:

| total | passed | failed | skipped | timeout | OOM | crash | pass rate |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 45 | 21 | 24 | 0 | 0 | 0 | 0 | 46.67% |

Delta from the immediate batch baseline
`test262_results/batch_object_gopn_after_native_function.json`: +4 passing
tests. The remaining failures cluster around Array instance/prototype
identity, richer object descriptor behavior, proxy invariants, and
additional ordinary object edge cases.
