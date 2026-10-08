// asm.js re-entry, exceptions and fallback probe: compare with `node`.
function R(stdlib, foreign, heap) {
  "use asm";
  var cb = foreign.cb;
  var H32 = new stdlib.Int32Array(heap);
  function a(x) { x = x|0; return cb(x|0)|0; }
  function b(x) { x = x|0; H32[0] = (H32[0]|0) + x | 0; return x * 3 | 0; }
  function c() { return H32[0]|0; }
  return { a: a, b: b, c: c };
}
const out = [];
const heap = new ArrayBuffer(4096);
let r;
r = R(globalThis, { cb(v) { return v > 0 ? r.a(v - 1) + r.b(v) : 100; } }, heap);
out.push(r.a(5), r.c());
const thrower = R(globalThis, { cb(v) { throw new RangeError("boom " + v); } }, new ArrayBuffer(4096));
try { thrower.a(3); } catch (e) { out.push(e.name + ":" + e.message); }
// Not asm.js: runs as JavaScript.
function Bad(stdlib) {
  "use asm";
  var o = {};
  function f() { return 1; }
  return { f: f, o: o };
}
const bad = Bad(globalThis);
out.push(bad.f(), typeof bad.o);
// Link failure (wrong heap size) also runs as JavaScript.
const odd = R(globalThis, { cb(v) { return v + 1; } }, new ArrayBuffer(5000));
out.push(odd.a(4), odd.b(2));
// A fake stdlib fails the link; the body runs as JavaScript.
function S(stdlib) { "use asm"; var floor = stdlib.Math.floor; function f(x) { x = +x; return +floor(x); } return f; }
out.push(S({ Math: { floor: (x) => 42 } })(1.5), S(globalThis)(1.5), S(globalThis).name);
// Exports object identity and accessors.
function E() { "use asm"; function f() { return 1; } return { f: f, g: f }; }
const e = E();
out.push(e.f === e.g, Object.keys(e).join("/"));
console.log(out.join(","));
