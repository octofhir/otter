/* otter-test:
name = "control-flow: ?? returns rhs only when lhs is nullish"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
let a = null ?? "default";
if (a !== "default") fail();
let b = undefined ?? 0;
if (b !== 0) fail();
// Falsy non-nullish lhs short-circuits.
let c = 0 ?? "fallback";
if (c !== 0) fail();
let d = "" ?? "fallback";
if (d !== "") fail();
let e = false ?? "fallback";
if (e !== false) fail();
