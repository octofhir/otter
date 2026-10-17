// Per-element cost of Array.prototype callbacks against a plain loop
// calling the same function.
const arr = Array.from({ length: 1000 }, (_, i) => i);
let sum = 0;
function cb(x) { sum += x; }
function viaLoop() { for (let i = 0; i < arr.length; i++) cb(arr[i], i, arr); }
function viaForEach() { arr.forEach(cb); }
function viaMap() { return arr.map((x) => x + 1); }
function viaFilter() { return arr.filter((x) => x & 1); }
function time(name, f) {
  for (let i = 0; i < 200; i++) f();
  const t = Date.now();
  for (let i = 0; i < 5000; i++) f();
  const ms = Date.now() - t;
  console.log(name, ms, "ms", (ms * 1e6 / 5e6).toFixed(1), "ns/elem");
}
time("loop", viaLoop);
time("forEach", viaForEach);
time("map", viaMap);
time("filter", viaFilter);
