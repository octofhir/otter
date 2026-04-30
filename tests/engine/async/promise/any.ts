/* otter-test:
name = "promise: Promise.any short-circuits on first fulfillment / aggregates rejections"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
// §27.2.4.3 — first fulfillment wins.
Promise.any([
    Promise.reject("a"),
    Promise.resolve(7),
    Promise.reject("b"),
]).then((v) => {
    if (v !== 7) fail();
});

// All-reject path produces an AggregateError carrying the reasons.
Promise.any([Promise.reject(1), Promise.reject(2), Promise.reject(3)]).then(
    () => fail(),
    (err) => {
        if (!(err instanceof AggregateError)) fail();
        if (err.name !== "AggregateError") fail();
        if (err.message !== "All promises were rejected") fail();
        if (!Array.isArray(err.errors)) fail();
        if (err.errors.length !== 3) fail();
        if (err.errors[0] !== 1) fail();
        if (err.errors[2] !== 3) fail();
    }
);

// Empty input rejects with an empty AggregateError.
Promise.any([]).then(
    () => fail(),
    (err) => {
        if (!(err instanceof AggregateError)) fail();
        if (err.errors.length !== 0) fail();
    }
);
