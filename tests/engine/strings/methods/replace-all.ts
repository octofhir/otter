/* otter-test:
name = "string method: .replaceAll(string, string)"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
if ("abcabc".replaceAll("b", "X") !== "aXcaXc") fail();
// Empty needle weaves the replacement between each unit and at both ends.
if ("abc".replaceAll("", "X") !== "XaXbXcX") fail();
// No match → original.
if ("abc".replaceAll("zz", "X") !== "abc") fail();
// Non-overlapping advance.
if ("aaa".replaceAll("aa", "X") !== "Xa") fail();
