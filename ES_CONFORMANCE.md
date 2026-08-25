# Test262 conformance baseline

- **Engine commit:** `37922aa96e52b8bb7086820e1ace60fe70e8c37b`
- **Test262 commit:** `be13516fb6441b950ba8a3df97eb34062c186972`
- **Captured:** 2026-08-25T12:09:04.807245+00:00

## Totals

| Bucket | Count |
|---|---|
| total      | 53459 |
| passed     | 51737 |
| failed     | 622 |
| skipped    | 1100 |
| crashed    | 0 |
| timed_out  | 0 |
| oom        | 0 |

**Pass rate (excl. skipped):** 98.81%

## Top failing sections (top 50)

| Section | total | passed | failed | pass-rate |
|---|---:|---:|---:|---:|
| intl402/DateTimeFormat/prototype | 180 | 143 | 37 | 79.4% |
| built-ins/Temporal/ZonedDateTime | 901 | 871 | 30 | 96.7% |
| built-ins/Temporal/Duration | 540 | 512 | 28 | 94.8% |
| intl402/Temporal/ZonedDateTime | 583 | 557 | 26 | 95.5% |
| intl402/NumberFormat/prototype | 179 | 155 | 24 | 86.6% |
| language/expressions/async-generator | 623 | 602 | 20 | 96.8% |
| language/expressions/class | 4059 | 4022 | 17 | 99.6% |
| language/statements/class | 4367 | 4336 | 17 | 99.6% |
| staging/sm/regress | 106 | 91 | 15 | 85.8% |
| intl402/Locale/prototype | 107 | 93 | 14 | 86.9% |
| staging/sm/class | 94 | 80 | 14 | 85.1% |
| built-ins/Promise/allSettledKeyed | 44 | 32 | 12 | 72.7% |
| intl402/Temporal/PlainDate | 493 | 481 | 12 | 97.6% |
| language/expressions/compound-assignment | 454 | 443 | 11 | 97.6% |
| language/statements/async-generator | 301 | 290 | 11 | 96.3% |
| staging/sm/RegExp | 91 | 80 | 11 | 87.9% |
| built-ins/Atomics/waitAsync | 101 | 91 | 10 | 90.1% |
| built-ins/Promise/prototype | 124 | 114 | 10 | 91.9% |
| intl402/Temporal/PlainDateTime | 483 | 473 | 10 | 97.9% |
| built-ins/Promise/allKeyed | 45 | 36 | 9 | 80.0% |
| intl402/Temporal/PlainYearMonth | 327 | 318 | 9 | 97.2% |
| staging/sm/Array | 90 | 81 | 9 | 90.0% |
| staging/sm/Date | 28 | 19 | 9 | 67.9% |
| staging/sm/extensions | 64 | 55 | 9 | 85.9% |
| staging/sm/lexical-environment | 34 | 25 | 9 | 73.5% |
| staging/sm/Function | 53 | 45 | 8 | 84.9% |
| staging/sm/expressions | 42 | 34 | 8 | 81.0% |
| annexB/language/expressions | 26 | 19 | 7 | 73.1% |
| intl402/Temporal/PlainMonthDay | 90 | 83 | 7 | 92.2% |
| language/eval-code/direct | 286 | 279 | 7 | 97.6% |
| language/expressions/object | 1170 | 1163 | 7 | 99.4% |
| staging/sm/TypedArray | 96 | 89 | 7 | 92.7% |
| built-ins/Temporal/Instant | 465 | 459 | 6 | 98.7% |
| built-ins/Temporal/PlainDateTime | 773 | 767 | 6 | 99.2% |
| built-ins/Temporal/PlainTime | 493 | 487 | 6 | 98.8% |
| intl402/DurationFormat/prototype | 81 | 75 | 6 | 92.6% |
| staging/sm/Proxy | 24 | 18 | 6 | 75.0% |
| built-ins/AsyncGeneratorPrototype/return | 19 | 14 | 5 | 73.7% |
| built-ins/Temporal/PlainMonthDay | 199 | 194 | 5 | 97.5% |
| built-ins/Temporal/PlainYearMonth | 509 | 504 | 5 | 99.0% |
| intl402/PluralRules/prototype | 34 | 29 | 5 | 85.3% |
| intl402/Temporal/Instant | 17 | 12 | 5 | 70.6% |
| language/expressions/dynamic-import | 1005 | 762 | 5 | 99.3% |
| language/statements/for-await-of | 1234 | 1227 | 5 | 99.6% |
| built-ins/AsyncGeneratorPrototype/next | 11 | 7 | 4 | 63.6% |
| language/expressions/assignment | 485 | 481 | 4 | 99.2% |
| staging/sm/PrivateName | 17 | 13 | 4 | 76.5% |
| staging/sm/fields | 8 | 4 | 4 | 50.0% |
| staging/sm/strict | 51 | 47 | 4 | 92.2% |
| built-ins/AsyncGeneratorPrototype/throw | 16 | 13 | 3 | 81.2% |

## Top failing-test patterns (top 100)

| Outcome | Reason (truncated) | Path |
|---|---|---|
| fail | sloppy: Test262Error: iterator closed properly Expected SameValue(«0», «1») to b… | `built-ins/AsyncFromSyncIteratorPrototype/next/iterator-result-poisoned-wrapper.js` |
| fail | sloppy: Test262Error: iterator closed properly Expected SameValue(«0», «1») to b… | `built-ins/AsyncFromSyncIteratorPrototype/next/next-result-poisoned-wrapper.js` |
| fail | sloppy: Test262Error: iterator closed properly Expected SameValue(«0», «1») to b… | `built-ins/AsyncFromSyncIteratorPrototype/throw/throw-result-poisoned-wrapper.js` |
| fail | sloppy: Test262Error: Promise should be rejected Expected a CatchError to be thr… | `built-ins/AsyncFromSyncIteratorPrototype/throw/throw-undefined-poisoned-return.js` |
| fail | sloppy: Test262Error: First result `value` Expected SameValue(«undefined», «5») … | `built-ins/AsyncGeneratorFunction/invoked-as-function-multiple-arguments.js` |
| fail | sloppy: Test262Error: First result `value` Expected SameValue(«undefined», «1») … | `built-ins/AsyncGeneratorFunction/invoked-as-function-single-argument.js` |
| fail | sloppy: Test262Error: Expected SameValue(«undefined», «2») to be true | `built-ins/AsyncGeneratorPrototype/next/request-queue-await-order.js` |
| fail | sloppy: $DONE was never called | `built-ins/AsyncGeneratorPrototype/next/request-queue-order-state-executing.js` |
| fail | sloppy: Test262Error: Expected SameValue(«1», «3») to be true | `built-ins/AsyncGeneratorPrototype/next/request-queue-order.js` |
| fail | sloppy: Test262Error: Expected SameValue(«1», «3») to be true | `built-ins/AsyncGeneratorPrototype/next/request-queue-promise-resolve-order.js` |
| fail | sloppy: Test262Error: Expected rejection | `built-ins/AsyncGeneratorPrototype/return/return-state-completed-broken-promise.js` |
| fail | sloppy: Test262Error: Expected rejection | `built-ins/AsyncGeneratorPrototype/return/return-suspendedStart-broken-promise.js` |
| fail | sloppy: Test262Error: AsyncGeneratorResolve(generator, completion.[[Value]], tru… | `built-ins/AsyncGeneratorPrototype/return/return-suspendedStart-promise.js` |
| fail | sloppy: TypeError: Cannot read property of null or undefined     at built-ins/As… | `built-ins/AsyncGeneratorPrototype/return/return-suspendedYield-broken-promise-try-catch.js` |
| fail | sloppy: Test262Error: AsyncGeneratorResolve(generator, resultValue, true) Expect… | `built-ins/AsyncGeneratorPrototype/return/return-suspendedYield-promise.js` |
| fail | sloppy: Error: Catch me.     at built-ins/AsyncGeneratorPrototype/throw/request-… | `built-ins/AsyncGeneratorPrototype/throw/request-queue-order-state-executing.js` |
| fail | sloppy: [object Promise] | `built-ins/AsyncGeneratorPrototype/throw/throw-suspendedYield-promise.js` |
| fail | sloppy: Error: boop     at built-ins/AsyncGeneratorPrototype/throw/throw-suspend… | `built-ins/AsyncGeneratorPrototype/throw/throw-suspendedYield.js` |
| fail | sloppy: Test262Error: Atomics.notify(new BigInt64Array(new SharedArrayBuffer(Big… | `built-ins/Atomics/waitAsync/bigint/nan-for-timeout-agent.js` |
| fail | sloppy: Test262Error: Atomics.notify(new BigInt64Array(new SharedArrayBuffer(Big… | `built-ins/Atomics/waitAsync/bigint/undefined-for-timeout-agent.js` |
| fail | sloppy: Test262Error: Atomics.notify(new BigInt64Array(new SharedArrayBuffer(Big… | `built-ins/Atomics/waitAsync/bigint/undefined-index-defaults-to-zero-agent.js` |
| fail | sloppy: Test262Error: Atomics.notify(new BigInt64Array(new SharedArrayBuffer(Big… | `built-ins/Atomics/waitAsync/bigint/waiterlist-block-indexedposition-wake.js` |
| fail | sloppy: Test262Error: Atomics.notify(new BigInt64Array(new SharedArrayBuffer(Big… | `built-ins/Atomics/waitAsync/bigint/was-woken-before-timeout.js` |
| fail | sloppy: Test262Error: Atomics.notify(new Int32Array(new SharedArrayBuffer(Int32A… | `built-ins/Atomics/waitAsync/nan-for-timeout-agent.js` |
| fail | sloppy: Test262Error: Atomics.notify(new Int32Array(new SharedArrayBuffer(Int32A… | `built-ins/Atomics/waitAsync/undefined-for-timeout-agent.js` |
| fail | sloppy: Test262Error: Atomics.notify(new Int32Array(new SharedArrayBuffer(Int32A… | `built-ins/Atomics/waitAsync/undefined-index-defaults-to-zero-agent.js` |
| fail | sloppy: Test262Error: Atomics.notify(new Int32Array(new SharedArrayBuffer(Int32A… | `built-ins/Atomics/waitAsync/waiterlist-block-indexedposition-wake.js` |
| fail | sloppy: Test262Error: Atomics.notify(new Int32Array(new SharedArrayBuffer(Int32A… | `built-ins/Atomics/waitAsync/was-woken-before-timeout.js` |
| fail | sloppy: Test262Error: Expected SameValue(«[object Object]», «null») to be true | `built-ins/Promise/allKeyed/arg-is-function.js` |
| fail | sloppy: Test262Error:  | `built-ins/Promise/allKeyed/capability-resolve-throws-reject.js` |
| fail | sloppy: Test262Error: result is null-prototype Expected SameValue(«[object Objec… | `built-ins/Promise/allKeyed/get-value-not-called-for-non-enumerable.js` |
| fail | sloppy: Test262Error: result is null-prototype Expected SameValue(«[object Objec… | `built-ins/Promise/allKeyed/getownproperty-not-enumerable.js` |
| fail | sloppy: Test262Error: Expected SameValue(«[object Object]», «null») to be true | `built-ins/Promise/allKeyed/key-order-preserved.js` |
| fail | sloppy: Test262Error: Expected SameValue(«[object Object]», «null») to be true | `built-ins/Promise/allKeyed/non-enumerable-properties-ignored.js` |
| fail | sloppy: Test262Error: Expected SameValue(«[object Object]», «null») to be true | `built-ins/Promise/allKeyed/prototype-keys-ignored.js` |
| fail | sloppy: Test262Error: result is null-prototype Expected SameValue(«[object Objec… | `built-ins/Promise/allKeyed/result-property-descriptors.js` |
| fail | sloppy: Test262Error: Expected SameValue(«[object Object]», «null») to be true | `built-ins/Promise/allKeyed/symbol-keys.js` |
| fail | sloppy: Test262Error: Expected SameValue(«[object Object]», «null») to be true | `built-ins/Promise/allSettledKeyed/arg-is-function.js` |
| fail | sloppy: Test262Error:  | `built-ins/Promise/allSettledKeyed/capability-resolve-throws-reject.js` |
| fail | sloppy: Test262Error: result is null-prototype Expected SameValue(«[object Objec… | `built-ins/Promise/allSettledKeyed/get-value-not-called-for-non-enumerable.js` |
| fail | sloppy: Test262Error: result is null-prototype Expected SameValue(«[object Objec… | `built-ins/Promise/allSettledKeyed/getownproperty-not-enumerable.js` |
| fail | sloppy: Test262Error: Expected SameValue(«[object Object]», «null») to be true | `built-ins/Promise/allSettledKeyed/key-order-preserved.js` |
| fail | sloppy: Test262Error: Expected SameValue(«[object Object]», «null») to be true | `built-ins/Promise/allSettledKeyed/non-enumerable-properties-ignored.js` |
| fail | sloppy: Test262Error: Expected SameValue(«[object Object]», «null») to be true | `built-ins/Promise/allSettledKeyed/prototype-keys-ignored.js` |
| fail | sloppy: Test262Error: Expected SameValue(«[object Object]», «null») to be true | `built-ins/Promise/allSettledKeyed/resolved-all-fulfilled.js` |
| fail | sloppy: Test262Error: Expected SameValue(«[object Object]», «null») to be true | `built-ins/Promise/allSettledKeyed/resolved-all-mixed.js` |
| fail | sloppy: Test262Error: Expected SameValue(«[object Object]», «null») to be true | `built-ins/Promise/allSettledKeyed/resolved-all-rejected.js` |
| fail | sloppy: Test262Error: result is null-prototype Expected SameValue(«[object Objec… | `built-ins/Promise/allSettledKeyed/result-property-descriptors.js` |
| fail | sloppy: Test262Error: Expected SameValue(«[object Object]», «null») to be true | `built-ins/Promise/allSettledKeyed/symbol-keys.js` |
| fail | sloppy: Error: ignored exception     at executor (built-ins/Promise/exception-af… | `built-ins/Promise/exception-after-resolve-in-executor.js` |
| fail | sloppy: [object Object] | `built-ins/Promise/exception-after-resolve-in-thenable-job.js` |
| fail | sloppy: Test262Error: Expected SameValue(«4», «5») to be true | `built-ins/Promise/prototype/finally/rejected-observable-then-calls-PromiseResolve.js` |
| fail | sloppy: Test262Error: `then` invoked with one argument Expected SameValue(«2», «… | `built-ins/Promise/prototype/finally/rejected-observable-then-calls-argument.js` |
| fail | sloppy: Test262Error: Expected SameValue(«4», «5») to be true | `built-ins/Promise/prototype/finally/resolved-observable-then-calls-PromiseResolve.js` |
| fail | sloppy: Test262Error: 7 new promises were created Expected SameValue(«6», «7») t… | `built-ins/Promise/prototype/finally/species-constructor.js` |
| fail | sloppy: Test262Error: Expected SameValue(«6», «7») to be true | `built-ins/Promise/prototype/finally/subclass-reject-count.js` |
| fail | sloppy: Test262Error: Expected SameValue(«6», «7») to be true | `built-ins/Promise/prototype/finally/subclass-resolve-count.js` |
| fail | sloppy: The promise should be fulfilled with the resolution value of the provide… | `built-ins/Promise/prototype/then/resolve-pending-fulfilled-prms-cstm-then.js` |
| fail | sloppy: The promise should be fulfilled with the resolution value of the provide… | `built-ins/Promise/prototype/then/resolve-pending-rejected-prms-cstm-then.js` |
| fail | sloppy: The promise should be fulfilled with the resolution value of the provide… | `built-ins/Promise/prototype/then/resolve-settled-fulfilled-prms-cstm-then.js` |
| fail | sloppy: The promise should be fulfilled with the resolution value of the provide… | `built-ins/Promise/prototype/then/resolve-settled-rejected-prms-cstm-then.js` |
| fail | sloppy: c | `built-ins/Promise/race/resolved-then-catch-finally.js` |
| fail | sloppy: The promise should be fulfilled with the provided value. | `built-ins/Promise/resolve-prms-cstm-then-deferred.js` |
| fail | sloppy: The promise should be fulfilled with the provided value. | `built-ins/Promise/resolve-prms-cstm-then-immed.js` |
| fail | sloppy: Test262Error: error thrown from callback must become a rejection Expecte… | `built-ins/Promise/try/throws.js` |
| fail | sloppy: runtime: TypeError (UNCAUGHT) uncaught exception: Test262Error: NewTarge… | `built-ins/SharedArrayBuffer/prototype-from-newtarget.js` |
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

