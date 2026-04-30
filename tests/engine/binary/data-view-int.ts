/* otter-test:
name = "binary: DataView integer get/set across byte orders"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}

const ab = new ArrayBuffer(16);
const dv = new DataView(ab);

if (dv.byteLength !== 16) fail();
if (dv.byteOffset !== 0) fail();
if (dv.buffer.byteLength !== 16) fail();

// §25.3.4.10 setUint8 / §25.3.4.4 getUint8 — single byte, no LE/BE.
dv.setUint8(0, 0x12);
dv.setUint8(1, 0x34);
if (dv.getUint8(0) !== 0x12) fail();
if (dv.getUint8(1) !== 0x34) fail();

// §25.3.4.7 setInt16 / §25.3.4.6 getInt16 — default big-endian.
dv.setInt16(2, 0x1234);
if (dv.getUint8(2) !== 0x12) fail();
if (dv.getUint8(3) !== 0x34) fail();
if (dv.getInt16(2) !== 0x1234) fail();
// Little-endian round-trip.
dv.setInt16(2, 0x1234, true);
if (dv.getUint8(2) !== 0x34) fail();
if (dv.getUint8(3) !== 0x12) fail();
if (dv.getInt16(2, true) !== 0x1234) fail();

// Negative int16 round-trip.
dv.setInt16(4, -1);
if (dv.getInt16(4) !== -1) fail();
if (dv.getUint16(4) !== 0xFFFF) fail();

// §25.3.4.11 setInt32 / §25.3.4.8 getInt32.
dv.setInt32(8, 0x01020304);
if (dv.getInt32(8) !== 0x01020304) fail();
dv.setInt32(8, 0x01020304, true);
if (dv.getInt32(8, true) !== 0x01020304) fail();

// §25.3.4.16 setUint32 / §25.3.4.13 getUint32 — large unsigned.
dv.setUint32(12, 0xFFFFFFFF);
if (dv.getUint32(12) !== 0xFFFFFFFF) fail();
