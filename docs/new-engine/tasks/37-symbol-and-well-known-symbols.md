# Task 37 — `Symbol` value and well-known symbols

## Goal

Add `Value::Symbol(JsSymbol)` and replace the temporary
`"@@iterator"` string sentinel from task 25 with the real
`Symbol.iterator`.

## Scope

- `JsSymbol`: an `Rc`-shared identity carrying an optional
  description string. Each `Symbol(desc)` call returns a new
  identity even if descriptions match.
- Well-known symbols: at minimum `Symbol.iterator` and
  `Symbol.asyncIterator` (registered as global, identity-stable).
- Property keys: `JsObject` accepts `JsSymbol` keys alongside
  `JsString`. Shape transitions extended.
- `Symbol.for(s)` / `Symbol.keyFor(sym)` — global symbol registry.
- Switch the iterator-protocol lookup to `Symbol.iterator`.

## Out of scope

- `Symbol.toPrimitive`, `Symbol.species` and related semantic
  hooks — separate slices once the relevant call sites exist.

## Files / directories you may touch

- `crates-next/otter-vm/`
- `crates-next/otter-runtime/`
- `tests/engine/symbols/`

## Acceptance criteria

- `Symbol("a") === Symbol("a")` returns `false`.
- `Symbol.for("x") === Symbol.for("x")` returns `true`.
- `for (let v of obj)` uses `Symbol.iterator`.
- Engine suite green.

## Verification commands

```bash
cargo run -p otter-cli -- test --suite engine --filter symbols/
```

## Risks

- Migrating object property keys from string-only to either-string-
  or-symbol is invasive; touch every property-load / store path.

## Status

- not started
