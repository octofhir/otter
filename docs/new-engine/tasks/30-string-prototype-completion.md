# Task 30 — Finish `String.prototype.*`

## Goal

Add the remaining commonly-used `String.prototype` methods to the
existing intrinsic table.

## Scope

- `replace(needle, replacement)` and `replaceAll(needle, replacement)` —
  string-needle form only in this slice; regex-needle is task 31.
- `split(separator, limit?)` — string separator only in this slice.
- `repeat(n)`.
- `padStart(targetLength, padString?)`, `padEnd(targetLength, padString?)`.
- `trim()`, `trimStart()`, `trimEnd()`.
- `at(idx)` (negative indices supported).
- `codePointAt(idx)`.
- `toLowerCase()`, `toUpperCase()` — ASCII fast path; full Unicode
  folding deferred.
- `concat(...args)`.
- `includes(needle)`.

## Out of scope

- Locale-aware methods (`localeCompare`, `toLocaleLowerCase` etc.)
  — task 40 (Intl).
- Regex-arg variants — task 31.
- Full Unicode case mapping — depends on a future ICU integration.

## Files / directories you may touch

- `crates-next/otter-vm/string_prototype.rs`
- `tests/engine/strings/methods/`

## Acceptance criteria

- `"abcabc".replace("b", "X")` returns `"aXcabc"`.
- `"a,b,c".split(",")` returns `["a","b","c"]` (depends on task 20
  for arrays).
- `"abc".repeat(3)` returns `"abcabcabc"`.
- `"42".padStart(5, "0")` returns `"00042"`.
- `"  hi ".trim()` returns `"hi"`.
- `"abc".at(-1)` returns `"c"`.
- Engine suite green.

## Verification commands

```bash
cargo run -p otter-cli -- test --suite engine --filter strings/methods/
```

## Risks

- ASCII case mapping is only a subset; document the limitation in
  the module docstring and surface it as a known gap.

## Status

- not started
