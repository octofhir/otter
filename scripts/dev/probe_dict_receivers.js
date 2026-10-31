// Loads at one site over many dictionary-mode receivers sharing a
// prototype: absent keys, keys inherited as data or accessors, own data and
// own accessors (the re-export pattern), then every mutation that must
// change the answer (own add/delete, prototype add/delete, accessor swap,
// setPrototypeOf, prototype freezing).
'use strict';
const out = [];
function mk(proto, i) {
  const o = Object.create(proto);
  o.a = i; o.b = 1; o['k' + i] = 2; delete o.b;
  return o;
}
function run(label, objs, read, n) {
  let acc = '';
  for (let r = 0; r < n; r++) {
    for (let i = 0; i < objs.length; i++) acc = String(read(objs[i]));
  }
  out.push(label + ' ' + acc);
}
const base = { enter: null, get g() { return 'pg' + this.a; } };
const objs = []; for (let i = 0; i < 40; i++) objs.push(mk(base, i));
function absent(o) { return o.noScope; }
function inherited(o) { return o.enter; }
function getter(o) { return o.g; }
function own(o) { return o.a; }
for (let w = 0; w < 3; w++) {
  run('absent', objs, absent, 300);
  run('inherited', objs, inherited, 300);
  run('getter', objs, getter, 300);
  run('own', objs, own, 300);
}
// an own key appears on one receiver: the receiver's table answers first
objs[7].noScope = 'own7'; objs[7].enter = 'own-enter7'; Object.defineProperty(objs[7], 'g', { get() { return 'og7'; }, configurable: true });
const seven = [objs[7]];
run('absent-own', seven, absent, 50);
run('inherited-own', seven, inherited, 50);
run('getter-own', seven, getter, 50);
run('absent-other', objs.slice(0, 7), absent, 50);
delete objs[7].noScope; delete objs[7].enter; delete objs[7].g;
run('absent-deleted', seven, absent, 50);
run('inherited-deleted', seven, inherited, 50);
run('getter-deleted', seven, getter, 50);
// the prototype changes
base.noScope = 'proto-added';
run('absent-proto-added', objs, absent, 50);
delete base.noScope;
run('absent-proto-deleted', objs, absent, 50);
base.enter = 'proto-enter';
run('inherited-changed', objs, inherited, 50);
Object.defineProperty(base, 'g', { get() { return 'pg2-' + this.a; }, configurable: true });
run('getter-swapped', objs, getter, 50);
Object.prototype.noScope = 'object-proto';
run('absent-object-proto', objs, absent, 50);
delete Object.prototype.noScope;
run('absent-object-proto-deleted', objs, absent, 50);
// receivers move to another prototype
const other = { enter: 'other-enter', noScope: 'other-noScope', get g() { return 'other-g'; } };
for (let i = 0; i < 5; i++) Object.setPrototypeOf(objs[i], other);
run('absent-reproto', objs.slice(0, 10), absent, 50);
run('inherited-reproto', objs.slice(0, 10), inherited, 50);
run('getter-reproto', objs.slice(0, 10), getter, 50);
// re-export pattern: own accessors on dictionary-mode exports objects
const exportsList = [];
for (let i = 0; i < 30; i++) {
  const e = {}; Object.defineProperty(e, '__esModule', { value: true });
  Object.defineProperty(e, 'isScope', { enumerable: true, configurable: true, get() { return 'scope' + i; } });
  e.x = 1; delete e.x; e['p' + i] = i;
  exportsList.push(e);
}
function reexport(e) { return e.isScope; }
run('reexport', exportsList, reexport, 300);
Object.defineProperty(exportsList[3], 'isScope', { value: 'data3' });
run('reexport-data', exportsList.slice(0, 5), reexport, 50);
// null-prototype dictionaries
const nulls = []; for (let i = 0; i < 20; i++) { const o = Object.create(null); o.a = i; o.b = 2; delete o.b; nulls.push(o); }
run('null-absent', nulls, absent, 200);
nulls[2].noScope = 'n2';
run('null-absent-own', nulls.slice(0, 4), absent, 20);
console.log(out.join('\n'));
