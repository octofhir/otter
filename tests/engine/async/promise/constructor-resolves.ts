/* otter-test:
name = "promise: new Promise((resolve) => resolve(v))"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
let result = 0;
new Promise((resolve, reject) => {
    resolve(42);
}).then((v) => {
    result = v;
});
queueMicrotask(() => {
    if (result !== 42) fail();
});
