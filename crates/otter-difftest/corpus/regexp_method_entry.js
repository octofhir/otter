// Hot `re.exec(s)` and `re.test(s)` calls of the original RegExp methods:
// plain, global and sticky lastIndex updates, a lastIndex that needs a
// coercion or cannot be written, legacy statics, the `d` flag, subjects
// that are not strings or are ropes, and the method replaced mid-loop.
const results = [];

function execAll(re, inputs, n) {
  let sum = 0;
  for (let i = 0; i < n; i++) {
    const m = re.exec(inputs[i % inputs.length]);
    if (m) sum += m.index * 7 + m[0].length + (m[1] ? m[1].length : 0) + m.length;
    else sum += 3;
  }
  return sum;
}
function testAll(re, inputs, n) {
  let hits = 0;
  for (let i = 0; i < n; i++) if (re.test(inputs[i % inputs.length])) hits += 1 + (re.lastIndex & 3);
  return hits;
}
const words = ['banana', 'apple', 'bandana', 'cherry', 'ba', 'xbax', '', 'aBa'];
results.push(execAll(/^ba(n*)/, words, 40000), testAll(/an/, words, 40000));
// Global and sticky regexps carry lastIndex across calls.
const g = /a(n?)/g;
results.push(execAll(g, words, 40000), g.lastIndex);
const y = /a/y;
results.push(testAll(y, words, 40000), y.lastIndex);
// A lastIndex needing a coercion runs it through the full method.
let coerced = 0;
const viaObject = /b/g;
for (let i = 0; i < 20000; i++) {
  viaObject.lastIndex = i % 5000 === 0 ? { valueOf() { coerced++; return 1; } } : i & 1;
  if (viaObject.exec('abcabc')) coerced += viaObject.lastIndex;
}
results.push(coerced);
// A lastIndex that cannot be written throws from a global exec.
const frozen = /q/g;
let threw = 0;
for (let i = 0; i < 20000; i++) {
  if (i === 15000) Object.defineProperty(frozen, 'lastIndex', { writable: false, value: 0 });
  try {
    frozen.exec('qq');
  } catch (e) {
    threw += e instanceof TypeError ? 1 : 100;
  }
}
results.push(threw);
// Legacy statics follow the last successful match.
let legacy = '';
for (let i = 0; i < 20000; i++) {
  if (/(\d+)-(\w)/.exec('id ' + (i % 97) + '-' + 'xyz'[i % 3])) legacy = RegExp.$1 + RegExp.$2;
}
results.push(legacy, RegExp.lastMatch, RegExp.leftContext.length);
// Match indices, a numeric subject and a rope subject.
const withIndices = /(o+)/d;
let spans = 0;
for (let i = 0; i < 20000; i++) {
  const m = withIndices.exec('fo' + 'o'.repeat(i & 7) + 'bar');
  spans += m.indices[1][1] - m.indices[0][0];
}
let numeric = 0;
for (let i = 0; i < 20000; i++) if (/^1\d*5$/.test(i)) numeric++;
let rope = 0;
for (let i = 0; i < 20000; i++) {
  const s = 'left-' + i + '-right';
  const m = /-(\d+)-/.exec(s);
  rope += m ? Number(m[1]) % 13 : 0;
}
results.push(spans, numeric, rope);
// The prototype method replaced while the loop is hot.
const original = RegExp.prototype.exec;
let replaced = 0;
const target = /z/;
for (let i = 0; i < 30000; i++) {
  if (i === 20000) {
    RegExp.prototype.exec = function (s) {
      replaced++;
      return original.call(this, s);
    };
  }
  if (target.exec('zz')) replaced += 2;
  if (target.test('zz')) replaced += 3;
}
RegExp.prototype.exec = original;
results.push(replaced);
console.log(JSON.stringify(results));
