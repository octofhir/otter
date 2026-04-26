# Task 38 — `Map` / `Set` / `WeakMap` / `WeakSet`

## Goal

Add the four collection built-ins with their iterator integration.

## Scope

- `Map` and `Set` stored as `IndexMap<Key, Value>` / `IndexSet<Key>`
  for insertion-order iteration semantics.
- `WeakMap` and `WeakSet` use a hash-map keyed on object identity
  with weak references implemented as a stable allocation id (real
  weak collection eviction lands when the GC slice ships; for now
  entries live until cleared explicitly).
- Iterator integration: `Map.prototype.{keys, values, entries,
  forEach}` and the equivalent for `Set`.
- `for…of` over a `Map` walks `[key, value]` pairs.

## Out of scope

- Real weak-eviction (hooks into a future GC slice).
- `Map.groupBy` / `Set.union`-family proposals.

## Files / directories you may touch

- `crates-next/otter-vm/`
- `crates-next/otter-runtime/`
- `tests/engine/collections/`

## Acceptance criteria

- `let m = new Map(); m.set("k", 1); m.get("k")` returns `1`.
- `[...new Set([1,1,2,3])].length` returns `3`.
- `WeakMap` with object key: get returns the stored value.
- Engine suite green.

## Verification commands

```bash
cargo run -p otter-cli -- test --suite engine --filter collections/
```

## Risks

- `WeakMap` without real GC eviction is a known gap — document it
  prominently in the module docstring so future GC work catches up.

## Status

- not started
