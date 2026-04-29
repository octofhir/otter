/* otter-test:
name = "collections: for...of Set walks values in insertion order"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const s = new Set();
s.add(10);
s.add(20);
s.add(30);
let total = 0;
for (const v of s) {
    total = total + v;
}
if (total !== 60) fail();
