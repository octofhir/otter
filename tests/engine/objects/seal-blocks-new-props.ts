/* otter-test:
name = "object: Object.seal blocks new properties but allows updates"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
let o = { a: 1 };
Object.seal(o);
if (Object.isSealed(o) !== true) fail();
if (Object.isFrozen(o) !== false) fail();
// Existing writable property still updates.
o.a = 5;
if (o.a !== 5) fail();
