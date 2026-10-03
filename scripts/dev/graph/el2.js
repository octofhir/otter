// One element site over packed and holey double arrays: holes read as
// undefined, present slots by value, stores into present slots of either
// kind, and a store into a hole that leaves generated code.
'use strict';
function sum(a) {
  let t = 0, u = 0;
  for (let i = 0; i < a.length; i++) { const v = a[i]; if (v === undefined) u++; else t += v; }
  return t * 1000 + u;
}
function scale(a, k) {
  for (let i = 0; i < a.length; i++) if (i % 3 !== 1) a[i] = a[i] * k;
}
const packed = [0.5, 1.5, 2.5, 3.5, 4.5, 5.5];
const holey = [0.5, , 2.5, , 4.5, 5.5];
const grown = new Array(6);
for (let i = 0; i < 6; i += 2) grown[i] = i + 0.25;
let acc = 0;
for (let r = 0; r < 3000; r++) {
  acc += sum(packed) + sum(holey) + sum(grown);
  scale(packed, 1); scale(holey, 1); scale(grown, 1);
}
console.log(acc, packed.join(','), holey.join(','), grown.join(','));
scale(holey, 2);
holey[1] = 9.5;
console.log(sum(holey), holey.join(','));
