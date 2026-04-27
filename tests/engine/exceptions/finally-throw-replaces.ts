/* otter-test:
name = "exceptions: finally throw replaces in-flight exception"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
let caught;
try {
    try {
        throw new Error("inner");
    } finally {
        throw new Error("outer");
    }
} catch (e) {
    caught = e.message;
}
if (caught !== "outer") fail();
