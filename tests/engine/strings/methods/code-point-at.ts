/* otter-test:
name = "string method: .codePointAt(idx)"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
if ("abc".codePointAt(0) !== 97) fail();
if ("abc".codePointAt(1) !== 98) fail();
if ("abc".codePointAt(2) !== 99) fail();
// Out-of-range yields undefined.
if ("abc".codePointAt(5) !== undefined) fail();
