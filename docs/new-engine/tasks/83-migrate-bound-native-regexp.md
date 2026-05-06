# Task 83 — Migrate `BoundFunction`, `NativeFunction`, `JsRegExp`

## Status

- [x] `BoundFunction` body migrated to `Gc<…>`
- [x] `NativeFunction` body migrated to `Gc<…>`
- [x] `JsRegExp` body migrated to `Gc<…>`
- [x] last `Rc<…>` removed from public `Value` variants
- [x] native function signature uses explicit `NativeCtx`; async host
      ops do not capture VM / GC references
- [x] gates green

Closed 2026-05-05. Non-Test262 gates run:
`cargo fmt --all`, targeted task-83 VM tests, `cargo clippy
--workspace --all-targets --all-features -- -D warnings`, and
`cargo test --workspace`.

## Goal

Final per-variant migration. These are the residual `Rc<…>`-shared
callable / regex types in `Value`. After this task, no `Value`
variant carries a non-leaf `Rc` or `RefCell`.

## Source

[`docs/new-engine/gc-architecture.md`](../gc-architecture.md) §4.1,
§10.2 Q4 (regex policy: leaf, no internal trace).

## Scope

1. **`BoundFunction`** —
   ```rust
   pub struct BoundFunction {
       pub target: Value,        // strong
       pub bound_this: Value,    // strong
       pub prefix: SmallVec<[Value; 4]>,
   }
   ```
   Trace traverses `target` + `bound_this` + each `prefix` element.
2. **`NativeFunction`** — body holds a Rust closure plus optional
   captured `Value`s. Trace traverses captures. The call signature
   takes an explicit `NativeCtx` / runtime context. Async host functions
   are represented by the ADR-0005 host-op bridge: sync validation on the
   isolate, owned data sent to `EventLoop`, completion posted back by op
   id. Do not store an `async fn` closure that can retain `NativeCtx`,
   `Value`, `Gc<T>`, or `Local<'gc, T>` across `.await`.
3. **`JsRegExp`** — leaf as far as the GC is concerned: the
   `regress::Regex` is owned by the body, has no GC children. Trace
   is empty. (Documented explicitly per architecture-doc §10.2 Q4 —
   `regress`-internal allocations escape the cap.)
4. **`trace_value` arms** for `BoundFunction`, `NativeFunction`,
   `RegExp`.

## Out of scope

- `Value::Symbol(JsSymbol)` — symbols are interned, the registry
  itself is a root (already traced in task 75); per-symbol body has
  no children to trace. Confirm in PR; if non-trivial, file a
  follow-up.
- `Value::BigInt(BigIntValue)` — primitive value, no children.
- `Value::Temporal(JsTemporal)` — owned by `temporal_rs`; treat
  as leaf like RegExp.

## Validation gates

- [x] `grep -rn "Rc<RefCell\|Rc<.*Body" crates-next/otter-vm/src` returns
  zero hits inside `Value`-variant bodies (Shape and other
  immutable-shared types still allowed).
- [x] Compile-fail test proves a native host future cannot capture
  `NativeCtx<'_>`, internal `Value`, `Gc<T>`, or `Local<'gc, T>`.
- [x] All existing engine fixtures pass.
- [x] `cargo clippy --workspace -- -D warnings` clean.

## Closing

Gates from [`README.md`](./README.md#closing-a-task), tick 83 in
[70-gc-master-tracker.md](./70-gc-master-tracker.md), delete this
file.
