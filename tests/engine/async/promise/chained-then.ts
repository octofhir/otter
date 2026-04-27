/* otter-test:
name = "promise: chained .then receives previous fulfillment"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
Promise.resolve(2)
    .then((v) => v * 10)
    .then((v) => {
        if (v !== 20) fail();
    });
