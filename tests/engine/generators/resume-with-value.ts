/* otter-test:
name = "generators: .next(arg) resumes the yield expression with arg"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

function* g() {
    const a = yield 1;
    const b = yield a + 10;
    return b * 2;
}

const it = g();
const r1 = it.next();
if (r1.value !== 1) fail();
// First .next() arg is dropped per spec — pass anyway.
const r2 = it.next(5);
if (r2.value !== 15) fail();
const r3 = it.next(7);
if (r3.value !== 14 || r3.done !== true) fail();

// Subsequent .next() short-circuits to done.
const r4 = it.next();
if (r4.done !== true) fail();
if (r4.value !== undefined) fail();
