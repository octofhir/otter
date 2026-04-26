# Task 40 — `Intl.*` localization

## Goal

Implement the most common `Intl.*` constructors backed by the
`icu_*` crate family from `unicode-org`.

## Scope (foundation subset)

- `Intl.Collator` — `compare(a, b)`.
- `Intl.NumberFormat` — `format(n)` with `style: "decimal" |
  "currency" | "percent"`.
- `Intl.DateTimeFormat` — `format(date)` with the common option
  bag (`year`, `month`, `day`, `hour`, `minute`, `second`).
- Locale resolution falls back to a default ICU locale when an
  unknown tag is requested; the resolved tag is exposed on
  `resolvedOptions()`.

## Out of scope

- `Intl.PluralRules`, `Intl.RelativeTimeFormat`,
  `Intl.ListFormat`, `Intl.DisplayNames`, `Intl.Segmenter` —
  follow-up tasks.
- Custom numbering systems / calendars beyond ICU defaults.

## Files / directories you may touch

- `crates-next/otter-vm/` (intl module).
- `crates-next/otter-runtime/`
- root `Cargo.toml` (the relevant `icu_*` crates).
- `tests/engine/intl/`

## Acceptance criteria

- `new Intl.NumberFormat("en-US", { style: "currency", currency:
  "USD" }).format(1234.5)` returns `"$1,234.50"`.
- `new Intl.Collator("en").compare("a", "b")` is negative.
- `new Intl.DateTimeFormat("en-US").format(...)` returns a
  formatted string.
- Engine suite green.

## Verification commands

```bash
cargo run -p otter-cli -- test --suite engine --filter intl/
```

## Risks

- ICU dependency footprint — pin minimal `icu_*` features and
  document the binary-size trade-off.

## Status

- not started
