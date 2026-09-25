// Named and computed loads on builtin iterators, collections, promises,
// buffers and generators walk the receiver's live prototype chain. Compiled
// miss transitions and the interpreter resolve them through one ladder, so a
// saturated site still finds `next` on %ArrayIteratorPrototype%.
function named(o) { return o.next; }
function computed(o, k) { return o[k]; }
const makers = [
  () => [][Symbol.iterator](),
  () => new Int8Array(2)[Symbol.iterator](),
  () => new Uint8Array(2)[Symbol.iterator](),
  () => [].keys(),
  () => [].entries(),
  () => new Map().keys(),
  () => new Set().values(),
  () => (function* () {})(),
];
const computedCases = [
  [() => Promise.resolve(1), "then"],
  [() => new ArrayBuffer(4), "byteLength"],
  [() => new DataView(new ArrayBuffer(4)), "getInt8"],
  [() => new WeakMap(), "get"],
  [() => new Map(), "set"],
  [() => new Set(), Symbol.iterator],
];
let namedHits = 0;
let computedHits = 0;
for (let round = 0; round < 120; round++) {
  for (const make of makers) if (typeof named(make()) === "function") namedHits++;
  for (const [make, key] of computedCases) if (computed(make(), key) !== undefined) computedHits++;
}
class Sub extends Map { extra() { return 7; } }
const sub = new Sub();
let subHits = 0;
for (let i = 0; i < 300; i++) if (computed(sub, "extra") === Sub.prototype.extra && sub.extra() === 7) subHits++;
const iterNextShared = [][Symbol.iterator]().next === new Uint8Array(1)[Symbol.iterator]().next;
JSON.stringify({ namedHits, computedHits, subHits, iterNextShared });
