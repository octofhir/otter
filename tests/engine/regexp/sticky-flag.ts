/* otter-test:
name = "regexp: y flag anchors matches at lastIndex"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const re = /a/y;
// Match at position 0.
const m1 = re.exec("aaa");
if (m1 === null) fail();
if (m1[0] !== "a") fail();
if (re.lastIndex !== 1) fail();

// Sticky requires the match at lastIndex; matches "a" at index 1.
const m2 = re.exec("aaa");
if (m2 === null) fail();
if (re.lastIndex !== 2) fail();

// Skip ahead — sticky won't roam.
re.lastIndex = 5;
if (re.exec("aaaba") !== null) fail();
// And lastIndex resets to 0 on miss.
if (re.lastIndex !== 0) fail();
