# Task 29 — `BigInt` value

## Goal

Add a `Value::BigInt(BigIntValue)` variant with arithmetic,
comparison, and the spec coercion rules between Number and BigInt.

## Scope

- Storage: `BigIntValue` wraps a heap-allocated arbitrary-precision
  integer (use `num-bigint` from crates.io to avoid hand-rolling).
- Bytecode: extend `Op::Add` / `Sub` / `Mul` / `Div` / `Rem` /
  `Neg` / comparisons to handle the BigInt path; mixed Number /
  BigInt operands throw `TypeError` per spec.
- Bitwise ops on BigInt (`& | ^ << >> ~`).
- Literals: `123n`.
- `BigInt(value)` and `BigInt.asIntN` / `asUintN` constructors are
  out of scope for this slice — provide only the literal form.

## Out of scope

- Full `BigInt` constructor and prototype.
- `JSON.stringify` of BigInt (throws `TypeError` per spec — fine
  to leave that for the JSON task).

## Files / directories you may touch

- `crates-next/otter-vm/` (bigint module).
- `crates-next/otter-compiler/`
- `tests/engine/numbers/bigint/`
- root `Cargo.toml` (`num-bigint`).

## Acceptance criteria

- `9007199254740993n + 1n` returns `9007199254740994n`.
- `1n + 1` throws `TypeError`.
- `1n === 1` is `false`.
- Engine suite green.

## Verification commands

```bash
cargo run -p otter-cli -- -p '9007199254740993n + 1n'
cargo run -p otter-cli -- test --suite engine --filter numbers/bigint/
```

## Risks

- Display rendering must end with `n` per `BigInt.prototype.toString`
  contract for the literal in REPL-like output? No — toString of
  BigInt returns the decimal **without** the `n` suffix. Keep
  the rule in the module docstring.

## Status

- not started
