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
microtask enqueueing. **158/158 engine fixtures pass.**

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
5. **Interpreter only.** No JIT in any task in this pool.
6. **LLM-friendly module docstrings.** Every Rust file in
   `crates-next/*` opens with `//! Summary / Contents / Invariants /
   See also` (ADR-0001 §6).
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

> **No GC during foundation.** Foundation goal is full ES spec
> coverage on the simple `Rc` value model. GC + JIT each ship as
> their own dedicated plan + crate **after** spec coverage is
> solid. Phase F tasks (34, 35, 36) all ship on `Rc` for now;
> task 57 is the placeholder for the eventual GC plan and is
> not on the critical path of foundation work.

| File | One-line goal |
|---|---|
| [35-async-await.md](./35-async-await.md) | `async` functions, `await`, async-call frame state machine. |
| [36-modules-import-export.md](./36-modules-import-export.md) | ES modules, `import` / `export`, dynamic `import()`, `import.meta`. |

### Phase G — modern surfaces (later)

| File | One-line goal |
|---|---|
| [37-symbol-and-well-known-symbols.md](./37-symbol-and-well-known-symbols.md) | `Symbol` value, well-known symbols, symbol-keyed properties. |
| [38-map-set-and-weak-collections.md](./38-map-set-and-weak-collections.md) | `Map`, `Set`, `WeakMap`, `WeakSet`. |
| [39-temporal.md](./39-temporal.md) | `Temporal.*` modern date / time API. |
| [40-intl.md](./40-intl.md) | `Intl.*` (Collator, NumberFormat, DateTimeFormat). |

### Infrastructure / ratchets (parallel to the above)

| File | One-line goal |
|---|---|
| [50-criterion-bench-suite.md](./50-criterion-bench-suite.md) | First Criterion bench targets covering call overhead, integer loops, string concat, property load. |
| [51-test262-curated-subset.md](./51-test262-curated-subset.md) | `otter test --suite test262` wired into CI; first conformance baseline recorded. |
| [52-trace-events-emission.md](./52-trace-events-emission.md) | Wire `vm.instruction` / `vm.call` / `vm.return` events through the trace sink. |
| [53-recreate-es-conformance.md](./53-recreate-es-conformance.md) | Recreate `ES_CONFORMANCE.md` once the curated test262 subset reports a stable baseline. |
| [55-otter-macros-next.md](./55-otter-macros-next.md) | New `otter-macros-next` proc-macro crate (`#[js_method]`, `js_proto!`, `#[js_namespace]`); migrate string / array / number / math / regexp prototype tables. |
| [56-remove-refcell-from-hot-paths.md](./56-remove-refcell-from-hot-paths.md) | Remove `RefCell` from every hot path in `crates-next/*`; replace with `&mut` field access threaded through `dispatch_loop`. Required before task 35 (async) lands. |
| [57-tracing-gc-migration.md](./57-tracing-gc-migration.md) | **HIGH PRIORITY.** Write our own tracing GC (no `boa_gc`) and replace every `Rc<T>` / `Rc<RefCell<T>>` in `crates-next/*` with a GC-managed handle. No production JS engine uses refcounting; we won't be competitive with V8 / JSC / SpiderMonkey until this is done. Sequence *between Phase F basics and task 35 (async with worker threads)*. |

### One-off cleanup follow-ups

| File | One-line goal |
|---|---|
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
