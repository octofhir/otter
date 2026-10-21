function put(o, k, v) { o[k] = v; }
function make(keys) { var o = {}; for (var i = 0; i < keys.length; i++) put(o, keys[i], i); return o; }
var out = [];
for (var n = 0; n < 3000; n++) {
  var o = make(["b", "2", "a", "1", "x" + (n % 7), "10", "constructor", "__proto__"]);
  if (n % 1000 === 999) out.push(Object.keys(o).join(","), JSON.stringify(o), typeof o.__proto__, Object.getPrototypeOf(o) === Object.prototype);
}
var frozen = Object.freeze({});
function putStrict(o, k, v) { "use strict"; o[k] = v; }
var threw = 0;
for (var n = 0; n < 2000; n++) { try { putStrict(frozen, "k" + (n % 3), 1); } catch (e) { threw++; } }
var setterHits = 0;
var proto = { set s(v) { setterHits++; } };
for (var n = 0; n < 2000; n++) { var c = Object.create(proto); put(c, "s", n); put(c, "t", n); if (c.hasOwnProperty("s")) throw new Error("setter bypassed"); }
var ro = Object.create(Object.defineProperty({}, "r", { value: 1, writable: false }));
for (var n = 0; n < 2000; n++) put(ro, "r", 5);
out.push(threw, setterHits, ro.r, Object.keys(ro).length);
console.log(out.join("\n"));
