// Map and Set keyed by objects while scavenges move them: lookups, deletes,
// re-insertion, iteration order, mixed key kinds and symbols.
var out = [];
var keys = []; for (var i = 0; i < 3000; i++) keys.push({ id: i });
var m = new Map(), s = new Set();
var sym = Symbol('k'); m.set(sym, 'symbol'); m.set('str', 1); m.set(42, 2);
for (var i = 0; i < keys.length; i++) { m.set(keys[i], i); if (i % 3 === 0) s.add(keys[i]); }
var junk; for (var r = 0; r < 20; r++) { junk = []; for (var j = 0; j < 20000; j++) junk.push({ j: j }); }
var hits = 0, sum = 0;
for (var i = 0; i < keys.length; i++) { if (m.has(keys[i])) hits++; sum += m.get(keys[i]); if (s.has(keys[i])) sum += 1; }
out.push(hits, sum, m.get(sym), m.get('str'), m.get(42), m.has({ id: 1 }));
for (var i = 0; i < keys.length; i += 2) { m.delete(keys[i]); s.delete(keys[i]); }
junk = []; for (var j = 0; j < 30000; j++) junk.push({ j: j });
var left = 0; for (var i = 0; i < keys.length; i++) if (m.has(keys[i])) left++;
for (var i = 0; i < 100; i += 2) m.set(keys[i], -i);
var order = []; m.forEach(function (v, k) { if (typeof k === 'object' && order.length < 6) order.push(k.id + ':' + v); });
out.push(m.size, s.size, left, order.join(','), m.get(keys[10]), m.get(keys[11]));
var fresh = new Map(); var objs = [];
for (var round = 0; round < 50; round++) {
  var o = { r: round }; objs.push(o); fresh.set(o, round);
  junk = []; for (var j = 0; j < 3000; j++) junk.push([j]);
  var ok = 0; for (var k = 0; k < objs.length; k++) if (fresh.get(objs[k]) === k) ok++;
  if (ok !== objs.length) out.push('LOST ' + round);
}
out.push(fresh.size);
console.log(out.join('\n'));
