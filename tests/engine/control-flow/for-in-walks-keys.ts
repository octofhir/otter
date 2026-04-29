/* otter-test:
name = "control-flow: for-in walks enumerable own keys in insertion order"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
let o = { a: 1, b: 2, c: 3 };
let collected: string[] = [];
for (let k in o) {
  collected.push(k);
}
if (collected.length !== 3) fail();
if (collected[0] !== "a") fail();
if (collected[1] !== "b") fail();
if (collected[2] !== "c") fail();
// Sum the values reached through the loop variable.
let total = 0;
for (let k in o) {
  total = total + (o as Record<string, number>)[k];
}
if (total !== 6) fail();
