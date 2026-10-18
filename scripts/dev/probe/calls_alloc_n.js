// Calls that allocate, closures and exceptions: a GC-stress probe for the
// interpreter call path. Run with n small under OTTER_GC_STRESS.
"use strict";
function pair(a, b) { return { a, b, s: "" + a + b }; }
function add(o) { return o.a + o.b; }
function thrower(i) { if (i % 7 === 0) throw new Error("x" + i); return [i, i + 1]; }
function main(n) {
  let s = 0;
  for (let i = 0; i < n; i++) {
    const o = pair(i, 1);
    s += add(o);
    try { s += thrower(i)[1]; } catch (e) { s += e.message.length; }
    const f = (x) => x + o.a;
    s += f(i);
  }
  return s;
}
console.log(main(+(globalThis.process?.argv?.at(-1) ?? 0) || 0));
