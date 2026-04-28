/* otter-test:
name = "async: throw inside async function rejects the result promise"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
async function f() {
    throw "boom";
}
f().catch((reason) => {
    if (reason !== "boom") fail();
});
