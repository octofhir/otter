/* otter-test:
name = "regexp: .test(s)"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
if (/abc/.test("abcdef") !== true) fail();
if (/abc/.test("xyz") !== false) fail();
// `i` flag.
if (/ABC/i.test("abc") !== true) fail();
// Anchors.
if (/^hi$/.test("hi") !== true) fail();
if (/^hi$/.test("hi there") !== false) fail();
