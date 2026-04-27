/* otter-test:
name = "microtask: tasks drain in FIFO order"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const log = [];
queueMicrotask(() => log.push(1));
queueMicrotask(() => log.push(2));
queueMicrotask(() => log.push(3));
queueMicrotask(() => {
    if (log.length !== 3) fail();
    if (log[0] !== 1) fail();
    if (log[1] !== 2) fail();
    if (log[2] !== 3) fail();
});
