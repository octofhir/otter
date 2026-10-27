// Typical regexp operations of web code, each timed in isolation.
const url = 'http://www.example.com:8080/path/to/page.html?query=value&x=1#frag';
const text = '  some text with   spaces and words like ba and lhv-h here  ';
// Hoisted like the regexp benchmark's patterns: literal evaluation is timed
// on its own.
const re1 = /(((\w+):\/\/)([^\/:]*)(:(\d+))?)?([^#?]*)(\?([^#]*))?(#(.*))?/;
const re18 = /^\s+|\s+$/g;
const re14 = /\s+/;
const re3 = /\bQBZPbageby_cynprubyqre\b/;
const re9 = /(^|\s)lhv\-h(\s|$)/;
const re11 = /\./g;
const re7 = /(\d*)(\D*)/g;
const re13 = /\?[\w\W]*(sevraqvq|punaaryvq|tebhcvq)=([^\&\?#]*)/i;
const cases = [
  ['url-parse', () => re1.exec(url)],
  ['trim-replace', () => text.replace(re18, '')],
  ['split-ws', () => text.split(re14)],
  ['word-test', () => re3.test(text)],
  ['class-test', () => re9.test(text)],
  ['replace-dot', () => url.replace(re11, '_')],
  ['digits-g', () => { re7.lastIndex = 0; let m, n = 0; while ((m = re7.exec('12ab34cd56')) && m[0]) n++; return n; }],
  ['match-i', () => re13.exec('?a=1&punaaryvq=42&b')],
  ['literal', () => /\bba\b/.test('x')],
];
for (const [label, fn] of cases) {
  for (let i = 0; i < 20000; i++) fn();
  const n = 300000, t = process.hrtime.bigint();
  for (let i = 0; i < n; i++) fn();
  console.log(label.padEnd(14), (Number(process.hrtime.bigint() - t) / n).toFixed(1), 'ns');
}
