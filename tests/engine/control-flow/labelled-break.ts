/* otter-test:
name = "control-flow: labelled break exits outer loop"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
let firstHit: number[] | null = null;
outer: for (let i = 0; i < 4; i = i + 1) {
  for (let j = 0; j < 4; j = j + 1) {
    if (i * j === 6) {
      firstHit = [i, j];
      break outer;
    }
  }
}
if (firstHit === null) fail();
if (firstHit![0] !== 2) fail();
if (firstHit![1] !== 3) fail();
