/* otter-test:
name = "string method: .replaceAll(regex, replacement)"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
if ("abcabc".replaceAll(/b/g, "X") !== "aXcaXc") fail();
// Capture groups round-trip through $1.
if ("foo123bar456".replaceAll(/(\d+)/g, "<$1>") !== "foo<123>bar<456>") fail();
// No matches → original.
if ("abc".replaceAll(/z/g, "Q") !== "abc") fail();
