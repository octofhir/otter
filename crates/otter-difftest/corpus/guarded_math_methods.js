// Guarded methods retain canonical completion for identity and numeric misses.
function absMethod(receiver, value) { return receiver.abs(value); }
function minMethod(receiver, left, right) { return receiver.min(left, right); }
function maxMethod(receiver, left, right) { return receiver.max(left, right); }
function int32AbsMethod(receiver, value) {
  const integer = value | 0;
  return receiver.abs(integer);
}
function inheritedInt32AbsMethod(receiver, value) {
  const integer = value | 0;
  return receiver.abs(integer);
}
function makeMappedMath(value) {
  return { receiver: arguments, replace(next) { value = next; } };
}
function invokeMappedMath(receiver) {
  const integer = -7;
  return receiver["0"](integer);
}
const inheritedMath = Object.create(Math);
const mappedMath = makeMappedMath(Math.abs);
for (let warm = 0; warm < 4010; warm++) {
  absMethod(Math, -7);
  minMethod(inheritedMath, -7, 2);
  maxMethod(Math, -7, 2);
  int32AbsMethod(Math, -7);
  inheritedInt32AbsMethod(inheritedMath, -7);
  invokeMappedMath(mappedMath.receiver);
}

const results = [
  absMethod(Math, -7),
  absMethod(Math, -2147483648),
  1 / absMethod(Math, -0) === Infinity,
  Number.isNaN(absMethod(Math, NaN)),
  minMethod(inheritedMath, -7, 2),
  1 / minMethod(inheritedMath, 0, -0) === -Infinity,
  1 / maxMethod(Math, -0, 0) === Infinity,
  Number.isNaN(maxMethod(Math, 1, NaN)),
  int32AbsMethod(Math, -2147483648),
];

let coercions = 0;
const coercive = {
  valueOf() {
    coercions++;
    const retained = [];
    for (let index = 0; index < 64; index++) {
      retained.push({ text: "math-coercion-" + index });
    }
    if (retained[63].text !== "math-coercion-63") throw new Error("root lost");
    return -9;
  },
};
results.push(absMethod(Math, coercive), coercions);
try { minMethod(inheritedMath, 1n, 2); }
catch (error) { results.push(error instanceof TypeError); }
const originalAbs = Math.abs;
Math.abs = value => value + 100;
results.push(absMethod(Math, 3));
results.push(int32AbsMethod(Math, 3));
Math.abs = originalAbs;
let getterCalls = 0;
Object.defineProperty(Math, "abs", { configurable: true, get() {
  getterCalls++;
  const retained = [];
  for (let index = 0; index < 64; index++) retained.push({ value: index });
  if (retained[63].value !== 63) throw new Error("getter roots lost");
  return originalAbs;
} });
results.push(int32AbsMethod(Math, -11));
results.push(inheritedInt32AbsMethod(inheritedMath, -13), getterCalls);
Object.defineProperty(Math, "abs", { configurable: true, writable: true, value: originalAbs });
Object.defineProperty(inheritedMath, "min", { get() {
  coercions++;
  return function(left, right) { return left + right + 100; };
} });
results.push(minMethod(inheritedMath, 3, 4), coercions);
const originalMath = Math;
globalThis.Math = { max(left, right) { return left + right + 200; } };
results.push(maxMethod(Math, 3, 4), absMethod(originalMath, -5));
globalThis.Math = originalMath;
results.push(invokeMappedMath(mappedMath.receiver));
// Parameter assignment updates the mapped cell while the ordinary slot and
// shape still name the original native function.
mappedMath.replace(Math.max);
results.push(invokeMappedMath(mappedMath.receiver));
const mathResult = JSON.stringify(results);
console.log(mathResult);
mathResult;
