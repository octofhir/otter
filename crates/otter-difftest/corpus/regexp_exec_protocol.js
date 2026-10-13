// RegExp.prototype.exec protocol edges on hot paths: numeric lastIndex
// forms, non-string subjects, legacy RegExp statics after many captures,
// results with capture counts around the inline buffer sizes, and
// start-anchored patterns over subjects that cannot begin a match.
const results = [];

function execFrom(re, subject, lastIndex) {
  re.lastIndex = lastIndex;
  const m = re.exec(subject);
  return m === null ? [null, re.lastIndex] : [m.index, m[0], re.lastIndex];
}

const sticky = /b/g;
const forms = [0, 1, 1.5, -0, -3, NaN, Infinity, 2 ** 53, '2', { valueOf: () => 3 }];
let acc = [];
for (let round = 0; round < 300; round++) {
  for (const form of forms) acc.push(JSON.stringify(execFrom(sticky, 'abcabcb', form)));
}
results.push(acc.slice(0, forms.length).join('|'), acc.length);

// Non-string subjects coerce.
const digits = /\d+/;
let coerced = 0;
for (let i = 0; i < 2000; i++) {
  const m = digits.exec(i * 7);
  coerced += m ? m[0].length : 0;
  if (/^1/.exec(i)) coerced++;
}
results.push(coerced);

// Legacy statics after a match with many captures.
function groups(n) {
  let source = '';
  for (let i = 0; i < n; i++) source += '(' + String.fromCharCode(97 + (i % 26)) + ')';
  return new RegExp(source);
}
for (const n of [7, 8, 9, 15, 16, 17, 20]) {
  const re = groups(n);
  let subject = '';
  for (let i = 0; i < n; i++) subject += String.fromCharCode(97 + (i % 26));
  let total = 0;
  let last;
  for (let i = 0; i < 500; i++) {
    last = re.exec('_' + subject + '_');
    total += last.length + last.index;
  }
  results.push([n, total, last.slice(1).join(''), RegExp.$1, RegExp.$9, RegExp.lastParen, RegExp.lastMatch.length]);
}

// Start-anchored patterns: most subjects cannot begin a match.
const anchored = [/^ba/, /^b/i, /^(?:x|y)z/, /^\u{1F600}/u, /^a*/];
const subjects = ['pyvpx', 'bax', 'Bz', 'yz', 'xz', '', '\u{1F600}!', 'aaab', 'b'];
let anchoredSum = 0;
for (let round = 0; round < 400; round++) {
  for (const re of anchored) {
    for (const s of subjects) {
      const m = re.exec(s);
      anchoredSum += m === null ? 1 : 10 + m[0].length;
    }
  }
}
results.push(anchoredSum);

console.log(JSON.stringify(results));
