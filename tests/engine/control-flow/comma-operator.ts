/* otter-test:
name = "control-flow: comma operator returns last value"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
let r = (1, 2, 3);
if (r !== 3) fail();
let counter = 0;
let s = (counter = counter + 1, counter = counter + 1, counter);
if (s !== 2) fail();
if (counter !== 2) fail();
// Comma in for-loop init.
let total = 0;
for (let i = 0, j = 10; i < 3; i = i + 1, j = j - 1) {
  total = total + i * j;
}
if (total !== 0 + 9 + 16) fail();
