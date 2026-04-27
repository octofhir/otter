/* otter-test:
name = "string method: .concat(...args)"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
if ("ab".concat("cd") !== "abcd") fail();
if ("ab".concat("cd", "ef") !== "abcdef") fail();
// Zero-arg concat is identity.
if ("hello".concat() !== "hello") fail();
// Empty arg.
if ("hello".concat("") !== "hello") fail();
