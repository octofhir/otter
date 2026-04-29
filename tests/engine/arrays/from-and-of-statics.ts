/* otter-test:
name = "array: Array.from / Array.of statics"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
let copy = Array.from([1, 2, 3]);
if (copy.length !== 3) fail();
if (copy[0] !== 1) fail();
if (copy[2] !== 3) fail();
let fromSet = Array.from(new Set([1, 2, 2, 3]));
if (fromSet.length !== 3) fail();
let fromMap = Array.from(new Map([["a", 1], ["b", 2]]));
if (fromMap.length !== 2) fail();
if (fromMap[0][0] !== "a") fail();
if (fromMap[0][1] !== 1) fail();
let xs = Array.of(7, 8, 9);
if (xs.length !== 3) fail();
if (xs[0] !== 7) fail();
if (xs[2] !== 9) fail();
