// RegExp exec/test protocol probes for the leaf entry: lastIndex, legacy
// statics, overridden exec, sticky/global tracking, rope subjects, groups.
function hot(f) { let r; for (let i = 0; i < 20000; i++) r = f(i); return r; }
const g = /a(b)?/g;
console.log(hot(() => { g.lastIndex = 0; const m = g.exec('xab ab a'); return [m.index, m[0], m[1], g.lastIndex, m.input.length].join(','); }));
console.log(hot(() => { g.lastIndex = 3; return g.test('xab ab a') + ',' + g.lastIndex; }));
console.log(hot(() => { g.lastIndex = 100; return g.test('xab') + ',' + g.lastIndex; }));
const re = /b(c)/;
console.log(hot(() => { const m = re.exec('abcd'); return RegExp.$1 + RegExp.leftContext + '|' + RegExp.rightContext + '|' + RegExp.lastMatch; }));
const o = /x/; o.exec = () => null;
console.log(hot(() => o.test('x')));
const p = /x/; p.lastIndex = 1.5;
console.log(hot(() => p.exec('x').index + ',' + p.lastIndex));
const y = /b/y;
console.log(hot(() => { y.lastIndex = 1; const a = y.test('ab'); y.lastIndex = 0; return a + ',' + y.test('ab') + ',' + y.lastIndex; }));
let rope = 'ab'; for (let i = 0; i < 3; i++) rope = rope + rope;
console.log(hot(() => /b+/.exec(rope + 'c')[0]));
const n = /(?<w>\w)(\d)?/;
console.log(hot(() => { const m = n.exec('q'); return m.groups.w + ',' + m[2] + ',' + m.length; }));
const d = /a/d;
console.log(hot(() => d.exec('xa').indices[0].join('-')));
console.log(hot(() => /nomatch/.exec('zzz')));
console.log(hot((i) => { const m = /(\d+)-(\d+)/.exec('t ' + (i % 7) + '-42 q'); return m[1] + '+' + m[2] + '@' + m.index; }));
