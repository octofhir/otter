/* otter-test:
name = "microtask: queueMicrotask inside a microtask runs in the next generation"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const log = [];
queueMicrotask(() => {
    log.push("outer");
    queueMicrotask(() => log.push("inner"));
    // Inner microtask must NOT run inside this body — it lands on
    // the next generation. So `log` ends this body with one entry.
    if (log.length !== 1) fail();
});
queueMicrotask(() => {
    // Runs after the outer body but before the inner microtask
    // (still the same generation as the outer one).
    if (log.length !== 1) fail();
    if (log[0] !== "outer") fail();
});
queueMicrotask(() => {
    // Runs in the next generation alongside `inner`. By the time
    // we get here `inner` has been queued; it runs as part of the
    // same outer drain call but in the next sweep.
    log.push("trailer");
});
