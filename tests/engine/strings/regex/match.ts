/* otter-test:
name = "string method: .match(regex)"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
// Non-global match returns capture-group array.
const m = "hello world".match(/(\w+)\s(\w+)/);
if (m === null) fail();
if (m.length !== 3) fail();
if (m[0] !== "hello world") fail();
if (m[1] !== "hello") fail();
if (m[2] !== "world") fail();

// Global match returns array of full matches.
const g = "abcabc".match(/b./g);
if (g === null) fail();
if (g.length !== 2) fail();
if (g[0] !== "bc") fail();
if (g[1] !== "bc") fail();

// No match → null.
if ("abc".match(/zz/) !== null) fail();
