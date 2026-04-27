# Otter New Engine — Active Task Pool

This directory holds the **currently open** tasks. Completed work
gets removed once it ships; the foundation phase tasks (07–13) have
already shipped and were deleted from this index. The new engine in
`crates-next/*` runs JS / TS scripts end-to-end with strings,
numbers, booleans, control flow, locals, function declarations, and
recursive calls. **39/39 engine fixtures pass.**

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

| File | One-line goal |
|---|---|
| [22-closures-with-upvalues.md](./22-closures-with-upvalues.md) | Captured-variable model so inner functions can read / mutate outer-scope locals. |
| [23-this-and-method-calls.md](./23-this-and-method-calls.md) | `this` binding, `obj.method()` calling, `Function.prototype.{bind,call,apply}`. |
| [24-throw-try-catch-finally.md](./24-throw-try-catch-finally.md) | `throw`, `try` / `catch` / `finally`, error objects, propagation through frames with diagnostics. |

### Phase D — iterators and language essentials

| File | One-line goal |
|---|---|
| [25-iterator-protocol-and-for-of.md](./25-iterator-protocol-and-for-of.md) | Iterator protocol, `for…of`, spread in calls and array literals. |
| [26-classes-extends-super.md](./26-classes-extends-super.md) | `class` declarations, `extends`, `super`, methods, getters / setters, static members. |
| [27-rest-default-destructuring-params.md](./27-rest-default-destructuring-params.md) | Rest parameters, default parameter values, destructuring patterns in parameters. |

### Phase E — number and string completion

| File | One-line goal |
|---|---|
| [28-bitwise-and-number-prototype.md](./28-bitwise-and-number-prototype.md) | Bitwise operators (`& | ^ << >> >>> ~`), `Number.prototype.{toString, toFixed}`, `Math.*` essentials. |
| [29-bigint-value.md](./29-bigint-value.md) | `BigInt` value, literals, arithmetic, comparison, mixed-type coercion rules. |
| [30-string-prototype-completion.md](./30-string-prototype-completion.md) | Remaining `String.prototype` methods (`replace` / `split` / `repeat` / `pad*` / `trim*` / `at` / `codePointAt` / `toLowerCase` / `toUpperCase`). |
| [31-regexp-and-pattern-methods.md](./31-regexp-and-pattern-methods.md) | RegExp value, literal syntax, `String.prototype.{match,matchAll,replace,replaceAll,search,split}` with regex args. |
| [32-json-stringify-parse.md](./32-json-stringify-parse.md) | `JSON.stringify` and `JSON.parse` with deterministic key order. |

### Phase F — promises, modules, async

| File | One-line goal |
|---|---|
| [33-microtask-queue.md](./33-microtask-queue.md) | Microtask queue plus `queueMicrotask` global. |
| [34-promise-value.md](./34-promise-value.md) | `Promise` constructor, `.then` / `.catch` / `.finally`, settled-state semantics. |
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
