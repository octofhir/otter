/* otter-test:
name = "object: Object.keys / values / entries walk insertion order"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
let o = { a: 1, b: 2, c: 3 };
let keys = Object.keys(o);
if (keys.length !== 3) fail();
if (keys[0] !== "a") fail();
if (keys[1] !== "b") fail();
if (keys[2] !== "c") fail();
let values = Object.values(o);
if (values.length !== 3) fail();
if (values[0] !== 1) fail();
if (values[2] !== 3) fail();
let entries = Object.entries(o);
if (entries.length !== 3) fail();
if (entries[0][0] !== "a") fail();
if (entries[0][1] !== 1) fail();
if (entries[2][0] !== "c") fail();
if (entries[2][1] !== 3) fail();
