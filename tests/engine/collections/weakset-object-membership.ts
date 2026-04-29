/* otter-test:
name = "collections: WeakSet tracks objects by identity"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const ws = new WeakSet();
const a = {};
const b = {};
ws.add(a);
if (!ws.has(a)) fail();
if (ws.has(b)) fail();
ws.add(b);
if (!ws.has(b)) fail();
const removed = ws.delete(a);
if (removed !== true) fail();
if (ws.has(a)) fail();
