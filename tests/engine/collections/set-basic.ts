/* otter-test:
name = "collections: Set add/has/delete/size with dedupe"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const s = new Set();
s.add(1);
s.add(1);
s.add(2);
if (s.size !== 2) fail();
if (!s.has(1)) fail();
if (s.has(99)) fail();
const removed = s.delete(1);
if (removed !== true) fail();
if (s.has(1)) fail();
if (s.size !== 1) fail();
s.clear();
if (s.size !== 0) fail();
