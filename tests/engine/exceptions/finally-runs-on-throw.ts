/* otter-test:
name = "exceptions: finally runs while exception unwinds"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
let log = "";
try {
    try {
        log = log + "A";
        throw new Error("x");
    } finally {
        log = log + "F";
    }
} catch (e) {
    log = log + "C";
}
if (log !== "AFC") fail();
