/* otter-test:
name = "string method: .at(idx) with negative indices"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
if ("abc".at(0) !== "a") fail();
if ("abc".at(2) !== "c") fail();
// Negative indices count from the end.
if ("abc".at(-1) !== "c") fail();
if ("abc".at(-3) !== "a") fail();
// Out-of-range yields undefined.
if ("abc".at(3) !== undefined) fail();
if ("abc".at(-4) !== undefined) fail();
