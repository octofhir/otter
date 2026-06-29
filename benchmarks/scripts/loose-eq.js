// Loose-equality (`==` / `!=`) optimizing-tier correctness + perf workload.
//
// Real OO code is saturated with `x == null` / `x != null` nullish guards and
// loose numeric comparisons. The optimizing tier lowers a loose compare against
// a nullish literal to a null-OR-undefined identity test, and a loose compare on
// numeric feedback to a guarded numeric compare (a non-number operand deopts).
// This script is primarily a differential gate: `interp`, `jit`, and `jit-osr`
// must agree byte-for-byte, which pins the nullish collapse (both `null` and
// `undefined` must match `== null`) and the numeric loose path. Deterministic
// output (no timing).

function classify(x) {
  let s = 0;
  for (let i = 0; i < 200000; i++) {
    if (x == null) s += 1;       // nullish: true for null AND undefined
    if (x != null) s += 2;       // its negation
    if (i == 100000) s += 3;     // numeric loose ==
    if (i != 0) s += 0;          // numeric loose != (no-op accumulation)
  }
  return s;
}

// null and undefined must score identically (both nullish); a number scores the
// complementary branch on every iteration.
const a = classify(null);
const b = classify(undefined);
const c = classify(7);
console.log("loose-eq null=" + a + " undef=" + b + " num=" + c);
