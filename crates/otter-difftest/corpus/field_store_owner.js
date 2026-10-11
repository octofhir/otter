// Native stores observe the value they leave behind: appends, aliases,
// overwrites from loaded fields, literals and nullish constants widen the
// defining summary exactly when a new category arrives.
const results = [];
class P { constructor(x) { this.x = x; } }
class Q { constructor(x) { this.y = 1; this.x = x; } }
class R { constructor(x) { this.z = 2; this.w = 3; this.x = x; } }

// An append whose value is the receiver leaves the published shape.
function selfLink(o) { o.me = o; return o; }
function readSelf(o) { return o.me.x + (o.me === o ? 100 : 0); }
let linked;
for (let i = 0; i < 6000; i++) linked = selfLink(new P(i & 7));
for (let i = 0; i < 6000; i++) readSelf(linked);
results.push(readSelf(linked), readSelf(selfLink(new P(9))));
const foreign = new P(4);
foreign.me = new Q(5);
results.push(readSelf(foreign));

// A value of the receiver's old shape may be the receiver itself.
function attach(a, b) { a.peer = b; return a; }
function readPeer(a) { const p = a.peer; return p.x + (p.peer === undefined ? 0 : 1000); }
let attached;
for (let i = 0; i < 6000; i++) {
  attached = attach(new P(i & 3), new P(1));
  readPeer(attached);
}
const same = new P(6);
attach(same, same);
results.push(readPeer(same), readPeer(attached));
for (let i = 0; i < 2000; i++) readPeer(i & 1 ? same : attached);
results.push(readPeer(same), readPeer(attach(new P(2), new Q(8))));

// Overwrites from loaded fields keep the loaded categories; a new shape widens.
class Node { constructor(key) { this.key = key; this.left = null; this.right = null; } }
function rotate(n) { const r = n.right; n.right = r.left; r.left = n; return r; }
function sumKeys(n, depth) {
  if (n == null || depth > 8) return 0;
  return (n.key ?? n.x) + sumKeys(n.left, depth + 1) + sumKeys(n.right, depth + 1);
}
function chain(length) {
  const head = new Node(0);
  let cursor = head;
  for (let i = 1; i < length; i++) { cursor.right = new Node(i); cursor = cursor.right; }
  return head;
}
let rotated = 0;
for (let i = 0; i < 3000; i++) rotated += sumKeys(rotate(chain(4)), 0);
results.push(rotated);
const odd = chain(3);
odd.right.left = new R(77);
const turned = rotate(odd);
results.push(turned.left.right.x, sumKeys(turned.left, 0), turned.key);
odd.right = { key: 5, left: null, right: undefined };
results.push(sumKeys(odd, 0));

// Literal fields from known values skip only already-captured categories.
function pair(holder) { const a = holder.x; return { first: holder, second: a }; }
function readPair(p) { return p.first.x + (typeof p.second === 'number' ? p.second : -1); }
let pairs = 0;
for (let i = 0; i < 6000; i++) pairs += readPair(pair(new P(i & 15)));
results.push(pairs);
results.push(readPair(pair(new Q(3))), readPair(pair(new P('s'))), readPair(pair(new R(4))));

// Nullish constants: each nullish alternative widens once.
class Box { constructor() { this.v = null; } }
function clear(box, undef) { if (undef) box.v = undefined; else box.v = null; return box; }
function readBox(box) { const v = box.v; return v === null ? 1 : v === undefined ? 2 : v.x; }
const box = new Box();
let boxes = 0;
for (let i = 0; i < 6000; i++) boxes += readBox(clear(box, false));
boxes += readBox(clear(box, true));
box.v = new P(30);
boxes += readBox(box);
results.push(boxes);

// Array named storage keeps the Array as receiver while the bag stores.
function tagArray(a, v) { a.tag = v; return a; }
function readTag(a) { const t = a.tag; return t === null ? -1 : t.x; }
const tagged = [1, 2, 3];
tagged.tag = new P(0);
let tags = 0;
for (let i = 0; i < 6000; i++) tags += readTag(tagArray(tagged, new P(i & 1)));
tags += readTag(tagArray(tagged, new Q(40)));
tags += readTag(tagArray(tagged, null));
results.push(tags, tagged.length, Object.keys(tagged).join(','));

// Generic stores of numbers and fresh receivers through megamorphic sites.
function setAny(o, k) { o[k] = 7; o.n = 3; return o; }
let generic = 0;
const kinds = [() => ({}), () => ({ a: 1 }), () => ({ b: 1 }), () => ({ c: 1 }), () => ({ d: 1 }), () => []];
for (let i = 0; i < 6000; i++) {
  const o = setAny(kinds[i % kinds.length](), 'k' + (i % 3));
  generic += o.n + o['k' + (i % 3)];
}
const keep = [];
for (let i = 0; i < 2000; i++) keep.push(setAny({ e: i }, 'held'));
for (let i = 0; i < 20000; i++) setAny({ f: i }, 'tmp');
for (const o of keep) { o.n = { x: o.e }; }
let held = 0;
for (const o of keep) held += o.n.x + o.held;
results.push(generic, held);

// Effects run exactly once around observed stores.
let effects = 0;
class Watched {
  constructor() { this.v = 0; }
  get once() { effects++; return this.v; }
}
function bump(w, o) { w.v = o; return w.once; }
const watched = new Watched();
let seen = 0;
for (let i = 0; i < 6000; i++) seen += bump(watched, i).valueOf() & 1;
const objectSeen = bump(watched, new R(11));
results.push(seen, effects, objectSeen.x, effects);

console.log(JSON.stringify(results));
