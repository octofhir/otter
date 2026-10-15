// Completed capture rows remain independent of matcher reuse and JS reentry.
const results = [];
const wide = /(a)(b)(c)(d)(e)(f)(g)(h)(i)(j)(k)(l)/d;
const narrow = /(?<x>a)/d;
const emptyGroups = /b/d;
const optional = /(?<x>a)(b)?/dg;
let checksum = 0;
for (let i = 0; i < 256; i++) {
  const large = wide.exec('abcdefghijkl');
  const small = narrow.exec('a');
  const none = emptyGroups.exec('b');
  optional.lastIndex = 0;
  const first = optional.exec('ab a');
  const last = optional.exec('ab a');
  checksum += large.length + small.length + none.length + first.length + last.length;
  if (i === 255) results.push(large[12], large.indices[12], small.length,
    small.groups.x, none.length, first.indices.groups.x,
    last[2] === undefined, last.indices[2] === undefined);
}
results.push(checksum);
const legacy = wide.exec('abcdefghijkl');
results.push(RegExp.$1, RegExp.$9, RegExp.lastParen, legacy[12]);

const pattern = /(?<left>a)(b)?|(?<left>c)/g;
const subject = 'ab-c-a-'.repeat(8) + '\ud800中💩\udfff';
checksum = 0;
for (let i = 0; i < 128; i++) {
  checksum += subject.replace(pattern, '<$&:$1:$2:$3:$<left>:$$>').length;
}
results.push(checksum);
const kept = [];
let calls = 0, coercions = 0;
const reentrant = /(?<left>a)(b)?|(?<left>c)/g;
const rendered = subject.replace(reentrant, function (match, a, b, c, position, original, groups) {
  calls++;
  if (original !== subject) throw new Error('replacement lost original subject');
  if (calls === 1) reentrant.exec = function () { throw new Error('replayed search'); };
  const inner = wide.exec('abcdefghijkl');
  const small = narrow.exec('a');
  const none = emptyGroups.exec('b');
  kept[calls & 7] = {inner, small, none, text: 'keep-' + calls + 'x'.repeat(60)};
  reentrant.lastIndex = 77;
  return {
    toString() {
      coercions++;
      const nested = 'ab a'.replace(/(?<n>a)(b)?/g, '$<n>:$2');
      kept[coercions & 7].nested = nested;
      return '[' + position + ':' + match + ':' + a + ':' + b + ':' + c + ':' + groups.left + ']';
    }
  };
});
results.push(calls, coercions, rendered, reentrant.lastIndex,
  kept[0].inner.length, kept[0].small.length, kept[0].none.length, kept[0].nested);

const equal = /(?<outer>(?<inner>a))/dg;
const snapshots = Array.from('aa'.matchAll(equal));
results.push(snapshots.map(m => [m.index, m.groups.outer, m.groups.inner,
  m.indices.groups.outer, m.indices.groups.inner,
  m.indices.groups.outer !== m.indices.groups.inner]));
const split = [];
for (let limit = 0; limit < 8; limit++) {
  split.push('ab-a-c'.split(/(?<x>a)(b)?/, limit));
  split.push('💩a💩'.split(/(?:)/u, limit));
}
results.push(split);
const marker = {marker: 817};
let throws = 0;
try {
  'ab a a'.replace(/(a)(b)?/g, function () {
    throws++;
    wide.exec('abcdefghijkl');
    if (throws === 2) throw marker;
    return 'x';
  });
} catch (error) { results.push(error === marker, throws); }
// Nullable loops keep progress marks in scratch, outside capture results.
for (const loop of [/(a?)*/dg, /(?<loop>a*)*/dg]) {
  let loopChecksum = 0;
  for (let i = 0; i < 96; i++) {
    loop.lastIndex = 0;
    const replacement = 'aa ba'.replace(loop, '<$&:$1:$2>');
    loop.lastIndex = 0;
    const records = Array.from('aa ba'.matchAll(loop));
    loopChecksum += replacement.length;
    if (i === 95) results.push(replacement, records.map(m => [m.index, m.length,
      m[0], m[1], m[2] === undefined, m.indices.length, m.indices[2] === undefined]));
  }
  results.push(loopChecksum);
}
console.log(JSON.stringify(results));
