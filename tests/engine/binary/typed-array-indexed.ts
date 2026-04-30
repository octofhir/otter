/* otter-test:
name = "binary: TypedArray indexed access + element coercion"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}

// Int8 wraps 128 → -128 per §6.1.6.1.5 ToInt8.
const i8 = new Int8Array(4);
i8[0] = 127;
i8[1] = 128;
i8[2] = -129;
i8[3] = 256;
if (i8[0] !== 127) fail();
if (i8[1] !== -128) fail();
if (i8[2] !== 127) fail();
if (i8[3] !== 0) fail();

// Uint16 wraps mod 2^16.
const u16 = new Uint16Array(3);
u16[0] = 65536;
u16[1] = -1;
u16[2] = 65537;
if (u16[0] !== 0) fail();
if (u16[1] !== 65535) fail();
if (u16[2] !== 1) fail();

// Int32 ToInt32 truncates toward zero.
const i32 = new Int32Array(2);
i32[0] = 7.7;
i32[1] = -7.9;
if (i32[0] !== 7) fail();
if (i32[1] !== -7) fail();

// Float32 stores the rounded f32 representation.
const f32 = new Float32Array(1);
f32[0] = 0.1;
if (f32[0] === 0.1) fail();
if (Math.abs(f32[0] - 0.1) > 1e-6) fail();

// BigInt64Array rejects Number stores.
const big = new BigInt64Array(2);
big[0] = 1n;
let threw = false;
try { big[1] = 1; } catch (e) { threw = true; }
if (!threw) fail();

// Number-typed array rejects BigInt stores.
const n = new Uint8Array(2);
let threw2 = false;
try { n[0] = 1n; } catch (e) { threw2 = true; }
if (!threw2) fail();
