/* otter-test:
name = "async: top-level await suspends and resumes the entry frame"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
let p = Promise.resolve(42);
let v = await p;
if (v !== 42) fail();
// Multiple sequential awaits.
let a = await Promise.resolve(1);
let b = await Promise.resolve(2);
let c = await Promise.resolve(3);
if (a + b + c !== 6) fail();
// Await of a non-thenable wraps + resolves.
let d = await 99;
if (d !== 99) fail();
