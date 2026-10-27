// Writable.write of a short string chunk, long enough to sample.
const { Writable } = require('node:stream');
let c = 0;
const w = new Writable({ write(ch, e, cb) { c += ch.length; cb(); } });
const t = performance.now();
for (let i = 0; i < 12_000_000; i++) w.write('abc');
console.log(((performance.now() - t) * 1e6 / 12_000_000).toFixed(0), 'ns/op', c);
