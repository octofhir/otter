/* otter-test:
name = "microtask: throw inside a microtask surfaces as a runtime error"
[expect]
exit_code = 1
*/
// The script itself completes successfully; the failure surfaces
// from the microtask drain that runs afterwards. Foundation
// exception policy is "first error wins, drain stops" — task 34
// (Promise) flips to spec "rejected promise, drain continues".
queueMicrotask(() => {
    // Trigger TYPE_MISMATCH on undefined.x.
    return undefined.x;
});
