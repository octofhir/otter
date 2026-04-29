/* otter-test:
name = "collections: Map get/set/has/delete/size"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const m = new Map();
m.set("a", 1);
m.set("b", 2);
if (m.size !== 2) fail();
if (m.get("a") !== 1) fail();
if (m.get("missing") !== undefined) fail();
if (!m.has("b")) fail();
if (m.has("missing")) fail();
const removed = m.delete("a");
if (removed !== true) fail();
if (m.has("a")) fail();
if (m.size !== 1) fail();
m.clear();
if (m.size !== 0) fail();
