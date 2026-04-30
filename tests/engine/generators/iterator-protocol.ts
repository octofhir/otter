/* otter-test:
name = "generators: for-of + spread iterate generator output"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

function* threeNumbers() {
    yield 0;
    yield 1;
    yield 2;
}

// for-of walks the generator like any iterable.
const out = [];
for (const v of threeNumbers()) {
    out.push(v);
}
if (out.length !== 3) fail();
if (out[0] !== 0 || out[2] !== 2) fail();

// Spread expands a generator into an array.
const arr = [...threeNumbers()];
if (arr.length !== 3) fail();
if (arr[1] !== 1) fail();

// Iterator helpers compose with generators because the gen value
// drives the helper protocol.
const doubled = Iterator.from(threeNumbers()).map((x) => x * 2).toArray();
if (doubled.length !== 3) fail();
if (doubled[2] !== 4) fail();
