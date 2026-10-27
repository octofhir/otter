function update(a, b, c) { return a + c; }
let keep;
const N = 2e6;
for (let i = 0; i < 1e5; i++) keep = update.bind(undefined, i, 2);
let t0 = process.hrtime.bigint();
for (let i = 0; i < N; i++) keep = update.bind(undefined, i, 2);
console.log('bind', (Number(process.hrtime.bigint() - t0) / N).toFixed(1), 'ns');
t0 = process.hrtime.bigint();
let s = 0;
for (let i = 0; i < N; i++) s += keep(i);
console.log('call bound', (Number(process.hrtime.bigint() - t0) / N).toFixed(1), 'ns', s > 0);
