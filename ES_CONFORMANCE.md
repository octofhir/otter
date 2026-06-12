# Test262 conformance baseline

- **Engine commit:** `9cf6c244852130acfb83a74e624660dbdcdf4420`
- **Test262 commit:** `7e115f46ac64340827d505fa928ad436cb7ba5a6`
- **Captured:** 2026-06-12T22:21:42.325561+00:00

## Totals

| Bucket | Count |
|---|---|
| total      | 53173 |
| passed     | 50151 |
| failed     | 1753 |
| skipped    | 1256 |
| crashed    | 0 |
| timed_out  | 12 |
| oom        | 1 |

**Pass rate (excl. skipped):** 96.60%

## Top failing sections (top 50)

| Section | total | passed | failed | pass-rate |
|---|---:|---:|---:|---:|
| intl402/DateTimeFormat/prototype | 180 | 30 | 150 | 16.7% |
| intl402/NumberFormat/prototype | 179 | 60 | 119 | 33.5% |
| intl402/Temporal/ZonedDateTime | 583 | 480 | 103 | 82.3% |
| intl402/Locale/prototype | 91 | 0 | 91 | 0.0% |
| intl402/DurationFormat/prototype | 81 | 0 | 81 | 0.0% |
| built-ins/Temporal/ZonedDateTime | 901 | 845 | 56 | 93.8% |
| intl402/Intl/getCanonicalLocales | 38 | 0 | 38 | 0.0% |
| intl402/ListFormat/prototype | 49 | 16 | 33 | 32.7% |
| built-ins/Temporal/Duration | 540 | 507 | 32 | 94.1% |
| intl402/Segmenter/constructor | 37 | 8 | 28 | 22.2% |
| intl402/RelativeTimeFormat/prototype | 44 | 17 | 27 | 38.6% |
| intl402/RelativeTimeFormat/constructor | 34 | 7 | 26 | 21.2% |
| intl402/Intl/supportedValuesOf | 25 | 0 | 25 | 0.0% |
| staging/sm/Iterator | 174 | 149 | 25 | 85.6% |
| intl402/ListFormat/constructor | 30 | 5 | 24 | 17.2% |
| staging/sm/RegExp | 91 | 69 | 22 | 75.8% |
| staging/sm/class | 94 | 72 | 22 | 76.6% |
| intl402/Segmenter/prototype | 36 | 15 | 21 | 41.7% |
| intl402/Temporal/PlainDate | 493 | 473 | 20 | 95.9% |
| staging/sm/Function | 53 | 33 | 20 | 62.3% |
| intl402/Temporal/PlainDateTime | 483 | 464 | 19 | 96.1% |
| intl402/Collator/prototype | 35 | 18 | 17 | 51.4% |
| staging/sm/regress | 106 | 88 | 17 | 83.0% |
| intl402/Temporal/PlainYearMonth | 327 | 311 | 16 | 95.1% |
| staging/sm/expressions | 42 | 27 | 15 | 64.3% |
| intl402/PluralRules/prototype | 34 | 20 | 14 | 58.8% |
| intl402/Temporal/PlainMonthDay | 90 | 76 | 14 | 84.4% |
| staging/sm/Array | 90 | 76 | 14 | 84.4% |
| intl402/Temporal/Instant | 17 | 4 | 13 | 23.5% |
| built-ins/Atomics/waitAsync | 101 | 89 | 12 | 88.1% |
| built-ins/Temporal/PlainDateTime | 773 | 760 | 12 | 98.4% |
| language/statements/class | 4367 | 4355 | 12 | 99.7% |
| staging/sm/extensions | 64 | 51 | 12 | 81.0% |
| built-ins/Temporal/Instant | 465 | 454 | 11 | 97.6% |
| intl402/String/prototype | 19 | 8 | 11 | 42.1% |
| intl402/Temporal/PlainTime | 12 | 1 | 11 | 8.3% |
| built-ins/Temporal/PlainDate | 652 | 642 | 10 | 98.5% |
| staging/sm/lexical-environment | 34 | 24 | 10 | 70.6% |
| staging/sm/object | 65 | 55 | 10 | 84.6% |
| built-ins/Temporal/PlainYearMonth | 509 | 500 | 9 | 98.2% |
| staging/sm/Proxy | 24 | 15 | 9 | 62.5% |
| staging/sm/TypedArray | 96 | 87 | 9 | 90.6% |
| built-ins/Temporal/PlainTime | 493 | 485 | 8 | 98.4% |
| intl402/DurationFormat/supportedLocalesOf | 8 | 0 | 8 | 0.0% |
| language/expressions/call | 92 | 83 | 8 | 91.2% |
| language/expressions/class | 4059 | 4041 | 8 | 99.8% |
| built-ins/Temporal/PlainMonthDay | 199 | 192 | 7 | 96.5% |
| intl402/BigInt/prototype | 11 | 4 | 7 | 36.4% |
| intl402/DisplayNames/prototype | 20 | 13 | 7 | 65.0% |
| language/computed-property-names/class | 29 | 22 | 7 | 75.9% |

## Top failing-test patterns (top 100)

| Outcome | Reason (truncated) | Path |
|---|---|---|
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected SameVal… | `built-ins/AsyncFunction/AsyncFunction-is-extensible.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Atomics.add(new … | `built-ins/Atomics/add/non-shared-int-views-throws.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected a TypeE… | `built-ins/Atomics/add/validate-arraytype-before-index-coercion.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected a TypeE… | `built-ins/Atomics/add/validate-arraytype-before-value-coercion.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Atomics.and(new … | `built-ins/Atomics/and/non-shared-int-views-throws.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected a TypeE… | `built-ins/Atomics/and/validate-arraytype-before-index-coercion.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected a TypeE… | `built-ins/Atomics/and/validate-arraytype-before-value-coercion.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Atomics.compareE… | `built-ins/Atomics/compareExchange/non-shared-int-views-throws.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected a TypeE… | `built-ins/Atomics/compareExchange/validate-arraytype-before-expectedValue-coercion.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected a TypeE… | `built-ins/Atomics/compareExchange/validate-arraytype-before-index-coercion.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected a TypeE… | `built-ins/Atomics/compareExchange/validate-arraytype-before-replacementValue-coercion.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Atomics.exchange… | `built-ins/Atomics/exchange/non-shared-int-views-throws.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected a TypeE… | `built-ins/Atomics/exchange/validate-arraytype-before-index-coercion.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected a TypeE… | `built-ins/Atomics/exchange/validate-arraytype-before-value-coercion.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Atomics.load(new… | `built-ins/Atomics/load/non-shared-int-views-throws.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected a TypeE… | `built-ins/Atomics/load/validate-arraytype-before-index-coercion.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Atomics.or(new F… | `built-ins/Atomics/or/non-shared-int-views-throws.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected a TypeE… | `built-ins/Atomics/or/validate-arraytype-before-index-coercion.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected a TypeE… | `built-ins/Atomics/or/validate-arraytype-before-value-coercion.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Atomics.store(ne… | `built-ins/Atomics/store/non-shared-int-views-throws.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected a TypeE… | `built-ins/Atomics/store/validate-arraytype-before-index-coercion.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected a TypeE… | `built-ins/Atomics/store/validate-arraytype-before-value-coercion.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Atomics.sub(new … | `built-ins/Atomics/sub/non-shared-int-views-throws.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected a TypeE… | `built-ins/Atomics/sub/validate-arraytype-before-index-coercion.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected a TypeE… | `built-ins/Atomics/sub/validate-arraytype-before-value-coercion.js` |
| timeout | timeout after 10004 ms | `built-ins/Atomics/wait/bigint/waiterlist-order-of-operations-is-fifo.js` |
| timeout | timeout after 10002 ms | `built-ins/Atomics/wait/waiterlist-order-of-operations-is-fifo.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: The value of Ato… | `built-ins/Atomics/waitAsync/bigint/false-for-timeout.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: The value of Ato… | `built-ins/Atomics/waitAsync/bigint/null-for-timeout.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: The value of Ato… | `built-ins/Atomics/waitAsync/bigint/object-for-timeout.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: setTimeout: host ru… | `built-ins/Atomics/waitAsync/bigint/true-for-timeout.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: The value of Ato… | `built-ins/Atomics/waitAsync/false-for-timeout.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: The value of Ato… | `built-ins/Atomics/waitAsync/null-for-timeout.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: The value of Ato… | `built-ins/Atomics/waitAsync/object-for-timeout.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: The value of `as… | `built-ins/Atomics/waitAsync/returns-result-object-value-is-promise-resolves-to-ok.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: setTimeout: host ru… | `built-ins/Atomics/waitAsync/returns-result-object-value-is-promise-resolves-to-timed-out.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: The value of `va… | `built-ins/Atomics/waitAsync/returns-result-object-value-is-string-not-equal.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: The value of `va… | `built-ins/Atomics/waitAsync/returns-result-object-value-is-string-timed-out.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: setTimeout: host ru… | `built-ins/Atomics/waitAsync/true-for-timeout.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Atomics.xor(new … | `built-ins/Atomics/xor/non-shared-int-views-throws.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected a TypeE… | `built-ins/Atomics/xor/validate-arraytype-before-index-coercion.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected a TypeE… | `built-ins/Atomics/xor/validate-arraytype-before-value-coercion.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: #1: If B = 110xx… | `built-ins/decodeURI/S15.1.3.1_A1.10_T1.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: #1: If B = 1110x… | `built-ins/decodeURI/S15.1.3.1_A1.11_T1.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: #1: If B = 1110x… | `built-ins/decodeURI/S15.1.3.1_A1.11_T2.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: #1: If B = 11110… | `built-ins/decodeURI/S15.1.3.1_A1.12_T1.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: #1: If B = 11110… | `built-ins/decodeURI/S15.1.3.1_A1.12_T2.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: #1: If B = 11110… | `built-ins/decodeURI/S15.1.3.1_A1.12_T3.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: #1: If string.ch… | `built-ins/decodeURI/S15.1.3.1_A1.2_T1.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: #1: If string.ch… | `built-ins/decodeURI/S15.1.3.1_A1.2_T2.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: #D800 differs | `built-ins/decodeURI/S15.1.3.1_A2.1_T1.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: #1: decodeURI("%… | `built-ins/decodeURI/S15.1.3.1_A3_T1.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: #1: decodeURI("%… | `built-ins/decodeURI/S15.1.3.1_A3_T2.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: #1: decodeURI("%… | `built-ins/decodeURI/S15.1.3.1_A3_T3.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: #3: http://ru.wi… | `built-ins/decodeURI/S15.1.3.1_A4_T2.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: #2: http:%2f%2Fu… | `built-ins/decodeURI/S15.1.3.1_A4_T4.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: #1: If B = 11110… | `built-ins/decodeURIComponent/S15.1.3.2_A1.12_T3.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: FinalizationRegistr… | `built-ins/FinalizationRegistry/prototype/register/return-undefined-register-object.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected a TypeE… | `built-ins/global/10.2.1.1.3-4-16-s.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected a TypeE… | `built-ins/global/10.2.1.1.3-4-18-s.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: globalThis descr… | `built-ins/global/property-descriptor.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Math.hypot propa… | `built-ins/Math/hypot/Math.hypot_ToNumberErr.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: The result of ev… | `built-ins/Math/round/S15.8.2.15_A7.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: #1: parseFloat("… | `built-ins/parseFloat/S15.1.2.3_A4_T2.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: #002B  | `built-ins/parseFloat/S15.1.2.3_A6.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: SyntaxError: RegExp: invalid r… | `built-ins/RegExp/S15.10.2.8_A3_T15.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: SyntaxError: RegExp: invalid r… | `built-ins/RegExp/S15.10.2.8_A3_T16.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [xabcd, a… | `built-ins/RegExp/lookBehind/alternations.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [c, abab]… | `built-ins/RegExp/lookBehind/back-references-to-captures.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual argument … | `built-ins/RegExp/lookBehind/mutual-recursive.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [a, abc] … | `built-ins/RegExp/lookahead-quantifier-match-groups.js` |
| fail | compile: codes=[FEATURE_NOT_IN_SLICE] messages=[unsupported AST node: RegExpLite… | `built-ins/RegExp/property-escapes/generated/White_Space.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: SyntaxError: RegExp: invalid r… | `built-ins/RegExp/quantifier-integer-limit.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: \b should match … | `built-ins/RegExp/regexp-modifiers/add-ignoreCase-affects-slash-lower-b.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: \B should match … | `built-ins/RegExp/regexp-modifiers/add-ignoreCase-affects-slash-upper-b.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: \P{Lu} should ma… | `built-ins/RegExp/regexp-modifiers/add-ignoreCase-affects-slash-upper-p.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: \u1fd3 does not … | `built-ins/RegExp/unicode_full_case_folding.js` |
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

