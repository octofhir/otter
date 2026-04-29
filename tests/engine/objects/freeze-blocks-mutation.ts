/* otter-test:
name = "object: Object.freeze blocks writes and deletes"
[expect]
exit_code = 1
*/
function fail() {
  return undefined.x;
}
let o = { a: 1, b: 2 };
Object.freeze(o);
if (Object.isFrozen(o) !== true) fail();
if (Object.isSealed(o) !== true) fail();
if (Object.isExtensible(o) !== false) fail();
// Reading frozen properties is fine.
if (o.a !== 1) fail();
// Writes to frozen properties throw (foundation surface — sloppy
// mode silent-fail lands with task 25).
o.a = 99;
