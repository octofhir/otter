/* otter-test:
name = "async iterators: for await ... of drains async + sync iterables"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

// for-await-of over a sync iterable — each value is awaited (a
// non-thenable value resolves to itself).
async function syncCase() {
    const out = [];
    for await (const v of [1, 2, 3]) {
        out.push(v);
    }
    if (out.length !== 3) fail();
    if (out[0] !== 1 || out[2] !== 3) fail();
}
syncCase();

// for-await-of over an async generator.
async function* gen() {
    yield 10;
    yield 20;
    yield 30;
}
async function asyncCase() {
    const out = [];
    for await (const v of gen()) {
        out.push(v);
    }
    if (out.length !== 3) fail();
    if (out[0] !== 10 || out[2] !== 30) fail();
}
asyncCase();
