/* otter-test:
name = "iterators: for...of walks string code units"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
let collected = "";
for (let c of "abc") {
    collected = collected + c;
}
if (collected !== "abc") fail();
