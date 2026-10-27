// Cost of building an `arguments` object that escapes into a call.
function sink(x) { return x.length; }
function sloppy(a, b) { return sink(arguments); }
function strict(a, b) { 'use strict'; return sink(arguments); }
const apply = Reflect.apply;
function target(a, b) { return a; }
function forward(a) { return apply(target, this, arguments); }
function time(label, fn) {
  let acc = 0;
  for (let i = 0; i < 200000; i++) acc += fn(i, 2);
  const n = 3000000, t = process.hrtime.bigint();
  for (let i = 0; i < n; i++) acc += fn(i, 2);
  const ns = Number(process.hrtime.bigint() - t) / n;
  console.log(label.padEnd(12), ns.toFixed(1), 'ns', acc > 0 ? '' : '!');
}
time('sloppy', sloppy);
time('strict', strict);
time('forward', forward);
