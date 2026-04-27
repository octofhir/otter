/* otter-test:
name = "regexp: .source / .flags / boolean flag accessors"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const re = /abc/gim;
if (re.source !== "abc") fail();
if (re.flags !== "gim") fail();
if (re.global !== true) fail();
if (re.ignoreCase !== true) fail();
if (re.multiline !== true) fail();
if (re.dotAll !== false) fail();
if (re.sticky !== false) fail();
