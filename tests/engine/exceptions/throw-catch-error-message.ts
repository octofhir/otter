/* otter-test:
name = "exceptions: throw new Error caught with .message"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
let seen;
try {
    throw new Error("boom");
} catch (e) {
    seen = e.message;
}
if (seen !== "boom") fail();
