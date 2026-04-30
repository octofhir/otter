# Otter New Engine — Active Task Pool

This directory holds the **currently open** tasks. Completed work
gets removed once it ships; the foundation phase tasks (07–13) have
already shipped and were deleted from this index. The new engine in
`crates-next/*` runs JS / TS scripts end-to-end with strings,
numbers, booleans, control flow, locals, function declarations,
recursive calls, objects, arrays, closures with captured upvalues,
`this` binding, method calls, `Function.prototype.{call, apply,
bind}`, `throw` / `try` / `catch` / `finally`, a foundation
`Error` constructor, the iterator protocol with `for…of` plus
spread in array literals and calls, `class` declarations with
`extends`, `super`, instance methods, and static members,
default / rest / destructuring parameters (and matching `let`
destructuring bindings), bitwise + `**` operators with all
compound-assignment shapes, `Number.prototype.{toString, toFixed}`,
the `Math` namespace (constants + abs/min/max/floor/ceil/round/
trunc/sqrt/pow), and `BigInt` literals with arbitrary-precision
arithmetic, bitwise ops, and spec-correct cross-kind coercion. The
full `String.prototype` foundation surface is in (`replace` /
`replaceAll` / `split` / `repeat` / `padStart` / `padEnd` / `trim*`
/ `at` / `codePointAt` / `toLowerCase` / `toUpperCase` / `concat`
/ `includes` / `match` / `matchAll` / `search`), and JS regex
literals are wired end-to-end: a `Value::RegExp` backed by the
`regress` engine (octoshikari fork), `RegExp.prototype.{exec, test,
toString}` plus `source` / `flags` / `lastIndex` accessors, the
six standard flags (`g` / `i` / `m` / `s` / `u` / `y`), and the
regex-arg overloads of every `String.prototype` pattern method
including `$$` / `$&` / `$1`–`$9` substitution. The `JSON`
namespace is implemented with a hand-rolled (no `serde_json`)
strict parser and an iterative `stringify` walker — insertion-
order preserved, `NaN`/`±Infinity` → `null`, BigInt + cycles +
1024-deep nesting all surface as catchable runtime errors, and
the `space` parameter accepts both numeric and string indents.
The microtask queue is in: a per-`Interpreter` `MicrotaskQueue`
(plain `&mut`-owned field, no `RefCell`/`UnsafeCell`),
`queueMicrotask(fn, ...args)` global, swap-and-drain semantics
with reentrant-depth tracking, generation counter, and a
cross-thread `AsyncRuntime` trait skeleton + optional
`crossbeam_channel` inbox ready for task 35 (async/await) to
plug in. `Otter::run_*` auto-drains after every script. The
`Promise` value is in: a `JsPromise` trait (the contract) plus
a concrete `JsPromiseHandle` tagged enum (`PurePromise` today,
host-bridged variants in Phase F) — no vtable indirection on
the hot path. Constructor + `Promise.{resolve, reject, all,
race}` statics + `.then`/`.catch`/`.finally` prototype methods
all wire through `Microtask::result_capability` so the handler's
return value flows into the next promise (chained `.then`
works). `Value::NativeFunction` lands as part of this slice —
host-implemented callables for `resolve` / `reject` /
aggregator-functions, with `&mut Interpreter` access for
microtask enqueueing. `async` functions / `await` / async-arrow
lowering ship on top of the same machinery: each `await` parks
the running async frame, attaches resume / reject native
reactions to the awaited promise, and resumes via a fresh
`MicrotaskKind::AsyncResume` task that re-pushes the parked
frame and continues from the next pc; throws inside an async
body settle the result promise as rejected, and `await` of a
rejected promise re-enters as a synchronous throw so existing
`try`/`catch`/`finally` shapes still work. ES modules are wired
end-to-end on a relative-path loader: one linked `BytecodeModule`
per program, post-order DFS evaluation, synthesised `<entry>`
driver, per-module `module_env` JsObject holding live bindings
(every importer-side read goes through `LoadProperty` so write
propagation is automatic), `import.meta`, literal-string
`import("./x")` resolved at compile time, cyclic graphs caught
with a `RangeError`-shaped diagnostic, multi-file fixtures via
the `_*` helper-directory convention. npm / `node_modules` /
workspace resolution layers on top via `oxc_resolver`-backed
bare-specifier resolution: `import x from "lodash"`, `@scope/pkg`
packages, `npm:` sugar prefix, walk-up `node_modules`,
conditional `exports` maps with ESM / CJS condition names,
configurable through `RuntimeBuilder::module_loader`. The `Intl.*`
namespace ships three constructors backed by ICU 4X
(`compiled_data` features, ~3 MiB binary cost): `Intl.Collator`
(locale-aware `compare`), `Intl.NumberFormat` (`format` for
`decimal` / `currency` / `percent`, `useGrouping`,
`minimumFractionDigits` / `maximumFractionDigits`,
`resolvedOptions`), and `Intl.DateTimeFormat` (`format` accepting
both epoch-ms numbers and `Temporal.{Instant,PlainDate,PlainDateTime}`,
ECMA-402 default option bag of `{year,month,day}` when no
component options were given). Locale resolution falls back to
`"en-US"` when the requested tag is unknown. The module is split
per-class: `crates-next/otter-vm/src/intl/{mod, payload, dispatch,
helpers, collator, number_format, date_time_format}.rs`. New
opcode `Op::NewIntl` + compiler interception lower
`new Intl.<Class>(locale?, options?)` directly. The `Symbol`
primitive ships with all 13 well-known symbols (asyncIterator,
hasInstance, isConcatSpreadable, iterator, match, matchAll, replace,
search, species, split, toPrimitive, toStringTag, unscopables),
`Symbol.for` / `Symbol.keyFor` registry round-trip, symbol-keyed own
properties on plain objects, the `typeof` operator returning
`"symbol"`, `arr[Symbol.iterator]()` returning a foundation iterator
factory, and `[Symbol.toPrimitive]` consultation from the unary `+`
operator. The four collection built-ins ship with insertion-order
semantics: `Map` / `Set` (`IndexMap`-backed, full prototype methods
+ `size` accessor + `forEach` callback dispatch + `keys`/`values`/
`entries` iterators) and `WeakMap` / `WeakSet` (object-keyed,
strong-ref today with the GC-driven weak eviction tracked under task
57). `for…of` over a `Map` walks `[key, value]` pairs and over a
`Set` walks values; `[...new Set(arr)]` dedupes; `new Map(iter)` /
`new Set(iter)` / `new WeakMap(iter)` / `new WeakSet(iter)` seed
from an array iterable; `Map` / `Set` use ECMA-262 SameValueZero
key matching so `+0`/`-0` collapse and `NaN` matches itself.
The `Temporal` proposal lands as a thin glue over `temporal_rs`
(octoshikari fork): one `Value::Temporal` variant carries a
typed payload (`Instant` / `Duration` / `PlainDate` / `PlainTime`
/ `PlainDateTime`); the namespace is split into per-type files
(`crates-next/otter-vm/src/temporal/{instant,duration,plain_date,
plain_time,plain_date_time,now,helpers,payload,dispatch,mod}.rs`)
with one prototype `IntrinsicTable` per kind; `Op::TemporalCall`
+ compiler interception lower `Temporal.<Class>.<method>(...)`
straight to `temporal_rs` so the runtime needs no global object;
component accessors flow through `Op::LoadProperty`; ISO calendar
+ host time-zone supported, non-ISO calendars / `ZonedDateTime` /
`PlainYearMonth` / `PlainMonthDay` filed as follow-ups.
**286/286 engine fixtures pass.**

The §7.2 type-check abstract operations have shipped as canonical
helpers (task 43): `same_value` / `same_value_zero` /
`same_value_non_numeric` / `is_array` / `is_callable` /
`is_constructor` live in `crates-next/otter-vm/src/abstract_ops.rs`
with full ECMA-262 spec links, and `Object.is(x, y)` /
`Array.isArray(v)` lower through dedicated `Op::SameValue` /
`Op::IsArray` opcodes.

The §19.3 / §20.5 native error class hierarchy shipped (user
surface) as task 57: a per-interpreter `ErrorClassRegistry` holds
the seven canonical classes (`Error`, `TypeError`, `RangeError`,
`SyntaxError`, `ReferenceError`, `URIError`, `EvalError`) with
proper prototype chains; new opcodes `Op::NewBuiltinError` /
`Op::LoadBuiltinError` build instances and load constructors;
the compiler intercepts `new <Kind>(msg)` / bare-identifier reads
of all seven names; `Op::Instanceof` was widened to read
`rhs.prototype` per §13.10.2 OrdinaryHasInstance. Implicit
runtime failures (`VmError::TypeMismatch` / `NotCallable` /
`TemporalDeadZone` / `UnknownIntrinsic`) are now converted into
typed `Error` instances by `Interpreter::vm_error_to_throwable`
and routed through `unwind_throw` so `try { ... } catch (e) { e
instanceof TypeError }` catches the same shape it would in any
spec-conforming engine.

The §7.4 iterator-protocol consultation shipped as task 79:
`IteratorState::User { iterator: Value }` plus multi-stage
`Op::GetIterator` and `Op::IteratorNext` ladders that call
`obj[@@iterator]()` and `iter.next()` on user objects and unpack
`{ value, done }` from the returned record. `for…of`, array
spread, and call-site spread all consult the user protocol.

The §7.2.13 / §7.2.14 loose-equality + relational comparison
operators shipped as task 42: `abstract_ops::is_loosely_equal` +
`abstract_ops::abstract_relational_comparison` (with the
`RelationalOutcome` enum); two new `Op::LooseEqual` /
`Op::LooseNotEqual` opcodes; `==` / `!=` in source code maps to
them; the compiler emits `Op::ToPrimitive(default)` before loose
equality and `Op::ToPrimitive(number)` before `<` / `<=` / `>` /
`>=`; `run_compare` now drives the §7.2.14 ladder.

The §23.1 Array completion shipped as task 73: pure-functional
methods (`at`, `lastIndexOf`, `reverse`, `fill`, `flat`, `splice`,
default `sort`) extend `array_prototype.rs`'s intrinsic table;
callback-driven methods (`forEach`, `map`, `filter`, `reduce`,
`reduceRight`, `find`, `findIndex`, `every`, `some`, `flatMap`, and
comparator-driven `sort`) intercept in `do_call_method_value` and
dispatch the user callbacks synchronously via `run_callable_sync`.
New `Op::ArrayCall` opcode + `crates-next/otter-vm/src/array_statics.rs`
lower `Array.from` (Array / Set / Map / String iterables) and
`Array.of`; `Array.isArray` keeps its dedicated `Op::IsArray`.

The §14.12 / §14.7.5 / §14.13 control-flow trifecta shipped as task 51:
`compile_switch_statement` lowers SwitchStatement with strict-equality
dispatch + per-case fall-through + default; `compile_for_in_statement`
lowers ForInStatement (snapshots own enumerable keys via `Object.keys`
+ the iterator protocol — full chain enumeration is filed against a
follow-up); `compile_labeled_statement` plus a `pending_label` slot
on `FunctionContext` thread `break label;` / `continue label;` to the
matching enclosing loop or switch. Switch frames are pushed with
`LoopFrame::switch_body()` so `continue` skips them and targets the
enclosing real loop, matching §13.10.1.

The §20.2 / §20.1.3 Object static + prototype surface shipped as
tasks 60 + 61: `Op::ObjectCall` now routes `keys` / `values` /
`entries` / `assign` / `fromEntries` / `hasOwn` /
`getOwnPropertyNames` through `crates-next/otter-vm/src/object_statics.rs`;
ordinary-object method calls (`obj.hasOwnProperty(k)` /
`propertyIsEnumerable` / `isPrototypeOf` / `toString` / `valueOf`)
intercept in `do_call_method_value` when the user hasn't shadowed
the name. `Object.prototype` is not yet installed as a real prototype
chain — that lands when task 62 (Function.prototype completion)
forces the issue.

The §6.1.7.1 / §10.1.6 / §10.1.8 / §10.1.9 property-descriptor surface
shipped as task 44: `crates-next/otter-vm/src/object.rs` rebuilt around
`PropertyFlags` + `PropertyDescriptor` + `PropertyLookup` + `SetOutcome`;
`Op::LoadProperty` / `Op::StoreProperty` route through the OrdinaryGet /
OrdinarySet ladders so accessor getters and setters dispatch as call
frames; the `delete` operator honours `[[Configurable]]`; a new
`Op::ObjectCall` opcode + `crates-next/otter-vm/src/object_statics.rs`
lower `Object.defineProperty` / `defineProperties` /
`getOwnPropertyDescriptor` / `getOwnPropertyDescriptors` / `freeze` /
`seal` / `preventExtensions` / `isFrozen` / `isSealed` / `isExtensible`
through one variadic dispatcher (mirroring the existing
`Op::MathCall` / `Op::JsonCall` shape). `JSON.stringify` walks the new
`enumerable_data_iter` so non-enumerable own properties are skipped per
ECMA-262 §25.5.2.4.

The §7.1.1 ToPrimitive ladder shipped as task 41:
`abstract_ops::ToPrimitiveHint` + `is_primitive`, a new
`Op::ToPrimitive` opcode with a multi-stage frame-local state
machine (`Frame::pending_to_primitive`) that drives
`[Symbol.toPrimitive]` → `valueOf` → `toString` → `TypeError`,
and a compiler change that emits `Op::ToPrimitive(default)` before
each `Op::Add` operand. `run_add` widens to ECMA-262 §13.15.4
ApplyStringOrNumericBinaryOperator (string concat fires when
either post-coerced operand is a string).

Foundation artifacts that stay (not tasks, never deleted):

- [Foundation plan](../../../NEW_ENGINE_FOUNDATION_PLAN.md)
- [Repository map](../repository-map.md)
- [ADR-0001 — staging directory](../adr/0001-staging-directory.md)
- [ADR-0002 — OXC frontend](../adr/0002-oxc-frontend.md)
- [ADR-0003 — public API & CLI](../adr/0003-public-api-and-cli.md)
- [Spec — `otter test` harness](../specs/otter-test-harness.md)
- [Spec — bytecode dump / disasm / trace](../specs/bytecode-dump-disasm-trace.md)

## Working rules

1. **Write from scratch.** Every line under `crates-next/*` is new
   code. We do not migrate, port, or paste from `crates/*`. Tasks
   below describe the **surface** to reproduce, not where to copy
   from.
2. **Legacy stays on disk.** `crates/*` is excluded from the
   workspace (ADR-0001) and stays untouched until a corresponding
   `crates-next` slice ships and we are confident the new
   implementation supersedes it. We delete a legacy crate **only**
   when the new one fully replaces its surface — not before.
3. **Small steps, end-to-end every step.** Pick the next narrow
   slice, implement it through every layer (parser/compiler/
   bytecode/interpreter/public API/CLI/fixtures), run gates, close
   the task. No giant batches.
4. **OXC owns parsing.** No regex parsing of JS / TS, no parallel
   parser stack (ADR-0002).
5. **Interpreter only.** No JIT anywhere in this phase. Spec
   coverage first; performance work comes later in its own
   dedicated track.
6. **LLM-friendly module docstrings.** Every Rust file in
   `crates-next/*` opens with `//! Summary / Contents / Invariants /
   See also` (ADR-0001 §6). **ECMA-262 spec links are mandatory**
   on any module / function that implements a spec algorithm,
   intrinsic, or spec-mandated semantic — cite as
   `https://tc39.es/ecma262/#sec-<anchor>` in the docstring's
   `# See also` (or `# Algorithm`) block. Audit + back-fill of
   already-shipped code is task 59.
7. **Idiomatic Rust.** `thiserror` for error enums, `serde` derive
   for wire types, `SmallVec` for small inline collections, `?` for
   propagation, no `Box<dyn Error>` on the public API,
   `#[non_exhaustive]` on public enums, `Default` derive where it
   fits.
8. **Status updates and deletion.** Each task file has a `## Status`
   section. Update it as work progresses. When a task is finished
   and any leftovers are filed as separate follow-up tasks,
   **delete the task file** — this index reflects only open work.

## Open task pool

Order is "simple → complex". Each task file is small, narrow, and
ships independently end-to-end.

### Phase A — sharpening what already exists

✅ Phase A complete — see Phase B for the next batch.

### Phase B — the object model

✅ Phase B complete — see Phase C for the next batch.

### Phase C — closures, methods, exceptions

✅ Phase C complete — see Phase D for the next batch.

### Phase D — iterators and language essentials

✅ Phase D complete — see Phase E for the next batch.

### Phase E — number and string completion

✅ Phase E complete — see Phase F for the next batch.

### Phase F — promises, modules, async

> **No GC, no JIT during this phase.** The whole foundation
> stays on the simple `Rc` value model. GC and JIT each get their
> own architectural plan **after** ECMA-262 spec coverage is
> complete — they're explicitly out of scope here. Every Phase F
> / G / H task ships on `Rc`.

✅ Phase F complete — see Phase G for the next batch.

### Phase G — modern surfaces (later)

_Phase G complete._

### Phase H — ECMA-262 spec-gap closeout

After Phase G shipped, a section-by-section walk through ECMA-262
surfaced ~30 gap areas. **The current focus is maximal spec
coverage on the existing `Rc` value model** — GC and JIT are
deliberately deferred to their own dedicated tracks once spec
work is done. The master tracker is
**[41-spec-gap-audit.md](./41-spec-gap-audit.md)**;
individual task files split out as work begins. Headlines:

- §7 type-conversion ladder (ToPrimitive / IsLooselyEqual /
  AbstractRelationalComparison) — blocks `==`, mixed-type ops. ✅
- §9.4 property descriptors — blocks `Object.defineProperty` /
  `Object.freeze` / Reflect / Proxy / TypedArrays. ✅
- §19.3 Error hierarchy — `try/catch (e instanceof TypeError)`.
- §19.2 `globalThis` + parseInt/parseFloat/isNaN/isFinite/encodeURI*.
- §20.1 `Object.prototype` (`hasOwnProperty`, `toString` with
  `[Symbol.toStringTag]`). ✅ (foundation surface)
- §20.2 full `Object` static surface. ✅
- §21.1 Number completion + §21.2 BigInt + §21.3 Math + §21.4 `Date`.
- §22.1 String completion + §22.2 RegExp completion (named groups /
  symbol hooks / `v` flag) + code-point string iterator.
- §23.1 full `Array.prototype` (every/some/find/forEach/map/filter/
  reduce/sort/splice/at/keys/values/entries) + Array statics. ✅
- §23.2 / §25.1 / §25.3 TypedArrays + ArrayBuffer + DataView.
- §24.4 Atomics + SAB single-thread subset.
- §25.4 Promise.{allSettled, any, withResolvers, finally, species}.
- §27.5–6 Generators + async generators + `for await … of`.
- §28 Reflect + Proxy.
- §14.12 / §14.7.5 / §14.13 switch / for-in / labelled break. ✅
- §15.7 class fields + static blocks.
- §16.2 top-level await + JSON modules + non-literal dynamic import.
- ECMA-402 expansion: PluralRules / RelativeTimeFormat / ListFormat
  / DisplayNames / Segmenter.

### Infrastructure / ratchets (parallel to the above)

| File | One-line goal |
|---|---|
| [50-criterion-bench-suite.md](./50-criterion-bench-suite.md) | First Criterion bench targets covering call overhead, integer loops, string concat, property load. |
| [51-test262-curated-subset.md](./51-test262-curated-subset.md) | `otter test --suite test262` wired into CI; first conformance baseline recorded. |
| [52-trace-events-emission.md](./52-trace-events-emission.md) | Wire `vm.instruction` / `vm.call` / `vm.return` events through the trace sink. |
| [53-recreate-es-conformance.md](./53-recreate-es-conformance.md) | Recreate `ES_CONFORMANCE.md` once the curated test262 subset reports a stable baseline. |
| [54-harness-richer-assertions.md](./54-harness-richer-assertions.md) | Wire spec-already-defined `expect.value` / `expect.stdout_contains` / `expect.throws` into the engine-suite runner so fixtures stop relying on the `undefined.x` fail-trick. |
| [55-otter-macros-next.md](./55-otter-macros-next.md) | New `otter-macros-next` proc-macro crate (`#[js_method]`, `js_proto!`, `#[js_namespace]`); migrate string / array / number / math / regexp prototype tables. |
| [56-remove-refcell-from-hot-paths.md](./56-remove-refcell-from-hot-paths.md) | Remove `RefCell` from every hot path in `crates-next/*`; replace with `&mut` field access threaded through `dispatch_loop`. Required before task 35 (async) lands. |

> GC and JIT are explicitly **out of scope** for the current phase.
> They each get their own architectural plan + dedicated track once
> ECMA-262 spec coverage is complete. Don't file or merge tasks for
> them in this pool.

### Test262 conformance

A fresh task pool sliced out of the spec-gap audit's row 83. The §41
audit landed every spec gap on the active stack — this pool turns
the engine's surface into a published, regression-gated number
against the official [`tc39/test262`](https://github.com/tc39/test262)
corpus.

| File | One-line goal |
|---|---|
| [100-test262-conformance.md](./100-test262-conformance.md) | Master plan — source acquisition, crate skeleton, execution model, outputs, CI, rollout. |
| [101-test262-runner-skeleton.md](./101-test262-runner-skeleton.md) | New `crates-next/otter-test262` workspace member; `vendor/test262` submodule; walkdir traversal stub. |
| [102-test262-harness-and-metadata.md](./102-test262-harness-and-metadata.md) | Frontmatter parser; harness preamble (`assert.js` / `sta.js` / `includes`); feature-readiness map. |
| [103-test262-outcomes-and-negative.md](./103-test262-outcomes-and-negative.md) | Per-test driver; `Outcome` enum; negative-test inversion; heap cap + timeout + `catch_unwind`. |
| [104-test262-baseline-and-diff.md](./104-test262-baseline-and-diff.md) | JSON + Markdown reports under `docs/new-engine/test262-baseline/`; sharding + merge; `--diff` regression detector. |
| [105-test262-ci-integration.md](./105-test262-ci-integration.md) | GitHub Actions sweep + PR comment + regression gate; baseline-bump workflow. |

> The legacy `51-test262-curated-subset.md` (curated subset against
> the old `crates/otter-test262`) supersedes here — once 105 lands,
> file a deletion task for 51.

### One-off cleanup follow-ups

| File | One-line goal |
|---|---|
| [59-spec-link-audit-and-rule.md](./59-spec-link-audit-and-rule.md) | Make ECMA-262 deep links mandatory in module / function docstrings; back-fill audit on already-shipped code in `crates-next/*`. |
| [60-archive-superseded-root-docs.md](./60-archive-superseded-root-docs.md) | Move `PRODUCTION_READINESS_PLAN.md` / `TOOLING_ROADMAP.md` / `ROADMAP.md` / `gc_migration_baseline.md` into `docs/archive/`. |
| [61-delete-committed-results.md](./61-delete-committed-results.md) | Delete `test262_results/`, `benchmarks/results/`, `benchmarks/node_modules/`, `scratch/`, root one-off shell scripts; extend `.gitignore`. |

## Closing a task

Steps when a task is done:

1. Run gates: `cargo fmt --all`, `cargo clippy --workspace
   --all-targets --all-features -- -D warnings`,
   `cargo test --workspace`, `cargo run -p otter-cli --
   test --suite engine`.
2. If anything was deferred, file a follow-up task file (or an
   amendment to an open one) before closing.
3. Delete this task file.
4. Update this README's index entry.
