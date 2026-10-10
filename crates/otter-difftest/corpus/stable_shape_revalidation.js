// Shape proofs carried across calls: a receiver proven before a call keeps
// its shape afterwards unless some object left that shape — which the
// callee does here at chosen iterations by adding, deleting and redefining
// properties, swapping the prototype, freezing, or reshaping a different
// object of the same shape.
class Point {
  constructor(x, y) { this.x = x; this.y = y; }
  sum() { return this.x + this.y; }
  scale(k) { return this.x * k; }
}
class Other {
  sum() { return -1; }
  scale() { return -2; }
}
let hook = null;
function touch(p, i) { if (hook) hook(p, i); return i & 3; }

function run(p, n) {
  let total = 0;
  for (let i = 0; i < n; i++) {
    total += p.x;
    total += touch(p, i);
    total += p.y + p.sum() + p.scale(2);
    total += touch(p, i);
    total += (p.x | 0) - (p.y | 0);
  }
  return total;
}

const out = [];
const cases = [
  null,
  (p, i) => { if (i === 3000) p.extra = 1; },
  (p, i) => { if (i === 3000) delete p.y; },
  (p, i) => { if (i === 3000) Object.defineProperty(p, "x", { get() { return 100; } }); },
  (p, i) => { if (i === 3000) Object.setPrototypeOf(p, Other.prototype); },
  (p, i) => { if (i === 3000) Object.freeze(p); },
  (p, i) => { if (i === 3000) Object.preventExtensions(p); },
  (p, i) => { if (i === 3000) { const q = new Point(1, 2); q.z = 3; } },
  (p, i) => { if (i === 3000) Point.prototype.sum = function () { return 7; }; },
];
for (const c of cases) {
  hook = c;
  out.push(run(new Point(3, 4), 6000));
}
hook = null;
out.push(run(new Point(5, 6), 6000));
console.log(out.join(","));

// Each prototype gives the case an independent stable final shape. Readers
// warm before their callee changes either that receiver or a peer.
const readers = [
  function append(o, effect) { const x = o.x; const m = effect(o); return [x, m, o.x, o.y, o.sum()]; },
  function remove(o, effect) { const x = o.x; const m = effect(o); return [x, m, o.x, o.y, o.sum()]; },
  function accessor(o, effect) { const x = o.x; const m = effect(o); return [x, m, o.x, o.y, o.sum()]; },
  function prototype(o, effect) { const x = o.x; const m = effect(o); return [x, m, o.x, o.y, o.sum()]; },
  function seal(o, effect) { const x = o.x; const m = effect(o); return [x, m, o.x, o.y, o.sum()]; },
  function freeze(o, effect) { const x = o.x; const m = effect(o); return [x, m, o.x, o.y, o.sum()]; },
  function prevent(o, effect) { const x = o.x; const m = effect(o); return [x, m, o.x, o.y, o.sum()]; },
  function peer(o, effect) { const x = o.x; const m = effect(o); return [x, m, o.x, o.y, o.sum()]; },
  function values(o, effect) { const x = o.x; const m = effect(o); return [x, m, o.x, o.y, o.sum()]; },
];
function exercise(change, index) {
  class Cell {
    constructor(x, y) { this.x = x; this.y = y; }
    sum() { return this.x + this.y; }
  }
  const cell = new Cell(11, 13);
  const peer = new Cell(17, 19);
  let mutate = false;
  function effect(object) {
    if (mutate) { mutate = false; change(object, peer); }
    return 1;
  }
  // Two distinct targets retain the call boundary in every reader.
  function effectA(object) { return effect(object); }
  function effectB(object) { return effect(object); }
  const read = readers[index];
  for (let i = 0; i < 6000; i++) read(cell, i & 1 ? effectA : effectB);
  mutate = true;
  return read(cell, effectA);
}
const transitions = [
  (o) => { o.extra = 23; },
  (o) => { delete o.x; },
  (o) => { Object.defineProperty(o, "y", { get() { return 29; } }); },
  (o) => { Object.setPrototypeOf(o, { sum() { return 31; } }); },
  (o) => { Object.seal(o); },
  (o) => { Object.freeze(o); },
  (o) => { Object.preventExtensions(o); },
  (o, peer) => { peer.extra = 37; o.y = 41; },
  (o) => { o.x = 43; o.y = 47; },
];
console.log(JSON.stringify(transitions.map(exercise)));
