/* otter-test:
name = "binary: detached-buffer ops throw / no-op per spec"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}

const ab = new ArrayBuffer(8);
const dv = new DataView(ab);
const ta = new Uint8Array(ab);

ab.transfer();
if (!ab.detached) fail();

// DataView reads / writes throw on a detached buffer.
let threw = false;
try { dv.getUint8(0); } catch (e) { threw = true; }
if (!threw) fail();
let threw2 = false;
try { dv.setUint8(0, 1); } catch (e) { threw2 = true; }
if (!threw2) fail();

// TypedArray length / byteLength go to 0 on detach. Indexed reads
// return undefined per §10.4.5.13.
if (ta.length !== 0) fail();
if (ta.byteLength !== 0) fail();
if (ta[0] !== undefined) fail();

// TypedArray methods that need bytes throw.
let threw3 = false;
try { ta.fill(7); } catch (e) { threw3 = true; }
if (!threw3) fail();

// Re-detach throws per §25.1.5.8 step 4.
let threw4 = false;
try { ab.transfer(); } catch (e) { threw4 = true; }
if (!threw4) fail();
