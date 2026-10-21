// Hot searches of a capture-heavy pattern and of an unfiltered empty-matching
// alternation: the shapes the native matcher tier targets.
const url = /(((\w+):\/\/)([^\/:]*)(:(\d+))?)?([^#?]*)(\?([^#]*))?(#(.*))?/;
const trim = /^\s*|\s*$/g;
const inputs = ['http://example.com:8080/path/to/x?query=1&b=2#frag', 'ftp://host/file', 'relative/path?x'];
let sum = 0;
const t0 = Date.now();
for (let i = 0; i < 300000; i++) {
  const m = url.exec(inputs[i % 3]);
  sum += m[0].length + (m[4] ? m[4].length : 0);
}
const t1 = Date.now();
const padded = '   some words here and there with trailing   ';
for (let i = 0; i < 200000; i++) sum += padded.replace(trim, '').length;
const t2 = Date.now();
console.log(sum, 'url', t1 - t0, 'ms trim', t2 - t1, 'ms');
