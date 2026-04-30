/* otter-test:
name = "atomics: load / store / add / sub / and / or / xor return previous value"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

const sab = new SharedArrayBuffer(32);
const i32 = new Int32Array(sab);

// load + store.
i32[0] = 5;
if (Atomics.load(i32, 0) !== 5) fail();
Atomics.store(i32, 1, 10);
if (i32[1] !== 10) fail();

// add returns the previous value.
if (Atomics.add(i32, 0, 7) !== 5) fail();
if (i32[0] !== 12) fail();

// sub returns the previous value.
if (Atomics.sub(i32, 0, 2) !== 12) fail();
if (i32[0] !== 10) fail();

// and / or / xor.
i32[2] = 0xF0;
if (Atomics.and(i32, 2, 0x0F) !== 0xF0) fail();
if (i32[2] !== 0) fail();
i32[3] = 0xF0;
if (Atomics.or(i32, 3, 0x0F) !== 0xF0) fail();
if (i32[3] !== 0xFF) fail();
if (Atomics.xor(i32, 3, 0xFF) !== 0xFF) fail();
if (i32[3] !== 0) fail();
