/* otter-test:
name = "binary: DataView float + bigint get/set round-trips"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}

const ab = new ArrayBuffer(32);
const dv = new DataView(ab);

// §25.3.4.5 / §25.3.4.9 — float32 round-trip both byte orders.
dv.setFloat32(0, 3.140000104904175);
if (Math.abs(dv.getFloat32(0) - 3.140000104904175) > 1e-7) fail();
dv.setFloat32(0, 1.5, true);
if (dv.getFloat32(0, true) !== 1.5) fail();

// §25.3.4.6 / §25.3.4.10 — float64.
dv.setFloat64(8, 3.141592653589793);
if (dv.getFloat64(8) !== 3.141592653589793) fail();
dv.setFloat64(8, -2.5, true);
if (dv.getFloat64(8, true) !== -2.5) fail();

// NaN / ±Infinity preserved.
dv.setFloat64(16, NaN);
if (!isNaN(dv.getFloat64(16))) fail();
dv.setFloat64(16, Infinity);
if (dv.getFloat64(16) !== Infinity) fail();
dv.setFloat64(16, -Infinity);
if (dv.getFloat64(16) !== -Infinity) fail();

// §25.3.4.3 / §25.3.4.4 — BigInt64.
dv.setBigInt64(24, 9223372036854775807n);
if (dv.getBigInt64(24) !== 9223372036854775807n) fail();
dv.setBigInt64(24, -1n);
if (dv.getBigInt64(24) !== -1n) fail();
if (dv.getBigUint64(24) !== 18446744073709551615n) fail();

// LE round-trip.
dv.setBigUint64(24, 1n, true);
if (dv.getBigUint64(24, true) !== 1n) fail();
