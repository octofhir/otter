// Element stores at sites whose receivers change storage kind, fill holes,
// grow, or are not arrays at all: the generic keyed store in optimized code
// and every case that must leave it.
function put(a, i, v) { a[i] = v; }
function fill(a, v) { for (let i = 0; i < a.length; i++) a[i] = v; return a; }
const out = [];

// new Array(n) starts with holes; numbers keep numeric storage, objects widen it.
let sum = 0;
for (let round = 0; round < 300; round++) {
  const n = 8 + (round % 13);
  const a = fill(new Array(n), round & 1 ? round : { v: round });
  sum += a.length + (typeof a[n - 1] === "number" ? a[n - 1] : a[n - 1].v);
  const b = new Array(n);
  for (let i = n - 1; i >= 0; i--) b[i] = i * 0.5; // fill holes from the top
  sum += b[0] + b[n - 1] + (b.indexOf(undefined) === -1 ? 1 : 0);
  b[n + 3] = 1; // grows past the length, leaving holes
  sum += b.length + (2 in b ? 1 : 0) + (n + 1 in b ? 1 : 0);
}
out.push(sum);

// grow from an empty array, downward, as crypto's zero fill does
let zeros = 0;
for (let round = 0; round < 200; round++) {
  const r = [];
  for (let i = 2 * (5 + round % 7); --i >= 0;) r[i] = 0;
  for (let i = 0; i < r.length; i++) if (r[i] === 0) zeros++;
}
out.push(zeros);

// mixed receivers at one site: arrays of each kind, objects, strings keys
const objects = [];
for (let i = 0; i < 2000; i++) {
  const target = i % 4 === 0 ? [1.5, 2.5, 3.5] : i % 4 === 1 ? [{}, {}, {}] : i % 4 === 2 ? { a: 1 } : new Array(3);
  put(target, i % 4 === 2 ? "k" + (i % 5) : i % 3, i);
  objects.push(target);
}
out.push(JSON.stringify(objects.slice(-8)));

// a hole store after an indexed setter appears on Array.prototype
const seen = [];
const holey = new Array(4);
for (let i = 0; i < 2000; i++) put(holey, i & 3, i);
Object.defineProperty(Array.prototype, 7, { set(v) { seen.push(v); }, configurable: true });
const late = new Array(10);
for (let i = 0; i < 20; i++) put(late, i % 10, i);
delete Array.prototype[7];
out.push(seen.join(), late.join());

// a custom prototype with an indexed setter, a frozen array, a sealed one
const proto = Object.create(Array.prototype, { 2: { set(v) { seen.push("p" + v); }, configurable: true } });
const custom = new Array(5);
Object.setPrototypeOf(custom, proto);
for (let i = 0; i < 1000; i++) put(custom, i % 5, i);
const frozen = Object.freeze([1, 2, 3]);
for (let i = 0; i < 1000; i++) put(frozen, i % 4, i);
const sealed = Object.seal([1, , 3]);
for (let i = 0; i < 1000; i++) put(sealed, i % 3, i);
out.push(custom.join(), seen.length, frozen.join(), sealed.join(), 1 in sealed);

// strict-mode failure still throws through the runtime
function putStrict(a, i, v) { "use strict"; a[i] = v; }
let threw = 0;
for (let i = 0; i < 1000; i++) { try { putStrict(frozen, i % 3, i); } catch (e) { threw++; } }
out.push(threw);
console.log(out.join("\n"));
