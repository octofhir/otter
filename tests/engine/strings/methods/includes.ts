/* otter-test:
name = "string method: .includes(needle, from?)"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
if ("hello world".includes("world") !== true) fail();
if ("hello world".includes("zz") !== false) fail();
// `from` argument.
if ("abcabc".includes("a", 1) !== true) fail();
if ("abcabc".includes("a", 4) !== false) fail();
// Empty needle is always present.
if ("abc".includes("") !== true) fail();
