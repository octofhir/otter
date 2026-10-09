// `in` with string keys built at run time against shaped objects,
// dictionary objects, prototypes, arrays, functions and proxies.
function has(k, o) { return k in o; }
const proto = { inherited: 1 };
const shaped = Object.create(proto); shaped.a = 1; shaped.b = 2;
const dict = {}; for (let i = 0; i < 64; i++) dict["k" + i] = i; delete dict.k3;
const arr = [1, , 3]; arr.extra = 1;
function fn() {} fn.custom = 1;
const proxy = new Proxy({ real: 1 }, { has(t, k) { return k === "virtual" || k in t; } });
const targets = [shaped, dict, arr, fn, proxy, Object.prototype];
const keys = ["a", "b", "inherited", "k1", "k3", "k63", "0", "1", "2", "length", "extra", "custom",
  "prototype", "real", "virtual", "toString", "nope", "__proto__"];
let out = [];
for (let round = 0; round < 400; round++) {
  let row = 0;
  for (let t = 0; t < targets.length; t++) {
    for (let k = 0; k < keys.length; k++) {
      const key = keys[k].slice(0, 1) + keys[k].slice(1); // a fresh string each time
      row = (row * 3 + (has(key, targets[t]) ? 1 : 0) + t + k) | 0;
    }
  }
  if (round % 100 === 0) out.push(row);
}
out.push(has("\ud800", { "\ud800": 1 }), has("�", { "\ud800": 1 }), has(Symbol.iterator, []));
console.log(out.join(","));
