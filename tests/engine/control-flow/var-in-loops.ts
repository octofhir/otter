/* otter-test:
name = "control-flow: var in for / for-in / for-of leaks to enclosing scope"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
function classicFor() {
  for (var i = 0; i < 3; i = i + 1) {}
  // i survives the loop because var hoists.
  return i;
}
if (classicFor() !== 3) fail();

function forOf() {
  for (var v of [10, 20, 30]) {
    // last-write wins.
  }
  return v;
}
if (forOf() !== 30) fail();

function forIn() {
  let target: string | undefined;
  let seen = 0;
  for (var k in { a: 1, b: 2 }) {
    seen = seen + 1;
    target = k;
  }
  // Both `k` and `target` survive — but only `k` was var-hoisted.
  if (typeof k !== "string") fail();
  if (seen !== 2) fail();
  return k;
}
if (forIn() !== "b") fail();
