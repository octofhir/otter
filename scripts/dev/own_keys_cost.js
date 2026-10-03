// Own-key enumeration of ordinary objects: Object.keys, spread, for-in and
// Reflect.ownKeys over a few shapes, with index-like keys mixed in.
const objects = [];
for (let i = 0; i < 64; i++) {
  const o = { alpha: i, beta: i * 2, gamma: 'g', delta: null, epsilon: [i] };
  if (i & 1) o[String(i)] = i;
  if (i & 2) o.zeta = true;
  objects.push(o);
}
const t0 = Date.now();
let n = 0;
for (let round = 0; round < 20000; round++) {
  const o = objects[round & 63];
  n += Object.keys(o).length;
  n += Reflect.ownKeys(o).length;
  const copy = { ...o };
  for (const k in copy) n += k.length;
}
console.log(n, Date.now() - t0, 'ms');
console.log(JSON.stringify(Reflect.ownKeys({ b: 1, 2: 2, a: 3, 1: 4, [Symbol.iterator]: 5 }).map(String)));
