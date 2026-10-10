// Hot loads and stores that reach an inherited getter or setter: class and
// defineProperty accessors, assignment results, receivers that change shape
// inside the setter, accessors redefined, shadowed or removed mid-loop,
// getters that throw, and receivers of several classes sharing one accessor.
class Vec {
  constructor(x) { this._x = x; this.sets = 0; }
  get x() { return this._x; }
  set x(v) { this._x = v; this.sets++; }
  get double() { return this._x * 2; }
}
class Tagged extends Vec {
  constructor(x) { super(x); this.tag = "t"; }
}

function Legacy(v) { this._v = v; }
Object.defineProperty(Legacy.prototype, "v", {
  configurable: true,
  get: function () { return this._v; },
});
Object.defineProperty(Legacy.prototype, "v", {
  configurable: true,
  set: function (value) { if (value === undefined) value = 0; this._v = value | 0; },
});

function sumLoop(objects, n) {
  let total = 0;
  for (let i = 0; i < n; i++) {
    const o = objects[i % objects.length];
    o.x = o.x + 1;
    total += o.double;
  }
  return total;
}

const out = [];
const vecs = [new Vec(1), new Vec(2), new Vec(3)];
out.push(sumLoop(vecs, 9000), vecs.map((v) => v.sets).join("/"));
out.push(sumLoop([new Vec(1), new Tagged(2)], 9000));

// The assignment's value is the assigned value, not the setter's return.
const legacy = new Legacy(5);
let assigned = 0;
for (let i = 0; i < 5000; i++) assigned += (legacy.v = i + 0.5);
out.push(assigned, legacy.v);

// A setter that adds a property changes the receiver's shape mid-loop.
function Growing() { this.n = 0; }
Object.defineProperty(Growing.prototype, "n2", {
  get() { return this.n; },
  set(v) { this.n = v; if (v === 2500) this.extra = true; },
});
const g = new Growing();
for (let i = 0; i < 5000; i++) g.n2 = g.n2 + 1;
out.push(g.n, "extra" in g);

// Redefinition, an own shadowing data property, and removal mid-loop.
function Box(v) { this._v = v; }
Object.defineProperty(Box.prototype, "v", { configurable: true, get() { return this._v; } });
const boxes = [new Box(1), new Box(2)];
let seen = 0;
for (let i = 0; i < 12000; i++) {
  if (i === 3000) {
    Object.defineProperty(Box.prototype, "v", { configurable: true, get() { return -this._v; } });
  }
  if (i === 6000) Object.defineProperty(boxes[1], "v", { value: 100, writable: true });
  if (i === 9000) delete Box.prototype.v;
  const v = boxes[i & 1].v;
  seen += v === undefined ? 1000 : v;
}
out.push(seen);

// A throwing getter inside a hot loop.
function Fragile(k) { this.k = k; }
Object.defineProperty(Fragile.prototype, "val", {
  get() { if (this.k % 997 === 0) throw new Error("bad " + this.k); return this.k; },
});
let caught = 0, ok = 0;
for (let i = 1; i < 6000; i++) {
  try { ok += new Fragile(i).val; } catch (e) { caught++; }
}
out.push(ok, caught);
console.log(out.join(","));
