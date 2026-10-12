// One element store site that sees typed views of every kind, Arrays and
// plain objects: int32 and double values, the conversions each element kind
// applies, indices at and past the length, non-Number values, a detached
// buffer and a view that tracks a resizable buffer.
const results = [];
const kinds = [Int8Array, Uint8Array, Uint8ClampedArray, Int16Array, Uint16Array,
  Int32Array, Uint32Array, Float32Array, Float64Array];

function put(target, index, value) { target[index] = value; }
function fill(target, length, value) {
  let index = 0;
  while (index < length) target[index++] = value;
  return target;
}
function digest(view) {
  let sum = 0;
  for (let i = 0; i < view.length; i++) {
    const x = Number(view[i]);
    sum = (sum * 31 + (Number.isFinite(x) ? x : 7)) % 1000000007;
  }
  return sum;
}

const values = [0, 1, -1, 127, 128, 255, 256, -129, 65535, 65536, 2147483647, -2147483648,
  0.5, 1.5, 2.5, -0.5, 254.5, 255.5, 3.7, -3.7, 1e10, -1e10, 4294967296.5, 2 ** 63, -(2 ** 64),
  NaN, Infinity, -Infinity, -0, 1e-310];
let rounds = 0;
for (let r = 0; r < 400; r++) {
  for (const Kind of kinds) {
    const view = new Kind(values.length + 2);
    for (let i = 0; i < values.length; i++) put(view, i, values[(i + r) % values.length]);
    // Past the length, a negative or fractional index: nothing is written.
    put(view, view.length, 9);
    put(view, -1, 9);
    put(view, 1.5, 9);
    put(view, '2', 7);
    rounds = (rounds + digest(view)) % 1000000007;
  }
}
results.push(rounds);

// Fill loops over several kinds, as an emulator allocates its memories.
const filled = [fill(new Int32Array(5000), 5000, 0xF8F8F8), fill(new Uint8Array(7000), 7000, 0xFF),
  fill(new Int8Array(300), 300, 200), fill(new Float32Array(900), 900, 1 / 3),
  fill(new Uint16Array(600), 600, -2), fill(new Uint8ClampedArray(500), 500, 300.5),
  fill(new Float64Array(700), 700, 0.1), fill(new Uint32Array(400), 400, -7.9)];
results.push(filled.map(digest));

// Non-Number values convert through the runtime, valueOf and all.
const log = [];
const probe = { valueOf() { log.push('valueOf'); return 42; } };
const odd = new Int16Array(4);
for (let i = 0; i < 2000; i++) put(odd, i & 3, i);
put(odd, 0, '12'); put(odd, 1, true); put(odd, 2, probe); put(odd, 3, null);
results.push(Array.from(odd), log.length);

// BigInt views and Arrays at the same site.
const big = new BigInt64Array(3);
put(big, 0, 5n); put(big, 2, -1n);
let threw = false;
try { put(big, 1, 1); } catch (e) { threw = e instanceof TypeError; }
results.push(String(big[0]), String(big[2]), threw);
const plain = [1, 2, 3];
put(plain, 1, 2.5); put(plain, 3, 'x');
results.push(plain.join(','));

// A detached buffer, then a view that tracks a resizable buffer.
const buffer = new ArrayBuffer(16);
const early = new Uint8Array(buffer);
for (let i = 0; i < 1000; i++) put(early, i & 15, i);
results.push(digest(early));
if (typeof structuredClone === 'function') {
  structuredClone(buffer, { transfer: [buffer] });
  put(early, 0, 1);
  results.push(early.length, early[0]);
} else {
  results.push(0, undefined);
}
const after = new Int32Array(8);
for (let i = 0; i < 1000; i++) put(after, i & 7, i * 3);
results.push(digest(after));
const growable = new ArrayBuffer(8, { maxByteLength: 32 });
const tracking = new Uint8Array(growable);
for (let i = 0; i < 1000; i++) put(tracking, i & 7, i);
growable.resize(16);
put(tracking, 12, 77);
results.push(tracking.length, tracking[12], digest(tracking));

// Magnitudes past 2^63 keep the low bits of their ToInt32 everywhere.
const dataView = new DataView(new ArrayBuffer(8));
const converted = [];
for (const value of [2 ** 63, -(2 ** 63), 2 ** 64 + 2 ** 12, 1e300, -1e21]) {
  dataView.setInt8(0, value); dataView.setUint16(2, value); dataView.setInt16(4, value);
  converted.push(dataView.getInt8(0), dataView.getUint16(2), dataView.getInt16(4));
  const cells = new Int8Array(2);
  cells[0] = 5;
  Atomics.add(cells, 0, value);
  Atomics.xor(cells, 1, value);
  converted.push(cells[0], cells[1], new Uint8Array([value])[0]);
}
results.push(converted);

console.log(JSON.stringify(results));
