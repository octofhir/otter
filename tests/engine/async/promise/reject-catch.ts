/* otter-test:
name = "promise: Promise.reject(reason).catch(handler)"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
let caught = "init";
Promise.reject("boom").catch((reason) => {
    caught = reason;
});
queueMicrotask(() => {
    if (caught !== "boom") fail();
});
