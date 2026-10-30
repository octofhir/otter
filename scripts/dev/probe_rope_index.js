// Indexed reads of strings built by concatenation (ropes), slices of them,
// wide and Latin-1 halves, surrogates, out-of-range and non-integer indices,
// reads before and after other uses, in hot compiled loops and under
// allocation pressure.
var out = [];
function build(n) { var s = ''; for (var i = 0; i < n; i++) s += String.fromCharCode(97 + (i % 26)) + (i % 7); return s; }
function at(s, i) { return s[i]; }
var rope = build(500);
var wide = 'ab' + 'ЖЗ' + build(20) + '😀';
var parts = [];
for (var i = 0; i < rope.length; i += 37) parts.push(at(rope, i));
out.push(parts.join(''));
out.push([at(wide, 2), at(wide, 3), at(wide, wide.length - 1), at(wide, wide.length - 2).charCodeAt(0)].join(','));
out.push([at(rope, -1), at(rope, rope.length), at(rope, 1.5), at(rope, '3'), at(rope, NaN)].map(String).join(','));
var sliced = rope.slice(100, 400) + 'XYZ';
out.push(at(sliced, 0) + at(sliced, 299) + at(sliced, 300) + at(sliced, 302) + sliced.length);
var hot = 0, garbage = [];
for (var k = 0; k < 40000; k++) {
  var r = (k % 1000 === 0) ? build(50 + (k % 9)) : rope;
  var c = at(r, k % r.length);
  hot += c.charCodeAt(0);
  if (k % 50 === 0) garbage.push({ k: k, s: r + k });
}
out.push('hot ' + hot + ' ' + garbage.length);
out.push(rope.length + ' ' + rope.indexOf('z5') + ' ' + rope.slice(-5));
console.log(out.join('\n'));
