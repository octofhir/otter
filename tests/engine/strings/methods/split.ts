/* otter-test:
name = "string method: .split(separator, limit?)"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const parts = "a,b,c".split(",");
if (parts.length !== 3) fail();
if (parts[0] !== "a") fail();
if (parts[1] !== "b") fail();
if (parts[2] !== "c") fail();

// Consecutive separators yield empty chunks.
const empties = "a,,b".split(",");
if (empties.length !== 3) fail();
if (empties[0] !== "a") fail();
if (empties[1] !== "") fail();
if (empties[2] !== "b") fail();

// Empty separator splits into individual code units.
const chars = "abc".split("");
if (chars.length !== 3) fail();
if (chars[0] !== "a") fail();
if (chars[2] !== "c") fail();

// Limit caps the result length.
const limited = "a,b,c,d".split(",", 2);
if (limited.length !== 2) fail();
if (limited[0] !== "a") fail();
if (limited[1] !== "b") fail();

// No match → singleton.
const single = "abc".split(",");
if (single.length !== 1) fail();
if (single[0] !== "abc") fail();
