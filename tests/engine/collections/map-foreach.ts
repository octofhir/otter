/* otter-test:
name = "collections: Map.prototype.forEach invokes callback with (value, key, map)"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const m = new Map();
m.set("a", 10);
m.set("b", 20);
m.set("c", 30);
let valueSum = 0;
let keys = "";
let recvSeen = 0;
m.forEach(function (value, key, recv) {
    valueSum = valueSum + value;
    keys = keys + key;
    if (recv === m) recvSeen = recvSeen + 1;
});
if (valueSum !== 60) fail();
if (keys !== "abc") fail();
if (recvSeen !== 3) fail();
