// Per-call cost of cheap native builtins from optimized code: each loop runs
// one builtin per iteration on values whose fast paths need no allocation.
function timed(name, n, body) {
  body(1000);
  const t = performance.now();
  const r = body(n);
  const ns = ((performance.now() - t) * 1e6) / n;
  return [name, Math.round(ns * 10) / 10, r];
}
const arr = [1, 2, 3];
const obj = { x: 1 };
const str = 'hello world';
const re = /^zz/;
const rows = [
  timed('Array.isArray', 5e6, (n) => { let c = 0; for (let i = 0; i < n; i++) if (Array.isArray(arr)) c++; return c; }),
  timed('Number.isInteger', 5e6, (n) => { let c = 0; for (let i = 0; i < n; i++) if (Number.isInteger(i)) c++; return c; }),
  timed('hasOwnProperty', 5e6, (n) => { let c = 0; for (let i = 0; i < n; i++) if (obj.hasOwnProperty('x')) c++; return c; }),
  timed('str.indexOf', 5e6, (n) => { let c = 0; for (let i = 0; i < n; i++) c += str.indexOf('w'); return c; }),
  timed('str.charCodeAt', 5e6, (n) => { let c = 0; for (let i = 0; i < n; i++) c += str.charCodeAt(i & 7); return c; }),
  timed('Math.max', 5e6, (n) => { let c = 0; for (let i = 0; i < n; i++) c += Math.max(i, 3); return c; }),
  timed('re.test', 5e6, (n) => { let c = 0; for (let i = 0; i < n; i++) if (re.test(str)) c++; return c; }),
  timed('re.exec', 5e6, (n) => { let c = 0; for (let i = 0; i < n; i++) if (re.exec(str)) c++; return c; }),
  timed('str.slice', 5e6, (n) => { let c = 0; for (let i = 0; i < n; i++) c += str.slice(1, 4).length; return c; }),
];
for (const [name, ns] of rows) console.log(name.padEnd(18), String(ns).padStart(6), 'ns');
