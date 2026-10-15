// Sticky searches try exactly lastIndex: a miss there never scans ahead,
// whatever a later start would cost.
const results = [];
const catastrophic = /x(a+)+b/y;
const subject = 'zx' + 'a'.repeat(32);
for (let i = 0; i < 200; i++) {
  catastrophic.lastIndex = 0;
  if (catastrophic.test(subject) || catastrophic.lastIndex !== 0) throw Error('sticky scanned ahead');
}
catastrophic.lastIndex = 0;
results.push(catastrophic.exec(subject), catastrophic.lastIndex);

const word = /\w+/y;
const text = 'ab cd  ef';
let sum = 0;
for (let i = 0; i < 300; i++) {
  word.lastIndex = i % 9;
  const m = word.exec(text);
  sum += m ? m.index * 10 + m[0].length : -1;
  sum += word.lastIndex;
}
results.push(sum);

const globalSticky = /a|b/gy;
results.push('aabxab'.replace(globalSticky, '<$&>'), globalSticky.lastIndex);
results.push('aabxab'.match(globalSticky).join(','));
results.push(JSON.stringify('a,b,,c'.split(/,/y)), JSON.stringify('xaxbx'.split(/x/y, 2)));

const anchoredStart = /^a/y;
anchoredStart.lastIndex = 1;
results.push(anchoredStart.test('aa'), anchoredStart.lastIndex);
const multilineStart = /^a/my;
multilineStart.lastIndex = 2;
results.push(multilineStart.exec('b\nab').index, multilineStart.lastIndex);
const behind = /(?<=a)b/y;
behind.lastIndex = 1;
results.push(behind.exec('ab')[0], behind.lastIndex);
const empty = /(?:)/y;
empty.lastIndex = 3;
results.push(empty.exec('abc').index, empty.lastIndex);
empty.lastIndex = 4;
results.push(empty.exec('abc'), empty.lastIndex);

for (const flags of ['uy', 'gu', 'guy', 'vy', 'y', 'g']) {
  for (const source of ['.', '(?:)', '\\u{1F600}', '[\\s\\S]', '^.', 'x']) {
    let re;
    try { re = new RegExp(source, flags); } catch (e) { results.push(flags + source + ':syntax'); continue; }
    re.lastIndex = 1;
    const m = re.exec('\u{1F600}x');
    results.push(flags + source + ':' + (m && [m.index, m[0].length].join('/')) + ':' + re.lastIndex);
  }
}

let coercions = 0;
const coerced = /b/y;
coerced.lastIndex = { valueOf() { coercions++; return 1; } };
results.push(coerced.exec('ab').index, coercions, coerced.lastIndex);

const statics = /(c)/y;
statics.lastIndex = 0;
statics.exec('cd');
statics.lastIndex = 0;
statics.exec('dc');
results.push(RegExp.$1, RegExp.lastMatch);

const frozen = /a/y;
Object.defineProperty(frozen, 'lastIndex', { value: 1, writable: false });
let throws = 0;
try { frozen.exec('ba'); } catch (e) { throws += e instanceof TypeError ? 1 : 0; }
try { frozen.exec('bb'); } catch (e) { throws += e instanceof TypeError ? 10 : 0; }
results.push(throws);

const original = RegExp.prototype.exec;
let execCalls = 0;
RegExp.prototype.exec = function (s) { execCalls++; return original.call(this, s); };
const patched = /b/y;
patched.lastIndex = 0;
results.push(patched.test('ab'), patched.lastIndex, execCalls);
results.push(JSON.stringify('b,b'.split(/,/y)), execCalls > 1);
RegExp.prototype.exec = original;

console.log(results.join('|'));
