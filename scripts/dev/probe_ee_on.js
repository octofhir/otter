// EventEmitter `on` of several events on fresh emitters (node's per-request
// listener setup), one loop.
const EE = require('node:events');
const noop = () => {};
function setup() { const e = new EE(); e.on('finish', noop); e.on('close', noop); e.on('error', noop); e.on('drain', noop); e.on('finish', noop); return e; }
for (let w = 0; w < 3; w++) for (let i = 0; i < 2e5; i++) setup();
const n = Number(process.argv[2] || 2e6); const t = performance.now();
for (let i = 0; i < n; i++) setup();
console.log(((performance.now() - t) * 1e6 / n).toFixed(0), 'ns per emitter (5 on)');
