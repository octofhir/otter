# Test262 conformance baseline

- **Engine commit:** `56bc0e56b86c668a919a302b5496063ecf3eab97`
- **Test262 commit:** `7e115f46ac64340827d505fa928ad436cb7ba5a6`
- **Captured:** 2026-07-10T18:55:58.738303+00:00

## Totals

| Bucket | Count |
|---|---|
| total      | 53173 |
| passed     | 51480 |
| failed     | 498 |
| skipped    | 1185 |
| crashed    | 0 |
| timed_out  | 10 |
| oom        | 0 |

**Pass rate (excl. skipped):** 99.02%

## Top failing sections (top 50)

| Section | total | passed | failed | pass-rate |
|---|---:|---:|---:|---:|
| intl402/DateTimeFormat/prototype | 180 | 143 | 37 | 79.4% |
| built-ins/Temporal/ZonedDateTime | 901 | 867 | 34 | 96.2% |
| built-ins/Temporal/Duration | 540 | 510 | 30 | 94.4% |
| intl402/Temporal/ZonedDateTime | 583 | 556 | 27 | 95.4% |
| intl402/NumberFormat/prototype | 179 | 155 | 24 | 86.6% |
| staging/sm/class | 94 | 79 | 15 | 84.0% |
| staging/sm/regress | 106 | 91 | 14 | 85.8% |
| intl402/Temporal/PlainDate | 493 | 481 | 12 | 97.6% |
| staging/sm/Function | 53 | 41 | 12 | 77.4% |
| staging/sm/RegExp | 91 | 79 | 12 | 86.8% |
| staging/sm/TypedArray | 96 | 84 | 12 | 87.5% |
| intl402/Temporal/PlainDateTime | 483 | 472 | 11 | 97.7% |
| built-ins/Temporal/PlainDateTime | 773 | 763 | 10 | 98.7% |
| staging/sm/extensions | 64 | 54 | 10 | 84.4% |
| built-ins/Temporal/Instant | 465 | 456 | 9 | 98.1% |
| intl402/Temporal/PlainYearMonth | 327 | 318 | 9 | 97.2% |
| staging/sm/Array | 90 | 81 | 9 | 90.0% |
| staging/sm/lexical-environment | 34 | 25 | 9 | 73.5% |
| built-ins/Temporal/PlainDate | 652 | 644 | 8 | 98.8% |
| annexB/language/expressions | 26 | 19 | 7 | 73.1% |
| built-ins/Temporal/PlainYearMonth | 509 | 502 | 7 | 98.6% |
| intl402/Temporal/PlainMonthDay | 90 | 83 | 7 | 92.2% |
| language/eval-code/direct | 286 | 279 | 7 | 97.6% |
| staging/sm/expressions | 42 | 35 | 7 | 83.3% |
| built-ins/Temporal/PlainTime | 493 | 487 | 6 | 98.8% |
| intl402/DurationFormat/prototype | 81 | 75 | 6 | 92.6% |
| staging/sm/Proxy | 24 | 18 | 6 | 75.0% |
| built-ins/Temporal/PlainMonthDay | 199 | 194 | 5 | 97.5% |
| intl402/PluralRules/prototype | 34 | 29 | 5 | 85.3% |
| intl402/Temporal/Instant | 17 | 12 | 5 | 70.6% |
| staging/sm/Error | 6 | 1 | 5 | 16.7% |
| staging/sm/object | 65 | 60 | 5 | 92.3% |
| built-ins/Temporal/Now | 66 | 62 | 4 | 93.9% |
| language/literals/regexp | 238 | 234 | 4 | 98.3% |
| staging/sm/PrivateName | 17 | 13 | 4 | 76.5% |
| staging/sm/fields | 8 | 4 | 4 | 50.0% |
| staging/sm/strict | 51 | 47 | 4 | 92.2% |
| built-ins/RegExp/lookBehind | 17 | 14 | 3 | 82.4% |
| intl402/Temporal/PlainTime | 12 | 9 | 3 | 75.0% |
| staging/sm/Date | 28 | 17 | 3 | 60.7% |
| staging/sm/Reflect | 17 | 14 | 3 | 82.4% |
| staging/sm/Symbol | 30 | 27 | 3 | 90.0% |
| staging/sm/eval | 20 | 17 | 3 | 85.0% |
| annexB/built-ins/RegExp | 62 | 53 | 2 | 96.4% |
| intl402/Array/prototype | 2 | 0 | 2 | 0.0% |
| intl402/BigInt/prototype | 11 | 9 | 2 | 81.8% |
| intl402/Date/prototype | 12 | 10 | 2 | 83.3% |
| language/expressions/assignment | 485 | 483 | 2 | 99.6% |
| language/expressions/tagged-template | 27 | 24 | 2 | 92.3% |
| staging/sm/BigInt | 5 | 3 | 2 | 60.0% |

## Top failing-test patterns (top 100)

| Outcome | Reason (truncated) | Path |
|---|---|---|
| fail | strict: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `built-ins/Proxy/revocable/tco-fn-realm.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: SyntaxError: RegExp: i… | `built-ins/RegExp/S15.10.2.8_A3_T15.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: SyntaxError: RegExp: i… | `built-ins/RegExp/S15.10.2.8_A3_T16.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [… | `built-ins/RegExp/lookBehind/alternations.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [… | `built-ins/RegExp/lookBehind/back-references-to-captures.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual a… | `built-ins/RegExp/lookBehind/mutual-recursive.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [… | `built-ins/RegExp/lookahead-quantifier-match-groups.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: SyntaxError: RegExp: i… | `built-ins/RegExp/quantifier-integer-limit.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: \P{Lu} s… | `built-ins/RegExp/regexp-modifiers/add-ignoreCase-affects-slash-upper-p.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: Temporal.Du… | `built-ins/Temporal/Duration/compare/order-of-operations.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: hour pro… | `built-ins/Temporal/Duration/compare/relativeto-propertybag-infinity-throws-rangeerror.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: case whe… | `built-ins/Temporal/Duration/from/argument-duration-precision-exact-numerical-values.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: years re… | `built-ins/Temporal/Duration/max.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: case whe… | `built-ins/Temporal/Duration/prototype/add/argument-duration-precision-exact-numerical-values.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: toString… | `built-ins/Temporal/Duration/prototype/add/float64-representable-integer.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: duration… | `built-ins/Temporal/Duration/prototype/add/precision-no-floating-point-loss.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `built-ins/Temporal/Duration/prototype/add/result-out-of-range-1.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `built-ins/Temporal/Duration/prototype/add/result-out-of-range-3.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: rounding… | `built-ins/Temporal/Duration/prototype/round/calendar-possibly-required.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: P31D wee… | `built-ins/Temporal/Duration/prototype/round/exact-multiple-of-larger-unit-plaindate.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: P7D days… | `built-ins/Temporal/Duration/prototype/round/exact-multiple-of-larger-unit-zoned.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: toString… | `built-ins/Temporal/Duration/prototype/round/float64-representable-integer.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: Temporal.Du… | `built-ins/Temporal/Duration/prototype/round/order-of-operations.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: BalanceT… | `built-ins/Temporal/Duration/prototype/round/precision-exact-in-balance-time-duration.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: hour pro… | `built-ins/Temporal/Duration/prototype/round/relativeto-infinity-throws-rangeerror.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Combinat… | `built-ins/Temporal/Duration/prototype/round/relativeto-largestunit-smallestunit-combinations.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `built-ins/Temporal/Duration/prototype/round/throws-if-neither-largestUnit-nor-smallestUnit-is-given.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `built-ins/Temporal/Duration/prototype/round/total-duration-nanoseconds-too-large-with-zoned-datetime.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: case whe… | `built-ins/Temporal/Duration/prototype/subtract/argument-duration-precision-exact-numerical-values.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: toString… | `built-ins/Temporal/Duration/prototype/subtract/float64-representable-integer.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: duration… | `built-ins/Temporal/Duration/prototype/subtract/precision-no-floating-point-loss.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: subtract… | `built-ins/Temporal/Duration/prototype/subtract/result-out-of-range-1.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `built-ins/Temporal/Duration/prototype/subtract/result-out-of-range-3.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: Temporal.Du… | `built-ins/Temporal/Duration/prototype/total/order-of-operations.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: hour pro… | `built-ins/Temporal/Duration/prototype/total/relativeto-infinity-throws-rangeerror.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `built-ins/Temporal/Duration/prototype/total/throws-if-unit-property-missing.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Throws T… | `built-ins/Temporal/Duration/prototype/with/argument-invalid-property.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Throw Ty… | `built-ins/Temporal/Duration/prototype/with/argument-singular-properties.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Instance… | `built-ins/Temporal/Duration/subclass.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Minimum … | `built-ins/Temporal/Instant/prototype/add/minimum-maximum-instant.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Duration… | `built-ins/Temporal/Instant/prototype/since/float64-representable-integer.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: does not… | `built-ins/Temporal/Instant/prototype/since/largestunit.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `built-ins/Temporal/Instant/prototype/subtract/minimum-maximum-instant.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `built-ins/Temporal/Instant/prototype/toString/get-timezone-throws.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [… | `built-ins/Temporal/Instant/prototype/toString/options-read-before-algorithmic-validation.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [… | `built-ins/Temporal/Instant/prototype/toString/order-of-operations.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Duration… | `built-ins/Temporal/Instant/prototype/until/float64-representable-integer.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Instance… | `built-ins/Temporal/Instant/subclass.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: prototyp… | `built-ins/Temporal/Now/builtin.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: The resu… | `built-ins/Temporal/Now/instant/return-value-value.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Now desc… | `built-ins/Temporal/Now/prop-desc.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: String: Typ… | `built-ins/Temporal/Now/toStringTag/string.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: P1M week… | `built-ins/Temporal/PlainDate/prototype/since/exact-multiple-of-larger-unit.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `built-ins/Temporal/PlainDate/prototype/toZonedDateTime/argument-object-get-plainTime-throws.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `built-ins/Temporal/PlainDate/prototype/toZonedDateTime/argument-object-get-timezone-throws.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: Temporal.Pl… | `built-ins/Temporal/PlainDate/prototype/toZonedDateTime/order-of-operations.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: P1M week… | `built-ins/Temporal/PlainDate/prototype/until/exact-multiple-of-larger-unit.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Partial … | `built-ins/Temporal/PlainDate/prototype/with/options-wrong-type.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [… | `built-ins/Temporal/PlainDate/prototype/with/order-of-operations.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Instance… | `built-ins/Temporal/PlainDate/subclass.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: P1M week… | `built-ins/Temporal/PlainDateTime/prototype/since/exact-multiple-of-larger-unit.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Duration… | `built-ins/Temporal/PlainDateTime/prototype/since/float64-representable-integer.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [… | `built-ins/Temporal/PlainDateTime/prototype/toZonedDateTime/options-read-before-algorithmic-validation.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [… | `built-ins/Temporal/PlainDateTime/prototype/toZonedDateTime/order-of-operations.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: P1M week… | `built-ins/Temporal/PlainDateTime/prototype/until/exact-multiple-of-larger-unit.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Duration… | `built-ins/Temporal/PlainDateTime/prototype/until/float64-representable-integer.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: can retu… | `built-ins/Temporal/PlainDateTime/prototype/until/units-changed.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Partial … | `built-ins/Temporal/PlainDateTime/prototype/with/options-wrong-type.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [… | `built-ins/Temporal/PlainDateTime/prototype/with/order-of-operations.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Instance… | `built-ins/Temporal/PlainDateTime/subclass.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: Temporal.Pl… | `built-ins/Temporal/PlainMonthDay/from/fields-object.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [… | `built-ins/Temporal/PlainMonthDay/prototype/toPlainDate/order-of-operations.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Partial … | `built-ins/Temporal/PlainMonthDay/prototype/with/options-wrong-type.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [… | `built-ins/Temporal/PlainMonthDay/prototype/with/order-of-operations.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Instance… | `built-ins/Temporal/PlainMonthDay/subclass.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: millisec… | `built-ins/Temporal/PlainTime/compare/exhaustive.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [… | `built-ins/Temporal/PlainTime/from/order-of-operations.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [… | `built-ins/Temporal/PlainTime/prototype/with/order-of-operations.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: PlainDat… | `built-ins/Temporal/PlainTime/prototype/with/plaintimelike-invalid.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: temporal… | `built-ins/Temporal/PlainTime/prototype/with/throws-if-time-is-invalid-when-overflow-is-reject.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Instance… | `built-ins/Temporal/PlainTime/subclass.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: Temporal.Pl… | `built-ins/Temporal/PlainYearMonth/from/argument-plaindate.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: P1Y mont… | `built-ins/Temporal/PlainYearMonth/prototype/since/exact-multiple-of-larger-unit.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [… | `built-ins/Temporal/PlainYearMonth/prototype/toPlainDate/order-of-operations.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: P1Y mont… | `built-ins/Temporal/PlainYearMonth/prototype/until/exact-multiple-of-larger-unit.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Partial … | `built-ins/Temporal/PlainYearMonth/prototype/with/options-wrong-type.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [… | `built-ins/Temporal/PlainYearMonth/prototype/with/order-of-operations.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Instance… | `built-ins/Temporal/PlainYearMonth/subclass.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: Temporal.Zo… | `built-ins/Temporal/ZonedDateTime/argument-convert.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [… | `built-ins/Temporal/ZonedDateTime/compare/order-of-operations.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: RangeError: Temporal.Z… | `built-ins/Temporal/ZonedDateTime/from/argument-string-limits.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: RangeError: Temporal.Z… | `built-ins/Temporal/ZonedDateTime/from/offset-overrides-critical-flag.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: UTC offs… | `built-ins/Temporal/ZonedDateTime/from/offset-string-invalid.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [… | `built-ins/Temporal/ZonedDateTime/from/order-of-operations.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: RangeError: Temporal.Z… | `built-ins/Temporal/ZonedDateTime/from/zoneddatetime-string.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [… | `built-ins/Temporal/ZonedDateTime/prototype/equals/order-of-operations.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `built-ins/Temporal/ZonedDateTime/prototype/getTimeZoneTransition/direction-undefined.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: Temporal.Zo… | `built-ins/Temporal/ZonedDateTime/prototype/getTimeZoneTransition/order-of-operations.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `built-ins/Temporal/ZonedDateTime/prototype/hoursInDay/get-start-of-day-throws.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Next day… | `built-ins/Temporal/ZonedDateTime/prototype/hoursInDay/next-day-out-of-range.js` |

