// Array.prototype.join over plain arrays: Latin-1, wide and rope strings,
// holes, undefined and null, numbers, objects with toString, custom and
// wide separators, empty arrays, inherited indices, and hot loops.
var out = [];
var rope = ''; for (var i = 0; i < 30; i++) rope += 'r' + i;
var cases = [
  ['a', 'b', 'c'], ['a', , 'c'], ['x', undefined, null, 'y'], ['Ж', 'b'], [rope, 'z'], [1, 'a', 2.5],
  [{ toString: function () { return 'T'; } }, 's'], [], [''], ['', ''], [[1, 2], 'q'],
];
var seps = [undefined, '', '-', ', ', '—', 0, null];
for (var c = 0; c < cases.length; c++) for (var s = 0; s < seps.length; s++) out.push(JSON.stringify(cases[c].join(seps[s])));
var holey = ['a', , 'c'];
Array.prototype[1] = 'P'; out.push(holey.join('+')); delete Array.prototype[1];
var parts = []; for (var k = 0; k < 2000; k++) parts.push('w' + (k % 10));
var hot = 0;
for (var r = 0; r < 3000; r++) hot += parts.slice(0, r % 50).join(' ').length + ['a', 'b', r & 1 ? 'c' : 'é'].join('').length;
out.push('hot ' + hot);
console.log(out.join('\n'));
