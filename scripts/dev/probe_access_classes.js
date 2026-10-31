// Keyed and named access classes that generated code handles: Int32 keys on
// shaped and dictionary objects, prototype-held, accessor, non-writable and
// frozen index keys, Arrays with named bags, arguments, String wrappers,
// proxies, typed arrays, holes, strings, dictionary prototypes and
// megamorphic primitive receivers. Every loop runs long enough to reach the
// optimizing tier; the printed digest must match node.
"use strict";
const out = [];
function digest(label, value) { out.push(label + "=" + JSON.stringify(value)); }

function readKey(o, k) { return o[k]; }
function writeKey(o, k, v) { o[k] = v; }

function intKeys() {
  const shaped = {}; for (let i = 0; i < 8; i++) shaped[i * 3] = i;
  const dict = {}; for (let i = 0; i < 300; i++) dict[i * 7] = i; delete dict[7];
  const proto = { 5: "p5", 6: "p6" };
  const child = Object.create(proto); child[1] = "c1";
  const acc = {}; let hits = 0;
  Object.defineProperty(acc, "4", { get() { hits++; return "g4"; }, configurable: true });
  const ro = {}; Object.defineProperty(ro, "2", { value: "ro", writable: false });
  const frozen = Object.freeze({ 0: "f0", 1: "f1" });
  const setterProto = {}; let seen = [];
  Object.defineProperty(setterProto, "3", { set(v) { seen.push(v); }, get() { return "s3"; } });
  const viaSetter = Object.create(setterProto);
  let s = 0, t = "";
  for (let n = 0; n < 20000; n++) {
    const k = n & 31;
    const a = readKey(shaped, k); if (a !== undefined) s += a;
    const b = readKey(dict, k * 7); if (b !== undefined) s += b;
    const c = readKey(child, k & 7); if (c !== undefined && n < 40) t += c;
    if (n < 64) t += String(readKey(acc, k & 7));
    if (n % 997 === 0) t += readKey(ro, 2) + readKey(frozen, n & 1);
    if (n % 1999 === 0) writeKey(viaSetter, 3, n);
    writeKey(shaped, k & 7 ? (k & 7) * 3 : 0, n);
    writeKey(dict, (k + 1) * 7, n);
    try { writeKey(ro, 2, n); } catch (e) { if (n === 0) t += e.constructor.name; }
    try { writeKey(frozen, 0, n); } catch (e) { if (n === 0) t += e.constructor.name; }
  }
  digest("intKeys", [s, t, hits, seen, shaped[0], shaped[21], dict[14], dict[7], ro[2], frozen[0],
    Object.keys(shaped).join(), Object.keys(viaSetter), viaSetter[3]]);
}

function addsAndDeletes() {
  let s = 0;
  for (let n = 0; n < 3000; n++) {
    const o = { x: 1 };
    writeKey(o, n & 15, n);
    writeKey(o, 100 + (n & 3), 1);
    if (n & 1) delete o[n & 15];
    s += (readKey(o, n & 15) ?? -1) + readKey(o, 100 + (n & 3));
    s += readKey(o, 999) === undefined ? 1 : 0;
  }
  digest("adds", s);
}

function exotic() {
  const arr = [10, , 30]; arr.name = "bag"; arr[5] = 50;
  const sw = new String("abc");
  const prox = new Proxy({ 1: "t1" }, { get(target, key) { return "P" + String(key) + (target[key] ?? ""); } });
  const ta = new Int16Array([7, 8, 9]);
  function args() { return arguments; }
  const ar = args(1, 2, 3);
  function mapped(a, b) { arguments[1] = "m"; return [a, b, arguments[0]]; }
  const recv = [arr, sw, prox, ta, ar, "str", { 0: "o0", 1: "o1", 2: "o2" }, [1, 2, 3]];
  const acc = [];
  for (let n = 0; n < 8000; n++) {
    const r = recv[n & 7];
    const v0 = readKey(r, n % 3), v1 = readKey(r, 5);
    if (n < 16) acc.push(String(v0) + "/" + String(v1));
  }
  let thrown = 0;
  for (let n = 0; n < 4000; n++) {
    try { writeKey(recv[(n & 3) + 4], n & 1, n); } catch (e) { thrown++; }
  }
  digest("exotic", [thrown, acc.join("|"), String(ar[0]) + ar[1], ta[0], ta[1], recv[6][0], recv[7][1], mapped(1, 2)]);
}

function holesAndProtoIndices() {
  const holey = [1, , 3, , 5];
  let s = 0;
  for (let n = 0; n < 6000; n++) { const v = readKey(holey, n % 5); s += v === undefined ? 100 : v; }
  Array.prototype[1] = "proto1";
  for (let n = 0; n < 2000; n++) { const v = readKey(holey, 1); if (n === 0) s += v; }
  delete Array.prototype[1];
  Object.prototype[3] = "op3";
  const plain = { 0: 1 };
  let t = "";
  for (let n = 0; n < 2000; n++) { const v = readKey(plain, 3); if (n === 0) t = v; }
  delete Object.prototype[3];
  t += String(readKey(plain, 3));
  digest("holes", [s, t]);
}

function namedClasses() {
  // Dictionary-mode receivers and prototypes, inherited getters, typed array
  // length through a replaced prototype, primitives at megamorphic sites.
  function Fake(n) { const b = new Uint8Array(n); Object.setPrototypeOf(b, Fake.prototype); return b; }
  Object.setPrototypeOf(Fake.prototype, Uint8Array.prototype);
  const fb = Fake(12);
  const dproto = Object.create({ get g() { return this.k * 2; }, own: 2 }); dproto.k = 3; dproto.z = 1; delete dproto.z;
  const dict = { a: 1, b: 2, c: 3 }; delete dict.c;
  const nodes = [{ type: "A" }, "str", { x: 1, type: "B" }, 7, { y: 1, type: "C" }, true, { z: 1, type: "D" }, { w: 1, type: "E" }];
  let s = 0, t = "";
  for (let n = 0; n < 20000; n++) {
    s += fb.length + dproto.g + dproto.own + dict.a + (dict.nope === undefined ? 1 : 0);
    const node = nodes[n & 7];
    const ty = node.type;
    if (n < 8) t += String(ty) + ",";
    s += node.length === undefined ? 0 : node.length;
  }
  digest("named", [s, t, fb.subarray(1, 3).length]);
}

function typedLength() {
  // Views on replaced prototypes, then a shadowing `length` on a prototype
  // and on a kind prototype: every tier must answer what [[Get]] answers.
  function Buf(n) { const b = new Uint8Array(n); b.__proto__ = Buf.prototype; return b; }
  Buf.prototype.__proto__ = Uint8Array.prototype;
  Buf.prototype.tag = "buf";
  const views = [new Uint8Array(3), Buf(5), new Float64Array(2), Buf(1)];
  function len(v) { return v.length; }
  let s = 0, t = "";
  for (let n = 0; n < 20000; n++) { const v = views[n & 3]; s += len(v); if (n < 4) t += String(v.tag); }
  const shadow = new Int16Array(4);
  Object.setPrototypeOf(shadow, Object.create(Int16Array.prototype, { length: { value: 42 } }));
  for (let n = 0; n < 5000; n++) s += len(shadow) + len(views[n & 1]);
  Object.defineProperty(Uint8Array.prototype, "length", { value: 7, configurable: true });
  for (let n = 0; n < 5000; n++) s += len(views[0]) + len(views[2]);
  delete Uint8Array.prototype.length;
  for (let n = 0; n < 5000; n++) s += len(views[0]);
  digest("typedLength", [s, t, views[1].length, views[0].length, shadow.length]);
}

intKeys();
addsAndDeletes();
exotic();
holesAndProtoIndices();
namedClasses();
typedLength();
console.log(out.join("\n"));
