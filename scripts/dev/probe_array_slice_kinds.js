// Array.prototype.slice over every dense representation (objects, small
// integers, doubles, mixed), holes inside and outside the range, every bound
// form, frozen and subclassed sources, and results that are mutated after.
'use strict';
const out = [];
const show = (r) => JSON.stringify(r, (k, v) => (v === undefined ? '<u>' : Number.isNaN(v) ? '<nan>' : v)) + (Object.getPrototypeOf(r) === Array.prototype ? '' : ':' + r.constructor.name);
const sources = {
  objects: [{ a: 1 }, 'b', null, true, 2.5, { c: [3] }, 7, 'x'],
  ints: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
  doubles: [1.5, -0, NaN, 3.25, Infinity, 6.5, 7.75],
  holeyOutside: (() => { const a = [1.5, 2.5, 3.5, 4.5, 5.5]; a[9] = 1; return a; })(),
  holeyInside: [1, , 3, , 5, 6],
  holeyTagged: ['a', , 'c', {}, , 'f'],
  empty: [],
  big: Array.from({ length: 300 }, (_, i) => (i % 3 ? i : { i })),
};
const bounds = [[], [0], [2], [-2], [1, 4], [-3, -1], [5, 2], [0, 100], [undefined, 3], [1.7, 3.2], [NaN, 2], ['1', '3']];
function sliceAll(a) {
  const row = [];
  for (const b of bounds) row.push(show(a.slice(...b)));
  return row.join(' ');
}
for (let round = 0; round < 3; round++) {
  for (const [name, a] of Object.entries(sources)) {
    let last;
    for (let i = 0; i < 40; i++) last = sliceAll(a);
    if (round === 2) out.push(name + ' ' + (last.length > 400 ? last.length + ':' + last.slice(0, 200) : last));
  }
}
// results are independent arrays of their own kind
const ints = sources.ints.slice(2, 5); ints.push(1.5, 'str'); ints[0] = {};
out.push('mutated ' + show(ints) + ' source ' + show(sources.ints));
const dbl = sources.doubles.slice(); dbl[1] = 'now-tagged';
out.push('dbl ' + show(dbl) + ' ' + Object.is(sources.doubles[1], -0));
// frozen and subclassed sources, arguments objects
out.push('frozen ' + show(Object.freeze([1, 2, 3]).slice(1)));
class Sub extends Array {}
out.push('sub ' + show(Sub.from([1, 2, 3]).slice(1)));
(function () { out.push('args ' + show(Array.prototype.slice.call(arguments, 1))); })(1, 2, 3);
// a slice of a large array survives the allocations of many more slices
const keep = [];
for (let i = 0; i < 2000; i++) keep.push(sources.big.slice(i % 50, (i % 50) + 100));
out.push('keep ' + keep.length + ' ' + keep[1999].length + ' ' + show(keep[1999][1]) + ' ' + show(keep[777][3]));
console.log(out.join('\n'));
