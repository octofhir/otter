/* otter-test:
name = "string method: .replace(regex, replacement)"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
// Non-global → first match only.
if ("abc".replace(/b/, "X") !== "aXc") fail();
// Global flag → all matches.
if ("abcabc".replace(/b/g, "X") !== "aXcaXc") fail();
// $& expands to the full match.
if ("hi".replace(/h(i)/, "[$&]") !== "[hi]") fail();
// $1 expands to the first capture group.
if ("hello".replace(/(l+)/, "<$1>") !== "he<ll>o") fail();
// $$ inserts a literal dollar.
if ("x".replace(/x/, "$$") !== "$") fail();
// No match → original string.
if ("abc".replace(/z/, "Q") !== "abc") fail();
