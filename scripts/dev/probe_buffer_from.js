// Buffer.from(string) per-call cost, long enough to sample.
let c = 0;
const t = performance.now();
for (let i = 0; i < 30_000_000; i++) c += Buffer.from('hello world').length;
console.log(((performance.now() - t) * 1e6 / 30_000_000).toFixed(0), 'ns/op', c);
