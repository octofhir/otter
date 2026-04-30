/* otter-test:
name = "object: globalThis carries a self-reference and accepts assignments"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
if (typeof globalThis !== "object") fail();
if (globalThis.globalThis !== globalThis) fail();
// User code can stash + read state on globalThis.
(globalThis as any).counter = 7;
if ((globalThis as any).counter !== 7) fail();
