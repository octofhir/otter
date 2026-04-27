/* otter-test:
name = "string method: .split(regex, limit?)"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const parts = "a,b;c d".split(/[,; ]/);
if (parts.length !== 4) fail();
if (parts[0] !== "a") fail();
if (parts[1] !== "b") fail();
if (parts[2] !== "c") fail();
if (parts[3] !== "d") fail();

// Limit caps the result.
const limited = "1-2-3-4".split(/-/, 2);
if (limited.length !== 2) fail();
if (limited[0] !== "1") fail();
if (limited[1] !== "2") fail();

// No match → singleton.
const single = "abc".split(/zz/);
if (single.length !== 1) fail();
if (single[0] !== "abc") fail();
