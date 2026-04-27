/* otter-test:
name = "regexp: .exec(s) returns capture array or null"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const r = /(\w+)@(\w+)/.exec("alice@example");
if (r === null) fail();
if (r.length !== 3) fail();
if (r[0] !== "alice@example") fail();
if (r[1] !== "alice") fail();
if (r[2] !== "example") fail();

// No match → null.
if (/zz/.exec("abc") !== null) fail();
