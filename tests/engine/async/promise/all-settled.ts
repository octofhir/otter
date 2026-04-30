/* otter-test:
name = "promise: Promise.allSettled records each settlement"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
// §27.2.4.2 — every input promise contributes a record regardless
// of whether it fulfilled or rejected.
Promise.allSettled([
    Promise.resolve(1),
    Promise.reject("oops"),
    Promise.resolve(3),
]).then((records) => {
    if (records.length !== 3) fail();
    if (records[0].status !== "fulfilled") fail();
    if (records[0].value !== 1) fail();
    if (records[1].status !== "rejected") fail();
    if (records[1].reason !== "oops") fail();
    if (records[2].status !== "fulfilled") fail();
    if (records[2].value !== 3) fail();
});

// Empty input fulfils synchronously (microtask) with [].
Promise.allSettled([]).then((rs) => {
    if (rs.length !== 0) fail();
});
