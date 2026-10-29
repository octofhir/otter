// Prototypes that keyed mixins leave in dictionary mode (hundreds of
// methods, accessors), read through megamorphic call sites, then mutated.
function Path(kind) { this.kind = kind; }
var methods = {};
for (var i = 0; i < 300; i++) methods['m' + i] = (function (k) { return function () { return this.kind * 1000 + k; }; })(i);
Object.assign(Path.prototype, methods);
Object.defineProperty(Path.prototype, 'twice', { get: function () { return this.kind * 2; }, configurable: true });
var shapes = [];
for (var i = 0; i < 12; i++) { var p = new Path(i); p['extra' + i] = i; shapes.push(p); }
var sum = 0;
for (var round = 0; round < 3000; round++) {
  var p = shapes[round % shapes.length];
  sum += p.m3() + p.m150() + p.m299() + p.twice;
}
Path.prototype.m3 = function () { return -1; };
delete Path.prototype.m150;
var after = 0, missing = 0;
for (var round = 0; round < 3000; round++) {
  var p = shapes[round % shapes.length];
  after += p.m3() + p.m299();
  if (typeof p.m150 !== 'function') missing++;
}
Object.prototype.m150 = function () { return 7; };
var inherited = shapes[0].m150();
delete Object.prototype.m150;
console.log(sum, after, missing, inherited, shapes[5] instanceof Path, Object.keys(Path.prototype).length);
