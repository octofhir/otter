/* otter-test:
name = "microtask: queued task runs after the script completes"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const log = [];
queueMicrotask(() => log.push("a"));
log.push("b");
// At script-completion time the microtask hasn't run, so log
// must contain only "b". The drain happens after; we verify
// post-drain state through a second microtask that aborts on
// mismatch.
if (log.length !== 1) fail();
if (log[0] !== "b") fail();
queueMicrotask(() => {
    // By the time this runs we are inside the drain — the first
    // microtask (push "a") has already executed, so the array
    // length is 2 with order ["b","a"].
    if (log.length !== 2) fail();
    if (log[0] !== "b") fail();
    if (log[1] !== "a") fail();
});
