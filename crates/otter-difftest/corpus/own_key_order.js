// Own-key order through every enumeration surface: array-index keys first in
// ascending numeric order, then other strings in insertion order, symbols
// last; canonical index spellings only ("01", "-1" and 2**32-1 are names).
const sym = Symbol('s');
function make(i) {
  const o = { b: 1, 10: 2, a: 3, 2: 4, '01': 5, '-1': 6, 4294967295: 7, 4294967294: 8 };
  o[sym] = 9;
  if (i & 1) o.z = i;
  if (i & 2) o[i] = i;
  return o;
}
let checksum = 0;
for (let i = 0; i < 3000; i++) {
  const o = make(i);
  checksum = (checksum + Object.keys(o).join('|').length + Reflect.ownKeys(o).length) | 0;
  for (const k in { ...o }) checksum = (checksum + k.length) | 0;
}
const o = make(3);
console.log(checksum);
console.log(JSON.stringify(Object.keys(o)));
console.log(JSON.stringify(Reflect.ownKeys(o).map(String)));
console.log(JSON.stringify(Object.getOwnPropertyNames(o)));
console.log(JSON.stringify(Object.entries(o)));
const seen = [];
for (const k in o) seen.push(k);
console.log(JSON.stringify(seen), JSON.stringify(Object.assign({}, o)));
