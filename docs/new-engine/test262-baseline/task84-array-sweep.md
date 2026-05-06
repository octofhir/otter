# Test262 conformance baseline

- **Engine commit:** `5705e8154841fc548a4b132535368979996814ce`
- **Test262 commit:** `unknown`
- **Captured:** 2026-05-06T06:47:43.687611+00:00

## Totals

| Bucket | Count |
|---|---|
| total      | 3083 |
| passed     | 517 |
| failed     | 2339 |
| skipped    | 227 |
| crashed    | 0 |
| timed_out  | 0 |
| oom        | 0 |

**Pass rate (excl. skipped):** 18.10%

## Task 84 run notes

- Command: `bash scripts/test262-safe.sh built-ins/Array --output docs/new-engine/test262-baseline/task84-array-sweep.json`
- Wrapper-normalized filter: `built-ins/Array/` (excludes `built-ins/ArrayBuffer/*`).
- Host: local macOS developer machine.
- Per-test heap cap: 512 MiB (`536870912` bytes).
- Per-test timeout: 5000 ms.
- Peak host RSS: 791936 KiB (~774 MiB), measured by external process-tree polling during the pre-fix run. A sandboxed `/usr/bin/time -l` rerun on 2026-05-06 completed the sweep but could not report RSS because `sysctl kern.clockrate` was denied.
- Completion: reached end of the Array directory sweep; no process crash, timeout, or in-engine OOM. The former OOM test (`built-ins/Array/S15.4_A1.1_T10.js`) now passes.

## Top failing sections (top 50)

| Section | total | passed | failed | pass-rate |
|---|---:|---:|---:|---:|
| built-ins/Array/prototype | 2810 | 479 | 2207 | 17.8% |
| built-ins/Array/from | 47 | 5 | 39 | 11.4% |
| built-ins/Array/length | 30 | 5 | 24 | 17.2% |
| built-ins/Array/of | 16 | 2 | 13 | 13.3% |
| built-ins/Array/isArray | 29 | 19 | 10 | 65.5% |
| built-ins/Array/Symbol.species | 4 | 0 | 4 | 0.0% |
| annexB/built-ins/Array | 1 | 0 | 1 | 0.0% |
| built-ins/Array/15.4.5-1.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/15.4.5.1-5-1.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/15.4.5.1-5-2.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/S15.4.1_A1.1_T1.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/S15.4.1_A1.1_T2.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/S15.4.1_A1.1_T3.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/S15.4.1_A1.2_T1.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/S15.4.1_A1.3_T1.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/S15.4.1_A3.1_T1.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/S15.4.2.1_A1.1_T1.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/S15.4.2.1_A1.1_T2.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/S15.4.2.1_A1.1_T3.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/S15.4.2.1_A1.2_T1.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/S15.4.2.1_A1.3_T1.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/S15.4.3_A1.1_T1.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/S15.4.3_A1.1_T2.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/S15.4.3_A1.1_T3.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/S15.4.5.1_A1.2_T2.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/S15.4.5.1_A2.1_T1.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/S15.4.5.1_A2.2_T1.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/S15.4.5.2_A1_T1.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/S15.4.5.2_A1_T2.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/S15.4.5.2_A3_T1.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/S15.4.5.2_A3_T2.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/S15.4.5.2_A3_T3.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/S15.4_A1.1_T4.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/S15.4_A1.1_T5.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/S15.4_A1.1_T6.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/S15.4_A1.1_T7.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/S15.4_A1.1_T8.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/S15.4_A1.1_T9.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/constructor.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/is-a-constructor.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/length.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/name.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/prop-desc.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/property-cast-boolean-primitive.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/property-cast-nan-infinity.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/property-cast-number.js | 1 | 0 | 1 | 0.0% |
| built-ins/Array/proto.js | 1 | 0 | 1 | 0.0% |
| staging/built-ins/Array | 1 | 0 | 1 | 0.0% |
| built-ins/Array/S15.4.1_A2.1_T1.js | 1 | 1 | 0 | 100.0% |
| built-ins/Array/S15.4.1_A2.2_T1.js | 1 | 1 | 0 | 100.0% |

## Top failing-test patterns (top 100)

| Outcome | Reason (truncated) | Path |
|---|---|---|
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: ReferenceError: $262 is not de… | `annexB/built-ins/Array/from/iterator-method-emulates-undefined.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: value is not a func… | `built-ins/Array/15.4.5-1.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: operand type mismat… | `built-ins/Array/15.4.5.1-5-1.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: operand type mismat… | `built-ins/Array/15.4.5.1-5-2.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: The value of x.myproperty is e… | `built-ins/Array/S15.4.1_A1.1_T1.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: unknown intrinsic m… | `built-ins/Array/S15.4.1_A1.1_T2.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Array.prototype.isPrototypeOf(… | `built-ins/Array/S15.4.1_A1.1_T3.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: unknown intrinsic m… | `built-ins/Array/S15.4.1_A1.2_T1.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: The value of x.length is not 1… | `built-ins/Array/S15.4.1_A1.3_T1.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: The result of evaluating (Arra… | `built-ins/Array/S15.4.1_A3.1_T1.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: The value of x.myproperty is e… | `built-ins/Array/S15.4.2.1_A1.1_T1.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: unknown intrinsic m… | `built-ins/Array/S15.4.2.1_A1.1_T2.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Array.prototype.isPrototypeOf(… | `built-ins/Array/S15.4.2.1_A1.1_T3.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: unknown intrinsic m… | `built-ins/Array/S15.4.2.1_A1.2_T1.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: The value of x.length is not 1… | `built-ins/Array/S15.4.2.1_A1.3_T1.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: The value of Array.myproperty … | `built-ins/Array/S15.4.3_A1.1_T1.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Array.toString() must return "… | `built-ins/Array/S15.4.3_A1.1_T2.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Function.prototype.isPrototype… | `built-ins/Array/S15.4.3_A1.1_T3.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: operand type mismat… | `built-ins/Array/S15.4.5.1_A1.2_T2.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: operand type mismat… | `built-ins/Array/S15.4.5.1_A2.1_T1.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: The value of x.length is expec… | `built-ins/Array/S15.4.5.1_A2.2_T1.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: The value of x.length is expec… | `built-ins/Array/S15.4.5.2_A1_T1.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: operand type mismat… | `built-ins/Array/S15.4.5.2_A1_T2.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: The value of x.length is expec… | `built-ins/Array/S15.4.5.2_A3_T1.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: The value of x.length is expec… | `built-ins/Array/S15.4.5.2_A3_T2.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: The value of x.length is expec… | `built-ins/Array/S15.4.5.2_A3_T3.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: operand type mismat… | `built-ins/Array/S15.4_A1.1_T4.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: operand type mismat… | `built-ins/Array/S15.4_A1.1_T5.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: operand type mismat… | `built-ins/Array/S15.4_A1.1_T6.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: operand type mismat… | `built-ins/Array/S15.4_A1.1_T7.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: operand type mismat… | `built-ins/Array/S15.4_A1.1_T8.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: operand type mismat… | `built-ins/Array/S15.4_A1.1_T9.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: value is not a func… | `built-ins/Array/Symbol.species/length.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: operand type mismat… | `built-ins/Array/Symbol.species/return-value.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: value is not a func… | `built-ins/Array/Symbol.species/symbol-species-name.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: value is not a func… | `built-ins/Array/Symbol.species/symbol-species.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: The value of `typeof Array` is… | `built-ins/Array/constructor.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: value is not a func… | `built-ins/Array/from/Array.from-descriptor.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: value is not a func… | `built-ins/Array/from/Array.from-name.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: value is not a func… | `built-ins/Array/from/Array.from_arity.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: value is not a func… | `built-ins/Array/from/Array.from_forwards-length-for-array-likes.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: operand type mismat… | `built-ins/Array/from/array-like-has-length-but-no-indexes-with-values.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: unknown intrinsic m… | `built-ins/Array/from/calling-from-valid-1-onlyStrict.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: unknown intrinsic m… | `built-ins/Array/from/calling-from-valid-2.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: unknown intrinsic m… | `built-ins/Array/from/elements-added-after.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: unknown intrinsic m… | `built-ins/Array/from/elements-deleted-after.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: unknown intrinsic m… | `built-ins/Array/from/elements-updated-after.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: The result of evaluating (resu… | `built-ins/Array/from/from-array.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: operand type mismat… | `built-ins/Array/from/get-iter-method-err.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: operand type mismat… | `built-ins/Array/from/items-is-arraybuffer.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Array.from(items) throws a Tes… | `built-ins/Array/from/iter-adv-err.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Array.from.call(C, items) thro… | `built-ins/Array/from/iter-cstm-ctor-err.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: value is not a func… | `built-ins/Array/from/iter-cstm-ctor.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Array.from(itemsPoisonedSymbol… | `built-ins/Array/from/iter-get-iter-err.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Array.from(itemsPoisonedIterat… | `built-ins/Array/from/iter-get-iter-val-err.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: unknown intrinsic m… | `built-ins/Array/from/iter-map-fn-args.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Array.from(items, mapFn) throw… | `built-ins/Array/from/iter-map-fn-err.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: unknown intrinsic m… | `built-ins/Array/from/iter-map-fn-return.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: unknown intrinsic m… | `built-ins/Array/from/iter-map-fn-this-arg.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: unknown intrinsic m… | `built-ins/Array/from/iter-map-fn-this-strict.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: The value of closeCount is exp… | `built-ins/Array/from/iter-set-elem-prop-err.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: value is not a func… | `built-ins/Array/from/iter-set-elem-prop-non-writable.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: operand type mismat… | `built-ins/Array/from/iter-set-elem-prop.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Array.from.call(poisonedProtot… | `built-ins/Array/from/iter-set-length-err.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: operand type mismat… | `built-ins/Array/from/iter-set-length.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Array.from(array, mapFnThrows)… | `built-ins/Array/from/mapfn-throws-exception.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: isConstructor invoked with a n… | `built-ins/Array/from/not-a-constructor.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: unknown intrinsic m… | `built-ins/Array/from/source-array-boundary.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: value is not a func… | `built-ins/Array/from/source-object-constructor.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Array.from(obj) throws a Test2… | `built-ins/Array/from/source-object-iterator-1.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: value is not a func… | `built-ins/Array/from/source-object-iterator-2.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: value is not a func… | `built-ins/Array/from/source-object-length-set-elem-prop-non-writable.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: operand type mismat… | `built-ins/Array/from/source-object-length.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: value is not a func… | `built-ins/Array/from/source-object-missing.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: operand type mismat… | `built-ins/Array/from/source-object-without.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: value is not a func… | `built-ins/Array/from/this-null.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: isConstructor invoked with a n… | `built-ins/Array/is-a-constructor.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: The value of `typeof f` is exp… | `built-ins/Array/isArray/15.4.3.2-0-1.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: operand type mismat… | `built-ins/Array/isArray/15.4.3.2-0-2.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Array.isArray(Array.prototype)… | `built-ins/Array/isArray/15.4.3.2-0-5.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: value is not a func… | `built-ins/Array/isArray/15.4.3.2-1-10.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Array.isArray(arguments) must … | `built-ins/Array/isArray/15.4.3.2-1-13.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: value is not a func… | `built-ins/Array/isArray/descriptor.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: value is not a func… | `built-ins/Array/isArray/name.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: isConstructor invoked with a n… | `built-ins/Array/isArray/not-a-constructor.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: operand type mismat… | `built-ins/Array/isArray/proxy-revoked.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: operand type mismat… | `built-ins/Array/isArray/proxy.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: [].length = 4294967296 throws … | `built-ins/Array/length/15.4.5.1-3.d-1.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: [].length = 4294967297 throws … | `built-ins/Array/length/15.4.5.1-3.d-2.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: The value of a.length is expec… | `built-ins/Array/length/15.4.5.1-3.d-3.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: The value of x.myproperty is e… | `built-ins/Array/length/S15.4.2.2_A1.1_T1.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: unknown intrinsic m… | `built-ins/Array/length/S15.4.2.2_A1.1_T2.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: Array.prototype.isPrototypeOf(… | `built-ins/Array/length/S15.4.2.2_A1.1_T3.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: TypeError: value is not a func… | `built-ins/Array/length/S15.4.2.2_A1.2_T1.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: The value of x.length is expec… | `built-ins/Array/length/S15.4.2.2_A2.1_T1.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: The result of evaluating (e in… | `built-ins/Array/length/S15.4.2.2_A2.2_T1.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: The result of evaluating (e in… | `built-ins/Array/length/S15.4.2.2_A2.2_T2.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: The result of evaluating (e in… | `built-ins/Array/length/S15.4.2.2_A2.2_T3.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: The value of Array.prototype.l… | `built-ins/Array/length/S15.4.4_A1.3_T1.js` |
| fail | runtime: TypeError (UNCAUGHT) uncaught exception: The result of evaluating (e in… | `built-ins/Array/length/S15.4.5.1_A1.1_T1.js` |
