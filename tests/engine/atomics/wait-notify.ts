/* otter-test:
name = "atomics: wait / notify / waitAsync (single-thread foundation)"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

const sab = new SharedArrayBuffer(16);
const i32 = new Int32Array(sab);
i32[0] = 5;

// §25.4.11 — wait returns "not-equal" when the cell does not match
// the expected value.
if (Atomics.wait(i32, 0, 99) !== "not-equal") fail();
// When the cell matches, the foundation always times out (no
// notify can ever fire on a single-threaded VM).
if (Atomics.wait(i32, 0, 5) !== "timed-out") fail();

// §25.4.12 — notify always wakes 0 waiters.
if (Atomics.notify(i32, 0, 1) !== 0) fail();

// §25.4.x — waitAsync produces a `{async, value}` record whose
// `value` is a fulfilled promise of the wait outcome.
const r = Atomics.waitAsync(i32, 0, 99);
if (r.async !== false) fail();
let observed = "";
r.value.then((s) => {
    observed = s;
});
// Microtask drain runs before script exit; the promise resolves.