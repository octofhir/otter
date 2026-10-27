// Array.prototype.slice: dense fast path and every case it must leave alone.
const out = [];
const a = [1, 'two', { three: 3 }, 4, 5];
out.push(JSON.stringify([a.slice(), a.slice(1), a.slice(-2), a.slice(1, -1), a.slice(3, 1), a.slice(NaN, 2.7), a.slice(-Infinity, Infinity)]));
const holes = [1, , 3]; const h = holes.slice(); out.push(1 in h, h.length);
Array.prototype[1] = 'inherited'; out.push(JSON.stringify([1, , 3].slice())); delete Array.prototype[1];
class MyArray extends Array {} const m = MyArray.from([1, 2, 3]).slice(1); out.push(m instanceof MyArray, m.length);
const custom = [1, 2]; custom.constructor = function C(n) { return { length: 0, made: n }; }; custom.constructor[Symbol.species] = custom.constructor;
try { out.push(JSON.stringify(custom.slice())); } catch (e) { out.push(e.name); }
out.push(JSON.stringify(Array.prototype.slice.call({ length: 2, 0: 'x', 1: 'y' })));
out.push(JSON.stringify(a.slice({ valueOf() { return 2; } })));
const t = performance.now(); let n = 0; const big = Array.from({ length: 8 }, (_, i) => i);
for (let i = 0; i < 2e6; i++) n += big.slice(1).length;
console.log(JSON.stringify(out), n, ((performance.now() - t) * 1e6 / 2e6).toFixed(1) + 'ns');
