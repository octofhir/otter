function add(a, b) { return a + b; }
var counter = 0;
function outer(x) {
  let y = x * 2;
  const inner = (z) => y + z + counter;
  function deep(k) { return inner(k) * 2; }
  return deep(1);
}
const obj = {
  m(a) { return a + 1; },
  get g() { return 42; },
  set s(v) { counter = v; },
  async am() { return 5; },
  *gen() { yield 1; yield 2; },
};
class C {
  #p = 3;
  constructor(v) { this.v = v; }
  get pv() { return this.#p + this.v; }
  meth(a) { return this.v + a; }
  static st() { return 'st'; }
  #priv() { return this.#p * 10; }
  callPriv() { return this.#priv(); }
}
class D extends C {
  meth(a) { return super.meth(a) * 2; }
}
function Ctor(x) { this.x = x; }
Ctor.prototype.get = function () { return this.x; };
const results = [];
results.push(add(1, 2));
results.push(outer(5));
results.push(obj.m(1), obj.g, (obj.s = 7, counter));
results.push([...obj.gen()].join(','));
const c = new C(1);
results.push(c.pv, c.meth(2), C.st(), c.callPriv());
results.push(new D(3).meth(1));
results.push(new Ctor(9).get());
results.push(add.toString());
results.push(obj.m.name, add.length, typeof obj.am().then);
try { C.prototype.callPriv.call({}); } catch (e) { results.push(e.constructor.name); }
function sloppyThis() { return typeof this; }
results.push(sloppyThis.call(5));
function args() { return arguments.length + arguments[1]; }
results.push(args(1, 2, 3));
function rec(n) { return n <= 0 ? 0 : n + rec(n - 1); }
results.push(rec(10));
function err() { throw new Error('boom'); }
try { err(); } catch (e) { results.push(e.stack.split('\n')[1].trim()); }
console.log(results.join(' | '));
