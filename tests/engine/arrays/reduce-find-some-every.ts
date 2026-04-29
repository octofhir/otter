/* otter-test:
name = "array: reduce + find + some + every callbacks"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
let xs = [1, 2, 3, 4, 5];
let total = xs.reduce((acc, v) => acc + v, 0);
if (total !== 15) fail();
let product = xs.reduce((acc, v) => acc * v, 1);
if (product !== 120) fail();
let firstEven = xs.find((v) => v % 2 === 0);
if (firstEven !== 2) fail();
let firstEvenIdx = xs.findIndex((v) => v % 2 === 0);
if (firstEvenIdx !== 1) fail();
if (xs.every((v) => v > 0) !== true) fail();
if (xs.every((v) => v > 2) !== false) fail();
if (xs.some((v) => v > 4) !== true) fail();
if (xs.some((v) => v > 99) !== false) fail();
