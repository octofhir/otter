# Test262 conformance baseline

- **Engine commit:** `15855d553b18ced30fdd1d3b6b5a35f3306e4af2`
- **Test262 commit:** `be13516fb6441b950ba8a3df97eb34062c186972`
- **Captured:** 2026-08-30T12:28:55.998135+00:00

## Totals

| Bucket | Count |
|---|---|
| total      | 53575 |
| passed     | 52463 |
| failed     | 12 |
| skipped    | 1100 |
| crashed    | 0 |
| timed_out  | 0 |
| oom        | 0 |

**Pass rate (excl. skipped):** 99.98%

## Top failing sections (top 50)

| Section | total | passed | failed | pass-rate |
|---|---:|---:|---:|---:|
| intl402/DateTimeFormat/prototype | 180 | 171 | 9 | 95.0% |
| staging/sm/String | 47 | 45 | 1 | 97.8% |
| staging/sm/lexical-environment | 34 | 33 | 1 | 97.1% |
| staging/sm/regress | 106 | 105 | 1 | 99.1% |
| annexB/built-ins/Array | 1 | 1 | 0 | 100.0% |
| annexB/built-ins/Date | 24 | 24 | 0 | 100.0% |
| annexB/built-ins/Function | 6 | 6 | 0 | 100.0% |
| annexB/built-ins/Object | 1 | 1 | 0 | 100.0% |
| annexB/built-ins/RegExp | 62 | 55 | 0 | 100.0% |
| annexB/built-ins/String | 111 | 111 | 0 | 100.0% |
| annexB/built-ins/TypedArrayConstructors | 1 | 1 | 0 | 100.0% |
| annexB/built-ins/escape | 16 | 16 | 0 | 100.0% |
| annexB/built-ins/unescape | 19 | 19 | 0 | 100.0% |
| annexB/language/comments | 8 | 8 | 0 | 100.0% |
| annexB/language/eval-code | 469 | 469 | 0 | 100.0% |
| annexB/language/expressions | 26 | 26 | 0 | 100.0% |
| annexB/language/function-code | 159 | 159 | 0 | 100.0% |
| annexB/language/global-code | 153 | 153 | 0 | 100.0% |
| annexB/language/literals | 8 | 8 | 0 | 100.0% |
| annexB/language/statements | 22 | 22 | 0 | 100.0% |
| built-ins/AbstractModuleSource/length.js | 1 | 0 | 0 | 0.0% |
| built-ins/AbstractModuleSource/name.js | 1 | 0 | 0 | 0.0% |
| built-ins/AbstractModuleSource/proto.js | 1 | 0 | 0 | 0.0% |
| built-ins/AbstractModuleSource/prototype | 3 | 0 | 0 | 0.0% |
| built-ins/AbstractModuleSource/prototype.js | 1 | 0 | 0 | 0.0% |
| built-ins/AbstractModuleSource/throw-from-constructor.js | 1 | 0 | 0 | 0.0% |
| built-ins/AggregateError/cause-property.js | 1 | 1 | 0 | 100.0% |
| built-ins/AggregateError/errors-iterabletolist-failures.js | 1 | 1 | 0 | 100.0% |
| built-ins/AggregateError/errors-iterabletolist.js | 1 | 1 | 0 | 100.0% |
| built-ins/AggregateError/is-a-constructor.js | 1 | 1 | 0 | 100.0% |
| built-ins/AggregateError/length.js | 1 | 1 | 0 | 100.0% |
| built-ins/AggregateError/message-method-prop-cast.js | 1 | 1 | 0 | 100.0% |
| built-ins/AggregateError/message-method-prop.js | 1 | 1 | 0 | 100.0% |
| built-ins/AggregateError/message-tostring-abrupt-symbol.js | 1 | 1 | 0 | 100.0% |
| built-ins/AggregateError/message-tostring-abrupt.js | 1 | 1 | 0 | 100.0% |
| built-ins/AggregateError/message-undefined-no-prop.js | 1 | 1 | 0 | 100.0% |
| built-ins/AggregateError/name.js | 1 | 1 | 0 | 100.0% |
| built-ins/AggregateError/newtarget-is-undefined.js | 1 | 1 | 0 | 100.0% |
| built-ins/AggregateError/newtarget-proto-custom.js | 1 | 1 | 0 | 100.0% |
| built-ins/AggregateError/newtarget-proto-fallback.js | 1 | 1 | 0 | 100.0% |
| built-ins/AggregateError/newtarget-proto.js | 1 | 1 | 0 | 100.0% |
| built-ins/AggregateError/order-of-args-evaluation.js | 1 | 1 | 0 | 100.0% |
| built-ins/AggregateError/prop-desc.js | 1 | 1 | 0 | 100.0% |
| built-ins/AggregateError/proto-from-ctor-realm.js | 1 | 0 | 0 | 0.0% |
| built-ins/AggregateError/proto.js | 1 | 1 | 0 | 100.0% |
| built-ins/AggregateError/prototype | 6 | 6 | 0 | 100.0% |
| built-ins/Array/15.4.5-1.js | 1 | 1 | 0 | 100.0% |
| built-ins/Array/15.4.5.1-5-1.js | 1 | 1 | 0 | 100.0% |
| built-ins/Array/15.4.5.1-5-2.js | 1 | 1 | 0 | 100.0% |
| built-ins/Array/S15.4.1_A1.1_T1.js | 1 | 1 | 0 | 100.0% |

## Top failing-test patterns (top 100)

| Outcome | Reason (truncated) | Path |
|---|---|---|
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `intl402/DateTimeFormat/prototype/formatRange/en-US.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: month co… | `intl402/DateTimeFormat/prototype/formatRangeToParts/chinese-calendar-dates.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: month co… | `intl402/DateTimeFormat/prototype/formatRangeToParts/dangi-calendar-dates.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `intl402/DateTimeFormat/prototype/formatRangeToParts/en-US.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: month co… | `intl402/DateTimeFormat/prototype/formatToParts/chinese-calendar-dates.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: date = 2… | `intl402/DateTimeFormat/prototype/formatToParts/compare-to-temporal-lunisolar.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: date = 2… | `intl402/DateTimeFormat/prototype/formatToParts/compare-to-temporal.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: month co… | `intl402/DateTimeFormat/prototype/formatToParts/dangi-calendar-dates.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: islamic-… | `intl402/DateTimeFormat/prototype/formatToParts/era.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/String/internalUsage.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/lexical-environment/block-scoped-functions-annex-b-arguments.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: function… | `staging/sm/regress/regress-602621.js` |

