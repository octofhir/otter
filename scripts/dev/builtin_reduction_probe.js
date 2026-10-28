// Builtin reductions against every operand kind; diff the output with node.
const values = [0, -0, 1, -1, 3, 1.5, -2.5, NaN, Infinity, -Infinity, 2 ** 53, 2 ** 53 - 1,
  -(2 ** 53 - 1), 2 ** 52 + 0.5, 1e300, 2 ** 31, -(2 ** 31), 4.5e15, 'x', '3', '', true, false,
  null, undefined, {}, [], [1], Symbol.iterator, 10n, new Proxy([], {}), new Proxy({}, {}),
  function () {}, new Number(3)];
const names = values.map((v) => {
  try { return typeof v === 'symbol' ? 'sym' : Object.is(v, -0) ? '-0' : String(v); } catch { return '?'; }
});
function isPair(a, b) { return Object.is(a, b); }
function tests(v) {
  return [Number.isNaN(v), Number.isFinite(v), Number.isInteger(v), Number.isSafeInteger(v),
    Array.isArray(v)];
}
const lines = [];
for (let round = 0; round < 3000; round++) {
  for (let i = 0; i < values.length; i++) {
    const r = tests(values[i]);
    if (round === 2999) lines.push(`${names[i]}: ${r.join(',')}`);
    for (let j = 0; j < values.length; j++) {
      const same = isPair(values[i], values[j]);
      if (round === 2999 && same) lines.push(`is ${names[i]} ${names[j]}`);
    }
  }
}
const ints = [];
for (let i = -5; i < 200000; i++) {
  if (Object.is(i, 3) || Object.is(i, -0) || Object.is(i, 0)) ints.push(i);
  if (Number.isInteger(i) !== true || Number.isNaN(i)) ints.push('bad' + i);
}
lines.push('ints ' + ints.join(','));
function mm(a, b, c) { return [Math.max(a, b, c), Math.min(a, b, c), Math.max(a), Math.min(c, a)]; }
const mmv = [[1, 2, 3], [-0, 0, -1], [NaN, 1, 2], [3, '7', 2], [Infinity, -Infinity, 0], [0, -0, 0]];
let acc = '';
for (let round = 0; round < 50000; round++) {
  const t = mmv[round % mmv.length];
  const r = mm(t[0], t[1], t[2]);
  if (round >= 49994) acc += r.map((x) => (Object.is(x, -0) ? '-0' : String(x))).join(' ') + ';';
}
lines.push('mm ' + acc);
// Replacing the builtin mid-run must take effect.
let hits = 0;
function viaIs(i) { return Object.is(i, 7) ? 1 : 0; }
for (let i = 0; i < 300000; i++) {
  if (i === 200000) Object.is = (a, b) => a < b;
  hits += viaIs(i);
}
lines.push('replaced ' + hits);
let arr = 0;
function viaIsArray(v) { return Array.isArray(v) ? 1 : 0; }
const mixed = [[], {}, new Proxy([1], {}), 'a'];
for (let i = 0; i < 200000; i++) arr += viaIsArray(mixed[i < 150000 ? i & 1 : i & 3]);
lines.push('isArray ' + arr);
console.log(lines.join('\n'));
// String, Array and keyed-collection registry methods on every receiver kind.
const strs = ['hello world', 'ÿé€😀x', '', 'a'.repeat(40) + 'b', 'x'.repeat(3) + 'y'.repeat(5)];
const ropeParts = ['abc', 'def'];
const sl = [];
for (let r = 0; r < 20000; r++) {
  const s = strs[r % strs.length];
  const rope = ropeParts[0] + ropeParts[1] + r;
  const i = r % 13 - 2;
  const a = [s.charAt(i), s.codePointAt(i & 7), s.charCodeAt(i & 3), s.indexOf('o'), s.includes('€'),
    s.startsWith('he'), s.endsWith('b'), rope.charAt(i), rope.codePointAt(1),
    String.fromCharCode(97 + (r % 300)), String.fromCharCode(-1 - r % 5)];
  if (r >= 19990) sl.push(a.map((x) => (typeof x === 'string' ? JSON.stringify(x) : String(x))).join(' '));
}
lines.push('str ' + sl.join(';'));
const arrs = [[1, 2, 3, NaN, -0, 'x'], [1.5, 2.5, 3.5], [, 1, 2], ['a', 'b', 'c'], [{}, null, undefined]];
const al = [];
const needles = [2, NaN, 0, -0, 'b', 3.5, undefined, 1];
for (let r = 0; r < 20000; r++) {
  const arr = arrs[r % arrs.length];
  const n = needles[r % needles.length];
  const res = [arr.indexOf(n), arr.includes(n)];
  if (r >= 19980) al.push(res.join(','));
}
lines.push('arr ' + al.join(';'));
const m = new Map([[1, 'one'], ['k', 'kay'], [NaN, 'nan']]);
const st = new Set([1, 'k', -0]);
let ml = '';
for (let r = 0; r < 20000; r++) {
  const key = [1, 'k', NaN, 0, 2][r % 5];
  const v = m.get(key);
  const h = m.has(key) + st.has(key);
  if (r === 7000) m.set(2, 'two');
  if (r >= 19995) ml += `${v}/${h};`;
}
lines.push('map ' + ml);
console.log(lines.slice(-3).join('\n'));
