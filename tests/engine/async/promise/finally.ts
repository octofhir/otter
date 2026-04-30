/* otter-test:
name = "promise: Promise.prototype.finally forwards original settlement"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

// Fulfilled path: finally callback runs, original value flows
// through to .then.
let aRan = false;
Promise.resolve(7)
    .finally(() => {
        aRan = true;
    })
    .then((v) => {
        if (!aRan) fail();
        if (v !== 7) fail();
    });

// Rejected path: finally callback runs, original reason still
// rejects the chained promise.
let bRan = false;
Promise.reject("nope")
    .finally(() => {
        bRan = true;
    })
    .then(
        () => fail(),
        (e) => {
            if (!bRan) fail();
            if (e !== "nope") fail();
        }
    );

// Non-callable onFinally — settlement still flows through.
Promise.resolve(42)
    .finally(undefined)
    .then((v) => {
        if (v !== 42) fail();
    });
