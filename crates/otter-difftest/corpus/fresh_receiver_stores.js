// Inlined constructors store young values into the receivers they just
// allocated while a growing old generation drives full collections: every
// stored edge must survive whatever collection runs between constructions.
function Pair(car, cdr) { this.car = car; this.cdr = cdr; }
function Leaf(n) { this.n = n; this.tag = "l" + (n % 7); }
function cons(a, b) { return new Pair(a, b); }
function build(n) {
  let list = null;
  for (let i = 0; i < n; i++) list = cons(new Leaf(i), list);
  return list;
}
function sum(list) {
  let s = 0;
  for (let p = list; p !== null; p = p.cdr) s += p.car.n + p.car.tag.length;
  return s;
}
const keep = [];
let total = 0;
for (let round = 0; round < 60; round++) {
  const list = build(4000 + round * 50);
  total += sum(list);
  if (round % 3 === 0) keep.push(list); // old-generation growth
  if (keep.length > 12) keep.shift();
}
for (const list of keep) total += sum(list);
console.log(total);
