// asm.js semantics probe: compare `otter` output with `node`.
function M(stdlib, foreign, heap) {
  "use asm";
  var H8 = new stdlib.Int8Array(heap);
  var HU8 = new stdlib.Uint8Array(heap);
  var H16 = new stdlib.Int16Array(heap);
  var HU16 = new stdlib.Uint16Array(heap);
  var H32 = new stdlib.Int32Array(heap);
  var HU32 = new stdlib.Uint32Array(heap);
  var F32 = new stdlib.Float32Array(heap);
  var F64 = new stdlib.Float64Array(heap);
  var imul = stdlib.Math.imul;
  var fround = stdlib.Math.fround;
  var floor = stdlib.Math.floor;
  var sqrt = stdlib.Math.sqrt;
  var abs = stdlib.Math.abs;
  var min = stdlib.Math.min;
  var max = stdlib.Math.max;
  var sin = stdlib.Math.sin;
  var pow = stdlib.Math.pow;
  var clz32 = stdlib.Math.clz32;
  var PI = stdlib.Math.PI;
  var inf = stdlib.Infinity;
  var log = foreign.log;
  var base = foreign.base | 0;
  var scale = +foreign.scale;
  var g = 0;
  var d = 0.0;
  function div(a, b) { a = a|0; b = b|0; return (a|0) / (b|0) | 0; }
  function udiv(a, b) { a = a|0; b = b|0; return ((a>>>0) / (b>>>0)) | 0; }
  function rem(a, b) { a = a|0; b = b|0; return (a|0) % (b|0) | 0; }
  function urem(a, b) { a = a|0; b = b|0; return ((a>>>0) % (b>>>0)) | 0; }
  function dmod(a, b) { a = +a; b = +b; return +(a % b); }
  function toint(x) { x = +x; return ~~x; }
  function loads(i) { i = i|0; return (H8[i]|0) + (HU8[i]|0) + (H16[i>>1]|0) + (HU16[i>>1]|0) | 0; }
  function load32(i) { i = i|0; return H32[i>>2]|0; }
  function loadu32(i) { i = i|0; return +(HU32[i>>2]>>>0); }
  function loadf(i) { i = i|0; return +F32[i>>2]; }
  function loadd(i) { i = i|0; return +F64[i>>3]; }
  function store(i, v) { i = i|0; v = v|0; H32[i>>2] = v; H8[i+5|0] = v; return H32[i>>2]|0; }
  function stored(i, v) { i = i|0; v = +v; F32[i>>2] = v; return +F32[i>>2]; }
  function math(x) { x = +x; return +(floor(x) + sqrt(abs(x)) + +sin(x) + +pow(x, 2.0) + PI); }
  function imath(a, b) { a = a|0; b = b|0; return (imul(a, b)|0) + (min(a|0, b|0)|0) + (max(a|0, b|0)|0) + (clz32(a)|0) | 0; }
  function f32(x) { x = fround(x); return fround(x * fround(1.5)); }
  function loop(n) { n = n|0; var s = 0, i = 0; for (i = 0; (i|0) < (n|0); i = i + 1 | 0) { if ((i & 1) == 0) continue; s = s + i | 0; } return s|0; }
  function sw(x) { x = x|0; var r = 0; switch (x|0) { case 0: r = 10; break; case 1: r = 11; case 2: r = r + 12 | 0; break; case -1: r = 9; break; default: r = 99; } return r|0; }
  function dense(x) { x = x|0; var r = 0; switch (x|0) { case 3: r = 1; break; case 4: r = 2; break; case 5: r = 3; break; case 6: r = 4; break; case 8: r = 5; break; case 3: r = 6; break; } return r|0; }
  function labeled(n) { n = n|0; var c = 0; outer: while (1) { inner: do { c = c + 1 | 0; if ((c|0) > (n|0)) break outer; if ((c & 3) == 0) continue outer; } while (0); c = c + 100 | 0; } return c|0; }
  function cond(x) { x = x|0; return ((x|0) > 5 ? x : 0 - x | 0) | 0; }
  function callffi(x) { x = x|0; return log(x|0, +(x|0) * 0.5) | 0; }
  function useglobals() { g = g + base | 0; d = d + scale; return +(+(g|0) + d); }
  function tbl(i, x) { i = i|0; x = x|0; return T[i & 1](x)|0; }
  function inc(x) { x = x|0; return x + 1 | 0; }
  function dec(x) { x = x|0; return x - 1 | 0; }
  function oob() { return (H32[0x7ffffff0 >> 2]|0) + ~~+F64[0x7ffffff0 >> 3] | 0; }
  function oobstore(i) { i = i|0; H32[i>>2] = 5; return H32[i>>2]|0; }
  function neg(x) { x = x|0; return -x|0; }
  function chain(a, b, c) { a = a|0; b = b|0; c = c|0; return a + b - c + a | 0; }
  var T = [inc, dec];
  return { div: div, udiv: udiv, rem: rem, urem: urem, dmod: dmod, toint: toint, loads: loads,
           load32: load32, loadu32: loadu32, loadf: loadf, loadd: loadd, store: store,
           stored: stored, math: math, imath: imath, f32: f32, loop: loop, sw: sw, dense: dense,
           labeled: labeled, cond: cond, callffi: callffi, useglobals: useglobals, tbl: tbl,
           oob: oob, oobstore: oobstore, neg: neg, chain: chain };
}

const heap = new ArrayBuffer(65536);
const logs = [];
const m = M(globalThis, { log(a, b) { logs.push(a + ":" + b); return a * 2; }, base: 7, scale: 1.25 }, heap);
const out = [];
out.push(m.div(7, 2), m.div(-7, 2), m.div(5, 0), m.div(-2147483648, -1), m.udiv(-1, 2), m.rem(-7, 2),
         m.rem(5, 0), m.rem(-2147483648, -1), m.urem(-1, 7), m.dmod(5.5, 2), m.dmod(-5.5, 2), m.dmod(1, 0));
out.push(m.toint(3.7), m.toint(-3.7), m.toint(2 ** 40 + 5), m.toint(NaN), m.toint(Infinity),
         m.toint(-(2 ** 70) - 2 ** 30), m.toint(2 ** 63), m.toint(-0.5), m.toint("12"), m.toint({ valueOf() { return 9.9; } }));
const u8 = new Uint8Array(heap);
u8[100] = 200; u8[101] = 1;
out.push(m.loads(100));
out.push(m.store(200, -123456), new Int32Array(heap)[50], new Int8Array(heap)[51 * 4 + 1]);
out.push(m.stored(300, 1.1), new Float32Array(heap)[75]);
new Float64Array(heap)[50] = Math.E;
out.push(m.loadd(400), m.loadf(300), m.load32(200), m.loadu32(200));
out.push(m.math(2.5), m.math(-1.25), m.imath(7, -3), m.imath(0, 5), m.f32(1.1));
out.push(m.loop(100), m.sw(0), m.sw(1), m.sw(2), m.sw(-1), m.sw(5));
out.push(m.dense(3), m.dense(4), m.dense(6), m.dense(7), m.dense(8), m.dense(100), m.dense(-5));
out.push(m.labeled(10), m.cond(9), m.cond(2), m.callffi(21), m.callffi(-4));
out.push(m.useglobals(), m.useglobals(), m.tbl(0, 5), m.tbl(1, 5), m.tbl(3, 5), m.oob());
out.push(m.oobstore(65536), m.oobstore(65532), m.oobstore(-4), m.neg(5), m.neg(-2147483648), m.chain(1, 2, 3));
out.push(typeof m.div, m.div.length, m.div.name, m.f32.length);
let transferThrew = false;
try { heap.transfer(); } catch (e) { transferThrew = e instanceof TypeError; }
out.push(transferThrew, heap.byteLength);
console.log(out.join(","));
console.log(logs.join(";"));
