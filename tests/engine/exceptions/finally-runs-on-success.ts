/* otter-test:
name = "exceptions: finally runs after a successful try"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
let log = "";
try {
    log = log + "A";
} finally {
    log = log + "F";
}
if (log !== "AF") fail();
