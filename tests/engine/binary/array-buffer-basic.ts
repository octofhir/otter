/* otter-test:
name = "binary: ArrayBuffer constructor + isView + slice + detach"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}

// §25.1.4.1 `new ArrayBuffer(length)`.
const ab = new ArrayBuffer(16);
if (ab.byteLength !== 16) fail();
if (ab.detached) fail();
if (ab.resizable) fail();
// `maxByteLength` reads the current byte length for non-resizable.
if (ab.maxByteLength !== 16) fail();

// §25.1.4.3 `ArrayBuffer.isView` — only TypedArrays / DataViews.
if (ArrayBuffer.isView(ab)) fail();
if (ArrayBuffer.isView({})) fail();
if (ArrayBuffer.isView([])) fail();
const view = new Uint8Array(ab);
if (!ArrayBuffer.isView(view)) fail();
const dv = new DataView(ab);
if (!ArrayBuffer.isView(dv)) fail();

// §25.1.5.4 `ArrayBuffer.prototype.slice`.
const sliced = ab.slice(2, 6);
if (sliced.byteLength !== 4) fail();
if (sliced.detached) fail();

// §25.1 resizable buffer.
const rab = new ArrayBuffer(8, { maxByteLength: 32 });
if (rab.byteLength !== 8) fail();
if (rab.maxByteLength !== 32) fail();
if (!rab.resizable) fail();
rab.resize(16);
if (rab.byteLength !== 16) fail();

// §25.1.5.3 — detach. Once detached, byteLength is 0.
const drop = new ArrayBuffer(4);
drop.transfer();
if (!drop.detached) fail();
if (drop.byteLength !== 0) fail();
