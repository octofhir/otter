/* otter-test:
name = "array: forEach + map + filter callbacks"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
let xs = [1, 2, 3, 4, 5];
let sum = 0;
xs.forEach((v) => { sum = sum + v; });
if (sum !== 15) fail();
let doubled = xs.map((v) => v * 2);
if (doubled.length !== 5) fail();
if (doubled[0] !== 2) fail();
if (doubled[4] !== 10) fail();
let evens = xs.filter((v) => v % 2 === 0);
if (evens.length !== 2) fail();
if (evens[0] !== 2) fail();
if (evens[1] !== 4) fail();
