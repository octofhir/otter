// instanceof: class chains, primitives, null prototypes, @@hasInstance
// overrides, bound and non-callable targets, a prototype replaced mid-run.
class A {}
class B extends A {}
class C {}
function F() {}
const odd = { [Symbol.hasInstance](v) { return typeof v === "number" && v % 2 === 1; } };
const bound = A.bind(null);
const values = [new A(), new B(), new C(), new F(), 1, "s", null, undefined, Object.create(null), [], () => 1];
const targets = [A, B, C, F, Object, Array, Function, bound];
function count(n) {
  let hits = 0;
  for (let i = 0; i < n; i++) {
    const v = values[i % values.length];
    const t = targets[i % targets.length];
    if (v instanceof t) hits++;
    if (v instanceof A) hits += 2;
    if ((i & 3) === 0 && i instanceof odd) hits += 4;
  }
  return hits;
}
let total = 0;
for (let r = 0; r < 40; r++) total += count(5000);
F.prototype = Object.create(A.prototype);
total += count(5000);
let errors = 0;
for (let i = 0; i < 3000; i++) {
  try { if ({} instanceof (i === 2999 ? 5 : A)) total++; } catch (e) { errors += e instanceof TypeError ? 1 : 100; }
}
console.log(total, errors);
