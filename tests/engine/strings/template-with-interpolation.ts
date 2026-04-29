/* otter-test:
name = "string: template literal interpolates expressions"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
let name = "Otter";
let n = 7;
let greet = `Hello, ${name}! You have ${n} fish.`;
if (greet !== "Hello, Otter! You have 7 fish.") fail();
let nested = `outer:${`inner:${n}`}.`;
if (nested !== "outer:inner:7.") fail();
let empty = `${""}${""}`;
if (empty !== "") fail();
let math = `sum=${1 + 2}`;
if (math !== "sum=3") fail();
