/* otter-test:
name = "collections: new Map([[k1,v1], [k2,v2]]) seeds entries"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const m = new Map([["a", 1], ["b", 2], ["a", 99]]);
// The third entry overwrites the first per Spec §24.1.1.2 step 9.b.iii.
if (m.get("a") !== 99) fail();
if (m.get("b") !== 2) fail();
if (m.size !== 2) fail();
