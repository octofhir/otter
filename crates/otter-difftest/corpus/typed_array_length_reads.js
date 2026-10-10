// `.length` on typed views in hot code: plain views of every width, a
// subarray, a view with a named own property or its own `length`, a subclass
// instance, length-tracking and fixed views over a resizable buffer that
// grows and shrinks, and views whose buffer is later detached.
function len(v) { return v.length; }

class Bytes extends Uint8Array {}
const named = new Int16Array(5);
named.tag = "named";
const own = new Uint32Array(6);
Object.defineProperty(own, "length", { value: 99 });
const resizable = new ArrayBuffer(8, { maxByteLength: 64 });
const tracking = new Uint8Array(resizable);
const fixedOverResizable = new Uint8Array(resizable, 0, 8);
const views = [
  new Uint8Array(3), new Int32Array(7), new Float64Array(2), new Uint8ClampedArray(4),
  new Float32Array(9).subarray(2, 5), named, own, new Bytes(11), tracking,
  fixedOverResizable, [1, 2], "abcd",
];

const out = [];
for (let round = 0; round < 3; round++) {
  let sum = 0;
  for (let i = 0; i < 6000; i++) sum += len(views[i % views.length]);
  out.push(sum);
  if (round === 0) resizable.resize(32);
  if (round === 1) resizable.resize(4);
}
console.log(out.join(","));
console.log(views.map(len).join(","));

// A detach after the views were read hot reports length 0.
const buffers = [new ArrayBuffer(16), new ArrayBuffer(32)];
const detachable = [new Uint8Array(buffers[0]), new Float64Array(buffers[1])];
let before = 0;
for (let i = 0; i < 8000; i++) before += len(detachable[i & 1]);
buffers[0].transfer();
let after = 0;
for (let i = 0; i < 8000; i++) after += len(detachable[i & 1]);
console.log(before, after, detachable.map(len).join(","));
