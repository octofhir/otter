// Named and symbol keys added to arrays from hot code, and every case the
// fast add must leave to [[Set]].
const out = [];
const k = Symbol('k');
function addStr(a, v) { a.tag = v; return a; }
function addSym(a, v) { a[k] = v; return a; }
for (let i = 0; i < 30000; i++) { addStr([i], i); addSym([i], i); }
const a = addSym(addStr([1, 2], 'x'), 'y');
out.push(a.tag, a[k], Object.keys(a).join(), JSON.stringify(Object.getOwnPropertyDescriptor(a, 'tag')), a.length);
const frozen = Object.freeze([1]); try { (function () { 'use strict'; addStr(frozen, 1); })(); } catch (e) { out.push(e.name); } out.push(frozen.tag);
let seen; Object.defineProperty(Array.prototype, 'tag', { set(v) { seen = v; }, configurable: true });
const b = addStr([3], 'via-setter'); out.push(seen, Object.prototype.hasOwnProperty.call(b, 'tag')); delete Array.prototype.tag;
Object.defineProperty(Array.prototype, 'ro', { value: 1, writable: false, configurable: true });
function addRo(x) { x.ro = 2; return x; } const c = addRo([1]); out.push(c.ro, Object.prototype.hasOwnProperty.call(c, 'ro')); delete Array.prototype.ro;
class Sub extends Array {} const s = addStr(new Sub(), 'sub'); out.push(s.tag, s instanceof Sub);
const d = addStr([1], 'z'); d.length = 0; out.push(d.tag, d.length);
console.log(JSON.stringify(out));
