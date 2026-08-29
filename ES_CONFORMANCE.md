# Test262 conformance baseline

- **Engine commit:** `fb29f6ab6851075450af3a7c242780f3f4905e02`
- **Test262 commit:** `be13516fb6441b950ba8a3df97eb34062c186972`
- **Captured:** 2026-08-29T21:19:07.883685+00:00

## Totals

| Bucket | Count |
|---|---|
| total      | 53575 |
| passed     | 52425 |
| failed     | 50 |
| skipped    | 1100 |
| crashed    | 0 |
| timed_out  | 0 |
| oom        | 0 |

**Pass rate (excl. skipped):** 99.90%

## Top failing sections (top 50)

| Section | total | passed | failed | pass-rate |
|---|---:|---:|---:|---:|
| intl402/DateTimeFormat/prototype | 180 | 171 | 9 | 95.0% |
| staging/sm/TypedArray | 96 | 91 | 5 | 94.8% |
| staging/sm/regress | 106 | 101 | 5 | 95.3% |
| staging/sm/RegExp | 91 | 87 | 4 | 95.6% |
| staging/sm/Array | 90 | 87 | 3 | 96.7% |
| staging/sm/expressions | 42 | 39 | 3 | 92.9% |
| staging/sm/Function | 53 | 51 | 2 | 96.2% |
| staging/sm/Proxy | 24 | 22 | 2 | 91.7% |
| staging/sm/extensions | 64 | 62 | 2 | 96.9% |
| language/eval-code/direct | 286 | 285 | 1 | 99.7% |
| language/expressions/call | 92 | 90 | 1 | 98.9% |
| staging/sm/ArrayBuffer | 5 | 4 | 1 | 80.0% |
| staging/sm/BigInt | 5 | 4 | 1 | 80.0% |
| staging/sm/Date | 28 | 27 | 1 | 96.4% |
| staging/sm/Math | 30 | 29 | 1 | 96.7% |
| staging/sm/Reflect | 17 | 16 | 1 | 94.1% |
| staging/sm/String | 47 | 45 | 1 | 97.8% |
| staging/sm/fields | 8 | 7 | 1 | 87.5% |
| staging/sm/lexical-environment | 34 | 33 | 1 | 97.1% |
| staging/sm/misc | 18 | 17 | 1 | 94.4% |
| staging/sm/module | 5 | 4 | 1 | 80.0% |
| staging/sm/statements | 17 | 16 | 1 | 94.1% |
| staging/sm/strict | 51 | 50 | 1 | 98.0% |
| staging/sm/syntax | 11 | 10 | 1 | 90.9% |
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
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: direct e… | `language/eval-code/direct/global-env-rec-with.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: fooCalle… | `language/expressions/call/11.2.3-3_3.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/Array/from-iterator-close.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/Array/from_proxy.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Assertio… | `staging/sm/Array/to-length.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/ArrayBuffer/slice-species.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/BigInt/Number-conversion-rounding.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/Date/non-iso.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Incorrec… | `staging/sm/Function/function-toString-builtin-name.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/Function/invalid-parameter-list.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Error: got -6.99823708… | `staging/sm/Math/atanh-approx.js` |
| fail | strict: runtime: TypeError (UNCAUGHT) uncaught exception: ReferenceError: barewo… | `staging/sm/Proxy/global-receiver.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/Proxy/regress-bug950407.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/Reflect/construct.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/RegExp/constructor-ordering.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/RegExp/ignoreCase-multiple.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/RegExp/ignoreCase-non-latin1-to-latin1.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/RegExp/lastIndex-match-or-replace.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/String/internalUsage.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/TypedArray/constructor-buffer-sequence.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/TypedArray/from_constructor.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: Cannot read… | `staging/sm/TypedArray/iterator-next-with-detached.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: Cannot read… | `staging/sm/TypedArray/set-wrapped.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/TypedArray/slice-bitwise-same.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: numeric … | `staging/sm/expressions/11.1.5-01.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/expressions/short-circuit-compound-assignment-anon-fns.js` |
| fail | sloppy: compile: codes=[FEATURE_NOT_IN_SLICE] messages=[unsupported AST node: Su… | `staging/sm/expressions/short-circuit-compound-assignment.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/extensions/arguments-property-access-in-function.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/extensions/dataview.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262: This statement should… | `staging/sm/fields/await-identifier-module-2.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/lexical-environment/block-scoped-functions-annex-b-arguments.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: SyntaxError: Unexpecte… | `staging/sm/misc/future-reserved-words.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262: This statement should… | `staging/sm/module/await-restricted-nested.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: proxy ca… | `staging/sm/regress/regress-1383630.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: group as… | `staging/sm/regress/regress-469625-02.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: function… | `staging/sm/regress/regress-602621.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: Cannot read… | `staging/sm/regress/regress-634210-4.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/regress/regress-665355.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/statements/regress-642975.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: wrong er… | `staging/sm/strict/directive-prologue-01.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: SyntaxError: Unexpecte… | `staging/sm/syntax/let-as-label.js` |

