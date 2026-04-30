/* otter-test:
name = "calls: eval(source) + new Function(args, body)"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
// eval primitives.
if (eval("1 + 2") !== 3) fail();
if (eval("'a' + 'b'") !== "ab") fail();
if (eval("null") !== null) fail();
// eval(non-string) returns its argument unchanged.
if (eval(42) !== 42) fail();
// new Function — args + body coerced to strings.
let add = new Function("a", "b", "return a + b;");
if (add(2, 3) !== 5) fail();
let bare = Function("x", "return x * x;");
if (bare(5) !== 25) fail();
// No-arg body.
let noop = new Function("");
if (noop() !== undefined) fail();
// Multi-arg with computation.
let join = new Function("a", "b", "c", "return a + '-' + b + '-' + c;");
if (join("x", "y", "z") !== "x-y-z") fail();
