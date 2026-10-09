// Own properties added to and deleted from functions while optimized code
// proves their ordinary lookup: `f.call`, `f.name`, symbols, accessors,
// non-extensible functions, and re-adding after the last delete.
function f(x) { return x + 1; }
function user(x) { return f.call(null, x) + (f.extra === undefined ? 0 : f.extra); }
const sym = Symbol("s");
const out = [];
let acc = 0;
for (let round = 0; round < 3000; round++) {
  acc += user(round);
  if (round === 500) { f.extra = 5; }
  if (round === 900) { delete f.extra; }
  if (round === 1200) { f.call = function (t, x) { return x * 2; }; }
  if (round === 1500) { delete f.call; }
  if (round === 1800) { f[sym] = 1; delete f[sym]; }
  if (round === 2000) { Object.defineProperty(f, "extra", { get() { return 3; }, configurable: true }); }
  if (round === 2300) { delete f.extra; }
  acc |= 0;
}
out.push(acc, Object.getOwnPropertyNames(f).sort().join(), Object.getOwnPropertySymbols(f).length);
function g() {}
g.a = 1; Object.preventExtensions(g); delete g.a;
out.push(Object.isExtensible(g), (() => { try { "use strict"; g.b = 2; return "added"; } catch (e) { return e.name; } })());
function h() {}
h.p = 1; delete h.p; h.q = 2;
out.push(h.q, Object.keys(h).join(), f.name, f.length, delete f.name, f.name, h.hasOwnProperty("p"));
console.log(out.join("|"));
// Element deletes with every key kind on objects and functions.
const keyed = { true: 1, null: 2, undefined: 3, 10: 4, 1: 5 };
function fk() {}
fk[1] = 1; fk.s = 2; const ks = Symbol("k"); fk[ks] = 3;
console.log(delete keyed[true], delete keyed[null], delete keyed[undefined], delete keyed[10n], JSON.stringify(keyed),
  delete fk[1], delete fk[ks], delete fk["s"], Object.getOwnPropertySymbols(fk).length, Object.keys(fk).join());
// Symbol-keyed own properties of bound functions.
function target() {}
const bound = target.bind(null);
const bs = Symbol("b"), bt = Symbol("t");
Object.defineProperty(bound, bt, { value: 5, configurable: true, enumerable: true });
bound[bs] = 1;
console.log(bound[bt], bound[bs], bt in bound, Object.getOwnPropertySymbols(bound).length, Reflect.ownKeys(bound).length,
  Object.prototype.hasOwnProperty.call(bound, bs), JSON.stringify(Object.getOwnPropertyDescriptor(bound, bt)),
  delete bound[bt], bound[bt], bt in bound, Reflect.deleteProperty(bound, bs), Object.getOwnPropertySymbols(bound).length);
