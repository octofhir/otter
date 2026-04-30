/* otter-test:
name = "control-flow: top-level function declarations hoist"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
// Call before the source-level declaration.
if (before() !== 42) fail();
function before() {
  return 42;
}
// Mutual recursion: both forward references resolve.
if (even(4) !== true) fail();
if (odd(5) !== true) fail();
function even(n: number): boolean {
  if (n === 0) return true;
  return odd(n - 1);
}
function odd(n: number): boolean {
  if (n === 0) return false;
  return even(n - 1);
}
// Nested hoisting inside another function.
function outer(): number {
  return inner();
  function inner() {
    return 7;
  }
}
if (outer() !== 7) fail();
// Last-declaration-wins for duplicate names.
function dup() {
  return "first";
}
function dup() {
  return "second";
}
if (dup() !== "second") fail();
