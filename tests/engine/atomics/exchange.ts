/* otter-test:
name = "atomics: exchange / compareExchange / isLockFree"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

const sab = new SharedArrayBuffer(16);
const i32 = new Int32Array(sab);

// §25.4.7 exchange returns the previous value, writes the new.
i32[0] = 5;
if (Atomics.exchange(i32, 0, 99) !== 5) fail();
if (i32[0] !== 99) fail();

// §25.4.6 compareExchange — only stores when current === expected.
if (Atomics.compareExchange(i32, 0, 99, 100) !== 99) fail();
if (i32[0] !== 100) fail();
// Mismatch: stays put.
if (Atomics.compareExchange(i32, 0, 0, 200) !== 100) fail();
if (i32[0] !== 100) fail();

// §25.4.13 isLockFree — true for sizes 1, 2, 4, 8.
if (!Atomics.isLockFree(1)) fail();
if (!Atomics.isLockFree(2)) fail();
if (!Atomics.isLockFree(4)) fail();
if (!Atomics.isLockFree(8)) fail();
if (Atomics.isLockFree(3)) fail();
if (Atomics.isLockFree(16)) fail();

// Float arrays reject — atomics need integer kinds.
const f32 = new Float32Array(sab);
let threw = false;
try { Atomics.load(f32, 0); } catch (e) { threw = true; }
if (!threw) fail();

// BigInt arrays work too.
const i64 = new BigInt64Array(sab);
i64[0] = 5n;
if (Atomics.add(i64, 0, 3n) !== 5n) fail();
if (i64[0] !== 8n) fail();
