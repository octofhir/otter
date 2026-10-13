// Hot code relying on shape facts it does not re-read, while those facts
// break: a callee widens a field's value summary, an object leaves a shape
// the caller proved stable, a prototype method changes under recursion, the
// code's own stores widen what it later loads, and baseline stores move
// objects off a shape optimized code relies on.
const results = [];

// A field whose values all had one shape, until a hot store that ran all
// along with that shape stores another, or null.
function Cell(value) {
  this.value = value;
}
Cell.prototype.get = function () {
  return this.value;
};
function Box(cell) {
  this.cell = cell;
}
function Other(value) {
  this.extra = 0;
  this.value = value * 3;
}
Other.prototype.get = function () {
  return this.value + this.extra;
};
function place(box, cell) {
  box.cell = cell;
}
function readCells(boxes, pool, n) {
  let sum = 0;
  for (let i = 0; i < n; i++) {
    const box = boxes[i & 15];
    const cell = box.cell;
    sum = (sum + (cell === null ? 7 : cell.get() + cell.value)) % 1000003;
    place(boxes[(i + 5) & 15], pool[i]);
  }
  return sum;
}
const boxes = [];
for (let i = 0; i < 16; i++) boxes.push(new Box(new Cell(i)));
const pool = [];
for (let i = 0; i < 60000; i++) pool.push(new Cell(i & 255));
pool[40000] = new Other(7);
pool[40001] = null;
results.push(readCells(boxes, pool, 60000), readCells(boxes, pool, 3000));

// An inner object proven to keep its shape across a call, until the hot
// keyed store in that call, which ran all along on other objects, moves it
// to another shape.
function Inner(a, b) {
  this.a = a;
  this.b = b;
}
function Outer(inner) {
  this.inner = inner;
}
function touch(target, i) {
  target['k' + (i % 3)] = i;
  return i & 7;
}
function walk(outers, targets, n) {
  let sum = 0;
  for (let i = 0; i < n; i++) {
    const outer = outers[i & 7];
    const inner = outer.inner;
    sum += inner.a;
    sum += touch(targets[i], i);
    sum += inner.b;
    sum %= 1000003;
  }
  return sum;
}
const outers = [];
for (let i = 0; i < 8; i++) outers.push(new Outer(new Inner(i, i * 2)));
const decoys = [];
for (let i = 0; i < 8; i++) decoys.push({ d: i });
const targets = [];
for (let i = 0; i < 60000; i++) targets.push(decoys[i & 7]);
targets[45000] = outers[0].inner;
targets[45001] = outers[3].inner;
results.push(walk(outers, targets, 60000), Object.keys(outers[0].inner).join(), Object.keys(outers[3].inner).join());

// A prototype method replaced from deep inside a recursion of the caller.
function Shape(n) {
  this.n = n;
}
Shape.prototype.area = function () {
  return this.n * 2;
};
function depth(shape, level, swapAt) {
  if (level === 0) return shape.area();
  if (level === swapAt) {
    Shape.prototype.area = function () {
      return this.n * 5 + 1;
    };
  }
  return shape.area() + depth(shape, level - 1, swapAt);
}
let areas = 0;
for (let i = 0; i < 3000; i++) areas = (areas + depth(new Shape(i % 11), 6, i === 2500 ? 3 : -1)) % 1000003;
results.push(areas);

// The loop's own stores widen the summary its loads rely on.
function Slot(v) {
  this.v = v;
}
function selfWiden(slots, n) {
  let sum = 0;
  for (let i = 0; i < n; i++) {
    const slot = slots[i & 7];
    const v = slot.v;
    sum = (sum + (typeof v === 'object' ? v.v : v)) % 1000003;
    if (i === n - 900) slot.v = new Slot(i);
    else if (typeof slot.v !== 'object') slot.v = (slot.v + 1) & 1023;
  }
  return sum;
}
const slots = [];
for (let i = 0; i < 8; i++) slots.push(new Slot(i));
results.push(selfWiden(slots, 50000), selfWiden(slots, 5000));

// Optimized code relies on a constructor shape staying stable while cold
// code keeps adding properties to fresh instances of it.
function Point(x, y) {
  this.x = x;
  this.y = y;
}
function norm(points) {
  let sum = 0;
  for (let i = 0; i < points.length; i++) sum += points[i].x * points[i].x + points[i].y;
  return sum % 1000003;
}
const points = [];
for (let i = 0; i < 64; i++) points.push(new Point(i, i + 1));
let norms = 0;
for (let round = 0; round < 400; round++) {
  norms = (norms + norm(points)) % 1000003;
  if (round === 300) {
    const late = new Point(1, 2);
    late.z = 3;
    points[5] = late;
  }
}
results.push(norms);

console.log(JSON.stringify(results));
