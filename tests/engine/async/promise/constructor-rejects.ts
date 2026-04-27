/* otter-test:
name = "promise: new Promise((_, reject) => reject(reason))"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
let caught = "init";
new Promise((resolve, reject) => {
    reject("oops");
}).catch((reason) => {
    caught = reason;
});
queueMicrotask(() => {
    if (caught !== "oops") fail();
});
