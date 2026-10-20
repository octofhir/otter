// Math methods inside a hot loop and inside a small function an optimizing
// compile inlines: the float64 lowering must answer exactly what the
// interpreter answers, edge cases included.
'use strict';
function set(m, angle) {
  const c = Math.cos(angle);
  const s = Math.sin(angle);
  m.a = c; m.b = -s; m.c = s; m.d = c;
}
const m = { a: 0, b: 0, c: 0, d: 0 };
let acc = 0;
const edges = [0, -0, 0.5, -0.5, 1.5, -1.5, 2.5, -2.5, 0.49999999999999994, -0.49999999999999994,
  1e300, -1e300, Infinity, -Infinity, NaN, 4503599627370497, -4503599627370497, 3, -7.25];
for (let i = 0; i < 200000; i++) {
  const x = edges[i % edges.length] + (i % 7) * 0.25;
  const y = edges[(i * 3) % edges.length];
  set(m, x);
  acc += (m.a + m.b + m.c + m.d) || 0;
  const r = [Math.abs(x), Math.sqrt(x), Math.floor(x), Math.ceil(x), Math.trunc(x), Math.round(x),
    Math.min(x, y), Math.max(x, y), Math.tan(x), Math.atan(x), Math.exp(x), Math.log(x),
    Math.atan2(x, y), Math.pow(x, y)];
  for (const v of r) acc += Number.isFinite(v) ? v % 1000 : (Number.isNaN(v) ? 1 : 2);
  if (Object.is(Math.round(-0.25), -0) && Object.is(Math.min(0, -0), -0) && Object.is(Math.max(-0, 0), 0)) acc += 1;
}
console.log(acc.toFixed(6));
