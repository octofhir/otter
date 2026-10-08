//! asm.js translation and linking.
//!
//! # Contents
//! - Accepted modules translate to valid WebAssembly.
//! - Constructs whose JavaScript value differs from a naive translation are
//!   rejected (they run as JavaScript).
//! - End to end: linked modules compute what Node computes for the same
//!   JavaScript, including heap aliasing, re-entry, throws and fallback.

use super::translate::{Heap, translate};
use crate::WebApiBuilderExt;
use otter_runtime::{Runtime, SourceInput};

fn translated(source: &str) -> Vec<u8> {
    let translation = translate(
        source,
        Some(Heap {
            len: 65536,
            reserved: false,
        }),
    )
    .unwrap_or_else(|invalid| panic!("rejected: {}", invalid.0));
    let engine = wasmtime::Engine::default();
    wasmtime::Module::validate(&engine, &translation.wasm)
        .unwrap_or_else(|error| panic!("invalid wasm: {error:?}"));
    translation.wasm
}

fn rejected(source: &str) -> &'static str {
    match translate(
        source,
        Some(Heap {
            len: 65536,
            reserved: false,
        }),
    ) {
        Ok(_) => panic!("accepted: {source}"),
        Err(invalid) => invalid.0,
    }
}

fn module(body: &str) -> String {
    format!(
        "function M(stdlib, foreign, heap) {{ \"use asm\"; \
         var H8 = new stdlib.Int8Array(heap); var H32 = new stdlib.Int32Array(heap); \
         var F64 = new stdlib.Float64Array(heap); {body} }}"
    )
}

#[test]
fn control_flow_heap_and_tables_translate_to_valid_wasm() {
    translated(&module(
        "var T = 0; \
         function f(a, b) { a = a|0; b = +b; var i = 0, s = 0.0; \
           L: for (i = 0; (i|0) < (a|0); i = i + 1 | 0) { \
             switch (i|0) { case 0: case 1: continue L; case 2: break; default: s = s + b; } \
             H32[i << 2 >> 2] = (H8[i]|0) + 1 | 0; F64[i << 3 >> 3] = s; } \
           return +s; } \
         function g(x) { x = x|0; return t[x & 1](x)|0; } \
         function h(x) { x = x|0; return x + 1 | 0; } \
         var t = [h, h]; \
         return { f: f, g: g };",
    ));
}

#[test]
fn intish_loads_never_join_additions() {
    // `HEAP32[i] + 1` is NaN in JavaScript when `i` is out of bounds.
    assert_eq!(
        rejected(&module(
            "function f(i) { i = i|0; return (H32[i >> 2] + 1)|0; } return f;"
        )),
        "illegal types for + or -"
    );
}

#[test]
fn word_views_need_a_matching_shift() {
    assert_eq!(
        rejected(&module(
            "function f(i) { i = i|0; return H32[i]|0; } return f;"
        )),
        "expected shift of word size"
    );
    assert_eq!(
        rejected(&module(
            "function f(i) { i = i|0; return H32[i >> 3]|0; } return f;"
        )),
        "expected heap access shift to match view"
    );
}

#[test]
fn non_asm_constructs_are_rejected() {
    rejected(&module(
        "function f(x) { x = x|0; x += 1; return x|0; } return f;",
    ));
    rejected(&module(
        "function f(x) { x = x|0; return ~~x|0; } return f;",
    ));
    rejected(&module(
        "function f(x) { x = x|0; L: if (x) x = 1; return x|0; } return f;",
    ));
    rejected(&module("var o = {}; function f() {} return f;"));
    rejected("function M(stdlib) { var x = 0; function f() {} return f; }");
}

fn run(script: &str) -> String {
    let mut runtime = Runtime::builder()
        .with_web_apis()
        .build()
        .expect("runtime with Web APIs");
    runtime
        .run_script(SourceInput::from_javascript(script.to_string()), "asm:test")
        .expect("script runs")
        .completion_string()
        .to_string()
}

const SEMANTICS: &str = r#"
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
const heap = new ArrayBuffer(SIZE);
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
out.push(m.oobstore(SIZE), m.oobstore(SIZE - 4), m.oobstore(-4), m.neg(5), m.neg(-2147483648), m.chain(1, 2, 3));
u8[SIZE - 1] = 255;
out.push(m.loads(SIZE - 1), m.loads(SIZE + 100), m.loadf(SIZE), m.loadd(SIZE + 8), m.load32(SIZE),
         m.loadu32(-8), m.loadd(SIZE - 8), m.load32(SIZE - 4));
out.push(typeof m.div, m.div.length, m.div.name, m.f32.length);
out.join(",") + "|" + logs.join(";");
"#;

fn semantics(size: u32) -> String {
    run(&SEMANTICS.replace("SIZE", &size.to_string()))
}

#[test]
fn linked_module_matches_javascript() {
    // Node's output for the same program run as JavaScript.
    assert_eq!(
        semantics(65536),
        "3,-3,0,-2147483648,2147483647,-1,0,0,3,1.5,-1.5,NaN,3,-3,5,0,0,-1073741824,0,0,12,9,\
         1056,-123456,-123456,-64,1.100000023841858,1.100000023841858,2.718281828459045,\
         1.100000023841858,-123456,4294843840,13.57120362777794,2.873142022984102,12,37,\
         1.6500000953674316,2500,10,23,12,9,99,1,2,4,0,5,0,0,102,9,-2,42,-8,8.25,16.5,6,4,4,0,\
         0,5,0,-5,-2147483648,1,65278,0,NaN,NaN,0,0,-5.486150228671794e+303,-16777211,\
         function,2,div,1|21:10.5;-4:-2"
    );
}

#[test]
fn reserved_heap_matches_javascript_past_the_end() {
    // A 16 MiB heap sits in a reservation: loads past it read zero
    // unchecked, float misses select NaN, and stores past it are dropped.
    assert_eq!(
        semantics(1 << 24),
        "3,-3,0,-2147483648,2147483647,-1,0,0,3,1.5,-1.5,NaN,3,-3,5,0,0,-1073741824,0,0,12,9,\
         1056,-123456,-123456,-64,1.100000023841858,1.100000023841858,2.718281828459045,\
         1.100000023841858,-123456,4294843840,13.57120362777794,2.873142022984102,12,37,\
         1.6500000953674316,2500,10,23,12,9,99,1,2,4,0,5,0,0,102,9,-2,42,-8,8.25,16.5,6,4,4,0,\
         0,5,0,-5,-2147483648,1,65278,0,NaN,NaN,0,0,-5.486150228671794e+303,-16777211,\
         function,2,div,1|21:10.5;-4:-2"
    );
}

#[test]
fn linked_heap_cannot_detach() {
    assert_eq!(
        run(r#"
            function M(stdlib, foreign, heap) {
              "use asm";
              var H32 = new stdlib.Int32Array(heap);
              function f(i) { i = i|0; return H32[i >> 2]|0; }
              return f;
            }
            const heap = new ArrayBuffer(65536);
            M(globalThis, null, heap);
            let threw = false;
            try { heap.transfer(); } catch (e) { threw = e instanceof TypeError; }
            [threw, heap.byteLength, heap.detached].join(",");
        "#),
        "true,65536,false"
    );
}

#[test]
fn reentry_throws_and_fallback_match_javascript() {
    assert_eq!(
        run(r#"
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
            let r;
            r = R(globalThis, { cb(v) { return v > 0 ? r.a(v - 1) + r.b(v) : 100; } }, new ArrayBuffer(4096));
            out.push(r.a(5), r.c());
            const thrower = R(globalThis, { cb(v) { throw new RangeError("boom " + v); } }, new ArrayBuffer(4096));
            try { thrower.a(3); } catch (e) { out.push(e.name + ":" + e.message); }
            function Bad(stdlib) { "use asm"; var o = {}; function f() { return 1; } return { f: f, o: o }; }
            const bad = Bad(globalThis);
            out.push(bad.f(), typeof bad.o);
            const odd = R(globalThis, { cb(v) { return v + 1; } }, new ArrayBuffer(5000));
            out.push(odd.a(4), odd.b(2));
            function S(stdlib) { "use asm"; var floor = stdlib.Math.floor; function f(x) { x = +x; return +floor(x); } return f; }
            out.push(S({ Math: { floor: (x) => 42 } })(1.5), S(globalThis)(1.5), S(globalThis).name);
            function E() { "use asm"; function f() { return 1; } return { f: f, g: f }; }
            const e = E();
            out.push(e.f === e.g, Object.keys(e).join("/"));
            out.join(",");
        "#),
        "145,15,RangeError:boom 3,1,object,5,6,42,1,f,true,f/g"
    );
}

#[test]
#[ignore = "dumps the zlib translation for codegen inspection"]
fn dump_zlib_codegen() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../benchmarks/results");
    let source = std::fs::read_to_string(format!("{dir}/../../scripts/dev/probe/asm/zlib_asm.js"))
        .expect("extracted zlib module");
    let translation = translate(
        &source,
        Some(Heap {
            len: 134_217_728,
            reserved: true,
        }),
    )
    .expect("translates");
    std::fs::write(format!("{dir}/zlib_asm.wasm"), &translation.wasm).expect("write wasm");
    let mut config = wasmtime::Config::new();
    config
        .memory_reservation(134_217_728)
        .memory_guard_size(0)
        .memory_reservation_for_growth(0)
        .memory_may_move(false)
        .guard_before_linear_memory(false);
    let engine = wasmtime::Engine::new(&config).expect("engine");
    let elf = engine
        .precompile_module(&translation.wasm)
        .expect("compiles");
    std::fs::write(format!("{dir}/zlib_asm.cwasm"), elf).expect("write elf");
}
