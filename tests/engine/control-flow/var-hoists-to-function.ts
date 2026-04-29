/* otter-test:
name = "control-flow: var hoists to enclosing function scope"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
function f() {
  // Read before declaration sees hoisted `undefined`, no TDZ.
  if (typeof y !== "undefined") fail();
  var y = 5;
  if (y !== 5) fail();
  // var inside a block also reaches the function scope.
  if (true) {
    var blockScoped = 7;
  }
  if (blockScoped !== 7) fail();
  return blockScoped;
}
if (f() !== 7) fail();
