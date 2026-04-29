/* otter-test:
name = "control-flow: var redeclaration keeps the same binding"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
function r() {
  var x = 1;
  if (x !== 1) fail();
  var x = 2;
  if (x !== 2) fail();
  return x;
}
if (r() !== 2) fail();
// Redeclaration at top level too.
var topX = "first";
if (topX !== "first") fail();
var topX = "second";
if (topX !== "second") fail();
