// Loads through prototypes re-parented after definition (node's
// ObjectSetPrototypeOf(X.prototype, Y.prototype) pattern), with few and many
// methods.
function make(methods) {
  function Base() {}
  Base.prototype.baseMethod = function () { return 1; };
  function Derived() { this.x = 1; }
  for (let i = 0; i < methods; i++) Derived.prototype['m' + i] = function () { return i; };
  Object.setPrototypeOf(Derived.prototype, Base.prototype);
  return Derived;
}
function bench(name, D, n) {
  const o = new D();
  const f = (n) => { let c = 0; for (let i = 0; i < n; i++) { c += o.m0(); c += o.baseMethod(); c += o.absent === undefined ? 1 : 0; } return c; };
  f(n / 10);
  const t = performance.now();
  f(n);
  console.log(name.padEnd(14), ((performance.now() - t) * 1e6 / n).toFixed(1), 'ns/iter');
}
bench('8 methods', make(8), 2e6);
bench('70 methods', make(70), 2e6);
bench('300 methods', make(300), 2e6);
function makeSymbol() {
  function Base() {}
  Base.prototype.baseMethod = function () { return 1; };
  function Derived() { this.x = 1; }
  Derived.prototype.m0 = function () { return 0; };
  Derived.prototype[Symbol.asyncDispose] = function () {};
  Object.setPrototypeOf(Derived.prototype, Base.prototype);
  return Derived;
}
bench('symbol key', makeSymbol(), 2e6);
