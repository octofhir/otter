// Typed payloads and indices must survive exact exits and moving-GC calls.
function signedPair(a, i) { return a[i] + a[i + 1]; }
function unsignedPair(a, i) { return a[i] + a[i + 1]; }
function floatPair(a, i) { return a[i] + a[i + 1]; }
function floatRead(a, i) { return a[i]; }
function doubleRead(a, i) { return a[i]; }
function indexed(a, i) { return a[i * 1.0]; }
function write(a, i, x) { a[i] = x; return a[i]; }
function allocate(x) {
  const a = new Array(40);
  a[0] = x;
  return a[0] + 1;
}
function acrossCall(a, i) {
  const x = a[i];
  const y = allocate(x);
  return x + y;
}
const signed = new Int32Array([-2147483648, 7, 11, -13]);
const unsigned = new Uint32Array([4294967295, 2147483648, 3, 4]);
const floats = new Float32Array([-0, 1.25, Infinity, NaN]);
const doubles = new Float64Array([-0, NaN, Infinity, 1.5]);
let total = 0;
for (let i = 0; i < 2000; i++) {
  total += signedPair(signed, 1);
  unsignedPair(unsigned, 0);
  floatPair(floats, 0);
  floatRead(floats, 0);
  doubleRead(doubles, 0);
  indexed(signed, 1);
  write(signed, 2, i | 0);
  acrossCall(unsigned, 0);
}
console.log(JSON.stringify([
  signedPair(signed, 0), unsignedPair(unsigned, 0),
  floatPair(floats, 0), acrossCall(unsigned, 0),
  indexed(signed, 1.5), indexed(signed, -1), indexed(signed, 4294967296),
  indexed(signed, NaN), indexed(signed, Infinity), indexed(signed, -0),
  Object.is(indexed(floats, -0), -0),
  Object.is(floatRead(floats, 0), -0), Number.isNaN(floatRead(floats, 3)),
  floatRead(floats, 2) === Infinity,
  Object.is(doubleRead(doubles, 0), -0), Number.isNaN(doubleRead(doubles, 1)),
  doubleRead(doubles, 2) === Infinity,
  signedPair(new Uint8Array([255, 128]), 0),
  write(signed, 1, 4294967295), write(signed, 1.5, 42), signed[1]
]));

// A reentrant operation invalidates the earlier view proof. The second load
// must observe the entire fixed view becoming out of bounds after shrinkage.
const buffer = new ArrayBuffer(32, {maxByteLength: 64});
const resizable = new Int32Array(buffer, 0, 8);
resizable[0] = 9;
let calls = 0;
function resizeBetween(a, size) {
  const before = a[0];
  calls++;
  buffer.resize(size);
  const after = a[0];
  return [before, after];
}
for (let i = 0; i < 1000; i++) resizeBetween(resizable, 32);
const shrunk = resizeBetween(resizable, 8);
buffer.resize(32);
console.log(JSON.stringify([shrunk, resizable[0], calls]));

// One source site switches from scalar speculation to canonical coercion.
let conversions = 0;
const coercible = {valueOf() { conversions++; return 258; }};
const narrow = new Uint8Array(2);
for (let i = 0; i < 2000; i++) write(narrow, 0, i | 0);
console.log(write(narrow, 0, coercible), conversions);

// Scalar floating stores retain Float32 rounding, NaN, -0 and infinities.
function putFloat(a, x) { a[0] = x * 1.0; return a[0]; }
function putDouble(a, x) { a[0] = x * 1.0; return a[0]; }
const fstore = new Float32Array(1);
const dstore = new Float64Array(1);
for (let i = 0; i < 2000; i++) {
  putFloat(fstore, i + 0.25);
  putDouble(dstore, i + 0.25);
}
console.log(JSON.stringify([
  putFloat(fstore, 1.00000006), putDouble(dstore, 1.00000006),
  Object.is(putFloat(fstore, -0), -0), Object.is(putDouble(dstore, -0), -0),
  Number.isNaN(putFloat(fstore, NaN)), Number.isNaN(putDouble(dstore, NaN)),
  putFloat(fstore, Infinity) === Infinity,
  putDouble(dstore, -Infinity) === -Infinity
]));

// Two tiny callees use identical source PCs with different storage layouts.
function readSignedByte(a) { return a[0]; }
function readUnsignedHalf(a) { return a[0]; }
let inlineEffects = 0;
function inlinePair(a, b) {
  const x = readSignedByte(a);
  inlineEffects++;
  return x + readUnsignedHalf(b);
}
const bytes = new Int8Array([-128]);
const halves = new Uint16Array([65535]);
for (let i = 0; i < 2000; i++) inlinePair(bytes, halves);
console.log(inlinePair(bytes, halves), inlinePair(bytes, new Int32Array([-17])), inlineEffects);
