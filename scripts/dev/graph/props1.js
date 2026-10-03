// Named loads through feedback programs in optimized code: methods found on
// prototypes (with prototype validity), loads from the pinned intrinsic
// prototypes of strings, arrays and functions, mixes of own and inherited
// slots, a prototype edited mid-run (validity cells must reject old code)
// and a dictionary-mode prototype.
'use strict';
class A { constructor(x) { this.x = x; } get2() { return this.x * 2; } }
class B extends A { constructor(x) { super(x); this.y = 1; } get3() { return this.x * 3; } }
function protoLoad(o) { return o.get2; }
function mixed(o) { return o.x + (o.tag || 0); }
function intrinsic(s, a, f) { return [s.charAt, a.push, f.call].map((m) => typeof m).join(); }
function viaDict(o) { return o.dictMethod; }
const objs = [new A(1), new B(2), new A(3)];
const proto = { dictMethod: 1 };
for (let i = 0; i < 40; i++) proto['k' + i] = i;
for (let i = 0; i < 40; i += 2) delete proto['k' + i];
const d1 = Object.create(proto), d2 = Object.create(proto);
let out = [];
for (let r = 0; r < 3000; r++) {
  let t = 0;
  for (const o of objs) t += protoLoad(o).call(o) + mixed(o);
  if (r === 1500) {
    A.prototype.get2 = function () { return -this.x; };
    A.prototype.tag = 100;
  }
  out = [t, intrinsic('s', [], Math.max), viaDict(d1), viaDict(d2)];
  if (r === 2000) proto.dictMethod = 7;
}
console.log(out.join(' '));
