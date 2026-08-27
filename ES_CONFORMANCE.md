# Test262 conformance baseline

- **Engine commit:** `022bc4aa4f66a98b2877f622d21c70075b8f0c14`
- **Test262 commit:** `be13516fb6441b950ba8a3df97eb34062c186972`
- **Captured:** 2026-08-27T21:26:59.369214+00:00

## Totals

| Bucket | Count |
|---|---|
| total      | 53459 |
| passed     | 52260 |
| failed     | 99 |
| skipped    | 1100 |
| crashed    | 0 |
| timed_out  | 0 |
| oom        | 0 |

**Pass rate (excl. skipped):** 99.81%

## Top failing sections (top 50)

| Section | total | passed | failed | pass-rate |
|---|---:|---:|---:|---:|
| intl402/DateTimeFormat/prototype | 180 | 171 | 9 | 95.0% |
| annexB/language/expressions | 26 | 19 | 7 | 73.1% |
| language/eval-code/direct | 286 | 280 | 6 | 97.9% |
| staging/sm/RegExp | 91 | 85 | 6 | 93.4% |
| staging/sm/TypedArray | 96 | 91 | 5 | 94.8% |
| staging/sm/expressions | 42 | 37 | 5 | 88.1% |
| staging/sm/regress | 106 | 101 | 5 | 95.3% |
| staging/sm/Array | 90 | 86 | 4 | 95.6% |
| staging/sm/Function | 53 | 49 | 4 | 92.5% |
| staging/sm/PrivateName | 17 | 13 | 4 | 76.5% |
| staging/sm/extensions | 64 | 60 | 4 | 93.8% |
| staging/sm/strict | 51 | 47 | 4 | 92.2% |
| staging/sm/eval | 20 | 17 | 3 | 85.0% |
| staging/sm/fields | 8 | 5 | 3 | 62.5% |
| language/import/import-defer | 103 | 100 | 2 | 98.0% |
| staging/sm/BigInt | 5 | 3 | 2 | 60.0% |
| staging/sm/Proxy | 24 | 22 | 2 | 91.7% |
| staging/sm/Reflect | 17 | 15 | 2 | 88.2% |
| staging/sm/class | 94 | 92 | 2 | 97.9% |
| staging/sm/object | 65 | 63 | 2 | 96.9% |
| intl402/Array/prototype | 2 | 1 | 1 | 50.0% |
| intl402/BigInt/prototype | 11 | 10 | 1 | 90.9% |
| intl402/Locale/constructor-non-iana-canon.js | 1 | 0 | 1 | 0.0% |
| intl402/constructors-string-and-single-element-array.js | 1 | 0 | 1 | 0.0% |
| language/expressions/call | 92 | 90 | 1 | 98.9% |
| language/expressions/tagged-template | 27 | 25 | 1 | 96.2% |
| staging/sm/ArrayBuffer | 5 | 4 | 1 | 80.0% |
| staging/sm/Date | 28 | 27 | 1 | 96.4% |
| staging/sm/Error | 3 | 2 | 1 | 66.7% |
| staging/sm/Math | 30 | 29 | 1 | 96.7% |
| staging/sm/String | 47 | 45 | 1 | 97.8% |
| staging/sm/Symbol | 30 | 29 | 1 | 96.7% |
| staging/sm/global | 16 | 15 | 1 | 93.8% |
| staging/sm/lexical-environment | 34 | 33 | 1 | 97.1% |
| staging/sm/misc | 18 | 17 | 1 | 94.4% |
| staging/sm/module | 5 | 4 | 1 | 80.0% |
| staging/sm/statements | 17 | 16 | 1 | 94.1% |
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
| annexB/language/function-code | 159 | 159 | 0 | 100.0% |

## Top failing-test patterns (top 100)

| Outcome | Reason (truncated) | Path |
|---|---|---|
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: ReferenceError: argume… | `language/eval-code/direct/arrow-fn-body-cntns-arguments-func-decl-arrow-func-declare-arguments-assign-incl-def-param-arrow-arguments.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: ReferenceError: argume… | `language/eval-code/direct/arrow-fn-body-cntns-arguments-var-bind-arrow-func-declare-arguments-assign-incl-def-param-arrow-arguments.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: direct e… | `language/eval-code/direct/global-env-rec-with.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: non stri… | `language/eval-code/direct/lex-env-heritage.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: binding … | `language/eval-code/direct/var-env-func-init-local-new-delete.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `language/eval-code/direct/var-env-var-init-local-new-delete.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: fooCalle… | `language/expressions/call/11.2.3-3_3.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: ReferenceError: a is n… | `language/expressions/tagged-template/cache-eval-inner-function.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Static and dynam… | `language/import/import-defer/deferred-namespace-object/identity.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [B, A-bef… | `language/import/import-defer/evaluation-top-level-await/async-cycle-dependency-of-deferred-module/main.js` |
| fail | sloppy: compile: codes=[SYNTAX_ERROR] messages=[Cannot assign to this expression… | `annexB/language/expressions/assignmenttargettype/callexpression-as-for-in-lhs.js` |
| fail | sloppy: compile: codes=[SYNTAX_ERROR] messages=[Cannot assign to this expression… | `annexB/language/expressions/assignmenttargettype/callexpression-as-for-of-lhs.js` |
| fail | sloppy: compile: codes=[SYNTAX_ERROR] messages=[Cannot assign to this expression… | `annexB/language/expressions/assignmenttargettype/callexpression-in-compound-assignment.js` |
| fail | sloppy: compile: codes=[SYNTAX_ERROR] messages=[Cannot assign to this expression… | `annexB/language/expressions/assignmenttargettype/callexpression-in-postfix-update.js` |
| fail | sloppy: compile: codes=[SYNTAX_ERROR] messages=[Cannot assign to this expression… | `annexB/language/expressions/assignmenttargettype/callexpression-in-prefix-update.js` |
| fail | sloppy: compile: codes=[SYNTAX_ERROR] messages=[Cannot assign to this expression… | `annexB/language/expressions/assignmenttargettype/callexpression.js` |
| fail | sloppy: compile: codes=[SYNTAX_ERROR] messages=[Cannot assign to this expression… | `annexB/language/expressions/assignmenttargettype/cover-callexpression-and-asyncarrowhead.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/Array/from-iterator-close.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/Array/from_proxy.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/Array/species.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Assertio… | `staging/sm/Array/to-length.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/ArrayBuffer/slice-species.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/BigInt/Number-conversion-rounding.js` |
| fail | sloppy: compile: codes=[FEATURE_NOT_IN_SLICE] messages=[unsupported AST node: Cl… | `staging/sm/BigInt/property-name.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/Date/non-iso.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/Error/AggregateError.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/Function/arguments-parameter-shadowing.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Incorrec… | `staging/sm/Function/function-toString-builtin-name.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/Function/invalid-parameter-list.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [… | `staging/sm/Function/strict-arguments.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Error: got -6.99823708… | `staging/sm/Math/atanh-approx.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: Cannot read… | `staging/sm/PrivateName/constructor-args.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: Private met… | `staging/sm/PrivateName/modify-non-extensible.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/PrivateName/names.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/PrivateName/not-iterable.js` |
| fail | strict: runtime: TypeError (UNCAUGHT) uncaught exception: ReferenceError: barewo… | `staging/sm/Proxy/global-receiver.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/Proxy/regress-bug950407.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/Reflect/construct.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: Reflect: Pr… | `staging/sm/Reflect/set.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/RegExp/constructor-constructor.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/RegExp/constructor-ordering.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/RegExp/constructor-regexp.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/RegExp/ignoreCase-multiple.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/RegExp/ignoreCase-non-latin1-to-latin1.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/RegExp/lastIndex-match-or-replace.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/String/internalUsage.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/Symbol/property-reflection.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/TypedArray/constructor-buffer-sequence.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/TypedArray/from_constructor.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: Cannot read… | `staging/sm/TypedArray/iterator-next-with-detached.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: Cannot read… | `staging/sm/TypedArray/set-wrapped.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/TypedArray/slice-bitwise-same.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: ReferenceError: C is n… | `staging/sm/class/fields-static-class-name-binding-eval.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/class/strictExecution.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/eval/exhaustive-fun-normalcaller-direct-normalcode.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: ReferenceError: argume… | `staging/sm/eval/redeclared-arguments-in-param-expression-eval.js` |
| fail | strict: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/eval/undeclared-name-in-nested-strict-eval.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: numeric … | `staging/sm/expressions/11.1.5-01.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: type mismat… | `staging/sm/expressions/ToPropertyKey-symbols.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: computed… | `staging/sm/expressions/object-literal-__proto__.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/expressions/short-circuit-compound-assignment-anon-fns.js` |
| fail | sloppy: compile: codes=[FEATURE_NOT_IN_SLICE] messages=[unsupported AST node: Su… | `staging/sm/expressions/short-circuit-compound-assignment.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/extensions/arguments-property-access-in-function.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/extensions/dataview.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: get __proto… | `staging/sm/extensions/destructuring-for-inof-__proto__.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: Error.proto… | `staging/sm/extensions/error-tostring-function.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262: This statement should… | `staging/sm/fields/await-identifier-module-2.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/fields/init-order.js` |
| fail | sloppy: compile: codes=[FEATURE_NOT_IN_SLICE] messages=[unsupported AST node: Cl… | `staging/sm/fields/numeric-fields.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/global/eval-02.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/lexical-environment/block-scoped-functions-annex-b-arguments.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: SyntaxError: Unexpecte… | `staging/sm/misc/future-reserved-words.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Test262: This statement should… | `staging/sm/module/await-restricted-nested.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/object/defineProperties-order.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: toString: C… | `staging/sm/object/duplProps.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: proxy ca… | `staging/sm/regress/regress-1383630.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: group as… | `staging/sm/regress/regress-469625-02.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: function… | `staging/sm/regress/regress-602621.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: Cannot read… | `staging/sm/regress/regress-634210-4.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/regress/regress-665355.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/statements/regress-642975.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/strict/15.3.5.2.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/strict/15.5.5.1.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `staging/sm/strict/8.12.7-2.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: wrong er… | `staging/sm/strict/directive-prologue-01.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: SyntaxError: Unexpecte… | `staging/sm/syntax/let-as-label.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Actual [… | `intl402/Array/prototype/toLocaleString/invoke-element-tolocalestring.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `intl402/BigInt/prototype/toLocaleString/de-DE.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `intl402/DateTimeFormat/prototype/formatRange/en-US.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: month co… | `intl402/DateTimeFormat/prototype/formatRangeToParts/chinese-calendar-dates.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: month co… | `intl402/DateTimeFormat/prototype/formatRangeToParts/dangi-calendar-dates.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: Expected… | `intl402/DateTimeFormat/prototype/formatRangeToParts/en-US.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: month co… | `intl402/DateTimeFormat/prototype/formatToParts/chinese-calendar-dates.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: date = 2… | `intl402/DateTimeFormat/prototype/formatToParts/compare-to-temporal-lunisolar.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: date = 2… | `intl402/DateTimeFormat/prototype/formatToParts/compare-to-temporal.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: month co… | `intl402/DateTimeFormat/prototype/formatToParts/dangi-calendar-dates.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: islamic-… | `intl402/DateTimeFormat/prototype/formatToParts/era.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: RangeError: Locale: in… | `intl402/Locale/constructor-non-iana-canon.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: Array.proto… | `intl402/constructors-string-and-single-element-array.js` |

