// The same helper chain, once with its calls in tail position
// (strict mode lowers them to TailCall) and once not.
function sq(x) { return x * x; }
function viaTail(x) { 'use strict'; return sq(x + 1); }
function viaCall(x) { 'use strict'; const r = sq(x + 1); return r; }
function run(f) {
  let s = 0;
  for (let i = 0; i < 20000000; i++) s = (s + f(i & 1023)) | 0;
  return s;
}
const which = process.argv[2];
const t0 = Date.now();
const s = run(which === 'tail' ? viaTail : viaCall);
console.log(which, s, Date.now() - t0, 'ms');
