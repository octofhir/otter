/* otter-test:
name = "control-flow: labelled continue skips to outer iteration"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
let pairs: Array<[number, number]> = [];
outer: for (let i = 0; i < 3; i = i + 1) {
  for (let j = 0; j < 3; j = j + 1) {
    if (j === 1) {
      continue outer;
    }
    pairs.push([i, j]);
  }
}
// We push only when j === 0, so 3 pairs total.
if (pairs.length !== 3) fail();
if (pairs[0][0] !== 0 || pairs[0][1] !== 0) fail();
if (pairs[1][0] !== 1 || pairs[1][1] !== 0) fail();
if (pairs[2][0] !== 2 || pairs[2][1] !== 0) fail();
