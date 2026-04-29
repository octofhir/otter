/* otter-test:
name = "collections: WeakMap stores values by object identity"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const wm = new WeakMap();
const k = {};
const other = {};
wm.set(k, 42);
if (wm.get(k) !== 42) fail();
if (!wm.has(k)) fail();
if (wm.has(other)) fail();
const removed = wm.delete(k);
if (removed !== true) fail();
if (wm.has(k)) fail();
