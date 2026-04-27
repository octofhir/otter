/* otter-test:
name = "exceptions: throw unwinds across function frames"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
function deep() {
    throw new Error("from-deep");
}
function middle() {
    deep();
}
let caught;
try {
    middle();
} catch (e) {
    caught = e.message;
}
if (caught !== "from-deep") fail();
