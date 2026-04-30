/* otter-test:
name = "generators: yield* delegates to inner iterables"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

function* inner() {
    yield 1;
    yield 2;
    yield 3;
}

function* outer() {
    yield 0;
    yield* inner();
    yield 4;
}

const out = [];
for (const v of outer()) {
    out.push(v);
}
if (out.length !== 5) fail();
if (out[0] !== 0) fail();
if (out[1] !== 1 || out[2] !== 2 || out[3] !== 3) fail();
if (out[4] !== 4) fail();

// yield* over an array (any iterable) flattens too.
function* fromArray() {
    yield* [10, 20, 30];
}
const arr = [];
for (const v of fromArray()) arr.push(v);
if (arr.length !== 3) fail();
if (arr[0] !== 10 || arr[2] !== 30) fail();

// Nested yield* — inner generator that itself delegates.
function* a() { yield "a"; yield* b(); yield "c"; }
function* b() { yield "b1"; yield "b2"; }
const seen = [];
for (const v of a()) seen.push(v);
if (seen.length !== 4) fail();
if (seen[0] !== "a" || seen[1] !== "b1" || seen[2] !== "b2" || seen[3] !== "c") fail();
