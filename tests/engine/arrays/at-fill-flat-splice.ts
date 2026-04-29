/* otter-test:
name = "array: at / fill / flat / splice mutation primitives"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
let xs = [10, 20, 30, 40, 50];
if (xs.at(0) !== 10) fail();
if (xs.at(-1) !== 50) fail();
if (xs.at(-2) !== 40) fail();
if (xs.at(99) !== undefined) fail();

let f = [1, 2, 3, 4, 5];
f.fill(0, 1, 4);
if (f[0] !== 1) fail();
if (f[1] !== 0) fail();
if (f[3] !== 0) fail();
if (f[4] !== 5) fail();

let nested = [1, [2, 3], [4, [5, 6]]];
let flat1 = nested.flat();
if (flat1.length !== 5) fail();
if (flat1[0] !== 1) fail();
if (flat1[3] !== 4) fail();
// flat(1) leaves [5,6] as a nested array.
if (Array.isArray(flat1[4]) !== true) fail();

let s = [1, 2, 3, 4, 5];
let removed = s.splice(1, 2, 99, 100, 101);
if (removed.length !== 2) fail();
if (removed[0] !== 2) fail();
if (removed[1] !== 3) fail();
if (s.length !== 6) fail();
if (s[1] !== 99) fail();
if (s[3] !== 101) fail();
if (s[4] !== 4) fail();
