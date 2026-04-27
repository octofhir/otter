/* otter-test:
name = "exceptions: throw a string value, catch as identity"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
let captured;
try {
    throw "literal-error";
} catch (e) {
    captured = e;
}
if (captured !== "literal-error") fail();
