// Prototype changes of many objects of one layout: __proto__ stores and
// Object.setPrototypeOf to the same and different prototypes, then property
// reads, inherited methods, additions, deletions, enumeration order, freezing
// and null prototypes, so objects sharing a cached replayed shape stay apart.
var out = [];
function A() {} A.prototype.kind = function () { return 'A' + this.x; };
function B() {} B.prototype.kind = function () { return 'B' + this.y; };
var made = [];
for (var i = 0; i < 300; i++) {
  var raw = { type: 'n', x: i, y: -i };
  if (i % 3 === 0) raw.__proto__ = A.prototype;
  else if (i % 3 === 1) Object.setPrototypeOf(raw, B.prototype);
  else raw.__proto__ = null;
  made.push(raw);
}
out.push(made.slice(0, 6).map(function (o) { return typeof o.kind === 'function' ? o.kind() : 'none:' + Object.keys(o).join('|'); }).join(','));
made[0].extra = 1; made[3].x = 'changed'; delete made[6].y;
out.push([made[0].kind(), made[3].kind(), made[6].kind(), 'y' in made[6], Object.keys(made[0]).join('|'), Object.keys(made[6]).join('|')].join(','));
Object.freeze(made[9]);
try { made[9].__proto__ = B.prototype; out.push('frozen moved ' + made[9].kind()); } catch (e) { out.push('frozen ' + e.constructor.name); }
out.push(Object.getPrototypeOf(made[9]) === A.prototype);
var back = made[12]; back.__proto__ = Object.prototype; out.push(String(back.kind) + ' ' + back.toString());
var swap = made[15]; swap.__proto__ = B.prototype; out.push(swap.kind() + ' ' + (made[18].__proto__ === A.prototype));
A.prototype.later = 7; out.push(made[21].later + ' ' + made[22].later);
var dict = { a: 1 }; for (var k = 0; k < 40; k++) dict['p' + k] = k; delete dict.p1; dict.__proto__ = A.prototype; out.push(dict.kind() + ' ' + Object.keys(dict).length);
var sum = 0;
for (var r = 0; r < 20000; r++) { var o = { type: 'n', x: r, y: 0 }; o.__proto__ = (r & 1) ? A.prototype : B.prototype; sum += o.kind().length; }
out.push('hot ' + sum);
console.log(out.join('\n'));
