# Task 28 — Bitwise operators and Number prototype basics

## Goal

Add the bitwise operator family and the most common
`Number.prototype` / `Math.*` methods so numeric code feels
complete.

## Scope

- Bitwise: `&`, `|`, `^`, `<<`, `>>`, `>>>`, `~`. Operate on
  `i32` after `ToInt32` / `ToUint32` coercion.
- Compound assignment forms: `&=`, `|=`, `^=`, `<<=`, `>>=`,
  `>>>=`.
- `Number.prototype.toString(radix)` (default 10), `toFixed(d)`.
- `Math.abs`, `Math.min`, `Math.max`, `Math.floor`, `Math.ceil`,
  `Math.round`, `Math.trunc`, `Math.sqrt`, `Math.pow` (or `**`),
  `Math.PI`, `Math.E`.
- `**` operator for `Math.pow`-equivalent (foundation lowers it
  through `f64::powf`).

## Out of scope

- `BigInt` bitwise ops (task 29 for BigInt itself).
- Full `Math.*` set (logarithms, hyperbolic, etc.) — separate
  follow-up.
- Complete `toString` with non-decimal radix for floats.

## Files / directories you may touch

- `crates-next/otter-bytecode/` (bitwise opcodes).
- `crates-next/otter-vm/` (number ops + Number prototype + Math
  namespace).
- `crates-next/otter-compiler/`
- `tests/engine/numbers/bitwise/`

## Acceptance criteria

- `5 & 3` returns `1`; `5 | 3` returns `7`; `1 << 3` returns `8`.
- `(-1 >>> 0)` returns `4294967295`.
- `(255).toString(16)` returns `"ff"`.
- `Math.max(1, 2, 3)` returns `3`.
- Engine suite green.

## Verification commands

```bash
cargo run -p otter-cli -- -p '5 & 3'
cargo run -p otter-cli -- test --suite engine --filter numbers/bitwise/
```

## Risks

- Sign-conversion correctness around `>>>` and negative
  `i32`-range values needs explicit fixtures.

## Status

- not started
