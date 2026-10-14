// Borrow subject units only between collecting work; callbacks still observe
// the original match list, subject, substitution order and thrown identity.
const results = [];
const retained = [];
function subject(seed) {
  return ('prefix-' + seed + '-').repeat(5) + 'a-b-a-\ud800-中-café-tail';
}
function allocate(seed, input) {
  const values = [];
  for (let i = 0; i < 8; i++) values.push({ input, text: ('keep-' + seed).repeat(30), i });
  retained[seed & 7] = values;
}
function warmReplace(seed) {
  const input = subject(seed);
  const template = seed & 1 ? '中<$&:$1:$2>' : '[$&:$1:$2]';
  return input.replace(/(a)(b)?/g, template).length;
}
let total = 0;
for (let i = 0; i < 2000; i++) total += warmReplace(i & 7);
results.push(total);
const input = subject(17);
for (const template of ['$$', '$&', "$`#$'", '$1/$2/$12/$01/$0', '$<part>/$<missing>', '中$1ÿ']) {
  results.push(input.replace(/(?<part>a)(b)?/g, template));
}
results.push('a-b'.replace(/(?<same>a)|(?<same>b)/g, '<$<same>>'));
results.push('\ud800a\udfff'.replace(/(.)/g, '中$1'));
results.push('😀'.replace(/(?:)/gu, '|'));
results.push(input.slice(3, -2).replace(/a/g, 'ÿ'));

let source = subject(23);
const original = source;
const pattern = /(?<part>a)|(b)/g;
let callbackCalls = 0;
let coercions = 0;
let unexpectedExec = 0;
const trace = [];
const replaced = source.replace(pattern, function (match, a, b, offset, text, groups) {
  callbackCalls++;
  allocate(callbackCalls, text);
  if (callbackCalls === 1) {
    source = null;
    pattern.exec = function () { unexpectedExec++; return null; };
  }
  pattern.lastIndex = 77;
  const inner = ('x-' + callbackCalls + '-x').replace(/x/g, '[$&]');
  trace.push([match, a, b, offset, text === original, groups.part, inner]);
  return {
    toString() {
      coercions++;
      allocate(10 + coercions, text);
      return (coercions & 1 ? '中' : 'ÿ') + match;
    }
  };
});
results.push(replaced, trace, callbackCalls, coercions, unexpectedExec, pattern.lastIndex,
  retained[(10 + coercions) & 7][0].input === original);

const marker = { tag: 'replacement-throw' };
let throws = 0;
let callsBeforeThrow = 0;
try {
  subject(31).replace(/a/g, function () {
    callsBeforeThrow++;
    allocate(30 + callsBeforeThrow, original);
    if (callsBeforeThrow === 2) throw marker;
    return 'first';
  });
} catch (error) { if (error === marker) throws++; }
let throwingCoercions = 0;
try {
  subject(37).replace(/a/g, function () {
    return { toString() { throwingCoercions++; allocate(40, original); throw marker; } };
  });
} catch (error) { if (error === marker) throws++; }
results.push(throws, callsBeforeThrow, throwingCoercions);

let templateCoercions = 0;
const templateObject = {
  toString() { templateCoercions++; allocate(50, original); return '<$&>'; }
};
results.push(subject(41).replace(/a/g, templateObject), templateCoercions);
let misses = 0;
const absent = subject(43);
results.push(absent.replace(/NEVER/g, function () { misses++; return 'bad'; }) === absent, misses);
console.log(JSON.stringify(results));
