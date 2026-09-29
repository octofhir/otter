// Closure allocation smoke: plain functions, arrows capturing `this` and
// `new.target`, capture-free functions, generators/async (runtime path),
// per-instance state on sibling closures, and GC pressure.
function Counter(n) {
  this.n = n;
  this.inc = () => ++this.n;
  this.nt = () => new.target === Counter;
}
function plain(k) {
  return function (x) { return x + k; };
}
function free() { return function () { return 7; }; }
function* gen(n) { for (let i = 0; i < n; i++) yield i; }
let sum = 0;
for (let round = 0; round < 3000; round++) {
  const c = new Counter(round);
  c.inc(); c.inc();
  sum += c.n + (c.nt() ? 1 : 0);
  const f = plain(round);
  const g = plain(round + 1);
  f.tag = round;
  sum += f(1) + g(2) + (g.tag === undefined ? 1 : 0) + free()();
  const arrow = () => this === undefined ? 3 : 4;
  sum += arrow();
  for (const v of gen(3)) sum += v;
  if (round % 97 === 0) {
    const junk = [];
    for (let j = 0; j < 2000; j++) junk.push({ j, s: 'x' + j });
    sum += junk.length;
  }
}
const siblings = [plain(1), plain(2)];
Object.preventExtensions(siblings[0]);
console.log(sum, Object.isExtensible(siblings[0]), Object.isExtensible(siblings[1]),
  plain(1).name, plain(1).length, typeof free(), (async () => 1)() instanceof Promise);
