// Runtime operations that hold an operand while user code allocates: a
// computed super key's toString, proxy set traps (and their absence), a
// setter reached from a primitive base, a getter on a collection, function
// own properties, for-in over dictionaries, Array map/filter/unshift fast
// paths and the generic pop/shift/at/with/toReversed/toSpliced over
// array-likes whose getters allocate. Every operand must arrive intact.
function churn() {
  const junk = [];
  for (let i = 0; i < 120; i++) junk.push({ i, s: 'x' + i, t: [i, i + 1] });
  return junk.length;
}
const out = [];

class A { set v(x) { this.got = x; } }
class B extends A { put(k, x) { super[k] = x; return this.got; } }
const b = new B();
const key = { toString() { churn(); return 'v'; } };
let superSum = 0;
for (let i = 0; i < 60; i++) superSum += b.put(key, { n: i }).n;
out.push(superSum);

const trapped = {};
const viaTrap = new Proxy(trapped, { set(t, k, v) { churn(); t[k] = v; return true; } });
const forwarded = {};
const viaTarget = new Proxy(forwarded, {});
let proxySum = 0;
for (let i = 0; i < 60; i++) {
  viaTrap.field = { n: i };
  viaTarget.other = { n: i * 2 };
  proxySum += trapped.field.n + forwarded.other.n;
}
out.push(proxySum);

let sunk = null;
Object.defineProperty(String.prototype, 'sink', {
  configurable: true,
  set(v) { churn(); sunk = v; },
});
let primitiveSum = 0;
for (let i = 0; i < 60; i++) { 'abc'.sink = { n: i }; primitiveSum += sunk.n; }
delete String.prototype.sink;
out.push(primitiveSum);

Object.defineProperty(Map.prototype, 'probe', {
  configurable: true,
  get() { churn(); return this.size; },
});
const map = new Map([[1, 2], [3, 4]]);
let probeSum = 0;
for (let i = 0; i < 60; i++) probeSum += map.probe;
delete Map.prototype.probe;
out.push(probeSum);

function fn() {}
let fnSum = 0;
for (let i = 0; i < 60; i++) { churn(); fn.custom = { n: i }; fnSum += fn.custom.n + fn.name.length; }
out.push(fnSum);

const dict = {};
for (let i = 0; i < 40; i++) dict['k' + i] = i;
delete dict.k3;
let forInCount = 0;
for (let r = 0; r < 30; r++) { churn(); for (const k in dict) forInCount += k.length; }
out.push(forInCount);

let fastSum = 0;
for (let r = 0; r < 40; r++) {
  const nums = [];
  for (let i = 0; i < 64; i++) nums.push(i);
  const mapped = nums.map((x) => x * 3 + 1);
  const filtered = nums.filter((x) => x % 4 === 0);
  churn();
  fastSum += mapped[63] + mapped.length + filtered.length + filtered[filtered.length - 1];
}
out.push(fastSum);

const front = [];
for (let i = 0; i < 80; i++) front.unshift({ a: i }, { b: i }, { c: i });
out.push(front.length, front[0].a, front[1].b, front[2].c, front[239].c);

function arrayLike(n) {
  const o = { length: n };
  for (let i = 0; i < n; i++) {
    Object.defineProperty(o, i, {
      configurable: true, enumerable: true,
      get() { churn(); return { n: i }; },
      set(v) { churn(); },
    });
  }
  return o;
}
const index = { valueOf() { churn(); return 2; } };
const P = Array.prototype;
const generic = [];
for (let r = 0; r < 8; r++) {
  generic.push(P.at.call(arrayLike(5), index).n);
  generic.push(P.with.call(arrayLike(5), index, { n: 99 }).map((x) => x.n).join(''));
  generic.push(P.toReversed.call(arrayLike(5)).map((x) => x.n).join(''));
  generic.push(P.toSpliced.call(arrayLike(6), 1, 2, { n: 7 }, { n: 8 }).map((x) => x.n).join(''));
}
const popped = arrayLike(4);
Object.defineProperty(popped, 'length', { value: 4, writable: true });
generic.push(P.pop.call(popped).n);
const shifted = arrayLike(4);
generic.push(P.shift.call(shifted).n);
out.push(generic.join(','));

console.log(out.join('|'));
