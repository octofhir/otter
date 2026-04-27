/* otter-test:
name = "string method: .matchAll(regex) returns capture arrays"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const all = "a1 b22 c333".matchAll(/([a-z])(\d+)/g);
if (all.length !== 3) fail();
// First match: full + 2 groups.
if (all[0].length !== 3) fail();
if (all[0][0] !== "a1") fail();
if (all[0][1] !== "a") fail();
if (all[0][2] !== "1") fail();
// Second match.
if (all[1][0] !== "b22") fail();
if (all[1][1] !== "b") fail();
if (all[1][2] !== "22") fail();
// Third match.
if (all[2][0] !== "c333") fail();
if (all[2][2] !== "333") fail();
