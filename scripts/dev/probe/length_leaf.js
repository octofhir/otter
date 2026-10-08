// `.length` reads over arrays, strings, ropes, ordinary objects and
// primitives, and plain calls of an extracted static native whose operands
// sometimes leave its leaf, all hot enough for the optimizing tier.
function len(value) { return value.length; }
const abs = Math.abs;
function absOf(value) { return abs(value); }
const values = [
  [1, 2, 3], "hello", "a".repeat(50) + "b".repeat(70), { length: 7 }, { length: "x" },
  function (a, b) {}, new Array(5), [], "", new String("wrapped"),
];
let out = [];
for (let round = 0; round < 4000; round++) {
  for (const value of values) {
    const n = len(value);
    if (round === 3999) out.push(String(n));
  }
  const inputs = [-3, 4.5, -0, "7", null, undefined, -2147483648, NaN];
  const a = absOf(inputs[round % inputs.length]);
  if (round >= 3992) out.push(String(a));
}
let rope = "";
for (let i = 0; i < 3000; i++) { rope += "xy"; if (i % 1000 === 999) out.push(len(rope)); }
try { len(null); } catch (e) { out.push(e.constructor.name); }
console.log(out.join(","));
