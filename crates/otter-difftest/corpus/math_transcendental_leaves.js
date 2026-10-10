// Hot Math.sin/cos/tan/atan/exp/log/ceil/round/trunc/atan2/pow calls over
// int32, double, signed zero, NaN and infinite arguments, arguments that
// coerce through valueOf, other arities, and a Math method replaced
// mid-loop.
const inputs = [
  0, -0, 1, -1, 2, 7, -13, 0.5, -0.5, 2.5, -2.5, 1.5, 0.49999999999999994,
  -0.49999999999999994, 3.141592653589793, 1e-300, -1e300, 1e21, NaN,
  Infinity, -Infinity, 123456789, -2147483648, 4294967295.5,
];
const fmt = (x) => (Object.is(x, -0) ? "-0" : String(x));

function unary(n) {
  const out = [];
  for (let r = 0; r < n; r++) {
    for (let i = 0; i < inputs.length; i++) {
      const x = inputs[i];
      const row = [
        Math.sin(x), Math.cos(x), Math.tan(x), Math.atan(x), Math.exp(x),
        Math.log(x), Math.ceil(x), Math.round(x), Math.trunc(x),
      ];
      if (r === n - 1) out.push(row.map(fmt).join(" "));
    }
  }
  return out.join("|");
}

function binary(n) {
  const out = [];
  for (let r = 0; r < n; r++) {
    for (let i = 0; i < inputs.length; i++) {
      const x = inputs[i];
      const y = inputs[(i * 7 + 3) % inputs.length];
      const row = [Math.atan2(x, y), Math.atan2(y, x), Math.pow(x, y), Math.pow(y, 2), Math.pow(-8, 1 / 3), Math.pow(2, -1074)];
      if (r === n - 1) out.push(row.map(fmt).join(" "));
    }
  }
  return out.join("|");
}

// Arguments that are not numbers coerce observably.
let coerced = 0;
const boxed = { valueOf() { coerced++; return 0.25; } };
function mixed(n) {
  let s = 0;
  for (let i = 0; i < n; i++) {
    const x = i % 50 === 0 ? boxed : i % 37 === 0 ? "0.75" : i * 0.001;
    s += Math.sin(x) + Math.cos(x) + Math.pow(x, 2) + Math.atan2(x, 1);
    s += Math.sin() + Math.pow(x) + Math.cos(x, 99);
  }
  return s;
}

// A replaced Math.sin must run from hot code on its very next call.
function replaced(n) {
  const original = Math.sin;
  let s = 0;
  for (let i = 0; i < n; i++) {
    if (i === n >> 1) Math.sin = (v) => v * 1000;
    s += Math.sin(i);
  }
  Math.sin = original;
  return s;
}

const out = [];
out.push(unary(400));
out.push(binary(400));
out.push(mixed(20000).toFixed(6), coerced);
out.push(replaced(20000).toFixed(6));
console.log(out.join("\n"));
