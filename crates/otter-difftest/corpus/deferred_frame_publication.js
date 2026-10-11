// Call-free bodies need their activation only when a cold boundary observes it.
// Every cold entry and exit preserves actuals, receiver, SELF and caller roots.
const out = [];
class Cell {
  constructor(x, y) { this.x = x; this.y = y; }
}
function readA(o, n) { return o.x + n; }
function readB(o, n) { return o.y + n; }
function readC(o, n) { return o.x - n; }
function readD(o, n) { return o.y - n; }
function readE(o, n) { return o.x * n; }
function write(o, n) { o.x = n; return o.y; }
function invoke(fn, o, n) { return fn(o, n); }
const readers = [readA, readB, readC, readD, readE];
const cell = new Cell(7, 11);
let checksum = 0;
for (let i = 0; i < 30000; i++) {
  checksum += invoke(readers[i % readers.length], cell, 3);
  write(cell, 7);
}
out.push(['fields', checksum, write(cell, 13), readA(cell, 5), readB(cell, 5)]);

function arity(a, b) {
  'use strict';
  return (a === undefined ? 2 : a) + (b === undefined ? 3 : b);
}
function args(a, b) {
  'use strict';
  return arguments.length * 100 + (arguments[0] === undefined ? 10 : arguments[0])
    + (arguments[1] === undefined ? 20 : arguments[1]);
}
for (let i = 0; i < 10000; i++) { arity(7, 11); args(7, 11); }
const bound = arity.bind(null, 7);
out.push(['actuals', arity(), arity(7), arity(7, 11, 13, 17),
  arity(...[7, 11]), arity.apply(null, [7]), bound(),
  args(), args(7), args(7, 11, 13), args(...[7, 11, 13, 17])]);

function sloppy() { return this.frameValue; }
function strict() { 'use strict'; return this === null ? 'null' : typeof this; }
function makeArrow() { return n => this.frameValue + n; }
const receiver = { frameValue: 19 };
const arrow = makeArrow.call(receiver);
for (let i = 0; i < 10000; i++) {
  sloppy.call(receiver);
  strict.call(receiver);
  arrow.call({ frameValue: -1 }, 2);
}
Number.prototype.frameValue = 23;
String.prototype.frameValue = 29;
globalThis.frameValue = 31;
out.push(['receiver', sloppy.call(receiver), sloppy.call(5), sloppy.call('s'),
  sloppy.call(null), sloppy.call(undefined), strict.call(5), strict.call(null),
  strict.call(undefined), arrow.call({ frameValue: 100 }, 3)]);
delete Number.prototype.frameValue;
delete String.prototype.frameValue;
delete globalThis.frameValue;

function Base(n) { this.value = n; this.target = new.target; return 1; }
class Derived extends Base {
  constructor(n) { super(n); this.derived = true; }
}
for (let i = 0; i < 10000; i++) {
  Base.call(receiver, 5);
  new Base(7);
}
const base = new Base(37);
const derived = new Derived(41);
Base.call(receiver, 43);
out.push(['constructor', base.value, base.target === Base,
  derived.value, derived.target === Derived, derived.derived,
  receiver.value, receiver.target === undefined]);

function numeric(a, b) {
  const sum = a + b;
  const product = sum * b;
  return product - b;
}
for (let i = 0; i < 10000; i++) numeric(2, 3);
out.push(['arithmetic-exit', numeric(2, 3), numeric(2147483647, 2),
  numeric('4', 2), numeric(undefined, 2) !== numeric(undefined, 2)]);

// The published caller retains a fresh object while a formerly pure callee
// deoptimizes and its interpreter continuation allocates or re-enters JS.
let coercions = 0;
let getters = 0;
let finalizers = 0;
const marker = { code: 47 };
function outer(fn, o, n) {
  const retained = { first: 'outer', second: o.y };
  try {
    const value = fn(o, n);
    const after = { text: retained.first, value: retained.second };
    return [value, after.text, after.value, o.y];
  } catch (error) {
    return [error === marker, retained.first, retained.second, o.y];
  } finally { finalizers++; }
}
const cold = new Cell(53, 59);
for (let i = 0; i < 6000; i++) outer(readers[i % readers.length], cold, 2);
function churn(value) {
  let result = value;
  for (let i = 0; i < 48; i++) {
    const fresh = { value: i, previous: result };
    result = fresh.value + value;
  }
  return result - 47;
}
cold.x = { valueOf() { coercions++; cold.y = 61; return churn(67); } };
finalizers = 0;
out.push(['coercion-exit', outer(readA, cold, 3), coercions, finalizers]);
Object.defineProperty(cold, 'x', {
  configurable: true,
  get() { getters++; cold.y = churn(71); throw marker; }
});
finalizers = 0;
out.push(['throw-exit', outer(readA, cold, 3), getters, finalizers]);
Object.defineProperty(cold, 'x', {
  configurable: true,
  get() { getters++; return churn(73); }
});
finalizers = 0;
out.push(['accessor-exit', outer(readA, cold, 3), getters, finalizers]);
delete cold.x;
Object.setPrototypeOf(cold, { x: 79 });
out.push(['prototype-exit', outer(readA, cold, 3)]);

// Neither a noncollecting native call nor a loop poll loses its frame contract.
const map = new Map([['key', new Cell(83, 89)]]);
function nativeRead(o, key) { return o.get(key).x; }
for (let i = 0; i < 10000; i++) nativeRead(map, 'key');
out.push(['native', nativeRead(map, 'key')]);
map.get = function () { return new Cell(churn(97), 101); };
out.push(['native-replaced', nativeRead(map, 'key')]);

function loop(o, n) {
  let sum = 0;
  for (let i = 0; i < n; i++) sum += o.x;
  return sum;
}
const loopCell = new Cell(103, 107);
for (let i = 0; i < 1000; i++) loop(loopCell, 80);
out.push(['loop', loop(loopCell, 20000)]);
let loopGets = 0;
Object.defineProperty(loopCell, 'x', {
  get() { loopGets++; return churn(109); }
});
out.push(['loop-exit', loop(loopCell, 8), loopGets]);
console.log(JSON.stringify(out));
