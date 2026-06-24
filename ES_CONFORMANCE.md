# Test262 conformance baseline

- **Engine commit:** `6c2e9f793c71aaacb23050788480698657eda0fc`
- **Test262 commit:** `7e115f46ac64340827d505fa928ad436cb7ba5a6`
- **Captured:** 2026-06-24T18:40:48.463293+00:00

## Totals

| Bucket | Count |
|---|---|
| total      | 53173 |
| passed     | 51219 |
| failed     | 752 |
| skipped    | 1191 |
| crashed    | 0 |
| timed_out  | 11 |
| oom        | 0 |

**Pass rate (excl. skipped):** 98.53%

## Top failing sections (top 50)

| Section | total | passed | failed | pass-rate |
|---|---:|---:|---:|---:|
| intl402/DateTimeFormat/prototype | 180 | 110 | 70 | 61.1% |
| intl402/NumberFormat/prototype | 179 | 138 | 41 | 77.1% |
| built-ins/Temporal/ZonedDateTime | 901 | 867 | 34 | 96.2% |
| built-ins/Temporal/Duration | 540 | 510 | 30 | 94.4% |
| intl402/DurationFormat/prototype | 81 | 54 | 27 | 66.7% |
| intl402/Temporal/ZonedDateTime | 583 | 559 | 24 | 95.9% |
| staging/sm/Iterator | 174 | 152 | 22 | 87.4% |
| staging/sm/class | 94 | 75 | 19 | 79.8% |
| intl402/Segmenter/prototype | 36 | 18 | 18 | 50.0% |
| staging/sm/Function | 53 | 36 | 17 | 67.9% |
| staging/sm/regress | 106 | 89 | 16 | 84.0% |
| intl402/RelativeTimeFormat/prototype | 44 | 29 | 15 | 65.9% |
| staging/sm/RegExp | 91 | 78 | 13 | 85.7% |
| staging/sm/expressions | 42 | 29 | 13 | 69.0% |
| intl402/Temporal/PlainDate | 493 | 481 | 12 | 97.6% |
| intl402/Temporal/PlainDateTime | 483 | 471 | 12 | 97.5% |
| staging/sm/extensions | 64 | 51 | 12 | 81.0% |
| intl402/ListFormat/prototype | 49 | 38 | 11 | 77.6% |
| built-ins/Temporal/PlainDateTime | 773 | 763 | 10 | 98.7% |
| intl402/Intl/supportedValuesOf | 25 | 15 | 10 | 60.0% |
| staging/sm/TypedArray | 96 | 86 | 10 | 89.6% |
| staging/sm/lexical-environment | 34 | 24 | 10 | 70.6% |
| annexB/language/expressions | 26 | 17 | 9 | 65.4% |
| built-ins/Temporal/Instant | 465 | 456 | 9 | 98.1% |
| staging/sm/Array | 90 | 81 | 9 | 90.0% |
| staging/sm/Proxy | 24 | 15 | 9 | 62.5% |
| built-ins/Temporal/PlainDate | 652 | 644 | 8 | 98.8% |
| intl402/Intl/getCanonicalLocales | 38 | 30 | 8 | 78.9% |
| intl402/String/prototype | 19 | 11 | 8 | 57.9% |
| intl402/Temporal/PlainYearMonth | 327 | 319 | 8 | 97.6% |
| built-ins/Temporal/PlainYearMonth | 509 | 502 | 7 | 98.6% |
| intl402/Collator/prototype | 35 | 28 | 7 | 80.0% |
| intl402/Temporal/PlainMonthDay | 90 | 83 | 7 | 92.2% |
| language/eval-code/direct | 286 | 279 | 7 | 97.6% |
| built-ins/Temporal/PlainTime | 493 | 487 | 6 | 98.8% |
| intl402/DisplayNames/prototype | 20 | 14 | 6 | 70.0% |
| intl402/Temporal/Instant | 17 | 11 | 6 | 64.7% |
| staging/sm/generators | 27 | 21 | 6 | 77.8% |
| built-ins/Temporal/PlainMonthDay | 199 | 194 | 5 | 97.5% |
| intl402/PluralRules/prototype | 34 | 29 | 5 | 85.3% |
| staging/sm/object | 65 | 60 | 5 | 92.3% |
| built-ins/Temporal/Now | 66 | 62 | 4 | 93.9% |
| intl402/RelativeTimeFormat/constructor | 34 | 29 | 4 | 87.9% |
| intl402/Temporal/PlainTime | 12 | 8 | 4 | 66.7% |
| language/literals/regexp | 238 | 234 | 4 | 98.3% |
| staging/sm/Error | 6 | 2 | 4 | 33.3% |
| staging/sm/PrivateName | 17 | 13 | 4 | 76.5% |
| staging/sm/fields | 8 | 4 | 4 | 50.0% |
| staging/sm/strict | 51 | 47 | 4 | 92.2% |
| annexB/language/statements | 22 | 19 | 3 | 86.4% |

## Top failing-test patterns (top 100)

| Outcome | Reason (truncated) | Path |
|---|---|---|
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: FinalizationRegistr… | `built-ins/FinalizationRegistry/prototype/register/return-undefined-register-object.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected a TypeE… | `built-ins/global/10.2.1.1.3-4-16-s.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected a TypeE… | `built-ins/global/10.2.1.1.3-4-18-s.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: SyntaxError: RegExp: invalid r… | `built-ins/RegExp/S15.10.2.8_A3_T15.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: SyntaxError: RegExp: invalid r… | `built-ins/RegExp/S15.10.2.8_A3_T16.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [xabcd, a… | `built-ins/RegExp/lookBehind/alternations.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [c, abab]… | `built-ins/RegExp/lookBehind/back-references-to-captures.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual argument … | `built-ins/RegExp/lookBehind/mutual-recursive.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [a, abc] … | `built-ins/RegExp/lookahead-quantifier-match-groups.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: SyntaxError: RegExp: invalid r… | `built-ins/RegExp/quantifier-integer-limit.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: \P{Lu} should ma… | `built-ins/RegExp/regexp-modifiers/add-ignoreCase-affects-slash-upper-p.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: Temporal.Duration: … | `built-ins/Temporal/Duration/compare/order-of-operations.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: hour property ca… | `built-ins/Temporal/Duration/compare/relativeto-propertybag-infinity-throws-rangeerror.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: case where float… | `built-ins/Temporal/Duration/from/argument-duration-precision-exact-numerical-values.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: years result: Ex… | `built-ins/Temporal/Duration/max.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: case where float… | `built-ins/Temporal/Duration/prototype/add/argument-duration-precision-exact-numerical-values.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: toString() shoul… | `built-ins/Temporal/Duration/prototype/add/float64-representable-integer.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: duration1.add(du… | `built-ins/Temporal/Duration/prototype/add/precision-no-floating-point-loss.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected a Range… | `built-ins/Temporal/Duration/prototype/add/result-out-of-range-1.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected a Range… | `built-ins/Temporal/Duration/prototype/add/result-out-of-range-3.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: rounding a year … | `built-ins/Temporal/Duration/prototype/round/calendar-possibly-required.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: P31D weeks..mont… | `built-ins/Temporal/Duration/prototype/round/exact-multiple-of-larger-unit-plaindate.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: P7D days..weeks … | `built-ins/Temporal/Duration/prototype/round/exact-multiple-of-larger-unit-zoned.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: toString() shoul… | `built-ins/Temporal/Duration/prototype/round/float64-representable-integer.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: Temporal.Duration: … | `built-ins/Temporal/Duration/prototype/round/order-of-operations.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: BalanceTimeDurat… | `built-ins/Temporal/Duration/prototype/round/precision-exact-in-balance-time-duration.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: hour property ca… | `built-ins/Temporal/Duration/prototype/round/relativeto-infinity-throws-rangeerror.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Combination of l… | `built-ins/Temporal/Duration/prototype/round/relativeto-largestunit-smallestunit-combinations.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected a Range… | `built-ins/Temporal/Duration/prototype/round/throws-if-neither-largestUnit-nor-smallestUnit-is-given.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected a Range… | `built-ins/Temporal/Duration/prototype/round/total-duration-nanoseconds-too-large-with-zoned-datetime.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: case where float… | `built-ins/Temporal/Duration/prototype/subtract/argument-duration-precision-exact-numerical-values.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: toString() shoul… | `built-ins/Temporal/Duration/prototype/subtract/float64-representable-integer.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: duration1.subtra… | `built-ins/Temporal/Duration/prototype/subtract/precision-no-floating-point-loss.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: subtracting the … | `built-ins/Temporal/Duration/prototype/subtract/result-out-of-range-1.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected a Range… | `built-ins/Temporal/Duration/prototype/subtract/result-out-of-range-3.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: Temporal.Duration: … | `built-ins/Temporal/Duration/prototype/total/order-of-operations.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: hour property ca… | `built-ins/Temporal/Duration/prototype/total/relativeto-infinity-throws-rangeerror.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected a Range… | `built-ins/Temporal/Duration/prototype/total/throws-if-unit-property-missing.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Throws TypeError… | `built-ins/Temporal/Duration/prototype/with/argument-invalid-property.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Throw TypeError … | `built-ins/Temporal/Duration/prototype/with/argument-singular-properties.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Instance of Cust… | `built-ins/Temporal/Duration/subclass.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Minimum to maxim… | `built-ins/Temporal/Instant/prototype/add/minimum-maximum-instant.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Duration.p.toStr… | `built-ins/Temporal/Instant/prototype/since/float64-representable-integer.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: does not include… | `built-ins/Temporal/Instant/prototype/since/largestunit.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected SameVal… | `built-ins/Temporal/Instant/prototype/subtract/minimum-maximum-instant.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected a Test2… | `built-ins/Temporal/Instant/prototype/toString/get-timezone-throws.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [get opti… | `built-ins/Temporal/Instant/prototype/toString/options-read-before-algorithmic-validation.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [get opti… | `built-ins/Temporal/Instant/prototype/toString/order-of-operations.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Duration.p.toStr… | `built-ins/Temporal/Instant/prototype/until/float64-representable-integer.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Instance of Cust… | `built-ins/Temporal/Instant/subclass.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: prototype Expect… | `built-ins/Temporal/Now/builtin.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: The result of ev… | `built-ins/Temporal/Now/instant/return-value-value.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Now descriptor s… | `built-ins/Temporal/Now/prop-desc.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: String: TypeError | `built-ins/Temporal/Now/toStringTag/string.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: P1M weeks..month… | `built-ins/Temporal/PlainDate/prototype/since/exact-multiple-of-larger-unit.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected a Test2… | `built-ins/Temporal/PlainDate/prototype/toZonedDateTime/argument-object-get-plainTime-throws.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected a Test2… | `built-ins/Temporal/PlainDate/prototype/toZonedDateTime/argument-object-get-timezone-throws.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: Temporal.PlainDate:… | `built-ins/Temporal/PlainDate/prototype/toZonedDateTime/order-of-operations.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: P1M weeks..month… | `built-ins/Temporal/PlainDate/prototype/until/exact-multiple-of-larger-unit.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Partial date pro… | `built-ins/Temporal/PlainDate/prototype/with/options-wrong-type.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [] and ex… | `built-ins/Temporal/PlainDate/prototype/with/order-of-operations.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Instance of Cust… | `built-ins/Temporal/PlainDate/subclass.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: P1M weeks..month… | `built-ins/Temporal/PlainDateTime/prototype/since/exact-multiple-of-larger-unit.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Duration.p.toStr… | `built-ins/Temporal/PlainDateTime/prototype/since/float64-representable-integer.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [] and ex… | `built-ins/Temporal/PlainDateTime/prototype/toZonedDateTime/options-read-before-algorithmic-validation.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [] and ex… | `built-ins/Temporal/PlainDateTime/prototype/toZonedDateTime/order-of-operations.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: P1M weeks..month… | `built-ins/Temporal/PlainDateTime/prototype/until/exact-multiple-of-larger-unit.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Duration.p.toStr… | `built-ins/Temporal/PlainDateTime/prototype/until/float64-representable-integer.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: can return lower… | `built-ins/Temporal/PlainDateTime/prototype/until/units-changed.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Partial datetime… | `built-ins/Temporal/PlainDateTime/prototype/with/options-wrong-type.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [] and ex… | `built-ins/Temporal/PlainDateTime/prototype/with/order-of-operations.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Instance of Cust… | `built-ins/Temporal/PlainDateTime/subclass.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: Temporal.PlainMonth… | `built-ins/Temporal/PlainMonthDay/from/fields-object.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [get fiel… | `built-ins/Temporal/PlainMonthDay/prototype/toPlainDate/order-of-operations.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Partial date pro… | `built-ins/Temporal/PlainMonthDay/prototype/with/options-wrong-type.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [] and ex… | `built-ins/Temporal/PlainMonthDay/prototype/with/order-of-operations.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Instance of Cust… | `built-ins/Temporal/PlainMonthDay/subclass.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: millisecond < Ex… | `built-ins/Temporal/PlainTime/compare/exhaustive.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [get opti… | `built-ins/Temporal/PlainTime/from/order-of-operations.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [get fiel… | `built-ins/Temporal/PlainTime/prototype/with/order-of-operations.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: PlainDateTime Ex… | `built-ins/Temporal/PlainTime/prototype/with/plaintimelike-invalid.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: temporalTimeLike… | `built-ins/Temporal/PlainTime/prototype/with/throws-if-time-is-invalid-when-overflow-is-reject.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Instance of Cust… | `built-ins/Temporal/PlainTime/subclass.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: Temporal.PlainYearM… | `built-ins/Temporal/PlainYearMonth/from/argument-plaindate.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: P1Y months..year… | `built-ins/Temporal/PlainYearMonth/prototype/since/exact-multiple-of-larger-unit.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [get fiel… | `built-ins/Temporal/PlainYearMonth/prototype/toPlainDate/order-of-operations.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: P1Y months..year… | `built-ins/Temporal/PlainYearMonth/prototype/until/exact-multiple-of-larger-unit.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Partial date pro… | `built-ins/Temporal/PlainYearMonth/prototype/with/options-wrong-type.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [] and ex… | `built-ins/Temporal/PlainYearMonth/prototype/with/order-of-operations.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Instance of Cust… | `built-ins/Temporal/PlainYearMonth/subclass.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: Temporal.ZonedDateT… | `built-ins/Temporal/ZonedDateTime/argument-convert.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [get one.… | `built-ins/Temporal/ZonedDateTime/compare/order-of-operations.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: RangeError: Temporal.ZonedDate… | `built-ins/Temporal/ZonedDateTime/from/argument-string-limits.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: RangeError: Temporal.ZonedDate… | `built-ins/Temporal/ZonedDateTime/from/offset-overrides-critical-flag.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: UTC offset synta… | `built-ins/Temporal/ZonedDateTime/from/offset-string-invalid.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [get item… | `built-ins/Temporal/ZonedDateTime/from/order-of-operations.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: RangeError: Temporal.ZonedDate… | `built-ins/Temporal/ZonedDateTime/from/zoneddatetime-string.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [get othe… | `built-ins/Temporal/ZonedDateTime/prototype/equals/order-of-operations.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected a Range… | `built-ins/Temporal/ZonedDateTime/prototype/getTimeZoneTransition/direction-undefined.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: Temporal.ZonedDateT… | `built-ins/Temporal/ZonedDateTime/prototype/getTimeZoneTransition/order-of-operations.js` |

