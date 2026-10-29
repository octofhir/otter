// Named loads from dictionary-mode receivers: keyed-built option bags read
// through constant names, with deletes, accessors and absent keys mixed in.
function build(n) {
  var o = {};
  for (var i = 0; i < n; i++) o['k' + i] = i;
  o.ecma = 2015; o.locations = true;
  return o;
}
var a = build(20), b = build(30);
Object.defineProperty(b, 'getterKey', { get: function () { return 7; }, enumerable: true, configurable: true });
function read(o) { return o.ecma + (o.locations ? 1 : 0) + o.k3 + (o.k19 | 0); }
function readG(o) { return o.getterKey; }
var N = +(process.argv[2] || 300000), sum = 0;
for (var i = 0; i < N; i++) {
  var o = (i & 1) ? a : b;
  sum += read(o);
  if (i === N >> 1) { delete a.k3; a.k3 = 100; delete b.k19; }
  if (i === (N >> 1) + 7) b.k19 = -5;
}
var g = 0;
for (var j = 0; j < 1000; j++) g += readG(b) + (readG(a) === undefined ? 1 : 0);
console.log(sum, g);
