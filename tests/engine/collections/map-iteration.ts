/* otter-test:
name = "collections: for...of Map walks [k,v] pairs in insertion order"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const m = new Map();
m.set("a", 1);
m.set("b", 2);
m.set("c", 3);
let keys = "";
let sum = 0;
for (const entry of m) {
    keys = keys + entry[0];
    sum = sum + entry[1];
}
if (keys !== "abc") fail();
if (sum !== 6) fail();
