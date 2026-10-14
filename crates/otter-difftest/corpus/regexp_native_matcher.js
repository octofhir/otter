// Hot regexps searched often enough to run as machine code: captures that
// reset inside quantified groups, give-back and lazy repeats, alternations,
// anchors in both line modes, word boundaries, backreferences, case-folded
// literals and classes, required literals, and every scan strategy, over
// one-byte and two-byte subjects, through exec, test, global replace, match
// and split.
const results = [];

const subjects = [
  'http://example.com:8080/path/to/x?query=1&b=2#frag',
  'ftp://host/file',
  'relative/path?x',
  '  some words here and there  ',
  'aaaaaaaaaaaaaaaaaaaaaaaab',
  'Foo BAR baz\nqux QUUX\r\nend',
  'été ÉTÉ ÿŸ café',
  'xĀy z w 一二 σΣς',
  'abcabcabcabd abab_ab-ab',
  '',
];

const patterns = [
  /(((\w+):\/\/)([^\/:]*)(:(\d+))?)?([^#?]*)(\?([^#]*))?(#(.*))?/,
  /^\s*|\s*$/g,
  /\b\w+\b/g,
  /(a|b)*c/,
  /((a)|b)+/,
  /(a?)*?b/,
  /a+?b/,
  /[ab]{2,}/g,
  /(\w)\1/g,
  /(a)|\1b/,
  /(ab)\1/i,
  /QUX|quux/gi,
  /^\w+$/gm,
  /\w+$/gm,
  /\B\w\B/g,
  /été/gi,
  /[à-ÿ]+/gi,
  /[Ā-一]+/g,
  /[^Ā-一\s]+/g,
  /σ/gi,
  /path\?\w*/,
  /(?:ab|a)+d/g,
  /a{2,4}?/g,
  /.*b/,
  /.+?z/s,
  /x*/g,
  /\d+(?:\.\d+)?/g,
  /[^]/g,
];

function runOne(re, s) {
  re.lastIndex = 0;
  const m = re.exec(s);
  let out = m ? m.index + ':' + m.map((g) => (g === undefined ? '-' : g)).join('|') : 'null';
  re.lastIndex = 0;
  out += '/' + re.test(s);
  if (re.global) {
    out += '/' + s.replace(re, '<$&>').length;
    const all = s.match(re);
    out += '/' + (all ? all.length + ':' + all.join(',') : 'none');
  }
  out += '/' + s.split(re).length;
  return out;
}

// Enough rounds that every pattern compiles; the digest folds every round.
for (let i = 0; i < patterns.length; i++) {
  let digest = 0;
  let last = '';
  for (let round = 0; round < 12; round++) {
    for (const s of subjects) {
      last = runOne(patterns[i], s);
      for (let k = 0; k < last.length; k++) digest = (digest * 31 + last.charCodeAt(k)) | 0;
    }
  }
  results.push(i + '=' + digest + ' ' + last);
}

// A deep give-back stack and undo log in one anchored search.
const deep = 'a'.repeat(20000);
const deepRuns = [];
for (let i = 0; i < 10; i++) {
  deepRuns.push(/^(a|b)*c/.test(deep), /^((a)|b)*$/.exec(deep)[2], deep.replace(/^a*?$/, 'E').length);
}
results.push(deepRuns.join());

// A two-byte subject long enough that the literal scan, not the matcher,
// decides.
const wideText = ('一 lorem ipsum NQ_VQ dolor ').repeat(200) + 'tail';
let hits = 0;
for (let i = 0; i < 50; i++) hits += wideText.replace(/NQ_VQ/g, '').length + (wideText.match(/um NQ/g) || []).length;
results.push(hits);

// Flatten fresh ropes through direct and coerced native receivers. Coercion
// runs once even when flattening moves the string and its search operand.
let coercions = 0;
let ropeDigest = 0;
for (let i = 0; i < 64; i++) {
  const rope = 'x'.repeat(80) + 'Ā' + i;
  const receiver = { toString() { coercions++; return 'x'.repeat(80) + 'Ā' + i; } };
  ropeDigest += rope.charCodeAt(80);
  ropeDigest += String.prototype.charCodeAt.call(receiver, 80);
  ropeDigest += rope.includes('x'.repeat(40) + 'Ā') ? 1 : 0;
  ropeDigest += String.prototype.includes.call(receiver, 'x'.repeat(40) + 'Ā') ? 1 : 0;
}
results.push([ropeDigest, coercions]);

const includesOrder = [];
const includesSearch = {
  get [Symbol.match]() { includesOrder.push('match'); return false; },
  toString() { includesOrder.push('string'); return 'Ā'; }
};
results.push(('x'.repeat(80) + 'Ā').includes(includesSearch), includesOrder);
const nonRegExp = /x/;
nonRegExp[Symbol.match] = false;
results.push('/x/'.includes(nonRegExp));
const marker = {};
try {
  ''.includes({ toString() { throw marker; } });
} catch (error) {
  results.push(error === marker);
}

console.log(JSON.stringify(results));
