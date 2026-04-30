/* otter-test:
name = "async generators: .next/.return/.throw return Promises"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

async function* g() {
    yield 1;
    yield 2;
    yield 3;
}

const it = g();

// .next() returns a Promise of {value, done}.
it.next().then((r) => {
    if (r.value !== 1 || r.done !== false) fail();
});
it.next().then((r) => {
    if (r.value !== 2) fail();
});
it.next().then((r) => {
    if (r.value !== 3) fail();
});
it.next().then((r) => {
    if (r.done !== true) fail();
});

// Throws settle as rejections.
async function* g2() {
    try { yield 1; } catch (e) { yield "caught:" + e; }
}
const it2 = g2();
it2.next().then((r) => { if (r.value !== 1) fail(); });
it2.throw("oops").then((r) => {
    if (r.value !== "caught:oops") fail();
});
