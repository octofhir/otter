// Monomorphic and polymorphic object shapes with property updates.
function makeA(i) {
  return { x: i, y: i + 1, z: 0 };
}
function makeB(i) {
  return { y: i + 1, x: i, w: 3, z: 0 };
}

const N = 70000;
const mono = new Array(N);
const poly = new Array(N);
for (let i = 0; i < N; i++) {
  mono[i] = makeA(i & 1023);
  poly[i] = (i & 1) === 0 ? makeA(i & 1023) : makeB(i & 1023);
}

let acc = 0;
for (let r = 0; r < 25; r++) {
  for (let i = 0; i < N; i++) {
    const o = mono[i];
    o.z = (o.x + o.y + r) & 4095;
    acc = (acc + o.z) | 0;
  }
  for (let i = 0; i < N; i++) {
    const o = poly[i];
    o.z = (o.x - o.y + r) | 0;
    acc = (acc ^ (o.z + o.x)) | 0;
  }
}
console.log(acc >>> 0);
