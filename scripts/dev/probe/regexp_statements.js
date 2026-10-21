// Per-statement wall time of the Octane regexp fixture's run blocks: wraps
// every `sum += EXPR;` inside a runBlock with a clock read, keyed by EXPR with
// its loop index stripped, minus the measured cost of an empty clock pair.
// Run under node and otter and compare.
const fs = require('fs');
const path = require('path');
let src = fs.readFileSync(path.join(__dirname, '../../../benchmarks/fixtures/fixed-work/regexp.js'), 'utf8');
const keys = [];
const times = (globalThis.__stmtTimes = []);
src = src.replace(/(\n {6})sum \+= ([^\n]*);(?=\n)/g, (whole, indent, expr) => {
  const key = expr.replace(/\[i[^\]]*\]/g, '[]');
  let id = keys.indexOf(key);
  if (id < 0) { id = keys.length; keys.push(key); times.push(0); }
  return `${indent}{ const __t = performance.now(); sum += ${expr}; globalThis.__stmtTimes[${id}] += performance.now() - __t; }`;
});
let overhead = 0;
for (let i = 0; i < 200000; i++) { const t = performance.now(); overhead += performance.now() - t; }
overhead /= 200000;
(0, eval)(src);
const counts = {};
const rows = keys.map((k, i) => [k, times[i]]);
rows.sort((a, b) => b[1] - a[1]);
console.log(JSON.stringify({ overheadNs: Math.round(overhead * 1e6), rows: rows.slice(0, 60).map(([k, v]) => [k, Math.round(v)]) }));
