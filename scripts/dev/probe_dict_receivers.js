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
  let last = [];
  for (let r = 0; r < n; r++) {
    last = [];
    for (let i = 0; i < objs.length; i++) last.push(String(read(objs[i])));
  }
  out.push(label + ' ' + last.join(','));
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
  e.x = 1; e['p' + i] = i; delete e.x;
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
// megamorphic sites: dictionary receivers of many prototypes
const protos = []; const mega = [];
for (let p = 0; p < 12; p++) {
  const proto = { ['m' + p]: p, enter: 'e' + p };
  protos.push(proto);
  for (let i = 0; i < 3; i++) { const o = Object.create(proto); o.a = p * 10 + i; o['k' + i] = 2; o.b = 1; delete o.a; mega.push(o); }
}
function megaAbsent(o) { return o.noScope; }
function megaInherited(o) { return o.enter; }
for (let w = 0; w < 3; w++) { run('mega-absent', mega, megaAbsent, 200); run('mega-inherited', mega, megaInherited, 200); }
mega[4].noScope = 'own4'; mega[4].enter = 'own-enter4';
run('mega-absent-own', mega.slice(3, 6), megaAbsent, 30);
run('mega-inherited-own', mega.slice(3, 6), megaInherited, 30);
protos[2].noScope = 'proto2';
run('mega-absent-proto', mega.slice(5, 9), megaAbsent, 30);
protos[2].enter = 'changed2';
run('mega-inherited-proto', mega.slice(5, 9), megaInherited, 30);
delete protos[2].noScope; delete mega[4].noScope;
run('mega-absent-deleted', mega.slice(3, 9), megaAbsent, 30);
Object.prototype.noScope = 'op';
run('mega-absent-object-proto', mega.slice(0, 4), megaAbsent, 30);
delete Object.prototype.noScope;
run('mega-absent-object-proto-deleted', mega.slice(0, 4), megaAbsent, 30);
Object.setPrototypeOf(mega[0], other);
run('mega-reproto', mega.slice(0, 3), megaInherited, 30);
run('mega-reproto-absent', mega.slice(0, 3), megaAbsent, 30);
console.log(out.join('\n'));
