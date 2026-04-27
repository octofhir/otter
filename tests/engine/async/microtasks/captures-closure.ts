/* otter-test:
name = "microtask: closures observe captured state at run time, not enqueue time"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
let counter = 10;
queueMicrotask(() => {
    // Reads counter at *drain* time — which is after the script
    // ran. The script bumped it to 99 before completing, so the
    // microtask sees 99, not 10.
    if (counter !== 99) fail();
});
counter = 99;
