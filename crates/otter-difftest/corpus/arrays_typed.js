const dense = [1, 2, 3];
const holey = [1, , 3];
const typed = new Int32Array(8);
for (let i = 0; i < 100; i++) { dense.push(i); typed[i & 7] += i; }

globalThis.hotValues = [1, 2.5, 3, 4.5];
function hotElement(values, index) {
  return values[index];
}
let warmSource = "";
for (let i = 0; i < 4010; i++) {
  warmSource += "hotElement(hotValues, 0);";
}
eval(warmSource);
const hotSum = hotElement(hotValues, 0) + hotElement(hotValues, 1)
  + hotElement(hotValues, 2) + hotElement(hotValues, 3);
let loadThrow = "";
try { hotElement(null, 0); } catch (error) { loadThrow = error.name; }

globalThis.hotFloatValues = [1.25, 2.5, 3.75, 5];
function hotFloatArray(values, limit) {
  let total = 0.25;
  let index = 0;
  while (index < limit) {
    total = total + values[index];
    index = index + 1;
  }
  return total;
}

globalThis.hotFloatRmwA = [1.25, 2.5, 3.75, 5];
globalThis.hotFloatRmwB = [0.5, 1, 1.5, 2];
function hotFloatRmw(a, b, c, limit) {
  for (let index = 0; index < limit; index = index + 1) {
    a[index] = a[index] * c + b[index];
  }
  return a[0] + a[1] + a[2] + a[3];
}

globalThis.hotManyA = [1, 2, 3, 4];
globalThis.hotManyB = [10, 20, 30, 40];
globalThis.hotManyC = [2, 4, 6, 8];
globalThis.hotManyD = [1, 2, 3, 4];
function hotManyArrays(a, b, c, d, limit) {
  let total = 0;
  let index = 0;
  while (index < limit) {
    total = total + a[index];
    total = total + b[index];
    total = total + c[index];
    total = total + d[index];
    index = index + 1;
  }
  return total;
}

globalThis.hotChainA = [0];
globalThis.hotChainB = [0];
globalThis.hotChainC = [0];
globalThis.hotChainD = [42.5];
function hotTaggedChain(a, b, c, d, index) {
  const first = a[index];
  const second = b[first];
  const third = c[second];
  return d[third];
}

let warmPreciseRoots = "";
for (let i = 0; i < 4010; i++) {
  warmPreciseRoots += "hotFloatArray(hotFloatValues, 4);";
  warmPreciseRoots += "hotFloatRmw(hotFloatRmwA, hotFloatRmwB, 0.5, 4);";
  warmPreciseRoots += "hotManyArrays(hotManyA, hotManyB, hotManyC, hotManyD, 4);";
  warmPreciseRoots += "hotTaggedChain(hotChainA, hotChainB, hotChainC, hotChainD, 0);";
}
eval(warmPreciseRoots);
const hotFloatSum = hotFloatArray(hotFloatValues, 4);
const hotFloatRmwSum = hotFloatRmw(hotFloatRmwA, hotFloatRmwB, 0.5, 4);
const hotManySum = hotManyArrays(hotManyA, hotManyB, hotManyC, hotManyD, 4);
const hotChain = hotTaggedChain(hotChainA, hotChainB, hotChainC, hotChainD, 0);

// A single call dominated by one numeric array loop. Function-entry hotness
// can never optimize it; the back-edge must enter optimized code at the loop
// header while the interpreter window remains the canonical GC root set.
const osrOnceA = [];
const osrOnceB = [];
for (let index = 0; index < 2048; index = index + 1) {
  osrOnceA[index] = 1.25;
  osrOnceB[index] = 0.5;
}
function osrOnceFloatRmw(a, b, scale, limit) {
  let total = 0.5;
  for (let index = 0; index < limit; index = index + 1) {
    a[index] = a[index] * scale + b[index];
    total = total + a[index];
  }
  return total;
}
const osrOnceTotal = osrOnceFloatRmw(osrOnceA, osrOnceB, 0.5, 2048);

// A loop bound by a typed view's `length`, read through a field: the view
// later turns detached, length-tracking, gains an own `length`, or is
// replaced by a plain array.
function viewChecksum(holder) {
  let sum = 0;
  for (let i = 0; i < holder.view.length; i++) sum = (sum + i * holder.view[i]) % 1000;
  return sum;
}
const viewHolder = { view: new Uint8Array(64).map((_, i) => i * 3) };
let viewTotal = 0;
for (let r = 0; r < 300; r++) viewTotal += viewChecksum(viewHolder);
const growable = new ArrayBuffer(8, { maxByteLength: 64 });
viewHolder.view = new Uint8Array(growable);
growable.resize(32);
viewTotal += viewChecksum(viewHolder) + viewHolder.view.length;
const shadowed = new Uint8Array(16).fill(2);
Object.defineProperty(shadowed, "length", { value: 4 });
viewHolder.view = shadowed;
viewTotal += viewChecksum(viewHolder);
viewHolder.view = [5, 6, 7];
viewTotal += viewChecksum(viewHolder);
const detachable = new Uint8Array(new ArrayBuffer(16)).fill(1);
viewHolder.view = detachable;
viewTotal += viewChecksum(viewHolder);
structuredClone(detachable.buffer, { transfer: [detachable.buffer] });
viewTotal += viewChecksum(viewHolder) + detachable.length;

JSON.stringify({
  dense: dense.length,
  hole: 1 in holey,
  typed: Array.from(typed),
  hotSum,
  hotFloatSum,
  hotFloatRmwSum,
  hotFloatRmw: hotFloatRmwA,
  hotManySum,
  hotChain,
  osrOnceTotal,
  viewTotal,
  osrOnceFirst: osrOnceA[0],
  osrOnceLast: osrOnceA[2047],
  hotFirst: hotElement(hotValues, 0),
  loadThrow
});
