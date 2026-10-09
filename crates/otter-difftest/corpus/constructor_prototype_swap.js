// Hot constructors and instanceof across prototype replacement.
function P(a, b) { this.a = a; this.b = b; }
function make(n) { return new P(n, n + 1); }
function test(o) { return o instanceof P; }
let out = [];
let sum = 0;
for (let i = 0; i < 20000; i++) { const o = make(i); sum += o.a + (test(o) ? 1 : 0); }
out.push(sum);
const old = P.prototype;
P.prototype = { tag: 'new' };
let hits = 0, olds = 0;
for (let i = 0; i < 20000; i++) {
  const o = make(i);
  if (Object.getPrototypeOf(o) === P.prototype) hits++;
  if (o instanceof P) olds++;
}
out.push(hits, olds);
P.prototype = old;
let back = 0;
for (let i = 0; i < 20000; i++) { const o = make(i); if (Object.getPrototypeOf(o) === old && test(o)) back++; }
out.push(back);
P.prototype = 5;
let thrown = 0;
for (let i = 0; i < 2000; i++) {
  const o = make(i);
  if (Object.getPrototypeOf(o) === Object.prototype) thrown++;
  try { test(o); } catch (e) { if (e instanceof TypeError) thrown++; }
}
out.push(thrown);
P.prototype = old;
Object.defineProperty(P, Symbol.hasInstance, { value: () => true });
let custom = 0;
for (let i = 0; i < 2000; i++) if (test({})) custom++;
out.push(custom);
console.log(JSON.stringify(out));
