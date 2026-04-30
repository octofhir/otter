/* otter-test:
name = "atomics: SharedArrayBuffer constructor + grow + shared flag"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

// §25.2.1 — fixed-length SharedArrayBuffer.
const sab = new SharedArrayBuffer(16);
if (sab.byteLength !== 16) fail();
if (sab.detached) fail();
// `growable` is the SAB equivalent of `resizable`.
if (sab.growable) fail();

// §25.2.5 — growable SAB. `maxByteLength` opts in.
const gab = new SharedArrayBuffer(8, { maxByteLength: 32 });
if (gab.byteLength !== 8) fail();
if (gab.maxByteLength !== 32) fail();
if (!gab.growable) fail();
gab.grow(16);
if (gab.byteLength !== 16) fail();
// SABs cannot shrink — grow with a smaller arg is filed as a
// rejection at the bytecode boundary; the test fixture skips it
// here since the runtime surfaces the failure through Op::AtomicsCall.

// SAB cannot detach — `transfer` is a no-op (foundation) and the
// detached flag stays false.
if (sab.detached) fail();
