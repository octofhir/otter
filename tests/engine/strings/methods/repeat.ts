/* otter-test:
name = "string method: .repeat(n)"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
if ("abc".repeat(3) !== "abcabcabc") fail();
if ("abc".repeat(0) !== "") fail();
if ("".repeat(5) !== "") fail();
if ("x".repeat(1) !== "x") fail();
