// Per-construction cost of common constructors, ns per op.
// Usage: otter scripts/dev/ctor_matrix.js [filter]; node/bun the same.
const N = 2_000_000;
const ab = new ArrayBuffer(16);
class Sub extends Uint8Array {}
class Base { constructor(x) { this.x = x; } }
class Derived extends Base { constructor(x) { super(x); this.y = x; } }
class MapSub extends Map {}
const cases = {
  "Map": () => new Map(),
  "Set": () => new Set(),
  "MapSub": () => new MapSub(),
  "Uint8Array(ab)": () => new Uint8Array(ab),
  "Uint8Array(8)": () => new Uint8Array(8),
  "Sub(ab)": () => new Sub(ab),
  "ArrayBuffer(16)": () => new ArrayBuffer(16),
  "Date(0)": () => new Date(0),
  "Error": () => new Error("x"),
  "Object": () => new Object(),
  "Array(4)": () => new Array(4),
  "Base JS ctor": () => new Base(1),
  "Derived JS ctor": () => new Derived(1),
  "Promise.resolve": () => Promise.resolve(1),
  "new Promise": () => new Promise(() => {}),
};
const filter = (typeof process !== "undefined" && process.argv[2]) || "";
let sink;
for (const [name, make] of Object.entries(cases)) {
  if (filter && !name.includes(filter)) continue;
  for (let i = 0; i < N / 10; i++) sink = make();
  const t0 = performance.now();
  for (let i = 0; i < N; i++) sink = make();
  const ns = ((performance.now() - t0) * 1e6) / N;
  console.log(`${name.padEnd(18)} ${ns.toFixed(1)}`);
}
