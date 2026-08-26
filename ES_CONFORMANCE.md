# Test262 conformance baseline

- **Engine commit:** `7bded8902e1dec3bdb4db7f04cb6d3835dc56328`
- **Test262 commit:** `be13516fb6441b950ba8a3df97eb34062c186972`
- **Captured:** 2026-08-26T03:14:23.888449+00:00

## Totals

| Bucket | Count |
|---|---|
| total      | 53459 |
| passed     | 52066 |
| failed     | 293 |
| skipped    | 1100 |
| crashed    | 0 |
| timed_out  | 0 |
| oom        | 0 |

**Pass rate (excl. skipped):** 99.44%

## Top failing sections (top 50)

| Section | total | passed | failed | pass-rate |
|---|---:|---:|---:|---:|
| built-ins/Temporal/ZonedDateTime | 901 | 871 | 30 | 96.7% |
| built-ins/Temporal/Duration | 540 | 512 | 28 | 94.8% |
| staging/sm/regress | 106 | 91 | 15 | 85.8% |
| staging/sm/class | 94 | 80 | 14 | 85.1% |
| intl402/Temporal/ZonedDateTime | 583 | 572 | 11 | 98.1% |
| staging/sm/RegExp | 91 | 80 | 11 | 87.9% |
| intl402/DateTimeFormat/prototype | 180 | 171 | 9 | 95.0% |
| staging/sm/Array | 90 | 81 | 9 | 90.0% |
| staging/sm/Date | 28 | 19 | 9 | 67.9% |
| staging/sm/extensions | 64 | 55 | 9 | 85.9% |
| staging/sm/lexical-environment | 34 | 25 | 9 | 73.5% |
| staging/sm/Function | 53 | 45 | 8 | 84.9% |
| staging/sm/expressions | 42 | 34 | 8 | 81.0% |
| annexB/language/expressions | 26 | 19 | 7 | 73.1% |
| language/eval-code/direct | 286 | 279 | 7 | 97.6% |
| staging/sm/TypedArray | 96 | 89 | 7 | 92.7% |
| built-ins/Temporal/Instant | 465 | 459 | 6 | 98.7% |
| built-ins/Temporal/PlainDateTime | 773 | 767 | 6 | 99.2% |
| built-ins/Temporal/PlainTime | 493 | 487 | 6 | 98.8% |
| staging/sm/Proxy | 24 | 18 | 6 | 75.0% |
| built-ins/Temporal/PlainMonthDay | 199 | 194 | 5 | 97.5% |
| built-ins/Temporal/PlainYearMonth | 509 | 504 | 5 | 99.0% |
| staging/sm/PrivateName | 17 | 13 | 4 | 76.5% |
| staging/sm/fields | 8 | 4 | 4 | 50.0% |
| staging/sm/strict | 51 | 47 | 4 | 92.2% |
| built-ins/Temporal/Now | 66 | 63 | 3 | 95.5% |
| built-ins/Temporal/PlainDate | 652 | 649 | 3 | 99.5% |
| intl402/Temporal/PlainMonthDay | 90 | 87 | 3 | 96.7% |
| staging/sm/Reflect | 17 | 14 | 3 | 82.4% |
| staging/sm/eval | 20 | 17 | 3 | 85.0% |
| staging/sm/object | 65 | 62 | 3 | 95.4% |
| intl402/Temporal/PlainDateTime | 483 | 481 | 2 | 99.6% |
| intl402/Temporal/PlainYearMonth | 327 | 325 | 2 | 99.4% |
| language/import/import-defer | 103 | 100 | 2 | 98.0% |
| staging/sm/BigInt | 5 | 3 | 2 | 60.0% |
| staging/sm/syntax | 11 | 9 | 2 | 81.8% |
| annexB/language/function-code | 159 | 158 | 1 | 99.4% |
| built-ins/Temporal/keys.js | 1 | 0 | 1 | 0.0% |
| built-ins/Temporal/toStringTag | 2 | 1 | 1 | 50.0% |
| intl402/Array/prototype | 2 | 1 | 1 | 50.0% |
| intl402/BigInt/prototype | 11 | 10 | 1 | 90.9% |
| intl402/DateTimeFormat/intl-legacy-constructed-symbol-on-unwrap.js | 1 | 0 | 1 | 0.0% |
| intl402/DateTimeFormat/intl-legacy-constructed-symbol.js | 1 | 0 | 1 | 0.0% |
| intl402/FallbackSymbol/per-realm.js | 1 | 0 | 1 | 0.0% |
| intl402/Locale/constructor-non-iana-canon.js | 1 | 0 | 1 | 0.0% |
| intl402/NumberFormat/intl-legacy-constructed-symbol-on-unwrap.js | 1 | 0 | 1 | 0.0% |
| intl402/NumberFormat/intl-legacy-constructed-symbol.js | 1 | 0 | 1 | 0.0% |
| intl402/Temporal/Instant | 17 | 16 | 1 | 94.1% |
| intl402/Temporal/PlainDate | 493 | 492 | 1 | 99.8% |
| intl402/Temporal/PlainTime | 12 | 11 | 1 | 91.7% |

## Top failing-test patterns (top 100)

| Outcome | Reason (truncated) | Path |
|---|---|---|
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
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Duration… | `built-ins/Temporal/Instant/prototype/until/float64-representable-integer.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Instance… | `built-ins/Temporal/Instant/subclass.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: prototyp… | `built-ins/Temporal/Now/builtin.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Now desc… | `built-ins/Temporal/Now/prop-desc.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: String: Ord… | `built-ins/Temporal/Now/toStringTag/string.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Partial … | `built-ins/Temporal/PlainDate/prototype/with/options-wrong-type.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [… | `built-ins/Temporal/PlainDate/prototype/with/order-of-operations.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Instance… | `built-ins/Temporal/PlainDate/subclass.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Duration… | `built-ins/Temporal/PlainDateTime/prototype/since/float64-representable-integer.js` |
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
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [… | `built-ins/Temporal/PlainYearMonth/prototype/toPlainDate/order-of-operations.js` |
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
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Duration… | `built-ins/Temporal/ZonedDateTime/prototype/since/float64-representable-integer.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: does not… | `built-ins/Temporal/ZonedDateTime/prototype/since/largestunit.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [… | `built-ins/Temporal/ZonedDateTime/prototype/since/order-of-operations.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Duration… | `built-ins/Temporal/ZonedDateTime/prototype/until/float64-representable-integer.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [… | `built-ins/Temporal/ZonedDateTime/prototype/until/order-of-operations.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `built-ins/Temporal/ZonedDateTime/prototype/with/disambiguation-invalid-string.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: null Exp… | `built-ins/Temporal/ZonedDateTime/prototype/with/disambiguation-wrong-type.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `built-ins/Temporal/ZonedDateTime/prototype/with/invalid-disambiguation.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `built-ins/Temporal/ZonedDateTime/prototype/with/invalid-offset.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `built-ins/Temporal/ZonedDateTime/prototype/with/minimum-instant-with-one-hour-offset.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `built-ins/Temporal/ZonedDateTime/prototype/with/offset-invalid-string.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: "00:00 i… | `built-ins/Temporal/ZonedDateTime/prototype/with/offset-property-invalid-string.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: null Exp… | `built-ins/Temporal/ZonedDateTime/prototype/with/offset-wrong-type.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [… | `built-ins/Temporal/ZonedDateTime/prototype/with/options-read-before-algorithmic-validation.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Partial … | `built-ins/Temporal/ZonedDateTime/prototype/with/options-wrong-type.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [… | `built-ins/Temporal/ZonedDateTime/prototype/with/order-of-operations.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Instance… | `built-ins/Temporal/ZonedDateTime/subclass.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: An ISO s… | `built-ins/Temporal/ZonedDateTime/timezone-iso-string.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [… | `built-ins/Temporal/keys.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: String: Ord… | `built-ins/Temporal/toStringTag/string.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: ReferenceError: argume… | `language/eval-code/direct/arrow-fn-body-cntns-arguments-func-decl-arrow-func-declare-arguments-assign-incl-def-param-arrow-arguments.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: ReferenceError: argume… | `language/eval-code/direct/arrow-fn-body-cntns-arguments-var-bind-arrow-func-declare-arguments-assign-incl-def-param-arrow-arguments.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: direct e… | `language/eval-code/direct/global-env-rec-with.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: non stri… | `language/eval-code/direct/lex-env-heritage.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: binding … | `language/eval-code/direct/var-env-func-init-local-new-delete.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `language/eval-code/direct/var-env-lower-lex-non-strict.js` |

