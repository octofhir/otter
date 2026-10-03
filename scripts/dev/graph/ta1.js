// Every converted typed-array element kind through hot generated loads and
// stores: narrow sign/zero extension, modular int32 stores, Uint8Clamped
// clamping, Uint32 values above i32::MAX, Float32 rounding of int32 and
// double values, and misses for values the fast path must not convert.
const kinds = [Int8Array, Uint8Array, Uint8ClampedArray, Int16Array, Uint16Array,
  Int32Array, Uint32Array, Float32Array, Float64Array];
const samples = [0, 1, -1, 127, 128, 255, 256, -129, 32767, 32768, 65535, 65536,
  2147483647, -2147483648, 0.5, -0.5, 1.5, 2.5, 3.14159, -7.75, 1e10, NaN];

function fill(view, values) {
  for (let i = 0; i < values.length; i++) view[i] = values[i];
}
function sum(view, n) {
  let total = 0;
  for (let i = 0; i < n; i++) total += view[i];
  return total;
}
function roundTrip(view, values) {
  const out = [];
  for (let i = 0; i < values.length; i++) { view[i] = values[i]; out.push(view[i]); }
  return out;
}

const report = {};
for (const Kind of kinds) {
  const view = new Kind(samples.length);
  let checksum = 0;
  for (let round = 0; round < 300; round++) {
    fill(view, samples);
    checksum = sum(view, samples.length);
  }
}
console.log("ok");
