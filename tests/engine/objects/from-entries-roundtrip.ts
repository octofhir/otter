/* otter-test:
name = "object: Object.fromEntries inverts Object.entries"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
let original = { a: 1, b: 2, c: 3 };
let pairs = Object.entries(original);
let copy = Object.fromEntries(pairs);
if (copy.a !== 1) fail();
if (copy.b !== 2) fail();
if (copy.c !== 3) fail();
let copyKeys = Object.keys(copy);
if (copyKeys.length !== 3) fail();
