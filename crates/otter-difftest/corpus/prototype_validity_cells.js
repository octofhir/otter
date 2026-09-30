// Shared chain proofs must survive collection and retire before prototype effects.
function read(o) { return o.x; }
function call(o) { return o.f(); }
function write(o, value) { o.x = value; return o.x; }
function warm(o) {
  let sum = 0;
  for (let i = 0; i < 1200; i++) sum += read(o) + call(o);
  return sum;
}
const root = { x: 2, f() { return this.x + 1; } };
const links = [root];
for (let i = 0; i < 12; i++) links.push(Object.create(links[links.length - 1]));
const a = Object.create(links[12]);
const b = Object.create(links[12]);
const results = [warm(a), warm(b)];
root.x = 4;
results.push(read(a), call(b));
root.f = function () { return this.x + 10; };
results.push(warm(a));
links[6].x = 7;
results.push(read(a), call(b));
delete links[6].x;
results.push(read(a));
Object.defineProperty(links[6], 'x', { get() { return 11; }, configurable: true });
results.push(read(a), call(b));
delete links[6].x;
Object.setPrototypeOf(links[6], { x: 13, f() { return this.x + 20; } });
results.push(warm(a));
// The same generated store sees normal receivers and a watched prototype.
const writable = { x: 1 };
const child = Object.create(writable);
for (let i = 0; i < 1500; i++) write({ x: 0 }, i);
results.push(read(child));
write(writable, 17);
results.push(read(child));
// Megamorphic sites share proofs but preserve distinct prototype identities.
const many = [];
for (let i = 0; i < 12; i++) {
  const p = Object.create(root);
  p['p' + i] = i;
  const o = Object.create(p);
  o['q' + i] = i;
  many.push(o);
}
let sum = 0;
for (let i = 0; i < 2400; i++) sum += read(many[i % many.length]);
results.push(sum);
root.x = 19;
for (let i = 0; i < many.length; i++) results.push(read(many[i]));
// Constructor transitions must discard a proof when an inherited setter appears.
function C(value) { this.v = value; }
for (let i = 0; i < 1500; i++) new C(i);
let setterCalls = 0;
Object.defineProperty(C.prototype, 'v', { set(v) { setterCalls++; this.observed = v; }, configurable: true });
const c = new C(23);
results.push(setterCalls, c.observed, Object.hasOwn(c, 'v'));
delete C.prototype.v;
results.push(new C(29).v);
// Global-object binding writes are also prototype mutations.
globalThis.prototypeValidityValue = 31;
const globalChild = Object.create(globalThis);
function globalRead(o) { return o.prototypeValidityValue; }
function globalWrite(v) { prototypeValidityValue = v; }
for (let i = 0; i < 1500; i++) { globalRead(globalChild); globalWrite(31); }
globalWrite(37);
results.push(globalRead(globalChild));
delete globalThis.prototypeValidityValue;
console.log(JSON.stringify(results));
