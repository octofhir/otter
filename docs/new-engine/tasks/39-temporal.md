# Task 39 — `Temporal.*` modern date / time API

## Goal

Implement the `Temporal` namespace from scratch with the modern
spec surface used in real applications.

## Scope (foundation subset)

- `Temporal.Instant` — `from(string)`, `epochMilliseconds`,
  `epochNanoseconds`, comparison, arithmetic with `Duration`.
- `Temporal.Duration` — construction, addition, `total({ unit })`.
- `Temporal.PlainDate` — `from(string)`, year / month / day
  components, `add` / `subtract`.
- `Temporal.PlainTime` — analogous.
- `Temporal.PlainDateTime` — combination of the two.
- `Temporal.Now.{instant, plainDateTimeISO}` — read-only views of
  the host clock.

The remaining surface (`PlainYearMonth`, `PlainMonthDay`,
`ZonedDateTime`, calendars, time zones beyond UTC) is filed as
follow-up tasks once this slice ships.

## Out of scope

- Non-ISO calendars.
- Non-UTC time zone math (just store the offset and keep
  arithmetic UTC).
- Locale-aware formatting (that is task 40 / Intl).

## Files / directories you may touch

- `crates-next/otter-vm/` (temporal module).
- `crates-next/otter-runtime/`
- `tests/engine/temporal/`

## Acceptance criteria

- `Temporal.Instant.from("2024-01-01T00:00:00Z").epochMilliseconds`
  returns the expected ms.
- `Temporal.PlainDate.from("2024-12-31").add({ days: 1 }).toString()`
  returns `"2025-01-01"`.
- `Temporal.Duration.from({ hours: 1, minutes: 30 }).total({ unit:
  "minutes" })` returns `90`.
- Engine suite green.

## Verification commands

```bash
cargo run -p otter-cli -- test --suite engine --filter temporal/
```

## Risks

- ISO date arithmetic edge cases (leap years, month rollover).
- BigInt-based nanoseconds (`epochNanoseconds`) depends on task 29.

## Status

- not started
