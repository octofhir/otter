// Computed string-key loads and stores across the paths a megamorphic keyed
// access takes: shaped and dictionary receivers, inherited data, getters and
// setters on the chain, frozen receivers, prototype objects, arguments
// objects, String wrappers, and array reads past the end.
const out = [];
const log = (...values) => out.push(values.map(String).join(" "));

function load(o, k) { return o[k]; }
function store(o, k, v) { o[k] = v; return o[k]; }

const keys = ["a", "b", "c", "toString", "missing", "x" + 1, "0", "length"];
const proto = { inherited: 1 };
Object.defineProperty(proto, "getter", { get() { return "got:" + this.a; } });
Object.defineProperty(proto, "setter", { set(v) { this.seen = v; } });
const shaped = Object.create(proto);
shaped.a = 1; shaped.b = 2; shaped.c = 3;
const dict = Object.create(proto);
for (let i = 0; i < 64; i++) dict["k" + i] = i;
delete dict.k3;
dict.a = "da";

for (let round = 0; round < 3000; round++) {
  for (const k of keys) { load(shaped, k); load(dict, k); }
  load(shaped, "getter"); load(dict, "getter"); load(shaped, "inherited");
  store(shaped, "a", round); store(dict, "k" + (round % 64), round);
}
for (const k of [...keys, "getter", "inherited", "k3", "k10"]) {
  log("shaped", k, typeof load(shaped, k), load(shaped, k) === undefined ? "u" : String(load(shaped, k)).slice(0, 20));
  log("dict", k, typeof load(dict, k), load(dict, k) === undefined ? "u" : String(load(dict, k)).slice(0, 20));
}
log("setter", store(shaped, "setter", 7), shaped.seen, Object.hasOwn(shaped, "setter"));
log("dict setter", store(dict, "setter", 8), dict.seen, Object.hasOwn(dict, "setter"));

const frozen = Object.freeze({ a: 1 });
log("frozen", store(frozen, "a", 2), frozen.a);
(function () { "use strict"; try { const k = "a"; frozen[k] = 3; log("strict frozen no throw"); } catch (e) { log("strict frozen", e.constructor.name); } })();

const P = function () {};
P.prototype.m = 1;
const instance = new P();
log("proto store", store(P.prototype, "m", 2), instance.m, load(instance, "m"));

(function (first) {
  log("arguments", store(arguments, "0", "changed"), first, load(arguments, "0"));
})("orig");

const wrapper = new String("abc");
log("wrapper", load(wrapper, "1"), load(wrapper, "length"), store(wrapper, "1", "z"), wrapper[1]);

const arr = [1, 2, 3];
let sum = 0;
for (let i = 0; i < 2000; i++) { const v = arr[i % 6]; sum += v === undefined ? 100 : v; }
log("oob before", sum);
Array.prototype[4] = 40;
sum = 0;
for (let i = 0; i < 2000; i++) { const v = arr[i % 6]; sum += v === undefined ? 100 : v; }
log("oob after", sum);
delete Array.prototype[4];

const nullProto = Object.create(null);
nullProto.z = "nz";
for (let i = 0; i < 1000; i++) load(nullProto, "z");
log("null proto", load(nullProto, "z"), load(nullProto, "toString"));

const proxyProto = Object.create(new Proxy({}, { get: (t, k) => "trap:" + String(k) }));
for (let i = 0; i < 1000; i++) load(proxyProto, "q");
log("proxy chain", load(proxyProto, "q"));

const lone = String.fromCharCode(0xd800);
const loneObj = { [lone]: "lone" };
log("lone surrogate", load(loneObj, lone), load(loneObj, "�"));

console.log(out.join("\n"));
