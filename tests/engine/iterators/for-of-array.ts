/* otter-test:
name = "iterators: for...of walks array elements"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
let sum = 0;
for (let n of [1, 2, 3, 4]) {
    sum = sum + n;
}
if (sum !== 10) fail();
