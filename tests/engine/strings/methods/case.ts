/* otter-test:
name = "string method: .toLowerCase / .toUpperCase (ASCII)"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
if ("ABC".toLowerCase() !== "abc") fail();
if ("abc".toUpperCase() !== "ABC") fail();
// Mixed.
if ("Hello, World!".toLowerCase() !== "hello, world!") fail();
if ("Hello, World!".toUpperCase() !== "HELLO, WORLD!") fail();
// Already-cased strings are identity.
if ("abc".toLowerCase() !== "abc") fail();
if ("ABC".toUpperCase() !== "ABC") fail();
