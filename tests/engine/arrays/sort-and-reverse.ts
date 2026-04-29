/* otter-test:
name = "array: sort default + sort comparator + reverse"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
let xs = [3, 1, 4, 1, 5, 9, 2, 6];
xs.sort();
// Default sort is lexicographic — "1" < "2" < ... < "9".
if (xs[0] !== 1) fail();
if (xs[1] !== 1) fail();
if (xs[xs.length - 1] !== 9) fail();
let ys = [3, 1, 4, 1, 5, 9, 2, 6];
ys.sort((a, b) => b - a);
if (ys[0] !== 9) fail();
if (ys[ys.length - 1] !== 1) fail();
let zs = [1, 2, 3];
zs.reverse();
if (zs[0] !== 3) fail();
if (zs[1] !== 2) fail();
if (zs[2] !== 1) fail();
