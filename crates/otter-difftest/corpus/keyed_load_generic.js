// Element loads at sites whose receivers change storage kind, hold holes,
// read past their length, or are not arrays at all: the generic keyed load
// in optimized code and every case that must leave it.
function get(a, i) { return a[i]; }
function sumAll(a) { let s = 0; for (let i = 0; i < a.length; i++) { const v = a[i]; s += typeof v === "number" ? v : (v === undefined ? 1 : 2); } return s; }
const out = [];

// mixed storage kinds at one site: tagged, packed double, holey double, holes
let total = 0;
for (let round = 0; round < 400; round++) {
  const tagged = [1, "x", { k: round }, null];
  const doubles = [1.5, 2.5, round + 0.25];
  const holey = new Array(6); holey[1] = 0.5; holey[4] = round;
  const sparse = [1, , 3];
  total += sumAll(tagged) + sumAll(doubles) + sumAll(holey) + sumAll(sparse);
  total += (get(doubles, 2) | 0) + (get(holey, 0) === undefined ? 7 : 0) + (get(holey, 9) === undefined ? 11 : 0);
}
out.push(total);

// name keys, string receivers, objects, prototype holes, negative and fractional keys
const proto = [10, 20, 30];
Object.prototype.inherited = "from-object";
const objects = [];
for (let i = 0; i < 1000; i++) {
  const o = i % 5 === 0 ? { a: i, b: "s" + i } : i % 5 === 1 ? "string" + i : i % 5 === 2 ? [i, i + 1] : i % 5 === 3 ? new Map() : { [i % 7]: i };
  objects.push(o);
}
const keys = ["a", "b", "length", 0, 1, 3, -1, 1.5, "inherited", "nope", "constructor"];
let acc = [];
for (let round = 0; round < 3; round++) {
  for (let i = 0; i < objects.length; i++) {
    const v = get(objects[i], keys[(i + round) % keys.length]);
    if (i % 97 === 0) acc.push(typeof v === "function" ? "fn" : String(v));
  }
}
out.push(acc.join("|"));
delete Object.prototype.inherited;

// a hole that reads through a prototype with an element, then a getter
const withProto = [, , 3];
Object.setPrototypeOf(withProto, proto);
let p = 0;
for (let i = 0; i < 2000; i++) p += get(withProto, i % 3);
out.push(p);
let calls = 0;
Object.defineProperty(Array.prototype, 5, { get() { calls++; return 100; }, configurable: true });
const gappy = new Array(8); gappy[0] = 1;
let g = 0;
for (let i = 0; i < 2000; i++) g += get(gappy, i & 7) === 100 ? 1 : 0;
delete Array.prototype[5];
out.push(g, calls);

// typed arrays, arguments objects, frozen arrays, a throwing getter
const typed = new Float64Array([1.25, 2.5]);
const frozen = Object.freeze([4, 5, 6]);
function args() { return arguments; }
const argObj = args(7, 8, 9);
let t = 0;
for (let i = 0; i < 3000; i++) {
  const r = i % 3 === 0 ? typed : i % 3 === 1 ? frozen : argObj;
  const v = get(r, i % 4);
  t += v === undefined ? 0.5 : v;
}
out.push(t);
const thrower = { get boom() { throw new Error("boom"); } };
let caught = 0;
for (let i = 0; i < 2000; i++) { try { get(i % 2 ? thrower : [1], i % 2 ? "boom" : 0); } catch (e) { caught++; } }
out.push(caught);
console.log(out.join("\n"));
