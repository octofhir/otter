/* otter-test:
name = "binary: DataView setX round-trips through getX"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}

const ab = new ArrayBuffer(64);
const dv = new DataView(ab);

// Each (set, get) pair round-trips for the integer + float kinds.
dv.setInt8(0, 5);
if (dv.getInt8(0) !== 5) fail();
dv.setUint8(4, 5);
if (dv.getUint8(4) !== 5) fail();
dv.setInt16(8, 5);
if (dv.getInt16(8) !== 5) fail();
dv.setUint16(12, 5);
if (dv.getUint16(12) !== 5) fail();
dv.setInt32(16, 5);
if (dv.getInt32(16) !== 5) fail();
dv.setUint32(20, 5);
if (dv.getUint32(20) !== 5) fail();
dv.setFloat32(24, 5);
if (dv.getFloat32(24) !== 5) fail();
dv.setFloat64(32, 5);
if (dv.getFloat64(32) !== 5) fail();

// LE round-trips.
dv.setInt32(40, 0x12345678, true);
if (dv.getInt32(40, true) !== 0x12345678) fail();

// BigInt round-trips.
dv.setBigInt64(44, 17n);
if (dv.getBigInt64(44) !== 17n) fail();
dv.setBigUint64(52, 17n);
if (dv.getBigUint64(52) !== 17n) fail();

// Out-of-bounds raises.
let threw = false;
try { dv.setUint32(62, 0); } catch (e) { threw = true; }
if (!threw) fail();
