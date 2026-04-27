/* otter-test:
name = "string method: .replace(string, string)"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
if ("abcabc".replace("b", "X") !== "aXcabc") fail();
// Empty needle prepends the replacement.
if ("abc".replace("", "X") !== "Xabc") fail();
// No match → original.
if ("abc".replace("zz", "X") !== "abc") fail();
// Multi-char needle.
if ("hello world".replace("world", "there") !== "hello there") fail();
