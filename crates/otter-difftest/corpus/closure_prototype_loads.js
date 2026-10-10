// `F.prototype` loads in hot code read the closure's own slot: the default
// object allocated on first read, an assigned value of any type, a read-only
// slot, a function kind without the property, a user-created `prototype` on
// such a kind, and sites that also see ordinary objects or other callables.
function Point(x) { this.x = x; }
function Assigned() {}
Assigned.prototype = { tag: "assigned" };
function Primitive() {}
Primitive.prototype = 7;
function Frozen() {}
Object.defineProperty(Frozen, "prototype", { writable: false });
function* Gen() {}
const arrow = () => 0;
const method = { m() {} }.m;
const decorated = () => 1;
decorated.prototype = "own";
function WithBag() {}
WithBag.extra = 1;
class Klass {}
const bound = Point.bind(null);

function read(f) { return f.prototype; }

const fns = [Point, Assigned, Primitive, Frozen, Gen, arrow, method, decorated,
  WithBag, Klass, bound, { prototype: "object" }, Object.create(null)];
const seen = [];
for (let round = 0; round < 3; round++) {
  for (let i = 0; i < 4000; i++) {
    const f = fns[i % fns.length];
    const p = read(f);
    if (i < fns.length && round === 0) {
      seen.push(p === undefined ? "undefined" : typeof p === "object"
        ? (p === null ? "null" : Object.keys(p).join("+") || "{}")
        : typeof p === "function" ? "fn" : String(p));
    }
  }
}
console.log(seen.join(","));

// The default object is created once and then stays the same object.
function Fresh() {}
let same = 0;
let first = null;
for (let i = 0; i < 5000; i++) {
  const p = read(Fresh);
  if (first === null) first = p;
  if (p === first) same++;
}
console.log(same, first.constructor === Fresh, Object.getPrototypeOf(read(Gen)) !== null);

// Reassignment and freezing between hot reads are observed.
function Mutable() {}
let tags = [];
for (let i = 0; i < 6000; i++) {
  if (i === 2000) Mutable.prototype = { v: 1 };
  if (i === 4000) Mutable.prototype = 42;
  if (i % 2000 === 1999) {
    const p = read(Mutable);
    tags.push(typeof p === "object" ? JSON.stringify(p) : String(p));
  } else {
    read(Mutable);
  }
}
console.log(tags.join("|"));
