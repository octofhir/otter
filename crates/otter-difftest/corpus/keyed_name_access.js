// Hot computed-name loads and stores over many receiver classes: the shared
// action table's keyed probes, keyed adds, absent keys, and every change that
// must make a recorded entry stop answering.
function Table() { this.count = 0; this.table = {}; }
Table.prototype.add = function (key, data) {
  if (this.table[key] != undefined) return false;
  this.table[key] = data;
  this.count++;
  return true;
};
Table.prototype.lookup = function (key) {
  const data = this.table[key];
  return data != undefined ? data : null;
};
const words = [];
for (let i = 0; i < 40; i++) words.push("w" + i);
let checksum = 0;
for (let round = 0; round < 400; round++) {
  const t = new Table();
  const n = 3 + (round % 30);
  for (let i = 0; i < n; i++) t.add(words[(i * 7 + round) % words.length], { v: i + round });
  for (let i = 0; i < words.length; i++) {
    const hit = t.lookup(words[i]);
    checksum = (checksum * 31 + (hit === null ? 1 : hit.v)) | 0;
  }
  // overwrite through the keyed store
  t.table[words[round % words.length]] = { v: -1 };
  checksum = (checksum + t.lookup(words[round % words.length]).v) | 0;
}
const out = [checksum];

// keys built at run time, read back by other keys of the same spelling
function get(o, k) { return o[k]; }
function put(o, k, v) { o[k] = v; }
const o = {};
for (let i = 0; i < 2000; i++) put(o, "k" + (i % 5), i);
let s = 0;
for (let i = 0; i < 2000; i++) s += get(o, ["k", i % 5].join(""));
out.push(s, Object.keys(o).join());

// inherited data, then an own shadow, then a getter replacing it
const base = { shared: 1 };
const child = Object.create(base);
let inherited = 0;
for (let i = 0; i < 2000; i++) inherited += get(child, "shared");
base.shared = 2;
for (let i = 0; i < 2000; i++) inherited += get(child, "shared");
put(child, "shared", 3);
for (let i = 0; i < 2000; i++) inherited += get(child, "shared");
delete child.shared;
Object.defineProperty(base, "shared", { get() { return 4; } });
for (let i = 0; i < 2000; i++) inherited += get(child, "shared");
out.push(inherited);

// absent key, then the key appears on the prototype
const lone = Object.create(base);
let absent = 0;
for (let i = 0; i < 2000; i++) if (get(lone, "late") === undefined) absent++;
base.late = 9;
for (let i = 0; i < 2000; i++) if (get(lone, "late") === 9) absent++;
out.push(absent);

// a prototype setter, a read-only inherited key, a frozen receiver
let setterCalls = 0;
const guarded = { set s(v) { setterCalls++; } };
Object.defineProperty(guarded, "ro", { value: 1, writable: false });
for (let i = 0; i < 2000; i++) {
  const r = Object.create(guarded);
  put(r, "s", i);
  put(r, "ro", i);
  put(r, "fresh", i);
  if (Object.prototype.hasOwnProperty.call(r, "s") || r.ro !== 1 || r.fresh !== i) throw new Error("set");
}
const frozen = Object.freeze({ a: 1 });
for (let i = 0; i < 2000; i++) put(frozen, i & 1 ? "a" : "b", i);
out.push(setterCalls, frozen.a, frozen.b);

// receivers past the keyed soft limit become dictionaries
const dict = {};
for (let i = 0; i < 300; i++) put(dict, "d" + i, i);
let d = 0;
for (let i = 0; i < 3000; i++) d += get(dict, "d" + (i % 300));
delete dict.d7;
for (let i = 0; i < 300; i++) d += get(dict, "d" + i) === undefined ? 1000 : 0;
out.push(d, Object.keys(dict).length);

// index-like and symbol keys stay on their own paths
const mixed = {};
for (let i = 0; i < 2000; i++) { put(mixed, String(i % 3), i); put(mixed, "x", i); }
const sym = Symbol("s");
for (let i = 0; i < 2000; i++) put(mixed, sym, i);
out.push(Object.keys(mixed).join(), get(mixed, sym), get(mixed, 1));

// keys that are not strings coerce
let coerced = 0;
const objKey = { toString() { return "x"; } };
for (let i = 0; i < 2000; i++) coerced += get(mixed, objKey) === mixed.x ? 1 : 0;
out.push(coerced);
console.log(out.join("\n"));
