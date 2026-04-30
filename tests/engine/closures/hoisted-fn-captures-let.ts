/* otter-test:
name = "closures: hoisted nested function captures forward let / const"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
function outer() {
  // `inner` hoists above `let value`, but the capture must still
  // resolve once `let value = 99` has run.
  function inner() {
    return value;
  }
  let value = 99;
  return inner();
}
if (outer() !== 99) fail();
