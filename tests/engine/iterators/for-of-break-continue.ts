/* otter-test:
name = "iterators: for...of honors break and continue"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
let sum = 0;
for (let n of [1, 2, 3, 4, 5, 6]) {
    if (n === 2) continue;
    if (n === 5) break;
    sum = sum + n;
}
// Skips 2, breaks before 5: 1 + 3 + 4 = 8.
if (sum !== 8) fail();
