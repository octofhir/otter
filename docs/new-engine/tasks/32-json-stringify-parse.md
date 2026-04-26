# Task 32 — `JSON.stringify` / `JSON.parse`

## Goal

Implement the `JSON` namespace with deterministic key ordering on
the stringify path and a strict parser.

## Scope

- `JSON.stringify(value, replacer?, space?)` — supports primitive
  replacer (function replacer is acceptable as a follow-up); space
  parameter as integer or string.
- `JSON.parse(text, reviver?)` — strict JSON only (no relaxations).
- Deterministic key order: enumerate object properties in insertion
  order (relies on the shape model from task 18).
- Cycle detection: throws `TypeError` when serializing a cyclic
  graph (foundation hard-cap of 1024 nesting levels suffices for
  the first version).
- Numeric edge cases: `NaN` / `±Infinity` serialize as `null`
  (per spec).

## Out of scope

- Source-map round-trip for parse errors.
- Streaming / incremental parse.

## Files / directories you may touch

- `crates-next/otter-vm/` (json module).
- `crates-next/otter-runtime/` (global `JSON` registration).
- `tests/engine/json/`

## Acceptance criteria

- `JSON.stringify({ b: 1, a: 2 })` returns `'{"b":1,"a":2}'`
  (insertion order).
- `JSON.parse('{"x":[1,2,3]}').x[1]` returns `2`.
- `JSON.stringify({ x: NaN })` returns `'{"x":null}'`.
- Engine suite green.

## Verification commands

```bash
cargo run -p otter-cli -- -p 'JSON.stringify({ b: 1, a: 2 })'
cargo run -p otter-cli -- test --suite engine --filter json/
```

## Risks

- Iterative serializer required (no unbounded recursion).
- `BigInt` serialization throws — keep aligned with task 29.

## Status

- not started
