/* otter-test:
name = "binary: TypedArray constructor overloads (length / array / buffer / typedarray)"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}

// §23.2.5.1.2 `new T(length)`.
const a = new Uint8Array(4);
if (a.length !== 4) fail();
if (a.byteLength !== 4) fail();
if (a.byteOffset !== 0) fail();
if (a.BYTES_PER_ELEMENT !== 1) fail();
if (Uint8Array.BYTES_PER_ELEMENT !== 1) fail();
if (Int32Array.BYTES_PER_ELEMENT !== 4) fail();
if (Float64Array.BYTES_PER_ELEMENT !== 8) fail();
if (BigInt64Array.BYTES_PER_ELEMENT !== 8) fail();

// §23.2.5.1.5 `new T(arrayLike)`.
const b = new Uint8Array([10, 20, 30]);
if (b.length !== 3) fail();
if (b[0] !== 10) fail();
if (b[1] !== 20) fail();
if (b[2] !== 30) fail();

// Element-type coercion on store: 257 → 1.
const c = new Uint8Array([257, -1, 256]);
if (c[0] !== 1) fail();
if (c[1] !== 255) fail();
if (c[2] !== 0) fail();

// §23.2.5.1.4 `new T(buffer, byteOffset, length)`.
const buf = new ArrayBuffer(16);
const view = new Int32Array(buf);
if (view.length !== 4) fail();
if (view.byteLength !== 16) fail();
view[0] = 0x10203040;
const view2 = new Int32Array(buf, 0, 1);
if (view2.length !== 1) fail();
if (view2[0] !== 0x10203040) fail();

// §23.2.5.1.3 `new T(typedArray)` — element copy.
const src = new Int16Array([1, 2, 3]);
const dst = new Uint8Array(src);
if (dst.length !== 3) fail();
if (dst[0] !== 1) fail();
if (dst[1] !== 2) fail();
if (dst[2] !== 3) fail();

// BigInt-typed arrays.
const big = new BigInt64Array(2);
if (big.length !== 2) fail();
if (big.byteLength !== 16) fail();
big[0] = 9n;
big[1] = -1n;
if (big[0] !== 9n) fail();
if (big[1] !== -1n) fail();

// Float64.
const f = new Float64Array([1.5, 2.5, 3.5]);
if (f.length !== 3) fail();
if (f[1] !== 2.5) fail();

// Uint8ClampedArray clamps on store.
const clamp = new Uint8ClampedArray(3);
clamp[0] = 300;
clamp[1] = -50;
clamp[2] = 200;
if (clamp[0] !== 255) fail();
if (clamp[1] !== 0) fail();
if (clamp[2] !== 200) fail();

// Out-of-range write silently drops.
const x = new Uint8Array(2);
x[5] = 99;
if (x.length !== 2) fail();
