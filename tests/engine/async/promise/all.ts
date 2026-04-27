/* otter-test:
name = "promise: Promise.all collects fulfilled values in input order"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
// Assertions run inside the .then callback — that's the reaction
// the all-aggregator schedules when the result settles.
Promise.all([Promise.resolve(1), Promise.resolve(2), Promise.resolve(3)]).then(
    (values) => {
        if (values.length !== 3) fail();
        if (values[0] !== 1) fail();
        if (values[1] !== 2) fail();
        if (values[2] !== 3) fail();
    }
);
