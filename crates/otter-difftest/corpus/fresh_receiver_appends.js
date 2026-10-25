// Constructors appending properties to the object they just allocated, on hot
// paths: plain appends, a receiver passed to a call between its stores, one
// made non-extensible or frozen mid-construction, one that becomes another
// object's prototype mid-construction, and a setter installed on the
// prototype for an appended key while the constructor is hot.
const results = [];

function Vec(x, y, z) {
  this.x = x;
  this.y = y;
  this.z = z;
}
Vec.prototype.sum = function () {
  return this.x + this.y + this.z;
};

let total = 0;
let keep = [];
for (let i = 0; i < 60000; i++) {
  const v = new Vec(i, i * 0.5, -i);
  total = (total + v.sum()) % 1000003;
  if (i % 97 === 0) keep.push(v);
  if (keep.length > 500) keep = [];
}
results.push(total, keep.length, Object.keys(new Vec(1, 2, 3)).join());

// The receiver reaches a call before its remaining stores.
const seen = [];
function note(o, i) {
  if (i % 1000 === 0) seen.push(o);
  if (i % 3 === 0) Object.preventExtensions(o);
  if (i % 5 === 0) Object.freeze(o);
}
function Leaky(i) {
  this.a = i;
  note(this, i);
  this.b = i + 1;
  this.c = i + 2;
}
let leaky = 0;
for (let i = 0; i < 30000; i++) {
  const o = new Leaky(i);
  leaky = (leaky + o.a + (o.b === undefined ? 1000 : o.b) + (o.c === undefined ? 7 : o.c)) % 1000003;
}
results.push(leaky, seen.length, Object.isExtensible(seen[1]), Object.keys(seen[2]).join());

// The receiver becomes a prototype between its stores.
const children = [];
function Parent(i) {
  this.a = i;
  if (i % 7 === 0) children.push(Object.create(this));
  this.b = i * 2;
}
let parents = 0;
for (let i = 0; i < 30000; i++) {
  const p = new Parent(i);
  parents = (parents + p.a + p.b) % 1000003;
}
let inherited = 0;
for (const c of children) inherited = (inherited + c.a + c.b) % 1000003;
results.push(parents, children.length, inherited);

// A setter for an appended key appears on the prototype while hot.
function Point(x, y) {
  this.x = x;
  this.y = y;
}
let points = 0;
for (let i = 0; i < 40000; i++) {
  if (i === 20000) {
    Object.defineProperty(Point.prototype, 'y', {
      set(v) {
        this._y = v * 3;
      },
      get() {
        return this._y;
      },
      configurable: true,
    });
  }
  const p = new Point(i, i + 1);
  points = (points + p.x + p.y + (Object.prototype.hasOwnProperty.call(p, 'y') ? 1 : 0)) % 1000003;
}
results.push(points);

// The same, with the counts the recompiled loop must keep: once the setter
// exists no receiver is born holding its own field for the key.
function Pair(a, b) {
  this.a = a;
  this.b = b;
}
let ownB = 0;
let wrongB = 0;
for (let i = 0; i < 40000; i++) {
  if (i === 20000) {
    Object.defineProperty(Pair.prototype, 'b', {
      set(v) {
        this._b = v * 3;
      },
      get() {
        return this._b;
      },
      configurable: true,
    });
  }
  const p = new Pair(i, i + 1);
  if (i >= 20000) {
    if (Object.prototype.hasOwnProperty.call(p, 'b')) ownB++;
    if (p.b !== (i + 1) * 3) wrongB++;
  }
}
results.push(ownB, wrongB);

console.log(JSON.stringify(results));
