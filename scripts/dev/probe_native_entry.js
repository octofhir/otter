// Native functions entered directly from generated code: results, thrown
// errors and their stacks, callbacks into JavaScript, allocation under GC
// pressure, polymorphic and generic call sites, receivers, and recursion
// through natives into stack overflow. Output must match node, also under
// OTTER_GC_STRESS.
'use strict';
const out = [];
const objs = [];
for (let i = 0; i < 64; i++) objs.push({ k: i, s: 'v' + i });

function hot(fn, n) { let r; for (let i = 0; i < n; i++) r = fn(i); return r; }

// Plain static natives, monomorphic.
out.push(hot((i) => Object.isExtensible(objs[i & 63]) ? 1 : 0, 20000));
out.push(hot((i) => Object.keys(objs[i & 63]).join('|'), 20000));
out.push(hot((i) => JSON.stringify(objs[i & 63]), 20000));
out.push(hot((i) => objs[i & 63].toString(), 20000));
out.push(hot((i) => String(i).padStart(6, '0'), 20000));

// Polymorphic and generic sites over natives and closures.
const fns = [Object.isFrozen, Object.isExtensible, (o) => o.k > 3, Object.keys];
out.push(hot((i) => String(fns[i & 3](objs[i & 63])), 20000));
const meth = [Array.prototype.indexOf, Array.prototype.includes, Array.prototype.lastIndexOf];
const arr = [3, 1, 4, 1, 5, 9, 2, 6];
out.push(hot((i) => meth[i % 3].call(arr, i & 7), 20000));

// Allocating natives keep their arguments and receiver across collections.
let acc = 0;
for (let i = 0; i < 30000; i++) {
  const parts = ('a' + i + ',b,' + (i * 3)).split(',');
  const joined = parts.concat([String(i)]).join('-');
  const o = Object.assign({}, objs[i & 63], { j: joined });
  acc += Object.entries(o).length + o.j.length;
}
out.push(acc);

// Callbacks: natives calling back into compiled code.
out.push(hot((i) => [1, 2, 3, i].map((v) => v * 2).reduce((a, b) => a + b, 0), 20000));
out.push(hot((i) => objs.filter((o) => (o.k & 7) === (i & 7)).length, 2000));

// Errors thrown by natives, caught in compiled code.
let caught = 0, message = '';
for (let i = 0; i < 20000; i++) {
  try {
    Object.defineProperty(i & 1 ? objs[0] : 1, 'x', { value: i });
  } catch (e) {
    caught++;
    message = e.constructor.name + ': ' + (e instanceof TypeError);
  }
}
out.push(caught, message);

// A native's error stack names the compiled JavaScript caller.
function thrower(v) { return JSON.parse(v); }
let stackLine = '';
for (let i = 0; i < 20000; i++) {
  try { thrower(i === 19999 ? '{bad' : '1'); } catch (e) {
    stackLine = String(e.stack).split('\n').slice(1, 3).join('').includes('thrower');
  }
}
out.push(stackLine);

// A native receiver that is a primitive, and `this` checks.
out.push(hot((i) => String.prototype.toUpperCase.call('ab' + (i & 3)), 20000));
let bad = '';
try { hot(() => String.prototype.trim.call(undefined), 20000); } catch (e) { bad = e.constructor.name; }
out.push(bad);

// Recursion through a native into stack overflow, then normal calls again.
function deep() { return [0].map(deep); }
let overflow = '';
try { deep(); } catch (e) { overflow = e.constructor.name; }
out.push(overflow, hot((i) => Math.max(i, 3), 100));

// Natives entered through apply/call/bind/Reflect.
out.push(hot((i) => Math.max.apply(null, [i & 7, 4, 2]), 20000));
out.push(hot((i) => Reflect.apply(Math.min, null, [i & 7, 4]), 20000));
const boundKeys = Object.keys.bind(null);
out.push(hot((i) => boundKeys(objs[i & 63]).length, 20000));

// `new` on a native constructor at a hot site.
out.push(hot((i) => new Array(i & 7).length + new Date(0).getTime(), 20000));

console.log(out.join('\n'));
