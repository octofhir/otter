# Task 31 — RegExp value and pattern-arg `String.prototype` methods

## Goal

Introduce `Value::RegExp` with literal syntax `/pattern/flags`,
`.exec(s)` / `.test(s)`, and wire up the regex-arg forms of
`String.prototype.{match, matchAll, replace, replaceAll, search,
split}`.

## Scope

- A pluggable regex backend: bring in `regress` (already in the
  Rust ecosystem; avoids reinventing the engine).
- New `Op::LoadRegExp` reading a pooled `Constant::RegExp { pattern,
  flags }`.
- `RegExp.prototype.{exec, test, source, flags, lastIndex}`.
- Pattern-arg overloads of the relevant `String.prototype` methods
  (already string-arg-only after task 30).

## Out of scope

- Full `dotAll`/`unicode-property-escapes`/named-group nuances
  beyond what `regress` already supports out of the box — track
  gaps as follow-ups.
- Compile-once / cached `RegExp` per literal site (optimization;
  later).

## Files / directories you may touch

- `crates-next/otter-vm/` (regexp module).
- `crates-next/otter-bytecode/` (`Constant::RegExp`).
- `crates-next/otter-compiler/` (regex literal lowering).
- `tests/engine/strings/regex/`
- root `Cargo.toml` (`regress`).

## Acceptance criteria

- `/abc/.test("abcdef")` returns `true`.
- `"abcabc".match(/b./g)` returns `["bc","bc"]`.
- `"abc".replace(/b/, "X")` returns `"aXc"`.
- Engine suite green.

## Verification commands

```bash
cargo run -p otter-cli -- -p '/abc/.test("abcdef")'
cargo run -p otter-cli -- test --suite engine --filter strings/regex/
```

## Risks

- `lastIndex` mutability + `g` flag semantics are easy to get
  wrong — write fixtures for both.

## Status

- not started
