// hasOwnProperty through compiled method sites and `hop.call` sites: plain,
// dictionary and inherited keys, built, rope and never-interned strings,
// symbols and numbers, arrays, String wrappers, arguments, proxies, getters,
// nullish `this`, and a replaced Object.prototype.hasOwnProperty.
var hop = Object.prototype.hasOwnProperty;
var out = [];
function viaMethod(o, k) { return o.hasOwnProperty(k); }
function viaCall(o, k) { return hop.call(o, k); }
function viaToString(o) { return Object.prototype.toString.call(o); }
var dict = {}; for (var i = 0; i < 40; i++) dict['k' + i] = i; delete dict.k3;
var sym = Symbol('s');
var objs = [
  { a: 1 }, Object.create({ a: 1 }), dict, [1, 2], new String('ab'), (function () { return arguments; })(1, 2),
  new Proxy({ a: 1 }, { getOwnPropertyDescriptor: function (t, k) { out.push('trap ' + String(k)); return Object.getOwnPropertyDescriptor(t, k); } }),
  { get a() { out.push('getter'); return 1; } }, Object.create(null), function f() {},
];
objs[8].a = 1;
var keys = ['a', 'k5', 'k3', '0', 'length', 'zz' + 'q', 'a'.concat(''), ('x' + 'yz').slice(0, 1), sym, 1, '1'];
objs[0][sym] = 1;
function sweep() {
  var row = [];
  for (var i = 0; i < objs.length; i++) for (var j = 0; j < keys.length; j++) {
    if (i === 8) { row.push(viaCall(objs[i], keys[j])); continue; }
    row.push(viaMethod(objs[i], keys[j]), viaCall(objs[i], keys[j]));
  }
  return row.map(Number).join('');
}
var first = sweep();
for (var r = 0; r < 400; r++) if (sweep() !== first) out.push('unstable ' + r);
out.push(first);
for (var t = 0; t < 2; t++) {
  try { viaCall(null, 'a'); } catch (e) { out.push('null ' + e.constructor.name); }
  try { viaCall(undefined, 'a'); } catch (e) { out.push('undefined ' + e.constructor.name); }
}
out.push('primitive ' + viaCall('abc', '1') + viaCall('abc', 'length') + viaCall(5, 'x'));
var trapCount = out.length;
out.splice(0, out.length, out.filter(function (l) { return l.indexOf('trap') !== 0 && l !== 'getter'; }).join('\n'), 'traps/getters ' + (trapCount));
Object.prototype.hasOwnProperty = function (k) { return 'replaced ' + k; };
var plain = { a: 1 };
for (var q = 0; q < 2000; q++) viaMethod(plain, 'a');
out.push(viaMethod(plain, 'a'));
Object.prototype.hasOwnProperty = hop;
out.push(viaMethod(plain, 'a') + ' ' + viaToString(plain) + ' ' + viaToString([]) + ' ' + viaToString(new Error('x')));
console.log(out.join('\n'));
