/* otter-test:
name = "control-flow: top-level var hoists to script / module scope"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
// Read before declaration sees hoisted `undefined`.
if (typeof topV !== "undefined") fail();
if (topV !== undefined) fail();
var topV = 42;
if (topV !== 42) fail();
// var inside block at top-level still hoists to module scope.
if (true) {
  var blockTop = "deep";
}
if (blockTop !== "deep") fail();
