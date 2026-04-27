/* otter-test:
name = "promise: Promise.resolve(7).then(v => v + 1)"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
let result = 0;
Promise.resolve(7).then((v) => {
    result = v + 1;
});
queueMicrotask(() => {
    if (result !== 8) fail();
});
