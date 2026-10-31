// ArraySpeciesCreate of plain arrays across every mutation that changes it:
// %Array.prototype%.constructor stored, redefined, deleted and restored, and
// %Array%[@@species] redefined and deleted, read by slice, splice, concat,
// map, filter, flat and flatMap from hot functions.
'use strict';
const out = [];
const ops = {
  slice: (a) => a.slice(1),
  splice: (a) => a.slice().splice(0, 2),
  concat: (a) => a.concat([9]),
  map: (a) => a.map((x) => x + 1),
  filter: (a) => a.filter((x) => x > 1),
  flat: (a) => [a, a].flat(),
  flatMap: (a) => a.flatMap((x) => [x]),
};
function describe(r) {
  if (r === undefined || r === null) return String(r);
  const proto = Object.getPrototypeOf(r);
  const tag = proto === Array.prototype ? 'Array' : proto === Mine.prototype ? 'Mine' : proto === Object.prototype ? 'Object' : typeof r;
  return tag + ':' + (r.length !== undefined ? r.length : '?') + (r.mark ? ':' + r.mark : '');
}
function phase(label) {
  const row = [];
  for (const [name, op] of Object.entries(ops)) {
    let last;
    for (let i = 0; i < 300; i++) {
      try { last = describe(op([1, 2, 3, 4])); } catch (e) { last = 'throw:' + e.constructor.name; }
    }
    row.push(name + '=' + last);
  }
  out.push(label + ' ' + row.join(' '));
}
class Mine extends Array {}
phase('default');
function Fake(n) { const o = { length: n, mark: 'fake' }; return o; }
const original = Array.prototype.constructor;
Array.prototype.constructor = Mine;
phase('ctor-stored');
Array.prototype.constructor = original;
phase('ctor-restored');
Object.defineProperty(Array.prototype, 'constructor', { value: { [Symbol.species]: Fake }, configurable: true, writable: true });
phase('ctor-object-species');
Object.defineProperty(Array.prototype, 'constructor', { value: original, configurable: true, writable: true, enumerable: false });
phase('ctor-redefined-back');
delete Array.prototype.constructor;
phase('ctor-deleted');
Array.prototype.constructor = original;
phase('ctor-readded');
const speciesDesc = Object.getOwnPropertyDescriptor(Array, Symbol.species);
Object.defineProperty(Array, Symbol.species, { get() { return Mine; }, configurable: true });
phase('species-mine');
Object.defineProperty(Array, Symbol.species, { value: undefined, configurable: true });
phase('species-undefined');
delete Array[Symbol.species];
phase('species-deleted');
Object.defineProperty(Array, Symbol.species, speciesDesc);
phase('species-restored');
const own = [1, 2, 3];
own.constructor = Mine;
out.push('instance-ctor ' + describe(own.slice(0)) + ' ' + describe(own.concat([1])));
phase('final');
console.log(out.join('\n'));
