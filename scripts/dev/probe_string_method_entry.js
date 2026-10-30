// String.prototype methods through their direct entry: slice, substring,
// substr, indexOf, includes, startsWith, padStart, repeat, trim, charAt,
// toUpperCase, concat and split/replace/match, with string and non-string
// receivers, coercing arguments (objects, booleans, null), empty and
// one-unit results (Latin-1 and wide), and hot loops.
var out = [];
var s = 'kitten sitting', w = 'aЖb😀c', e = '';
function row(x) {
  return [x.slice(0, -1), x.slice(-1), x.slice(3, 3), x.slice(1, 2), x.substring(5, 1), x.substr(-3, 2), x.indexOf('t', 2),
    x.includes('it', 1), x.startsWith('k'), x.padStart(18, '*'), x.repeat(2).length, x.trim(), x.charAt(1), x.toUpperCase(),
    x.concat(1, null), x.split('t').length, x.replace('t', 'T'), String(x.match(/t+/))].map(String).join('|');
}
out.push(row(s)); out.push(row(w)); out.push(row(e));
var obj = { valueOf: function () { return 2; }, toString: function () { return '3'; } };
out.push([s.slice(obj), s.slice(true, null), s.substring(obj, '5'), s.charAt(obj), s.indexOf(obj), s.padStart(obj, obj)].join('|'));
out.push([String.prototype.slice.call(12345, 1, 3), String.prototype.toUpperCase.call(true), String.prototype.charAt.call({ toString: function () { return 'xyz'; } }, 2)].join('|'));
try { String.prototype.slice.call(null, 1); } catch (err) { out.push('null ' + err.constructor.name); }
var same = 0, hot = 0;
for (var i = 0; i < 30000; i++) { var t = s.slice(i % 14, (i % 14) + 1); if (t === s[i % 14]) same++; hot += s.slice(0, i % 15).length + w.slice(-1).length; }
out.push('hot ' + same + ' ' + hot);
console.log(out.join('\n'));
